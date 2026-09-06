use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn log_levels_filter_diagnostics_without_changing_exit_status() {
    for level in ["info", "warning", "error", "silent"] {
        for (loader, input, fails) in [("css", "//", false), ("js", "let =", true)] {
            let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
                .args([
                    format!("--loader={loader}"),
                    format!("--log-level={level}"),
                    "--color=false".into(),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn esbuild");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.success(), !fails, "{level} {loader}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            let visible = level != "silent" && (fails || level != "error");
            assert_eq!(!stderr.is_empty(), visible, "{level} {loader}: {stderr:?}");
            if visible {
                let noun = if fails { "error" } else { "warning" };
                assert_eq!(
                    stderr.contains(&format!("1 {noun}")),
                    level == "info",
                    "{stderr:?}"
                );
                assert_eq!(stderr.ends_with("\n\n"), level != "info", "{stderr:?}");
            }
        }
    }
}

#[test]
fn syntax_errors_do_not_leak_internal_lexer_panics() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .arg("--loader=ts")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn esbuild");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"class Foo { constructor(public {x}) {} }")
        .expect("write invalid TypeScript");
    let output = child.wait_with_output().expect("wait for esbuild");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr.contains("[ERROR] Expected identifier but found \"{\""),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("thread '"), "{stderr}");
}

#[test]
fn syntax_errors_include_source_excerpt_and_summary() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn esbuild");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"function f(){return new.target} const g=()=>new.target")
        .expect("write invalid JavaScript");
    let output = child.wait_with_output().expect("wait for esbuild");

    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        "✘ [ERROR] Cannot use \"new.target\" here:\n\
         \n\
         \x20\x20\x20\x20<stdin>:1:44:\n\
         \x20\x20\x20\x20\x20\x201 │ function f(){return new.target} const g=()=>new.target\n\
         \x20\x20\x20\x20\x20\x20\x20\x20╵                                             ~~~~~~~~~~\n\
         \n\
         1 error\n"
    );
}

#[test]
fn formatted_stdin_does_not_discover_an_ambient_tsconfig() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("esbuild-rs-transform-tsconfig-{unique}"));
    std::fs::create_dir_all(&directory).expect("create test directory");
    std::fs::write(
        directory.join("tsconfig.json"),
        r#"{"compilerOptions":{"jsxFactory":"ambient"}}"#,
    )
    .expect("write ambient tsconfig");

    let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .args(["--loader=jsx", "--format=esm"])
        .current_dir(&directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn esbuild");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"<div />")
        .expect("write JSX");
    let output = child.wait_with_output().expect("wait for esbuild");
    std::fs::remove_dir_all(directory).expect("remove test directory");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(
        stdout.contains("React.createElement(\"div\", null)"),
        "{stdout}"
    );
    assert!(!stdout.contains("ambient("), "{stdout}");
}
