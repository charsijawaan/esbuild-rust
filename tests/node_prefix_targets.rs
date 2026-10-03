use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Engine, EngineName, Target, build,
};

const IMPORT_SOURCE: &str = "import fs from 'node:fs'; import('node:fs'); fs()";
const REQUIRE_SOURCE: &str = "require('node:fs'); require.resolve('node:fs')";

fn options_for(version: &str, format: BuildFormat) -> BuildOptions {
    BuildOptions {
        bundle: true,
        platform: BuildPlatform::Node,
        format,
        engines: vec![Engine {
            name: EngineName::Node,
            version: version.into(),
        }],
        write: false,
        ..BuildOptions::default()
    }
}

fn reference_output(source: &str, options: &BuildOptions) -> Option<String> {
    let executable = std::env::var_os("ESBUILD_RS_TEAM_NODE_PREFIX_GO")?;
    let mut command = Command::new(executable);
    if options.bundle {
        command.arg("--bundle");
    }
    command.arg(format!(
        "--platform={}",
        match options.platform {
            BuildPlatform::Node => "node",
            BuildPlatform::Default | BuildPlatform::Browser => "browser",
            BuildPlatform::Neutral => "neutral",
        }
    ));
    command.arg(format!(
        "--format={}",
        match options.format {
            BuildFormat::EsModule => "esm",
            BuildFormat::CommonJs => "cjs",
            BuildFormat::Iife => "iife",
            BuildFormat::Default => panic!("reference fixtures use an explicit format"),
        }
    ));
    let mut targets: Vec<_> = options
        .engines
        .iter()
        .map(|engine| {
            assert_eq!(engine.name, EngineName::Node);
            format!("node{}", engine.version)
        })
        .collect();
    match options.target {
        Target::Default => {}
        Target::Es2015 => targets.push("es6".into()),
        _ => panic!("unexpected reference target"),
    }
    command.arg(format!("--target={}", targets.join(",")));
    for (feature, supported) in &options.supported {
        command.arg(format!("--supported:{feature}={supported}"));
    }
    for external in &options.external {
        command.arg(format!("--external:{external}"));
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run pinned Go compiler");
    child
        .stdin
        .take()
        .expect("Go stdin")
        .write_all(source.as_bytes())
        .expect("write Go stdin");
    let output = child.wait_with_output().expect("read Go result");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).expect("Go JavaScript"))
}

fn output_for(source: &str, mut options: BuildOptions) -> String {
    options.stdin = Some(BuildStdin {
        contents: source.into(),
        ..BuildStdin::default()
    });
    let result = build(options.clone());
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(result.output_files.len(), 1);
    let output =
        String::from_utf8(result.output_files[0].contents.clone()).expect("Rust JavaScript");
    if let Some(reference) = reference_output(source, &options) {
        // The original nodeColonPrefix tests compare from the source label.
        // Keep this same scope for the opt-in differential assertion.
        if options.bundle {
            assert_eq!(
                source_body(&output),
                source_body(&reference),
                "source={source}, options={options:?}"
            );
        } else {
            assert_eq!(output, reference, "source={source}, options={options:?}");
        }
    }
    output
}

fn source_body(output: &str) -> &str {
    &output[output.find("// <stdin>\n").expect("original source label")..]
}

#[test]
fn node_colon_prefix_import_support_ranges_and_lowered_dynamic_imports() {
    for (version, path, lower_dynamic) in [
        ("14.13.1", "node:fs", false),
        ("14.13.0", "fs", false),
        ("13", "fs", true),
        ("12.99", "node:fs", false),
        ("12.20", "node:fs", false),
        ("12.19", "fs", true),
        ("18", "node:fs", false),
    ] {
        let mut options = options_for(version, BuildFormat::EsModule);
        if version == "18" {
            options.target = Target::Es2015;
        }
        let output = output_for(IMPORT_SOURCE, options);
        let dynamic = if lower_dynamic {
            format!("Promise.resolve().then(() => __toESM(__require(\"{path}\")));\n")
        } else {
            format!("import(\"{path}\");\n")
        };
        assert_eq!(
            source_body(&output),
            format!("// <stdin>\nimport fs from \"{path}\";\n{dynamic}fs();\n"),
            "node{version}"
        );
    }
}

#[test]
fn node_colon_prefix_require_and_require_resolve_support_ranges() {
    for (version, path) in [
        ("16", "node:fs"),
        ("15.99", "fs"),
        ("15", "fs"),
        ("14.99", "node:fs"),
        ("14.18", "node:fs"),
        ("14.17", "fs"),
        ("18", "node:fs"),
    ] {
        let mut options = options_for(version, BuildFormat::CommonJs);
        if version == "18" {
            options.target = Target::Es2015;
        }
        let output = output_for(REQUIRE_SOURCE, options);
        assert_eq!(
            source_body(&output),
            format!("// <stdin>\nrequire(\"{path}\");\nrequire.resolve(\"{path}\");\n"),
            "node{version}"
        );
    }
}

