//! Go goldens for MIME sourcefile splitting, independent of the host OS.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::api::{self, BuildFormat, Loader, TransformOptions};
use serde_json::Value;

fn check(linker: bool) {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/dataurl_sourcefile_go_vectors.json")).unwrap();
    let mut count = 0;
    for case in fixture["vectors"].as_array().unwrap() {
        let options = &case["options"];
        if options["format"].is_string() != linker {
            continue;
        }
        count += 1;
        let result = api::transform(
            STANDARD
                .decode(case["input_base64"].as_str().unwrap())
                .unwrap(),
            TransformOptions {
                sourcefile: options["sourcefile"].as_str().unwrap().into(),
                loader: Loader::DataUrl,
                format: match options["format"].as_str() {
                    None => BuildFormat::Default,
                    Some("cjs") => BuildFormat::CommonJs,
                    Some("esm") => BuildFormat::EsModule,
                    Some("iife") => BuildFormat::Iife,
                    other => panic!("unhandled format {other:?}"),
                },
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
            "{}: {}",
            case["name"],
            options["sourcefile"]
        );
        assert!(result.map.is_empty());
    }
    assert_eq!(count, if linker { 22 } else { 53 });
}

#[test]
fn direct_dataurl_sourcefile_matches_go_for_slashes_dot_components_and_module_css() {
    check(false);
}

#[test]
fn explicit_formats_keep_existing_sourcefile_semantics() {
    check(true);
}
