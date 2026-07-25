use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
};

use crate::internal::{
    compat::JsFeature,
    helpers::{is_inside_node_modules, utf16_to_string},
    js_ast::{
        ArrayExpr, Expr, ExprData, ObjectExpr, Property, PropertyFlags, PropertyKind, StringExpr,
    },
    js_lexer::{JsonFlavor, Lexer, LexerPanic, Token},
    logger::{LineColumnTracker, Log, MsgId, MsgKind, Range, Source},
};

#[derive(Clone, Debug)]
pub struct JsonOptions {
    pub unsupported_js_features: JsFeature,
    pub flavor: JsonFlavor,
    pub error_suffix: String,
    pub is_for_define: bool,
}

impl Default for JsonOptions {
    fn default() -> Self {
        Self {
            unsupported_js_features: JsFeature::NONE,
            flavor: JsonFlavor::Json,
            error_suffix: String::new(),
            is_for_define: false,
        }
    }
}

struct JsonParser {
    log: Log,
    tracker: LineColumnTracker,
    lexer: Lexer,
    options: JsonOptions,
    suppress_warnings_about_weird_code: bool,
}

impl JsonParser {
    fn parse_maybe_trailing_comma(&mut self, close_token: Token) -> bool {
        let comma_range = self.lexer.range();
        self.lexer.expect(Token::Comma);
        if self.lexer.token == close_token {
            if self.options.flavor == JsonFlavor::Json {
                self.log.add_error(
                    Some(&mut self.tracker),
                    comma_range,
                    "JSON does not support trailing commas",
                );
            }
            false
        } else {
            true
        }
    }

    #[allow(clippy::too_many_lines)]
    fn parse_expr(&mut self) -> Expr {
        let location = self.lexer.loc();
        match self.lexer.token {
            Token::False => {
                self.lexer.next();
                Expr::new(location, ExprData::Boolean(false))
            }
            Token::True => {
                self.lexer.next();
                Expr::new(location, ExprData::Boolean(true))
            }
            Token::Null => {
                self.lexer.next();
                Expr::new(location, ExprData::Null)
            }
            Token::StringLiteral => {
                let value = self.lexer.string_literal().to_vec();
                self.lexer.next();
                Expr::new(
                    location,
                    ExprData::String(StringExpr {
                        value,
                        ..StringExpr::default()
                    }),
                )
            }
            Token::NumericLiteral => {
                let value = self.lexer.number;
                self.lexer.next();
                Expr::new(location, ExprData::Number(value))
            }
            Token::Minus => {
                self.lexer.next();
                let value = self.lexer.number;
                self.lexer.expect(Token::NumericLiteral);
                Expr::new(location, ExprData::Number(-value))
            }
            Token::OpenBracket => self.parse_array(location),
            Token::OpenBrace => self.parse_object(location),
            Token::BigIntegerLiteral => {
                if !self.options.is_for_define {
                    self.lexer.unexpected();
                }
                let value = String::from_utf8_lossy(&self.lexer.identifier.string).into_owned();
                self.lexer.next();
                Expr::new(location, ExprData::BigInt(value))
            }
            _ => self.lexer.unexpected(),
        }
    }

    fn parse_array(&mut self, location: crate::internal::logger::Loc) -> Expr {
        self.lexer.next();
        let mut is_single_line = !self.lexer.has_newline_before;
        let mut items = Vec::new();
        while self.lexer.token != Token::CloseBracket {
            if !items.is_empty() {
                if self.lexer.has_newline_before {
                    is_single_line = false;
                }
                if !self.parse_maybe_trailing_comma(Token::CloseBracket) {
                    break;
                }
                if self.lexer.has_newline_before {
                    is_single_line = false;
                }
            }
            items.push(self.parse_expr());
        }
        if self.lexer.has_newline_before {
            is_single_line = false;
        }
        let close_bracket_loc = self.lexer.loc();
        self.lexer.expect(Token::CloseBracket);
        Expr::new(
            location,
            ExprData::Array(ArrayExpr {
                items,
                close_bracket_loc,
                is_single_line,
                ..ArrayExpr::default()
            }),
        )
    }

