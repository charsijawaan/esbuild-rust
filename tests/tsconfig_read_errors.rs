use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use esbuild_rs::api::{AbsPaths, BuildOptions, BuildResult, build};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct ConfigFixture(PathBuf);

impl ConfigFixture {
    fn new(extends: &str, files: &[(&str, &str)], directories: &[&str]) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-tsconfig-read-{}-{unique}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(directory.join("src")).expect("create tsconfig fixture");
        std::fs::write(directory.join("src/entry.jsx"), "console.log(<div/>);")
            .expect("write JSX input");
        std::fs::write(
            directory.join("src/tsconfig.json"),
            serde_json::json!({"extends":extends}).to_string(),
        )
        .expect("write extending config");
        for (path, contents) in files {
            let path = directory.join(path);
            std::fs::create_dir_all(path.parent().expect("file parent"))
                .expect("create file parent");
            std::fs::write(path, contents).expect("write base config");
        }
        for path in directories {
            std::fs::create_dir_all(directory.join(path)).expect("create config directory");
        }
        Self(std::fs::canonicalize(directory).expect("canonical config fixture"))
    }

    fn build(&self, absolute: bool) -> BuildResult {
        build(BuildOptions {
            entry_points: vec!["src/entry.jsx".into()],
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            outfile: "out.js".into(),
            abs_paths: if absolute {
                AbsPaths::LOG
            } else {
                AbsPaths::default()
            },
            ..BuildOptions::default()
        })
    }
}

impl Drop for ConfigFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const BASE: &str = r#"{"compilerOptions":{"jsxFactory":"factory"}}"#;
const ALTERNATE: &str = r#"{"compilerOptions":{"jsxFactory":"alternate"}}"#;

#[test]
fn relative_directory_configs_report_read_errors_without_missing_config_warnings() {
    for (extends, directories, files, failing_path) in [
        (
            "./base.json",
            vec!["src/base.json"],
            vec![("src/base.json.json", BASE)],
            "src/base.json",
        ),
        ("./base", vec!["src/base"], Vec::new(), "src/base"),
        (
            "./base",
            vec!["src/base", "src/base.json"],
            Vec::new(),
            "src/base",
        ),
    ] {
        let fixture = ConfigFixture::new(extends, &files, &directories);
        for absolute in [false, true] {
            let result = fixture.build(absolute);
            assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
            assert!(result.warnings.is_empty(), "{:?}", result.warnings);
            let path = if absolute {
                fixture.0.join(failing_path).to_string_lossy().into_owned()
            } else {
                failing_path.to_string()
            };
            assert_eq!(
                result.errors[0].text,
                format!("Cannot read file {path:?}: is a directory")
            );
            let location = result.errors[0]
                .location
                .as_ref()
                .expect("extends location");
            assert_eq!(location.line, 1);
            assert_eq!(location.length, extends.len() + 2);
            assert!(location.file.ends_with("src/tsconfig.json"));
            assert!(result.output_files.is_empty());
        }
    }
}

#[test]
fn extension_fallback_and_package_search_skip_directories_only_where_upstream_does() {
    for (extends, directories, files) in [
        ("./base", Vec::new(), vec![("src/base.json", BASE)]),
        ("./base", vec!["src/base"], vec![("src/base.json", BASE)]),
        (
            "./base",
            Vec::new(),
            vec![("src/base", BASE), ("src/base.json", ALTERNATE)],
        ),
        (
            "pkg",
            vec!["node_modules/pkg/tsconfig.json"],
            vec![("node_modules/pkg.json", BASE)],
        ),
    ] {
        let fixture = ConfigFixture::new(extends, &files, &directories);
        let result = fixture.build(false);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let code = String::from_utf8_lossy(&result.output_files[0].contents);
        assert!(code.contains("factory(\"div\""), "{code}");
        assert!(!code.contains("alternate("), "{code}");
    }
    let fixture = ConfigFixture::new("pkg", &[], &["node_modules/pkg/tsconfig.json"]);
    let result = fixture.build(false);
    assert!(result.errors.is_empty());
    assert_eq!(result.warnings.len(), 1);
    assert_eq!(
        result.warnings[0].text,
        "Cannot find base config file \"pkg\""
    );
}

#[test]
fn an_invalid_existing_config_stops_search_before_extension_fallback() {
    let fixture = ConfigFixture::new("./base", &[("src/base", "{"), ("src/base.json", BASE)], &[]);
    let result = fixture.build(false);
    assert_eq!(result.errors.len(), 1);
    assert!(result.warnings.is_empty());
    assert_eq!(
        result.errors[0]
            .location
            .as_ref()
            .expect("parse error location")
            .file,
        "src/base"
    );
    assert!(result.output_files.is_empty());
}
