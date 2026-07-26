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
