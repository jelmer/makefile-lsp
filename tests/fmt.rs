use std::process::Command;

fn makefile_lsp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_makefile-lsp"))
}

#[test]
fn test_fmt_rewrites_files_in_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/rules.mk"), "all:\n    echo hi  \n").unwrap();
    std::fs::write(dir.path().join("Makefile"), "all:\n\techo hi\n").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x  \n").unwrap();

    let output = makefile_lsp()
        .args(["fmt", "."])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        (output.status.code(), output.stdout, output.stderr),
        (Some(0), vec![], vec![])
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sub/rules.mk")).unwrap(),
        "all:\n\techo hi\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        "x  \n"
    );
}

#[test]
fn test_fmt_check_lists_unformatted_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.mk"), "all:\n    echo hi\n").unwrap();
    std::fs::write(dir.path().join("b.mk"), "all:\n\techo hi\n").unwrap();

    let output = makefile_lsp()
        .args(["fmt", "--check", "a.mk", "b.mk"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap()
        ),
        (Some(1), "a.mk\n".to_string())
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.mk")).unwrap(),
        "all:\n    echo hi\n"
    );
}

#[test]
fn test_fmt_parse_error_leaves_file_alone() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("bad.mk"), "ifeq (a,b)\nx = 1  \n").unwrap();
    std::fs::write(dir.path().join("good.mk"), "x = 1\nall:\n    echo\n").unwrap();

    let output = makefile_lsp()
        .args(["fmt", "bad.mk", "good.mk"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("makefile-lsp fmt: bad.mk: not formatting:"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("bad.mk")).unwrap(),
        "ifeq (a,b)\nx = 1  \n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("good.mk")).unwrap(),
        "x = 1\nall:\n\techo\n"
    );
}

#[test]
fn test_fmt_stdin() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = makefile_lsp()
        .arg("fmt")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"all:\n    echo hi")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        (output.status.code(), output.stdout),
        (Some(0), b"all:\n\techo hi\n".to_vec())
    );
}
