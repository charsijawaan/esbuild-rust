use std::{collections::HashMap, sync::Arc};

use crate::internal::{
    config::DefineExpr,
    js_ast::{Expr, ExprData, is_identifier},
    js_lexer::{Token, keyword_token},
    logger::{DeferLogKind, Log, Source},
};

use super::{JsonOptions, parse_json};

#[must_use]
pub fn parse_define_expr(text: &str) -> (DefineExpr, Expr) {
    if text.is_empty() {
        return (DefineExpr::default(), Expr::default());
    }

    let parts = text.split('.').collect::<Vec<_>>();
    let is_property_chain = parts.iter().enumerate().all(|(index, part)| {
        if !is_identifier(part) {
            return false;
        }
        if index == 0
            && let Some(token) = keyword_token(part)
        {
            return token == Token::Null
                || token == Token::This
                || (token == Token::Import && parts.get(1) == Some(&"meta"));
        }
        true
    });
    if is_property_chain {
        return (
            DefineExpr {
                parts: parts.into_iter().map(str::to_string).collect(),
                ..DefineExpr::default()
            },
            Expr::default(),
        );
    }

    let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
    let (expr, ok) = parse_json(
        log,
        Source {
            contents: Arc::from(text.as_bytes()),
            ..Source::default()
        },
        JsonOptions {
            is_for_define: true,
            ..JsonOptions::default()
        },
    );
    if !ok {
        return (DefineExpr::default(), Expr::default());
    }

    if matches!(
        expr.data.as_deref(),
        Some(
            ExprData::Null
                | ExprData::Boolean(_)
                | ExprData::String(_)
                | ExprData::Number(_)
                | ExprData::BigInt(_)
        )
    ) {
        return (
            DefineExpr {
                constant: expr,
                ..DefineExpr::default()
            },
            Expr::default(),
        );
    }

    (DefineExpr::default(), expr)
}

#[cfg(test)]
mod tests {
    use super::parse_define_expr;
    use crate::internal::js_ast::ExprData;

    #[test]
    fn parses_identifier_chains_with_upstream_keyword_rules() {
        let (define, injected) = parse_define_expr("process.env.NODE_ENV");
        assert_eq!(define.parts, ["process", "env", "NODE_ENV"]);
        assert!(injected.data.is_none());

        let (define, _) = parse_define_expr("import.meta.url");
        assert_eq!(define.parts, ["import", "meta", "url"]);

        let (define, _) = parse_define_expr("null");
        assert_eq!(define.parts, ["null"]);
    }

    #[test]
    fn inlines_primitive_json_values() {
        let (define, injected) = parse_define_expr("\"value\"");
        assert!(matches!(
            define.constant.data.as_deref(),
            Some(ExprData::String(_))
        ));
        assert!(injected.data.is_none());

        let (define, _) = parse_define_expr("123n");
        assert!(matches!(
            define.constant.data.as_deref(),
            Some(ExprData::BigInt(value)) if value == "123"
        ));
    }

    #[test]
    fn returns_compound_json_for_out_of_line_injection() {
        let (define, injected) = parse_define_expr("[1, 2]");
        assert!(define.constant.data.is_none());
        assert!(matches!(injected.data.as_deref(), Some(ExprData::Array(_))));
    }

    #[test]
    fn rejects_invalid_define_expressions() {
        let (define, injected) = parse_define_expr("a + b");
        assert!(define.parts.is_empty());
        assert!(define.constant.data.is_none());
        assert!(injected.data.is_none());
    }
}
