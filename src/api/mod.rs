//! Port of esbuild's public `pkg/api` package.

use std::{collections::HashMap, sync::Arc};

use crate::internal::{
    ast::SymbolMap,
    css_parser, css_printer, js_parser, js_printer,
    logger::{DeferLogKind, Log, Msg, MsgKind, PrettyPaths, Source},
    renamer::new_no_op_renamer,
};

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
    js_printer::print(
        &ast,
        &renamer,
        js_printer::Options {
            line_limit: options.line_limit,
            minify_syntax: options.minify_syntax,
            minify_whitespace: options.minify_whitespace,
            ascii_only: options.ascii_only,
            ..js_printer::Options::default()
        },
    )
    .js
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
