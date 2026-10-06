//! Index of the makefiles visible from a document.
//!
//! A document sees the files it includes (recursively) and, when known, the
//! files that include it together with everything those include. Open editor
//! buffers take precedence over the copies on disk; files read from disk are
//! parsed once and cached until their modification time or size changes.
//!
//! Feature modules work on a [`FileSet`], which is a snapshot of the
//! documents visible from one document and can be built without a server.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use makefile_lossless::{Include, Makefile, MakefileVariant, Parse, SyntaxKind};
use rowan::ast::AstNode;
use text_size::{TextRange, TextSize};
use tower_lsp_server::ls_types::Uri;

/// Upper bound on the number of files visited when following includes.
const MAX_FILES: usize = 256;

/// Included files larger than this are not loaded.
const MAX_FILE_SIZE: u64 = 4 * 1024 * 1024;

/// A parsed makefile, either an open editor buffer or a file read from disk.
pub struct Document {
    uri: Uri,
    path: Option<PathBuf>,
    text: String,
    parsed: Parse<Makefile>,
}

impl Document {
    pub fn new(uri: Uri, text: String) -> Self {
        let parsed = Makefile::parse(&text);
        let path = file_path(&uri);
        Self {
            uri,
            path,
            text,
            parsed,
        }
    }

    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// The normalized local path, for `file://` documents.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The directory containing the document, for `file://` documents.
    pub fn dir(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent)
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn parsed(&self) -> &Parse<Makefile> {
        &self.parsed
    }

    pub fn makefile(&self) -> Makefile {
        self.parsed.tree()
    }
}

/// The normalized local path of a `file://` URI.
pub fn file_path(uri: &Uri) -> Option<PathBuf> {
    if uri.scheme().as_str() != "file" {
        return None;
    }
    uri.to_file_path().map(|p| normalize(&p))
}

/// Lexically normalize a path, removing `.` components and resolving `..`
/// against the preceding component.
///
/// This deliberately doesn't resolve symlinks, so paths stay comparable with
/// the ones the editor uses.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// One file name in an include directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludePath {
    /// The file name as written, which may contain variable references.
    pub name: String,
    /// Where the name appears in the including file.
    pub range: TextRange,
    /// Whether this is a `-include` or `sinclude`, which tolerate missing files.
    pub optional: bool,
}

/// List the file names of all include directives in a makefile, including
/// those inside conditionals.
///
/// GNU make allows several names per directive; each gets its own entry.
pub fn include_paths(makefile: &Makefile, text: &str) -> Vec<IncludePath> {
    makefile
        .syntax()
        .descendants()
        .filter_map(Include::cast)
        .flat_map(|inc| {
            let optional = inc.is_optional();
            let Some(range) = inc.path_range() else {
                return Vec::new();
            };
            let raw = &text[range];
            // BSD make and nmake paths are delimited and name a single file.
            if raw.starts_with(['<', '"']) {
                return inc
                    .path()
                    .filter(|p| !p.is_empty())
                    .map(|name| IncludePath {
                        name,
                        range,
                        optional,
                    })
                    .into_iter()
                    .collect();
            }
            split_words(raw)
                .into_iter()
                .map(|(start, end)| IncludePath {
                    name: raw[start..end].replace("\\#", "#"),
                    range: TextRange::new(
                        range.start() + TextSize::from(start as u32),
                        range.start() + TextSize::from(end as u32),
                    ),
                    optional,
                })
                .collect()
        })
        .collect()
}

/// Split an include path list into words, returning byte ranges.
///
/// Whitespace inside variable references or function calls doesn't split, and
/// a backslash-newline counts as whitespace.
fn split_words(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut words = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let continuation = b == b'\\' && matches!(bytes.get(i + 1), Some(b'\n' | b'\r'));
        let separator = depth == 0 && (b.is_ascii_whitespace() || continuation);
        if separator {
            if let Some(s) = start.take() {
                words.push((s, i));
            }
        } else {
            if start.is_none() {
                start = Some(i);
            }
            match b {
                b'$' if matches!(bytes.get(i + 1), Some(b'(' | b'{')) => {
                    depth += 1;
                    i += 1;
                }
                b'(' | b'{' if depth > 0 => depth += 1,
                b')' | b'}' if depth > 0 => depth -= 1,
                _ => {}
            }
        }
        i += 1;
    }
    if let Some(s) = start {
        words.push((s, bytes.len()));
    }
    words
}

