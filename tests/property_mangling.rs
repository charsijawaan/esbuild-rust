use std::{
    io::Write,
    process::{Command, Stdio},
};

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Loader, Target, TransformOptions, build,
    transform,
};

fn execute(code: &[u8]) {
    let output = Command::new("node")
        .arg("-e")
        .arg(String::from_utf8_lossy(code).as_ref())
        .output()
        .expect("Node.js is required for property mangling regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

const PROPERTY_MANGLING: &str = r#"
const assert = require('node:assert/strict');
const original = name => [name, '_'].join('');
const object = {
  a: 9,
  value_: 1,
  reserved_: 2,
  'quoted_': 3,
  method_() { return this.value_; },
  callback_: function() { return 4; },
};
object.value_++;
const { value_: value } = object;
let assigned;
({ value_: assigned } = object);
assert.equal(value, 2);
assert.equal(assigned, 2);
assert.equal(object?.method_?.(), 2);
assert.equal(object.reserved_, 2);
assert.equal(object['quoted_'], 3);
assert.equal(object.a, 9);
assert.equal(object.callback_(), 4);
assert.equal(Object.hasOwn(object, original('value')), false);
assert.equal(Object.hasOwn(object, original('quoted')), !quoted);
if (quoted) assert.equal('quoted_' in object, true);
class Box {
  value_ = 5;
  callback_ = () => 6;
  static total_ = 7;
  accessor box_ = 8;
  method_() { return this.value_; }
}
class Derived extends Box {
  method_() { return super.method_() + 1; }
}
const instance = new Derived;
assert.equal(instance.method_(), 6);
assert.equal(instance.callback_(), 6);
assert.equal(Box.total_, 7);
assert.equal(instance.box_++, 8);
assert.equal(instance.box_, 9);
assert.equal(Object.hasOwn(instance, original('value')), false);
delete object.value_;
assert.equal(object.method_(), undefined);
console.log('ok');
"#;

#[test]
fn property_mangling_preserves_accesses_and_lowered_class_keys() {
    for target in [Target::Es2015, Target::Es2022] {
        for quoted in [false, true] {
            for minify in [false, true] {
                let source = format!("const quoted = {quoted};\n{PROPERTY_MANGLING}");
                let result = transform(
                    &source,
                    TransformOptions {
                        target,
                        mangle_props: "_$".into(),
                        reserve_props: "^reserved_$".into(),
                        mangle_quoted: quoted,
                        keep_names: true,
                        minify_identifiers: minify,
                        minify_syntax: minify,
                        minify_whitespace: minify,
                        ..TransformOptions::default()
                    },
                );
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                execute(&result.code);
                let result = build(BuildOptions {
                    bundle: true,
                    platform: BuildPlatform::Node,
                    format: BuildFormat::CommonJs,
                    stdin: Some(BuildStdin {
                        contents: source.clone(),
                        ..BuildStdin::default()
                    }),
                    target,
                    mangle_props: "_$".into(),
                    reserve_props: "^reserved_$".into(),
                    mangle_quoted: quoted,
                    keep_names: true,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    ..BuildOptions::default()
                });
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                execute(&result.output_files[0].contents);
                let mut child = Command::new(env!("CARGO_BIN_EXE_esbuild"))
                    .args([
                        if target == Target::Es2015 {
                            "--target=es2015"
                        } else {
                            "--target=es2022"
                        },
                        "--mangle-props=_$",
                        "--reserve-props=^reserved_$",
                        "--keep-names",
                    ])
                    .arg(format!("--mangle-quoted={quoted}"))
                    .args(if minify { vec!["--minify"] } else { vec![] })
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
                execute(&output.stdout);
            }
        }
    }
}

#[test]
fn typescript_assignment_fields_use_mangled_keys() {
    let source = "class Box { value_ = 1; static total_ = 2; } const box = new Box; if(box.value_ !== 1 || Box.total_ !== 2 || Object.hasOwn(box, ['value', '_'].join(''))) throw 'fail'; console.log('ok');";
    for target in [Target::Es2015, Target::Es2022] {
        let result = transform(
            source,
            TransformOptions {
                loader: Loader::Ts,
                target,
                mangle_props: "_$".into(),
                tsconfig_raw: r#"{"compilerOptions":{"useDefineForClassFields":false}}"#.into(),
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        execute(&result.code);
    }
}

#[test]
fn invalid_property_patterns_are_reported_by_the_api_and_cli() {
    let result = transform(
        "",
        TransformOptions {
            mangle_props: "[".into(),
            ..TransformOptions::default()
        },
    );
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].text.contains("mangle props"));
    let result = build(BuildOptions {
        reserve_props: "[".into(),
        ..BuildOptions::default()
    });
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].text.contains("reserve props"));
    for loader in [Loader::Css, Loader::Json] {
        let result = transform(
            "",
            TransformOptions {
                loader,
                mangle_props: "[".into(),
                ..TransformOptions::default()
            },
        );
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].text.contains("mangle props"));
    }
    for argument in [
        "--mangle-props=[",
        "--reserve-props=[",
        "--mangle-quoted=maybe",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_esbuild"))
            .arg(argument)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn property_names_are_shared_by_modules_and_split_chunks() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("esbuild-mangle-{}-{unique}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("shared.js"), "export const object = { value_: 3, keep_: 4 }; export function read(object) { return object.value_; }").unwrap();
    for (name, value) in [("one", 5), ("two", 7)] {
        std::fs::write(root.join(format!("{name}.js")), format!(
            "import {{ object, read }} from './shared.js'; object.value_ = {value}; if(read(object) !== {value} || object.keep_ !== 4 || ['value', '_'].join('') in object) throw 'fail'; console.log('ok');"
        )).unwrap();
    }
    for splitting in [false, true] {
        for minify in [false, true] {
            let result = build(BuildOptions {
                abs_working_dir: root.to_string_lossy().into_owned(),
                entry_points: vec!["one.js".into(), "two.js".into()],
                outdir: "out".into(),
                bundle: true,
                splitting,
                format: BuildFormat::EsModule,
                platform: BuildPlatform::Node,
                mangle_props: "_$".into(),
                reserve_props: "^keep_$".into(),
                minify_identifiers: minify,
                minify_syntax: minify,
                minify_whitespace: minify,
                ..BuildOptions::default()
            });
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            for output in result.output_files {
                let path = std::path::Path::new(&output.path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, output.contents).unwrap();
            }
            for name in ["one", "two"] {
                let path = root.join("out").join(format!("{name}.js"));
                let url = url::Url::from_file_path(path).unwrap();
                execute(format!("import({:?}).catch(error => {{ console.error(error); process.exitCode = 1; }});", url.as_str()).as_bytes());
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
