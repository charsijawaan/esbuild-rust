use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::{
    api::{AbsPaths, BuildOptions, BuildSourceMap, BuildSourcesContent, LogLevel, build},
    internal::{
        js_parser,
        logger::{DeferLogKind, Log, Source},
    },
};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct MapFixture(PathBuf);

impl MapFixture {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-input-map-{}-{unique}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(directory.join("src")).expect("create source-map fixture");
        Self(std::fs::canonicalize(directory).expect("canonical source-map fixture"))
    }

    fn write(&self, path: &str, contents: &str) {
        std::fs::write(self.0.join(path), contents).expect("write source-map fixture");
    }

    fn options(&self, extension: &str) -> BuildOptions {
        BuildOptions {
            entry_points: vec![format!("src/entry.{extension}")],
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            outfile: format!("out.{extension}"),
            sourcemap: BuildSourceMap::External,
            ..BuildOptions::default()
        }
    }
}

impl Drop for MapFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn builds_compose_file_and_data_source_maps_and_fill_original_contents() {
    let fixture = MapFixture::new();
    for (extension, original_extension, source) in [
        ("js", "ts", "console.log('mapped');"),
        ("css", "css", "a { color: red }"),
    ] {
        let original = format!("src/original.{original_extension}");
        let original_contents = "original source π\n";
        fixture.write(&original, original_contents);
        let mapping = serde_json::json!({"version":3,"sources":[format!("original.{original_extension}")],"names":[],"mappings":"AAwCA"}).to_string();
        let map_name = format!("entry.{extension}.map");
        fixture.write(&format!("src/{map_name}"), &mapping);
        let file_url =
            url::Url::from_file_path(fixture.0.join(format!("src/{map_name}"))).expect("file URL");
        for comment in [
            map_name.clone(),
            file_url.to_string(),
            format!("{map_name}?query#fragment"),
            format!(
                "data:application/json;base64,{}",
                STANDARD.encode(mapping.as_bytes())
            ),
            format!(
                "data:application/json,{}",
                mapping
                    .replace('%', "%25")
                    .replace('{', "%7B")
                    .replace('}', "%7D")
                    .replace('"', "%22")
            ),
        ] {
            fixture.write(
                &format!("src/entry.{extension}"),
                &if extension == "js" {
                    format!("{source}\n//# sourceMappingURL={comment}")
                } else {
                    format!("{source}\n/*# sourceMappingURL={comment} */")
                },
            );
            for minify in [false, true] {
                for exclude_contents in [false, true] {
                    let mut options = fixture.options(extension);
                    options.minify_whitespace = minify;
                    options.minify_identifiers = minify;
                    options.minify_syntax = minify;
                    if exclude_contents {
                        options.sources_content = BuildSourcesContent::Exclude;
                    }
                    let result = build(options);
                    assert!(result.errors.is_empty(), "{:?}", result.errors);
                    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
                    let output = result
                        .output_files
                        .iter()
                        .find(|file| file.path.ends_with(".map"))
                        .expect("output source map");
                    let map: serde_json::Value =
                        serde_json::from_slice(&output.contents).expect("output map JSON");
                    assert_eq!(map["sources"], serde_json::json!([original]));
                    if exclude_contents {
                        assert!(map.get("sourcesContent").is_none());
                    } else {
                        assert_eq!(
                            map["sourcesContent"],
                            serde_json::json!([original_contents])
                        );
                    }
                    let parsed = js_parser::parse_source_map(
                        Log::new_defer(DeferLogKind::All, HashMap::new()),
                        Source {
                            contents: output.contents.clone().into(),
                            ..Source::default()
                        },
                    )
                    .expect("composed source map");
                    assert!(!parsed.mappings.is_empty());
                    assert!(
                        parsed
                            .mappings
                            .iter()
                            .all(|mapping| mapping.original_line == 40
                                && mapping.original_column == 0)
                    );
                }
            }
        }
    }
}

