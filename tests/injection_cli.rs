use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let unique = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "esbuild-rs-injection-cli-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let fixture = Self(std::fs::canonicalize(path).unwrap());
        std::fs::write(fixture.0.join("entry.js"), "console.log(value)").unwrap();
        std::fs::write(fixture.0.join("inject.js"), "export let value = 7;").unwrap();
        fixture
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cli_injection_validates_paths_loaders_and_transform_flags() {
    let fixture = Fixture::new();
    for (arguments, expected) in [
        (
            vec!["entry.js", "--bundle", "--inject:missing.js"],
            "Could not resolve \"missing.js\"",
        ),
        (
            vec![
                "entry.js",
                "--inject:inject.js",
                "--loader:.js=copy",
                "--outdir=out",
            ],
            "Cannot inject \"inject.js\" with the \"copy\" loader without bundling enabled",
        ),
        (
            vec!["--inject:inject.js"],
            "Invalid transform flag: \"--inject:inject.js\"",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args(arguments)
            .arg("--color=false")
            .current_dir(&fixture.0)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(expected), "{error}");
    }
    for flag in ["--external:./inject.js", "--external:*"] {
        let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args(["entry.js", "--bundle", "--inject:inject.js", flag])
            .current_dir(&fixture.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let code = String::from_utf8(output.stdout).unwrap();
        let executed = Command::new("node").args(["-e", &code]).output().unwrap();
        assert!(executed.status.success());
        assert_eq!(executed.stdout, b"7\n");
    }
}

#[test]
fn stdin_injection_diagnostics_use_absolute_primary_and_export_paths() {
    let fixture = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .args([
            "--bundle",
            "--inject:inject.js",
            "--sourcefile=entry.js",
            "--abs-paths=log",
            "--color=false",
        ])
        .current_dir(&fixture.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"value = 1;")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains(&format!("{}:1:0:", fixture.0.join("entry.js").display())),
        "{error}"
    );
    assert!(
        error.contains(&format!("{}:1:11:", fixture.0.join("inject.js").display())),
        "{error}"
    );
    assert!(
        error.contains("because it's an import from an injected file"),
        "{error}"
    );
}
