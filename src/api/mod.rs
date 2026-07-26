//! Port of esbuild's public `pkg/api` package.

use std::{
    collections::{HashMap, HashSet},
    path::Path as FsPath,
    sync::Arc,
};

use crate::internal::{
    ast::{DEFAULT_NAME_MINIFIER_CSS, Ref, SymbolKind, SymbolMap},
    css_parser, css_printer,
    helpers::{encode_string_as_shortest_data_url, mime_type_by_extension, string_to_utf16},
    js_ast::generate_non_unique_name_from_path,
    js_parser, js_printer,
    logger::{DeferLogKind, Log, Msg, MsgKind, PrettyPaths, Source},
    renamer::new_no_op_renamer,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u16)]
pub enum Loader {
    #[default]
    None,
    Base64,
    Binary,
    Copy,
    Css,
    DataUrl,
    Default,
    Empty,
    File,
    GlobalCss,
    Js,
    Json,
    Jsx,
    LocalCss,
    Text,
    Ts,
    Tsx,
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct TransformOptions {
    pub sourcefile: String,
    pub loader: Loader,
    pub banner: String,
    pub footer: String,
    pub line_limit: usize,
    pub minify_whitespace: bool,
    pub minify_identifiers: bool,
    pub minify_syntax: bool,
    pub ascii_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageKind {
    Error,
    Warning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub text: String,
    pub kind: MessageKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TransformResult {
    pub errors: Vec<Message>,
    pub warnings: Vec<Message>,
    pub code: Vec<u8>,
    pub map: Vec<u8>,
    pub legal_comments: Vec<u8>,
}

#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn transform(input: impl AsRef<[u8]>, options: TransformOptions) -> TransformResult {
    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let mut options = options;
    let sourcefile = if options.sourcefile.is_empty() {
        "<stdin>".to_string()
    } else {
        options.sourcefile.clone()
    };
    if options.loader == Loader::Default {
        let Some(loader) = default_loader_for_sourcefile(&sourcefile) else {
            return TransformResult {
                errors: vec![Message {
                    text: format!("Do not know how to load path: {sourcefile}"),
                    kind: MessageKind::Error,
                }],
                ..TransformResult::default()
            };
        };
        options.loader = loader;
    }
    let source = Source {
        pretty_paths: PrettyPaths {
            abs: sourcefile.clone(),
            rel: sourcefile.clone(),
        },
        identifier_name: generate_non_unique_name_from_path(&sourcefile),
        contents: Arc::from(input.as_ref()),
        ..Source::default()
    };

    let mut code = match options.loader {
        Loader::Css | Loader::GlobalCss | Loader::LocalCss => transform_css(&log, source, &options),
        Loader::Js | Loader::Jsx | Loader::Ts | Loader::Tsx | Loader::None => {
            transform_javascript(&log, source, &options)
        }
        Loader::Json => transform_json(&log, source, &options),
        Loader::Text => transform_text(&source, &options),
        Loader::Base64 => transform_base64(&source, &options),
        Loader::Binary => transform_binary(&source, &options),
        Loader::DataUrl => transform_data_url(&source, &options),
        Loader::Empty => Vec::new(),
        loader => {
            let message = format!("Transform loader {loader:?} is not implemented yet");
            return TransformResult {
                errors: vec![Message {
                    text: message,
                    kind: MessageKind::Error,
                }],
                ..TransformResult::default()
            };
        }
    };

    let messages = log.done();
    let (errors, warnings) = public_messages(messages);
    if errors.is_empty() {
        code = add_banner_and_footer(code, &options.banner, &options.footer);
    } else {
        code.clear();
    }
    TransformResult {
        errors,
        warnings,
        code,
        ..TransformResult::default()
    }
}

fn default_loader_for_sourcefile(sourcefile: &str) -> Option<Loader> {
    let file_name = FsPath::new(sourcefile)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(sourcefile);
    if file_name.ends_with(".module.css") {
        return Some(Loader::LocalCss);
    }
    match FsPath::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        None | Some("js" | "mjs" | "cjs") => Some(Loader::Js),
        Some("jsx") => Some(Loader::Jsx),
        Some("ts" | "cts" | "mts") => Some(Loader::Ts),
        Some("tsx") => Some(Loader::Tsx),
        Some("css") => Some(Loader::Css),
        Some("json") => Some(Loader::Json),
        Some("txt") => Some(Loader::Text),
        Some(_) => None,
    }
}

fn transform_json(log: &Log, source: Source, options: &TransformOptions) -> Vec<u8> {
    let (expression, ok) =
        js_parser::parse_json(log.clone(), source, js_parser::JsonOptions::default());
    if !ok {
        return Vec::new();
    }
    let renamer = new_no_op_renamer(SymbolMap::new(1));
    let value = js_printer::print_expr(&expression, &renamer, js_printer_options(options));
    export_default(value, options.minify_whitespace)
}

fn transform_text(source: &Source, options: &TransformOptions) -> Vec<u8> {
    let contents = source
        .contents
        .strip_prefix(&[0xef, 0xbb, 0xbf])
        .unwrap_or(&source.contents);
    export_string(contents, options)
}

fn transform_base64(source: &Source, options: &TransformOptions) -> Vec<u8> {
    export_string(STANDARD.encode(&source.contents).as_bytes(), options)
}

fn transform_binary(source: &Source, options: &TransformOptions) -> Vec<u8> {
    let encoded = STANDARD.encode(&source.contents);
    let mut value = b"Uint8Array.fromBase64(".to_vec();
    value.extend(js_printer::quote_utf16(
        &string_to_utf16(encoded.as_bytes()),
        js_printer_options(options),
        true,
    ));
    value.push(b')');
    export_default(value, options.minify_whitespace)
}

fn transform_data_url(source: &Source, options: &TransformOptions) -> Vec<u8> {
    let mime_type = guess_mime_type(&source.pretty_paths.abs, &source.contents);
    let url = encode_string_as_shortest_data_url(&mime_type, &source.contents);
    export_string(url.as_bytes(), options)
}

fn export_string(value: &[u8], options: &TransformOptions) -> Vec<u8> {
    let quoted =
        js_printer::quote_utf16(&string_to_utf16(value), js_printer_options(options), true);
    export_default(quoted, options.minify_whitespace)
}

fn export_default(mut value: Vec<u8>, minify_whitespace: bool) -> Vec<u8> {
    let mut code = if minify_whitespace {
        b"module.exports=".to_vec()
    } else {
        b"module.exports = ".to_vec()
    };
    code.append(&mut value);
    code.extend_from_slice(b";\n");
    code
}

fn js_printer_options(options: &TransformOptions) -> js_printer::Options {
    js_printer::Options {
        line_limit: options.line_limit,
        minify_syntax: options.minify_syntax,
        minify_whitespace: options.minify_whitespace,
        ascii_only: options.ascii_only,
        ..js_printer::Options::default()
    }
}

fn guess_mime_type(sourcefile: &str, contents: &[u8]) -> String {
    let extension = FsPath::new(sourcefile)
        .extension()
        .and_then(|extension| extension.to_str())
        .map_or_else(String::new, |extension| format!(".{extension}"));
    let known = mime_type_by_extension(&extension);
    let mime_type = if known.is_empty() {
        detect_content_type(contents)
    } else {
        known
    };
    mime_type.replace("; ", ";")
}

fn detect_content_type(contents: &[u8]) -> &'static str {
    if contents.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if contents.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if contents.starts_with(b"GIF87a") || contents.starts_with(b"GIF89a") {
        "image/gif"
    } else if contents.starts_with(b"%PDF-") {
        "application/pdf"
    } else if contents.starts_with(b"\0asm") {
        "application/wasm"
    } else if contents.starts_with(b"PK\x03\x04") {
        "application/zip"
    } else if std::str::from_utf8(contents).is_ok()
        && !contents
            .iter()
            .any(|byte| *byte < 0x20 && !matches!(*byte, b'\t' | b'\n' | b'\r' | b'\x0c'))
    {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

fn transform_javascript(log: &Log, source: Source, options: &TransformOptions) -> Vec<u8> {
    let mut parser_options = js_parser::Options::default();
    parser_options.ts.parse = matches!(options.loader, Loader::Ts | Loader::Tsx);
    parser_options.jsx.parse = matches!(options.loader, Loader::Jsx | Loader::Tsx);
    parser_options.minify_syntax = options.minify_syntax;
    parser_options.minify_identifiers = options.minify_identifiers;
    parser_options.minify_whitespace = options.minify_whitespace;
    parser_options.ascii_only = options.ascii_only;
    let (ast, ok) = js_parser::parse(log.clone(), source, parser_options);
    if !ok {
        return Vec::new();
    }
    let mut symbols = SymbolMap::new(1);
    symbols.symbols_for_source[0].clone_from(&ast.symbols);
    let renamer = new_no_op_renamer(symbols);
    js_printer::print(&ast, &renamer, js_printer_options(options)).js
}

fn transform_css(log: &Log, source: Source, options: &TransformOptions) -> Vec<u8> {
    let identifier_name = source.identifier_name.clone();
    let tree = css_parser::parse(
        log.clone(),
        source,
        css_parser::Options {
            minify_syntax: options.minify_syntax,
            minify_whitespace: options.minify_whitespace,
            minify_identifiers: options.minify_identifiers,
            symbol_mode: match options.loader {
                Loader::LocalCss => css_parser::SymbolMode::Local,
                Loader::GlobalCss => css_parser::SymbolMode::Global,
                _ => css_parser::SymbolMode::Disabled,
            },
        },
    );
    let mut symbols = SymbolMap::new(1);
    symbols.symbols_for_source[0].clone_from(&tree.symbols);
    let local_names = if options.loader == Loader::LocalCss {
        local_css_names(
            &tree,
            &symbols,
            &identifier_name,
            options.minify_identifiers,
        )
    } else {
        HashMap::new()
    };
    let mut css = css_printer::print(
        &tree,
        &symbols,
        css_printer::Options {
            local_names,
            line_limit: options.line_limit,
            minify_whitespace: options.minify_whitespace,
            ascii_only: options.ascii_only,
            ..css_printer::Options::default()
        },
    )
    .css;
    if !css.is_empty() && css.last() != Some(&b'\n') {
        css.push(b'\n');
    }
    css
}

fn local_css_names(
    tree: &crate::internal::css_ast::Ast,
    symbols: &SymbolMap,
    identifier_name: &str,
    minify_identifiers: bool,
) -> HashMap<Ref, String> {
    let global_names = tree
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::GlobalCss)
        .map(|symbol| symbol.original_name.clone())
        .collect::<HashSet<_>>();
    let mut references = tree
        .local_symbols
        .iter()
        .map(|loc_ref| loc_ref.reference)
        .collect::<Vec<_>>();
    references.sort_by(|left, right| {
        symbols
            .get(*right)
            .use_count_estimate
            .cmp(&symbols.get(*left).use_count_estimate)
            .then_with(|| left.source_index.cmp(&right.source_index))
            .then_with(|| left.inner_index.cmp(&right.inner_index))
    });

    let mut names = HashMap::new();
    let mut used_names = HashSet::new();
    if minify_identifiers {
        let minifier =
            DEFAULT_NAME_MINIFIER_CSS.shuffle_by_char_freq(tree.char_freq.unwrap_or_default());
        let mut next_name = 0;
        for reference in references {
            let mut name = minifier.number_to_minified_name(next_name);
            while global_names.contains(&name) || used_names.contains(&name) {
                next_name += 1;
                name = minifier.number_to_minified_name(next_name);
            }
            used_names.insert(name.clone());
            names.insert(reference, name);
        }
    } else {
        let mut name_counts = HashMap::<String, u32>::new();
        for reference in references {
            let symbol = symbols.get(reference);
            let mut name = format!("{identifier_name}_{}", symbol.original_name);
            if global_names.contains(&name) || used_names.contains(&name) {
                let prefix = name;
                let tries = name_counts.entry(prefix.clone()).or_insert(1);
                loop {
                    *tries += 1;
                    name = format!("{prefix}{tries}");
                    if !global_names.contains(&name) && !used_names.contains(&name) {
                        break;
                    }
                }
            }
            used_names.insert(name.clone());
            names.insert(reference, name);
        }
    }
    names
}

fn public_messages(messages: Vec<Msg>) -> (Vec<Message>, Vec<Message>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for message in messages {
        let public = Message {
            text: message.data.text,
            kind: if message.kind == MsgKind::Error {
                MessageKind::Error
            } else {
                MessageKind::Warning
            },
        };
        if public.kind == MessageKind::Error {
            errors.push(public);
        } else if message.kind == MsgKind::Warning {
            warnings.push(public);
        }
    }
    (errors, warnings)
}

fn add_banner_and_footer(mut code: Vec<u8>, banner: &str, footer: &str) -> Vec<u8> {
    if !banner.is_empty() {
        let mut with_banner = Vec::with_capacity(banner.len() + 1 + code.len());
        with_banner.extend_from_slice(banner.as_bytes());
        with_banner.push(b'\n');
        with_banner.append(&mut code);
        code = with_banner;
    }
    if !footer.is_empty() {
        if !code.is_empty() && code.last() != Some(&b'\n') {
            code.push(b'\n');
        }
        code.extend_from_slice(footer.as_bytes());
        code.push(b'\n');
    }
    code
}

#[cfg(test)]
mod tests {
    use super::{Loader, TransformOptions, transform};

