use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use esbuild_rs::api::{BuildOptions, BuildResult, LogLevel, build};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct ResolverFixture(PathBuf);

impl ResolverFixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rs-tsconfig-inheritance-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("create resolver fixture");
        for (path, contents) in files {
            let path = directory.join(path);
            std::fs::create_dir_all(path.parent().expect("file parent"))
                .expect("create resolver file parent");
            std::fs::write(path, contents).expect("write resolver fixture");
        }
        Self(std::fs::canonicalize(directory).expect("canonical resolver fixture"))
    }

    fn build(&self, raw_config: &str, log_override: HashMap<String, LogLevel>) -> BuildResult {
        let options = BuildOptions {
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            entry_points: vec!["entry.ts".into()],
            bundle: true,
            outfile: "out.js".into(),
            tsconfig_raw: raw_config.into(),
            log_override,
            ..BuildOptions::default()
        };
        let result = build(options.clone());
        // Opt-in validation against the pinned compiler keeps ordinary tests
        // independent of a local Go checkout.
        if let Some(executable) = std::env::var_os("ESBUILD_RS_TEAM_RESOLVER_GO") {
            let mut command = std::process::Command::new(executable);
            command.current_dir(&self.0).args([
                "entry.ts",
                "--bundle",
                "--outfile=out.js",
                "--color=false",
                "--log-limit=0",
                "--log-level=warning",
            ]);
            if !raw_config.is_empty() {
                command.arg(format!("--tsconfig-raw={raw_config}"));
            }
            for (id, level) in &options.log_override {
                let level = match level {
                    LogLevel::Silent => "silent",
                    LogLevel::Error => "error",
                    _ => panic!("unsupported reference log level"),
                };
                command.arg(format!("--log-override:{id}={level}"));
            }
            let reference = command.output().expect("run pinned Go compiler");
            let diagnostics = String::from_utf8_lossy(&reference.stderr);
            assert_eq!(
                reference.status.success(),
                result.errors.is_empty(),
                "{diagnostics}"
            );
            assert_eq!(
                diagnostics.matches("[WARNING]").count(),
                result.warnings.len(),
                "{diagnostics}"
            );
            assert_eq!(
                diagnostics.matches("[ERROR]").count(),
                result.errors.len(),
                "{diagnostics}"
            );
            for message in result.errors.iter().chain(&result.warnings) {
                assert!(diagnostics.contains(&message.text), "{diagnostics}");
            }
            if result.errors.is_empty() {
                assert_eq!(result.output_files.len(), 1);
                assert_eq!(
                    std::fs::read(self.0.join("out.js")).expect("read Go output"),
                    result.output_files[0].contents,
                    "output differs from pinned Go"
                );
            }
        }
        result
    }
}

impl Drop for ResolverFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn output(result: &BuildResult) -> String {
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.output_files.len(), 1);
    String::from_utf8(result.output_files[0].contents.clone()).expect("JavaScript output")
}

fn invalid_path_warning(path: &str) -> String {
    format!(
        "Non-relative path {path:?} is not allowed when \"baseUrl\" is not set (did you forget a leading \"./\"?)"
    )
}

#[test]
fn no_base_url_filters_invalid_substitutions_and_preserves_fallback_order() {
    let config = r#"{"compilerOptions":{"paths":{"alias/*":["./missing/*","bad/*","./good/*"]}}}"#;
    let fixture = ResolverFixture::new(&[
        (
            "entry.ts",
            "import { value } from 'alias/selected'; console.log(value);",
        ),
        ("tsconfig.json", config),
        (
            "bad/selected.ts",
            "export const value = 'invalid substitution';",
        ),
        ("good/selected.ts", "export const value = 'valid fallback';"),
    ]);
    let result = fixture.build("", HashMap::new());
    let code = output(&result);
    assert!(code.contains("valid fallback"), "{code}");
    assert!(!code.contains("invalid substitution"), "{code}");
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    let warning = &result.warnings[0];
    assert_eq!(warning.id, "tsconfig.json");
    assert_eq!(warning.text, invalid_path_warning("bad/*"));
    let location = warning.location.as_ref().expect("substitution location");
    assert_eq!(location.file, "tsconfig.json");
    assert_eq!(location.line, 1);
    assert_eq!(location.column, config.find("\"bad/*\"").unwrap());
    assert_eq!(location.length, "\"bad/*\"".len());
    assert_eq!(location.line_text, config);
}

