//! Implementation of the `check` subcommand, which reports diagnostics for
//! Makefiles on disk without running a language server.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde_json::json;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

use crate::position::utf16_to_char_column;
use crate::workspace::Workspace;

/// No diagnostics at or above the severity threshold.
pub const EXIT_CLEAN: i32 = 0;
/// At least one diagnostic was reported.
pub const EXIT_DIAGNOSTICS: i32 = 1;
/// Invalid usage or an I/O error.
pub const EXIT_ERROR: i32 = 2;

const INFORMATION_URI: &str = "https://github.com/jelmer/makefile-lsp";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Hint,
    Info,
    Warning,
    Error,
}

impl Severity {
    fn from_lsp(severity: Option<DiagnosticSeverity>) -> Self {
        match severity {
            Some(DiagnosticSeverity::WARNING) => Severity::Warning,
            Some(DiagnosticSeverity::INFORMATION) => Severity::Info,
            Some(DiagnosticSeverity::HINT) => Severity::Hint,
            // Unspecified severities are treated as errors so they are never
            // hidden by the threshold.
            _ => Severity::Error,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "error" => Some(Severity::Error),
            "warning" => Some(Severity::Warning),
            "info" => Some(Severity::Info),
            "hint" => Some(Severity::Hint),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        }
    }

    fn sarif_level(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info | Severity::Hint => "note",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Sarif,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub format: Format,
    pub min_severity: Severity,
    /// Whether to take the makefiles a file includes, and those including
    /// it, into account.
    pub follow_includes: bool,
    pub paths: Vec<PathBuf>,
}

/// A diagnostic resolved to a file location with 1-based line and character
/// columns. End positions are exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: PathBuf,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
}

impl Finding {
    fn from_diagnostic(path: &Path, text: &str, diag: &Diagnostic) -> Self {
        let code = diag.code.as_ref().map(|c| match c {
            NumberOrString::String(s) => s.clone(),
            NumberOrString::Number(n) => n.to_string(),
        });
        Finding {
            path: path.to_path_buf(),
            start_line: diag.range.start.line + 1,
            start_column: utf16_to_char_column(text, diag.range.start) + 1,
            end_line: diag.range.end.line + 1,
            end_column: utf16_to_char_column(text, diag.range.end) + 1,
            severity: Severity::from_lsp(diag.severity),
            code,
            message: diag.message.clone(),
        }
    }
}

/// Parse the arguments following `check`. Returns `Ok(None)` when help was
/// requested.
pub fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut format = Format::Text;
    let mut min_severity = Severity::Warning;
    let mut follow_includes = true;
    let mut paths = Vec::new();

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--format" => {
                let value = iter.next().ok_or("missing value for --format")?;
                format = match value.as_str() {
                    "text" => Format::Text,
                    "sarif" => Format::Sarif,
                    other => return Err(format!("unknown format '{other}'")),
                };
            }
            "--severity" => {
                let value = iter.next().ok_or("missing value for --severity")?;
                min_severity =
                    Severity::parse(value).ok_or_else(|| format!("unknown severity '{value}'"))?;
            }
            "--no-follow-includes" => follow_includes = false,
            "-h" | "--help" => return Ok(None),
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}'"));
            }
            other => paths.push(PathBuf::from(other)),
        }
    }

    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }

    Ok(Some(Options {
        format,
        min_severity,
        follow_includes,
        paths,
    }))
}

fn print_help() {
    eprintln!(
        "Usage: makefile-lsp check [OPTIONS] [PATH...]\n\n\
         Report diagnostics for Makefiles, for use in CI.\n\n\
         Directories are searched recursively (skipping hidden directories) for\n\
         Makefile, makefile, GNUmakefile, *.mk and *.mak. Files given explicitly\n\
         are checked regardless of their name. With no PATH, the current\n\
         directory is searched.\n\n\
         Included makefiles, and makefiles nearby that include a checked file,\n\
         are read so that definitions and uses in them are taken into account.\n\
         Diagnostics are only reported for the checked files.\n\n\
         Options:\n      \
         --format FORMAT      Output format: text (default) or sarif\n      \
         --severity LEVEL     Minimum severity to report: error, warning\n                           \
         (default), info or hint\n      \
         --no-follow-includes Check each file on its own\n  \
         -h, --help               Show this help\n\n\
         Exit status: 0 if nothing was reported, 1 if diagnostics were reported,\n\
         2 on usage or I/O errors."
    );
}

