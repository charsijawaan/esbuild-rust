use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Engine, EngineName, build,
};

fn source_body(source: &str, format: BuildFormat, defines: &[(&str, &str)]) -> String {
    let result = build(BuildOptions {
        bundle: true,
        platform: BuildPlatform::Node,
        format,
        engines: vec![Engine {
            name: EngineName::Node,
            version: "14.17".into(),
        }],
        define: defines
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
        stdin: Some(BuildStdin {
            contents: source.into(),
            sourcefile: "review.js".into(),
            ..BuildStdin::default()
        }),
        write: false,
        ..BuildOptions::default()
    });
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    let output = String::from_utf8(result.output_files[0].contents.clone()).unwrap();
    let body = &output[output.find("// review.js\n").expect("source label")..];
    // Compare only the entry-point body. Runtime helper brace formatting has
    // an independently recorded baseline mismatch with pinned Go.
    let body = body.strip_suffix("})();\n").unwrap_or(body);
    body.lines()
        .skip(1)
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn partial_define_bases_are_visited_as_values() {
    for format in [BuildFormat::EsModule, BuildFormat::Iife] {
        for (source, expected) in [
            ("REQUIRE.resolve('node:fs')", "__require.resolve(\"node:fs\");"),
            (
                "REQUIRE['resolve']('node:fs')",
                "__require[\"resolve\"](\"node:fs\");",
            ),
        ] {
            assert_eq!(
                source_body(source, format, &[("REQUIRE", "require")]),
                expected
            );
        }
    }
}

#[test]
fn generated_require_values_and_comma_call_targets_use_runtime_require() {
    for format in [BuildFormat::EsModule, BuildFormat::Iife] {
        for (source, expected) in [
            (
                "var resolve = RESOLVE; resolve('node:fs')",
                "var resolve = __require.resolve;\nresolve(\"node:fs\");",
            ),
            (
                "(0, RESOLVE)('node:fs')",
                "(0, __require.resolve)(\"node:fs\");",
            ),
        ] {
            assert_eq!(
                source_body(source, format, &[("RESOLVE", "require.resolve")]),
                expected
            );
        }
    }
}

#[test]
fn special_define_roots_forward_the_call_target_context() {
    for format in [BuildFormat::EsModule, BuildFormat::Iife] {
        for (source, key) in [
            ("this.resolve('node:fs')", "this"),
            ("import.meta.resolve('node:fs')", "import.meta"),
        ] {
            assert_eq!(
                source_body(source, format, &[(key, "require")]),
                "__require.resolve(\"node:fs\");"
            );
        }
    }
}

#[test]
fn complete_define_call_targets_reach_require_resolve_record_recognition() {
    for format in [BuildFormat::EsModule, BuildFormat::Iife] {
        for (source, key) in [
            ("RESOLVE('node:fs')", "RESOLVE"),
            ("X.y('node:fs')", "X.y"),
            ("X['y']('node:fs')", "X.y"),
            ("import.meta('node:fs')", "import.meta"),
        ] {
            assert_eq!(
                source_body(source, format, &[(key, "require.resolve")]),
                "require.resolve(\"fs\");"
            );
        }
    }
}

#[test]
fn source_and_generated_this_calls_preserve_the_pinned_go_flag_order() {
    for format in [BuildFormat::EsModule, BuildFormat::Iife] {
        assert_eq!(
            source_body("this('node:fs')", format, &[("this", "require.resolve")]),
            "(0, __require.resolve)(\"node:fs\");"
        );
        assert_eq!(
            source_body(
                "SELF('node:fs')",
                format,
                &[("SELF", "this"), ("this", "require.resolve")],
            ),
            "(0, __require.resolve)(\"node:fs\");"
        );
    }
}