#[test]
fn fully_filtered_exact_and_pattern_mappings_fall_back_to_node_modules() {
    let fixture = ResolverFixture::new(&[
        (
            "entry.ts",
            "import { root } from 'pkg'; import { sub } from 'pkg/sub'; console.log(root, sub);",
        ),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{"pkg":["bad/root.ts"],"pkg/*":["bad/*"]}}}"#,
        ),
        ("bad/root.ts", "export const root = 'invalid root';"),
        ("bad/sub.ts", "export const sub = 'invalid subpath';"),
        (
            "node_modules/pkg/index.js",
            "export const root = 'package root';",
        ),
        (
            "node_modules/pkg/sub.js",
            "export const sub = 'package subpath';",
        ),
    ]);
    let result = fixture.build("", HashMap::new());
    let code = output(&result);
    assert!(
        code.contains("package root") && code.contains("package subpath"),
        "{code}"
    );
    assert!(
        !code.contains("invalid root") && !code.contains("invalid subpath"),
        "{code}"
    );
    let warnings: Vec<_> = result
        .warnings
        .iter()
        .map(|warning| warning.text.clone())
        .collect();
    assert_eq!(
        warnings,
        [
            invalid_path_warning("bad/root.ts"),
            invalid_path_warning("bad/*")
        ]
    );
}

