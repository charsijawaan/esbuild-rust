//! Independent pinned Go MIME lookup and raw-byte API outputs.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::{
    api::{self, BuildFormat, BuildOptions, BuildStdin, Loader, TransformOptions},
    internal::helpers::mime_type_by_extension,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

#[test]
fn extension_lookup_matches_go_simple_unicode_lowercase() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/dataurl_extension_go_mime.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        assert_eq!(
            mime_type_by_extension(case["extension"].as_str().unwrap()),
            case["mime"].as_str().unwrap(),
            "{} (Go lower {})",
            case["extension"],
            case["lower"]
        );
    }
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 79);
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/dataurl_extension_go_api.json")).unwrap()
}

#[test]
fn dataurl_extension_lookup_preserves_exact_go_byte_exports() {
    for case in fixture()["transforms"].as_array().unwrap() {
        let settings = &case["options"];
        let minify = settings["minify"].as_bool().unwrap_or(false);
        let result = api::transform(
            STANDARD
                .decode(case["input_base64"].as_str().unwrap())
                .unwrap(),
            TransformOptions {
                loader: Loader::DataUrl,
                sourcefile: settings["sourcefile"].as_str().unwrap().into(),
                format: match settings["format"].as_str() {
                    None => BuildFormat::Default,
                    Some("esm") => BuildFormat::EsModule,
                    Some("cjs") => BuildFormat::CommonJs,
                    Some("iife") => BuildFormat::Iife,
                    other => panic!("unexpected format {other:?}"),
                },
                ascii_only: settings["charset"] != "utf8",
                minify_syntax: minify,
                minify_identifiers: minify,
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
        );
        assert!(
            result.errors.is_empty() && result.warnings.is_empty(),
            "{}",
            case["name"]
        );
        assert_eq!(
            result.code,
            case["code"].as_str().unwrap().as_bytes(),
            "{}",
            case["name"]
        );
        assert!(result.map.is_empty());
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "esbuild-dataurl-extension-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn css_dataurl_and_base64_use_the_same_unicode_lookup() {
    let dir = Fixture::new();
    for case in fixture()["css"].as_array().unwrap() {
        let file = case["file"].as_str().unwrap();
        std::fs::write(
            dir.0.join(file),
            STANDARD
                .decode(case["input_base64"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        let result = api::build(BuildOptions {
            bundle: true,
            abs_working_dir: dir.0.to_string_lossy().into_owned(),
            stdin: Some(BuildStdin {
                contents: format!("a{{background:url(./{file})}}"),
                resolve_dir: dir.0.to_string_lossy().into_owned(),
                loader: Loader::Css,
                ..BuildStdin::default()
            }),
            loader: HashMap::from([(
                file[file.rfind('.').unwrap()..].into(),
                if case["loader"] == "dataurl" {
                    Loader::DataUrl
                } else {
                    Loader::Base64
                },
            )]),
            ..BuildOptions::default()
        });
        assert!(
            result.errors.is_empty() && result.warnings.is_empty(),
            "{}",
            case["name"]
        );
        assert_eq!(result.output_files.len(), 1);
        assert_eq!(
            result.output_files[0].contents,
            case["code"].as_str().unwrap().as_bytes(),
            "{}",
            case["name"]
        );
        assert_eq!(
            result.output_files[0].hash,
            case["hash"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
}