    fn parse_object(&mut self, location: crate::internal::logger::Loc) -> Expr {
        self.lexer.next();
        let mut is_single_line = !self.lexer.has_newline_before;
        let mut properties = Vec::new();
        let mut duplicates = HashMap::<Vec<u8>, Range>::new();
        while self.lexer.token != Token::CloseBrace {
            if !properties.is_empty() {
                if self.lexer.has_newline_before {
                    is_single_line = false;
                }
                if !self.parse_maybe_trailing_comma(Token::CloseBrace) {
                    break;
                }
                if self.lexer.has_newline_before {
                    is_single_line = false;
                }
            }

            let key_string = self.lexer.string_literal().to_vec();
            let key_range = self.lexer.range();
            let key = Expr::new(
                key_range.loc,
                ExprData::String(StringExpr {
                    value: key_string.clone(),
                    ..StringExpr::default()
                }),
            );
            self.lexer.expect(Token::StringLiteral);

            if !self.suppress_warnings_about_weird_code {
                let key_text = utf16_to_string(&key_string);
                if let Some(previous_range) = duplicates.get(&key_text).copied() {
                    let display = String::from_utf8_lossy(&key_text);
                    let note = self.tracker.msg_data(
                        previous_range,
                        format!("The original key {display:?} is here:"),
                    );
                    self.log.add_id_with_notes(
                        MsgId::JsDuplicateObjectKey,
                        MsgKind::Warning,
                        Some(&mut self.tracker),
                        key_range,
                        format!("Duplicate key {display:?} in object literal"),
                        vec![note],
                    );
                } else {
                    duplicates.insert(key_text, key_range);
                }
            }

            self.lexer.expect(Token::Colon);
            let value = self.parse_expr();
            let mut property = Property {
                kind: PropertyKind::Field,
                loc: key_range.loc,
                key,
                value_or_nil: value,
                ..Property::default()
            };
            if key_string == "__proto__".encode_utf16().collect::<Vec<_>>()
                && !self
                    .options
                    .unsupported_js_features
                    .contains(JsFeature::OBJECT_EXTENSIONS)
            {
                property.flags |= PropertyFlags::IS_COMPUTED;
            }
            properties.push(property);
        }

        if self.lexer.has_newline_before {
            is_single_line = false;
        }
        let close_brace_loc = self.lexer.loc();
        self.lexer.expect(Token::CloseBrace);
        Expr::new(
            location,
            ExprData::Object(ObjectExpr {
                properties,
                close_brace_loc,
                is_single_line,
                ..ObjectExpr::default()
            }),
        )
    }
}

