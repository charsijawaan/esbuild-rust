use std::process::Command;

use esbuild_rs::api::{
    BuildFormat, BuildOptions, BuildPlatform, BuildStdin, Target, TransformOptions, build,
    transform,
};

const DEFAULT_CLASS: &str = r#"
class Base { static foo = 123; }
export default class extends Base {
  #instance = 4;
  static #value = super.foo;
  static #read() { return this.#value; }
  static nameAtInitialization = this.name;
  static result = this.#read();
  static arrow = () => this.#read();
  static nested = () => class { [this.result] = 1; };
  read() { return this.#instance; }
}
"#;

const DRIVER: &str = r#"
const assert = require('node:assert/strict');
const Box = module.exports.default;
assert.equal(Box.result, 123);
assert.equal(Box.arrow.call({}), 123);
assert.equal(new Box().read(), 4);
assert.equal(new (Box.nested())()[123], 1);
console.log('ok');
"#;

fn execute(code: &[u8], keep_names: bool) {
    let names = if keep_names {
        "require('node:assert/strict').equal(module.exports.default.name, 'default');\
         require('node:assert/strict').equal(module.exports.default.nameAtInitialization, 'default');"
    } else {
        ""
    };
    let output = Command::new("node")
        .arg("-e")
        .arg(format!(
            "{}\n{names}\n{DRIVER}",
            String::from_utf8_lossy(code)
        ))
        .output()
        .expect("Node.js is required for default class initialization regressions");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(code)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn anonymous_default_classes_initialize_the_captured_class_before_exporting() {
    for source in [
        DEFAULT_CLASS.to_string(),
        DEFAULT_CLASS
            .replace(" extends Base", "")
            .replace("super.foo", "123"),
    ] {
        for (target, supported) in [
            (Target::Es2015, std::collections::HashMap::new()),
            (Target::Es2022, std::collections::HashMap::new()),
            (
                Target::Es2022,
                std::collections::HashMap::from([("class-static-field".into(), false)]),
            ),
        ] {
            for keep_names in [false, true] {
                for minify in [false, true] {
                    let result = transform(
                        &source,
                        TransformOptions {
                            format: BuildFormat::CommonJs,
                            target,
                            supported: supported.clone(),
                            keep_names,
                            minify_identifiers: minify,
                            minify_syntax: minify,
                            minify_whitespace: minify,
                            ..TransformOptions::default()
                        },
                    );
                    assert!(result.errors.is_empty(), "{:?}", result.errors);
                    execute(&result.code, keep_names);
                    let result = build(BuildOptions {
                        bundle: true,
                        stdin: Some(BuildStdin {
                            contents: source.clone(),
                            ..BuildStdin::default()
                        }),
                        format: BuildFormat::CommonJs,
                        platform: BuildPlatform::Node,
                        target,
                        supported: supported.clone(),
                        keep_names,
                        minify_identifiers: minify,
                        minify_syntax: minify,
                        minify_whitespace: minify,
                        ..BuildOptions::default()
                    });
                    assert!(result.errors.is_empty(), "{:?}", result.errors);
                    execute(&result.output_files[0].contents, keep_names);
                }
            }
        }
    }
}
