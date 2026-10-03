use std::{
    collections::HashMap,
    io::Write,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use esbuild_rs::{
    api::{BuildSourceMap, Loader, TransformOptions, TransformResult, transform},
    internal::{
        ast::SymbolMap,
        config,
        css_ast::{
            Ast, Combinator, ComplexSelector, CompoundSelector, NameToken, NamespacedName, Rule,
            RuleData, SelectorRule,
        },
        css_lexer::TokenKind,
        css_parser, css_printer, js_parser,
        logger::{DeferLogKind, Loc, Log, Range, Source},
        sourcemap::{Mapping, SourceMap, generate_line_offset_tables},
    },
};
use serde_json::{Value, json};

static REPORT_LOCK: Mutex<()> = Mutex::new(());

fn parse_map(map: &Value) -> SourceMap {
    js_parser::parse_source_map(
        Log::new_defer(DeferLogKind::All, HashMap::new()),
        Source {
            contents: map.to_string().into_bytes().into(),
            ..Source::default()
        },
    )
    .unwrap()
}

fn generated_position(code: &str, offset: usize) -> (i32, i32) {
    let prefix = &code[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let column = prefix.rsplit('\n').next().unwrap().encode_utf16().count();
    (i32::try_from(line).unwrap(), i32::try_from(column).unwrap())
}

fn mapping_position(mapping: &Mapping) -> (i32, i32) {
    (mapping.generated_line, mapping.generated_column)
}

fn run_json(executable: &std::ffi::OsStr, args: &[&str], input: &Value) -> Value {
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(input).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn compare_go(input: &str, options: &TransformOptions, result: &TransformResult, origin: &str) {
    // These are native Go API comparisons, not original JavaScript API coverage.
    const SCRIPT: &str = r"
const {SourceMap} = require('node:module');
const data = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
const rust = new SourceMap(data.rust), go = new SourceMap(data.go);
let samples = 0, skippedCloseBraceSamples = 0, mismatches = [];
for (const [line, text] of data.code.split('\n').entries()) {
  for (let column = 0; column <= text.length; column++) {
    const a = rust.findEntry(line, column), b = go.findEntry(line, column);
    if (a.generatedLine !== line || a.generatedColumn !== column) continue;
    if (text.slice(column).trimStart().startsWith('}')) {
      skippedCloseBraceSamples++;
      continue;
    }
    for (const key of ['originalSource', 'originalLine', 'originalColumn', 'name']) {
      if (a[key] !== b[key]) mismatches.push({line, column, key, rust: a[key], go: b[key]});
    }
    samples++;
  }
}
let selectorSamples = 0;
for (const match of data.code.matchAll(/[^{}]+(?=\{)/g)) {
  const selector = match[0].trimStart();
  const start = match.index + match[0].length - selector.length;
  for (let offset = start; offset < start + selector.length; offset++) {
    const prefix = data.code.slice(0, offset), lines = prefix.split('\n');
    const line = lines.length - 1, column = lines.at(-1).length;
    const a = rust.findEntry(line, column), b = go.findEntry(line, column);
    for (const key of ['originalSource', 'originalLine', 'originalColumn', 'name']) {
      if (a[key] !== b[key]) mismatches.push({line, column, key, rust: a[key], go: b[key]});
    }
    selectorSamples++;
  }
}
console.log(JSON.stringify({samples, selectorSamples, skippedCloseBraceSamples, mismatches}));
";
    let Some(executable) = std::env::var_os("ESBUILD_RS_TEAM_GO_TRANSFORM") else {
        return;
    };
    let request = json!({"Input": input, "Options": {
        "Loader": 4, "Sourcefile": options.sourcefile, "Sourcemap": 3,
        "MinifyWhitespace": options.minify_whitespace,
    }});
    let go = run_json(&executable, &[], &request);
    assert_eq!(go["Errors"], Value::Null, "{request}");
    assert_eq!(go["Warnings"], Value::Null, "{request}");
    let code = String::from_utf8(result.code.clone()).unwrap();
    assert_eq!(go["Code"], code, "{request}");
    let mut rust_map: Value = serde_json::from_slice(&result.map).unwrap();
    let mut go_map: Value = serde_json::from_str(go["Map"].as_str().unwrap()).unwrap();
    let checks = run_json(
        std::ffi::OsStr::new("node"),
        &["-e", SCRIPT],
        &json!({"rust": rust_map, "go": go_map, "code": code}),
    );
    assert_eq!(checks["mismatches"], json!([]), "{request}\n{checks}");
    let rust_mappings = rust_map
        .as_object_mut()
        .unwrap()
        .remove("mappings")
        .unwrap();
    let go_mappings = go_map.as_object_mut().unwrap().remove("mappings").unwrap();
    // Rust's CSS parser currently leaves close-brace locations at zero. Go
    // emits those additional mappings; this bounded printer test does not
    // claim complete mapping-stream parity or change the parser.
    assert_eq!(rust_map, go_map, "{request}");
    if let Some(path) = std::env::var_os("ESBUILD_RS_TEAM_CSS_MAP_REPORT") {
        let _report_guard = REPORT_LOCK.lock().unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"origin": origin, "request": request, "code": code,
            "metadata": rust_map, "rustMappings": rust_mappings, "goMappings": go_mappings,
            "exactMapEqual": rust_mappings == go_mappings, "checks": checks})
        )
        .unwrap();
    }
}

fn transform_css(
    input: &str,
    minify: bool,
) -> (TransformOptions, TransformResult, Value, SourceMap) {
    let options = TransformOptions {
        loader: Loader::Css,
        sourcefile: "generated/input.css".into(),
        sourcemap: BuildSourceMap::External,
        minify_whitespace: minify,
        ..TransformOptions::default()
    };
    let result = transform(input, options.clone());
    assert!(
        result.errors.is_empty() && result.warnings.is_empty(),
        "{result:?}"
    );
    let map: Value = serde_json::from_slice(&result.map).unwrap();
    let parsed = parse_map(&map);
    (options, result, map, parsed)
}

#[test]
fn indexed_css_maps_do_not_map_an_unmapped_middle_rule() {
    for (css, final_line, first_mappings, last_mappings, minified) in [
        (
            ".first {color:red}\n.unmapped {color:blue}\n.final {color:green}",
            2,
            "AAUA",
            "AAoBA",
            true,
        ),
        (
            ".first {color:red}\n.unmapped.inner > .child + .leaf ~ .last {color:blue}\n.final {color:green}",
            2,
            "AAUA",
            "AAoBA",
            true,
        ),
        (
            "@media screen {\n.first {color:red}\n.unmapped > .child > .leaf {color:blue}\n.final {color:green}\n}",
            3,
            "AAUA;AACA",
            "AAoBA;AACA",
            true,
        ),
        (
            ".scope {\n& > .first {color:red}\n& > .unmapped > .leaf {color:blue}\n& + .final {color:green}\n}",
            3,
            "AAUA;AACA",
            "AAoBA;AACA",
            true,
        ),
        // The existing CSS parser merges descendant class selectors under
        // whitespace minification. Preserve this pretty-printer regression
        // without claiming that separate parser behavior matches Go.
        (
            ".first {color:red}\n.unmapped .child {color:blue}\n.final {color:green}",
            2,
            "AAUA",
            "AAoBA",
            false,
        ),
    ] {
        let map = json!({"version": 3, "sections": [
            {"offset": {"line": 0, "column": 0}, "map": {
                "version": 3, "sources": ["first.scss"], "sourcesContent": ["first"],
                "names": [], "mappings": first_mappings,
            }},
            {"offset": {"line": final_line, "column": 0}, "map": {
                "version": 3, "sourceRoot": "../original/", "sources": ["last.scss"],
                "sourcesContent": ["last"], "names": [], "mappings": last_mappings,
            }},
        ]});
        let input = format!(
            "{css}\n/*# sourceMappingURL=data:application/json;base64,{} */",
            STANDARD.encode(map.to_string())
        );
        for minify in [false, true] {
            if minify && !minified {
                continue;
            }
            let (options, result, map, parsed) = transform_css(&input, minify);
            assert_eq!(
                map["sources"],
                json!(["first.scss", "../original/last.scss"])
            );
            assert_eq!(map["sourcesContent"], json!(["first", "last"]));
            let code = String::from_utf8(result.code.clone()).unwrap();
            let start = code.find(".unmapped").unwrap();
            let end = start + code[start..].find('}').unwrap() + 1;
            let start_position = generated_position(&code, start);
            let end_position = generated_position(&code, end);
            assert!(
                parsed.mappings.iter().all(|mapping| {
                    mapping_position(mapping) < start_position
                        || mapping_position(mapping) >= end_position
                }),
                "phantom mapping in unmapped rule: {code}\n{map}"
            );
            if !minify {
                assert!(
                    parsed.find(start_position.0, start_position.1).is_none(),
                    "{code}\n{map}"
                );
            }
            // The first real token is at byte zero and must keep its mapping.
            assert!(
                parsed
                    .mappings
                    .iter()
                    .any(|mapping| mapping.source_index == 0)
            );
            assert!(
                parsed
                    .mappings
                    .iter()
                    .any(|mapping| mapping.source_index == 1)
            );
            compare_go(&input, &options, &result, "native-transform");
        }
    }
}

#[test]
fn real_css_combinators_map_after_preceding_whitespace() {
    for css in [
        ".a  > .b + .c ~ .d { color: red }",
        ".a > .b:is(.c > .d, .e + .f):has(> .g) { color: blue }",
        ".parent { & > .child { color: green } }",
    ] {
        for minify in [false, true] {
            let (options, result, _, parsed) = transform_css(css, minify);
            let code = String::from_utf8(result.code.clone()).unwrap();
            let original_operators: Vec<_> = css
                .char_indices()
                .filter(|(_, character)| matches!(character, '>' | '+' | '~'))
                .collect();
            let generated_operators: Vec<_> = code
                .char_indices()
                .filter(|(_, character)| matches!(character, '>' | '+' | '~'))
                .collect();
            assert_eq!(original_operators.len(), generated_operators.len());
            for ((original_offset, original), (generated_offset, generated)) in
                original_operators.iter().zip(&generated_operators)
            {
                assert_eq!(original, generated);
                let position = generated_position(&code, *generated_offset);
                let mapping = parsed
                    .mappings
                    .iter()
                    .find(|mapping| mapping_position(mapping) == position)
                    .unwrap_or_else(|| {
                        panic!("missing combinator mapping at {position:?}: {code}")
                    });
                assert_eq!(
                    (mapping.original_line, mapping.original_column),
                    generated_position(css, *original_offset)
                );
            }
            assert_eq!(parsed.find(0, 0).unwrap().original_column, 0);
            compare_go(css, &options, &result, "native-transform");
        }
    }
}

#[test]
fn byte_zero_is_valid_for_real_combinators_but_absent_tokens_do_not_map() {
    // A relative selector can have a real combinator at byte zero. Give byte
    // zero and the type name distinct input mappings to expose accidental
    // mapping of either an absent combinator or a synthesized closing brace.
    let input_map = Arc::new(parse_map(&json!({
        "version": 3, "sources": ["original.scss"], "names": [],
        "mappings": "AAOG,EACE",
    })));
    for byte in [0, b'>'] {
        let tree = Ast {
            rules: vec![Rule {
                loc: Loc::default(),
                data: RuleData::Selector(SelectorRule {
                    selectors: vec![ComplexSelector {
                        selectors: vec![CompoundSelector {
                            combinator: Combinator {
                                byte,
                                loc: Loc::default(),
                            },
                            type_selector: Some(NamespacedName {
                                name: NameToken {
                                    text: "item".into(),
                                    kind: TokenKind::Ident,
                                    range: Range {
                                        loc: Loc { start: 2 },
                                        len: 4,
                                    },
                                },
                                ..NamespacedName::default()
                            }),
                            ..CompoundSelector::default()
                        }],
                    }],
                    // Zero means there is no source closing brace in this AST.
                    close_brace_loc: Loc::default(),
                    ..SelectorRule::default()
                }),
            }],
            ..Ast::default()
        };
        let result = css_printer::print(
            &tree,
            &SymbolMap::default(),
            css_printer::Options {
                source_map: config::SourceMap::ExternalWithoutComment,
                add_source_mappings: true,
                input_source_map: Some(input_map.clone()),
                line_offset_tables: generate_line_offset_tables(b"> item", 1),
                ..css_printer::Options::default()
            },
        );
        assert_eq!(
            result.css,
            if byte == 0 {
                b"item {\n}\n".as_slice()
            } else {
                b"> item {\n}\n".as_slice()
            }
        );
        let parsed = parse_map(&json!({
            "version": 3, "sources": ["original.scss"], "names": [],
            "mappings": String::from_utf8(result.source_map_chunk.buffer.data).unwrap(),
        }));
        assert!(
            parsed
                .mappings
                .iter()
                .all(|mapping| mapping.generated_line == 0)
        );
        if byte == 0 {
            assert_eq!(parsed.mappings.len(), 1);
            assert_eq!(
                (
                    parsed.mappings[0].original_line,
                    parsed.mappings[0].original_column
                ),
                (8, 5)
            );
        } else {
            assert_eq!(parsed.mappings.len(), 2);
            assert_eq!(
                (
                    parsed.mappings[0].original_line,
                    parsed.mappings[0].original_column
                ),
                (7, 3)
            );
            assert_eq!(parsed.mappings[1].generated_column, 2);
        }
    }
}

#[test]
fn complete_css_ast_preserves_exact_go_printer_mapping_streams() {
    const CSS: &str = ".a > .b { color: red }";
    for (minify, expected) in [
        (false, "AAAA,CAAC,EAAE,EAAE,CAAC;AAAI,SAAO;AAAI;"),
        (true, "AAAA,CAAC,CAAE,CAAE,CAAC,EAAI,MAAO,GAAI"),
    ] {
        let source = Source {
            contents: CSS.as_bytes().into(),
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut tree = css_parser::parse(log.clone(), source, css_parser::Options::default());
        assert!(log.done().is_empty());
        let RuleData::Selector(rule) = &mut tree.rules[0].data else {
            panic!("selector AST");
        };
        // The parser's missing close-brace location is outside this patch.
        // Supply a complete AST to isolate the printer's exact Go contract.
        assert_eq!(rule.close_brace_loc, Loc::default());
        rule.close_brace_loc = Loc {
            start: i32::try_from(CSS.find('}').unwrap()).unwrap(),
        };
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = tree.symbols.clone();
        let mut printed = css_printer::print(
            &tree,
            &symbols,
            css_printer::Options {
                source_map: config::SourceMap::ExternalWithoutComment,
                add_source_mappings: true,
                minify_whitespace: minify,
                line_offset_tables: generate_line_offset_tables(CSS.as_bytes(), 1),
                ..css_printer::Options::default()
            },
        );
        let mappings = String::from_utf8(printed.source_map_chunk.buffer.data).unwrap();
        assert_eq!(mappings, expected);
        // Public transforms terminate minified CSS with a newline after the
        // printer returns its chunk. Mirror that wrapper for the Go comparison.
        if minify {
            printed.css.push(b'\n');
        }
        let map = json!({"version": 3, "sources": ["generated/input.css"],
            "sourcesContent": [CSS], "names": [], "mappings": mappings});
        let result = TransformResult {
            code: printed.css,
            map: serde_json::to_vec(&map).unwrap(),
            ..TransformResult::default()
        };
        compare_go(
            CSS,
            &TransformOptions {
                sourcefile: "generated/input.css".into(),
                minify_whitespace: minify,
                ..TransformOptions::default()
            },
            &result,
            "printer-complete-ast",
        );
    }
}