/// Parse JSON or TypeScript configuration JSON into esbuild's JavaScript AST.
#[must_use]
pub fn parse_json(log: Log, source: Source, mut options: JsonOptions) -> (Expr, bool) {
    if options.error_suffix.is_empty() {
        " in JSON".clone_into(&mut options.error_suffix);
    }
    let mut result = Expr::default();
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        let tracker = LineColumnTracker::new(Some(&source));
        let suppress_warnings_about_weird_code = is_inside_node_modules(&source.key_path.text);
        let lexer = Lexer::new_json(
            log.clone(),
            source,
            options.flavor,
            options.error_suffix.clone(),
        );
        let mut parser = JsonParser {
            log,
            tracker,
            lexer,
            options,
            suppress_warnings_about_weird_code,
        };
        result = parser.parse_expr();
        parser.lexer.expect(Token::EndOfFile);
    }));
    match parsed {
        Ok(()) => (result, true),
        Err(payload) if payload.downcast_ref::<LexerPanic>().is_some() => (result, false),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[must_use]
pub fn is_valid_json(value: &Expr) -> bool {
    match value.data.as_deref() {
        Some(ExprData::Null | ExprData::Boolean(_) | ExprData::String(_) | ExprData::Number(_)) => {
            true
        }
        Some(ExprData::Array(array)) => array.items.iter().all(is_valid_json),
        Some(ExprData::Object(object)) => object.properties.iter().all(|property| {
            property.kind == PropertyKind::Field
                && !property.flags.contains(PropertyFlags::IS_COMPUTED)
                && matches!(property.key.data.as_deref(), Some(ExprData::String(_)))
                && is_valid_json(&property.value_or_nil)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{JsonOptions, is_valid_json, parse_json};
    use crate::internal::{
        js_ast::ExprData,
        js_lexer::JsonFlavor,
        logger::{DeferLogKind, Log, Source},
    };
    use std::collections::HashMap;

    fn parse(text: &str, options: JsonOptions) -> (crate::internal::js_ast::Expr, bool, Log) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text.as_bytes()),
            ..Source::default()
        };
        let (value, ok) = parse_json(log.clone(), source, options);
        (value, ok, log)
    }

    #[test]
    fn parses_recursive_json_values() {
        let (value, ok, log) = parse(
            r#"{"a":[null,true,false,-1,2.5,"x"],"b":{"c":3}}"#,
            JsonOptions::default(),
        );
        assert!(ok);
        assert!(is_valid_json(&value));
        assert!(matches!(value.data.as_deref(), Some(ExprData::Object(_))));
        assert!(log.done().is_empty());
    }

    #[test]
    fn reports_json_extensions_but_tsconfig_accepts_them() {
        let (_, ok, log) = parse("[1, // comment\n2,]", JsonOptions::default());
        assert!(ok);
        assert_eq!(log.done().len(), 2);

        let (_, ok, log) = parse(
            "[1, // comment\n2,]",
            JsonOptions {
                flavor: JsonFlavor::TsConfigJson,
                ..JsonOptions::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
    }

    #[test]
    fn warns_about_duplicate_keys_and_protects_proto() {
        let (value, ok, log) = parse(r#"{"x":1,"x":2,"__proto__":3}"#, JsonOptions::default());
        assert!(ok);
        assert_eq!(log.done().len(), 1);
        assert!(!is_valid_json(&value));
    }

    #[test]
    fn bigints_are_only_allowed_for_defines() {
        let (_, ok, _) = parse("123n", JsonOptions::default());
        assert!(!ok);
        let (value, ok, log) = parse(
            "123n",
            JsonOptions {
                is_for_define: true,
                ..JsonOptions::default()
            },
        );
        assert!(ok);
        assert!(matches!(value.data.as_deref(), Some(ExprData::BigInt(text)) if text == "123"));
        assert!(log.done().is_empty());
    }

    #[test]
    fn rejects_trailing_tokens_and_invalid_shapes() {
        let (_, ok, log) = parse("true false", JsonOptions::default());
        assert!(!ok);
        assert!(!log.done().is_empty());
    }

    #[test]
    fn translated_upstream_valid_json_matrix() {
        for text in [
            "false",
            "true",
            "null",
            "\"x\"",
            "\"\\\"\"",
            "\"\\\\\"",
            "\"\\/\"",
            "\"\\b\"",
            "\"\\f\"",
            "\"\\n\"",
            "\"\\r\"",
            "\"\\t\"",
            "\"\\u0000\"",
            "\"\\u0078\"",
            "\"\\u1234\"",
            "\"\\uD800\"",
            "\"\\uDC00\"",
            "0",
            "-0",
            "123",
            "123.456",
            "123e20",
            "123e-20",
            "{}",
            "{\"x\":0}",
            "{\"x\":0,\"y\":1}",
            "[]",
            "[1]",
            "[1,2]",
        ] {
            let (value, ok, log) = parse(text, JsonOptions::default());
            assert!(ok, "{text}");
            assert!(is_valid_json(&value), "{text}");
            assert!(log.done().is_empty(), "{text}");
        }
    }

    #[test]
    fn translated_upstream_invalid_json_matrix() {
        for text in [
            "undefined",
            "'x'",
            "`x`",
            "\"\r\"",
            "\"\n\"",
            "\"\\",
            "\"\\0\"",
            "\"\\1\"",
            "\"\\'\"",
            "\"\\a\"",
            "\"\\v\"",
            "\"\\\n\"",
            "\"\\x78\"",
            "\"\\u{1234}\"",
            "\"\\uG\"",
            "123.",
            "-123.",
            ".123",
            "-.123",
            "NaN",
            "Infinity",
            "-Infinity",
            "+1",
            "- 1",
            "01",
            "0b1",
            "0o1",
            "0x1",
            "0n",
            "1_2",
            "1.e2",
            "{\"x\":0,}",
            "{x:0}",
            "{1:0}",
            "{[\"x\"]:0}",
            "[,]",
            "[,1]",
            "[1,]",
            "[1,,2]",
            "({\"x\":0})",
            "{\"x\":(0)}",
            "#!/usr/bin/env node\n{}",
            "{\"x\":0}{\"y\":1}",
            "/*comment*/{}",
            "//comment\n{}",
            "{/*comment*/}",
            "{}/*comment*/",
        ] {
            let (_, _, log) = parse(text, JsonOptions::default());
            assert!(!log.done().is_empty(), "{text:?}");
        }
    }
}