#[test]
fn node_colon_prefix_imports_in_commonjs_use_require_support_and_original_names() {
    for (version, path) in [
        ("16", "node:fs"),
        ("15.99", "fs"),
        ("15", "fs"),
        ("14.99", "node:fs"),
        ("14.18", "node:fs"),
        ("14.17", "fs"),
        ("18", "node:fs"),
    ] {
        let mut options = options_for(version, BuildFormat::CommonJs);
        if version == "18" {
            options.target = Target::Es2015;
        }
        let output = output_for(IMPORT_SOURCE, options);
        assert_eq!(
            source_body(&output),
            format!(
                "// <stdin>\nvar import_node_fs = __toESM(require(\"{path}\"));\nimport(\"{path}\");\n(0, import_node_fs.default)();\n"
            ),
            "node{version}"
        );
    }
}

#[test]
fn node_colon_prefix_feature_overrides_follow_output_format() {
    for format in [BuildFormat::EsModule, BuildFormat::CommonJs] {
        for import_supported in [false, true] {
            for require_supported in [false, true] {
                let mut options = options_for("18", format);
                options.supported = HashMap::from([
                    ("node-colon-prefix-import".into(), import_supported),
                    ("node-colon-prefix-require".into(), require_supported),
                ]);
                let output = output_for(IMPORT_SOURCE, options.clone());
                let path = if match format {
                    BuildFormat::EsModule => import_supported,
                    _ => require_supported,
                } {
                    "node:fs"
                } else {
                    "fs"
                };
                assert!(source_body(&output).contains(&format!("import(\"{path}\")")));
                let output = output_for(REQUIRE_SOURCE, options);
                let path = if require_supported { "node:fs" } else { "fs" };
                assert!(source_body(&output).contains(&format!("require(\"{path}\")")));
                if format == BuildFormat::CommonJs {
                    assert!(output.contains(&format!("require.resolve(\"{path}\")")));
                } else {
                    // ESM require.resolve is a property access on __require,
                    // so the pinned parser leaves its argument untouched.
                    assert!(output.contains("__require.resolve(\"node:fs\")"));
                }
            }
        }
    }
}

#[test]
fn lowered_dynamic_import_keeps_the_resolver_import_guard_in_esm() {
    for format in [BuildFormat::EsModule, BuildFormat::CommonJs] {
        let mut options = options_for("14.13.1", format);
        options.supported = HashMap::from([
            ("dynamic-import".into(), false),
            ("node-colon-prefix-require".into(), false),
        ]);
        let output = output_for(IMPORT_SOURCE, options);
        let dynamic = if format == BuildFormat::EsModule {
            "Promise.resolve().then(() => __toESM(__require(\"node:fs\")));"
        } else {
            "Promise.resolve().then(() => __toESM(require(\"fs\")));"
        };
        assert!(source_body(&output).contains(dynamic), "{output}");
    }
}

#[test]
fn node_prefix_stripping_preserves_default_named_and_namespace_interop() {
    let source = "import fs, { readFileSync as read } from 'node:fs'; import * as ns from 'node:path'; console.log(fs, read, ns.join)";
    let output = output_for(source, options_for("15", BuildFormat::CommonJs));
    assert_eq!(
        source_body(&output),
        "// <stdin>\nvar import_node_fs = __toESM(require(\"fs\"));\nvar ns = __toESM(require(\"path\"));\nconsole.log(import_node_fs.default, import_node_fs.readFileSync, ns.join);\n"
    );
}

#[test]
fn explicit_externals_other_platforms_and_unbundled_builds_keep_prefixes() {
    for platform in [
        BuildPlatform::Node,
        BuildPlatform::Browser,
        BuildPlatform::Neutral,
    ] {
        let mut options = options_for("12.19", BuildFormat::EsModule);
        options.platform = platform;
        options.external = vec!["node:fs".into()];
        let output = output_for(IMPORT_SOURCE, options);
        assert!(source_body(&output).contains("from \"node:fs\""));
        assert!(source_body(&output).contains("__require(\"node:fs\")"));
    }
    let mut options = options_for("14.13.0", BuildFormat::EsModule);
    options.bundle = false;
    assert_eq!(
        output_for(IMPORT_SOURCE, options),
        "import fs from \"node:fs\";\nimport(\"node:fs\");\nfs();\n"
    );
}

#[test]
fn stripped_static_and_lowered_dynamic_imports_retain_runtime_interop() {
    let source = "import fs, { readFileSync as read } from 'node:fs'; import * as ns from 'node:path'; import('node:fs').then(mod => { if (mod.default !== fs || mod.readFileSync !== read || typeof ns.join !== 'function') throw new Error('interop'); console.log('ok') })";
    let mut options = options_for("14.17", BuildFormat::CommonJs);
    options.supported.insert("dynamic-import".into(), false);
    let output = output_for(source, options);
    let result = Command::new("node")
        .args(["--eval", &output])
        .output()
        .expect("execute stripped imports in Node.js");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"ok\n");
}