/// Run the `check` subcommand and return the process exit code.
pub fn run(args: &[String]) -> i32 {
    let options = match parse_args(args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print_help();
            return EXIT_CLEAN;
        }
        Err(e) => {
            eprintln!("makefile-lsp check: {e}");
            return EXIT_ERROR;
        }
    };

    let base = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("makefile-lsp check: cannot determine current directory: {e}");
            return EXIT_ERROR;
        }
    };

    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    check(&options, &base, &mut stdout.lock(), &mut stderr.lock())
}

/// Check the files selected by `options`, writing the report to `out` and
/// errors to `err`. `base` is the absolute directory relative paths are
/// resolved against and SARIF URIs are made relative to.
pub fn check(options: &Options, base: &Path, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut errors = Vec::new();
    let files = collect_files(&options.paths, &mut errors);

    let mut workspace = Workspace::new();
    // Look for makefiles including a checked file no further up than `base`.
    workspace.set_roots(vec![base.to_path_buf()]);

    let mut findings = Vec::new();
    for file in &files {
        let found = if options.follow_includes {
            check_file_with_includes(&mut workspace, file, &base.join(file))
        } else {
            check_file(file).map_err(|e| e.to_string())
        };
        match found {
            Ok(found) => findings.extend(
                found
                    .into_iter()
                    .filter(|f| f.severity >= options.min_severity),
            ),
            Err(e) => errors.push(format!("{}: {e}", file.display())),
        }
    }

    for e in &errors {
        // Nothing sensible can be done if stderr is unwritable.
        let _ = writeln!(err, "makefile-lsp check: {e}");
    }

    let written = match options.format {
        Format::Text => write_text(out, &findings),
        Format::Sarif => write_sarif(out, &findings, &errors, base),
    };
    if let Err(e) = written {
        let _ = writeln!(err, "makefile-lsp check: failed to write output: {e}");
        return EXIT_ERROR;
    }

    if !errors.is_empty() {
        EXIT_ERROR
    } else if !findings.is_empty() {
        EXIT_DIAGNOSTICS
    } else {
        EXIT_CLEAN
    }
}

/// Read and diagnose a single file on its own, returning findings sorted by
/// position.
pub fn check_file(path: &Path) -> std::io::Result<Vec<Finding>> {
    let text = std::fs::read_to_string(path)?;
    let parsed = makefile_lossless::Makefile::parse(&text);
    let base_dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let diagnostics = crate::diagnostics::get_diagnostics(&text, &parsed, Some(&base_dir));
    Ok(findings(path, &text, &parsed, diagnostics))
}

/// Diagnose a file together with the makefiles it includes and those that
/// include it, like the language server does. `path` is how the file is
/// reported, `absolute` where it is.
pub fn check_file_with_includes(
    workspace: &mut Workspace,
    path: &Path,
    absolute: &Path,
) -> Result<Vec<Finding>, String> {
    let files = workspace
        .file_set_for_path(absolute)
        .map_err(|e| e.to_string())?;
    let diagnostics = crate::diagnostics::get_file_set_diagnostics(&files);
    let current = files.current();
    Ok(findings(
        path,
        current.text(),
        current.parsed(),
        diagnostics,
    ))
}

