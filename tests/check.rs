use std::process::Command;

fn makefile_lsp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_makefile-lsp"))
}

#[test]
fn test_check_walks_current_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/rules.mk"), "all:\n    echo hi\n").unwrap();

    let output = makefile_lsp()
        .arg("check")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("sub/rules.mk:2:"), "{stdout}");
}

#[test]
fn test_check_clean() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Makefile"), "all:\n\techo hi\n").unwrap();

    let output = makefile_lsp()
        .args(["check", "Makefile"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"");
}

#[test]
fn test_check_usage_error() {
    let output = makefile_lsp()
        .args(["check", "--severity", "loud"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "makefile-lsp check: unknown severity 'loud'\n"
    );
}

#[test]
fn test_check_missing_path() {
    let dir = tempfile::tempdir().unwrap();
    let output = makefile_lsp()
        .args(["check", "missing.mk"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn test_check_follows_includes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Makefile"),
        "all: $(OBJ)\n\techo $(OBJ)\ninclude rules.mk\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("rules.mk"), "OBJ = x.o\nx.o:\n\ttouch $@\n").unwrap();

    let output = makefile_lsp()
        .args(["check", "Makefile"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap()
        ),
        (Some(0), String::new())
    );

    let output = makefile_lsp()
        .args(["check", "--no-follow-includes", "Makefile"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap()
        ),
        (
            Some(1),
            "Makefile:1:6: warning: variable 'OBJ' is not defined [undefined-variable]\n"
                .to_string()
        )
    );
}
