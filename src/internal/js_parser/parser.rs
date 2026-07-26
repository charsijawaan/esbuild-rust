use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
};

use crate::internal::{
    ast::SymbolKind,
    helpers::utf16_to_string,
    js_ast::{Ast, ExportsKind, ExprData, Part, ScopeKind, Stmt, StmtData},
    js_lexer::{Lexer, LexerPanic, Token},
    logger::{Loc, Log, Source},
};

use super::{
    Options, parser_core::ParserCore, parser_types::AwaitOrYield, syntax_statement::parse_statement,
};

const MODULE_SCOPE_LOC: Loc = Loc { start: -1 };

/// Parse a JavaScript or TypeScript source file into esbuild's JavaScript AST.
///
/// This follows upstream's two-value error convention: syntax errors raised by
/// the lexer are logged and return `false`, while internal panics continue
/// unwinding.
///
/// # Panics
///
/// Panics if parser invariants are violated. Syntax errors from the lexer are
/// caught and reported through the returned boolean instead.
#[must_use]
pub fn parse(log: Log, source: Source, options: Options) -> (Ast, bool) {
    let mut result = Ast::default();
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        let mut lexer = Lexer::new(log.clone(), source.clone(), options.ts.clone());
        let mut core = ParserCore::new_with_log(source, options, log);
        core.push_scope_for_parse_pass(ScopeKind::Entry, MODULE_SCOPE_LOC);

        let hashbang = if lexer.token == Token::Hashbang {
            let value = String::from_utf8_lossy(&lexer.identifier.string).into_owned();
            lexer.next();
            value
        } else {
            String::new()
        };

        // Top-level await is syntactically allowed. The visit pass later
        // determines whether the file is an ECMAScript module.
        core.fn_or_arrow_data_parse.await_policy = AwaitOrYield::AllowExpression;

        let mut statements = Vec::new();
        while lexer.token != Token::EndOfFile {
            statements.push(parse_statement(&mut core, &mut lexer));
        }

        let directives = strip_directive_prologue(&core, &mut statements);
        let module_scope = core
            .module_scope
            .clone()
            .expect("the parser must have an entry scope");
        core.pop_scope();

        // These symbols are always present in upstream's parser result. Their
        // declarations and use counts are filled in by the visit pass.
        let exports_ref = core.new_symbol(SymbolKind::Hoisted, "exports");
        let module_ref = core.new_symbol(SymbolKind::Hoisted, "module");
        let wrapper_ref = core.new_symbol(
            SymbolKind::Other,
            format!("require_{}", core.source.identifier_name),
        );

        let mut parts = vec![Part {
            symbol_uses: HashMap::new(),
            can_be_removed_if_unused: true,
            ..Part::default()
        }];
        if !statements.is_empty() {
            parts.push(Part {
                statements,
                scopes: vec![module_scope.clone()],
                symbol_uses: std::mem::take(&mut core.symbol_uses),
                ..Part::default()
            });
        }

        let exports_kind = if core.options.module_type_data.module_type.is_esm() {
            ExportsKind::Esm
        } else if core.options.module_type_data.module_type.is_common_js() {
            ExportsKind::CommonJs
        } else {
            ExportsKind::None
        };
        let mut top_level_symbol_to_parts_from_parser = HashMap::new();
        top_level_symbol_to_parts_from_parser.insert(exports_ref, vec![0]);

        result = Ast {
            module_type_data: core.options.module_type_data,
            parts,
            symbols: core.symbols,
            module_scope: Some(module_scope),
            hashbang,
            directives,
            top_level_symbol_to_parts_from_parser,
            mangled_props: core.mangled_props,
            reserved_props: core.reserved_props,
            source_map_comment: lexer.source_mapping_url.clone(),
            exports_ref,
            module_ref,
            wrapper_ref,
            approximate_line_count: i32::try_from(lexer.approximate_newline_count)
                .unwrap_or(i32::MAX)
                .saturating_add(1),
            exports_kind,
            ..Ast::default()
        };
    }));

    match parsed {
        Ok(()) => (result, true),
        Err(payload) if payload.downcast_ref::<LexerPanic>().is_some() => (result, false),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn strip_directive_prologue(core: &ParserCore, statements: &mut Vec<Stmt>) -> Vec<String> {
    let mut directives = Vec::new();
    if core
        .options
        .ts_always_strict
        .as_deref()
        .is_some_and(|value| value.value)
    {
        directives.push("use strict".to_owned());
    }

    let mut count = 0;
    for statement in statements.iter() {
        let Some(StmtData::Expr(expression)) = statement.data.as_deref() else {
            break;
        };
        let Some(ExprData::String(value)) = expression.value.data.as_deref() else {
            break;
        };
        let start = usize::try_from(statement.loc.start).unwrap_or(usize::MAX);
        if !matches!(core.source.contents.get(start), Some(b'\'' | b'"')) {
            break;
        }

        let directive = String::from_utf8_lossy(&utf16_to_string(&value.value)).into_owned();
        if !directives.contains(&directive) {
            directives.push(directive);
        }
        count += 1;
    }
    statements.drain(..count);
    directives
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse;
    use crate::internal::{
        js_ast::{ExprData, StmtData},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    fn parse_source(text: &str) -> (crate::internal::js_ast::Ast, bool, Log) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text.as_bytes()),
            identifier_name: "entry".to_owned(),
            ..Source::default()
        };
        let (ast, ok) = parse(log.clone(), source, Options::default());
        (ast, ok, log)
    }

    #[test]
    fn parses_a_complete_source_file_into_parts() {
        let (ast, ok, log) = parse_source(
            "#!/usr/bin/env node\n\"use strict\";\nlet [a, b = 2] = values;\nif (a) b++;",
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.hashbang, "#!/usr/bin/env node");
        assert_eq!(ast.directives, ["use strict"]);
        assert_eq!(ast.approximate_line_count, 4);
        assert_eq!(ast.parts.len(), 2);
        assert!(ast.parts[0].statements.is_empty());
        assert_eq!(ast.parts[1].statements.len(), 2);
        assert_eq!(ast.parts[1].scopes.len(), 1);
        assert!(ast.module_scope.is_some());
        assert_eq!(ast.symbols.len(), 3);
        assert!(matches!(
            ast.parts[1].statements[0].data.as_deref(),
            Some(StmtData::Local(_))
        ));
        assert!(matches!(
            ast.parts[1].statements[1].data.as_deref(),
            Some(StmtData::If(_))
        ));
    }

    #[test]
    fn allows_top_level_await() {
        let (ast, ok, log) = parse_source("await work();");
        assert!(ok);
        assert!(log.done().is_empty());
        let Some(StmtData::Expr(statement)) = ast.parts[1].statements[0].data.as_deref() else {
            panic!("expected expression statement");
        };
        assert!(matches!(
            statement.value.data.as_deref(),
            Some(ExprData::Await(_))
        ));
    }

    #[test]
    fn reports_lexer_panics_as_parse_failure() {
        let (ast, ok, log) = parse_source("let x = ;");
        assert!(!ok);
        assert!(ast.parts.is_empty());
        assert!(!log.done().is_empty());
    }
}