    fn code(result: super::TransformResult) -> String {
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        String::from_utf8(result.code).expect("transform output is UTF-8")
    }

    #[test]
    fn transforms_type_script_to_javascript() {
        assert_eq!(
            code(transform(
                "const value: number = 1;",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "const value = 1;\n"
        );
    }

    #[test]
    fn annotates_generated_jsx_calls_as_pure() {
        assert_eq!(
            code(transform(
                "const element = <div/>",
                TransformOptions {
                    loader: Loader::Jsx,
                    ..TransformOptions::default()
                }
            )),
            "const element = /* @__PURE__ */ React.createElement(\"div\", null);\n"
        );
        assert_eq!(
            code(transform(
                "const element = <div/>",
                TransformOptions {
                    loader: Loader::Jsx,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "const element=React.createElement(\"div\",null);"
        );
    }

    #[test]
    fn preserves_user_authored_pure_annotations() {
        assert_eq!(
            code(transform(
                "const call = /* @__PURE__ */ factory();\
                 const instance = /* #__PURE__ */ new Factory();",
                TransformOptions {
                    loader: Loader::Js,
                    ..TransformOptions::default()
                }
            )),
            "const call = /* @__PURE__ */ factory();\n\
             const instance = /* @__PURE__ */ new Factory();\n"
        );
    }

    #[test]
    fn transforms_type_script_enums_through_lowering() {
        assert_eq!(
            code(transform(
                "enum Color { Red, Blue = 'blue' } const red = Color.Red;",
                TransformOptions {
                    loader: Loader::Ts,
                    ..TransformOptions::default()
                }
            )),
            "var Color;\n\
             Color = /* @__PURE__ */ ((Color) => {\n\
             \x20\x20Color[Color[\"Red\"] = 0] = \"Red\";\n\
             \x20\x20Color[\"Blue\"] = \"blue\";\n\
             \x20\x20return Color;\n\
             })(Color || {});\n\
             const red = 0 /* Red */;\n"
        );
        let impure = code(transform(
            "enum Value { Item = sideEffect() }",
            TransformOptions {
                loader: Loader::Ts,
                ..TransformOptions::default()
            },
        ));
        assert!(impure.contains("Value = ((Value) =>"));
        assert!(!impure.contains("@__PURE__"));
    }

    #[test]
    fn transforms_and_minifies_css() {
        assert_eq!(
            code(transform(
                ".card { color: red; margin: 0 !important }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            ".card{color:red;margin:0!important}\n"
        );
    }

    #[test]
    fn minifies_core_css_syntax() {
        assert_eq!(
            code(transform(
                ".a {} .b { color: blue }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            ".b{color:#00f}\n"
        );
        assert_eq!(
            code(transform(
                "@keyframes fade { from { opacity: 0 } to { opacity: 1 } } .fade { animation: fade 1s }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "@keyframes fade{0%{opacity:0}to{opacity:1}}.fade{animation:fade 1s}\n"
        );
        assert_eq!(
            code(transform(
                ".a { width: calc(1px + 2px); height: calc(2 * 3px); opacity: calc(1 / 2); margin: calc(2px * 3 + 4px * 5); padding: calc((2px + 3px) * 4); inset: calc(100% / 8); top: calc(100% / 3) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            ".a{width:3px;height:6px;opacity:.5;margin:26px;padding:20px;inset:12.5%;top:calc(100% / 3)}\n"
        );
    }

    #[test]
    fn minifies_symbolic_calc_expressions() {
        assert_eq!(
            code(transform(
                "a { one: calc(x + -1); two: calc(x - -1); three: calc(1px - x + 2px); four: calc(1px - var(x) + 2px); five: calc(x * .25); six: calc(x / .25); seven: calc((a + b) + c); eight: calc(a + (b + c)); nine: calc(2px * 3 + x); ten: calc(x * var(y)) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{one:calc(x - 1);two:calc(x + 1);three:calc(3px - x);four:calc(1px - var(x) + 2px);five:calc(x/4);six:calc(x*4);seven:calc(a + b + c);eight:calc(a + b + c);nine:calc(6px + x);ten:calc(x * var(y))}\n"
        );
    }

    #[test]
    fn minifies_grouped_calc_expressions() {
        assert_eq!(
            code(transform(
                "a { one: calc((a * b) * c); two: calc(a * (b * c)); three: calc((a / b) / c); four: calc(a / (b / c)); five: calc(a * (b / c)); six: calc(a / (b * c)); seven: calc(3 * (2px + 1em / 8)); eight: calc(3 * (2px + 1em / 7)); nine: calc((2px * 3) + x) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{one:calc(a*b*c);two:calc(a*b*c);three:calc(a/b/c);four:calc(a/(b/c));five:calc(a*b/c);six:calc(a/(b*c));seven:calc(3*(2px + .125em));eight:calc(3 * (2px + 1em / 7));nine:calc(6px + x)}\n"
        );
    }

    #[test]
    fn minifies_css_declarations() {
        assert_eq!(
            code(transform(
                "a { padding: 1px 1px 1px 1px; margin: 1px 2px 1px 2px; inset: 1px 2px 1px; color: rgb(300, 0, 0); background: rgba(0, 0, 255, 1); background-color: #FFFFFFFF; outline-color: #ff0000; caret-color: rgb(255, 0, 0); font-weight: normal; opacity: 0.5000; border-color: red red red red; scroll-margin: 1px 2px 1px 2px } b { color: rgba(255, 0, 0, 50%); background: rgb(50%, 25%, 0%) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{padding:1px;margin:1px 2px;inset:1px 2px;color:red;background:#00f;background-color:#fff;outline-color:red;caret-color:red;font-weight:400;opacity:.5;border-color:red red red red;scroll-margin:1px 2px 1px 2px}b{color:#ff00007f;background:#7f4000}\n"
        );
    }

    #[test]
    fn minifies_css_transforms() {
        assert_eq!(
            code(transform(
                "a { transform: translate(0px, 0em) translate(0px, 2px) translateX(0%) scale(2, 2) scale(2, 1) scale(1, 3) scale(50%) rotateZ(0deg) skewX(0rad) skew(2deg, 0turn) matrix(2, 0, 0, 2, 0, 0) translate3d(0px, 0%, 2px) scale3d(1, 1, 50%) rotate3d(1, 0, 0, 0deg) }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{transform:translate(0) translateY(2px) translate(0) scale(2) scaleX(2) scaleY(3) scale(.5) rotate(0) skew(0) skew(2deg) scale(2) translateZ(2px) scaleZ(.5) rotateX(0)}\n"
        );
    }

    #[test]
    fn minifies_named_css_colors() {
        assert_eq!(
            code(transform(
                "a { color: aliceblue; background-color: rebeccapurple; outline-color: darkslategrey; caret-color: navy; fill: fuchsia; stroke: transparent }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "a{color:#f0f8ff;background-color:#639;outline-color:#2f4f4f;caret-color:navy;fill:#f0f;stroke:transparent}\n"
        );
    }

    #[test]
    fn merges_safe_adjacent_css_selector_rules() {
        assert_eq!(
            code(transform(
                ".a { color: red } .b { color: red } .b { color: red } a:focus { color: red } b:focus { color: red } div { margin: 0 } span { margin: 0 } article { padding: 0 } section { padding: 0 }",
                TransformOptions {
                    loader: Loader::Css,
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            ".a,.b{color:red}a:focus{color:red}b:focus{color:red}div,span{margin:0}article{padding:0}section{padding:0}\n"
        );
    }

    #[test]
    fn scopes_and_minifies_local_css_names() {
        let input = ".card { color: red } #root .card { color: blue }";
        assert_eq!(
            code(transform(
                input,
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    ..TransformOptions::default()
                }
            )),
            ".entry_card {\n\
             \x20\x20color: red;\n\
             }\n\
             #entry_root .entry_card {\n\
             \x20\x20color: blue;\n\
             }\n"
        );
        assert_eq!(
            code(transform(
                input,
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    minify_identifiers: true,
                    ..TransformOptions::default()
                }
            )),
            ".o {\n\
             \x20\x20color: red;\n\
             }\n\
             #l .o {\n\
             \x20\x20color: blue;\n\
             }\n"
        );
    }

    #[test]
    fn scopes_local_css_animation_and_explicit_selector_modes() {
        assert_eq!(
            code(transform(
                "@keyframes fade { from { opacity: 0 } }\
                 .fade { animation: fade 1s }\
                 :global(.external) .local, :local(.forced) {}",
                TransformOptions {
                    sourcefile: "entry.module.css".into(),
                    loader: Loader::LocalCss,
                    ..TransformOptions::default()
                }
            )),
            "@keyframes entry_fade {\n\
             \x20\x20from {\n\
             \x20\x20\x20\x20opacity: 0;\n\
             \x20\x20}\n\
             }\n\
             .entry_fade {\n\
             \x20\x20animation: entry_fade 1s;\n\
             }\n\
             .external .entry_local,\n\
             .entry_forced {\n\
             }\n"
        );
    }

    #[test]
    fn transforms_json_into_a_common_js_export() {
        assert_eq!(
            code(transform(
                r#"{"answer": 42, "invalid-identifier": true}"#,
                TransformOptions {
                    loader: Loader::Json,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = { answer: 42, \"invalid-identifier\": true };\n"
        );
    }

    #[test]
    fn transforms_text_base64_and_binary_loaders() {
        assert_eq!(
            code(transform(
                b"\xef\xbb\xbfhello",
                TransformOptions {
                    loader: Loader::Text,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = \"hello\";\n"
        );
        assert_eq!(
            code(transform(
                "hello",
                TransformOptions {
                    loader: Loader::Base64,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = \"aGVsbG8=\";\n"
        );
        assert_eq!(
            code(transform(
                "hello",
                TransformOptions {
                    loader: Loader::Binary,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = Uint8Array.fromBase64(\"aGVsbG8=\");\n"
        );
    }

    #[test]
    fn transforms_data_urls_and_empty_input() {
        assert_eq!(
            code(transform(
                "<svg></svg>",
                TransformOptions {
                    sourcefile: "icon.svg".into(),
                    loader: Loader::DataUrl,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = \"data:image/svg+xml,<svg></svg>\";\n"
        );
        assert_eq!(
            code(transform(
                [0xff],
                TransformOptions {
                    loader: Loader::DataUrl,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = \"data:application/octet-stream;base64,/w==\";\n"
        );
        assert_eq!(
            code(transform(
                "ignored",
                TransformOptions {
                    loader: Loader::Empty,
                    ..TransformOptions::default()
                }
            )),
            ""
        );
    }

    #[test]
    fn minifies_data_loader_exports() {
        assert_eq!(
            code(transform(
                r#"{"x": 1}"#,
                TransformOptions {
                    loader: Loader::Json,
                    minify_whitespace: true,
                    ..TransformOptions::default()
                }
            )),
            "module.exports={x:1};\n"
        );
    }

    #[test]
    fn default_loader_uses_the_sourcefile_extension() {
        assert_eq!(
            code(transform(
                "const value: number = 1",
                TransformOptions {
                    sourcefile: "entry.ts".into(),
                    loader: Loader::Default,
                    ..TransformOptions::default()
                }
            )),
            "const value = 1;\n"
        );
        assert_eq!(
            code(transform(
                r#"{"x": 1}"#,
                TransformOptions {
                    sourcefile: "entry.json".into(),
                    loader: Loader::Default,
                    ..TransformOptions::default()
                }
            )),
            "module.exports = { x: 1 };\n"
        );
        let result = transform(
            "data",
            TransformOptions {
                sourcefile: "entry.unknown".into(),
                loader: Loader::Default,
                ..TransformOptions::default()
            },
        );
        assert_eq!(
            result.errors.first().map(|message| message.text.as_str()),
            Some("Do not know how to load path: entry.unknown")
        );
    }

    #[test]
    fn returns_diagnostics_and_suppresses_code_on_errors() {
        let result = transform(
            "const = 1",
            TransformOptions {
                loader: Loader::Js,
                ..TransformOptions::default()
            },
        );
        assert!(!result.errors.is_empty());
        assert!(result.code.is_empty());
    }

    #[test]
    fn adds_transform_banners_and_footers() {
        assert_eq!(
            code(transform(
                "let x = 1",
                TransformOptions {
                    banner: "/* before */".into(),
                    footer: "/* after */".into(),
                    ..TransformOptions::default()
                }
            )),
            "/* before */\nlet x = 1;\n/* after */\n"
        );
    }
}
