//! Node execution of bindings hoisted from `CommonJS` wrappers.

use esbuild_rs::api::{BuildFormat, BuildOptions, BuildPlatform, build};
use std::{path::PathBuf, process::Command};

fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "esbuild-external-names-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create fixture");
    for (path, text) in files {
        std::fs::write(root.join(path), text).expect("write input");
    }
    std::fs::canonicalize(root).expect("canonical fixture")
}

fn execute(root: &std::path::Path, minify: bool, format: BuildFormat) {
    let result = build(BuildOptions {
        abs_working_dir: root.to_string_lossy().into_owned(),
        entry_points: vec!["entry.js".into()],
        bundle: true,
        platform: BuildPlatform::Node,
        format,
        minify_identifiers: minify,
        ..BuildOptions::default()
    });
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(result.output_files.len(), 1);
    let filename = if format == BuildFormat::EsModule {
        "output.mjs"
    } else {
        "output.cjs"
    };
    std::fs::write(root.join(filename), &result.output_files[0].contents).expect("write output");
    let result = Command::new("node")
        .arg(filename)
        .current_dir(root)
        .output()
        .expect("run node");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"ok\n");
}

#[test]
fn external_namespace_shims_keep_independent_values() {
    let root = fixture(
        "namespaces",
        &[
            ("fs.js", "import * as all from 'fs'; module.exports = all;"),
            (
                "url.js",
                "import * as all from 'url'; module.exports = all;",
            ),
            (
                "path.js",
                "import * as all from 'path'; module.exports = all;",
            ),
            (
                "entry.js",
                "const fs = require('./fs.js'); const url = require('./url.js'); const path = require('./path.js'); if (!fs.existsSync(url.fileURLToPath(new URL(import.meta.url))) || path.basename(url.fileURLToPath(new URL(import.meta.url))) !== 'output.mjs') throw new Error('wrong namespace'); console.log('ok');",
            ),
        ],
    );
    for minify in [false, true] {
        execute(&root, minify, BuildFormat::EsModule);
    }
    std::fs::remove_dir_all(root).expect("clean successful fixture");
}

#[test]
fn hoisted_default_and_named_imports_avoid_entry_collisions() {
    let root = fixture(
        "bindings",
        &[
            (
                "a.js",
                "import all from 'path'; const local = 1; module.exports = [all.basename('/left'), local];",
            ),
            (
                "b.js",
                "import {basename as all} from 'path'; const local = 2; module.exports = [all('/right'), local];",
            ),
            (
                "entry.js",
                "const all = require('./a.js'); const all2 = require('./b.js'); if (all[0] !== 'left' || all[1] !== 1 || all2[0] !== 'right' || all2[1] !== 2) throw new Error('wrong binding'); console.log('ok');",
            ),
        ],
    );
    for minify in [false, true] {
        execute(&root, minify, BuildFormat::EsModule);
        execute(&root, minify, BuildFormat::CommonJs);
    }
    std::fs::remove_dir_all(root).expect("clean successful fixture");
}

#[test]
fn internal_namespace_imports_stay_bound_inside_wrappers() {
    let root = fixture(
        "internal",
        &[
            ("left.js", "export const value = 1;"),
            ("right.js", "export const value = 2;"),
            (
                "a.js",
                "import * as all from './left.js'; module.exports = all.value;",
            ),
            (
                "b.js",
                "import * as all from './right.js'; module.exports = all.value;",
            ),
            (
                "entry.js",
                "if (require('./a.js') !== 1 || require('./b.js') !== 2) throw new Error('wrong internal namespace'); console.log('ok');",
            ),
        ],
    );
    for minify in [false, true] {
        execute(&root, minify, BuildFormat::EsModule);
        execute(&root, minify, BuildFormat::CommonJs);
    }
    std::fs::remove_dir_all(root).expect("clean successful fixture");
}
