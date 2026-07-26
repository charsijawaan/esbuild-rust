//! Port of esbuild's public `pkg/api` package.

use std::{collections::HashMap, path::Path as FsPath, sync::Arc};

use crate::internal::{
    ast::SymbolMap,
    css_parser, css_printer,
    helpers::{encode_string_as_shortest_data_url, mime_type_by_extension, string_to_utf16},
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
    let sourcefile = if options.sourcefile.is_empty() {
        "<stdin>".to_string()
    } else {
        options.sourcefile.clone()
    };
    let source = Source {
        pretty_paths: PrettyPaths {
            abs: sourcefile.clone(),
            rel: sourcefile.clone(),
        },
        identifier_name: sourcefile,
        contents: Arc::from(input.as_ref()),
        ..Source::default()
    };

    let mut code = match options.loader {
        Loader::Css | Loader::GlobalCss | Loader::LocalCss => transform_css(&log, source, &options),
        Loader::Js | Loader::Jsx | Loader::Ts | Loader::Tsx | Loader::Default | Loader::None => {
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
    let tree = css_parser::parse(
        log.clone(),
        source,
        css_parser::Options {
            minify_syntax: options.minify_syntax,
            minify_whitespace: options.minify_whitespace,
            minify_identifiers: options.minify_identifiers,
        },
    );
    let mut symbols = SymbolMap::new(1);
    symbols.symbols_for_source[0].clone_from(&tree.symbols);
    css_printer::print(
        &tree,
        &symbols,
        css_printer::Options {
            line_limit: options.line_limit,
            minify_whitespace: options.minify_whitespace,
            ascii_only: options.ascii_only,
            ..css_printer::Options::default()
        },
    )
    .css
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
             Color = ((Color) => {\n\
             \x20\x20Color[Color[\"Red\"] = 0] = \"Red\";\n\
             \x20\x20Color[\"Blue\"] = \"blue\";\n\
             \x20\x20return Color;\n\
             })(Color || {});\n\
             const red = 0 /* Red */;\n"
        );
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
            ".card{color:red;margin:0!important}"
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
