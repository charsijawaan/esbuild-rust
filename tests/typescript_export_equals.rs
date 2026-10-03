use std::{
    io::Write,
    process::{Command, Stdio},
};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, Loader, TransformOptions, build, transform,
};

const SOURCES: &[(&str, &str)] = &[
    ("export = module; let module = 123;", "123"),
    (
        "const module = { value: 123 }; export = module;",
        "{ value: 123 }",
    ),
    (
        "function module() { return 123; } export = module();",
        "123",
    ),
    (
        "class module { static value = 123; } export = module.value;",
        "123",
    ),
    ("const exports = 123; export = exports;", "123"),
    ("export = (() => module)(); let module = 123;", "123"),
    (
        "export = new (class { value = module })().value; let module = 123;",
        "123",
    ),
];

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for TypeScript export assignment regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

fn execute_transform(code: &[u8], expected: &str) {
    let code = String::from_utf8_lossy(code);
    execute(
        format!(
            "const assert = require('node:assert/strict'); const result = {{ exports: {{}} }}; new Function('module', 'exports', {code:?})(result, result.exports); assert.deepEqual(result.exports, {expected}); console.log('ok');"
        )
        .as_bytes(),
    );
}

#[test]
fn export_assignment_uses_the_common_js_module_symbol() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "esbuild-export-equals-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    for (index, (source, expected)) in SOURCES.iter().enumerate() {
        std::fs::write(root.join(format!("module{index}.ts")), source).unwrap();
        std::fs::write(
            root.join("entry.js"),
            format!("require('node:assert/strict').deepEqual(require('./module{index}.ts'), {expected}); console.log('ok');"),
        )
        .unwrap();
        for minify in [false, true] {
            let result = transform(
                source,
                TransformOptions {
                    loader: Loader::Ts,
                    format: BuildFormat::CommonJs,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    ..TransformOptions::default()
                },
            );
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            execute_transform(&result.code, expected);

            let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
                .arg("--loader=ts")
                .arg("--format=cjs")
                .args(if minify { vec!["--minify"] } else { Vec::new() })
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(source.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            execute_transform(&output.stdout, expected);

            for format in [
                BuildFormat::CommonJs,
                BuildFormat::EsModule,
                BuildFormat::Iife,
            ] {
                let result = build(BuildOptions {
                    abs_working_dir: root.to_string_lossy().into_owned(),
                    entry_points: vec!["entry.js".into()],
                    outfile: "out.js".into(),
                    bundle: true,
                    format,
                    platform: BuildPlatform::Node,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    ..BuildOptions::default()
                });
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                execute(&result.output_files[0].contents);
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
