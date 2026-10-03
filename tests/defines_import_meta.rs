use esbuild_rs::api::{
    BuildFormat, BuildTreeShaking, Target, TransformOptions, TransformResult, transform,
};

fn options(format: BuildFormat, define: &[(&str, &str)]) -> TransformOptions {
    TransformOptions {
        format,
        define: define
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
        ..TransformOptions::default()
    }
}

fn checked_transform(input: &str, options: TransformOptions) -> TransformResult {
    let result = transform(input, options);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    result
}

fn assert_code(input: &str, options: TransformOptions, expected: &str) {
    let result = checked_transform(input, options);
    assert_eq!(String::from_utf8(result.code).unwrap(), expected);
}

// Original sources, options, and code assertions from the pinned
// scripts/js-api-tests.js at 6ff1d8b0d8c134e867a397eef39702a223ebef9e.
#[test]
fn original_transform_tests_define_this() {
    assert_code(
        "console.log(a, b); export {}",
        options(BuildFormat::EsModule, &[("a", "this"), ("b", "this.foo")]),
        "console.log(void 0, (void 0).foo);\n",
    );
    assert_code(
        "console.log(this, this.x); export {}",
        options(BuildFormat::EsModule, &[("this", "a"), ("this.x", "b")]),
        "console.log(a, b);\n",
    );
}

#[test]
fn original_transform_tests_define_import_meta_esm() {
    assert_code(
        "console.log(a, b); export {}",
        options(
            BuildFormat::EsModule,
            &[("a", "import.meta"), ("b", "import.meta.foo")],
        ),
        "console.log(import.meta, import.meta.foo);\n",
    );
    assert_code(
        "console.log(import.meta, import.meta.x); export {}",
        options(
            BuildFormat::EsModule,
            &[("import.meta", "a"), ("import.meta.x", "b")],
        ),
        "console.log(a, b);\n",
    );
}

#[test]
fn original_transform_tests_define_import_meta_iife() {
    assert_code(
        "console.log(a, b); export {}",
        options(
            BuildFormat::Iife,
            &[("a", "import.meta"), ("b", "import.meta.foo")],
        ),
        "(() => {\n  const import_meta = {};\n  console.log(import_meta, import_meta.foo);\n})();\n",
    );
}

#[test]
fn original_transform_tests_define_quoted_property_name_transform() {
    assert_code(
        "return x.y['z!']",
        options(BuildFormat::Default, &[("x.y[\"z!\"]", "true")]),
        "return true;\n",
    );
    for key in ["x.y.z", "x[\"y\"].z", "x.y[\"z\"]", "x[\"y\"]['z']"] {
        assert_code(
            "foo(x['y'].z, x.y['z'], x['y']['z'])",
            options(BuildFormat::Default, &[(key, "true")]),
            "foo(true, true, true);\n",
        );
    }
    assert_code(
        "foo(import.meta['y'].z, import.meta.y['z'], import.meta['y']['z'])",
        options(BuildFormat::Default, &[("import.meta[\"y\"].z", "true")]),
        "foo(true, true, true);\n",
    );
    assert_code(
        "foo(import.meta['y!'].z, import.meta.y['z!'], import.meta['y!']['z!'])",
        options(
            BuildFormat::Default,
            &[
                ("import.meta[\"y!\"].z", "true"),
                ("import.meta.y[\"z!\"]", "true"),
                ("import.meta[\"y!\"][\"z!\"]", "true"),
            ],
        ),
        "foo(true, true, true);\n",
    );
}

#[test]
fn original_transform_tests_pure_import_meta() {
    for (pure, expected) in [
        (Vec::new(), "import.meta.foo(123, foo);\n"),
        (vec!["import.meta.foo".into()], "foo;\n"),
    ] {
        assert_code(
            "import.meta.foo(123, foo)",
            TransformOptions {
                minify_syntax: true,
                pure,
                ..TransformOptions::default()
            },
            expected,
        );
    }
}

#[test]
fn defines_match_complete_chains_before_shorter_defines_or_lowering() {
    assert_code(
        "foo(x.y, x[\"y\"], x.z)",
        options(BuildFormat::Default, &[("x", "a"), ("x.y", "b")]),
        "foo(b, b, a.z);\n",
    );
    for format in [
        BuildFormat::EsModule,
        BuildFormat::Iife,
        BuildFormat::CommonJs,
    ] {
        let result = checked_transform(
            "console.log(import.meta, import.meta.x)",
            options(format, &[("import.meta", "a"), ("import.meta.x", "b")]),
        );
        let expected = if format == BuildFormat::Iife {
            "(() => {\n  console.log(a, b);\n})();\n"
        } else {
            "console.log(a, b);\n"
        };
        assert_eq!(result.code, expected.as_bytes());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }
}

#[test]
fn this_defines_follow_arrow_scope_and_preserve_function_and_class_receivers() {
    assert_code(
        "console.log(this, this.x, (() => this.x)()); function f() { return [this, this.x, (() => this.x)()] } export {}",
        options(BuildFormat::EsModule, &[("this", "a"), ("this.x", "b")]),
        "console.log(a, b, /* @__PURE__ */ (() => b)());\nfunction f() {\n  return [this, this.x, (() => this.x)()];\n}\n",
    );
    assert_code(
        "console.log(this.x); class C { field = this.x; static field = this.x; method() { return this.x } } export { C }",
        options(BuildFormat::EsModule, &[("this.x", "b")]),
        "console.log(b);\nclass C {\n  field = this.x;\n  static field = this.x;\n  method() {\n    return this.x;\n  }\n}\nexport {\n  C\n};\n",
    );
}

