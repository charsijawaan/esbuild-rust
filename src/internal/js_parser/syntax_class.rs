#![allow(dead_code)]

use crate::internal::{
    ast::{LocRef, SymbolKind},
    helpers::string_to_utf16,
    js_ast::{
        Class, ClassExpr, ClassStaticBlock, Expr, ExprData, FunctionExpr, NameOfSymbolExpr,
        Precedence, PrivateIdentifierExpr, Property, PropertyFlags, PropertyKind, StringExpr,
    },
    js_lexer::{Lexer, MaybeSubstring, Token},
    logger::Loc,
};

use super::{
    parser_core::ParserCore,
    parser_types::{AwaitOrYield, FnOrArrowDataParse},
    syntax_expression::parse_expression,
    syntax_function::parse_function_tail,
    syntax_literals::{
        parse_big_int_or_string_if_unsupported, parse_numeric_literal, parse_string_literal,
    },
    syntax_statement::parse_block_with_scope,
};

pub(crate) fn parse_class_prefix(core: &mut ParserCore, lexer: &mut Lexer) -> Option<Expr> {
    if lexer.token != Token::Class {
        return None;
    }
    let loc = lexer.loc();
    let class_keyword = lexer.range();
    lexer.next();
    core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::ClassName, loc);

    let name = if lexer.token == Token::Identifier {
        let name = LocRef {
            loc: lexer.loc(),
            reference: core.store_name_in_ref(lexer.identifier.clone()),
        };
        lexer.next();
        Some(name)
    } else {
        None
    };
    if core.options.ts.parse {
        super::syntax_typescript::skip_type_parameters(lexer);
    }
    let extends_or_nil = if lexer.token == Token::Extends {
        lexer.next();
        let extends = parse_expression(core, lexer, Precedence::New, true);
        if core.options.ts.parse {
            super::syntax_typescript::skip_type_parameters(lexer);
        }
        extends
    } else {
        Expr::default()
    };
    if core.options.ts.parse {
        super::syntax_typescript::skip_class_implements_clause(lexer);
    }

    let body_loc = lexer.loc();
    lexer.expect(Token::OpenBrace);
    core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::ClassBody, body_loc);
    let mut properties = Vec::new();
    while lexer.token != Token::CloseBrace {
        if lexer.token == Token::Semicolon {
            lexer.next();
            continue;
        }
        if let Some(property) = parse_class_property(core, lexer, extends_or_nil.data.is_some()) {
            properties.push(property);
        }
    }
    let mut has_constructor = false;
    for property in &properties {
        if property.kind == PropertyKind::Method
            && !property.flags.contains(PropertyFlags::IS_STATIC)
            && key_is_named(&property.key, "constructor")
        {
            if has_constructor {
                core.add_error_range(
                    crate::internal::logger::Range {
                        loc: property.key.loc,
                        len: 0,
                    },
                    "Classes cannot contain more than one constructor",
                );
            }
            has_constructor = true;
        }
    }
    let close_brace_loc = lexer.loc();
    lexer.expect(Token::CloseBrace);
    core.pop_scope();
    core.pop_scope();
    Some(Expr::new(
        loc,
        ExprData::Class(ClassExpr {
            class: Class {
                name,
                extends_or_nil,
                properties,
                class_keyword,
                body_loc,
                close_brace_loc,
                ..Class::default()
            },
        }),
    ))
}