#[test]
fn source_map_read_errors_keep_comment_locations_and_overrides() {
    let fixture = MapFixture::new();
    fixture.write(
        "src/entry.js",
        "console.log(1);\n//# sourceMappingURL=entry.js.map",
    );
    std::fs::create_dir(fixture.0.join("src/entry.js.map")).expect("source-map directory");
    for absolute in [false, true] {
        let mut options = fixture.options("js");
        if absolute {
            options.abs_paths = AbsPaths::LOG;
        }
        let result = build(options);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), 1);
        let warning = &result.warnings[0];
        assert_eq!(warning.id, "missing-source-map");
        assert_eq!(
            warning.text,
            format!(
                "Cannot read file {:?}: is a directory",
                if absolute {
                    fixture
                        .0
                        .join("src/entry.js.map")
                        .to_string_lossy()
                        .into_owned()
                } else {
                    "src/entry.js.map".into()
                }
            )
        );
        let location = warning.location.as_ref().expect("comment location");
        assert_eq!(
            (location.line, location.column, location.length),
            (2, 21, 12)
        );
    }
    for level in [LogLevel::Silent, LogLevel::Error] {
        let mut options = fixture.options("js");
        options
            .log_override
            .insert("missing-source-map".into(), level);
        let result = build(options);
        assert!(result.warnings.is_empty());
        assert_eq!(result.errors.len(), usize::from(level == LogLevel::Error));
    }
    let mut options = fixture.options("js");
    options.sourcemap = BuildSourceMap::None;
    let result = build(options);
    assert!(result.errors.is_empty());
    assert!(result.warnings.is_empty());
}

#[test]
fn map_parse_errors_reference_the_originating_comment_and_remote_urls_are_ignored() {
    let fixture = MapFixture::new();
    fixture.write(
        "src/entry.js",
        "console.log(1);\n//# sourceMappingURL=entry.js.map",
    );
    fixture.write("src/entry.js.map", "{");
    let result = build(fixture.options("js"));
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].notes.len(), 1);
    assert_eq!(
        result.errors[0].notes[0].text,
        "The source map \"src/entry.js.map\" was referenced by the file \"src/entry.js\" here:"
    );
    assert_eq!(
        result.errors[0].notes[0]
            .location
            .as_ref()
            .expect("originating comment")
            .line,
        2
    );
    for comment in ["https://example.com/file.js.map", "missing.js.map"] {
        fixture.write(
            "src/entry.js",
            &format!("console.log(1);\n//# sourceMappingURL={comment}"),
        );
        let result = build(fixture.options("js"));
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }
}

#[test]
fn source_map_paths_escape_url_characters_and_empty_files_remain_strings() {
    let fixture = MapFixture::new();
    for (name, expected, contents) in [
        (
            "original #? π.ts",
            "original %23%3F %CF%80.ts",
            "original source",
        ),
        ("this:that.ts", "./this:that.ts", "original source"),
        ("empty.ts", "empty.ts", ""),
    ] {
        fixture.write(&format!("src/{name}"), contents);
        let original_url = url::Url::from_file_path(fixture.0.join(format!("src/{name}")))
            .expect("original file URL");
        let mapping = serde_json::json!({"version":3,"sources":[original_url.as_str()],"names":[],"mappings":"AAAA"});
        fixture.write("src/entry.js.map", &mapping.to_string());
        fixture.write(
            "src/entry.js",
            "console.log(1);\n//# sourceMappingURL=entry.js.map",
        );
        let mut options = fixture.options("js");
        options.outfile = "src/out.js".into();
        let result = build(options);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let output = result
            .output_files
            .iter()
            .find(|file| file.path.ends_with(".map"))
            .expect("output map");
        let map: serde_json::Value = serde_json::from_slice(&output.contents).expect("map JSON");
        assert_eq!(map["sources"], serde_json::json!([expected]));
        assert_eq!(map["sourcesContent"], serde_json::json!([contents]));
    }
}

#[test]
fn node_stack_traces_follow_composed_input_maps() {
    let fixture = MapFixture::new();
    fixture.write(
        "src/original.ts",
        &format!("{}throw new Error('mapped');", "\n".repeat(40)),
    );
    fixture.write(
        "src/entry.js",
        "throw new Error('mapped');\n//# sourceMappingURL=entry.js.map",
    );
    fixture.write(
        "src/entry.js.map",
        r#"{"version":3,"sources":["original.ts"],"names":[],"mappings":"AAwCA"}"#,
    );
    let mut options = fixture.options("js");
    options.sourcemap = BuildSourceMap::Linked;
    let result = build(options);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    for output in result.output_files {
        std::fs::write(output.path, output.contents).expect("write mapped output");
    }
    let output = Command::new("node")
        .arg("--enable-source-maps")
        .arg(fixture.0.join("out.js"))
        .output()
        .expect("execute mapped output with Node.js");
    assert_eq!(output.status.code(), Some(1));
    let stack = String::from_utf8_lossy(&output.stderr);
    assert!(stack.contains("Error: mapped"), "{stack}");
    assert!(stack.contains("src/original.ts:41:1"), "{stack}");
}
