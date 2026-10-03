use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use esbuild_rs::api::{BuildFormat, BuildOptions, BuildSourceMap, build};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct InputFixture(PathBuf);

impl InputFixture {
    fn new(source: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-cli-stdout-{}-{unique}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("create stdout fixture");
        std::fs::write(directory.join("example.js"), source).expect("write input");
        Self(directory)
    }

    fn cli(&self, flags: &[&str], absolute: bool) -> Output {
        Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .current_dir(&self.0)
            .arg(if absolute {
                self.0.join("example.js")
            } else {
                PathBuf::from("example.js")
            })
            .args(flags)
            .args(["--log-level=warning", "--color=false"])
            .output()
            .expect("run Rust CLI")
    }
}

impl Drop for InputFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn inline_map(code: &[u8]) -> serde_json::Value {
    let code = String::from_utf8_lossy(code);
    let base64 = code
        .split("//# sourceMappingURL=data:application/json;base64,")
        .nth(1)
        .expect("inline source map")
        .trim();
    let output = Command::new("node")
        .args([
            "-e",
            "process.stdout.write(Buffer.from(process.argv[1], 'base64'))",
            base64,
        ])
        .output()
        .expect("decode source map with Node.js");
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).expect("source map JSON")
}

#[test]
fn file_builds_to_stdout_keep_inputs_and_use_relative_source_maps() {
    let source = "exports.foo = 123";
    let fixture = InputFixture::new(source);
    for absolute in [false, true] {
        for bundle in [false, true] {
            for sourcemap in [false, true] {
                let mut flags = Vec::new();
                if bundle {
                    flags.extend(["--bundle", "--format=cjs"]);
                }
                if sourcemap {
                    flags.push("--sourcemap");
                }
                let output = fixture.cli(&flags, absolute);
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(output.stderr.is_empty());
                let prefix = if bundle {
                    "// example.js\nexports.foo = 123;\n"
                } else {
                    "exports.foo = 123;\n"
                };
                if sourcemap {
                    assert!(output.stdout.starts_with(prefix.as_bytes()));
                    let map = inline_map(&output.stdout);
                    assert_eq!(map["version"], 3);
                    assert_eq!(map["sources"], serde_json::json!(["example.js"]));
                    assert_eq!(map["sourcesContent"], serde_json::json!([source]));
                } else {
                    assert_eq!(output.stdout, prefix.as_bytes());
                }
                assert_eq!(
                    std::fs::read_to_string(fixture.0.join("example.js"))
                        .expect("read original input"),
                    source
                );
            }
        }
    }
}

#[test]
fn file_builds_to_stdout_honor_extension_loaders_and_output_restrictions() {
    let fixture = InputFixture::new("stuff");
    for absolute in [false, true] {
        let output = fixture.cli(&["--loader:.js=text"], absolute);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"module.exports = \"stuff\";\n");
        for flags in [
            vec!["--metafile=graph.json"],
            vec!["--sourcemap=external"],
            vec!["--loader:.js=file"],
            vec!["--loader:.js=copy"],
        ] {
            let output = fixture.cli(&flags, absolute);
            assert!(!output.status.success(), "{flags:?}");
            assert!(output.stdout.is_empty(), "{flags:?}");
        }
    }
    assert!(!fixture.0.join("graph.json").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("example.js")).expect("read original input"),
        "stuff"
    );
}

#[test]
fn unwritten_api_outputs_use_stdout_topology_without_overwrite_errors() {
    let source = "exports.foo = 123";
    let fixture = InputFixture::new(source);
    for bundle in [false, true] {
        let result = build(BuildOptions {
            entry_points: vec!["example.js".into()],
            abs_working_dir: fixture.0.to_string_lossy().into_owned(),
            bundle,
            format: BuildFormat::CommonJs,
            sourcemap: BuildSourceMap::Inline,
            ..BuildOptions::default()
        });
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.output_files.len(), 1);
        assert_eq!(result.output_files[0].path, "<stdout>");
        assert_eq!(
            inline_map(&result.output_files[0].contents)["sources"],
            serde_json::json!(["example.js"])
        );
    }
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("example.js")).expect("read original input"),
        source
    );
    std::fs::write(fixture.0.join("style.css"), ".test { color: red }").expect("write CSS");
    std::fs::write(fixture.0.join("example.js"), "import './style.css'").expect("write CSS import");
    let result = build(BuildOptions {
        entry_points: vec!["example.js".into()],
        abs_working_dir: fixture.0.to_string_lossy().into_owned(),
        bundle: true,
        ..BuildOptions::default()
    });
    assert_eq!(result.errors.len(), 1);
    assert_eq!(
        result.errors[0].text,
        "Cannot import \"style.css\" into a JavaScript file without an output path configured"
    );
}
