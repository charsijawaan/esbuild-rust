use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use esbuild_rs::api::{BuildFormat, BuildOptions, BuildPlatform, build};

struct SplitFixture(PathBuf);

impl SplitFixture {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-splitting-import-names-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create splitting fixture");
        for (path, contents) in [
            ("package.json", r#"{"type":"module"}"#),
            (
                "test1.js",
                "export let sameName = { test: 1 }; export function change(value) { sameName = value; }",
            ),
            (
                "test2.js",
                "export let sameName = { test: 2 }; export function change(value) { sameName = value; }",
            ),
            (
                "entry1.js",
                "export { sameName, change as change1 } from './test1.js'; export { sameName as renameVar, change as change2 } from './test2.js';",
            ),
            ("entry2.js", "export * from './entry1.js';"),
            (
                "entry3.js",
                "import { sameName as first } from './test1.js'; import { sameName as second } from './test2.js'; export const pair = () => [first, second]; export const load = async () => { const ns = await import('./entry1.js'); return [ns.sameName, ns.renameVar]; };",
            ),
        ] {
            std::fs::write(directory.join(path), contents).expect("write splitting input");
        }
        Self(std::fs::canonicalize(directory).expect("canonical splitting fixture"))
    }
}

impl Drop for SplitFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn split_entries_rename_colliding_imports_and_preserve_live_export_bindings() {
    let fixture = SplitFixture::new();
    for minify in [false, true] {
        for keep_names in [false, true] {
            for custom_paths in [false, true] {
                let result = build(BuildOptions {
                    entry_points: vec!["entry1.js".into(), "entry2.js".into(), "entry3.js".into()],
                    abs_working_dir: fixture.0.to_string_lossy().into_owned(),
                    outdir: "out".into(),
                    bundle: true,
                    splitting: true,
                    format: BuildFormat::EsModule,
                    platform: BuildPlatform::Node,
                    minify_identifiers: minify,
                    minify_syntax: minify,
                    minify_whitespace: minify,
                    keep_names,
                    entry_names: if custom_paths {
                        "entries/[name]".into()
                    } else {
                        String::new()
                    },
                    chunk_names: if custom_paths {
                        "chunks/[name]-[hash]".into()
                    } else {
                        String::new()
                    },
                    out_extension: HashMap::from([(".js".into(), ".mjs".into())]),
                    ..BuildOptions::default()
                });
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                assert!(result.warnings.is_empty(), "{:?}", result.warnings);
                for output in result.output_files {
                    let path = PathBuf::from(output.path);
                    std::fs::create_dir_all(path.parent().expect("output parent"))
                        .expect("create output directory");
                    std::fs::write(path, output.contents).expect("write split output");
                }
                let prefix = if custom_paths {
                    "./out/entries"
                } else {
                    "./out"
                };
                let source = format!(
                    r#"
import assert from 'node:assert/strict';
import * as a from '{prefix}/entry1.mjs';
import * as b from '{prefix}/entry2.mjs';
import * as c from '{prefix}/entry3.mjs';
assert.equal(a.sameName.test, 1);
assert.equal(a.renameVar.test, 2);
assert.equal(b.sameName, a.sameName);
assert.equal(b.renameVar, a.renameVar);
assert.deepEqual(c.pair(), [a.sameName, a.renameVar]);
assert.deepEqual(await c.load(), [a.sameName, a.renameVar]);
a.change1({{ test: 3 }});
b.change2({{ test: 4 }});
assert.equal(a.sameName.test, 3);
assert.equal(a.renameVar.test, 4);
assert.equal(b.sameName, a.sameName);
assert.equal(b.renameVar, a.renameVar);
assert.deepEqual(c.pair(), [a.sameName, a.renameVar]);
assert.deepEqual(await c.load(), [a.sameName, a.renameVar]);
if ({keep_names}) {{
  assert.equal(a.change1.name, 'change');
  assert.equal(a.change2.name, 'change');
}}
console.log('ok');
"#
                );
                std::fs::write(fixture.0.join("run.mjs"), source)
                    .expect("write splitting assertions");
                let output = Command::new("node")
                    .arg(fixture.0.join("run.mjs"))
                    .output()
                    .expect("execute split modules with Node.js");
                assert!(
                    output.status.success(),
                    "minify={minify}, keep_names={keep_names}, custom_paths={custom_paths}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(output.stdout, b"ok\n");
                std::fs::remove_dir_all(fixture.0.join("out")).expect("clear splitting output");
            }
        }
    }
}