/// Add the shell syntax diagnostics and convert to findings sorted by
/// position.
fn findings(
    path: &Path,
    text: &str,
    parsed: &makefile_lossless::Parse<makefile_lossless::Makefile>,
    mut diagnostics: Vec<Diagnostic>,
) -> Vec<Finding> {
    diagnostics.extend(crate::shell_check::check_shell_syntax(
        text,
        &parsed.tree(),
        crate::workspace::parsed_variant(parsed),
    ));
    let mut findings: Vec<Finding> = diagnostics
        .iter()
        .map(|d| Finding::from_diagnostic(path, text, d))
        .collect();
    findings.sort_by_key(|f| (f.start_line, f.start_column));
    findings
}

fn is_makefile_name(name: &str) -> bool {
    matches!(name, "Makefile" | "makefile" | "GNUmakefile")
        || name.ends_with(".mk")
        || name.ends_with(".mak")
}

/// Drop `.` components so `./Makefile` is reported as `Makefile`.
fn normalize(path: &Path) -> PathBuf {
    let normalized: PathBuf = path
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

/// Expand the given paths into a sorted, de-duplicated list of Makefiles.
/// Paths that cannot be accessed are recorded in `errors`.
pub fn collect_files(paths: &[PathBuf], errors: &mut Vec<String>) -> Vec<PathBuf> {
    let mut files = BTreeSet::new();
    for path in paths {
        let path = normalize(path);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => walk_dir(&path, &mut files, errors),
            Ok(_) => {
                files.insert(path);
            }
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    files.into_iter().collect()
}

fn walk_dir(dir: &Path, files: &mut BTreeSet<PathBuf>, errors: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push(format!("{}: {e}", dir.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                errors.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        let path = normalize(&entry.path());
        // file_type() does not follow symlinks, so symlinked directories are
        // not descended into and cannot cause loops.
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if !name.starts_with('.') {
                walk_dir(&path, files, errors);
            }
        } else if is_makefile_name(&name) {
            files.insert(path);
        }
    }
}

/// Collapse a message onto one line so each diagnostic is a single line.
fn single_line(message: &str) -> String {
    message.lines().collect::<Vec<_>>().join(" ")
}

pub fn format_text(finding: &Finding) -> String {
    let mut line = format!(
        "{}:{}:{}: {}: {}",
        finding.path.display(),
        finding.start_line,
        finding.start_column,
        finding.severity.as_str(),
        single_line(&finding.message)
    );
    if let Some(code) = &finding.code {
        line.push_str(&format!(" [{code}]"));
    }
    line
}

fn write_text(out: &mut dyn Write, findings: &[Finding]) -> std::io::Result<()> {
    for finding in findings {
        writeln!(out, "{}", format_text(finding))?;
    }
    Ok(())
}

/// Percent-encode a path for use in a URI, keeping `/` separators.
fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Build the SARIF artifact URI for `path`: relative to `base` when possible,
/// otherwise an absolute `file://` URI.
pub fn artifact_uri(base: &Path, path: &Path) -> String {
    let relative = if path.is_absolute() {
        path.strip_prefix(base).ok()
    } else {
        Some(path)
    };
    match relative {
        Some(rel) => percent_encode_path(&normalize(rel).to_string_lossy().replace('\\', "/")),
        None => {
            let s = path.to_string_lossy().replace('\\', "/");
            format!("file://{}", percent_encode_path(&s))
        }
    }
}

pub fn sarif_report(findings: &[Finding], errors: &[String], base: &Path) -> serde_json::Value {
    let rule_ids: BTreeSet<&str> = findings.iter().filter_map(|f| f.code.as_deref()).collect();
    let rules: Vec<_> = rule_ids.iter().map(|id| json!({ "id": id })).collect();

    let results: Vec<_> = findings
        .iter()
        .map(|f| {
            let mut result = json!({
                "level": f.severity.sarif_level(),
                "message": { "text": f.message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": artifact_uri(base, &f.path) },
                        "region": {
                            "startLine": f.start_line,
                            "startColumn": f.start_column,
                            "endLine": f.end_line,
                            "endColumn": f.end_column,
                        }
                    }
                }]
            });
            if let Some(code) = &f.code {
                result["ruleId"] = json!(code);
            }
            result
        })
        .collect();

    let notifications: Vec<_> = errors
        .iter()
        .map(|e| json!({ "level": "error", "message": { "text": e } }))
        .collect();

    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "makefile-lsp",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": INFORMATION_URI,
                    "rules": rules,
                }
            },
            "invocations": [{
                "executionSuccessful": errors.is_empty(),
                "toolExecutionNotifications": notifications,
            }],
            "columnKind": "unicodeCodePoints",
            "results": results,
        }]
    })
}

