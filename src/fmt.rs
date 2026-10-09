//! Implementation of the `fmt` subcommand, which formats Makefiles on disk
//! or on stdin.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use makefile_lossless::Makefile;

use crate::check::{collect_files, EXIT_CLEAN, EXIT_DIAGNOSTICS, EXIT_ERROR};
use crate::formatting::{format, FormatError};

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    /// Only report files that are not formatted, without changing them.
    pub check: bool,
    /// Files and directories to format; empty to format stdin.
    pub paths: Vec<PathBuf>,
}

/// Parse the arguments following `fmt`. Returns `Ok(None)` when help was
/// requested.
pub fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut check = false;
    let mut paths = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            "-h" | "--help" => return Ok(None),
            "-" => paths.push(PathBuf::from("-")),
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}'"));
            }
            other => paths.push(PathBuf::from(other)),
        }
    }
    if paths == [Path::new("-")] {
        paths.clear();
    } else if paths.iter().any(|p| p == Path::new("-")) {
        return Err("'-' cannot be combined with other paths".to_string());
    }
    Ok(Some(Options { check, paths }))
}

fn print_help() {
    eprintln!(
        "Usage: makefile-lsp fmt [OPTIONS] [PATH...]\n\n\
         Format Makefiles in place.\n\n\
         Directories are searched recursively (skipping hidden directories) for\n\
         Makefile, makefile, GNUmakefile, *.mk and *.mak. Files given explicitly\n\
         are formatted regardless of their name. With no PATH, or with '-', the\n\
         Makefile is read from stdin and the formatted version written to stdout.\n\n\
         Files with parse errors are not formatted.\n\n\
         Options:\n      \
         --check   Do not change anything; list the files that are not\n                \
         formatted\n  \
         -h, --help    Show this help\n\n\
         Exit status: 0 on success, 1 if --check found unformatted files, 2 on\n\
         usage, parse or I/O errors."
    );
}

/// Run the `fmt` subcommand and return the process exit code.
pub fn run(args: &[String]) -> i32 {
    let options = match parse_args(args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print_help();
            return EXIT_CLEAN;
        }
        Err(e) => {
            eprintln!("makefile-lsp fmt: {e}");
            return EXIT_ERROR;
        }
    };
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    if options.paths.is_empty() {
        fmt_stdin(
            options.check,
            &mut std::io::stdin().lock(),
            &mut out,
            &mut err,
        )
    } else {
        fmt_files(&options, &mut out, &mut err)
    }
}

/// Format the text read from `input`, writing it to `out`. With `check`,
/// nothing is written and the exit code tells whether it was formatted.
pub fn fmt_stdin(
    check: bool,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let mut text = String::new();
    let result = input
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())
        .and_then(|_| format_text(&text).map_err(|e| e.to_string()));
    let formatted = match result {
        Ok(formatted) => formatted,
        Err(e) => {
            let _ = writeln!(err, "makefile-lsp fmt: <stdin>: {e}");
            return EXIT_ERROR;
        }
    };
    if check {
        return if formatted == text {
            EXIT_CLEAN
        } else {
            EXIT_DIAGNOSTICS
        };
    }
    if let Err(e) = out.write_all(formatted.as_bytes()) {
        let _ = writeln!(err, "makefile-lsp fmt: failed to write output: {e}");
        return EXIT_ERROR;
    }
    EXIT_CLEAN
}

/// Format the files selected by `options` in place. With `options.check`,
/// list the files that would change on `out` instead.
pub fn fmt_files(options: &Options, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut errors = Vec::new();
    let mut unformatted = false;
    for file in collect_files(&options.paths, &mut errors) {
        match fmt_file(&file, options.check) {
            Ok(true) if options.check => {
                unformatted = true;
                if let Err(e) = writeln!(out, "{}", file.display()) {
                    errors.push(format!("failed to write output: {e}"));
                }
            }
            Ok(_) => {}
            Err(e) => errors.push(format!("{}: {e}", file.display())),
        }
    }

    for e in &errors {
        // Nothing sensible can be done if stderr is unwritable.
        let _ = writeln!(err, "makefile-lsp fmt: {e}");
    }

    if !errors.is_empty() {
        EXIT_ERROR
    } else if unformatted {
        EXIT_DIAGNOSTICS
    } else {
        EXIT_CLEAN
    }
}

/// Format `path`, writing it back unless `check` is set. Returns whether the
/// file was not formatted.
fn fmt_file(path: &Path, check: bool) -> Result<bool, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let formatted = format_text(&text).map_err(|e| e.to_string())?;
    if formatted == text {
        return Ok(false);
    }
    if !check {
        std::fs::write(path, formatted).map_err(|e| e.to_string())?;
    }
    Ok(true)
}

fn format_text(text: &str) -> Result<String, FormatError> {
    format(&Makefile::parse(text), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parse_args_default_is_stdin() {
        assert_eq!(
            parse_args(&[]),
            Ok(Some(Options {
                check: false,
                paths: vec![]
            }))
        );
        assert_eq!(
            parse_args(&args(&["--check", "-"])),
            Ok(Some(Options {
                check: true,
                paths: vec![]
            }))
        );
    }

    #[test]
    fn test_parse_args_paths() {
        assert_eq!(
            parse_args(&args(&["Makefile", "build/"])),
            Ok(Some(Options {
                check: false,
                paths: vec![PathBuf::from("Makefile"), PathBuf::from("build/")]
            }))
        );
    }

    #[test]
    fn test_parse_args_errors() {
        assert_eq!(
            parse_args(&args(&["--write"])),
            Err("unknown option '--write'".to_string())
        );
        assert_eq!(
            parse_args(&args(&["-", "Makefile"])),
            Err("'-' cannot be combined with other paths".to_string())
        );
        assert_eq!(parse_args(&args(&["--help"])), Ok(None));
    }

    fn run_stdin(check: bool, input: &str) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = fmt_stdin(check, &mut input.as_bytes(), &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn test_stdin() {
        assert_eq!(
            run_stdin(false, "all:  \n    echo hi"),
            (EXIT_CLEAN, "all:\n\techo hi\n".to_string(), String::new())
        );
    }

    #[test]
    fn test_stdin_check() {
        assert_eq!(
            run_stdin(true, "all:\n    echo hi\n"),
            (EXIT_DIAGNOSTICS, String::new(), String::new())
        );
        assert_eq!(
            run_stdin(true, "all:\n\techo hi\n"),
            (EXIT_CLEAN, String::new(), String::new())
        );
    }

    #[test]
    fn test_stdin_parse_error() {
        let (code, out, err) = run_stdin(false, "ifeq (a,b)\n");
        assert_eq!((code, out.as_str()), (EXIT_ERROR, ""));
        assert!(
            err.starts_with("makefile-lsp fmt: <stdin>: not formatting:"),
            "{err}"
        );
    }
}