/// Variables whose value is the same plain literal everywhere they are
/// assigned, used to expand include paths like `$(TOPDIR)/rules.mk`.
///
/// This is deliberately conservative: a variable counts only if every
/// assignment seen is an unconditional `=`, `:=`, `::=` or `:::=` of the same
/// value without variable references or whitespace.
#[derive(Debug, Default)]
pub struct LiteralVariables {
    values: HashMap<String, Option<String>>,
}

impl LiteralVariables {
    pub fn add(&mut self, makefile: &Makefile) {
        for def in makefile.variable_definitions() {
            let Some(name) = def.name() else {
                continue;
            };
            let literal = literal_value(&def);
            self.values
                .entry(name)
                .and_modify(|existing| {
                    if *existing != literal {
                        *existing = None;
                    }
                })
                .or_insert(literal);
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name)?.as_deref()
    }

    fn is_assigned(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }
}

fn literal_value(def: &makefile_lossless::VariableDefinition) -> Option<String> {
    let op = def.assignment_operator()?;
    if !matches!(op.as_str(), "=" | ":=" | "::=" | ":::=") || def.is_define() {
        return None;
    }
    if def
        .syntax()
        .ancestors()
        .any(|a| a.kind() == SyntaxKind::CONDITIONAL)
    {
        return None;
    }
    let value = def.value(MakefileVariant::GNUMake)?;
    if value.is_empty() || value.contains(['$', ' ', '\t', '\n']) {
        return None;
    }
    Some(value)
}

/// Expand the variable references in an include file name, or return `None`
/// if it uses anything other than simple references to literal variables.
///
/// `CURDIR` expands to `cwd` unless the makefiles assign it.
fn expand(name: &str, vars: &LiteralVariables, cwd: Option<&Path>) -> Option<String> {
    let mut out = String::new();
    let mut rest = name;
    while let Some(idx) = rest.find('$') {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 1..];
        let close = match after.chars().next()? {
            '(' => ')',
            '{' => '}',
            _ => return None,
        };
        let end = after.find(close)?;
        let var = &after[1..end];
        if var.is_empty() || var.contains(|c: char| c.is_whitespace() || "$:,=(){}".contains(c)) {
            return None;
        }
        let value = match vars.get(var) {
            Some(v) => v.to_string(),
            None if var == "CURDIR" && !vars.is_assigned(var) => cwd?.to_str()?.to_string(),
            None => return None,
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    // TODO: support wildcards, which GNU make expands in include directives.
    if out.contains(['*', '?', '[']) {
        return None;
    }
    Some(out)
}

/// The outcome of resolving an include file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The file exists, on disk or as an open buffer.
    Found(PathBuf),
    /// The name could be resolved, but there is no such file. Holds the path
    /// make would try first.
    Missing(PathBuf),
    /// The file exists but could not be loaded.
    Unreadable(PathBuf, String),
    /// The name depends on something that can't be evaluated statically.
    Unresolved,
}

impl Resolution {
    /// How much this resolution tells us; used to pick the most informative
    /// one when a file is reached from several roots.
    fn rank(&self) -> u8 {
        match self {
            Resolution::Unresolved => 0,
            Resolution::Missing(_) => 1,
            Resolution::Unreadable(..) => 2,
            Resolution::Found(_) => 3,
        }
    }
}

/// An include file name together with what it resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInclude {
    pub path: IncludePath,
    pub resolution: Resolution,
}

/// Resolve an include file name.
///
/// GNU make resolves relative names against its working directory, which is
/// normally the directory of the top-level makefile (`cwd`). Names are also
/// tried relative to the including file's directory (`dir`), since fragments
/// are often written that way.
///
/// TODO: try `-I` directories and make's default include directories; those
/// aren't known to the server.
pub fn resolve_include(
    name: &str,
    vars: &LiteralVariables,
    cwd: Option<&Path>,
    dir: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
) -> Resolution {
    let Some(expanded) = expand(name, vars, cwd) else {
        return Resolution::Unresolved;
    };
    let expanded = Path::new(&expanded);
    let candidates: Vec<PathBuf> = if expanded.is_absolute() {
        vec![normalize(expanded)]
    } else {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for base in [cwd, dir].into_iter().flatten() {
            let candidate = normalize(&base.join(expanded));
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        candidates
    };
    if let Some(found) = candidates.iter().find(|c| exists(c)) {
        return Resolution::Found(found.clone());
    }
    match candidates.into_iter().next() {
        Some(first) => Resolution::Missing(first),
        None => Resolution::Unresolved,
    }
}

/// Why a file could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    TooLarge(u64),
    NotAFile,
    InvalidPath,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "{e}"),
            LoadError::TooLarge(len) => {
                write!(f, "file is too large ({len} bytes, limit {MAX_FILE_SIZE})")
            }
            LoadError::NotAFile => write!(f, "not a regular file"),
            LoadError::InvalidPath => write!(f, "path can't be represented as a URI"),
        }
    }
}

impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        LoadError::Io(e)
    }
}

/// The documents visible from one document.
pub struct FileSet {
    /// The current document first, then the others in the order make reads
    /// them.
    docs: Vec<Arc<Document>>,
    /// Whether each document may be edited by a rename.
    editable: Vec<bool>,
    /// The include directives of the current document.
    includes: Vec<ResolvedInclude>,
}

impl FileSet {
    /// A file set containing just `doc`, with no include information.
    #[cfg(test)]
    pub fn single(doc: Document) -> Self {
        Self {
            docs: vec![Arc::new(doc)],
            editable: vec![true],
            includes: Vec::new(),
        }
    }

    pub fn current(&self) -> &Document {
        &self.docs[0]
    }

    /// All documents, starting with the current one.
    pub fn docs(&self) -> impl Iterator<Item = &Document> {
        self.docs.iter().map(|d| d.as_ref())
    }

    /// All documents except the current one.
    pub fn others(&self) -> impl Iterator<Item = &Document> {
        self.docs.iter().skip(1).map(|d| d.as_ref())
    }

    /// Whether a rename may edit `uri`: open documents and files inside the
    /// workspace folders are editable, files elsewhere (such as system-wide
    /// makefile fragments) are not.
    pub fn is_editable(&self, uri: &Uri) -> bool {
        self.docs
            .iter()
            .zip(&self.editable)
            .any(|(d, e)| *e && d.uri() == uri)
    }

    /// The include directives of the current document.
    pub fn includes(&self) -> &[ResolvedInclude] {
        &self.includes
    }

    /// The include file name at `offset` in the current document.
    pub fn include_at(&self, offset: TextSize) -> Option<&ResolvedInclude> {
        self.includes
            .iter()
            .find(|i| i.path.range.contains_inclusive(offset))
    }
}

struct CachedFile {
    mtime: SystemTime,
    len: u64,
    doc: Arc<Document>,
}

/// State for one traversal of include directives from a root makefile.
#[derive(Default)]
struct Walk {
    docs: Vec<Arc<Document>>,
    seen: HashSet<PathBuf>,
    failed: HashMap<PathBuf, String>,
    vars: LiteralVariables,
    includes: HashMap<Uri, Vec<ResolvedInclude>>,
    truncated: bool,
}

