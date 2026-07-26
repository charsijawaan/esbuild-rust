use std::{
    io::Write,
    process::{Command, Stdio},
};

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
        stderr.contains("error: Expected identifier but found \"{\""),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("thread '"), "{stderr}");
}
