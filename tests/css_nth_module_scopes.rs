use std::{collections::HashMap, sync::Arc};

use esbuild_rs::internal::{
    ast::SymbolMap,
    bundler::{EntryPoint, bundle_javascript},
    cache::CacheSet,
    compat::CssFeature,
    config::{Format, Loader, Mode, Options as BuildOptions},
    css_ast::{Ast, PseudoClassKind, RuleData, SubclassData},
    css_parser::{self, Options, SymbolMode},
    css_printer,
    fs::{MockKind, mock_fs},
    logger::{DeferLogKind, Log, PrettyPaths, Source},
};
use serde_json::{Value, json};

#[test]
fn nth_module_selectors_match_original_pinned_go_bundler_snapshot() {
    let corpus: Vec<Value> = serde_json::from_str(include_str!("upstream/bundler.json")).unwrap();
    let case = &corpus[16];
    assert_eq!(case["upstream_test"], "TestImportCSSFromJSNthIndexLocal");
    assert_eq!(case["file_system"], "unix");
    assert_eq!(
        case["options"],
        json!({
            "AbsOutputDir": "/out",
            "ExtensionToLoader": {".css": 14, ".js": 10},
            "Mode": 2,
            "UnsupportedCSSFeatures": "2048"
        })
    );
    assert!(case["entry_paths_advanced"].is_null());
    assert_eq!(case["expected_scan_log"], "");
    assert_eq!(case["expected_compile_log"], "");
    let files: HashMap<_, _> = case["files"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(path, contents)| (path.clone(), contents.as_str().unwrap().to_owned()))
        .collect();
    let fs = mock_fs(&files, MockKind::Unix, "/");
    let entries: Vec<_> = case["entry_paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| EntryPoint {
            input_path: path.as_str().unwrap().into(),
            ..EntryPoint::default()
        })
        .collect();
    let mut options = BuildOptions {
        mode: Mode::Bundle,
        output_format: Format::EsModule,
        tree_shaking: true,
        omit_runtime_for_tests: true,
        abs_output_dir: "/out".into(),
        unsupported_css_features: CssFeature::NESTING,
        extension_to_loader: HashMap::from([
            (".css".into(), Loader::LocalCss),
            (".js".into(), Loader::Js),
        ]),
        ..BuildOptions::default()
    };
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let result = bundle_javascript(
        &log,
        &fs,
        &CacheSet::default(),
        &entries,
        &mut options,
        "UPSTREAM_TEST",
    );
    assert!(log.done().is_empty());
    assert!(result.metafile.is_empty());
    let snapshot = result
        .output_files
        .iter()
        .map(|file| {
            format!(
                "---------- {} ----------\n{}",
                file.abs_path,
                String::from_utf8(file.contents.clone()).unwrap()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(snapshot, case["expected_snapshot"].as_str().unwrap());
}

fn parse_nth(source: &str, mode: SymbolMode, minify: bool) -> (Ast, String) {
    let log = Log::new_defer(DeferLogKind::All, HashMap::new());
    let tree = css_parser::parse(
        log.clone(),
        Source {
            pretty_paths: PrettyPaths {
                abs: "<stdin>".into(),
                rel: "<stdin>".into(),
            },
            contents: Arc::from(source.as_bytes()),
            ..Source::default()
        },
        Options {
            symbol_mode: mode,
            minify_syntax: minify,
            minify_whitespace: minify,
            ..Options::default()
        },
    );
    assert!(log.done().is_empty(), "{source}: unexpected diagnostics");
    let mut symbols = SymbolMap::new(1);
    symbols.symbols_for_source[0].clone_from(&tree.symbols);
    let local_names = tree
        .local_scope
        .iter()
        .map(|(name, entry)| (entry.reference, format!("renamed_{name}")))
        .collect();
    let css = String::from_utf8(
        css_printer::print(
            &tree,
            &symbols,
            css_printer::Options {
                minify_whitespace: minify,
                local_names,
                ..css_printer::Options::default()
            },
        )
        .css,
    )
    .unwrap();
    (tree, css)
}

#[test]
fn nth_of_module_scopes_are_contained_and_complex_annotations_are_flattened() {
    for kind in ["nth-child", "nth-last-child"] {
        let source = format!(
            ":{kind}(2n of .first :global .GLOBAL, .GLOBAL2 :local .last).after {{ animation-name: own }} \
             .sibling {{ color: red }}"
        );
        let (tree, css) = parse_nth(&source, SymbolMode::Local, false);
        for name in ["first", "last", "after", "own", "sibling"] {
            assert!(tree.local_scope.contains_key(name), "missing local {name}");
        }
        for name in ["GLOBAL", "GLOBAL2"] {
            assert!(
                tree.global_scope.contains_key(name),
                "missing global {name}"
            );
            assert!(
                !tree.local_scope.contains_key(name),
                "unexpected local {name}"
            );
        }
        assert_eq!(
            css,
            format!(
                ":{kind}(2n of .renamed_first .GLOBAL, .GLOBAL2 .renamed_last).renamed_after {{\n  animation-name: renamed_own;\n}}\n.renamed_sibling {{\n  color: red;\n}}\n"
            )
        );
        let (_, minified) = parse_nth(&source, SymbolMode::Local, true);
        assert_eq!(
            minified,
            format!(
                ":{kind}(2n of.renamed_first .GLOBAL,.GLOBAL2 .renamed_last).renamed_after{{animation-name:renamed_own}}.renamed_sibling{{color:red}}"
            )
        );
        let RuleData::Selector(rule) = &tree.rules[0].data else {
            panic!("selector rule")
        };
        let SubclassData::PseudoWithSelectorList(nth) =
            &rule.selectors[0].selectors[0].subclass_selectors[0].data
        else {
            panic!("nth pseudo-class must have selector AST")
        };
        assert_eq!(nth.index.a, "2");
        assert!(nth.index.b.is_empty());
        assert_eq!(nth.selectors.len(), 2);
        assert_eq!(
            nth.kind,
            if kind == "nth-child" {
                PseudoClassKind::NthChild
            } else {
                PseudoClassKind::NthLastChild
            }
        );

        let source = format!(
            ":{kind}(odd of div:local(.a > .b):hover, :global(.g + .h)).after {{ color: blue }}"
        );
        let (_, css) = parse_nth(&source, SymbolMode::Local, false);
        assert_eq!(
            css,
            format!(
                ":{kind}(odd of div.renamed_a > .renamed_b:hover, .g + .h).renamed_after {{\n  color: blue;\n}}\n"
            )
        );
    }
}

#[test]
fn nth_of_printing_renames_symbols_and_preserves_required_minified_spaces() {
    for kind in ["nth-child", "nth-last-child"] {
        for (first, printed) in [
            (".local", ".renamed_local"),
            ("#local", "#renamed_local"),
            ("[href]", "[href]"),
            (":hover", ":hover"),
            ("div.local", " div.renamed_local"),
            ("*", " *"),
        ] {
            let source = format!(":{kind}(even of {first}, :global(.g)) {{ color: red }}");
            let (_, css) = parse_nth(&source, SymbolMode::Local, true);
            assert_eq!(css, format!(":{kind}(2n of{printed},.g){{color:red}}"));
        }
    }
}

#[test]
fn nth_of_indices_preserve_explicit_zero_terms_and_match_go_minification() {
    for kind in ["nth-child", "nth-last-child"] {
        for (input, pretty, minified) in [
            ("0n+0", "0n+0", "0"),
            ("-n+0", "-n+0", "-n"),
            ("-0n-0", "-0n-0", "-0n-0"),
            ("+0002n+0001", "2n+1", "odd"),
            ("+0003", "3", "3"),
            ("-0000", "-0", "-0"),
        ] {
            let source = format!(":{kind}({input} of .local) {{ color: red }}");
            let (_, css) = parse_nth(&source, SymbolMode::Local, false);
            assert_eq!(
                css,
                format!(":{kind}({pretty} of .renamed_local) {{\n  color: red;\n}}\n")
            );
            let (_, css) = parse_nth(&source, SymbolMode::Local, true);
            assert_eq!(
                css,
                format!(":{kind}({minified} of.renamed_local){{color:red}}")
            );
        }
    }
}

#[test]
fn nth_of_plain_css_preserves_module_annotations_and_global_css_limits_exports() {
    let source = ":nth-child(2n of :local(.local), :global(.GLOBAL)).after { color: red }";
    let (plain, css) = parse_nth(source, SymbolMode::Disabled, false);
    assert!(plain.local_scope.is_empty());
    assert_eq!(
        css,
        ":nth-child(2n of :local(.local), :global(.GLOBAL)).after {\n  color: red;\n}\n"
    );
    let (global, css) = parse_nth(source, SymbolMode::Global, false);
    assert_eq!(global.local_scope.len(), 1);
    assert!(global.local_scope.contains_key("local"));
    assert!(global.global_scope.contains_key("GLOBAL"));
    assert!(global.global_scope.contains_key("after"));
    assert_eq!(
        css,
        ":nth-child(2n of .renamed_local, .GLOBAL).after {\n  color: red;\n}\n"
    );
}