#[allow(clippy::too_many_lines)]
fn parse_class_property(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    class_has_extends: bool,
) -> Option<Property> {
    let start_loc = lexer.loc();
    let mut is_static = false;
    let mut preconsumed_static = None;
    if lexer.is_contextual_keyword(b"static") {
        let name = lexer.identifier.clone();
        let name_loc = lexer.loc();
        let name_range = lexer.range();
        lexer.next();
        if lexer.token == Token::OpenBrace {
            let old_context = core.fn_or_arrow_data_parse;
            core.fn_or_arrow_data_parse = FnOrArrowDataParse {
                is_return_disallowed: true,
                allow_super_property: true,
                ..FnOrArrowDataParse::default()
            };
            let (block_loc, block) = parse_block_with_scope(
                core,
                lexer,
                crate::internal::js_ast::ScopeKind::ClassStaticInit,
            );
            core.fn_or_arrow_data_parse = old_context;
            return Some(Property {
                class_static_block: Some(Box::new(ClassStaticBlock {
                    block,
                    loc: block_loc,
                })),
                loc: start_loc,
                kind: PropertyKind::ClassStaticBlock,
                ..Property::default()
            });
        }
        if lexer.token == Token::OpenParen {
            preconsumed_static = Some((name, name_loc, name_range));
        } else {
            is_static = true;
        }
    }

    let mut kind = PropertyKind::Field;
    let mut is_type_only = false;
    let mut preconsumed_ts_key = preconsumed_static;
    while preconsumed_ts_key.is_none()
        && core.options.ts.parse
        && lexer.token == Token::Identifier
        && matches!(
            lexer.raw(),
            b"public"
                | b"private"
                | b"protected"
                | b"readonly"
                | b"abstract"
                | b"declare"
                | b"override"
                | b"accessor"
                | b"static"
        )
    {
        let name = lexer.identifier.clone();
        let name_loc = lexer.loc();
        let name_range = lexer.range();
        let modifier = lexer.raw().to_vec();
        lexer.next();
        let could_be_modifier = lexer.is_identifier_or_keyword()
            || matches!(
                lexer.token,
                Token::OpenBracket
                    | Token::NumericLiteral
                    | Token::StringLiteral
                    | Token::PrivateIdentifier
                    | Token::Asterisk
            );
        if !could_be_modifier || lexer.has_newline_before {
            preconsumed_ts_key = Some((name, name_loc, name_range));
            break;
        }
        match modifier.as_slice() {
            b"abstract" | b"declare" => is_type_only = true,
            b"accessor" => kind = PropertyKind::AutoAccessor,
            b"static" => is_static = true,
            _ => {}
        }
    }

    let mut is_async = false;
    let mut preconsumed_key = preconsumed_ts_key;
    if preconsumed_key.is_none()
        && lexer.token == Token::Identifier
        && matches!(lexer.raw(), b"get" | b"set" | b"async")
    {
        let name = lexer.identifier.clone();
        let name_loc = lexer.loc();
        let name_range = lexer.range();
        let modifier = lexer.raw().to_vec();
        lexer.next();
        let could_be_modifier = lexer.is_identifier_or_keyword()
            || matches!(
                lexer.token,
                Token::OpenBracket
                    | Token::NumericLiteral
                    | Token::StringLiteral
                    | Token::PrivateIdentifier
            )
            || (modifier == b"async" && lexer.token == Token::Asterisk);
        if could_be_modifier && (modifier != b"async" || !lexer.has_newline_before) {
            match modifier.as_slice() {
                b"get" => kind = PropertyKind::Getter,
                b"set" => kind = PropertyKind::Setter,
                b"async" => is_async = true,
                _ => unreachable!(),
            }
        } else {
            preconsumed_key = Some((name, name_loc, name_range));
        }
    }

    let is_generator = preconsumed_key.is_none() && lexer.token == Token::Asterisk;
    if is_generator {
        lexer.next();
    }
    let key_loc = preconsumed_key
        .as_ref()
        .map_or_else(|| lexer.loc(), |(_, loc, _)| *loc);
    let key_range = preconsumed_key
        .as_ref()
        .map_or_else(|| lexer.range(), |(_, _, range)| *range);
    let mut flags = if is_static {
        PropertyFlags::IS_STATIC
    } else {
        PropertyFlags::NONE
    };
    let mut close_bracket_loc = Loc::default();
    let mut key = if let Some((name, loc, _)) = preconsumed_key {
        class_property_name(core, loc, name)
    } else {
        match lexer.token {
            Token::PrivateIdentifier => {
                let reference = core.store_name_in_ref(lexer.identifier.clone());
                let key = Expr::new(
                    lexer.loc(),
                    ExprData::PrivateIdentifier(PrivateIdentifierExpr { reference }),
                );
                lexer.next();
                key
            }
            Token::OpenBracket => {
                flags |= PropertyFlags::IS_COMPUTED;
                lexer.next();
                let key = parse_expression(core, lexer, Precedence::Comma, true);
                if core.options.ts.parse
                    && lexer.token == Token::Colon
                    && matches!(key.data.as_deref(), Some(ExprData::Identifier(_)))
                {
                    super::syntax_typescript::skip_type_annotation(lexer, &[Token::CloseBracket]);
                    lexer.expect(Token::CloseBracket);
                    super::syntax_typescript::skip_type_annotation(
                        lexer,
                        &[Token::Semicolon, Token::CloseBrace],
                    );
                    lexer.expect_or_insert_semicolon();
                    return None;
                }
                close_bracket_loc = lexer.loc();
                lexer.expect(Token::CloseBracket);
                key
            }
            Token::NumericLiteral => parse_numeric_literal(core, lexer),
            Token::BigIntegerLiteral => {
                let key = parse_big_int_or_string_if_unsupported(core, lexer);
                lexer.next();
                key
            }
            Token::StringLiteral => parse_string_literal(core, lexer),
            _ => {
                if !lexer.is_identifier_or_keyword() {
                    lexer.expected(Token::Identifier);
                }
                let name = lexer.identifier.clone();
                lexer.next();
                class_property_name(core, key_loc, name)
            }
        }
    };

    if core.options.ts.parse {
        if lexer.token == Token::Question
            || (lexer.token == Token::Exclamation && !lexer.has_newline_before)
        {
            lexer.next();
        }
        if kind != PropertyKind::AutoAccessor {
            super::syntax_typescript::skip_type_parameters(lexer);
        }
    }

    if is_type_only {
        if lexer.token == Token::OpenParen || kind.is_method_definition() {
            super::syntax_typescript::skip_type_script_method_signature(lexer);
        } else {
            super::syntax_typescript::skip_type_annotation(
                lexer,
                &[Token::Equals, Token::Semicolon, Token::CloseBrace],
            );
            if lexer.token == Token::Equals {
                let scope_index = core.scopes_in_order.len();
                lexer.next();
                let _ = parse_expression(core, lexer, Precedence::Comma, true);
                core.discard_scopes_up_to(scope_index);
            }
            lexer.expect_or_insert_semicolon();
        }
        return None;
    }

    if let Some(ExprData::PrivateIdentifier(private)) = key.data.as_deref_mut() {
        let name = String::from_utf8_lossy(core.load_name_from_ref(private.reference)).into_owned();
        let is_method = lexer.token == Token::OpenParen || kind.is_method_definition();
        let symbol_kind = match (is_static, kind, is_method) {
            (false, PropertyKind::AutoAccessor, _) => SymbolKind::PrivateGetSetPair,
            (false, PropertyKind::Getter, _) => SymbolKind::PrivateGet,
            (false, PropertyKind::Setter, _) => SymbolKind::PrivateSet,
            (false, _, true) => SymbolKind::PrivateMethod,
            (false, _, false) => SymbolKind::PrivateField,
            (true, PropertyKind::AutoAccessor, _) => SymbolKind::PrivateStaticGetSetPair,
            (true, PropertyKind::Getter, _) => SymbolKind::PrivateStaticGet,
            (true, PropertyKind::Setter, _) => SymbolKind::PrivateStaticSet,
            (true, _, true) => SymbolKind::PrivateStaticMethod,
            (true, _, false) => SymbolKind::PrivateStaticField,
        };
        private.reference = core.declare_symbol(symbol_kind, key.loc, &name);
    }

    let is_method = lexer.token == Token::OpenParen || kind.is_method_definition();
    let key_name = class_key_name(core, &key);
    if is_method {
        if key_name.as_deref() == Some("#constructor") {
            core.add_error_range(key_range, "Invalid method name \"#constructor\"");
        } else if is_static && key_name.as_deref() == Some("prototype") {
            core.add_error_range(key_range, "Invalid static method name \"prototype\"");
        } else if !is_static && key_name.as_deref() == Some("constructor") {
            let error = if is_async {
                Some("Class constructor cannot be an async function")
            } else if is_generator {
                Some("Class constructor cannot be a generator")
            } else if kind == PropertyKind::Getter {
                Some("Class constructor cannot be a getter")
            } else if kind == PropertyKind::Setter {
                Some("Class constructor cannot be a setter")
            } else {
                None
            };
            if let Some(error) = error {
                core.add_error_range(key_range, error);
            }
        }
    } else if key_name.as_deref() == Some("constructor")
        || key_name.as_deref() == Some("#constructor")
        || (is_static && key_name.as_deref() == Some("prototype"))
    {
        core.add_error_range(
            key_range,
            format!(
                "Invalid field name {:?}",
                key_name.expect("invalid field name is present")
            ),
        );
    }

    if is_method {
        let is_constructor = !is_static && key_is_named(&key, "constructor");
        let mut function = parse_function_tail(
            core,
            lexer,
            None,
            false,
            true,
            FnOrArrowDataParse {
                await_policy: if is_async {
                    AwaitOrYield::AllowExpression
                } else {
                    AwaitOrYield::AllowIdentifier
                },
                yield_policy: if is_generator {
                    AwaitOrYield::AllowExpression
                } else {
                    AwaitOrYield::AllowIdentifier
                },
                allow_super_call: class_has_extends && is_constructor,
                allow_super_property: true,
                is_constructor,
                ..FnOrArrowDataParse::default()
            },
        );
        if !function.has_body {
            return None;
        }
        function.is_unique_formal_parameters = true;
        if kind == PropertyKind::Getter && !function.args.is_empty() {
            core.add_error_range(
                key_range,
                format!(
                    "Getter {} must have zero arguments",
                    key_name_for_error(&key)
                ),
            );
        } else if kind == PropertyKind::Setter && function.args.len() != 1 {
            core.add_error_range(
                key_range,
                format!(
                    "Setter {} must have exactly one argument",
                    key_name_for_error(&key)
                ),
            );
        } else if kind == PropertyKind::Field {
            kind = PropertyKind::Method;
        }
        return Some(Property {
            key,
            value_or_nil: Expr::new(
                start_loc,
                ExprData::Function(FunctionExpr {
                    function,
                    ..FunctionExpr::default()
                }),
            ),
            loc: start_loc,
            close_bracket_loc,
            kind,
            flags,
            ..Property::default()
        });
    }
    if is_generator {
        lexer.expected(Token::OpenParen);
    }

    if core.options.ts.parse {
        super::syntax_typescript::skip_type_annotation(
            lexer,
            &[Token::Equals, Token::Semicolon, Token::CloseBrace],
        );
    }
    let initializer_or_nil = if lexer.token == Token::Equals {
        lexer.next();
        let old_context = core.fn_or_arrow_data_parse;
        core.fn_or_arrow_data_parse.is_this_disallowed = false;
        core.fn_or_arrow_data_parse.allow_super_property = true;
        let value = parse_expression(core, lexer, Precedence::Comma, true);
        core.fn_or_arrow_data_parse = old_context;
        value
    } else {
        Expr::default()
    };
    lexer.expect_or_insert_semicolon();
    Some(Property {
        key,
        initializer_or_nil,
        loc: start_loc,
        close_bracket_loc,
        kind,
        flags,
        ..Property::default()
    })
}