#[test]
fn inherited_paths_wait_for_base_url_from_derived_and_array_configs() {
    for config in [
        r#"{"extends":"./middle.json","compilerOptions":{"baseUrl":"."}}"#,
        r#"{"extends":["./base/paths.json","./base/url.json"]}"#,
        r#"{"extends":["./base/url.json","./base/paths.json"]}"#,
        r#"{"extends":"./base/url.json","compilerOptions":{"paths":{"alias/*":["lib/*"]}}}"#,
    ] {
        let fixture = ResolverFixture::new(&[
            (
                "entry.ts",
                "import { value } from 'alias/selected'; console.log(value);",
            ),
            ("tsconfig.json", config),
            ("middle.json", r#"{"extends":"./base/paths.json"}"#),
            (
                "base/paths.json",
                r#"{"compilerOptions":{"paths":{"alias/*":["lib/*"]}}}"#,
            ),
            ("base/url.json", r#"{"compilerOptions":{"baseUrl":".."}}"#),
            (
                "lib/selected.ts",
                "export const value = 'inherited mapping';",
            ),
        ]);
        let result = fixture.build("", HashMap::new());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(output(&result).contains("inherited mapping"));
    }
}

#[test]
fn inherited_warnings_use_the_base_source_and_overridden_paths_do_not_warn() {
    let base = r#"{"compilerOptions":{"paths":{"alias/*":["bad/*","./good/*"]}}}"#;
    for (config, expected_value, should_warn) in [
        (r#"{"extends":"./base/paths.json"}"#, "base fallback", true),
        (
            r#"{"extends":"./base/paths.json","compilerOptions":{"paths":{"alias/*":["./replacement/*"]}}}"#,
            "derived mapping",
            false,
        ),
    ] {
        let fixture = ResolverFixture::new(&[
            (
                "entry.ts",
                "import { value } from 'alias/selected'; console.log(value);",
            ),
            ("tsconfig.json", config),
            ("base/paths.json", base),
            (
                "base/bad/selected.ts",
                "export const value = 'invalid substitution';",
            ),
            (
                "base/good/selected.ts",
                "export const value = 'base fallback';",
            ),
            (
                "replacement/selected.ts",
                "export const value = 'derived mapping';",
            ),
        ]);
        let result = fixture.build("", HashMap::new());
        assert!(output(&result).contains(expected_value));
        assert_eq!(
            result.warnings.len(),
            usize::from(should_warn),
            "{:?}",
            result.warnings
        );
        if should_warn {
            let warning = &result.warnings[0];
            assert_eq!(warning.text, invalid_path_warning("bad/*"));
            let location = warning.location.as_ref().expect("inherited location");
            assert_eq!(location.file, "base/paths.json");
            assert_eq!(location.column, base.find("\"bad/*\"").unwrap());
            assert_eq!(location.length, "\"bad/*\"".len());
        }
    }
}

#[test]
fn config_dir_templates_are_expanded_before_no_base_url_validation() {
    let fixture = ResolverFixture::new(&[
        (
            "entry.ts",
            "import { value } from 'alias/selected'; console.log(value);",
        ),
        ("tsconfig.json", r#"{"extends":"./base/paths.json"}"#),
        (
            "base/paths.json",
            r#"{"compilerOptions":{"paths":{"alias/*":["${configDir}/generated/*"]}}}"#,
        ),
        (
            "generated/selected.ts",
            "export const value = 'config directory';",
        ),
    ]);
    let result = fixture.build("", HashMap::new());
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert!(output(&result).contains("config directory"));
}

#[test]
fn separate_derived_configs_each_report_inherited_invalid_paths() {
    let fixture = ResolverFixture::new(&[
        ("entry.ts", "import './one/first'; import './two/second';"),
        ("tsconfig.json", "{}"),
        (
            "base.json",
            r#"{"compilerOptions":{"paths":{"unused":["bad.ts"]}}}"#,
        ),
        ("one/tsconfig.json", r#"{"extends":"../base.json"}"#),
        ("one/first.ts", "console.log('first config');"),
        ("two/tsconfig.json", r#"{"extends":"../base.json"}"#),
        ("two/second.ts", "console.log('second config');"),
    ]);
    let result = fixture.build("", HashMap::new());
    assert!(output(&result).contains("second config"));
    assert_eq!(result.warnings.len(), 2, "{:?}", result.warnings);
    for warning in &result.warnings {
        assert_eq!(warning.text, invalid_path_warning("bad.ts"));
        assert_eq!(warning.location.as_ref().unwrap().file, "base.json");
    }
}

#[test]
fn unused_path_substitutions_warn_and_respect_log_overrides() {
    let fixture = ResolverFixture::new(&[
        ("entry.ts", "console.log('no imports');"),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{"unused":["bad.ts"]}}}"#,
        ),
    ]);
    for level in [None, Some(LogLevel::Silent), Some(LogLevel::Error), None] {
        let overrides = level.map_or_else(HashMap::new, |level| {
            HashMap::from([("tsconfig.json".into(), level)])
        });
        let result = fixture.build("", overrides);
        if level == Some(LogLevel::Error) {
            assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
            assert_eq!(result.errors[0].text, invalid_path_warning("bad.ts"));
            assert_eq!(result.errors[0].id, "tsconfig.json");
            assert!(result.output_files.is_empty());
        } else {
            assert!(output(&result).contains("no imports"));
        }
        assert_eq!(result.warnings.len(), usize::from(level.is_none()));
    }
}

#[test]
fn raw_api_configs_also_filter_paths_without_base_url() {
    let fixture = ResolverFixture::new(&[
        (
            "entry.ts",
            "import { value } from 'alias'; console.log(value);",
        ),
        ("bad.ts", "export const value = 'raw configuration';"),
    ]);
    let result = fixture.build(
        r#"{"compilerOptions":{"paths":{"alias":["bad.ts"]}}}"#,
        HashMap::new(),
    );
    assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
    assert_eq!(result.warnings[0].text, invalid_path_warning("bad.ts"));
    assert_eq!(
        result.warnings[0].location.as_ref().unwrap().file,
        "<tsconfig.json>"
    );
    assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
    assert_eq!(result.errors[0].text, "Could not resolve \"alias\"");
    assert!(result.output_files.is_empty());
}

#[test]
fn context_rebuilds_revalidate_paths_after_config_changes() {
    let fixture = ResolverFixture::new(&[
        ("entry.ts", "console.log('rebuild');"),
        ("tsconfig.json", "{}"),
    ]);
    let context = esbuild_rs::api::context(BuildOptions {
        abs_working_dir: fixture.0.to_string_lossy().into_owned(),
        entry_points: vec!["entry.ts".into()],
        bundle: true,
        outfile: "out.js".into(),
        ..BuildOptions::default()
    })
    .expect("create build context");
    for invalid_path in [Some("first.ts"), Some("second.ts"), None, Some("first.ts")] {
        let config = invalid_path.map_or_else(
            || "{}".to_string(),
            |path| format!(r#"{{"compilerOptions":{{"paths":{{"unused":[{path:?}]}}}}}}"#),
        );
        std::fs::write(fixture.0.join("tsconfig.json"), config).expect("update tsconfig");
        let rebuilt = context.rebuild();
        assert!(output(&rebuilt).contains("rebuild"));
        assert_eq!(rebuilt.warnings.len(), usize::from(invalid_path.is_some()));
        if let Some(path) = invalid_path {
            assert_eq!(rebuilt.warnings[0].text, invalid_path_warning(path));
        }
        // The fixture's optional Go comparison also verifies these changed
        // configs and repeated warning counts against an independent build.
        let independent = fixture.build("", HashMap::new());
        assert_eq!(rebuilt.warnings.len(), independent.warnings.len());
        assert_eq!(
            rebuilt.output_files[0].contents,
            independent.output_files[0].contents
        );
    }
    context.dispose();
}

#[test]
fn tsconfig_package_exports_use_require_for_root_exact_and_pattern_subpaths() {
    for (extends, package) in [
        (
            "@scope/config",
            r#"{"exports":{"import":"./import.json","require":"./require.json","default":"./default.json"}}"#,
        ),
        (
            "@scope/config/exact",
            r#"{"exports":{"./exact":{"import":"./import.json","require":"./require.json","default":"./default.json"}}}"#,
        ),
        (
            "@scope/config/a/b/c.json",
            r#"{"exports":{"./*":{"import":"./import.json","require":"./require.json","default":"./default.json"}}}"#,
        ),
    ] {
        let config = format!(r#"{{"extends":{extends:?}}}"#);
        let fixture = ResolverFixture::new(&[
            (
                "entry.ts",
                "import { value } from 'alias'; console.log(value);",
            ),
            ("tsconfig.json", &config),
            ("node_modules/@scope/config/package.json", package),
            ("node_modules/@scope/config/import.json", "FAILURE"),
            ("node_modules/@scope/config/default.json", "FAILURE"),
            (
                "node_modules/@scope/config/require.json",
                r#"{"compilerOptions":{"paths":{"alias":["./selected.ts"],"unused":["bad.ts"]}}}"#,
            ),
            (
                "node_modules/@scope/config/selected.ts",
                "export const value = 'require config';",
            ),
        ]);
        // A completed root is cached within one scan, but each new build
        // validates inherited paths again and reports the base source.
        for _ in 0..2 {
            let result = fixture.build("", HashMap::new());
            assert!(output(&result).contains("require config"));
            assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
            assert_eq!(result.warnings[0].text, invalid_path_warning("bad.ts"));
            assert_eq!(
                result.warnings[0].location.as_ref().unwrap().file,
                "node_modules/@scope/config/require.json"
            );
        }
    }
}

#[test]
fn skipped_pnp_tsconfig_exports_use_node_modules_require_conditions() {
    let fixture = ResolverFixture::new(&[
        (
            "entry.ts",
            "import { value } from 'alias'; console.log(value);",
        ),
        ("tsconfig.json", r#"{"extends":"config/base"}"#),
        (
            ".pnp.data.json",
            r#"{"ignorePatternData":".","packageRegistryData":[]}"#,
        ),
        (
            "node_modules/config/package.json",
            r#"{"exports":{"./base":{"import":"./import.json","require":"./require.json","default":"./default.json"}}}"#,
        ),
        ("node_modules/config/import.json", "FAILURE"),
        ("node_modules/config/default.json", "FAILURE"),
        (
            "node_modules/config/require.json",
            r#"{"compilerOptions":{"paths":{"alias":["./selected.ts"]}}}"#,
        ),
        (
            "node_modules/config/selected.ts",
            "export const value = 'node_modules require config';",
        ),
    ]);
    let result = fixture.build("", HashMap::new());
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert!(output(&result).contains("node_modules require config"));
}

#[test]
fn resolved_pnp_tsconfig_exports_preserve_import_conditions() {
    let fixture = ResolverFixture::new(&[
        (
            "entry.ts",
            "import { value } from 'alias'; console.log(value);",
        ),
        ("tsconfig.json", r#"{"extends":"config/base"}"#),
        (
            ".pnp.data.json",
            r#"{"packageRegistryData":[[null,[[null,{"packageLocation":"./","packageDependencies":[["config","workspace:config"]],"linkType":"SOFT"}]]],["config",[["workspace:config",{"packageLocation":"./configs/","packageDependencies":[],"linkType":"SOFT"}]]]]}"#,
        ),
        (
            "configs/package.json",
            r#"{"exports":{"./base":{"require":"./require.json","import":"./import.json","default":"./default.json"}}}"#,
        ),
        ("configs/require.json", "FAILURE"),
        ("configs/default.json", "FAILURE"),
        (
            "configs/import.json",
            r#"{"compilerOptions":{"paths":{"alias":["./selected.ts"]}}}"#,
        ),
        (
            "configs/selected.ts",
            "export const value = 'PnP import config';",
        ),
    ]);
    let result = fixture.build("", HashMap::new());
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert!(output(&result).contains("PnP import config"));
}