/// The open documents plus everything known about the makefiles they include
/// or are included by.
#[derive(Default)]
pub struct Workspace {
    open: HashMap<Uri, Arc<Document>>,
    open_paths: HashMap<PathBuf, Uri>,
    disk: HashMap<PathBuf, CachedFile>,
    /// Files each file includes, as of the last time it was visited.
    includes: HashMap<PathBuf, BTreeSet<PathBuf>>,
    /// The reverse of `includes`.
    included_by: HashMap<PathBuf, BTreeSet<PathBuf>>,
    /// Workspace folders.
    roots: Vec<PathBuf>,
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots.iter().map(|r| normalize(r)).collect();
    }

    /// Add or replace an open editor buffer.
    pub fn open(&mut self, uri: Uri, text: String) -> Arc<Document> {
        let doc = Arc::new(Document::new(uri.clone(), text));
        if let Some(path) = doc.path() {
            self.open_paths.insert(path.to_path_buf(), uri.clone());
        }
        self.open.insert(uri, doc.clone());
        doc
    }

    /// Forget an open editor buffer; the file on disk is used from now on.
    pub fn close(&mut self, uri: &Uri) {
        if let Some(path) = self.open.remove(uri).and_then(|d| d.path.clone()) {
            self.open_paths.remove(&path);
        }
    }

    pub fn document(&self, uri: &Uri) -> Option<Arc<Document>> {
        self.open.get(uri).cloned()
    }

    /// Build the file set for an open document.
    pub fn file_set(&mut self, uri: &Uri) -> Option<FileSet> {
        let current = self.open.get(uri)?.clone();
        let mut walk = Walk::default();
        if let Some(path) = current.path() {
            walk.seen.insert(path.to_path_buf());
        }
        self.visit(current.clone(), current.dir(), &mut walk);
        let mut docs = walk.docs;
        let mut includes = walk.includes.remove(uri).unwrap_or_default();

        if let Some(path) = current.path() {
            for root in self.roots_including(path) {
                let Some(root_walk) = self.walk_from(&root) else {
                    continue;
                };
                // The edge to this file may have been removed since it was
                // recorded.
                if !root_walk.docs.iter().any(|d| d.path() == Some(path)) {
                    continue;
                }
                for doc in root_walk.docs {
                    if !docs.iter().any(|d| d.uri() == doc.uri()) {
                        docs.push(doc);
                    }
                }
                if let Some(root_includes) = root_walk.includes.get(uri) {
                    merge_includes(&mut includes, root_includes);
                }
            }
        }

        let editable = docs.iter().map(|d| self.is_editable(d)).collect();
        Some(FileSet {
            docs,
            editable,
            includes,
        })
    }

    fn is_editable(&self, doc: &Document) -> bool {
        if self.open.contains_key(doc.uri()) || self.roots.is_empty() {
            return true;
        }
        doc.path()
            .is_some_and(|p| self.roots.iter().any(|r| p.starts_with(r)))
    }

    /// The topmost known makefiles that (transitively) include `path`.
    fn roots_including(&self, path: &Path) -> Vec<PathBuf> {
        let mut ancestors: BTreeSet<PathBuf> = BTreeSet::new();
        let mut queue: Vec<&Path> = vec![path];
        while let Some(p) = queue.pop() {
            for parent in self.included_by.get(p).into_iter().flatten() {
                if parent != path && ancestors.insert(parent.clone()) {
                    queue.push(parent);
                }
            }
        }
        let tops: Vec<PathBuf> = ancestors
            .iter()
            .filter(|a| self.included_by.get(*a).is_none_or(|s| s.is_empty()))
            .cloned()
            .collect();
        // With an include cycle above this file there may be no top.
        if tops.is_empty() {
            ancestors.into_iter().collect()
        } else {
            tops
        }
    }

    /// Follow the includes of the makefile at `path`.
    fn walk_from(&mut self, path: &Path) -> Option<Walk> {
        let doc = match self.load(path) {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!("unable to load {}: {e}", path.display());
                return None;
            }
        };
        let mut walk = Walk::default();
        walk.seen.insert(path.to_path_buf());
        self.visit(doc, path.parent(), &mut walk);
        Some(walk)
    }

    fn visit(&mut self, doc: Arc<Document>, cwd: Option<&Path>, walk: &mut Walk) {
        if walk.docs.len() >= MAX_FILES {
            if !walk.truncated {
                tracing::warn!(
                    "not following includes beyond {MAX_FILES} files (at {})",
                    doc.uri().as_str()
                );
                walk.truncated = true;
            }
            return;
        }
        walk.docs.push(doc.clone());
        let makefile = doc.makefile();
        walk.vars.add(&makefile);

        let mut resolved = Vec::new();
        let mut children = BTreeSet::new();
        for path in include_paths(&makefile, doc.text()) {
            let exists = |p: &Path| self.open_paths.contains_key(p) || p.is_file();
            let mut resolution = resolve_include(&path.name, &walk.vars, cwd, doc.dir(), &exists);
            if let Resolution::Found(target) = &resolution {
                let target = target.clone();
                children.insert(target.clone());
                if walk.seen.insert(target.clone()) {
                    match self.load(&target) {
                        Ok(child) => self.visit(child, cwd, walk),
                        Err(e) => {
                            tracing::warn!("unable to load {}: {e}", target.display());
                            walk.failed.insert(target.clone(), e.to_string());
                        }
                    }
                }
                if let Some(error) = walk.failed.get(&target) {
                    resolution = Resolution::Unreadable(target, error.clone());
                }
            }
            resolved.push(ResolvedInclude { path, resolution });
        }

        if let Some(path) = doc.path() {
            self.set_includes(path, children);
        }
        walk.includes.insert(doc.uri().clone(), resolved);
    }

    fn set_includes(&mut self, path: &Path, children: BTreeSet<PathBuf>) {
        let old = self
            .includes
            .insert(path.to_path_buf(), children.clone())
            .unwrap_or_default();
        for removed in old.difference(&children) {
            if let Some(parents) = self.included_by.get_mut(removed) {
                parents.remove(path);
            }
        }
        for child in children {
            self.included_by
                .entry(child)
                .or_default()
                .insert(path.to_path_buf());
        }
    }

    /// Get the document for `path`, preferring an open buffer and otherwise
    /// reading it from disk unless the cached copy is still current.
    fn load(&mut self, path: &Path) -> Result<Arc<Document>, LoadError> {
        if let Some(doc) = self.open_paths.get(path).and_then(|u| self.open.get(u)) {
            return Ok(doc.clone());
        }
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(LoadError::NotAFile);
        }
        if meta.len() > MAX_FILE_SIZE {
            return Err(LoadError::TooLarge(meta.len()));
        }
        let mtime = meta.modified()?;
        if let Some(cached) = self.disk.get(path) {
            if cached.mtime == mtime && cached.len == meta.len() {
                return Ok(cached.doc.clone());
            }
        }
        let text = std::fs::read_to_string(path)?;
        let uri = Uri::from_file_path(path).ok_or(LoadError::InvalidPath)?;
        let doc = Arc::new(Document::new(uri, text));
        self.disk.insert(
            path.to_path_buf(),
            CachedFile {
                mtime,
                len: meta.len(),
                doc: doc.clone(),
            },
        );
        Ok(doc)
    }
}

