use std::{
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use esbuild_rs::api::{LogLevel, TransformOptions, transform};

#[test]
fn imported_binding_assignments_warn_and_respect_overrides() {
    for (name, hint) in [("value", "setValue"), ("π", "set_π"), ("_", "")] {
        let source = format!("import {{x as {name}}} from 'foo'; {name}++");
        let result = transform(&source, TransformOptions::default());
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), 1);
        let warning = &result.warnings[0];
        assert_eq!(warning.id, "assign-to-import");
        assert_eq!(
            warning.text,
            format!("This assignment will throw because {name:?} is an import")
        );
        assert_eq!(warning.notes.len(), 1);
        assert!(warning.notes[0].location.is_none());
        assert!(warning.notes[0].text.contains("Imports are immutable"));
        if hint.is_empty() {
            assert!(!warning.notes[0].text.contains("e.g."));
        } else {
            assert!(warning.notes[0].text.contains(hint));
        }
        let silent = transform(
            &source,
            TransformOptions {
                log_override: [("assign-to-import".into(), LogLevel::Silent)].into(),
                ..TransformOptions::default()
            },
        );
        assert!(silent.errors.is_empty());
        assert!(silent.warnings.is_empty());
        assert!(!silent.code.is_empty());
        let promoted = transform(
            &source,
            TransformOptions {
                log_override: [("assign-to-import".into(), LogLevel::Error)].into(),
                ..TransformOptions::default()
            },
        );
        assert_eq!(promoted.errors.len(), 1);
        assert!(promoted.warnings.is_empty());
        assert!(promoted.code.is_empty());
    }
}

#[test]
fn transformed_import_assignments_keep_runtime_errors_and_allow_object_mutation() {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "esbuild-rs-import-assignment-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join("dependency.mjs"),
        "export let value=1; export const box={n:0};",
    )
    .unwrap();
    for minify in [false, true] {
        let source = r#"
            import {value, box} from './dependency.mjs';
            import * as ns from './dependency.mjs';
            box.n=9;
            let caught=0;
            try {value++} catch (e) {if(e instanceof TypeError)caught++}
            try {ns.value=2} catch (e) {if(e instanceof TypeError)caught++}
            try {delete ns.value} catch (e) {if(e instanceof TypeError)caught++}
            console.log(caught, value, box.n);
        "#;
        let result = transform(
            source,
            TransformOptions {
                minify_syntax: minify,
                minify_identifiers: minify,
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), 1);
        std::fs::write(path.join("entry.mjs"), result.code).unwrap();
        let output = Command::new("node")
            .arg(path.join("entry.mjs"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"3 1 9\n");
    }
    std::fs::remove_dir_all(path).unwrap();
}
