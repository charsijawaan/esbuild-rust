use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Engine, EngineName, OnResolveOptions,
    OnResolveResult, Plugin, ResolveKind, build,
};

const FORMATS: [BuildFormat; 3] = [
    BuildFormat::CommonJs,
    BuildFormat::EsModule,
    BuildFormat::Iife,
];
const ALIASES: [(&str, &str); 3] = [
    ("RESOLVE", "RESOLVE"),
    ("X.resolve", "X.resolve"),
    ("X[\"resolve\"]", "X.resolve"),
];

fn options(format: BuildFormat, version: &str, define: &[(&str, &str)]) -> BuildOptions {
    BuildOptions {
        bundle: true,
        platform: BuildPlatform::Node,
        format,
        engines: vec![Engine {
            name: EngineName::Node,
            version: version.into(),
        }],
        supported: [("dynamic-import".into(), false)].into(),
        define: define
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
        write: false,
        ..BuildOptions::default()
    }
}

fn body(output: &str) -> &str {
    &output[output.find("// <stdin>\n").expect("source label")..]
}

fn reference_output(source: &str, options: &BuildOptions) -> Option<String> {
    let executable = std::env::var_os("ESBUILD_RS_TEAM_DEFINED_RESOLVE_GO")?;
    let format = match options.format {
        BuildFormat::CommonJs => "cjs",
        BuildFormat::EsModule => "esm",
        BuildFormat::Iife => "iife",
        BuildFormat::Default => panic!("explicit fixture format"),
    };
    let mut command = Command::new(executable);
    command.args(["--bundle", "--platform=node"]);
    command.arg(format!("--format={format}"));
    command.arg(format!("--target=node{}", options.engines[0].version));
    command.arg("--supported:dynamic-import=false");
    for (key, value) in &options.define {
        command.arg(format!("--define:{key}={value}"));
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
        .expect("write Go source");
    let output = child.wait_with_output().expect("read Go output");
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
    let output = String::from_utf8(result.output_files[0].contents.clone()).unwrap();
    if options.plugins.is_empty()
        && let Some(reference) = reference_output(source, &options)
    {
        // Match the existing original nodeColonPrefix tests' comparison scope.
        assert_eq!(body(&output), body(&reference), "{source}, {options:?}");
    }
    output
}

#[test]
fn defined_literal_resolve_uses_records_and_require_prefix_support() {
    for format in FORMATS {
        for (version, path) in [("14.17", "fs"), ("14.18", "node:fs")] {
            for (target, key) in ALIASES {
                let source = format!("{target}(\"node:fs\");");
                let output = output_for(
                    &source,
                    options(format, version, &[(key, "require.resolve")]),
                );
                assert!(body(&output).contains(&format!("require.resolve(\"{path}\");")));
                assert!(!body(&output).contains("__require"));
                assert!(!body(&output).contains("(0,"));
            }
        }
    }
}

#[test]
fn source_resolve_property_substitutes_before_record_recognition() {
    for format in FORMATS {
        let output = output_for(
            "require.resolve(\"node:fs\");",
            options(format, "14.17", &[]),
        );
        let expected = if format == BuildFormat::CommonJs {
            "require.resolve(\"fs\");"
        } else {
            "__require.resolve(\"node:fs\");"
        };
        assert!(body(&output).contains(expected));
        assert!(!body(&output).contains("(0,"));
    }
}

#[test]
fn source_require_define_precedes_runtime_require_substitution() {
    for format in FORMATS {
        let output = output_for(
            "require.resolve(\"node:fs\");",
            options(format, "14.17", &[("require", "receiver")]),
        );
        assert!(body(&output).contains("receiver.resolve(\"node:fs\");"));
        assert!(!output.contains("__require"));
    }
}

#[test]
fn defined_nonliteral_resolve_retains_original_call_receiver_kind() {
    for format in FORMATS {
        for (target, key) in ALIASES {
            let source = format!("var name = \"node:fs\"; {target}(name);");
            let output = output_for(
                &source,
                options(format, "14.17", &[(key, "require.resolve")]),
            );
            let require = if format == BuildFormat::CommonJs {
                "require"
            } else {
                "__require"
            };
            let expected = if key == "RESOLVE" {
                format!("(0, {require}.resolve)(name);")
            } else {
                format!("{require}.resolve(name);")
            };
            assert!(body(&output).contains(&expected));
            assert!(body(&output).contains("var name = \"node:fs\";"));
        }
    }
}

#[test]
fn defined_literal_resolve_plugin_sees_original_path_and_record_kind() {
    for format in FORMATS {
        for (target, key) in ALIASES {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut opts = options(format, "14.17", &[(key, "require.resolve")]);
            let observed = Arc::clone(&calls);
            opts.plugins
                .push(Plugin::new("resolve-record", move |build| {
                    let observed = Arc::clone(&observed);
                    build.on_resolve(
                        OnResolveOptions {
                            filter: "^node:".into(),
                            ..OnResolveOptions::default()
                        },
                        move |args| {
                            observed.lock().unwrap().push((args.path, args.kind));
                            Ok(OnResolveResult {
                                path: "node:path".into(),
                                external: true,
                                ..OnResolveResult::default()
                            })
                        },
                    );
                    Ok(())
                }));
            let output = output_for(&format!("{target}(\"node:fs\");"), opts);
            assert_eq!(
                *calls.lock().unwrap(),
                vec![("node:fs".into(), ResolveKind::RequireResolve)]
            );
            assert!(body(&output).contains("require.resolve(\"node:path\");"));
        }
    }
}

#[test]
fn defined_literal_resolve_executes_with_require_receiver() {
    for format in FORMATS {
        for (version, path) in [("14.17", "fs"), ("14.18", "node:fs")] {
            let output = output_for(
                "report(RESOLVE(\"node:fs\"));",
                options(format, version, &[("RESOLVE", "require.resolve")]),
            );
            let harness = format!(
                "function req() {{ throw Error('unexpected require'); }}\n\
                 req.resolve = function(path) {{ if (this !== req) throw Error('lost receiver'); return path; }};\n\
                 new Function('require', 'report', {})(req, value => {{ if (value !== {}) throw Error(value); }});",
                serde_json::to_string(&output).unwrap(),
                serde_json::to_string(path).unwrap(),
            );
            let execution = Command::new("node")
                .args(["-e", &harness])
                .output()
                .expect("execute receiver regression");
            assert!(
                execution.status.success(),
                "{}",
                String::from_utf8_lossy(&execution.stderr)
            );
        }
    }
}
