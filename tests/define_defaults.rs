use std::{collections::HashMap, process::Command};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, TransformOptions, build, transform,
};

fn transformed(source: &str, options: TransformOptions) -> String {
    let result = transform(source, options);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    String::from_utf8(result.code).unwrap()
}

fn built(source: &str, options: BuildOptions) -> String {
    let result = build(BuildOptions {
        stdin: Some(BuildStdin {
            contents: source.into(),
            ..BuildStdin::default()
        }),
        write: false,
        ..options
    });
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.output_files.len(), 1);
    String::from_utf8(result.output_files[0].contents.clone()).unwrap()
}

fn defines(entries: &[(&str, &str)]) -> HashMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

// Existing gain from the excluded define/import.meta prerequisite. This is the
// unchanged transformTests/defineBuiltInConstants source/options/assertion at
// pinned Go revision 6ff1d8b0d8c134e867a397eef39702a223ebef9e, line 6671.
#[test]
fn original_define_built_in_constants_is_a_dependency_control() {
    assert_eq!(
        transformed(
            "console.log([typeof a, typeof b, typeof c, typeof d, typeof e])",
            TransformOptions {
                define: defines(&[
                    ("a", "NaN"),
                    ("b", "Infinity"),
                    ("c", "undefined"),
                    ("d", "something"),
                    ("e", "null")
                ]),
                ..TransformOptions::default()
            },
        ),
        "console.log([\"number\", \"number\", \"undefined\", typeof something, \"object\"]);\n",
    );
}

// All six unchanged transformTests/defineProcessEnvNodeEnv assertions, line 6649.
#[test]
fn original_define_process_env_node_env() {
    for input in [
        "console.log(process.env.NODE_ENV)",
        "console.log(process.env['NODE_ENV'])",
        "console.log(process['env'].NODE_ENV)",
        "console.log(process['env']['NODE_ENV'])",
    ] {
        assert_eq!(
            transformed(
                input,
                TransformOptions {
                    define: defines(&[("process.env.NODE_ENV", "\"something\"")]),
                    ..TransformOptions::default()
                }
            ),
            "console.log(\"something\");\n",
        );
    }
    for platform in [BuildPlatform::Default, BuildPlatform::Browser] {
        assert_eq!(
            transformed(
                "console.log(process.env.NODE_ENV)",
                TransformOptions {
                    platform,
                    ..TransformOptions::default()
                }
            ),
            "console.log(process.env.NODE_ENV);\n",
        );
    }
}

#[test]
fn node_env_defaults_distinguish_api_platform_and_all_minify_flag_combinations() {
    let source = "console.log(process.env.NODE_ENV)";
    for platform in [
        BuildPlatform::Default,
        BuildPlatform::Browser,
        BuildPlatform::Node,
        BuildPlatform::Neutral,
    ] {
        for mask in 0..8 {
            let minify_whitespace = mask & 1 != 0;
            let minify_identifiers = mask & 2 != 0;
            let minify_syntax = mask & 4 != 0;
            assert_eq!(
                transformed(
                    source,
                    TransformOptions {
                        platform,
                        minify_whitespace,
                        minify_identifiers,
                        minify_syntax,
                        ..TransformOptions::default()
                    }
                ),
                "console.log(process.env.NODE_ENV);\n",
            );
            let expected = if matches!(platform, BuildPlatform::Default | BuildPlatform::Browser) {
                if mask == 7 {
                    "console.log(\"production\");\n"
                } else {
                    "console.log(\"development\");\n"
                }
            } else {
                "console.log(process.env.NODE_ENV);\n"
            };
            assert_eq!(
                built(
                    source,
                    BuildOptions {
                        platform,
                        minify_whitespace,
                        minify_identifiers,
                        minify_syntax,
                        ..BuildOptions::default()
                    }
                ),
                expected,
            );
        }
    }
}

#[test]
fn pure_call_metadata_does_not_disable_browser_build_defaults() {
    for pure in ["process", "process.env", "process.env.NODE_ENV"] {
        for minify in [false, true] {
            assert_eq!(
                built(
                    "console.log(process.env.NODE_ENV)",
                    BuildOptions {
                        pure: vec![pure.into()],
                        minify_whitespace: minify,
                        minify_identifiers: minify,
                        minify_syntax: minify,
                        ..BuildOptions::default()
                    }
                ),
                if minify {
                    "console.log(\"production\");\n"
                } else {
                    "console.log(\"development\");\n"
                },
            );
            assert_eq!(
                transformed(
                    "console.log(process.env.NODE_ENV)",
                    TransformOptions {
                        pure: vec![pure.into()],
                        ..TransformOptions::default()
                    }
                ),
                "console.log(process.env.NODE_ENV);\n",
            );
        }
    }
}

#[test]
fn node_env_defaults_apply_to_bundled_and_unbundled_browser_builds() {
    assert_eq!(
        built(
            "console.log(process.env.NODE_ENV)",
            BuildOptions {
                bundle: true,
                ..BuildOptions::default()
            }
        ),
        "(() => {\n  // <stdin>\n  console.log(\"development\");\n})();\n",
    );
    assert_eq!(
        built(
            "console.log(process.env.NODE_ENV)",
            BuildOptions {
                bundle: true,
                minify_whitespace: true,
                minify_identifiers: true,
                minify_syntax: true,
                ..BuildOptions::default()
            }
        ),
        "(()=>{console.log(\"production\");})();\n",
    );
}

