use std::{collections::HashMap, sync::Arc};

use esbuild_rs::internal::{
    ast::SymbolMap,
    css_ast::Ast,
    css_parser::{self, Options, SymbolMode},
    css_printer,
    logger::{DeferLogKind, Log, PrettyPaths, Source},
};

fn parse_module(source: &str, symbol_mode: SymbolMode) -> (Ast, String) {
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
            symbol_mode,
            ..Options::default()
        },
    );
    assert!(log.done().is_empty(), "{source}: unexpected diagnostics");
    let mut symbols = SymbolMap::new(1);
    symbols.symbols_for_source[0].clone_from(&tree.symbols);
    let css =
        String::from_utf8(css_printer::print(&tree, &symbols, css_printer::Options::default()).css)
            .unwrap();
    (tree, css)
}

#[test]
fn module_annotation_scope_is_contained_and_nested_declarations_keep_their_order() {
    let (tree, css) = parse_module(
        ".outer { before: 1; :global { animation-name: shared; .global { color: red } } after: 2; } \
         .after { animation-name: own; } \
         :is(:global .inside).outside { color: blue } \
         :GLOBAL .upper { color: green }",
        SymbolMode::Local,
    );
    for name in ["outer", "after", "own", "outside", "upper"] {
        assert!(tree.local_scope.contains_key(name), "missing local {name}");
    }
    for name in ["shared", "global", "inside"] {
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
        ".outer {\n  before: 1;\n  animation-name: shared;\n  .global {\n    color: red;\n  }\n  after: 2;\n}\n.after {\n  animation-name: own;\n}\n:is(.inside).outside {\n  color: blue;\n}\n:GLOBAL .upper {\n  color: green;\n}\n"
    );
}

#[test]
fn ordinary_css_preserves_annotations_without_creating_module_exports() {
    let (tree, css) = parse_module(
        ":local(.a .b) { color: red } :global { .c { color: blue } }",
        SymbolMode::Disabled,
    );
    assert!(tree.local_symbols.is_empty());
    assert!(tree.local_scope.is_empty());
    assert_eq!(
        css,
        ":local(.a .b) {\n  color: red;\n}\n:global {\n  .c {\n    color: blue;\n  }\n}\n"
    );
}
