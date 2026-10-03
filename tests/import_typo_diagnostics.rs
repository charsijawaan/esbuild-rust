use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use esbuild_rs::api::{BuildFormat, BuildOptions, Loader, LogLevel, Message, build};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct TypoFixture(PathBuf);

impl TypoFixture {
    fn new(extension: &str, entry: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-import-typo-{}-{unique}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("create import typo fixture");
        std::fs::write(
            directory.join("entry.js"),
            entry.replace("EXTENSION", extension).replace(
                "buton",
                if extension == "txt" {
                    "defaul"
                } else {
                    "buton"
                },
            ),
        )
        .expect("write importer");
        std::fs::write(
            directory.join(format!("dependency.{extension}")),
            match extension {
                "css" => r#".bu\74 ton { color: red }"#,
                "js" => "export const button = 1;",
                "json" => r#"{"button":1}"#,
                "txt" => "stuff",
                _ => unreachable!(),
            },
        )
        .expect("write export");
        Self(std::fs::canonicalize(directory).expect("canonical typo fixture"))
    }

    fn options(&self) -> BuildOptions {
        BuildOptions {
            entry_points: vec!["entry.js".into()],
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            bundle: true,
            format: BuildFormat::CommonJs,
            outfile: "out.js".into(),
            loader: HashMap::from([
                (".css".into(), Loader::LocalCss),
                (".txt".into(), Loader::Text),
            ]),
            ..BuildOptions::default()
        }
    }
}

impl Drop for TypoFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn check_suggestion(message: &Message, extension: &str) {
    let suggestion = if extension == "txt" {
        "default"
    } else {
        "button"
    };
    assert_eq!(
        message
            .location
            .as_ref()
            .expect("import location")
            .suggestion,
        suggestion
    );
    assert_eq!(message.notes.len(), 1);
    assert_eq!(
        message.notes[0].text,
        format!("Did you mean to import {suggestion:?} instead?")
    );
    if extension == "txt" {
        assert!(message.notes[0].location.is_none());
    } else {
        let location = message.notes[0].location.as_ref().expect("export location");
        assert_eq!(location.file, format!("dependency.{extension}"));
        assert_eq!(
            location.length,
            match extension {
                "css" => 9,
                "json" => 8,
                _ => 6,
            }
        );
    }
}

#[test]
fn generated_namespace_import_warnings_include_replacement_and_export_ranges() {
    for extension in ["js", "css", "json", "txt"] {
        let fixture = TypoFixture::new(
            extension,
            "import * as ns from './dependency.EXTENSION';\nif (ns.buton !== void 0) throw new Error('expected undefined');\nconsole.log('ok');",
        );
        let result = build(fixture.options());
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert_eq!(result.warnings[0].id, "import-is-undefined");
        check_suggestion(&result.warnings[0], extension);
        let output = result
            .output_files
            .iter()
            .find(|file| file.path.ends_with(".js"))
            .expect("JS output");
        let execution = Command::new("node")
            .arg("-e")
            .arg(String::from_utf8_lossy(&output.contents).as_ref())
            .output()
            .expect("execute missing namespace import");
        assert!(
            execution.status.success(),
            "{}",
            String::from_utf8_lossy(&execution.stderr)
        );
        assert_eq!(execution.stdout, b"ok\n");
    }
}

#[test]
fn named_import_errors_include_replacement_and_export_ranges() {
    for extension in ["js", "css", "json", "txt"] {
        let fixture = TypoFixture::new(
            extension,
            "import { buton } from './dependency.EXTENSION'; console.log(buton);",
        );
        let result = build(fixture.options());
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert!(result.warnings.is_empty());
        check_suggestion(&result.errors[0], extension);
        assert!(result.output_files.is_empty());
    }
}

#[test]
fn generated_import_suggestions_survive_warning_promotion() {
    let fixture = TypoFixture::new(
        "css",
        "import * as ns from './dependency.css'; console.log(ns.buton);",
    );
    let mut options = fixture.options();
    options
        .log_override
        .insert("import-is-undefined".into(), LogLevel::Error);
    let result = build(options);
    assert_eq!(result.errors.len(), 1);
    assert!(result.warnings.is_empty());
    check_suggestion(&result.errors[0], "css");
}