fn write_sarif(
    out: &mut dyn Write,
    findings: &[Finding],
    errors: &[String],
    base: &Path,
) -> std::io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, &sarif_report(findings, errors, base))?;
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn finding(path: &str, severity: Severity, code: Option<&str>) -> Finding {
        Finding {
            path: PathBuf::from(path),
            start_line: 3,
            start_column: 5,
            end_line: 3,
            end_column: 9,
            severity,
            code: code.map(str::to_string),
            message: "something is off".to_string(),
        }
    }

    fn run_check(options: &Options, base: &Path) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = check(options, base, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn options(paths: Vec<PathBuf>, min_severity: Severity, format: Format) -> Options {
        Options {
            format,
            min_severity,
            follow_includes: true,
            paths,
        }
    }

    #[test]
    fn test_parse_args_defaults() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            Some(Options {
                format: Format::Text,
                min_severity: Severity::Warning,
                follow_includes: true,
                paths: vec![PathBuf::from(".")],
            })
        );
    }

    #[test]
    fn test_parse_args_options() {
        assert_eq!(
            parse_args(&args(&[
                "--format",
                "sarif",
                "--severity",
                "hint",
                "--no-follow-includes",
                "a",
                "b"
            ]))
            .unwrap(),
            Some(Options {
                format: Format::Sarif,
                min_severity: Severity::Hint,
                follow_includes: false,
                paths: vec![PathBuf::from("a"), PathBuf::from("b")],
            })
        );
    }

    #[test]
    fn test_parse_args_help() {
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), None);
    }

    #[test]
    fn test_parse_args_errors() {
        assert_eq!(
            parse_args(&args(&["--format", "xml"])),
            Err("unknown format 'xml'".to_string())
        );
        assert_eq!(
            parse_args(&args(&["--severity"])),
            Err("missing value for --severity".to_string())
        );
        assert_eq!(
            parse_args(&args(&["--bogus"])),
            Err("unknown option '--bogus'".to_string())
        );
    }

    #[test]
    fn test_severity_order() {
        assert!(Severity::Error > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
        assert!(Severity::Info > Severity::Hint);
    }

    #[test]
    fn test_format_text() {
        assert_eq!(
            format_text(&finding("sub/Makefile", Severity::Warning, Some("foo"))),
            "sub/Makefile:3:5: warning: something is off [foo]"
        );
        assert_eq!(
            format_text(&finding("Makefile", Severity::Error, None)),
            "Makefile:3:5: error: something is off"
        );
    }

    #[test]
    fn test_format_text_multiline_message() {
        let mut f = finding("Makefile", Severity::Hint, None);
        f.message = "first\nsecond".to_string();
        assert_eq!(format_text(&f), "Makefile:3:5: hint: first second");
    }

    #[test]
    fn test_finding_uses_character_columns() {
        let text = "X := \u{1F600}$(\n";
        let diag = Diagnostic {
            range: Range::new(Position::new(0, 7), Position::new(0, 9)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("bad".to_string())),
            message: "msg".to_string(),
            ..Default::default()
        };
        let f = Finding::from_diagnostic(Path::new("Makefile"), text, &diag);
        assert_eq!((f.start_line, f.start_column), (1, 7));
        assert_eq!((f.end_line, f.end_column), (1, 9));
        assert_eq!(f.code.as_deref(), Some("bad"));
    }

    #[test]
    fn test_artifact_uri() {
        let base = Path::new("/src/project");
        assert_eq!(artifact_uri(base, Path::new("./a/Makefile")), "a/Makefile");
        assert_eq!(
            artifact_uri(base, Path::new("/src/project/rules/x y.mk")),
            "rules/x%20y.mk"
        );
        assert_eq!(
            artifact_uri(base, Path::new("/elsewhere/Makefile")),
            "file:///elsewhere/Makefile"
        );
    }

    #[test]
    fn test_sarif_report() {
        let findings = vec![
            finding("Makefile", Severity::Error, Some("b-rule")),
            finding("Makefile", Severity::Hint, Some("a-rule")),
            finding("Makefile", Severity::Warning, Some("b-rule")),
        ];
        let report = sarif_report(&findings, &[], Path::new("/src"));
        assert_eq!(report["version"], "2.1.0");
        let run = &report["runs"][0];
        assert_eq!(run["tool"]["driver"]["name"], "makefile-lsp");
        assert_eq!(run["tool"]["driver"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            run["tool"]["driver"]["rules"],
            json!([{ "id": "a-rule" }, { "id": "b-rule" }])
        );
        assert_eq!(run["invocations"][0]["executionSuccessful"], true);
        let results = run["results"].as_array().unwrap();
        assert_eq!(
            results
                .iter()
                .map(|r| r["level"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["error", "note", "warning"]
        );
        assert_eq!(
            results[0],
            json!({
                "ruleId": "b-rule",
                "level": "error",
                "message": { "text": "something is off" },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": "Makefile" },
                        "region": {
                            "startLine": 3,
                            "startColumn": 5,
                            "endLine": 3,
                            "endColumn": 9,
                        }
                    }
                }]
            })
        );
    }

    #[test]
    fn test_sarif_report_errors() {
        let report = sarif_report(&[], &["x: denied".to_string()], Path::new("/src"));
        let invocation = &report["runs"][0]["invocations"][0];
        assert_eq!(invocation["executionSuccessful"], false);
        assert_eq!(
            invocation["toolExecutionNotifications"],
            json!([{ "level": "error", "message": { "text": "x: denied" } }])
        );
    }

    #[test]
    fn test_collect_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in [
            "Makefile",
            "README",
            "rules.mk",
            "old.mak",
            "sub/makefile",
            "sub/deeper/GNUmakefile",
            "sub/notes.txt",
            ".git/config.mk",
            "sub/.hidden/Makefile",
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let mut errors = Vec::new();
        let files = collect_files(&[root.to_path_buf()], &mut errors);
        assert_eq!(errors, Vec::<String>::new());
        let rel: Vec<_> = files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rel,
            vec![
                "Makefile",
                "old.mak",
                "rules.mk",
                "sub/deeper/GNUmakefile",
                "sub/makefile",
            ]
        );
    }

    #[test]
    fn test_collect_files_explicit_file_any_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("build.rules");
        std::fs::write(&path, "").unwrap();
        let mut errors = Vec::new();
        assert_eq!(
            collect_files(std::slice::from_ref(&path), &mut errors),
            vec![path]
        );
        assert_eq!(errors, Vec::<String>::new());
    }

    #[test]
    fn test_collect_files_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let mut errors = Vec::new();
        assert_eq!(
            collect_files(std::slice::from_ref(&missing), &mut errors),
            Vec::<PathBuf>::new()
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with(&format!("{}: ", missing.display())));
    }

    #[test]
    fn test_check_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let clean = root.join("clean");
        std::fs::create_dir(&clean).unwrap();
        std::fs::write(clean.join("Makefile"), "all:\n\techo hi\n").unwrap();
        let broken = root.join("broken");
        std::fs::create_dir(&broken).unwrap();
        // A recipe line indented with spaces is a parse error.
        std::fs::write(broken.join("Makefile"), "all:\n    echo hi\n").unwrap();

        let (code, out, err) = run_check(
            &options(vec![clean.clone()], Severity::Warning, Format::Text),
            root,
        );
        assert_eq!((code, out.as_str(), err.as_str()), (EXIT_CLEAN, "", ""));

        let (code, out, err) = run_check(
            &options(vec![broken.clone()], Severity::Warning, Format::Text),
            root,
        );
        assert_eq!(code, EXIT_DIAGNOSTICS);
        assert_eq!(err, "");
        let prefix = format!("{}:2:", broken.join("Makefile").display());
        assert!(out.lines().all(|l| l.starts_with(&prefix)), "{out}");
        assert!(out.contains(": error: "), "{out}");

        let (code, _, err) = run_check(
            &options(
                vec![clean.clone(), root.join("missing")],
                Severity::Warning,
                Format::Text,
            ),
            root,
        );
        assert_eq!(code, EXIT_ERROR);
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn test_check_file_includes_shell_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Makefile");
        std::fs::write(&path, ".PHONY: all\nall:\n\tls )\n").unwrap();
        let codes: Vec<_> = check_file(&path)
            .unwrap()
            .into_iter()
            .map(|f| f.code)
            .collect();
        assert_eq!(codes, vec![Some("invalid-shell-syntax".to_string())]);
    }

    #[test]
    fn test_check_severity_threshold() {
        let dir = tempfile::tempdir().unwrap();
        // A .PHONY target nothing depends on only yields a hint.
        let text = ".PHONY: format\nformat:\n\tfoo\n";
        let path = dir.path().join("Makefile");
        std::fs::write(&path, text).unwrap();

        let all = check_file(&path).unwrap();
        assert!(!all.is_empty());
        assert!(
            all.iter().all(|f| f.severity < Severity::Warning),
            "{all:?}"
        );

        let (code, out, _) = run_check(
            &options(vec![path.clone()], Severity::Warning, Format::Text),
            dir.path(),
        );
        assert_eq!((code, out.as_str()), (EXIT_CLEAN, ""));

        let (code, out, _) = run_check(
            &options(vec![path.clone()], Severity::Hint, Format::Text),
            dir.path(),
        );
        assert_eq!(code, EXIT_DIAGNOSTICS);
        assert_eq!(out.lines().count(), all.len());
    }

    #[test]
    fn test_check_sarif_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Makefile"), "all:\n    echo hi\n").unwrap();
        let (code, out, _) = run_check(
            &options(
                vec![dir.path().to_path_buf()],
                Severity::Warning,
                Format::Sarif,
            ),
            dir.path(),
        );
        assert_eq!(code, EXIT_DIAGNOSTICS);
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        let results = report["runs"][0]["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(
            results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "Makefile"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_check_unreadable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Makefile");
        std::fs::write(&path, "all:\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            // Running as root; permissions are not enforced.
            return;
        }
        let (code, _, err) = run_check(
            &options(
                vec![dir.path().to_path_buf()],
                Severity::Warning,
                Format::Text,
            ),
            dir.path(),
        );
        assert_eq!(code, EXIT_ERROR);
        assert!(err.starts_with(&format!("makefile-lsp check: {}: ", path.display())));
    }

    const MAKEFILE: &str =
        "FLAGS = $(LIBS)\n\nall: build $(OBJ)\n\techo $(FLAGS)\n\ninclude rules.mk\n";
    const RULES: &str =
        "LIBS = -lm\nOBJ = x.o\nUNUSED = 1\n\n.PHONY: build\nbuild:\n\techo build\n";

    /// Check `paths` (relative to `dir`) at hint level, with the output's
    /// paths made relative to `dir`.
    fn check_in(dir: &Path, paths: &[&str], follow_includes: bool) -> (i32, String, String) {
        let mut options = options(
            paths.iter().map(|p| dir.join(p)).collect(),
            Severity::Hint,
            Format::Text,
        );
        options.follow_includes = follow_includes;
        let (code, out, err) = run_check(&options, dir);
        let prefix = format!("{}/", dir.display());
        (code, out.replace(&prefix, ""), err.replace(&prefix, ""))
    }

    fn fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    #[test]
    fn test_check_follows_includes() {
        let dir = fixture(&[("Makefile", MAKEFILE), ("rules.mk", RULES)]);
        assert_eq!(
            check_in(dir.path(), &["."], true),
            (
                EXIT_DIAGNOSTICS,
                "rules.mk:3:1: hint: variable 'UNUSED' is defined but never used [unused-variable]\n"
                    .to_string(),
                String::new()
            )
        );
    }

    #[test]
    fn test_check_included_file_not_reported_unless_checked() {
        let dir = fixture(&[("Makefile", MAKEFILE), ("rules.mk", RULES)]);
        assert_eq!(
            check_in(dir.path(), &["Makefile"], true),
            (EXIT_CLEAN, String::new(), String::new())
        );
        // A fragment checked on its own sees the makefile including it.
        assert_eq!(
            check_in(dir.path(), &["rules.mk", "Makefile", "rules.mk"], true),
            (
                EXIT_DIAGNOSTICS,
                "rules.mk:3:1: hint: variable 'UNUSED' is defined but never used [unused-variable]\n"
                    .to_string(),
                String::new()
            )
        );
    }

    #[test]
    fn test_check_no_follow_includes() {
        let dir = fixture(&[("Makefile", MAKEFILE), ("rules.mk", RULES)]);
        assert_eq!(
            check_in(dir.path(), &["."], false),
            (
                EXIT_DIAGNOSTICS,
                "Makefile:1:9: warning: variable 'LIBS' is not defined [undefined-variable]\n\
                 Makefile:3:12: warning: variable 'OBJ' is not defined [undefined-variable]\n\
                 rules.mk:1:1: hint: variable 'LIBS' is defined but never used [unused-variable]\n\
                 rules.mk:2:1: hint: variable 'OBJ' is defined but never used [unused-variable]\n\
                 rules.mk:3:1: hint: variable 'UNUSED' is defined but never used [unused-variable]\n"
                    .to_string(),
                String::new()
            )
        );
    }

    #[test]
    fn test_check_missing_include() {
        let dir = fixture(&[(
            "Makefile",
            "X = $(Y)\n.PHONY: all\nall: gen\n\techo $(X)\ninclude nope.mk\n",
        )]);
        // As the include can't be followed, Y and gen may be defined there.
        assert_eq!(
            check_in(dir.path(), &["Makefile"], true),
            (
                EXIT_DIAGNOSTICS,
                "Makefile:1:5: warning: variable 'Y' is not defined [undefined-variable]\n\
                 Makefile:5:9: warning: included file 'nope.mk' does not exist [missing-include-file]\n"
                    .to_string(),
                String::new()
            )
        );
    }

    #[test]
    fn test_check_include_cycle() {
        let dir = fixture(&[
            ("Makefile", "include a.mk\n.PHONY: all\nall: $(A)\n"),
            ("a.mk", "include Makefile\nA = a\nB = b\n"),
        ]);
        assert_eq!(
            check_in(dir.path(), &["."], true),
            (
                EXIT_DIAGNOSTICS,
                "a.mk:3:1: hint: variable 'B' is defined but never used [unused-variable]\n"
                    .to_string(),
                String::new()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_check_unreadable_include() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fixture(&[("Makefile", "include a.mk\n"), ("a.mk", "")]);
        let path = dir.path().join("a.mk");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            // Running as root; permissions are not enforced.
            return;
        }
        assert_eq!(
            check_in(dir.path(), &["Makefile"], true),
            (
                EXIT_DIAGNOSTICS,
                "Makefile:1:9: warning: included file 'a.mk' could not be read: \
                 Permission denied (os error 13) [unreadable-include-file]\n"
                    .to_string(),
                String::new()
            )
        );
    }
}