/// Replace entries in `into` with more informative ones from `from`.
fn merge_includes(into: &mut [ResolvedInclude], from: &[ResolvedInclude]) {
    for (a, b) in into.iter_mut().zip(from) {
        if a.path == b.path && b.resolution.rank() > a.resolution.rank() {
            a.resolution = b.resolution.clone();
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A temporary directory with makefiles in it.
    pub struct Fixture {
        pub dir: tempfile::TempDir,
    }

    impl Fixture {
        pub fn new(files: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            for (name, text) in files {
                let path = dir.path().join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            Self { dir }
        }

        pub fn path(&self, name: &str) -> PathBuf {
            normalize(&self.dir.path().join(name))
        }

        pub fn uri(&self, name: &str) -> Uri {
            Uri::from_file_path(self.path(name)).unwrap()
        }

        /// A workspace with `name` opened from disk.
        pub fn open(&self, name: &str) -> (Workspace, Uri) {
            let mut ws = Workspace::new();
            let uri = self.open_in(&mut ws, name);
            (ws, uri)
        }

        pub fn open_in(&self, ws: &mut Workspace, name: &str) -> Uri {
            let uri = self.uri(name);
            let text = std::fs::read_to_string(self.path(name)).unwrap();
            ws.open(uri.clone(), text);
            uri
        }

        /// The file set for `name`, opened from disk.
        pub fn file_set(&self, name: &str) -> FileSet {
            let (mut ws, uri) = self.open(name);
            ws.file_set(&uri).unwrap()
        }

        /// Paths of the documents in `set`, relative to the fixture.
        pub fn names(&self, set: &FileSet) -> Vec<String> {
            set.docs
                .iter()
                .map(|d| {
                    d.path()
                        .unwrap()
                        .strip_prefix(self.path(""))
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }
    }

    fn names_of(text: &str) -> Vec<(String, bool)> {
        let parsed = Makefile::parse(text);
        include_paths(&parsed.tree(), text)
            .into_iter()
            .map(|p| {
                assert_eq!(&text[p.range], p.name.replace('#', "\\#"));
                (p.name, p.optional)
            })
            .collect()
    }

    #[test]
    fn test_include_paths_single() {
        assert_eq!(
            names_of("include a.mk\n"),
            vec![("a.mk".to_string(), false)]
        );
    }

    #[test]
    fn test_include_paths_several() {
        assert_eq!(
            names_of("-include a.mk  b.mk\n"),
            vec![("a.mk".to_string(), true), ("b.mk".to_string(), true)]
        );
    }

    #[test]
    fn test_include_paths_function_call_is_one_word() {
        assert_eq!(
            names_of("include $(wildcard *.mk) x.mk\n"),
            vec![
                ("$(wildcard *.mk)".to_string(), false),
                ("x.mk".to_string(), false)
            ]
        );
    }

    #[test]
    fn test_include_paths_in_conditional() {
        assert_eq!(
            names_of("ifdef X\nsinclude a.mk\nendif\n"),
            vec![("a.mk".to_string(), true)]
        );
    }

    #[test]
    fn test_include_paths_empty() {
        assert_eq!(names_of("include\n"), vec![]);
    }

    fn vars(text: &str) -> LiteralVariables {
        let mut vars = LiteralVariables::default();
        vars.add(&Makefile::parse(text).tree());
        vars
    }

    #[test]
    fn test_literal_variables() {
        let v = vars("A = x\nB := $(A)\nC ?= y\nD = 1\nD = 2\nE = a b\nifdef Q\nF = f\nendif\nG = g\nG = g\n");
        assert_eq!(v.get("A"), Some("x"));
        assert_eq!(v.get("B"), None);
        assert_eq!(v.get("C"), None);
        assert_eq!(v.get("D"), None);
        assert_eq!(v.get("E"), None);
        assert_eq!(v.get("F"), None);
        assert_eq!(v.get("G"), Some("g"));
    }

    #[test]
    fn test_expand() {
        let v = vars("TOP = ../top\n");
        let cwd = Path::new("/src");
        assert_eq!(
            expand("$(TOP)/rules.mk", &v, Some(cwd)),
            Some("../top/rules.mk".to_string())
        );
        assert_eq!(
            expand("${TOP}/x", &v, Some(cwd)),
            Some("../top/x".to_string())
        );
        assert_eq!(
            expand("$(CURDIR)/x.mk", &v, Some(cwd)),
            Some("/src/x.mk".to_string())
        );
        assert_eq!(expand("$(OTHER)/x.mk", &v, Some(cwd)), None);
        assert_eq!(expand("$(wildcard x)", &v, Some(cwd)), None);
        assert_eq!(expand("*.mk", &v, Some(cwd)), None);
        assert_eq!(expand("$@", &v, Some(cwd)), None);
    }

    #[test]
    fn test_normalize() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(normalize(Path::new("a/../../b")), PathBuf::from("../b"));
    }

    #[test]
    fn test_resolve_prefers_cwd() {
        let v = LiteralVariables::default();
        let exists = |p: &Path| p == Path::new("/top/x.mk") || p == Path::new("/top/sub/x.mk");
        assert_eq!(
            resolve_include(
                "x.mk",
                &v,
                Some(Path::new("/top")),
                Some(Path::new("/top/sub")),
                &exists
            ),
            Resolution::Found(PathBuf::from("/top/x.mk"))
        );
        assert_eq!(
            resolve_include(
                "x.mk",
                &v,
                Some(Path::new("/elsewhere")),
                Some(Path::new("/top/sub")),
                &exists
            ),
            Resolution::Found(PathBuf::from("/top/sub/x.mk"))
        );
        assert_eq!(
            resolve_include("y.mk", &v, Some(Path::new("/top")), None, &exists),
            Resolution::Missing(PathBuf::from("/top/y.mk"))
        );
        assert_eq!(
            resolve_include("y.mk", &v, None, None, &exists),
            Resolution::Unresolved
        );
    }

    #[test]
    fn test_file_set_follows_includes() {
        let fx = Fixture::new(&[
            ("Makefile", "include a.mk\n"),
            ("a.mk", "include sub/b.mk\n"),
            ("sub/b.mk", "X = 1\n"),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile", "a.mk", "sub/b.mk"]);
        assert_eq!(
            set.includes()
                .iter()
                .map(|i| i.resolution.clone())
                .collect::<Vec<_>>(),
            vec![Resolution::Found(fx.path("a.mk"))]
        );
    }

    #[test]
    fn test_file_set_cycle() {
        let fx = Fixture::new(&[
            ("Makefile", "include a.mk\n"),
            ("a.mk", "include Makefile\ninclude a.mk\n"),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile", "a.mk"]);
    }

    #[test]
    fn test_file_set_expands_literal_variable() {
        let fx = Fixture::new(&[
            ("Makefile", "TOP := mk\ninclude $(TOP)/rules.mk\n"),
            ("mk/rules.mk", "R = 1\n"),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile", "mk/rules.mk"]);
    }

    #[test]
    fn test_file_set_missing_and_unresolved() {
        let fx = Fixture::new(&[("Makefile", "include nope.mk\ninclude $(shell ls)\n")]);
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile"]);
        assert_eq!(
            set.includes()
                .iter()
                .map(|i| i.resolution.clone())
                .collect::<Vec<_>>(),
            vec![
                Resolution::Missing(fx.path("nope.mk")),
                Resolution::Unresolved
            ]
        );
    }

    #[test]
    fn test_file_set_unreadable() {
        let fx = Fixture::new(&[("Makefile", "include dir.mk\n")]);
        std::fs::create_dir(fx.path("dir.mk")).unwrap();
        let set = fx.file_set("Makefile");
        // A directory isn't a file, so it's reported as missing like make does.
        assert_eq!(
            set.includes()[0].resolution,
            Resolution::Missing(fx.path("dir.mk"))
        );

        let fx = Fixture::new(&[("Makefile", "include bad.mk\n"), ("bad.mk", "")]);
        std::fs::write(fx.path("bad.mk"), [0xff, 0xfe]).unwrap();
        let set = fx.file_set("Makefile");
        assert_eq!(
            set.includes()[0].resolution,
            Resolution::Unreadable(
                fx.path("bad.mk"),
                "stream did not contain valid UTF-8".to_string()
            )
        );
    }

    #[test]
    fn test_open_buffer_preferred_over_disk() {
        let fx = Fixture::new(&[("Makefile", "include a.mk\n"), ("a.mk", "X = disk\n")]);
        let (mut ws, uri) = fx.open("Makefile");
        ws.open(fx.uri("a.mk"), "X = buffer\n".to_string());
        let set = ws.file_set(&uri).unwrap();
        assert_eq!(set.docs[1].text(), "X = buffer\n");

        ws.close(&fx.uri("a.mk"));
        let set = ws.file_set(&uri).unwrap();
        assert_eq!(set.docs[1].text(), "X = disk\n");
    }

    #[test]
    fn test_disk_cache_reloads_on_change() {
        let fx = Fixture::new(&[("Makefile", "include a.mk\n"), ("a.mk", "X = 1\n")]);
        let (mut ws, uri) = fx.open("Makefile");
        ws.file_set(&uri).unwrap();
        std::fs::write(fx.path("a.mk"), "X = 22\n").unwrap();
        let set = ws.file_set(&uri).unwrap();
        assert_eq!(set.docs[1].text(), "X = 22\n");
    }

    #[test]
    fn test_included_file_sees_includer() {
        let fx = Fixture::new(&[
            ("Makefile", "include a.mk b.mk\n"),
            ("a.mk", "A = 1\n"),
            ("b.mk", "B = 1\n"),
        ]);
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        let a = fx.open_in(&mut ws, "a.mk");
        let set = ws.file_set(&a).unwrap();
        assert_eq!(fx.names(&set), vec!["a.mk", "Makefile", "b.mk"]);
    }

    #[test]
    fn test_removed_include_drops_includer() {
        let fx = Fixture::new(&[("Makefile", "include a.mk\n"), ("a.mk", "A = 1\n")]);
        let (mut ws, makefile) = fx.open("Makefile");
        ws.file_set(&makefile).unwrap();
        ws.open(makefile.clone(), "all:\n".to_string());
        ws.file_set(&makefile).unwrap();
        let a = fx.open_in(&mut ws, "a.mk");
        let set = ws.file_set(&a).unwrap();
        assert_eq!(fx.names(&set), vec!["a.mk"]);
    }

    #[test]
    fn test_editable() {
        let fx = Fixture::new(&[
            ("ws/Makefile", "include ../outside.mk inside.mk\n"),
            ("ws/inside.mk", ""),
            ("outside.mk", ""),
        ]);
        let mut ws = Workspace::new();
        ws.set_roots(vec![fx.path("ws")]);
        let uri = fx.open_in(&mut ws, "ws/Makefile");
        let set = ws.file_set(&uri).unwrap();
        assert!(set.is_editable(&uri));
        assert!(set.is_editable(&fx.uri("ws/inside.mk")));
        assert!(!set.is_editable(&fx.uri("outside.mk")));
    }
}
