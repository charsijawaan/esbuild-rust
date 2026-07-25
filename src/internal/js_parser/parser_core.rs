#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::internal::{
    ast::{INVALID_REF, LocRef, Ref, Symbol, SymbolFlags, SymbolKind},
    config::Mode,
    js_ast::{Expr, ExprData, Scope, ScopeKind, ScopeRef, SymbolUse},
    logger::{Loc, Source},
};

use super::Options;

#[derive(Clone, Debug)]
pub(crate) struct ScopeOrder {
    scope: ScopeRef,
    loc: Loc,
}

#[derive(Debug)]
pub(crate) struct ParserCore {
    pub(crate) options: Options,
    pub(crate) source: Source,
    pub(crate) current_scope: Option<ScopeRef>,
    pub(crate) scopes_in_order: Vec<ScopeOrder>,
    pub(crate) scopes_for_current_part: Vec<ScopeRef>,
    pub(crate) symbols: Vec<Symbol>,
    pub(crate) symbol_uses: HashMap<Ref, SymbolUse>,
    pub(crate) ts_use_counts: Vec<u32>,
    pub(crate) is_file_considered_esm: bool,
    pub(crate) is_control_flow_dead: bool,
}

impl ParserCore {
    pub(crate) fn new(source: Source, options: Options) -> Self {
        Self {
            options,
            source,
            current_scope: None,
            scopes_in_order: Vec::new(),
            scopes_for_current_part: Vec::new(),
            symbols: Vec::new(),
            symbol_uses: HashMap::new(),
            ts_use_counts: Vec::new(),
            is_file_considered_esm: false,
            is_control_flow_dead: false,
        }
    }

    pub(crate) fn push_scope_for_parse_pass(&mut self, kind: ScopeKind, loc: Loc) -> usize {
        let parent = self.current_scope.clone();
        let scope = Arc::new(Mutex::new(Scope {
            kind,
            parent: parent.as_ref().map(Arc::downgrade),
            label: LocRef {
                reference: INVALID_REF,
                ..LocRef::default()
            },
            ..Scope::default()
        }));

        if let Some(parent) = &parent {
            let (strict_mode, use_strict_loc) = {
                let mut parent = parent
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                parent.children.push(scope.clone());
                (parent.strict_mode, parent.use_strict_loc)
            };
            let mut scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scope.strict_mode = strict_mode;
            scope.use_strict_loc = use_strict_loc;
        }
        self.current_scope = Some(scope.clone());

        if let Some(previous) = self.scopes_in_order.last() {
            assert!(
                previous.loc.start < loc.start,
                "Scope location {} must be greater than {}",
                loc.start,
                previous.loc.start
            );
        }

        if kind == ScopeKind::FunctionBody {
            let parent = parent.expect("function body scopes must have a parent");
            let members = {
                let parent = parent
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                assert_eq!(
                    parent.kind,
                    ScopeKind::FunctionArgs,
                    "function body scope must follow function arguments"
                );
                parent.members.clone()
            };
            let mut scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (name, member) in members {
                let symbol_index =
                    usize::try_from(member.reference.inner_index).expect("symbol index fits usize");
                if self.symbols[symbol_index].kind != SymbolKind::HoistedFunction {
                    scope.members.insert(name, member);
                }
            }
        }

        let scope_index = self.scopes_in_order.len();
        self.scopes_in_order.push(ScopeOrder { scope, loc });
        scope_index
    }

    pub(crate) fn pop_scope(&mut self) {
        let current = self
            .current_scope
            .take()
            .expect("cannot pop an empty scope stack");
        let (contains_direct_eval, members, parent) = {
            let current = current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                current.contains_direct_eval,
                current.members.values().copied().collect::<Vec<_>>(),
                current.parent.as_ref().and_then(std::sync::Weak::upgrade),
            )
        };