#[test]
fn define_matching_reserves_unbound_names_and_preserves_shadowing_and_with() {
    assert_code(
        "foo(x.y); function f(x) { return x[\"y\"] }",
        options(BuildFormat::Default, &[("x.y", "true")]),
        "foo(true);\nfunction f(x2) {\n  return x2[\"y\"];\n}\n",
    );
    assert_code(
        "with (scope) { foo(x.y) }",
        options(BuildFormat::Default, &[("x.y", "true")]),
        "with (scope) {\n  foo(x.y);\n}\n",
    );
}

#[test]
fn source_import_meta_warns_for_formats_but_defined_or_generated_values_do_not() {
    for (format, name, expected) in [
        (
            BuildFormat::CommonJs,
            "cjs",
            "const import_meta = {};\nconsole.log(import_meta, import_meta.foo);\n",
        ),
        (
            BuildFormat::Iife,
            "iife",
            "(() => {\n  const import_meta = {};\n  console.log(import_meta, import_meta.foo);\n})();\n",
        ),
    ] {
        let result = checked_transform(
            "console.log(import.meta, import.meta.foo)",
            options(format, &[]),
        );
        assert_eq!(result.code, expected.as_bytes());
        assert_eq!(result.warnings.len(), 2);
        for (warning, column) in result.warnings.iter().zip([12, 25]) {
            assert_eq!(warning.id, "empty-import-meta");
            assert_eq!(
                warning.text,
                format!(
                    "\"import.meta\" is not available with the \"{name}\" output format and will be empty"
                )
            );
            let location = warning.location.as_ref().unwrap();
            assert_eq!(location.column, column);
            assert_eq!(location.length, 11);
            assert_eq!(warning.notes.len(), 1);
            assert_eq!(
                warning.notes[0].text,
                "You need to set the output format to \"esm\" for \"import.meta\" to work correctly."
            );
        }
    }
    let result = checked_transform(
        "console.log(a, b); export {}",
        options(
            BuildFormat::Iife,
            &[("a", "import.meta"), ("b", "import.meta.foo")],
        ),
    );
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    let result = checked_transform(
        "try { console.log(import.meta) } catch {}",
        options(BuildFormat::Iife, &[]),
    );
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
}

#[test]
fn generated_import_meta_obeys_target_and_tree_shaking() {
    for input in ["a; export {}", "b; export {}"] {
        for (tree_shaking, expected_expression) in [
            (BuildTreeShaking::Enabled, None),
            (
                BuildTreeShaking::Disabled,
                Some(if input.starts_with('a') {
                    "import_meta"
                } else {
                    "import_meta.foo"
                }),
            ),
        ] {
            let mut options = options(
                BuildFormat::Iife,
                &[("a", "import.meta"), ("b", "import.meta.foo")],
            );
            options.tree_shaking = tree_shaking;
            let result = checked_transform(input, options);
            let expected = expected_expression.map_or_else(
                || "(() => {\n})();\n".into(),
                |expression| {
                    format!("(() => {{\n  const import_meta = {{}};\n  {expression};\n}})();\n")
                },
            );
            assert_eq!(result.code, expected.as_bytes());
            assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        }
    }
    let mut target_options = options(
        BuildFormat::Iife,
        &[("a", "import.meta"), ("b", "import.meta.foo")],
    );
    target_options
        .supported
        .insert("const-and-let".into(), false);
    assert_code(
        "console.log(a, b); export {}",
        target_options,
        "(() => {\n  var import_meta = {};\n  console.log(import_meta, import_meta.foo);\n})();\n",
    );
    let result = checked_transform(
        "console.log(a)",
        TransformOptions {
            target: Target::Es2015,
            ..options(BuildFormat::Default, &[("a", "import.meta")])
        },
    );
    assert_eq!(
        result.code,
        b"const import_meta = {};\nconsole.log(import_meta);\n"
    );
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
}

#[test]
fn pure_import_meta_calls_keep_argument_side_effects_and_format_warnings() {
    for (format, expected, warning_count) in [
        (BuildFormat::Default, "sideEffect(), foo;\n", 0),
        (BuildFormat::EsModule, "sideEffect(), foo;\n", 0),
        (
            BuildFormat::CommonJs,
            "const import_meta = {};\nsideEffect(), foo;\n",
            1,
        ),
        (
            BuildFormat::Iife,
            "(() => {\n  const import_meta = {};\n  sideEffect(), foo;\n})();\n",
            1,
        ),
    ] {
        for input in [
            "import.meta.foo(sideEffect(), foo)",
            "import.meta['foo'](sideEffect(), foo)",
        ] {
            let result = checked_transform(
                input,
                TransformOptions {
                    format,
                    minify_syntax: true,
                    pure: vec!["import.meta.foo".into()],
                    ..TransformOptions::default()
                },
            );
            assert_eq!(result.code, expected.as_bytes());
            assert_eq!(result.warnings.len(), warning_count);
        }
    }
}
