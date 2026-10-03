//! Independent exact outputs from pinned Go esbuild 0.28.1 (Go 1.26.5).
//! The generator and unchanged-wrapper process reports accompany the proposal.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{
    self, BuildFormat, BuildOptions, BuildStdin, Loader, Target, TransformOptions,
};
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("fixtures/dataurl_go_vectors.json")).unwrap()
}

fn check_transforms(group: &str) {
    let vectors = vectors();
    let mut count = 0;
    for case in vectors["transform"].as_array().unwrap() {
        if case["group"] != group {
            continue;
        }
        count += 1;
        let settings = &case["options"];
        let minify = settings["minify"].as_bool().unwrap_or(false);
        let result = api::transform(
            STANDARD
                .decode(case["input_base64"].as_str().unwrap())
                .unwrap(),
            TransformOptions {
                loader: Loader::DataUrl,
                sourcefile: settings["sourcefile"].as_str().unwrap_or("").into(),
                format: match settings["format"].as_str() {
                    Some("cjs") => BuildFormat::CommonJs,
                    Some("esm") => BuildFormat::EsModule,
                    Some("iife") => BuildFormat::Iife,
                    None => BuildFormat::Default,
                    value => panic!("unhandled format {value:?}"),
                },
                global_name: settings["globalName"].as_str().unwrap_or("").into(),
                target: if settings["target"] == "es5" {
                    Target::Es5
                } else {
                    Target::Default
                },
                ascii_only: settings["charset"] != "utf8",
                minify_whitespace: minify
                    || settings["minifyWhitespace"].as_bool().unwrap_or(false),
                minify_syntax: minify,
                minify_identifiers: minify,
                banner: settings["banner"].as_str().unwrap_or("").into(),
                footer: settings["footer"].as_str().unwrap_or("").into(),
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
        assert!(result.map.is_empty(), "{}", case["name"]);
    }
    assert!(count > 0);
}

#[test]
fn original_dataurl_and_invalid_utf8_keep_exact_bytes() {
    check_transforms("raw");
}

#[test]
fn go_sniff_signatures_controls_and_512_byte_window() {
    check_transforms("sniff");
}

#[test]
fn extension_metadata_overrides_sniffing_including_dotfiles() {
    check_transforms("extension");
}

#[test]
fn shortest_encoding_ties_and_output_options_match_go() {
    check_transforms("encoding");
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
            "esbuild-dataurl-mime-{}-{stamp}-{}",
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
fn css_urls_share_mime_sniffing_for_dataurl_and_base64() {
    let fixture = Fixture::new();
    for case in vectors()["css"].as_array().unwrap() {
        let file = case["file"].as_str().unwrap();
        let entry = case["entry"].as_str().unwrap();
        std::fs::write(
            fixture.0.join(file),
            STANDARD
                .decode(case["input_base64"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            fixture.0.join(entry),
            format!("a{{background:url(./{file})}}"),
        )
        .unwrap();
        let result = api::build(BuildOptions {
            bundle: true,
            entry_points: vec![entry.into()],
            abs_working_dir: fixture.0.to_string_lossy().into_owned(),
            loader: HashMap::from([(
                ".unknown".into(),
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

#[test]
fn build_stdin_dataurl_uses_the_same_detector() {
    let fixture = Fixture::new();
    for case in vectors()["stdin"].as_array().unwrap() {
        let result = api::build(BuildOptions {
            abs_working_dir: fixture.0.to_string_lossy().into_owned(),
            format: BuildFormat::EsModule,
            stdin: Some(BuildStdin {
                contents: case["contents"].as_str().unwrap().into(),
                sourcefile: case["sourcefile"].as_str().unwrap().into(),
                loader: Loader::DataUrl,
                ..BuildStdin::default()
            }),
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
    }
}
