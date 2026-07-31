#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::internal::{
    ast::{
        INVALID_REF, ImportKind, ImportPhase, ImportRecord, ImportRecordFlags, LocRef,
        NamespaceAlias, Ref, Symbol, SymbolFlags, SymbolKind,
    },
    compat::JsFeature,
    config::{Mode, pretty_print_target_environment},
    helpers::contains_non_bmp_code_point,
    js_ast::{
        Binding, CallExpr, ConstValue, DeclaredSymbol, DotExpr, Expr, ExprData, IdentifierExpr,
        IndexExpr, NameOfSymbolExpr, NamedImport, OptionalChain, Scope, ScopeKind, ScopeMember,
        ScopeRef, StrictModeKind, SymbolUse, for_each_identifier_binding,
    },
    js_lexer::{MaybeSubstring, range_of_identifier},
    logger::{LineColumnTracker, Loc, Log, MsgId, MsgKind, Path, Range, Source},
};

use super::{
    Options,
    parser_types::FnOrArrowDataParse,
    symbols::{MergeResult, can_merge_symbols},
};

#[derive(Clone, Debug)]
pub(crate) struct ScopeOrder {
    scope: ScopeRef,
    loc: Loc,
}

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct ParserCore {
    pub(crate) options: Options,
    pub(crate) log: Option<Log>,
    pub(crate) source: Source,
    pub(crate) tracker: LineColumnTracker,
    pub(crate) current_scope: Option<ScopeRef>,
    pub(crate) module_scope: Option<ScopeRef>,
    pub(crate) scopes_in_order: Vec<ScopeOrder>,
    pub(crate) scopes_for_current_part: Vec<ScopeRef>,
    pub(crate) symbols: Vec<Symbol>,
    pub(crate) import_records: Vec<ImportRecord>,
    pub(crate) symbol_uses: HashMap<Ref, SymbolUse>,
    pub(crate) declared_symbols: Vec<DeclaredSymbol>,
    pub(crate) runtime_imports: HashMap<String, LocRef>,
    pub(crate) jsx_imports: HashMap<super::parser_types::JsxImport, Ref>,
    pub(crate) jsx_import_records: HashMap<String, (u32, Ref)>,
    pub(crate) generated_named_imports: HashMap<Ref, NamedImport>,
    pub(crate) generated_injected_defines: HashMap<u32, Ref>,
    pub(crate) allocated_names: Vec<Vec<u8>>,
    pub(crate) mangled_props: HashMap<String, Ref>,
    pub(crate) reserved_props: HashMap<String, bool>,
    pub(crate) unrepresentable_identifiers: HashMap<String, bool>,
    pub(crate) ts_enums: HashMap<Ref, HashMap<String, crate::internal::js_ast::TsEnumValue>>,
    pub(crate) ts_enum_values_by_ref: HashMap<Ref, crate::internal::js_ast::TsEnumValue>,
    pub(crate) const_values: HashMap<Ref, ConstValue>,
    pub(crate) ts_use_counts: Vec<u32>,
    pub(crate) is_file_considered_esm: bool,
    pub(crate) is_control_flow_dead: bool,
    pub(crate) promise_ref: Ref,
    pub(crate) reg_exp_ref: Ref,
    pub(crate) big_int_ref: Ref,
    pub(crate) require_ref: Ref,
    pub(crate) exports_ref: Ref,
    pub(crate) module_ref: Ref,
    pub(crate) legacy_octal_literals: HashMap<Loc, Range>,
    pub(crate) esm_import_meta: Range,
    pub(crate) esm_export_keyword: Range,
    pub(crate) top_level_await_keyword: Range,
    pub(crate) fn_or_arrow_data_parse: FnOrArrowDataParse,
    pub(crate) lower_all_of_these_private_names: HashMap<String, bool>,
    pub(crate) hoisted_ref_for_sloppy_mode_block_fn: HashMap<Ref, Ref>,
    pub(crate) visit_loop_depth: usize,
    pub(crate) visit_switch_depth: usize,
    pub(crate) visit_try_body_depth: usize,
    pub(crate) visit_new_target_allowed: bool,
    pub(crate) visit_is_async_generator: bool,
    pub(crate) has_top_level_return: bool,
    pub(crate) has_jsx_element: bool,
    pub(crate) has_type_script_export: bool,
    pub(crate) should_fold_type_script_constant_expressions: bool,
}

impl ParserCore {
    pub(crate) fn new(source: Source, options: Options) -> Self {
        let tracker = LineColumnTracker::new(Some(&source));
        Self {
            options,
            log: None,
            source,
            tracker,
            current_scope: None,
            module_scope: None,
            scopes_in_order: Vec::new(),
            scopes_for_current_part: Vec::new(),
            symbols: Vec::new(),
            import_records: Vec::new(),
            symbol_uses: HashMap::new(),
            declared_symbols: Vec::new(),
            runtime_imports: HashMap::new(),
            jsx_imports: HashMap::new(),
            jsx_import_records: HashMap::new(),
            generated_named_imports: HashMap::new(),
            generated_injected_defines: HashMap::new(),
            allocated_names: Vec::new(),
            mangled_props: HashMap::new(),
            reserved_props: HashMap::new(),
            unrepresentable_identifiers: HashMap::new(),
            ts_enums: HashMap::new(),
            ts_enum_values_by_ref: HashMap::new(),
            const_values: HashMap::new(),
            ts_use_counts: Vec::new(),
            is_file_considered_esm: false,
            is_control_flow_dead: false,
            promise_ref: INVALID_REF,
            reg_exp_ref: INVALID_REF,
            big_int_ref: INVALID_REF,
            require_ref: INVALID_REF,
            exports_ref: INVALID_REF,
            module_ref: INVALID_REF,
            legacy_octal_literals: HashMap::new(),
            esm_import_meta: Range::default(),
            esm_export_keyword: Range::default(),
            top_level_await_keyword: Range::default(),
            fn_or_arrow_data_parse: FnOrArrowDataParse::default(),
            lower_all_of_these_private_names: HashMap::new(),
            hoisted_ref_for_sloppy_mode_block_fn: HashMap::new(),
            visit_loop_depth: 0,
            visit_switch_depth: 0,
            visit_try_body_depth: 0,
            visit_new_target_allowed: false,
            visit_is_async_generator: false,
            has_top_level_return: false,
            has_jsx_element: false,
            has_type_script_export: false,
            should_fold_type_script_constant_expressions: false,
        }
    }

