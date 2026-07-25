#![allow(dead_code)]

use crate::internal::{
    ast::SymbolKind,
    compat::JsFeature,
    config::Mode,
    js_ast::{LocalKind, Scope, ScopeKind},
};

use super::Options;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MergeResult {
    Forbidden,
    ReplaceWithNew,
    OverwriteWithNew,
    KeepExisting,
    BecomePrivateGetSetPair,
    BecomePrivateStaticGetSetPair,
}

pub(crate) fn can_merge_symbols(
    scope: &Scope,
    existing: SymbolKind,
    new: SymbolKind,
    parse_typescript: bool,
) -> MergeResult {
    if existing == SymbolKind::Unbound {
        return MergeResult::ReplaceWithNew;
    }

    if parse_typescript && existing == SymbolKind::Import {
        return MergeResult::ReplaceWithNew;
    }

    if new == SymbolKind::TsEnum && existing == SymbolKind::TsEnum {
        return MergeResult::KeepExisting;
    }

    if new == SymbolKind::TsEnum && existing == SymbolKind::TsNamespace {
        return MergeResult::ReplaceWithNew;
    }

    if new == SymbolKind::TsNamespace
        && matches!(
            existing,
            SymbolKind::TsNamespace
                | SymbolKind::HoistedFunction
                | SymbolKind::GeneratorOrAsyncFunction
                | SymbolKind::TsEnum
                | SymbolKind::Class
        )
    {
        return MergeResult::KeepExisting;
    }

    if new.is_hoisted_or_function()
        && existing.is_hoisted_or_function()
        && (matches!(
            scope.kind,
            ScopeKind::Entry | ScopeKind::FunctionBody | ScopeKind::FunctionArgs
        ) || (new == existing && new.is_hoisted()))
    {
        return MergeResult::ReplaceWithNew;
    }

    if matches!(
        (existing, new),
        (SymbolKind::PrivateGet, SymbolKind::PrivateSet)
            | (SymbolKind::PrivateSet, SymbolKind::PrivateGet)
    ) {
        return MergeResult::BecomePrivateGetSetPair;
    }

    if matches!(
        (existing, new),
        (SymbolKind::PrivateStaticGet, SymbolKind::PrivateStaticSet)
            | (SymbolKind::PrivateStaticSet, SymbolKind::PrivateStaticGet)
    ) {
        return MergeResult::BecomePrivateStaticGetSetPair;
    }

    if existing == SymbolKind::CatchIdentifier && new == SymbolKind::Hoisted {
        return MergeResult::ReplaceWithNew;
    }

    if existing == SymbolKind::Arguments && new == SymbolKind::Hoisted {
        return MergeResult::KeepExisting;
    }

    if existing == SymbolKind::Arguments && new != SymbolKind::Hoisted {
        return MergeResult::OverwriteWithNew;
    }

    MergeResult::Forbidden
}

pub(crate) fn select_local_kind(
    kind: LocalKind,
    options: &Options,
    is_top_level: bool,
    will_wrap_module_in_try_catch_for_using: bool,
) -> LocalKind {
    if matches!(kind, LocalKind::Let | LocalKind::Const)
        && options
            .unsupported_js_features
            .contains(JsFeature::CONST_AND_LET)
    {
        return LocalKind::Var;
    }

    if is_top_level
        && matches!(kind, LocalKind::Let | LocalKind::Const)
        && (options.mode == Mode::Bundle || will_wrap_module_in_try_catch_for_using)
    {
        return LocalKind::Var;
    }

    if options.mode == Mode::Bundle && kind == LocalKind::Const && options.minify_syntax {
        return LocalKind::Let;
    }

    kind
}

#[cfg(test)]
mod tests {
    use super::{MergeResult, can_merge_symbols, select_local_kind};
    use crate::internal::{
        ast::SymbolKind,
        compat::JsFeature,
        config::Mode,
        js_ast::{LocalKind, Scope, ScopeKind},
        js_parser::Options,
    };

    fn scope(kind: ScopeKind) -> Scope {
        Scope {
            kind,
            ..Scope::default()
        }
    }

    #[test]
    fn translates_upstream_symbol_merge_table() {
        let block = scope(ScopeKind::Block);
        let entry = scope(ScopeKind::Entry);

        let cases = [
            (
                &block,
                SymbolKind::Unbound,
                SymbolKind::Other,
                false,
                MergeResult::ReplaceWithNew,
            ),
            (
                &block,
                SymbolKind::Import,
                SymbolKind::Class,
                true,
                MergeResult::ReplaceWithNew,
            ),
            (
                &block,
                SymbolKind::TsEnum,
                SymbolKind::TsEnum,
                false,
                MergeResult::KeepExisting,
            ),
            (
                &block,
                SymbolKind::TsNamespace,
                SymbolKind::TsEnum,
                false,
                MergeResult::ReplaceWithNew,
            ),
            (
                &block,
                SymbolKind::Class,
                SymbolKind::TsNamespace,
                false,
                MergeResult::KeepExisting,
            ),
            (
                &entry,
                SymbolKind::Hoisted,
                SymbolKind::HoistedFunction,
                false,
                MergeResult::ReplaceWithNew,
            ),
            (
                &block,
                SymbolKind::PrivateGet,
                SymbolKind::PrivateSet,
                false,
                MergeResult::BecomePrivateGetSetPair,
            ),
            (
                &block,
                SymbolKind::PrivateStaticSet,
                SymbolKind::PrivateStaticGet,
                false,
                MergeResult::BecomePrivateStaticGetSetPair,
            ),
            (
                &block,
                SymbolKind::CatchIdentifier,
                SymbolKind::Hoisted,
                false,
                MergeResult::ReplaceWithNew,
            ),
            (
                &block,
                SymbolKind::Arguments,
                SymbolKind::Hoisted,
                false,
                MergeResult::KeepExisting,
            ),
            (
                &block,
                SymbolKind::Arguments,
                SymbolKind::Const,
                false,
                MergeResult::OverwriteWithNew,
            ),
            (
                &block,
                SymbolKind::Import,
                SymbolKind::Class,
                false,
                MergeResult::Forbidden,
            ),
        ];

        for (scope, existing, new, parse_typescript, expected) in cases {
            assert_eq!(
                can_merge_symbols(scope, existing, new, parse_typescript),
                expected
            );
        }
    }

    #[test]
    fn selects_local_kind_for_target_bundle_and_minification() {
        let mut options = Options::default();
        assert_eq!(
            select_local_kind(LocalKind::Const, &options, false, false),
            LocalKind::Const
        );

        options.unsupported_js_features = JsFeature::CONST_AND_LET;
        assert_eq!(
            select_local_kind(LocalKind::Let, &options, false, false),
            LocalKind::Var
        );

        options.unsupported_js_features = JsFeature::default();
        options.mode = Mode::Bundle;
        assert_eq!(
            select_local_kind(LocalKind::Const, &options, true, false),
            LocalKind::Var
        );

        options.minify_syntax = true;
        assert_eq!(
            select_local_kind(LocalKind::Const, &options, false, false),
            LocalKind::Let
        );

        options.mode = Mode::default();
        assert_eq!(
            select_local_kind(LocalKind::Let, &options, true, true),
            LocalKind::Var
        );
    }
}