        if contains_direct_eval {
            let skip_top_level_esm = self.options.mode == Mode::Bundle
                && parent.is_none()
                && self.is_file_considered_esm;
            if !skip_top_level_esm {
                for member in members {
                    let symbol_index = usize::try_from(member.reference.inner_index)
                        .expect("symbol index fits usize");
                    self.symbols[symbol_index].flags |= SymbolFlags::MUST_NOT_BE_RENAMED;
                }
            }
        }
        self.current_scope = parent;
    }

    pub(crate) fn push_scope_for_visit_pass(&mut self, kind: ScopeKind, loc: Loc) {
        let order = self
            .scopes_in_order
            .first()
            .expect("visit pass generated more scopes than parse pass");
        let actual_kind = order
            .scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .kind;
        assert!(
            order.loc == loc && actual_kind == kind,
            "Expected scope ({kind:?}, {}) in {:?}, found scope ({actual_kind:?}, {})",
            loc.start,
            self.source.pretty_paths.select(self.options.log_path_style),
            order.loc.start
        );

        let order = self.scopes_in_order.remove(0);
        self.current_scope = Some(order.scope.clone());
        self.scopes_for_current_part.push(order.scope);
    }

    pub(crate) fn pop_and_discard_scope(&mut self, scope_index: usize) {
        for order in self.scopes_in_order[scope_index..].iter().rev() {
            let parent = {
                let scope = order
                    .scope
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                scope
                    .parent
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .expect("discarded scopes must have a parent")
            };
            let mut parent = parent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let child = parent
                .children
                .pop()
                .expect("discarded scope must be the last child");
            assert!(
                Arc::ptr_eq(&child, &order.scope),
                "discarded scope must be the last child"
            );
        }

        let current = self
            .current_scope
            .take()
            .expect("cannot discard an empty scope stack");
        self.current_scope = current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .parent
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        self.scopes_in_order.truncate(scope_index);
    }

    pub(crate) fn pop_and_flatten_scope(&mut self, scope_index: usize) {
        let to_flatten = self
            .current_scope
            .take()
            .expect("cannot flatten an empty scope stack");
        let (parent, children) = {
            let mut scope = to_flatten
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let parent = scope
                .parent
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .expect("flattened scopes must have a parent");
            (parent, std::mem::take(&mut scope.children))
        };
        self.current_scope = Some(parent.clone());
        self.scopes_in_order.remove(scope_index);

        let mut parent_scope = parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let child = parent_scope
            .children
            .pop()
            .expect("flattened scope must be the last child");
        assert!(
            Arc::ptr_eq(&child, &to_flatten),
            "flattened scope must be the last child"
        );
        for child in children {
            child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .parent = Some(Arc::downgrade(&parent));
            parent_scope.children.push(child);
        }
    }

    pub(crate) fn discard_scopes_up_to(&mut self, scope_index: usize) {
        let current = self
            .current_scope
            .as_ref()
            .expect("cannot discard from an empty scope stack")
            .clone();
        let discarded = self.scopes_in_order[scope_index..]
            .iter()
            .map(|order| order.scope.clone())
            .collect::<Vec<_>>();
        current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .children
            .retain(|child| {
                !discarded.iter().any(|discarded| {
                    let is_direct_child = discarded
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .parent
                        .as_ref()
                        .and_then(std::sync::Weak::upgrade)
                        .is_some_and(|parent| Arc::ptr_eq(&parent, &current));
                    is_direct_child && Arc::ptr_eq(child, discarded)
                })
            });
        self.scopes_in_order.truncate(scope_index);
    }

    pub(crate) fn new_symbol(&mut self, kind: SymbolKind, name: impl Into<String>) -> Ref {
        let inner_index =
            u32::try_from(self.symbols.len()).expect("parser symbol count must fit in u32");
        let reference = Ref {
            source_index: self.source.index,
            inner_index,
        };
        self.symbols.push(Symbol::new(kind, name));
        if self.options.ts.parse {
            self.ts_use_counts.push(0);
        }
        reference
    }

    pub(crate) fn merge_symbols(&mut self, old: Ref, new: Ref) -> Ref {
        if old == new {
            return new;
        }

        let old_index = usize::try_from(old.inner_index).expect("symbol index fits usize");
        let old_link = self.symbols[old_index].link;
        if old_link != INVALID_REF {
            let merged = self.merge_symbols(old_link, new);
            self.symbols[old_index].link = merged;
            return merged;
        }

        let new_index = usize::try_from(new.inner_index).expect("symbol index fits usize");
        let new_link = self.symbols[new_index].link;
        if new_link != INVALID_REF {
            let merged = self.merge_symbols(old, new_link);
            self.symbols[new_index].link = merged;
            return merged;
        }

        self.symbols[old_index].link = new;
        let old_symbol = self.symbols[old_index].clone();
        self.symbols[new_index].merge_contents_with(&old_symbol);
        new
    }

    pub(crate) fn record_usage(&mut self, reference: Ref) {
        let symbol_index = usize::try_from(reference.inner_index).expect("symbol index fits usize");
        if !self.is_control_flow_dead {
            self.symbols[symbol_index].use_count_estimate = self.symbols[symbol_index]
                .use_count_estimate
                .wrapping_add(1);
            let usage = self.symbol_uses.entry(reference).or_default();
            usage.count_estimate = usage.count_estimate.wrapping_add(1);
        }

        if self.options.ts.parse {
            self.ts_use_counts[symbol_index] = self.ts_use_counts[symbol_index].wrapping_add(1);
        }
    }

    pub(crate) fn ignore_usage(&mut self, reference: Ref) {
        if self.is_control_flow_dead {
            return;
        }

        let symbol_index = usize::try_from(reference.inner_index).expect("symbol index fits usize");
        self.symbols[symbol_index].use_count_estimate -= 1;
        let usage = self
            .symbol_uses
            .get_mut(&reference)
            .expect("ignored symbol usage must have been recorded");
        usage.count_estimate -= 1;
        if usage.count_estimate == 0 {
            self.symbol_uses.remove(&reference);
        }
    }

    pub(crate) fn ignore_usage_of_identifier_in_dot_chain(&mut self, mut expr: &Expr) {
        loop {
            match expr.data.as_deref() {
                Some(ExprData::Identifier(identifier)) => {
                    self.ignore_usage(identifier.reference);
                }
                Some(ExprData::Dot(dot)) => {
                    expr = &dot.target;
                    continue;
                }
                Some(ExprData::Index(index))
                    if matches!(index.index.data.as_deref(), Some(ExprData::String(_))) =>
                {
                    expr = &index.target;
                    continue;
                }
                _ => {}
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::ParserCore;
    use crate::internal::{
        ast::{INVALID_REF, SymbolFlags, SymbolKind},
        js_ast::{
            DotExpr, Expr, ExprData, IdentifierExpr, IndexExpr, ScopeKind, ScopeMember, StringExpr,
        },
        logger::{Loc, Source},
    };

    fn parser() -> ParserCore {
        ParserCore::new(
            Source {
                index: 7,
                ..Source::default()
            },
            super::Options::default(),
        )
    }

    #[test]
    fn function_body_copies_arguments_except_function_expression_name() {
        let mut parser = parser();
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        parser.push_scope_for_parse_pass(ScopeKind::FunctionArgs, Loc { start: 2 });
        let argument = parser.new_symbol(SymbolKind::Other, "argument");
        let function_name = parser.new_symbol(SymbolKind::HoistedFunction, "name");
        {
            let scope = parser.current_scope.as_ref().unwrap();
            let mut scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scope.members.insert(
                "argument".into(),
                ScopeMember {
                    reference: argument,
                    ..ScopeMember::default()
                },
            );
            scope.members.insert(
                "name".into(),
                ScopeMember {
                    reference: function_name,
                    ..ScopeMember::default()
                },
            );
        }
        parser.push_scope_for_parse_pass(ScopeKind::FunctionBody, Loc { start: 3 });
        let scope = parser.current_scope.as_ref().unwrap();
        let scope = scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(scope.members["argument"].reference, argument);
        assert!(!scope.members.contains_key("name"));
    }

    #[test]
    fn discard_and_flatten_preserve_scope_tree_invariants() {
        let mut parser = parser();
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        let entry = parser.current_scope.clone().unwrap();
        let block_index = parser.push_scope_for_parse_pass(ScopeKind::Block, Loc { start: 2 });
        parser.push_scope_for_parse_pass(ScopeKind::Block, Loc { start: 3 });
        parser.pop_scope();
        parser.pop_and_flatten_scope(block_index);
        assert_eq!(parser.scopes_in_order.len(), 2);
        let entry_scope = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(entry_scope.children.len(), 1);
        assert!(Arc::ptr_eq(
            &entry,
            &entry_scope.children[0]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .parent
                .as_ref()
                .unwrap()
                .upgrade()
                .unwrap()
        ));
        drop(entry_scope);

        let discard_index = parser.push_scope_for_parse_pass(ScopeKind::Block, Loc { start: 4 });
        parser.push_scope_for_parse_pass(ScopeKind::Block, Loc { start: 5 });
        parser.pop_scope();
        parser.pop_and_discard_scope(discard_index);
        assert_eq!(parser.scopes_in_order.len(), 2);
        assert_eq!(
            entry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .children
                .len(),
            1
        );
    }

    #[test]
    fn allocates_and_merges_parser_local_symbols() {
        let mut parser = parser();
        parser.options.ts.parse = true;
        let old = parser.new_symbol(SymbolKind::Other, "old");
        let new = parser.new_symbol(SymbolKind::Other, "new");
        parser.symbols[0].use_count_estimate = 3;
        parser.symbols[0].flags = SymbolFlags::MUST_NOT_BE_RENAMED;
        assert_eq!(parser.merge_symbols(old, new), new);
        assert_eq!(parser.symbols[0].link, new);
        assert_eq!(parser.symbols[1].link, INVALID_REF);
        assert_eq!(parser.symbols[1].use_count_estimate, 3);
        assert_eq!(parser.symbols[1].original_name, "old");
        assert_eq!(parser.ts_use_counts, [0, 0]);
    }

    #[test]
    fn visit_pass_replays_first_pass_scope_order() {
        let mut parser = parser();
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        parser.push_scope_for_parse_pass(ScopeKind::Block, Loc { start: 2 });
        parser.current_scope = None;
        parser.push_scope_for_visit_pass(ScopeKind::Entry, Loc { start: 1 });
        parser.push_scope_for_visit_pass(ScopeKind::Block, Loc { start: 2 });
        assert!(parser.scopes_in_order.is_empty());
        assert_eq!(parser.scopes_for_current_part.len(), 2);
    }

    #[test]
    fn usage_accounting_keeps_typescript_counts_when_usage_is_ignored() {
        let mut parser = parser();
        parser.options.ts.parse = true;
        let reference = parser.new_symbol(SymbolKind::Other, "value");
        parser.record_usage(reference);
        assert_eq!(parser.symbols[0].use_count_estimate, 1);
        assert_eq!(parser.symbol_uses[&reference].count_estimate, 1);
        assert_eq!(parser.ts_use_counts[0], 1);
        parser.ignore_usage(reference);
        assert_eq!(parser.symbols[0].use_count_estimate, 0);
        assert!(!parser.symbol_uses.contains_key(&reference));
        assert_eq!(parser.ts_use_counts[0], 1);

        parser.is_control_flow_dead = true;
        parser.record_usage(reference);
        assert_eq!(parser.symbols[0].use_count_estimate, 0);
        assert_eq!(parser.ts_use_counts[0], 2);
    }

    #[test]
    fn ignores_identifier_usage_through_static_property_chain() {
        let mut parser = parser();
        let reference = parser.new_symbol(SymbolKind::Other, "value");
        parser.record_usage(reference);
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference,
                ..IdentifierExpr::default()
            }),
        );
        let dot = Expr::new(
            Loc::default(),
            ExprData::Dot(DotExpr {
                target: identifier,
                name: "x".into(),
                ..DotExpr::default()
            }),
        );
        let index = Expr::new(
            Loc::default(),
            ExprData::Index(IndexExpr {
                target: dot,
                index: Expr::new(Loc::default(), ExprData::String(StringExpr::default())),
                ..IndexExpr::default()
            }),
        );
        parser.ignore_usage_of_identifier_in_dot_chain(&index);
        assert_eq!(parser.symbols[0].use_count_estimate, 0);
    }
}
