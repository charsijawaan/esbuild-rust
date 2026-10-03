use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, OnStartResult, Plugin, ResolveKind,
    ResolveOptions, build,
};

const ENTRY: &str = "import selected from 'pkg'; const required = require('pkg'); export const values = [selected, typeof required === 'string' ? required : required.default];";
static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct PackageFixture(PathBuf);

impl PackageFixture {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-package-conditions-{}-{unique}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let package = directory.join("node_modules/pkg");
        std::fs::create_dir_all(&package).expect("create conditional package");
        std::fs::write(
            package.join("package.json"),
            r#"{"exports":{".":{"custom":"./custom.js","module":"./module.js","import":"./import.js","require":"./require.cjs","default":"./default.cjs"}}}"#,
        )
        .expect("write package exports");
        for condition in ["custom", "module", "import"] {
            std::fs::write(
                package.join(format!("{condition}.js")),
                format!("export default '{condition}';"),
            )
            .expect("write ESM condition");
        }
        for condition in ["require", "default"] {
            std::fs::write(
                package.join(format!("{condition}.cjs")),
                format!("module.exports = '{condition}';"),
            )
            .expect("write CommonJS condition");
        }
        Self(directory)
    }

    fn build_options(
        &self,
        platform: BuildPlatform,
        conditions: Option<Vec<String>>,
    ) -> BuildOptions {
        BuildOptions {
            bundle: true,
            stdin: Some(BuildStdin {
                contents: ENTRY.into(),
                resolve_dir: self.0.to_string_lossy().into_owned(),
                ..BuildStdin::default()
            }),
            format: BuildFormat::CommonJs,
            platform,
            conditions,
            ..BuildOptions::default()
        }
    }
}

impl Drop for PackageFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn configurations() -> [Option<Vec<String>>; 4] {
    [
        None,
        Some(Vec::new()),
        Some(vec!["custom".into()]),
        Some(vec!["module".into()]),
    ]
}

fn expected(platform: BuildPlatform, conditions: Option<&[String]>) -> [&str; 2] {
    match conditions {
        Some(conditions) if !conditions.is_empty() => [conditions[0].as_str(); 2],
        None if platform != BuildPlatform::Neutral => ["module"; 2],
        _ => ["import", "require"],
    }
}

fn execute(code: &[u8], expected: [&str; 2]) {
    let source = format!(
        "{}\nconsole.log(JSON.stringify(module.exports.values));",
        String::from_utf8_lossy(code)
    );
    let output = Command::new("node")
        .args(["-e", &source])
        .output()
        .expect("Node.js is required for package export condition regressions");
    assert!(
        output.status.success(),
        "{}\n{source}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("[\"{}\",\"{}\"]\n", expected[0], expected[1])
    );
}

#[test]
fn api_distinguishes_default_and_explicit_package_conditions() {
    let fixture = PackageFixture::new();
    for platform in [
        BuildPlatform::Browser,
        BuildPlatform::Node,
        BuildPlatform::Neutral,
    ] {
        for conditions in configurations() {
            for minify in [false, true] {
                let mut options = fixture.build_options(platform, conditions.clone());
                options.minify_whitespace = minify;
                options.minify_identifiers = minify;
                options.minify_syntax = minify;
                let result = build(options);
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                execute(
                    &result.output_files[0].contents,
                    expected(platform, conditions.as_deref()),
                );
            }
        }
    }
}

#[test]
fn cli_distinguishes_omitted_and_empty_conditions_flags() {
    let fixture = PackageFixture::new();
    for (platform, flag) in [
        (BuildPlatform::Browser, "--platform=browser"),
        (BuildPlatform::Node, "--platform=node"),
        (BuildPlatform::Neutral, "--platform=neutral"),
    ] {
        for conditions in configurations() {
            for minify in [false, true] {
                let mut command = Command::new(env!("CARGO_BIN_EXE_esbuild"));
                command
                    .current_dir(&fixture.0)
                    .args(["--bundle", "--format=cjs", flag]);
                if let Some(conditions) = &conditions {
                    command.arg(format!("--conditions={}", conditions.join(",")));
                }
                if minify {
                    command.arg("--minify");
                }
                let mut child = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("start Rust CLI");
                child
                    .stdin
                    .take()
                    .expect("CLI stdin")
                    .write_all(ENTRY.as_bytes())
                    .expect("write CLI input");
                let output = child.wait_with_output().expect("wait for Rust CLI");
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                execute(&output.stdout, expected(platform, conditions.as_deref()));
            }
        }
    }
}

#[test]
fn plugin_builtin_resolve_uses_the_same_package_conditions() {
    let fixture = PackageFixture::new();
    for platform in [
        BuildPlatform::Browser,
        BuildPlatform::Node,
        BuildPlatform::Neutral,
    ] {
        for conditions in configurations() {
            let directory = fixture.0.clone();
            let names = expected(platform, conditions.as_deref()).map(str::to_string);
            let plugin = Plugin::new("condition-resolve", move |plugin_build| {
                let resolve = plugin_build.resolve.clone();
                let directory = directory.clone();
                let names = names.clone();
                plugin_build.on_start(move || {
                    for (index, kind) in [ResolveKind::ImportStatement, ResolveKind::RequireCall]
                        .into_iter()
                        .enumerate()
                    {
                        let result = resolve(
                            "pkg",
                            ResolveOptions {
                                resolve_dir: directory.to_string_lossy().into_owned(),
                                kind,
                                ..ResolveOptions::default()
                            },
                        );
                        assert!(result.errors.is_empty(), "{:?}", result.errors);
                        let extension = if names[index] == "require" {
                            "cjs"
                        } else {
                            "js"
                        };
                        assert!(
                            result
                                .path
                                .ends_with(&format!("/{}.{}", names[index], extension)),
                            "{}",
                            result.path
                        );
                    }
                    Ok(OnStartResult::default())
                });
                Ok(())
            });
            let mut options = fixture.build_options(platform, conditions);
            options.plugins.push(plugin);
            let result = build(options);
            assert!(result.errors.is_empty(), "{:?}", result.errors);
        }
    }
}