    pub(crate) fn new_with_log(source: Source, options: Options, log: Log) -> Self {
        Self {
            log: Some(log),
            ..Self::new(source, options)
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
        if kind == ScopeKind::Entry && self.module_scope.is_none() {
            self.module_scope = Some(scope.clone());
        }

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

    pub(crate) fn scope_refs_in_order(&self) -> Vec<ScopeRef> {
        self.scopes_in_order
            .iter()
            .map(|order| order.scope.clone())
            .collect()
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

    pub(crate) fn push_next_scope_for_visit_pass(&mut self, kind: ScopeKind) {
        let loc = self
            .scopes_in_order
            .first()
            .expect("visit pass generated more scopes than parse pass")
            .loc;
        self.push_scope_for_visit_pass(kind, loc);
    }

    pub(crate) fn remaining_scope_count(&self) -> usize {
        self.scopes_in_order.len()
    }

    pub(crate) fn record_declared_symbol(&mut self, reference: Ref) {
        let mut is_top_level = self
            .current_scope
            .as_ref()
            .zip(self.module_scope.as_ref())
            .is_some_and(|(current, module)| Arc::ptr_eq(current, module));

        if !is_top_level {
            let symbol_index =
                usize::try_from(reference.inner_index).expect("symbol index fits usize");
            let symbol = &self.symbols[symbol_index];
            if symbol.kind.is_hoisted() {
                let name = symbol.original_name.clone();
                let module_reference = self.module_scope.as_ref().and_then(|scope| {
                    scope
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .members
                        .get(&name)
                        .map(|member| member.reference)
                });
                if let Some(module_reference) = module_reference {
                    is_top_level = self.follow_symbol_link(reference)
                        == self.follow_symbol_link(module_reference);
                }
            }
        }

        if let Some(existing) = self
            .declared_symbols
            .iter_mut()
            .find(|declared| declared.reference == reference)
        {
            existing.is_top_level |= is_top_level;
        } else {
            self.declared_symbols.push(DeclaredSymbol {
                reference,
                is_top_level,
            });
        }
    }

    fn follow_symbol_link(&self, mut reference: Ref) -> Ref {
        loop {
            let index = usize::try_from(reference.inner_index).expect("symbol index fits usize");
            let link = self.symbols[index].link;
            if link == INVALID_REF {
                return reference;
            }
            reference = link;
        }
    }

    pub(crate) fn is_strict_mode(&self) -> bool {
        self.current_scope.as_ref().is_some_and(|scope| {
            scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .strict_mode
                != StrictModeKind::Sloppy
        })
    }

    pub(crate) fn is_current_scope_module_scope(&self) -> bool {
        self.current_scope
            .as_ref()
            .zip(self.module_scope.as_ref())
            .is_some_and(|(current, module)| Arc::ptr_eq(current, module))
    }

    pub(crate) fn is_inside_function_scope(&self) -> bool {
        let mut scope = self.current_scope.clone();
        while let Some(current) = scope {
            let current = current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if current.kind == ScopeKind::FunctionBody {
                return true;
            }
            scope = current.parent.as_ref().and_then(std::sync::Weak::upgrade);
        }
        false
    }

    pub(crate) fn is_inside_class_static_block(&self) -> bool {
        let mut scope = self.current_scope.clone();
        while let Some(current) = scope {
            let current = current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if current.kind == ScopeKind::ClassStaticInit {
                return true;
            }
            scope = current.parent.as_ref().and_then(std::sync::Weak::upgrade);
        }
        false
    }

    pub(crate) fn mark_current_scope_as_containing_direct_eval(&mut self) {
        let mut scope = self.current_scope.clone();
        while let Some(current) = scope {
            let parent = {
                let mut current = current
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                current.contains_direct_eval = true;
                current.parent.as_ref().and_then(std::sync::Weak::upgrade)
            };
            scope = parent;
        }
    }

    pub(crate) fn prepare_for_visit_pass(
        &mut self,
        has_esm_exports: bool,
        has_import_statement: bool,
    ) {
        self.push_scope_for_visit_pass(ScopeKind::Entry, Loc { start: -1 });
        self.module_scope.clone_from(&self.current_scope);

        if self
            .options
            .ts_always_strict
            .as_deref()
            .is_some_and(|value| value.value)
            && let Some(scope) = &self.current_scope
        {
            scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .strict_mode = StrictModeKind::ImplicitStrictTsAlwaysStrict;
        }

        let is_file_considered_to_have_esm_exports =
            has_esm_exports || self.options.module_type_data.module_type.is_esm();
        self.is_file_considered_esm =
            is_file_considered_to_have_esm_exports || has_import_statement;
        if self.is_file_considered_esm {
            Scope::recursive_set_strict_mode(
                self.module_scope
                    .as_ref()
                    .expect("visit pass requires a module scope"),
                StrictModeKind::ImplicitStrictEsm,
            );
        }

        self.require_ref = if self.options.mode == Mode::PassThrough {
            self.new_symbol(SymbolKind::Unbound, "require")
        } else {
            self.declare_common_js_symbol(SymbolKind::Unbound, "require")
        };
        if self.options.mode != Mode::PassThrough && !is_file_considered_to_have_esm_exports {
            self.exports_ref = self.declare_common_js_symbol(SymbolKind::Hoisted, "exports");
            self.module_ref = self.declare_common_js_symbol(SymbolKind::Hoisted, "module");
        } else {
            self.exports_ref = self.new_symbol(SymbolKind::Hoisted, "exports");
            self.module_ref = self.new_symbol(SymbolKind::Hoisted, "module");
        }
    }

    pub(crate) fn hoist_symbols(&mut self) {
        let module_scope = self
            .module_scope
            .clone()
            .expect("symbol hoisting requires a module scope");
        self.hoist_symbols_in_scope(&module_scope);
    }

    pub(crate) fn add_import_record(
        &mut self,
        kind: ImportKind,
        phase: ImportPhase,
        range: Range,
        path: String,
        flags: ImportRecordFlags,
    ) -> u32 {
        let index =
            u32::try_from(self.import_records.len()).expect("import record count fits in u32");
        self.import_records.push(ImportRecord {
            path: Path {
                text: path,
                ..Path::default()
            },
            range,
            flags,
            phase,
            kind,
            ..ImportRecord::default()
        });
        index
    }

    fn hoist_symbols_in_scope(&mut self, scope: &ScopeRef) {
        let (kind, strict_mode, parent, mut members, children) = {
            let scope = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                scope.kind,
                scope.strict_mode,
                scope.parent.as_ref().and_then(std::sync::Weak::upgrade),
                scope.members.values().copied().collect::<Vec<_>>(),
                scope.children.clone(),
            )
        };
        members.sort_by_key(|member| (member.reference.inner_index, member.reference.source_index));

        if !kind.stops_hoisting() {
            for mut member in members {
                let original_reference = member.reference;
                let mut symbol_index =
                    usize::try_from(member.reference.inner_index).expect("symbol index fits usize");
                let symbol_kind = self.symbols[symbol_index].kind;
                if !symbol_kind.is_hoisted() {
                    continue;
                }
                let name = self.symbols[symbol_index].original_name.clone();
                let mut is_sloppy_mode_block_function = false;
                if symbol_kind == SymbolKind::HoistedFunction {
                    if strict_mode != StrictModeKind::Sloppy {
                        continue;
                    }
                    let hoisted_reference = self.new_symbol(SymbolKind::Hoisted, name.clone());
                    scope
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .generated
                        .push(hoisted_reference);
                    self.hoisted_ref_for_sloppy_mode_block_fn
                        .insert(original_reference, hoisted_reference);
                    member.reference = hoisted_reference;
                    symbol_index = usize::try_from(hoisted_reference.inner_index)
                        .expect("symbol index fits usize");
                    is_sloppy_mode_block_function = true;
                }
                let mut target = parent.clone();
                while let Some(target_scope) = target {
                    let (existing, target_kind, next_parent) = {
                        let target = target_scope
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        (
                            target.members.get(&name).copied(),
                            target.kind,
                            target.parent.as_ref().and_then(std::sync::Weak::upgrade),
                        )
                    };

                    if let Some(existing) = existing {
                        if existing.reference == member.reference {
                            break;
                        }
                        let existing_index = usize::try_from(existing.reference.inner_index)
                            .expect("symbol index fits usize");
                        let existing_kind = self.symbols[existing_index].kind;
                        if existing_kind == SymbolKind::Unbound
                            || existing_kind == SymbolKind::Hoisted
                            || (existing_kind.is_function() && target_kind.stops_hoisting())
                        {
                            self.symbols[symbol_index].link = existing.reference;
                        } else if existing_kind != SymbolKind::CatchIdentifier
                            && existing_kind != SymbolKind::Arguments
                        {
                            if is_sloppy_mode_block_function
                                && parent
                                    .as_ref()
                                    .is_some_and(|parent| Arc::ptr_eq(parent, &target_scope))
                            {
                                self.hoisted_ref_for_sloppy_mode_block_fn
                                    .remove(&original_reference);
                            } else {
                                self.add_symbol_already_declared_error(
                                    &name,
                                    member.loc,
                                    existing.loc,
                                );
                            }
                        }
                        break;
                    }

                    target_scope
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .members
                        .insert(name.clone(), member);
                    if target_kind.stops_hoisting() {
                        break;
                    }
                    target = next_parent;
                }
            }
        }

        for child in children {
            self.hoist_symbols_in_scope(&child);
        }
    }

    fn declare_common_js_symbol(&mut self, kind: SymbolKind, name: &str) -> Ref {
        let module_scope = self
            .module_scope
            .as_ref()
            .expect("CommonJS symbols require a module scope")
            .clone();
        let existing = module_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .get(name)
            .copied();
        if let Some(existing) = existing {
            let existing_kind = self.symbols
                [usize::try_from(existing.reference.inner_index).expect("symbol index fits usize")]
            .kind;
            if existing_kind == SymbolKind::Hoisted
                && kind == SymbolKind::Hoisted
                && !self.is_file_considered_esm
            {
                return existing.reference;
            }
        }

        let reference = self.new_symbol(kind, name);
        let mut module_scope = module_scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if existing.is_none() {
            module_scope.members.insert(
                name.into(),
                ScopeMember {
                    reference,
                    loc: Loc { start: -1 },
                },
            );
        } else {
            module_scope.generated.push(reference);
        }
        reference
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

    pub(crate) fn find_symbol(&mut self, loc: Loc, name: &str) -> FindSymbolResult {
        let mut scope = self
            .current_scope
            .clone()
            .expect("symbol lookup requires a current scope");
        let mut is_inside_with_scope = false;
        let mut did_forbid_arguments = false;

        let (reference, declare_loc) = loop {
            let (member, namespace_match, parent, is_with, forbid_arguments) = {
                let scope = scope
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let namespace_match = scope.ts_namespace.as_ref().and_then(|namespace| {
                    namespace
                        .exported_members
                        .get(name)
                        .filter(|member| namespace.is_enum_scope == member.is_enum_value)
                        .map(|member| {
                            (
                                namespace
                                    .lazily_generated_property_accesses
                                    .get(name)
                                    .copied(),
                                namespace.argument_ref,
                                member.loc,
                            )
                        })
                });
                (
                    scope.members.get(name).copied(),
                    namespace_match,
                    scope.parent.as_ref().and_then(std::sync::Weak::upgrade),
                    scope.kind == ScopeKind::With,
                    scope.forbid_arguments,
                )
            };

            is_inside_with_scope |= is_with;
            if forbid_arguments && name == "arguments" && !did_forbid_arguments {
                self.add_error(loc, format!("Cannot access {name:?} here:"));
                did_forbid_arguments = true;
            }

            if let Some(member) = member {
                break (member.reference, member.loc);
            }

            if let Some((cached, namespace_ref, member_loc)) = namespace_match {
                let reference = if let Some(reference) = cached {
                    reference
                } else {
                    let reference = self.new_symbol(SymbolKind::Other, name);
                    let symbol_index =
                        usize::try_from(reference.inner_index).expect("symbol index fits usize");
                    self.symbols[symbol_index].namespace_alias = Some(NamespaceAlias {
                        namespace_ref,
                        alias: name.into(),
                    });
                    scope
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .ts_namespace
                        .as_mut()
                        .expect("namespace metadata must still be present")
                        .lazily_generated_property_accesses
                        .insert(name.into(), reference);
                    reference
                };
                break (reference, member_loc);
            }

            if let Some(parent) = parent {
                scope = parent;
                continue;
            }

            self.check_for_unrepresentable_identifier(loc, name);
            let reference = self.new_symbol(SymbolKind::Unbound, name);
            self.module_scope
                .as_ref()
                .expect("unbound symbols require a module scope")
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .members
                .insert(
                    name.into(),
                    ScopeMember {
                        reference,
                        loc: Loc { start: -1 },
                    },
                );
            break (reference, loc);
        };

        if is_inside_with_scope {
            let symbol_index =
                usize::try_from(reference.inner_index).expect("symbol index fits usize");
            self.symbols[symbol_index].flags |= SymbolFlags::MUST_NOT_BE_RENAMED;
        }
        self.record_usage(reference);
        FindSymbolResult {
            reference,
            declare_loc,
            is_inside_with_scope,
        }
    }

    pub(crate) fn find_label_symbol(&mut self, loc: Loc, name: &str) -> (Ref, bool, bool) {
        let mut scope = self.current_scope.clone();
        while let Some(current) = scope {
            let (kind, label, is_loop, parent) = {
                let scope = current
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    scope.kind,
                    scope.label.reference,
                    scope.label_stmt_is_loop,
                    scope.parent.as_ref().and_then(std::sync::Weak::upgrade),
                )
            };
            if kind.stops_hoisting() {
                break;
            }
            if kind == ScopeKind::Label {
                let symbol_index =
                    usize::try_from(label.inner_index).expect("symbol index fits usize");
                if self.symbols[symbol_index].original_name == name {
                    self.record_usage(label);
                    return (label, is_loop, true);
                }
            }
            scope = parent;
        }

        self.add_error(loc, format!("There is no containing label named {name:?}"));
        let reference = self.new_symbol(SymbolKind::Unbound, name);
        self.record_usage(reference);
        (reference, false, false)
    }

    pub(crate) fn declare_symbol(&mut self, kind: SymbolKind, loc: Loc, name: &str) -> Ref {
        self.check_for_unrepresentable_identifier(loc, name);
        let mut reference = self.new_symbol(kind, name);
        let scope = self
            .current_scope
            .as_ref()
            .expect("symbol declarations require a current scope")
            .clone();
        let existing = scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .get(name)
            .copied();

        if let Some(existing) = existing {
            let existing_index =
                usize::try_from(existing.reference.inner_index).expect("symbol index fits usize");
            let existing_kind = self.symbols[existing_index].kind;
            let merge_result = {
                let scope = scope
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                can_merge_symbols(&scope, existing_kind, kind, self.options.ts.parse)
            };
            match merge_result {
                MergeResult::Forbidden => {
                    self.add_symbol_already_declared_error(name, loc, existing.loc);
                    return existing.reference;
                }
                MergeResult::KeepExisting => reference = existing.reference,
                MergeResult::ReplaceWithNew => {
                    self.symbols[existing_index].link = reference;
                    scope
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .replaced
                        .push(existing);
                    if self.options.minify_syntax
                        && kind.is_function()
                        && existing_kind.is_function()
                    {
                        self.symbols[existing_index].flags |=
                            SymbolFlags::REMOVE_OVERWRITTEN_FUNCTION_DECLARATION;
                    }
                }
                MergeResult::BecomePrivateGetSetPair => {
                    reference = existing.reference;
                    self.symbols[existing_index].kind = SymbolKind::PrivateGetSetPair;
                }
                MergeResult::BecomePrivateStaticGetSetPair => {
                    reference = existing.reference;
                    self.symbols[existing_index].kind = SymbolKind::PrivateStaticGetSetPair;
                }
                MergeResult::OverwriteWithNew => {}
            }
        }

        scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .insert(name.into(), ScopeMember { reference, loc });
        reference
    }

    pub(crate) fn import_from_runtime(&mut self, loc: Loc, name: &str) -> Expr {
        let item = if let Some(item) = self.runtime_imports.get(name).copied() {
            item
        } else {
            let item = LocRef {
                loc,
                reference: self.new_symbol(SymbolKind::Other, name),
            };
            self.module_scope
                .as_ref()
                .expect("runtime imports require a module scope")
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generated
                .push(item.reference);
            self.runtime_imports.insert(name.into(), item);
            item
        };
        self.record_usage(item.reference);
        Expr::new(
            loc,
            ExprData::Identifier(IdentifierExpr {
                reference: item.reference,
                ..IdentifierExpr::default()
            }),
        )
    }

    pub(crate) fn call_runtime(&mut self, loc: Loc, name: &str, args: Vec<Expr>) -> Expr {
        Expr::new(
            loc,
            ExprData::Call(CallExpr {
                target: self.import_from_runtime(loc, name),
                args,
                ..CallExpr::default()
            }),
        )
    }

    pub(crate) fn make_promise_ref(&mut self) -> Ref {
        if self.promise_ref == INVALID_REF {
            self.promise_ref = self.new_symbol(SymbolKind::Unbound, "Promise");
        }
        self.promise_ref
    }

    pub(crate) fn make_reg_exp_ref(&mut self) -> Ref {
        if self.reg_exp_ref == INVALID_REF {
            self.reg_exp_ref = self.new_symbol(SymbolKind::Unbound, "RegExp");
            self.module_scope
                .as_ref()
                .expect("generated references require a module scope")
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generated
                .push(self.reg_exp_ref);
        }
        self.reg_exp_ref
    }

    pub(crate) fn make_big_int_ref(&mut self) -> Ref {
        if self.big_int_ref == INVALID_REF {
            self.big_int_ref = self.new_symbol(SymbolKind::Unbound, "BigInt");
            self.module_scope
                .as_ref()
                .expect("generated references require a module scope")
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generated
                .push(self.big_int_ref);
        }
        self.big_int_ref
    }

    pub(crate) fn store_name_in_ref(&mut self, name: MaybeSubstring) -> Ref {
        if name.start.is_valid() {
            let length =
                u32::try_from(name.string.len()).expect("identifier length must fit in u32");
            assert!(length > 0, "source identifier names must not be empty");
            Ref {
                source_index: 0_u32.wrapping_sub(length),
                inner_index: name.start.get_index(),
            }
        } else {
            let inner_index = u32::try_from(self.allocated_names.len())
                .expect("allocated identifier count must fit in u32");
            self.allocated_names.push(name.string);
            Ref {
                source_index: 0x8000_0000,
                inner_index,
            }
        }
    }

    pub(crate) fn is_stored_name_ref(reference: Ref) -> bool {
        reference.source_index & 0x8000_0000 != 0
    }

    pub(crate) fn declare_binding(&mut self, kind: SymbolKind, binding: &mut Binding) {
        for_each_identifier_binding(binding, &mut |loc, identifier| {
            let name =
                String::from_utf8_lossy(self.load_name_from_ref(identifier.reference)).into_owned();
            identifier.reference = self.declare_symbol(kind, loc, &name);
        });
    }

    pub(crate) fn load_name_from_ref(&self, reference: Ref) -> &[u8] {
        if reference.source_index == 0x8000_0000 {
            let index =
                usize::try_from(reference.inner_index).expect("allocated name index fits usize");
            &self.allocated_names[index]
        } else {
            assert!(
                reference.source_index & 0x8000_0000 != 0,
                "Internal error: invalid symbol reference"
            );
            let start =
                usize::try_from(reference.inner_index).expect("identifier offset fits usize");
            let length = usize::try_from(0_u32.wrapping_sub(reference.source_index))
                .expect("identifier length fits usize");
            &self.source.contents[start..start + length]
        }
    }

    pub(crate) fn is_mangled_prop(&mut self, name: &str) -> bool {
        let should_mangle = self
            .options
            .mangle_props
            .as_ref()
            .is_some_and(|pattern| pattern.is_match(name))
            && !matches!(name, "__proto__" | "constructor" | "prototype")
            && self
                .options
                .reserve_props
                .as_ref()
                .is_none_or(|pattern| !pattern.is_match(name));
        if should_mangle {
            return true;
        }
        self.reserved_props.insert(name.into(), true);
        false
    }

    pub(crate) fn symbol_for_mangled_prop(&mut self, name: &str) -> Ref {
        let reference = if let Some(reference) = self.mangled_props.get(name).copied() {
            reference
        } else {
            let reference = self.new_symbol(SymbolKind::MangledProp, name);
            self.mangled_props.insert(name.into(), reference);
            reference
        };
        if !self.is_control_flow_dead {
            let symbol_index =
                usize::try_from(reference.inner_index).expect("symbol index fits usize");
            self.symbols[symbol_index].use_count_estimate = self.symbols[symbol_index]
                .use_count_estimate
                .wrapping_add(1);
        }
        reference
    }

    pub(crate) fn dot_or_mangled_prop_parse(
        &mut self,
        target: Expr,
        name: MaybeSubstring,
        name_loc: Loc,
        optional_chain: OptionalChain,
        original: WasOriginallyDotOrIndex,
    ) -> ExprData {
        let text = String::from_utf8(name.string.clone())
            .expect("JavaScript identifier property names must be valid UTF-8");
        if (original != WasOriginallyDotOrIndex::Index || self.options.mangle_quoted)
            && self.is_mangled_prop(&text)
        {
            ExprData::Index(IndexExpr {
                target,
                index: Expr::new(
                    name_loc,
                    ExprData::NameOfSymbol(NameOfSymbolExpr {
                        reference: self.store_name_in_ref(name),
                        ..NameOfSymbolExpr::default()
                    }),
                ),
                optional_chain,
                ..IndexExpr::default()
            })
        } else {
            ExprData::Dot(DotExpr {
                target,
                name: text,
                name_loc,
                optional_chain,
                ..DotExpr::default()
            })
        }
    }

    pub(crate) fn dot_or_mangled_prop_visit(
        &mut self,
        target: Expr,
        name: &str,
        name_loc: Loc,
    ) -> ExprData {
        if self.is_mangled_prop(name) {
            ExprData::Index(IndexExpr {
                target,
                index: Expr::new(
                    name_loc,
                    ExprData::NameOfSymbol(NameOfSymbolExpr {
                        reference: self.symbol_for_mangled_prop(name),
                        ..NameOfSymbolExpr::default()
                    }),
                ),
                ..IndexExpr::default()
            })
        } else {
            ExprData::Dot(DotExpr {
                target,
                name: name.into(),
                name_loc,
                ..DotExpr::default()
            })
        }
    }

    pub(crate) fn is_valid_assignment_target(&self, expr: &Expr, is_strict_mode: bool) -> bool {
        match expr.data.as_deref() {
            Some(ExprData::Identifier(identifier)) => {
                !is_strict_mode
                    || !matches!(
                        self.load_name_from_ref(identifier.reference),
                        b"eval" | b"arguments"
                    )
            }
            Some(ExprData::Dot(dot)) => dot.optional_chain == OptionalChain::None,
            Some(ExprData::Index(index)) => index.optional_chain == OptionalChain::None,
            Some(ExprData::Object(object)) => !object.is_parenthesized,
            Some(ExprData::Array(array)) => !array.is_parenthesized,
            _ => false,
        }
    }

    pub(crate) fn mark_async_fn(&mut self, async_range: Range, is_generator: bool) -> bool {
        // Lowered async functions are implemented in terms of generators. If
        // generators are supported then async functions can still be lowered,
        // even when the async syntax itself isn't supported by the target.
        if !self
            .options
            .unsupported_js_features
            .contains(JsFeature::GENERATOR)
        {
            return false;
        }

        self.mark_syntax_feature(
            if is_generator {
                JsFeature::ASYNC_GENERATOR
            } else {
                JsFeature::ASYNC_AWAIT
            },
            async_range,
        )
    }

    pub(crate) fn mark_syntax_feature(&mut self, feature: JsFeature, range: Range) -> bool {
        if !self.options.unsupported_js_features.contains(feature) {
            if feature == JsFeature::TOP_LEVEL_AWAIT
                && !self.options.output_format.keep_esm_import_export_syntax()
            {
                self.add_error_range(
                    range,
                    format!(
                        "Top-level await is currently not supported with the {:?} output format",
                        self.options.output_format.as_str()
                    ),
                );
                return true;
            }
            return false;
        }

        let environment = pretty_print_target_environment(
            &self.options.original_target_env,
            self.options.unsupported_js_feature_overrides_mask,
        );
        if feature == JsFeature::TOP_LEVEL_AWAIT {
            self.add_error_range(
                range,
                format!("Top-level await is not available in {environment}"),
            );
            return true;
        }

        let name = if feature == JsFeature::DEFAULT_ARGUMENT {
            "default arguments".to_owned()
        } else if feature == JsFeature::REST_ARGUMENT {
            "rest arguments".to_owned()
        } else if feature == JsFeature::DESTRUCTURING {
            "destructuring".to_owned()
        } else if feature == JsFeature::NESTED_REST_BINDING {
            "non-identifier array rest patterns".to_owned()
        } else if feature == JsFeature::CLASS {
            "class syntax".to_owned()
        } else if feature == JsFeature::CONST_AND_LET {
            String::from_utf8_lossy(self.source.text_for_range(range)).into_owned()
        } else if feature == JsFeature::GENERATOR {
            "generator functions".to_owned()
        } else if feature == JsFeature::ASYNC_AWAIT {
            "async functions".to_owned()
        } else if feature == JsFeature::ASYNC_GENERATOR {
            "async generator functions".to_owned()
        } else if feature == JsFeature::EXPONENT_OPERATOR {
            "exponentiation assignment operators".to_owned()
        } else {
            self.add_error_range(
                range,
                format!("This feature is not available in {environment}"),
            );
            return true;
        };
        self.add_error_range(
            range,
            format!("Transforming {name} to {environment} is not supported yet"),
        );
        true
    }

    fn check_for_unrepresentable_identifier(&mut self, loc: Loc, name: &str) {
        if self.options.ascii_only
            && self
                .options
                .unsupported_js_features
                .contains(JsFeature::UNICODE_ESCAPES)
            && contains_non_bmp_code_point(name.as_bytes())
            && self
                .unrepresentable_identifiers
                .insert(name.into(), true)
                .is_none()
        {
            let environment = pretty_print_target_environment(
                &self.options.original_target_env,
                self.options.unsupported_js_feature_overrides_mask,
            );
            self.add_error(
                loc,
                format!(
                    "{name:?} cannot be escaped in {environment} but you can set the charset to \
                     \"utf8\" to allow unescaped Unicode characters"
                ),
            );
        }
    }

    fn add_symbol_already_declared_error(&mut self, name: &str, new_loc: Loc, old_loc: Loc) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let note = self.tracker.msg_data(
            range_of_identifier(&self.source, old_loc),
            format!("The symbol {name:?} was originally declared here:"),
        );
        log.add_error_with_notes(
            Some(&mut self.tracker),
            range_of_identifier(&self.source, new_loc),
            format!("The symbol {name:?} has already been declared"),
            vec![note],
        );
    }

    pub(crate) fn add_error_range(&mut self, range: Range, text: impl Into<String>) {
        if let Some(log) = &self.log {
            log.add_error(Some(&mut self.tracker), range, text);
        }
    }

    pub(crate) fn add_warning_range(&mut self, range: Range, text: impl Into<String>) {
        if let Some(log) = &self.log {
            log.add_id(
                MsgId::None,
                MsgKind::Warning,
                Some(&mut self.tracker),
                range,
                text,
            );
        }
    }

    fn add_error(&mut self, loc: Loc, text: impl Into<String>) {
        if self.log.is_none() {
            return;
        }
        self.add_error_range(range_of_identifier(&self.source, loc), text);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FindSymbolResult {
    pub(crate) reference: Ref,
    pub(crate) declare_loc: Loc,
    pub(crate) is_inside_with_scope: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WasOriginallyDotOrIndex {
    Dot,
    Index,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regex::Regex;

    use super::{ParserCore, WasOriginallyDotOrIndex};
    use crate::internal::{
        ast::{INVALID_REF, Index32, SymbolFlags, SymbolKind},
        js_ast::{
            DotExpr, Expr, ExprData, IdentifierExpr, IndexExpr, OptionalChain, ScopeKind,
            ScopeMember, StringExpr, TsNamespaceMember, TsNamespaceMemberData, TsNamespaceScope,
        },
        js_lexer::MaybeSubstring,
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

    #[test]
    fn resolves_symbols_through_with_scopes_and_allocates_unbound_names() {
        let mut parser = parser();
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        let local = parser.new_symbol(SymbolKind::Other, "local");
        parser
            .current_scope
            .as_ref()
            .unwrap()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .insert(
                "local".into(),
                ScopeMember {
                    reference: local,
                    loc: Loc { start: 1 },
                },
            );
        parser.push_scope_for_parse_pass(ScopeKind::With, Loc { start: 2 });

        let result = parser.find_symbol(Loc { start: 2 }, "local");
        assert_eq!(result.reference, local);
        assert!(result.is_inside_with_scope);
        assert!(
            parser.symbols[0]
                .flags
                .contains(SymbolFlags::MUST_NOT_BE_RENAMED)
        );

        let unbound = parser.find_symbol(Loc { start: 2 }, "missing");
        assert_eq!(
            parser.symbols[usize::try_from(unbound.reference.inner_index).unwrap()].kind,
            SymbolKind::Unbound
        );
        assert_eq!(
            parser
                .module_scope
                .as_ref()
                .unwrap()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .members["missing"]
                .loc
                .start,
            -1
        );
    }

    #[test]
    fn lazily_generates_typescript_namespace_property_aliases() {
        let mut parser = parser();
        parser.options.ts.parse = true;
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        let namespace_ref = parser.new_symbol(SymbolKind::Other, "namespace");
        {
            let mut scope = parser
                .current_scope
                .as_ref()
                .unwrap()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut namespace = TsNamespaceScope {
                argument_ref: namespace_ref,
                ..TsNamespaceScope::default()
            };
            namespace.exported_members.insert(
                "member".into(),
                TsNamespaceMember {
                    data: TsNamespaceMemberData::Property,
                    loc: Loc { start: 1 },
                    is_enum_value: false,
                },
            );
            scope.ts_namespace = Some(namespace);
        }

        let first = parser.find_symbol(Loc { start: 1 }, "member");
        let second = parser.find_symbol(Loc { start: 1 }, "member");
        assert_eq!(first.reference, second.reference);
        let alias = parser.symbols[usize::try_from(first.reference.inner_index).unwrap()]
            .namespace_alias
            .as_ref()
            .unwrap();
        assert_eq!(alias.namespace_ref, namespace_ref);
        assert_eq!(alias.alias, "member");
    }

    #[test]
    fn resolves_containing_labels_and_recovers_from_missing_labels() {
        let mut parser = parser();
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        parser.push_scope_for_parse_pass(ScopeKind::Label, Loc { start: 2 });
        let label = parser.new_symbol(SymbolKind::Label, "loop");
        {
            let mut scope = parser
                .current_scope
                .as_ref()
                .unwrap()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scope.label.reference = label;
            scope.label_stmt_is_loop = true;
        }
        parser.push_scope_for_parse_pass(ScopeKind::Block, Loc { start: 3 });
        assert_eq!(
            parser.find_label_symbol(Loc { start: 3 }, "loop"),
            (label, true, true)
        );
        let (missing, is_loop, found) = parser.find_label_symbol(Loc { start: 3 }, "missing");
        assert!(!found);
        assert!(!is_loop);
        assert_eq!(
            parser.symbols[usize::try_from(missing.inner_index).unwrap()].kind,
            SymbolKind::Unbound
        );
    }

    #[test]
    fn declarations_update_replacements_and_private_accessor_pairs() {
        let mut parser = parser();
        parser.options.minify_syntax = true;
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });

        let first =
            parser.declare_symbol(SymbolKind::HoistedFunction, Loc { start: 2 }, "function");
        let second =
            parser.declare_symbol(SymbolKind::HoistedFunction, Loc { start: 3 }, "function");
        assert_ne!(first, second);
        assert_eq!(
            parser.symbols[usize::try_from(first.inner_index).unwrap()].link,
            second
        );
        assert!(
            parser.symbols[usize::try_from(first.inner_index).unwrap()]
                .flags
                .contains(SymbolFlags::REMOVE_OVERWRITTEN_FUNCTION_DECLARATION)
        );

        let getter = parser.declare_symbol(SymbolKind::PrivateGet, Loc { start: 4 }, "#accessor");
        let setter = parser.declare_symbol(SymbolKind::PrivateSet, Loc { start: 5 }, "#accessor");
        assert_eq!(getter, setter);
        assert_eq!(
            parser.symbols[usize::try_from(getter.inner_index).unwrap()].kind,
            SymbolKind::PrivateGetSetPair
        );
    }

    #[test]
    fn forbidden_declarations_reuse_the_existing_scope_member() {
        let mut parser = parser();
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        let first = parser.declare_symbol(SymbolKind::Const, Loc { start: 2 }, "value");
        let second = parser.declare_symbol(SymbolKind::Class, Loc { start: 3 }, "value");
        assert_eq!(first, second);
        assert_eq!(
            parser
                .current_scope
                .as_ref()
                .unwrap()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .members["value"]
                .reference,
            first
        );
    }

    #[test]
    fn runtime_imports_are_generated_once_and_count_each_use() {
        let mut parser = parser();
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        let first = parser.import_from_runtime(Loc { start: 2 }, "__helper");
        let call = parser.call_runtime(
            Loc { start: 3 },
            "__helper",
            vec![Expr::new(Loc::default(), ExprData::Number(1.0))],
        );
        let Some(ExprData::Identifier(first)) = first.data.as_deref() else {
            panic!("expected runtime identifier");
        };
        let Some(ExprData::Call(call)) = call.data.as_deref() else {
            panic!("expected runtime call");
        };
        let Some(ExprData::Identifier(target)) = call.target.data.as_deref() else {
            panic!("expected runtime call target");
        };
        assert_eq!(first.reference, target.reference);
        assert_eq!(parser.runtime_imports.len(), 1);
        assert_eq!(
            parser.symbols[usize::try_from(first.reference.inner_index).unwrap()]
                .use_count_estimate,
            2
        );
        assert_eq!(
            parser
                .module_scope
                .as_ref()
                .unwrap()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generated,
            [first.reference]
        );
    }

    #[test]
    fn special_global_references_match_upstream_generation_rules() {
        let mut parser = parser();
        parser.push_scope_for_parse_pass(ScopeKind::Entry, Loc { start: 1 });
        let promise = parser.make_promise_ref();
        let regexp = parser.make_reg_exp_ref();
        let bigint = parser.make_big_int_ref();
        assert_eq!(parser.make_promise_ref(), promise);
        assert_eq!(parser.make_reg_exp_ref(), regexp);
        assert_eq!(parser.make_big_int_ref(), bigint);
        assert_eq!(
            parser
                .module_scope
                .as_ref()
                .unwrap()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generated,
            [regexp, bigint]
        );
    }

    #[test]
    fn compact_name_refs_round_trip_source_and_allocated_names() {
        let mut parser = ParserCore::new(
            Source {
                contents: std::sync::Arc::from(&b"let source_name"[..]),
                ..Source::default()
            },
            super::Options::default(),
        );
        let source_ref = parser.store_name_in_ref(MaybeSubstring {
            string: b"source_name".to_vec(),
            start: Index32::new(4),
        });
        assert_eq!(parser.load_name_from_ref(source_ref), b"source_name");

        let allocated_ref =
            parser.store_name_in_ref(MaybeSubstring::from_allocated(b"escaped".to_vec()));
        assert_eq!(parser.load_name_from_ref(allocated_ref), b"escaped");
        assert_eq!(parser.allocated_names.len(), 1);
    }

    #[test]
    fn property_mangling_respects_reserved_names_and_quoted_mode() {
        let mut parser = parser();
        parser.options.mangle_props = Some(Arc::new(
            Regex::new("^_").expect("valid regular expression"),
        ));
        parser.options.reserve_props = Some(Arc::new(
            Regex::new("^_keep$").expect("valid regular expression"),
        ));

        assert!(parser.is_mangled_prop("_value"));
        assert!(!parser.is_mangled_prop("_keep"));
        assert!(!parser.is_mangled_prop("constructor"));
        assert!(parser.reserved_props.contains_key("_keep"));
        assert!(parser.reserved_props.contains_key("constructor"));

        let mangled = parser.dot_or_mangled_prop_parse(
            Expr::default(),
            MaybeSubstring::from_allocated(b"_value".to_vec()),
            Loc::default(),
            OptionalChain::None,
            WasOriginallyDotOrIndex::Dot,
        );
        assert!(matches!(mangled, ExprData::Index(_)));

        let quoted = parser.dot_or_mangled_prop_parse(
            Expr::default(),
            MaybeSubstring::from_allocated(b"_quoted".to_vec()),
            Loc::default(),
            OptionalChain::None,
            WasOriginallyDotOrIndex::Index,
        );
        assert!(matches!(quoted, ExprData::Dot(_)));
    }

    #[test]
    fn mangled_property_symbols_are_reused_and_not_counted_in_dead_code() {
        let mut parser = parser();
        let first = parser.symbol_for_mangled_prop("_value");
        let second = parser.symbol_for_mangled_prop("_value");
        assert_eq!(first, second);
        assert_eq!(parser.symbols[0].use_count_estimate, 2);
        parser.is_control_flow_dead = true;
        assert_eq!(parser.symbol_for_mangled_prop("_value"), first);
        assert_eq!(parser.symbols[0].use_count_estimate, 2);
    }

    #[test]
    fn validates_assignment_targets_and_strict_mode_names() {
        let mut parser = parser();
        let eval_ref = parser.store_name_in_ref(MaybeSubstring::from_allocated(b"eval".to_vec()));
        let identifier = Expr::new(
            Loc::default(),
            ExprData::Identifier(IdentifierExpr {
                reference: eval_ref,
                ..IdentifierExpr::default()
            }),
        );
        assert!(parser.is_valid_assignment_target(&identifier, false));
        assert!(!parser.is_valid_assignment_target(&identifier, true));

        let optional = Expr::new(
            Loc::default(),
            ExprData::Dot(DotExpr {
                optional_chain: OptionalChain::Start,
                ..DotExpr::default()
            }),
        );
        assert!(!parser.is_valid_assignment_target(&optional, false));
        assert!(
            !parser.is_valid_assignment_target(
                &Expr::new(Loc::default(), ExprData::Number(1.0)),
                false
            )
        );
    }
}