fn class_property_name(core: &mut ParserCore, loc: Loc, name: MaybeSubstring) -> Expr {
    let text =
        String::from_utf8(name.string.clone()).expect("class property names must be valid UTF-8");
    if core.is_mangled_prop(&text) {
        Expr::new(
            loc,
            ExprData::NameOfSymbol(NameOfSymbolExpr {
                reference: core.store_name_in_ref(name),
                has_property_key_comment: true,
            }),
        )
    } else {
        Expr::new(
            loc,
            ExprData::String(StringExpr {
                value: string_to_utf16(text.as_bytes()),
                ..StringExpr::default()
            }),
        )
    }
}

fn key_is_named(key: &Expr, expected: &str) -> bool {
    matches!(
        key.data.as_deref(),
        Some(ExprData::String(string))
            if crate::internal::helpers::utf16_to_string(&string.value) == expected.as_bytes()
    )
}

fn class_key_name(core: &ParserCore, key: &Expr) -> Option<String> {
    match key.data.as_deref() {
        Some(ExprData::String(string)) => Some(
            String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(&string.value))
                .into_owned(),
        ),
        Some(ExprData::PrivateIdentifier(private)) => {
            let index = usize::try_from(private.reference.inner_index).ok()?;
            core.symbols
                .get(index)
                .map(|symbol| symbol.original_name.clone())
        }
        _ => None,
    }
}

