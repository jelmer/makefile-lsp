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

use makefile_lossless::{
    split_references, Include, IncludeKind, Makefile, MakefileVariant, Parse, TextPart, TextRange,
    TextSize,
};
use tower_lsp_server::ls_types::Uri;

/// Upper bound on the number of files visited when following includes.
const MAX_FILES: usize = 256;

/// Included files larger than this are not loaded.
const MAX_FILE_SIZE: u64 = 4 * 1024 * 1024;

/// Upper bound on the number of documents searched for workspace symbols.
const MAX_WORKSPACE_FILES: usize = 1024;

/// Upper bound on the directory entries looked at when searching the
/// workspace folders for makefiles.
const MAX_SCAN_ENTRIES: usize = 20_000;

/// The names make looks for when run without `-f`, in order.
const DEFAULT_MAKEFILES: &[&str] = &["GNUmakefile", "makefile", "Makefile"];

/// How many directories above a document to look for a makefile that may
/// include it, when the document isn't inside a workspace folder.
const MAX_PROBE_DEPTH: usize = 2;

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

    pub fn variant(&self) -> MakefileVariant {
        parsed_variant(&self.parsed)
    }
}

/// The make variant `parsed` was parsed for. [`Makefile::parse`] accepts
/// both GNU and BSD syntax without recording a variant; references in its
/// result end where GNU make ends them, so it is treated as GNU make.
pub fn parsed_variant(parsed: &Parse<Makefile>) -> MakefileVariant {
    parsed.variant().unwrap_or(MakefileVariant::GNUMake)
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
    /// Whether this is a GNU make include, whose expanded name make splits
    /// into file names, unescaping blanks.
    pub gnu: bool,
}