#[test]
fn user_define_precedence_matches_pinned_go_exact_key_rules() {
    for (key, value, transform_code, build_code) in [
        (
            "process",
            "custom",
            "console.log(custom.env.NODE_ENV);\n",
            "console.log(custom.env.NODE_ENV);\n",
        ),
        (
            "process.env",
            "custom",
            "console.log(custom.NODE_ENV);\n",
            "console.log(\"development\");\n",
        ),
        (
            "process[\"env\"]",
            "custom",
            "console.log(custom.NODE_ENV);\n",
            "console.log(\"development\");\n",
        ),
        (
            "process.env.NODE_ENV",
            "\"override\"",
            "console.log(\"override\");\n",
            "console.log(\"override\");\n",
        ),
        (
            "process[\"env\"][\"NODE_ENV\"]",
            "\"override\"",
            "console.log(\"override\");\n",
            "console.log(\"override\");\n",
        ),
        (
            "process.env.OTHER",
            "true",
            "console.log(process.env.NODE_ENV);\n",
            "console.log(\"development\");\n",
        ),
    ] {
        assert_eq!(
            transformed(
                "console.log(process.env.NODE_ENV)",
                TransformOptions {
                    define: defines(&[(key, value)]),
                    ..TransformOptions::default()
                }
            ),
            transform_code
        );
        assert_eq!(
            built(
                "console.log(process.env.NODE_ENV)",
                BuildOptions {
                    define: defines(&[(key, value)]),
                    ..BuildOptions::default()
                }
            ),
            build_code
        );
    }
}

#[test]
fn default_globals_preserve_shadowing_user_overrides_and_assignment_code() {
    for (source, define, expected) in [
        (
            "console.log(typeof undefined, typeof Infinity, typeof NaN, undefined, Infinity, NaN)",
            HashMap::new(),
            "console.log(\"undefined\", \"number\", \"number\", void 0, Infinity, NaN);\n",
        ),
        (
            "function f(undefined, Infinity, NaN) { return [undefined, Infinity, NaN] } console.log(f(1,2,3))",
            HashMap::new(),
            "function f(undefined, Infinity, NaN) {\n  return [undefined, Infinity, NaN];\n}\nconsole.log(f(1, 2, 3));\n",
        ),
        (
            "console.log(undefined, Infinity, NaN)",
            defines(&[("undefined", "1"), ("Infinity", "2"), ("NaN", "3")]),
            "console.log(1, 2, 3);\n",
        ),
        (
            "undefined = 1; Infinity = 2; NaN = 3; console.log(undefined, Infinity, NaN)",
            HashMap::new(),
            "undefined = 1;\nInfinity = 2;\nNaN = 3;\nconsole.log(void 0, Infinity, NaN);\n",
        ),
    ] {
        assert_eq!(
            transformed(
                source,
                TransformOptions {
                    define: define.clone(),
                    ..TransformOptions::default()
                }
            ),
            expected
        );
        assert_eq!(
            built(
                source,
                BuildOptions {
                    define,
                    ..BuildOptions::default()
                }
            ),
            expected
        );
    }
}

#[test]
fn process_scope_and_assignment_code_and_import_meta_dependency_remain_intact() {
    for (source, expected) in [
        (
            "function f(process) { return process.env.NODE_ENV } console.log(f({env:{NODE_ENV:\"local\"}}))",
            "function f(process) {\n  return process.env.NODE_ENV;\n}\nconsole.log(f({ env: { NODE_ENV: \"local\" } }));\n",
        ),
        (
            "with (scope) { console.log(process.env.NODE_ENV) }",
            "with (scope) {\n  console.log(process.env.NODE_ENV);\n}\n",
        ),
    ] {
        assert_eq!(transformed(source, TransformOptions::default()), expected);
        assert_eq!(built(source, BuildOptions::default()), expected);
    }
    assert_eq!(
        transformed(
            "process.env.NODE_ENV = \"changed\"; console.log(process.env.NODE_ENV)",
            TransformOptions::default()
        ),
        "process.env.NODE_ENV = \"changed\";\nconsole.log(process.env.NODE_ENV);\n"
    );
    assert_eq!(
        built(
            "process.env.NODE_ENV = \"changed\"; console.log(process.env.NODE_ENV)",
            BuildOptions::default()
        ),
        "process.env.NODE_ENV = \"changed\";\nconsole.log(\"development\");\n"
    );
    assert_eq!(
        transformed(
            "console.log(a, import.meta.x); export {}",
            TransformOptions {
                format: BuildFormat::EsModule,
                define: defines(&[("a", "import.meta"), ("import.meta.x", "true")]),
                ..TransformOptions::default()
            }
        ),
        "console.log(import.meta, true);\n"
    );
}

#[test]
fn constants_and_node_env_transforms_keep_runtime_values() {
    let source = r"
const assert = require('node:assert/strict');
assert.equal(a, undefined);
assert.equal(b, Infinity);
assert.ok(Number.isNaN(c));
assert.equal(process.env.NODE_ENV, 'live');
assert.equal((function(undefined, Infinity, NaN) { return undefined + Infinity + NaN })(1, 2, 3), 6);
console.log('ok');
";
    let code = transformed(
        source,
        TransformOptions {
            define: defines(&[("a", "undefined"), ("b", "Infinity"), ("c", "NaN")]),
            ..TransformOptions::default()
        },
    );
    let result = Command::new("node")
        .env("NODE_ENV", "live")
        .args(["-e", &code])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{code}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"ok\n");
}
