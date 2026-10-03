use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "esbuild-rs-cli-watch-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create watch fixture");
        Self(std::fs::canonicalize(path).expect("canonical watch fixture"))
    }

    fn write(&self, path: &str, contents: &str) {
        std::fs::write(self.0.join(path), contents).expect("write watch input");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct WatchChild {
    child: Child,
    lines: Receiver<(bool, String)>,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

impl WatchChild {
    fn new(fixture: &Fixture, arguments: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args(arguments)
            .current_dir(&fixture.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start watch CLI");
        let (sender, lines) = mpsc::channel();
        let stdout = child.stdout.take().expect("watch stdout");
        let stderr = child.stderr.take().expect("watch stderr");
        let stderr_sender = sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send((false, line.expect("read stdout"))).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if stderr_sender
                    .send((true, line.expect("read stderr")))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn wait(&mut self, condition: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition(self) {
            let wait = deadline.saturating_duration_since(Instant::now());
            let (stderr, line) = self.lines.recv_timeout(wait).unwrap_or_else(|error| {
                panic!(
                    "watch condition: {error}; stdout={:?}, stderr={:?}",
                    self.stdout, self.stderr
                )
            });
            if stderr {
                self.stderr.push(line);
            } else {
                self.stdout.push(line);
            }
        }
    }

    fn finished(&self) -> usize {
        self.stderr
            .iter()
            .filter(|line| line.contains("[watch] build finished"))
            .count()
    }
}

impl Drop for WatchChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn watches_stdout_and_ignores_closed_stdin_in_forever_mode() {
    let fixture = Fixture::new();
    fixture.write("in.js", "console.log(1+2)");
    let mut watch = WatchChild::new(&fixture, &["--watch=forever", "in.js"]);
    watch.child.stdin.take();
    for (build, input, output) in [
        (1, "console.log(1+2)", "console.log(1 + 2);"),
        (2, "console.log(2+3)", "console.log(2 + 3);"),
        (3, "console.log(3+4)", "console.log(3 + 4);"),
    ] {
        if build != 1 {
            fixture.write("in.js", input);
        }
        watch.wait(|watch| {
            watch.finished() >= build && watch.stdout.iter().any(|line| line == output)
        });
    }
    assert_eq!(
        watch.stdout,
        [
            "console.log(1 + 2);",
            "console.log(2 + 3);",
            "console.log(3 + 4);"
        ]
    );
    assert_eq!(
        watch.stderr,
        [
            "[watch] build finished, watching for changes...",
            "[watch] build started (change: \"in.js\")",
            "[watch] build finished",
            "[watch] build started (change: \"in.js\")",
            "[watch] build finished"
        ]
    );
}

#[test]
fn watches_dependencies_recovers_from_errors_and_updates_metafiles() {
    let fixture = Fixture::new();
    fixture.write(
        "in.js",
        "import { value } from './dep.js'; console.log(value)",
    );
    fixture.write("dep.js", "export const value = 1;");
    let mut watch = WatchChild::new(
        &fixture,
        &[
            "in.js",
            "--watch=forever",
            "--bundle",
            "--outfile=out.js",
            "--metafile=reports/meta.json",
        ],
    );
    watch.wait(|watch| watch.finished() == 1);
    assert!(
        std::fs::read_to_string(fixture.0.join("out.js"))
            .unwrap()
            .contains("value = 1")
    );
    fixture.write("dep.js", "export const value = 222;");
    watch.wait(|watch| watch.finished() == 2);
    assert!(
        watch
            .stderr
            .iter()
            .any(|line| line == "[watch] build started (change: \"dep.js\")")
    );
    assert!(
        std::fs::read_to_string(fixture.0.join("out.js"))
            .unwrap()
            .contains("value = 222")
    );
    let metadata = std::fs::read_to_string(fixture.0.join("reports/meta.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(
        json["inputs"]["dep.js"]["bytes"],
        "export const value = 222;".len()
    );
    fixture.write("dep.js", "export const value = ;");
    watch.wait(|watch| watch.finished() == 3);
    assert!(watch.stderr.iter().any(|line| line.contains("[ERROR]")));
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("reports/meta.json")).unwrap(),
        metadata
    );
    fixture.write("dep.js", "export const value = 4444;");
    watch.wait(|watch| watch.finished() == 4);
    assert!(
        std::fs::read_to_string(fixture.0.join("out.js"))
            .unwrap()
            .contains("value = 4444")
    );
    let json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixture.0.join("reports/meta.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        json["inputs"]["dep.js"]["bytes"],
        "export const value = 4444;".len()
    );
}

#[test]
fn normal_watch_stops_on_stdin_eof_and_reports_delay_and_absolute_paths() {
    let fixture = Fixture::new();
    fixture.write("in.js", "foo()");
    let mut watch = WatchChild::new(
        &fixture,
        &[
            "--watch=true",
            "in.js",
            "--watch-delay=50",
            "--abs-paths=log",
        ],
    );
    watch.wait(|watch| watch.finished() == 1);
    assert_eq!(
        watch.stderr[0],
        "[watch] build finished, watching for changes with a 50ms delay..."
    );
    fixture.write("in.js", "foo(1)");
    watch.wait(|watch| watch.finished() == 2);
    assert_eq!(
        watch.stderr[1],
        format!(
            "[watch] build started (change: {:?})",
            fixture.0.join("in.js").to_string_lossy()
        )
    );
    watch.child.stdin.take();
    watch.wait(|watch| {
        watch
            .stderr
            .iter()
            .any(|line| line.contains("stopped automatically because stdin was closed"))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = watch.child.try_wait().expect("watch exit") {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watch did not exit after stdin EOF"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn watch_flags_validate_and_false_disables_watch() {
    let fixture = Fixture::new();
    fixture.write("in.js", "foo()");
    for argument in ["--watch=x", "--watch-delay=wat"] {
        let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .args([argument, "in.js"])
            .current_dir(&fixture.0)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("[ERROR] Invalid value"));
        assert!(stderr.ends_with("1 error\n"));
        assert!(!stderr.contains("[watch]"));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
        .args(["in.js", "--watch=forever", "--watch=false"])
        .current_dir(&fixture.0)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"foo();\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn watch_status_honors_colors_and_log_levels() {
    let fixture = Fixture::new();
    fixture.write("in.js", "foo(1)");
    let mut watch = WatchChild::new(&fixture, &["in.js", "--watch=forever", "--color"]);
    watch.wait(|watch| watch.finished() == 1);
    assert_eq!(
        watch.stderr[0],
        "\u{1b}[37m[watch] build finished, watching for changes...\u{1b}[0m"
    );
    fixture.write("in.js", "foo(2)");
    watch.wait(|watch| watch.finished() == 2);
    assert_eq!(
        watch.stderr[1],
        "\u{1b}[37m[watch] build started (change: \"in.js\")\u{1b}[0m"
    );
    assert_eq!(watch.stderr[2], "\u{1b}[37m[watch] build finished\u{1b}[0m");
    drop(watch);
    for level in [
        "--log-level=warning",
        "--log-level=error",
        "--log-level=silent",
    ] {
        let mut watch = WatchChild::new(&fixture, &["in.js", "--watch=forever", level]);
        watch.wait(|watch| watch.stdout.iter().any(|line| line == "foo(2);"));
        fixture.write("in.js", "foo(333)");
        watch.wait(|watch| watch.stdout.iter().any(|line| line == "foo(333);"));
        assert!(watch.stderr.is_empty());
        drop(watch);
        fixture.write("in.js", "foo(2)");
    }
}

#[test]
fn watch_updates_property_cache_only_after_successful_builds() {
    let fixture = Fixture::new();
    fixture.write("in.js", "foo()");
    let mut watch = WatchChild::new(
        &fixture,
        &[
            "in.js",
            "--watch=forever",
            "--outdir=out",
            "--mangle-props=.",
            "--mangle-cache=cache/mangle.json",
        ],
    );
    watch.wait(|watch| watch.finished() == 1);
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("cache/mangle.json")).unwrap(),
        "{}\n"
    );
    fixture.write("in.js", "foo(bar.baz)");
    watch.wait(|watch| watch.finished() == 2);
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("out/in.js")).unwrap(),
        "foo(bar.a);\n"
    );
    let cache = "{\n  \"baz\": \"a\"\n}\n";
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("cache/mangle.json")).unwrap(),
        cache
    );
    fixture.write("in.js", "const x = ;");
    watch.wait(|watch| watch.finished() == 3);
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("cache/mangle.json")).unwrap(),
        cache
    );
    fixture.write("in.js", "foo(bar.quux)");
    watch.wait(|watch| watch.finished() == 4);
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("out/in.js")).unwrap(),
        "foo(bar.a);\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("cache/mangle.json")).unwrap(),
        "{\n  \"quux\": \"a\"\n}\n"
    );
}