/// List the file names of all include directives in a makefile, including
/// those inside conditionals.
///
/// GNU make allows several names per directive; each gets its own entry.
pub fn include_paths(makefile: &Makefile) -> Vec<IncludePath> {
    makefile
        .includes()
        .flat_map(|inc| {
            let optional = inc.is_optional();
            let gnu = matches!(
                inc.include_kind(),
                Some(IncludeKind::Include | IncludeKind::DashInclude | IncludeKind::Sinclude)
            );
            inc.paths()
                .zip(inc.path_ranges())
                .map(|(name, range)| IncludePath {
                    name,
                    range,
                    optional,
                    gnu,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Variables whose value is the same plain literal everywhere they are
/// assigned, used to expand include paths like `$(TOPDIR)/rules.mk`.
///
/// This is deliberately conservative: a variable counts only if every
/// assignment seen is an unconditional, global `=`, `:=`, `::=` or `:::=` of
/// the same value without variable references or whitespace.
#[derive(Debug, Default)]
pub struct LiteralVariables {
    values: HashMap<String, Option<String>>,
}

impl LiteralVariables {
    pub fn add(&mut self, makefile: &Makefile, variant: MakefileVariant) {
        for def in makefile.variable_definitions() {
            let Some(name) = def.name() else {
                continue;
            };
            let literal = literal_value(&def, variant);
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

fn literal_value(
    def: &makefile_lossless::VariableDefinition,
    variant: MakefileVariant,
) -> Option<String> {
    let op = def.assignment_operator()?;
    if !matches!(op.as_str(), "=" | ":=" | "::=" | ":::=") || def.is_define() {
        return None;
    }
    if def.is_target_specific() || !def.enclosing_branches().is_empty() {
        return None;
    }
    let value = def.value_for(variant)?;
    if value.is_empty() || value.contains(['$', ' ', '\t', '\n']) {
        return None;
    }
    Some(value)
}

/// Expand the variable references in an include file name, or return `None`
/// if it uses anything other than simple references to literal variables.
///
/// `CURDIR` expands to `cwd` unless the makefiles assign it.
fn expand(
    name: &str,
    vars: &LiteralVariables,
    variant: MakefileVariant,
    cwd: Option<&Path>,
) -> Option<String> {
    let mut out = String::new();
    for part in split_references(name, variant) {
        let reference = match part {
            TextPart::Literal(range) => {
                out.push_str(&name[range]);
                continue;
            }
            TextPart::Reference {
                parsed: Ok(reference),
                ..
            } if reference.modifiers.is_empty() && !reference.name.contains('$') => reference,
            _ => return None,
        };
        let var = reference.name.as_str();
        let value = match vars.get(var) {
            Some(v) => v.to_string(),
            None if var == "CURDIR" && !vars.is_assigned(var) => cwd?.to_str()?.to_string(),
            None => return None,
        };
        out.push_str(&value);
    }
    Some(out)
}

/// Upper bound on the files one wildcard in an include file name may match.
const MAX_GLOB_MATCHES: usize = MAX_FILES;

/// Upper bound on the directory entries read while expanding one wildcard.
const MAX_GLOB_ENTRIES: usize = 10_000;

/// Whether `name` contains an unescaped wildcard character.
pub fn has_wildcard(name: &str) -> bool {
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

/// One element of a file name pattern.
#[derive(Debug, PartialEq, Eq)]
enum GlobToken {
    Literal(char),
    /// `?`
    Any,
    /// `*`
    Star,
    /// `[...]`: inclusive character ranges, and whether the set is negated.
    Class(Vec<(char, char)>, bool),
}

impl GlobToken {
    fn matches(&self, c: char) -> bool {
        match self {
            GlobToken::Literal(l) => *l == c,
            GlobToken::Any => true,
            GlobToken::Star => false,
            GlobToken::Class(ranges, negated) => {
                ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&c)) != *negated
            }
        }
    }
}

/// Split one path component of a pattern into tokens, as glob(3) reads it:
/// a backslash quotes the next character and a `[` without a closing `]`
/// is literal.
///
/// TODO: support POSIX character classes such as `[[:alpha:]]`.
fn glob_tokens(pattern: &str) -> Vec<GlobToken> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let token = match chars[i] {
            '\\' if i + 1 < chars.len() => {
                i += 1;
                GlobToken::Literal(chars[i])
            }
            '*' => GlobToken::Star,
            '?' => GlobToken::Any,
            '[' => match glob_class(&chars[i + 1..]) {
                Some((class, len)) => {
                    i += len;
                    class
                }
                None => GlobToken::Literal('['),
            },
            c => GlobToken::Literal(c),
        };
        tokens.push(token);
        i += 1;
    }
    tokens
}

/// Parse the rest of a `[...]` set, returning it and the number of
/// characters it used including the closing `]`.
fn glob_class(chars: &[char]) -> Option<(GlobToken, usize)> {
    let mut i = 0;
    let negated = matches!(chars.first(), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let mut lo = *chars.get(i)?;
        if lo == ']' && !first {
            return Some((GlobToken::Class(ranges, negated), i + 1));
        }
        first = false;
        if lo == '\\' {
            i += 1;
            lo = *chars.get(i)?;
        }
        i += 1;
        let mut hi = lo;
        if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(|c| *c != ']') {
            i += 1;
            hi = chars[i];
            if hi == '\\' {
                i += 1;
                hi = *chars.get(i)?;
            }
            i += 1;
        }
        ranges.push((lo, hi));
    }
}

/// Whether the file name `name` matches the pattern `tokens`.
///
/// As with glob(3), a leading `.` must be matched by a literal `.`.
fn glob_match(tokens: &[GlobToken], name: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    if name.first() == Some(&'.') && tokens.first() != Some(&GlobToken::Literal('.')) {
        return false;
    }
    let (mut t, mut n) = (0, 0);
    // Where to resume after the last `*`: the token after it and the
    // position in `name` it has consumed up to.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match tokens.get(t) {
            Some(GlobToken::Star) => {
                star = Some((t + 1, n));
                t += 1;
                continue;
            }
            Some(token) if token.matches(name[n]) => {
                t += 1;
                n += 1;
                continue;
            }
            _ => {}
        }
        let Some((resume, consumed)) = star else {
            return false;
        };
        t = resume;
        n = consumed + 1;
        star = Some((resume, consumed + 1));
    }
    tokens[t..].iter().all(|t| *t == GlobToken::Star)
}

/// More files or directory entries than the limits allow.
#[derive(Debug, PartialEq, Eq)]
struct TooManyMatches;

/// Expand the wildcards in `pattern` like glob(3) does for GNU make, with
/// relative patterns taken relative to `base`. The result is sorted by
/// byte value; make sorts it according to the locale.
///
/// Directories that can't be read contribute no matches, as with glob(3).
fn glob(base: &Path, pattern: &str) -> Result<Vec<PathBuf>, TooManyMatches> {
    let mut paths = vec![if pattern.starts_with('/') {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    }];
    let mut budget = MAX_GLOB_ENTRIES;
    for component in pattern.split('/').filter(|c| !c.is_empty()) {
        let tokens = glob_tokens(component);
        let literal: Option<String> = tokens
            .iter()
            .map(|t| match t {
                GlobToken::Literal(c) => Some(*c),
                _ => None,
            })
            .collect();
        if let Some(literal) = literal {
            for path in &mut paths {
                path.push(&literal);
            }
            continue;
        }
        let mut matched = Vec::new();
        for dir in &paths {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                budget = budget.checked_sub(1).ok_or(TooManyMatches)?;
                let name = entry.file_name();
                // TODO: match names that aren't valid UTF-8.
                if name.to_str().is_some_and(|n| glob_match(&tokens, n)) {
                    matched.push(dir.join(name));
                }
            }
        }
        if matched.len() > MAX_GLOB_MATCHES {
            return Err(TooManyMatches);
        }
        paths = matched;
    }
    paths.retain(|p| p.exists());
    let mut paths: Vec<PathBuf> = paths.iter().map(|p| normalize(p)).collect();
    paths.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    paths.dedup();
    Ok(paths)
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

/// An include file name together with what it resolved to. A name with
/// wildcards that match several files has one of these per file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInclude {
    pub path: IncludePath,
    pub resolution: Resolution,
}

/// Resolve an include file name, reading its references as `variant`, the
/// variant the including file was parsed as.
///
/// GNU make resolves relative names against its working directory, which is
/// normally the directory of the top-level makefile (`cwd`). Names are also
/// tried relative to the including file's directory (`dir`), since fragments
/// are often written that way.
///
/// GNU make expands wildcards in the name, giving one resolution per
/// matching file. A pattern that matches nothing is used as a file name.
///
/// TODO: try `-I` directories and make's default include directories; those
/// aren't known to the server.
pub fn resolve_include(
    path: &IncludePath,
    vars: &LiteralVariables,
    variant: MakefileVariant,
    cwd: Option<&Path>,
    dir: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<Resolution> {
    let Some(mut expanded) = expand(&path.name, vars, variant, cwd) else {
        return vec![Resolution::Unresolved];
    };
    if path.gnu {
        // `path` is a single name and literal values contain no blanks, so
        // this only unescapes blanks.
        let Ok([name]) = <[String; 1]>::try_from(Include::split_file_names(&expanded)) else {
            return vec![Resolution::Unresolved];
        };
        expanded = name;
        if has_wildcard(&expanded) {
            let mut bases: Vec<&Path> = [cwd, dir].into_iter().flatten().collect();
            bases.dedup();
            if Path::new(&expanded).is_absolute() {
                bases.truncate(1);
            }
            for base in bases {
                match glob(base, &expanded) {
                    Ok(matches) if matches.is_empty() => {}
                    Ok(matches) => return matches.into_iter().map(Resolution::Found).collect(),
                    Err(TooManyMatches) => {
                        tracing::warn!("too many files match {expanded} in {}", base.display());
                        return vec![Resolution::Unresolved];
                    }
                }
            }
        }
    }
    vec![resolve_name(Path::new(&expanded), cwd, dir, exists)]
}

fn resolve_name(
    expanded: &Path,
    cwd: Option<&Path>,
    dir: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
) -> Resolution {
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
    /// Whether every include directive in the documents was resolved to a
    /// file that could be loaded.
    complete: bool,
}

impl FileSet {
    /// A file set containing just `doc`, with no include information.
    #[cfg(test)]
    pub fn single(doc: Document) -> Self {
        Self {
            docs: vec![Arc::new(doc)],
            editable: vec![true],
            includes: Vec::new(),
            complete: false,
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

    /// Whether the set holds every makefile that its documents include, so
    /// that nothing is defined in files it doesn't know about.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// The resolutions of the include file name at `offset` in the current
    /// document.
    pub fn includes_at(&self, offset: TextSize) -> impl Iterator<Item = &ResolvedInclude> {
        self.includes
            .iter()
            .filter(move |i| i.path.range.contains_inclusive(offset))
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

impl Walk {
    /// Whether every include directive seen was followed.
    fn is_complete(&self) -> bool {
        !self.truncated
            && self
                .includes
                .values()
                .flatten()
                .all(|i| matches!(i.resolution, Resolution::Found(_)))
    }
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
    /// Makefiles checked for includes of nearby files, with their mtime then.
    probed: HashMap<PathBuf, SystemTime>,
    /// The paths in the last file set built for each open document.
    visible: HashMap<Uri, BTreeSet<PathBuf>>,
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
        self.visible.remove(uri);
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
        let files = self.build_file_set(current);
        self.visible.insert(
            uri.clone(),
            files
                .docs()
                .filter_map(|d| d.path().map(Path::to_path_buf))
                .collect(),
        );
        Some(files)
    }

    /// Build the file set for the makefile at `path`, which must be
    /// absolute. An open buffer for it is used if there is one, otherwise it
    /// is read from disk.
    pub fn file_set_for_path(&mut self, path: &Path) -> Result<FileSet, LoadError> {
        let current = self.load(&normalize(path))?;
        Ok(self.build_file_set(current))
    }

    fn build_file_set(&mut self, current: Arc<Document>) -> FileSet {
        let uri = current.uri().clone();
        let mut walk = Walk::default();
        if let Some(path) = current.path() {
            walk.seen.insert(path.to_path_buf());
        }
        self.visit(current.clone(), current.dir(), &mut walk);
        let mut complete = walk.is_complete();
        let mut docs = walk.docs;
        let mut includes = walk.includes.remove(&uri).unwrap_or_default();

        if let Some(path) = current.path() {
            self.probe_includers(path);
            for root in self.roots_including(path) {
                let Some(root_walk) = self.walk_from(&root) else {
                    continue;
                };
                // The edge to this file may have been removed since it was
                // recorded.
                if !root_walk.docs.iter().any(|d| d.path() == Some(path)) {
                    continue;
                }
                complete &= root_walk.is_complete();
                for doc in root_walk.docs {
                    if !docs.iter().any(|d| d.uri() == doc.uri()) {
                        docs.push(doc);
                    }
                }
                if let Some(root_includes) = root_walk.includes.get(&uri) {
                    merge_includes(&mut includes, root_includes);
                }
            }
        }

        let editable = docs.iter().map(|d| self.is_editable(d)).collect();
        FileSet {
            docs,
            editable,
            includes,
            complete,
        }
    }

    /// Open documents other than `uri` whose diagnostics may depend on it:
    /// those that saw it in their last file set, and those in its own last
    /// file set.
    pub fn dependents(&self, uri: &Uri) -> Vec<Uri> {
        let own = self.visible.get(uri);
        let path = file_path(uri);
        let mut dependents: Vec<Uri> = self
            .open
            .values()
            .filter(|doc| doc.uri() != uri)
            .filter(|doc| {
                let seen_by_doc = path
                    .as_ref()
                    .is_some_and(|p| self.visible.get(doc.uri()).is_some_and(|v| v.contains(p)));
                let seen_by_uri = doc
                    .path()
                    .is_some_and(|p| own.is_some_and(|v| v.contains(p)));
                seen_by_doc || seen_by_uri
            })
            .map(|doc| doc.uri().clone())
            .collect();
        dependents.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        dependents
    }

    /// Open documents whose last file set contained `path`.
    pub fn open_documents_seeing(&self, path: &Path) -> Vec<Uri> {
        let mut uris: Vec<Uri> = self
            .visible
            .iter()
            .filter(|(_, paths)| paths.contains(path))
            .map(|(uri, _)| uri.clone())
            .collect();
        uris.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        uris
    }

    pub fn open_documents(&self) -> Vec<Uri> {
        let mut uris: Vec<Uri> = self.open.keys().cloned().collect();
        uris.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        uris
    }

    /// The open documents and the makefiles in the workspace folders, each
    /// followed by the files it includes.
    ///
    /// Open documents come first, together with the makefiles that include
    /// them. The search of the workspace folders skips hidden directories
    /// and stops after `MAX_SCAN_ENTRIES` directory entries; at most
    /// `MAX_WORKSPACE_FILES` documents are returned.
    pub fn all_documents(&mut self) -> Vec<Arc<Document>> {
        let mut docs: Vec<Arc<Document>> = Vec::new();
        let mut seen: HashSet<Uri> = HashSet::new();
        let mut add = |docs: &mut Vec<Arc<Document>>, found: Vec<Arc<Document>>| {
            for doc in found {
                if seen.insert(doc.uri().clone()) {
                    docs.push(doc);
                }
            }
        };
        for uri in self.open_documents() {
            let current = self.open[&uri].clone();
            let files = self.build_file_set(current);
            add(&mut docs, files.docs);
        }
        for path in find_makefiles(&self.roots, MAX_SCAN_ENTRIES) {
            if docs.len() >= MAX_WORKSPACE_FILES {
                tracing::warn!("not looking at more than {MAX_WORKSPACE_FILES} workspace files");
                break;
            }
            if docs.iter().any(|d| d.path() == Some(&path)) {
                continue;
            }
            if let Some(walk) = self.walk_from(&path) {
                add(&mut docs, walk.docs);
            }
        }
        docs.truncate(MAX_WORKSPACE_FILES);
        docs
    }

    /// A name to show for `doc`: its path relative to the innermost
    /// workspace folder containing it, its full path when it is outside the
    /// workspace folders, or its URI when it isn't a local file.
    pub fn display_name(&self, doc: &Document) -> String {
        let Some(path) = doc.path() else {
            return doc.uri().as_str().to_string();
        };
        self.roots
            .iter()
            .filter_map(|r| path.strip_prefix(r).ok())
            .min_by_key(|p| p.components().count())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// Drop the cached copy of a file that changed on disk.
    pub fn invalidate(&mut self, path: &Path) {
        self.disk.remove(&normalize(path));
    }

    /// Look for makefiles that may include `path` in its directory and the
    /// ones above it, and record what they include.
    ///
    /// The search stops at the workspace folder containing `path`, or after
    /// `MAX_PROBE_DEPTH` levels if there is none. Each makefile is only
    /// re-read when it changes.
    fn probe_includers(&mut self, path: &Path) {
        let root = self
            .roots
            .iter()
            .filter(|r| path.starts_with(r))
            .max_by_key(|r| r.components().count())
            .cloned();
        let mut depth = 0;
        let mut dir = path.parent();
        while let Some(d) = dir {
            if let Some(candidate) = DEFAULT_MAKEFILES
                .iter()
                .map(|name| d.join(name))
                .find(|c| c.is_file())
            {
                if candidate != path && !self.open_paths.contains_key(&candidate) {
                    self.probe(&candidate);
                }
            }
            if root.as_deref() == Some(d) || (root.is_none() && depth >= MAX_PROBE_DEPTH) {
                break;
            }
            depth += 1;
            dir = d.parent();
        }
    }

    fn probe(&mut self, makefile: &Path) {
        let mtime = match std::fs::metadata(makefile).and_then(|m| m.modified()) {
            Ok(mtime) => mtime,
            Err(e) => {
                tracing::warn!("unable to stat {}: {e}", makefile.display());
                return;
            }
        };
        if self.probed.get(makefile) == Some(&mtime) {
            return;
        }
        self.probed.insert(makefile.to_path_buf(), mtime);
        self.walk_from(makefile);
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
        walk.vars.add(&makefile, doc.variant());

        let mut resolved = Vec::new();
        let mut children = BTreeSet::new();
        for path in include_paths(&makefile) {
            let exists = |p: &Path| self.open_paths.contains_key(p) || p.is_file();
            let resolutions =
                resolve_include(&path, &walk.vars, doc.variant(), cwd, doc.dir(), &exists);
            for mut resolution in resolutions {
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
                resolved.push(ResolvedInclude {
                    path: path.clone(),
                    resolution,
                });
            }
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

/// Whether `name` is a conventional makefile name.
pub fn is_makefile_name(name: &str) -> bool {
    DEFAULT_MAKEFILES.contains(&name) || name.ends_with(".mk") || name.ends_with(".mak")
}

/// Find the makefiles under `roots`, breadth first, looking at no more than
/// `max_entries` directory entries.
///
/// Hidden directories and symbolic links to directories are skipped.
fn find_makefiles(roots: &[PathBuf], max_entries: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue: std::collections::VecDeque<PathBuf> = roots.iter().cloned().collect();
    let mut remaining = max_entries;
    while let Some(dir) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("unable to list {}: {e}", dir.display());
                continue;
            }
        };
        let mut entries: Vec<_> = match entries.collect::<Result<_, _>>() {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("unable to list {}: {e}", dir.display());
                continue;
            }
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if remaining == 0 {
                tracing::warn!(
                    "not searching beyond {max_entries} directory entries for makefiles"
                );
                return found;
            }
            remaining -= 1;
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("unable to stat {}: {e}", entry.path().display());
                    continue;
                }
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_dir() {
                if !name.starts_with('.') {
                    queue.push_back(entry.path());
                }
            } else if is_makefile_name(&name) {
                found.push(entry.path());
            }
        }
    }
    found
}

/// Replace the resolutions of names in `into` with more informative ones
/// from `from`. Both list the same names in the same order, but a name with
/// wildcards may have matched a different number of files.
fn merge_includes(into: &mut Vec<ResolvedInclude>, from: &[ResolvedInclude]) {
    let rank = |group: &[ResolvedInclude]| group.iter().map(|i| i.resolution.rank()).min();
    let mut merged = Vec::with_capacity(into.len());
    let mut from_groups = from.chunk_by(|a, b| a.path == b.path);
    for group in into.chunk_by(|a, b| a.path == b.path) {
        match from_groups.next() {
            Some(other) if other[0].path == group[0].path && rank(other) > rank(group) => {
                merged.extend_from_slice(other)
            }
            _ => merged.extend_from_slice(group),
        }
    }
    *into = merged;
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
            // Keep the search for including makefiles inside the fixture.
            ws.set_roots(vec![self.path("")]);
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
        include_paths(&parsed.tree())
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

    #[test]
    fn test_include_paths_escapes() {
        assert_eq!(
            names_of("include a\\#b.mk a\\ b.mk c.mk\n"),
            vec![
                ("a#b.mk".to_string(), false),
                ("a\\ b.mk".to_string(), false),
                ("c.mk".to_string(), false)
            ]
        );
    }

    #[test]
    fn test_include_paths_continuation() {
        assert_eq!(
            names_of("include a.mk \\\n  b.mk\\\nc.mk\n"),
            vec![
                ("a.mk".to_string(), false),
                ("b.mk".to_string(), false),
                ("c.mk".to_string(), false)
            ]
        );
        let text = "include $(subst a \\\n  b,c,a  b) d.mk\n";
        let paths = include_paths(&Makefile::parse(text).tree());
        assert_eq!(
            paths
                .iter()
                .map(|p| (p.name.as_str(), &text[p.range]))
                .collect::<Vec<_>>(),
            vec![
                ("$(subst a b,c,a  b)", "$(subst a \\\n  b,c,a  b)"),
                ("d.mk", "d.mk")
            ]
        );
    }

    #[test]
    fn test_include_paths_delimited() {
        let text = ".include <a b.mk>\n";
        let parsed = Makefile::parse_with_variant(text, MakefileVariant::BSDMake);
        let paths = include_paths(&parsed.tree());
        assert_eq!(
            paths
                .iter()
                .map(|p| (p.name.as_str(), &text[p.range]))
                .collect::<Vec<_>>(),
            vec![("a b.mk", "a b.mk")]
        );
    }

    fn vars(text: &str) -> LiteralVariables {
        let mut vars = LiteralVariables::default();
        vars.add(&Makefile::parse(text).tree(), MakefileVariant::GNUMake);
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
        assert_eq!(vars("all: T = x\n").get("T"), None);
    }

    #[test]
    fn test_expand() {
        let v = vars("TOP = ../top\nT = t\n");
        let cwd = Path::new("/src");
        assert_eq!(
            expand("$(TOP)/rules.mk", &v, MakefileVariant::GNUMake, Some(cwd)),
            Some("../top/rules.mk".to_string())
        );
        assert_eq!(
            expand("${TOP}/x", &v, MakefileVariant::GNUMake, Some(cwd)),
            Some("../top/x".to_string())
        );
        assert_eq!(
            expand("$(CURDIR)/x.mk", &v, MakefileVariant::GNUMake, Some(cwd)),
            Some("/src/x.mk".to_string())
        );
        assert_eq!(
            expand("$(OTHER)/x.mk", &v, MakefileVariant::GNUMake, Some(cwd)),
            None
        );
        assert_eq!(
            expand("$(wildcard x)", &v, MakefileVariant::GNUMake, Some(cwd)),
            None
        );
        assert_eq!(
            expand("*.mk", &v, MakefileVariant::GNUMake, Some(cwd)),
            Some("*.mk".to_string())
        );
        assert_eq!(expand("$@", &v, MakefileVariant::GNUMake, Some(cwd)), None);
        assert_eq!(
            expand("$T/x.mk", &v, MakefileVariant::GNUMake, Some(cwd)),
            Some("t/x.mk".to_string())
        );
        assert_eq!(
            expand("$$T/x.mk", &v, MakefileVariant::GNUMake, Some(cwd)),
            None
        );
        assert_eq!(
            expand("$(TOP:%=%/)x.mk", &v, MakefileVariant::GNUMake, Some(cwd)),
            None
        );
        assert_eq!(
            expand("$(TOP", &v, MakefileVariant::GNUMake, Some(cwd)),
            None
        );
        assert_eq!(
            expand("$(TO P)", &v, MakefileVariant::GNUMake, Some(cwd)),
            None
        );
        assert_eq!(
            expand("$(T$(T))", &v, MakefileVariant::GNUMake, Some(cwd)),
            None
        );
        assert_eq!(
            expand("x.mk$", &v, MakefileVariant::GNUMake, Some(cwd)),
            None
        );
    }

    #[test]
    fn test_literal_variables_variant() {
        let mut v = LiteralVariables::default();
        let parsed = Makefile::parse_with_variant("X = a^#b\n", MakefileVariant::NMake);
        v.add(&parsed.tree(), MakefileVariant::NMake);
        assert_eq!(v.get("X"), Some("a#b"));
    }

    #[test]
    fn test_expand_variant() {
        let v = vars("TOP = ../top\n");
        assert_eq!(
            expand("${TOP:}/x.mk", &v, MakefileVariant::BSDMake, None),
            Some("../top/x.mk".to_string())
        );
        assert_eq!(
            expand("${TOP:}/x.mk", &v, MakefileVariant::GNUMake, None),
            None
        );
    }

    #[test]
    fn test_normalize() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(normalize(Path::new("a/../../b")), PathBuf::from("../b"));
    }

    fn gnu_path(name: &str) -> IncludePath {
        IncludePath {
            name: name.to_string(),
            range: TextRange::default(),
            optional: false,
            gnu: true,
        }
    }

    #[test]
    fn test_resolve_prefers_cwd() {
        let v = LiteralVariables::default();
        let exists = |p: &Path| p == Path::new("/top/x.mk") || p == Path::new("/top/sub/x.mk");
        assert_eq!(
            resolve_include(
                &gnu_path("x.mk"),
                &v,
                MakefileVariant::GNUMake,
                Some(Path::new("/top")),
                Some(Path::new("/top/sub")),
                &exists
            ),
            vec![Resolution::Found(PathBuf::from("/top/x.mk"))]
        );
        assert_eq!(
            resolve_include(
                &gnu_path("x.mk"),
                &v,
                MakefileVariant::GNUMake,
                Some(Path::new("/elsewhere")),
                Some(Path::new("/top/sub")),
                &exists
            ),
            vec![Resolution::Found(PathBuf::from("/top/sub/x.mk"))]
        );
        assert_eq!(
            resolve_include(
                &gnu_path("y.mk"),
                &v,
                MakefileVariant::GNUMake,
                Some(Path::new("/top")),
                None,
                &exists
            ),
            vec![Resolution::Missing(PathBuf::from("/top/y.mk"))]
        );
        assert_eq!(
            resolve_include(
                &gnu_path("y.mk"),
                &v,
                MakefileVariant::GNUMake,
                None,
                None,
                &exists
            ),
            vec![Resolution::Unresolved]
        );
    }

    #[test]
    fn test_resolve_bsd_name_not_split() {
        let text = ".include <a b.mk>\n";
        let makefile = Makefile::parse_with_variant(text, MakefileVariant::BSDMake).tree();
        let paths = include_paths(&makefile);
        let exists = |p: &Path| p == Path::new("/top/a b.mk");
        assert_eq!(
            paths
                .iter()
                .flat_map(|p| resolve_include(
                    p,
                    &LiteralVariables::default(),
                    MakefileVariant::BSDMake,
                    Some(Path::new("/top")),
                    None,
                    &exists
                ))
                .collect::<Vec<_>>(),
            vec![Resolution::Found(PathBuf::from("/top/a b.mk"))]
        );
    }

    fn matches(pattern: &str, name: &str) -> bool {
        glob_match(&glob_tokens(pattern), name)
    }

    #[test]
    fn test_glob_match() {
        assert!(matches("*.mk", "a.mk"));
        assert!(!matches("*.mk", ".mk.mk"));
        assert!(matches(".*.mk", ".h.mk"));
        assert!(!matches("[.]h.mk", ".h.mk"));
        assert!(matches("*", "abc"));
        assert!(matches("a*b*c", "aXbYbc"));
        assert!(!matches("a*b*c", "aXbYbcd"));
        assert!(matches("?.mk", "x.mk"));
        assert!(!matches("?.mk", "xy.mk"));
        assert!(matches("[ab].mk", "b.mk"));
        assert!(!matches("[ab].mk", "c.mk"));
        assert!(matches("[a-c].mk", "c.mk"));
        assert!(matches("[!a-c].mk", "d.mk"));
        assert!(!matches("[^a-c].mk", "b.mk"));
        assert!(matches("[]x]", "]"));
        assert!(matches("[a-]", "-"));
        assert!(matches("[", "["));
        assert!(matches("[ab", "[ab"));
        assert!(matches("\\*.mk", "*.mk"));
        assert!(!matches("\\*.mk", "a.mk"));
    }

    #[test]
    fn test_has_wildcard() {
        assert!(!has_wildcard("a.mk"));
        assert!(has_wildcard("*.mk"));
        assert!(has_wildcard("a?.mk"));
        assert!(has_wildcard("[ab].mk"));
        assert!(!has_wildcard("\\*.mk"));
    }

    fn include_targets(fx: &Fixture, set: &FileSet) -> Vec<(String, Resolution)> {
        let relative = |p: &Path| p.strip_prefix(fx.path("")).unwrap().to_path_buf();
        set.includes()
            .iter()
            .map(|i| {
                let resolution = match &i.resolution {
                    Resolution::Found(p) => Resolution::Found(relative(p)),
                    Resolution::Missing(p) => Resolution::Missing(relative(p)),
                    other => other.clone(),
                };
                (i.path.name.clone(), resolution)
            })
            .collect()
    }

    #[test]
    fn test_file_set_include_wildcard() {
        let fx = Fixture::new(&[
            ("Makefile", "include *.mk\n"),
            ("b.mk", "B = 1\n"),
            ("a.mk", "A = 1\n"),
            (".hidden.mk", ""),
            ("sub/c.mk", ""),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile", "a.mk", "b.mk"]);
        assert_eq!(
            include_targets(&fx, &set),
            vec![
                ("*.mk".to_string(), Resolution::Found("a.mk".into())),
                ("*.mk".to_string(), Resolution::Found("b.mk".into())),
            ]
        );
        assert!(set.is_complete());
    }

    #[test]
    fn test_file_set_include_wildcard_with_variable() {
        let fx = Fixture::new(&[
            (
                "Makefile",
                "D = mk\ninclude $(D)/?.mk $(D)/[!b]x.mk */rules.mk\n",
            ),
            ("mk/a.mk", ""),
            ("mk/ab.mk", ""),
            ("mk/ax.mk", ""),
            ("mk/bx.mk", ""),
            ("x/rules.mk", ""),
            ("y/rules.mk", ""),
            ("z/other.mk", ""),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(
            fx.names(&set),
            vec![
                "Makefile",
                "mk/a.mk",
                "mk/ax.mk",
                "x/rules.mk",
                "y/rules.mk"
            ]
        );
    }

    #[test]
    fn test_file_set_include_wildcard_no_match() {
        let fx = Fixture::new(&[("Makefile", "include none*.mk\n-include *.inc\n")]);
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile"]);
        // make uses a pattern that matches nothing as the file name.
        assert_eq!(
            include_targets(&fx, &set),
            vec![
                (
                    "none*.mk".to_string(),
                    Resolution::Missing("none*.mk".into())
                ),
                ("*.inc".to_string(), Resolution::Missing("*.inc".into())),
            ]
        );
        assert!(!set.is_complete());
    }

    #[test]
    fn test_file_set_include_wildcard_directory() {
        let fx = Fixture::new(&[("Makefile", "include *.d\n")]);
        std::fs::create_dir(fx.path("x.d")).unwrap();
        let set = fx.file_set("Makefile");
        assert_eq!(
            set.includes()
                .iter()
                .map(|i| i.resolution.clone())
                .collect::<Vec<_>>(),
            vec![Resolution::Unreadable(
                fx.path("x.d"),
                "not a regular file".to_string()
            )]
        );
    }

    #[test]
    fn test_file_set_include_wildcard_too_many() {
        let fx = Fixture::new(&[("Makefile", "include d/*.mk\n")]);
        std::fs::create_dir(fx.path("d")).unwrap();
        for i in 0..=MAX_GLOB_MATCHES {
            std::fs::write(fx.path(&format!("d/{i}.mk")), "").unwrap();
        }
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile"]);
        assert_eq!(
            include_targets(&fx, &set),
            vec![("d/*.mk".to_string(), Resolution::Unresolved)]
        );
    }

    #[test]
    fn test_bsd_include_not_globbed() {
        let fx = Fixture::new(&[("Makefile", ".include \"*.mk\"\n"), ("a.mk", "")]);
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile"]);
        assert_eq!(
            include_targets(&fx, &set),
            vec![("*.mk".to_string(), Resolution::Missing("*.mk".into()))]
        );
    }

    #[test]
    fn test_merge_includes_wildcard() {
        let include = |name: &str, start: u32, resolution: Resolution| ResolvedInclude {
            path: IncludePath {
                range: TextRange::at(start.into(), (name.len() as u32).into()),
                ..gnu_path(name)
            },
            resolution,
        };
        let mut into = vec![
            include("*.mk", 0, Resolution::Missing("/a/*.mk".into())),
            include("x.mk", 5, Resolution::Missing("/a/x.mk".into())),
        ];
        let from = vec![
            include("*.mk", 0, Resolution::Found("/b/1.mk".into())),
            include("*.mk", 0, Resolution::Found("/b/2.mk".into())),
            include("x.mk", 5, Resolution::Unresolved),
        ];
        merge_includes(&mut into, &from);
        assert_eq!(
            into,
            vec![
                include("*.mk", 0, Resolution::Found("/b/1.mk".into())),
                include("*.mk", 0, Resolution::Found("/b/2.mk".into())),
                include("x.mk", 5, Resolution::Missing("/a/x.mk".into())),
            ]
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
    fn test_file_set_include_escaped_space() {
        let fx = Fixture::new(&[
            ("Makefile", "B = b\ninclude a\\ $(B).mk\n"),
            ("a b.mk", "X = 1\n"),
        ]);
        let set = fx.file_set("Makefile");
        assert_eq!(fx.names(&set), vec!["Makefile", "a b.mk"]);
    }

    #[test]
    fn test_file_set_for_path() {
        let fx = Fixture::new(&[
            ("Makefile", "include a.mk\n"),
            ("a.mk", "include b.mk\n"),
            ("b.mk", ""),
        ]);
        let mut ws = Workspace::new();
        ws.set_roots(vec![fx.path("")]);
        let set = ws.file_set_for_path(&fx.path("a.mk")).unwrap();
        assert_eq!(fx.names(&set), vec!["a.mk", "b.mk", "Makefile"]);
        assert!(set.is_complete());
        assert!(matches!(
            ws.file_set_for_path(&fx.path("missing.mk")),
            Err(LoadError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound
        ));
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

    #[test]
    fn test_fragment_finds_makefile_nearby() {
        let fx = Fixture::new(&[
            ("Makefile", "TOP = 1\ninclude mk/rules.mk\n"),
            ("mk/rules.mk", "include mk/other.mk\n"),
            ("mk/other.mk", "O = 1\n"),
        ]);
        let set = fx.file_set("mk/rules.mk");
        // mk/other.mk is relative to the top-level directory, so it's only
        // found when following includes from the Makefile.
        assert_eq!(
            fx.names(&set),
            vec!["mk/rules.mk", "Makefile", "mk/other.mk"]
        );
    }

    #[test]
    fn test_probe_rereads_changed_makefile() {
        let fx = Fixture::new(&[("Makefile", "all:\n"), ("rules.mk", "")]);
        let (mut ws, rules) = fx.open("rules.mk");
        assert_eq!(fx.names(&ws.file_set(&rules).unwrap()), vec!["rules.mk"]);
        // Make sure the modification time changes.
        let later = SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::write(fx.path("Makefile"), "include rules.mk\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(fx.path("Makefile"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            fx.names(&ws.file_set(&rules).unwrap()),
            vec!["rules.mk", "Makefile"]
        );
    }

    #[test]
    fn test_dependents() {
        let fx = Fixture::new(&[
            ("Makefile", "include a.mk\n"),
            ("a.mk", ""),
            ("other/Makefile", ""),
        ]);
        let (mut ws, makefile) = fx.open("Makefile");
        let a = fx.open_in(&mut ws, "a.mk");
        let other = fx.open_in(&mut ws, "other/Makefile");
        for uri in [&makefile, &a, &other] {
            ws.file_set(uri).unwrap();
        }
        assert_eq!(ws.dependents(&a), vec![makefile.clone()]);
        assert_eq!(ws.dependents(&makefile), vec![a.clone()]);
        assert_eq!(ws.dependents(&other), Vec::<Uri>::new());
        assert_eq!(ws.open_documents_seeing(&fx.path("a.mk")), {
            let mut v = vec![makefile, a];
            v.sort_by(|x, y| x.as_str().cmp(y.as_str()));
            v
        });
    }

    fn relative_names(fx: &Fixture, docs: &[Arc<Document>]) -> Vec<String> {
        docs.iter()
            .map(|d| {
                d.path()
                    .unwrap()
                    .strip_prefix(fx.path(""))
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn test_all_documents() {
        let fx = Fixture::new(&[
            ("ws/Makefile", "include common.mk ../outside.mk\n"),
            ("ws/common.mk", ""),
            ("ws/sub/GNUmakefile", ""),
            ("ws/sub/rules.mk", ""),
            ("ws/.git/hidden.mk", ""),
            ("ws/README", ""),
            ("outside.mk", ""),
            ("other/Makefile", ""),
            ("elsewhere/open.mk", "include inc.mk\n"),
            ("elsewhere/inc.mk", ""),
        ]);
        let mut ws = Workspace::new();
        ws.set_roots(vec![fx.path("ws")]);
        fx.open_in(&mut ws, "ws/sub/rules.mk");
        fx.open_in(&mut ws, "elsewhere/open.mk");
        let docs = ws.all_documents();
        assert_eq!(
            relative_names(&fx, &docs),
            vec![
                "elsewhere/open.mk",
                "elsewhere/inc.mk",
                "ws/sub/rules.mk",
                "ws/Makefile",
                "ws/common.mk",
                "outside.mk",
                "ws/sub/GNUmakefile",
            ]
        );
    }

    #[test]
    fn test_find_makefiles_limit() {
        let fx = Fixture::new(&[
            ("a/deep.mk", ""),
            ("Makefile", ""),
            ("b.mk", ""),
            ("c.txt", ""),
        ]);
        let root = vec![fx.path("")];
        assert_eq!(
            find_makefiles(&root, 100),
            vec![fx.path("Makefile"), fx.path("b.mk"), fx.path("a/deep.mk")]
        );
        assert_eq!(
            find_makefiles(&root, 3),
            vec![fx.path("Makefile"), fx.path("b.mk")]
        );
    }

    #[test]
    fn test_display_name() {
        let fx = Fixture::new(&[]);
        let mut ws = Workspace::new();
        ws.set_roots(vec![fx.path("ws"), fx.path("ws/sub")]);
        let name = |uri: Uri| ws.display_name(&Document::new(uri, String::new()));
        assert_eq!(name(fx.uri("ws/a.mk")), "a.mk");
        assert_eq!(name(fx.uri("ws/sub/b.mk")), "b.mk");
        assert_eq!(
            name(fx.uri("outside.mk")),
            fx.path("outside.mk").display().to_string()
        );
        assert_eq!(
            name("untitled:Untitled-1".parse().unwrap()),
            "untitled:Untitled-1"
        );
    }
}
