#![cfg(any(target_os = "macos", target_os = "windows"))]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use esbuild_rs::api::{AbsPaths, BuildFormat, BuildOptions, BuildResult, LogLevel, build};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct CaseFixture(PathBuf);

impl CaseFixture {
    fn new(package: bool, importer_in_package: bool) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-path-case-{}-{unique}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let package_dir = directory.join("node_modules/pkg");
        std::fs::create_dir_all(&package_dir).expect("create case fixture");
        let files = if package { &package_dir } else { &directory };
        std::fs::write(files.join("file1.js"), "export default 123").expect("write lowercase file");
        std::fs::write(files.join("File2.js"), "export default 234").expect("write uppercase file");
        let prefix = if package && !importer_in_package {
            "pkg/"
        } else {
            "./"
        };
        let source = format!(
            "import x from '{prefix}File1.js';\nimport y from '{prefix}file2.js';\nconsole.log(JSON.stringify([x, y]));"
        );
        if importer_in_package {
            std::fs::write(package_dir.join("index.js"), source).expect("write package importer");
            std::fs::write(directory.join("entry.js"), "import 'pkg'").expect("write entry");
        } else {
            std::fs::write(directory.join("entry.js"), source).expect("write entry");
        }
        Self(std::fs::canonicalize(directory).expect("canonical case fixture"))
    }

    fn options(&self) -> BuildOptions {
        BuildOptions {
            entry_points: vec!["entry.js".into()],
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            bundle: true,
            format: BuildFormat::CommonJs,
            ..BuildOptions::default()
        }
    }
}

impl Drop for CaseFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn execute(result: &BuildResult) {
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(&result.output_files[0].contents).as_ref())
        .output()
        .expect("execute case-insensitive bundle with Node.js");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"[123,234]\n");
}

#[test]
fn external_importers_warn_about_filename_case_with_locations_and_path_styles() {
    for package in [false, true] {
        let fixture = CaseFixture::new(package, false);
        for absolute in [false, true] {
            for minify in [false, true] {
                let mut options = fixture.options();
                if absolute {
                    options.abs_paths = AbsPaths::LOG;
                }
                options.minify_identifiers = minify;
                options.minify_syntax = minify;
                options.minify_whitespace = minify;
                let result = build(options);
                execute(&result);
                assert_eq!(result.warnings.len(), 2, "{:?}", result.warnings);
                let prefix = if package { "node_modules/pkg/" } else { "" };
                for (index, (actual, query)) in [("file1.js", "File1.js"), ("File2.js", "file2.js")]
                    .into_iter()
                    .enumerate()
                {
                    let path = |name: &str| {
                        let relative = format!("{prefix}{name}");
                        if absolute {
                            fixture.0.join(relative).to_string_lossy().into_owned()
                        } else {
                            relative
                        }
                    };
                    let warning = &result.warnings[index];
                    assert_eq!(warning.id, "different-path-case");
                    assert_eq!(
                        warning.text,
                        format!(
                            "Use {:?} instead of {:?} to avoid issues with case-sensitive file systems",
                            path(actual),
                            path(query)
                        )
                    );
                    assert_eq!(
                        warning.location.as_ref().expect("import location").line,
                        index + 1
                    );
                }
            }
        }
    }
}

#[test]
fn importers_inside_node_modules_do_not_emit_path_case_warnings() {
    let fixture = CaseFixture::new(true, true);
    let result = build(fixture.options());
    execute(&result);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
}

#[test]
fn path_case_warning_overrides_preserve_suppression_and_error_promotion() {
    let fixture = CaseFixture::new(false, false);
    for level in [LogLevel::Silent, LogLevel::Error] {
        let mut options = fixture.options();
        options
            .log_override
            .insert("different-path-case".into(), level);
        let result = build(options);
        assert!(result.warnings.is_empty());
        if level == LogLevel::Error {
            assert_eq!(result.errors.len(), 2);
            assert!(result.output_files.is_empty());
        } else {
            execute(&result);
        }
    }
}