fn key_name_for_error(key: &Expr) -> String {
    if let Some(ExprData::String(string)) = key.data.as_deref() {
        format!(
            "{:?}",
            String::from_utf8_lossy(&crate::internal::helpers::utf16_to_string(&string.value))
        )
    } else {
        "property".into()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_class_prefix;
    use crate::internal::{
        config::TsOptions,
        js_ast::{ExprData, PropertyFlags, PropertyKind},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_class_name_extends_methods_fields_static_and_private_keys() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"class Child extends Base { field = 1; static count = 2; #private; method() { return this.field } static { cleanup() } }"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_class_prefix(&mut core, &mut lexer).expect("class");
        let Some(ExprData::Class(class)) = expr.data.as_deref() else {
            panic!("expected class");
        };
        assert!(class.class.name.is_some());
        assert!(class.class.extends_or_nil.data.is_some());
        assert_eq!(class.class.properties.len(), 5);
        assert!(
            class.class.properties[1]
                .flags
                .contains(PropertyFlags::IS_STATIC)
        );
        assert_eq!(class.class.properties[3].kind, PropertyKind::Method);
        assert_eq!(
            class.class.properties[4].kind,
            PropertyKind::ClassStaticBlock
        );
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_class_accessors_async_generators_and_static_methods() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"class Example { get value() { return this.x } set value(v) { this.x = v } async load() { await work } async *stream() { yield await item } static async fetch() { await request } }"[..],
            ),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_class_prefix(&mut core, &mut lexer).expect("class");
        let Some(ExprData::Class(class)) = expr.data.as_deref() else {
            panic!("expected class");
        };
        assert_eq!(class.class.properties[0].kind, PropertyKind::Getter);
        assert_eq!(class.class.properties[1].kind, PropertyKind::Setter);
        assert!(matches!(
            class.class.properties[2].value_or_nil.data.as_deref(),
            Some(ExprData::Function(function))
                if function.function.is_async && !function.function.is_generator
        ));
        assert!(matches!(
            class.class.properties[3].value_or_nil.data.as_deref(),
            Some(ExprData::Function(function))
                if function.function.is_async && function.function.is_generator
        ));
        assert!(
            class.class.properties[4]
                .flags
                .contains(PropertyFlags::IS_STATIC)
        );
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn erases_type_script_class_types_and_modifiers() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                &b"class Box<T> extends Base<Map<K, V>> implements Readable<T>, Disposable {\
                    public readonly value!: T;\
                    protected optional?: number = 1;\
                    private declare hidden: string;\
                    abstract omitted: boolean;\
                    override map<U>(input: U): T { return this.value; }\
                    static accessor count: number = 0;\
                    public() {}\
                }"[..],
            ),
            ..Source::default()
        };
        let ts_options = TsOptions {
            parse: true,
            ..TsOptions::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), ts_options.clone());
        let mut core = super::ParserCore::new(
            source,
            Options {
                ts: ts_options,
                ..Options::default()
            },
        );
        let expr = parse_class_prefix(&mut core, &mut lexer).expect("class");
        let Some(ExprData::Class(class)) = expr.data.as_deref() else {
            panic!("expected class");
        };
        assert!(matches!(
            class.class.extends_or_nil.data.as_deref(),
            Some(ExprData::Identifier(_))
        ));
        assert_eq!(class.class.properties.len(), 5);
        assert!(class.class.properties[0].initializer_or_nil.data.is_none());
        assert!(matches!(
            class.class.properties[1].initializer_or_nil.data.as_deref(),
            Some(ExprData::Number(1.0))
        ));
        assert_eq!(class.class.properties[2].kind, PropertyKind::Method);
        assert_eq!(class.class.properties[3].kind, PropertyKind::AutoAccessor);
        assert!(
            class.class.properties[3]
                .flags
                .contains(PropertyFlags::IS_STATIC)
        );
        assert_eq!(class.class.properties[4].kind, PropertyKind::Method);
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
