//! Port of `internal/renamer`.

use crate::internal::ast::{
    DEFAULT_NAME_MINIFIER_JS, INVALID_REF, Index32, NameMinifier, NamespaceAlias, Ref, SlotCounts,
    SlotNamespace, Symbol, SymbolFlags, SymbolKind, SymbolMap,
};
use crate::internal::js_ast::{ScopeRef, SymbolUse};
use crate::internal::js_lexer::{KEYWORDS, STRICT_MODE_RESERVED_WORDS, is_keyword};
use std::collections::HashMap;

#[must_use]
pub fn compute_reserved_names(
    module_scopes: &[ScopeRef],
    symbols: &SymbolMap,
) -> HashMap<String, u32> {
    let mut names = HashMap::new();
    names.extend(KEYWORDS.iter().map(|name| ((*name).to_string(), 1)));
    names.extend(
        STRICT_MODE_RESERVED_WORDS
            .iter()
            .map(|name| ((*name).to_string(), 1)),
    );
    for scope in module_scopes {
        compute_reserved_names_for_scope(scope, symbols, &mut names);
    }
    names
}

fn compute_reserved_names_for_scope(
    scope: &ScopeRef,
    symbols: &SymbolMap,
    names: &mut HashMap<String, u32>,
) {
    let scope = scope
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for member in scope.members.values() {
        let symbol = symbols.get(member.reference);
        if symbol.kind == SymbolKind::Unbound
            || symbol.flags.contains(SymbolFlags::MUST_NOT_BE_RENAMED)
        {
            names.insert(symbol.original_name.clone(), 1);
        }
    }
    for &reference in &scope.generated {
        let symbol = symbols.get(reference);
        if symbol.kind == SymbolKind::Unbound
            || symbol.flags.contains(SymbolFlags::MUST_NOT_BE_RENAMED)
        {
            names.insert(symbol.original_name.clone(), 1);
        }
    }
    if scope.contains_direct_eval {
        for child in &scope.children {
            if child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_direct_eval
            {
                compute_reserved_names_for_scope(child, symbols, names);
            }
        }
    }
}

pub trait Renamer {
    fn name_for_symbol(&self, reference: Ref) -> String;

    fn original_name_for_symbol(&self, reference: Ref) -> String {
        self.name_for_symbol(reference)
    }

    fn namespace_alias_for_symbol(&self, _reference: Ref) -> Option<NamespaceAlias> {
        None
    }
}

pub struct NoOpRenamer {
    symbols: SymbolMap,
}

#[must_use]
pub fn new_no_op_renamer(symbols: SymbolMap) -> NoOpRenamer {
    NoOpRenamer { symbols }
}

impl Renamer for NoOpRenamer {
    fn name_for_symbol(&self, reference: Ref) -> String {
        let reference = self.symbols.follow_symbols_const(reference);
        self.symbols.get(reference).original_name.clone()
    }

    fn namespace_alias_for_symbol(&self, reference: Ref) -> Option<NamespaceAlias> {
        let reference = self.symbols.follow_symbols_const(reference);
        self.symbols.get(reference).namespace_alias.clone()
    }
}

#[derive(Clone, Debug, Default)]
struct SymbolSlot {
    name: String,
    count: u32,
    needs_capital_for_jsx: bool,
}

pub struct MinifyRenamer {
    reserved_names: HashMap<String, u32>,
    slots: [Vec<SymbolSlot>; 4],
    top_level_symbol_to_slot: HashMap<Ref, u32>,
    symbols: SymbolMap,
}

impl MinifyRenamer {
    #[must_use]
    pub fn new(
        symbols: SymbolMap,
        first_top_level_slots: SlotCounts,
        reserved_names: HashMap<String, u32>,
    ) -> Self {
        Self {
            reserved_names,
            slots: std::array::from_fn(|index| {
                vec![SymbolSlot::default(); first_top_level_slots.0[index] as usize]
            }),
            top_level_symbol_to_slot: HashMap::new(),
            symbols,
        }
    }

    pub fn accumulate_symbol_use_counts(
        &mut self,
        top_level_symbols: &mut Vec<StableSymbolCount>,
        symbol_uses: &HashMap<Ref, SymbolUse>,
        stable_source_indices: &[u32],
    ) {
        for (&reference, symbol_use) in symbol_uses {
            self.accumulate_symbol_count(
                top_level_symbols,
                reference,
                symbol_use.count_estimate,
                stable_source_indices,
            );
        }
    }

    pub fn accumulate_symbol_count(
        &mut self,
        top_level_symbols: &mut Vec<StableSymbolCount>,
        mut reference: Ref,
        count: u32,
        stable_source_indices: &[u32],
    ) {
        reference = self.symbols.follow_symbols_const(reference);
        let mut symbol = self.symbols.get(reference);
        while let Some(alias) = &symbol.namespace_alias {
            reference = self.symbols.follow_symbols_const(alias.namespace_ref);
            symbol = self.symbols.get(reference);
        }

        let namespace = symbol.slot_namespace();
        if namespace == SlotNamespace::MustNotBeRenamed {
            return;
        }
        if symbol.nested_scope_slot.is_valid() {
            let slot =
                &mut self.slots[namespace as usize][symbol.nested_scope_slot.get_index() as usize];
            slot.count = slot.count.wrapping_add(count);
            if symbol
                .flags
                .contains(SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX)
            {
                slot.needs_capital_for_jsx = true;
            }
            return;
        }

        top_level_symbols.push(StableSymbolCount {
            stable_source_index: stable_source_indices[reference.source_index as usize],
            reference,
            count,
        });
    }

    /// # Panics
    ///
    /// Panics if the total number of slots exceeds esbuild's 32-bit index space.
    pub fn allocate_top_level_symbol_slots(&mut self, top_level_symbols: &[StableSymbolCount]) {
        for stable in top_level_symbols {
            let symbol = self.symbols.get(stable.reference);
            let namespace = symbol.slot_namespace();
            let slots = &mut self.slots[namespace as usize];
            if let Some(&index) = self.top_level_symbol_to_slot.get(&stable.reference) {
                let slot = &mut slots[index as usize];
                slot.count = slot.count.wrapping_add(stable.count);
                if symbol
                    .flags
                    .contains(SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX)
                {
                    slot.needs_capital_for_jsx = true;
                }
            } else {
                let index = u32::try_from(slots.len()).expect("slot index must fit in u32");
                slots.push(SymbolSlot {
                    count: stable.count,
                    needs_capital_for_jsx: symbol
                        .flags
                        .contains(SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX),
                    ..SymbolSlot::default()
                });
                self.top_level_symbol_to_slot
                    .insert(stable.reference, index);
            }
        }
    }

    pub fn accumulate_synthetic_default_nested_slot(&mut self, slot: usize, count: u32) {
        let slots = &mut self.slots[SlotNamespace::Default as usize];
        if slots.len() <= slot {
            slots.resize_with(slot + 1, SymbolSlot::default);
        }
        slots[slot].count = slots[slot].count.wrapping_add(count);
    }

    #[must_use]
    pub fn allocate_synthetic_default_top_level_slot(&mut self, count: u32) -> usize {
        let slots = &mut self.slots[SlotNamespace::Default as usize];
        let slot = slots.len();
        slots.push(SymbolSlot {
            count,
            ..SymbolSlot::default()
        });
        slot
    }

    #[must_use]
    pub fn name_for_synthetic_default_slot(&self, slot: usize) -> String {
        self.slots[SlotNamespace::Default as usize][slot]
            .name
            .clone()
    }

    pub fn assign_names_by_frequency(&mut self, minifier: &NameMinifier) {
        for (namespace_index, slots) in self.slots.iter_mut().enumerate() {
            let mut sorted: Vec<_> = slots
                .iter()
                .enumerate()
                .map(|(slot, value)| (slot, value.count))
                .collect();
            sorted.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

            let namespace = match namespace_index {
                0 => SlotNamespace::Default,
                1 => SlotNamespace::Label,
                2 => SlotNamespace::PrivateName,
                3 => SlotNamespace::MangledProp,
                _ => unreachable!(),
            };
            let mut next_name = 0;
            for (slot_index, _) in sorted {
                let slot = &mut slots[slot_index];
                let mut name = minifier.number_to_minified_name(next_name);
                next_name += 1;
                match namespace {
                    SlotNamespace::Default => {
                        while self.reserved_names.contains_key(&name) {
                            name = minifier.number_to_minified_name(next_name);
                            next_name += 1;
                        }
                        if slot.needs_capital_for_jsx {
                            while name.as_bytes()[0].is_ascii_lowercase() {
                                name = minifier.number_to_minified_name(next_name);
                                next_name += 1;
                            }
                        }
                    }
                    SlotNamespace::Label => {
                        while is_keyword(&name) {
                            name = minifier.number_to_minified_name(next_name);
                            next_name += 1;
                        }
                    }
                    _ => {}
                }
                if namespace == SlotNamespace::PrivateName {
                    name.insert(0, '#');
                }
                slot.name = name;
            }
        }
    }
}

impl Renamer for MinifyRenamer {
    fn name_for_symbol(&self, reference: Ref) -> String {
        let reference = self.symbols.follow_symbols_const(reference);
        let symbol = self.symbols.get(reference);
        let namespace = symbol.slot_namespace();
        if namespace == SlotNamespace::MustNotBeRenamed {
            return symbol.original_name.clone();
        }
        let index = if symbol.nested_scope_slot.is_valid() {
            symbol.nested_scope_slot.get_index()
        } else if let Some(&index) = self.top_level_symbol_to_slot.get(&reference) {
            index
        } else {
            return symbol.original_name.clone();
        };
        self.slots[namespace as usize][index as usize].name.clone()
    }

    fn original_name_for_symbol(&self, reference: Ref) -> String {
        let reference = self.symbols.follow_symbols_const(reference);
        self.symbols.get(reference).original_name.clone()
    }

    fn namespace_alias_for_symbol(&self, reference: Ref) -> Option<NamespaceAlias> {
        let reference = self.symbols.follow_symbols_const(reference);
        self.symbols.get(reference).namespace_alias.clone()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StableSymbolCount {
    pub stable_source_index: u32,
    pub reference: Ref,
    pub count: u32,
}

pub fn sort_stable_symbol_counts(values: &mut [StableSymbolCount]) {
    values.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.stable_source_index.cmp(&right.stable_source_index))
            .then_with(|| left.reference.inner_index.cmp(&right.reference.inner_index))
    });
}

/// # Panics
///
/// Panics if a scope references a symbol outside `symbols`.
pub fn assign_nested_scope_slots(module_scope: &ScopeRef, symbols: &mut [Symbol]) -> SlotCounts {
    let (members, generated, children) = scope_parts(module_scope);
    let valid_slot = Index32::new(1);
    for reference in members.iter().chain(&generated) {
        symbols[reference.inner_index as usize].nested_scope_slot = valid_slot;
    }

    let mut slot_counts = SlotCounts::default();
    for child in children {
        slot_counts.union_max(assign_nested_scope_slots_helper(
            &child,
            symbols,
            SlotCounts::default(),
        ));
    }

    for reference in members.iter().chain(&generated) {
        symbols[reference.inner_index as usize].nested_scope_slot = Index32::default();
    }
    slot_counts
}

fn assign_nested_scope_slots_helper(
    scope: &ScopeRef,
    symbols: &mut [Symbol],
    mut slot: SlotCounts,
) -> SlotCounts {
    let (members, generated, children) = scope_parts(scope);
    for reference in members.iter().chain(&generated) {
        let symbol = &mut symbols[reference.inner_index as usize];
        let namespace = symbol.slot_namespace();
        if namespace != SlotNamespace::MustNotBeRenamed && !symbol.nested_scope_slot.is_valid() {
            symbol.nested_scope_slot = Index32::new(slot.0[namespace as usize]);
            slot.0[namespace as usize] += 1;
        }
    }

    let label = scope
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .label
        .reference;
    if label != INVALID_REF {
        let symbol = &mut symbols[label.inner_index as usize];
        symbol.nested_scope_slot = Index32::new(slot.0[SlotNamespace::Label as usize]);
        slot.0[SlotNamespace::Label as usize] += 1;
    }

    let mut slot_counts = slot;
    for child in children {
        slot_counts.union_max(assign_nested_scope_slots_helper(&child, symbols, slot));
    }
    slot_counts
}

fn scope_parts(scope: &ScopeRef) -> (Vec<Ref>, Vec<Ref>, Vec<ScopeRef>) {
    let scope = scope
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let namespace_argument = scope
        .ts_namespace
        .as_ref()
        .map(|namespace| namespace.argument_ref);
    let mut members = scope
        .members
        .values()
        .map(|member| member.reference)
        .collect::<Vec<_>>();
    members.sort_by_key(|reference| {
        (
            namespace_argument == Some(*reference),
            reference.inner_index,
        )
    });
    (members, scope.generated.clone(), scope.children.clone())
}

pub struct NumberRenamer {
    symbols: SymbolMap,
    scopes: Vec<NumberScope>,
    names: Vec<Vec<String>>,
}

impl NumberRenamer {
    #[must_use]
    pub fn new(symbols: SymbolMap, reserved_names: HashMap<String, u32>) -> Self {
        let names = symbols
            .symbols_for_source
            .iter()
            .map(|symbols| vec![String::new(); symbols.len()])
            .collect();
        Self {
            symbols,
            scopes: vec![NumberScope {
                parent: None,
                name_counts: reserved_names,
            }],
            names,
        }
    }

    pub fn add_top_level_symbol(&mut self, reference: Ref) {
        self.assign_name(0, reference);
    }

    pub fn assign_names_by_scope(&mut self, nested_scopes: &HashMap<u32, Vec<ScopeRef>>) {
        let mut sources: Vec<_> = nested_scopes.iter().collect();
        sources.sort_by_key(|(source_index, _)| **source_index);
        for (&source_index, scopes) in sources {
            for scope in scopes {
                self.assign_names_recursive(scope.clone(), source_index, 0);
            }
        }
    }

    fn assign_name(&mut self, scope_index: usize, reference: Ref) {
        let reference = self.symbols.follow_symbols_const(reference);
        if !self.names[reference.source_index as usize][reference.inner_index as usize].is_empty() {
            return;
        }

        let symbol = self.symbols.get(reference);
        let namespace = symbol.slot_namespace();
        if !matches!(
            namespace,
            SlotNamespace::Default | SlotNamespace::PrivateName
        ) {
            return;
        }
        let mut original_name = symbol.original_name.clone();
        if symbol
            .flags
            .contains(SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX)
            && original_name
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_lowercase)
        {
            original_name.replace_range(0..1, &original_name[..1].to_ascii_uppercase());
        }

        let name = self.find_unused_name(scope_index, original_name, namespace);
        self.names[reference.source_index as usize][reference.inner_index as usize] = name;
    }

    fn assign_names_in_scope(
        &mut self,
        scope: &ScopeRef,
        source_index: u32,
        parent: usize,
    ) -> usize {
        let (members, generated, _) = scope_parts(scope);
        let scope_index = self.scopes.len();
        self.scopes.push(NumberScope {
            parent: Some(parent),
            name_counts: HashMap::new(),
        });
        for member in members {
            self.assign_name(
                scope_index,
                Ref {
                    source_index,
                    inner_index: member.inner_index,
                },
            );
        }
        for reference in generated {
            self.assign_name(scope_index, reference);
        }
        scope_index
    }

    fn assign_names_recursive(
        &mut self,
        mut scope: ScopeRef,
        source_index: u32,
        mut parent: usize,
    ) {
        loop {
            let (members, generated, children) = scope_parts(&scope);
            if !members.is_empty() || !generated.is_empty() {
                parent = self.assign_names_in_scope(&scope, source_index, parent);
            }
            if let [only_child] = children.as_slice() {
                scope = only_child.clone();
                continue;
            }
            for child in children {
                self.assign_names_recursive(child, source_index, parent);
            }
            break;
        }
    }

    fn find_name_use(&self, mut scope_index: usize, name: &str) -> NameUse {
        let original = scope_index;
        loop {
            let scope = &self.scopes[scope_index];
            if scope.name_counts.contains_key(name) {
                return if scope_index == original {
                    NameUse::UsedInSameScope
                } else {
                    NameUse::Used
                };
            }
            let Some(parent) = scope.parent else {
                return NameUse::Unused;
            };
            scope_index = parent;
        }
    }

    fn find_unused_name(
        &mut self,
        scope_index: usize,
        mut name: String,
        namespace: SlotNamespace,
    ) -> String {
        if namespace == SlotNamespace::PrivateName {
            let identifier = name.strip_prefix('#').unwrap_or(&name);
            if !crate::internal::js_ast::is_identifier(identifier) {
                name = crate::internal::js_ast::force_valid_identifier("#", identifier);
            }
        } else if !crate::internal::js_ast::is_identifier(&name) {
            name = crate::internal::js_ast::force_valid_identifier("", &name);
        }

        let name_use = self.find_name_use(scope_index, &name);
        if name_use != NameUse::Unused {
            let prefix = name;
            let mut tries = if name_use == NameUse::UsedInSameScope {
                self.scopes[scope_index].name_counts[&prefix]
            } else {
                1
            };
            loop {
                tries += 1;
                name = format!("{prefix}{tries}");
                if self.find_name_use(scope_index, &name) == NameUse::Unused {
                    if name_use == NameUse::UsedInSameScope {
                        self.scopes[scope_index].name_counts.insert(prefix, tries);
                    }
                    break;
                }
            }
        }
        self.scopes[scope_index].name_counts.insert(name.clone(), 1);
        name
    }
}

impl Renamer for NumberRenamer {
    fn name_for_symbol(&self, reference: Ref) -> String {
        let reference = self.symbols.follow_symbols_const(reference);
        let name = &self.names[reference.source_index as usize][reference.inner_index as usize];
        if name.is_empty() {
            self.symbols.get(reference).original_name.clone()
        } else {
            name.clone()
        }
    }

    fn original_name_for_symbol(&self, reference: Ref) -> String {
        let reference = self.symbols.follow_symbols_const(reference);
        self.symbols.get(reference).original_name.clone()
    }

    fn namespace_alias_for_symbol(&self, reference: Ref) -> Option<NamespaceAlias> {
        let reference = self.symbols.follow_symbols_const(reference);
        self.symbols.get(reference).namespace_alias.clone()
    }
}

struct NumberScope {
    parent: Option<usize>,
    name_counts: HashMap<String, u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NameUse {
    Unused,
    Used,
    UsedInSameScope,
}

#[derive(Debug, Default)]
pub struct ExportRenamer {
    used: HashMap<String, u32>,
    count: usize,
}

impl ExportRenamer {
    pub fn next_renamed_name(&mut self, original_name: &str) -> String {
        let mut name = original_name.to_string();
        if let Some(&original_tries) = self.used.get(original_name) {
            let mut tries = original_tries;
            loop {
                tries += 1;
                name = format!("{original_name}{tries}");
                if !self.used.contains_key(&name) {
                    self.used.insert(name.clone(), tries);
                    break;
                }
            }
        } else {
            self.used.insert(name.clone(), 1);
        }
        name
    }

    pub fn next_minified_name(&mut self) -> String {
        let name = DEFAULT_NAME_MINIFIER_JS.number_to_minified_name(self.count);
        self.count += 1;
        name
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExportRenamer, MinifyRenamer, NumberRenamer, Renamer, StableSymbolCount,
        assign_nested_scope_slots, compute_reserved_names, new_no_op_renamer,
        sort_stable_symbol_counts,
    };
    use crate::internal::ast::{
        DEFAULT_NAME_MINIFIER_JS, Ref, SlotCounts, Symbol, SymbolFlags, SymbolKind, SymbolMap,
    };
    use crate::internal::js_ast::{Scope, ScopeMember};
    use crate::internal::logger::Loc;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn symbol_map(symbols: Vec<Symbol>) -> SymbolMap {
        SymbolMap {
            symbols_for_source: vec![symbols],
        }
    }

    #[test]
    fn reserves_keywords_and_unbound_names_through_eval_scopes() {
        let unbound = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let child = Arc::new(Mutex::new(Scope {
            members: HashMap::from([(
                "global".into(),
                ScopeMember {
                    reference: unbound,
                    loc: Loc::default(),
                },
            )]),
            contains_direct_eval: true,
            ..Scope::default()
        }));
        let root = Arc::new(Mutex::new(Scope {
            children: vec![child],
            contains_direct_eval: true,
            ..Scope::default()
        }));
        let names = compute_reserved_names(
            &[root],
            &symbol_map(vec![Symbol::new(SymbolKind::Unbound, "global")]),
        );
        assert!(names.contains_key("break"));
        assert!(names.contains_key("yield"));
        assert!(names.contains_key("global"));
    }

    #[test]
    fn no_op_renamer_follows_symbol_links() {
        let first = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let second = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let mut symbols = vec![
            Symbol::new(SymbolKind::Other, "old"),
            Symbol::new(SymbolKind::Other, "new"),
        ];
        symbols[0].link = second;
        let renamer = new_no_op_renamer(symbol_map(symbols));
        assert_eq!(renamer.name_for_symbol(first), "new");
    }

    #[test]
    fn minifier_assigns_frequent_names_and_private_prefixes() {
        let hot = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let cold = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let private = Ref {
            source_index: 0,
            inner_index: 2,
        };
        let symbols = symbol_map(vec![
            Symbol::new(SymbolKind::Other, "hot"),
            Symbol::new(SymbolKind::Other, "cold"),
            Symbol::new(SymbolKind::PrivateField, "#field"),
        ]);
        let mut renamer = MinifyRenamer::new(
            symbols,
            SlotCounts::default(),
            HashMap::from([("a".into(), 1)]),
        );
        let stable = vec![
            StableSymbolCount {
                stable_source_index: 0,
                reference: hot,
                count: 10,
            },
            StableSymbolCount {
                stable_source_index: 0,
                reference: cold,
                count: 1,
            },
            StableSymbolCount {
                stable_source_index: 0,
                reference: private,
                count: 5,
            },
        ];
        renamer.allocate_top_level_symbol_slots(&stable);
        renamer.assign_names_by_frequency(&DEFAULT_NAME_MINIFIER_JS);
        assert_eq!(renamer.name_for_symbol(hot), "b");
        assert_eq!(renamer.name_for_symbol(cold), "c");
        assert_eq!(renamer.name_for_symbol(private), "#a");
    }

    #[test]
    fn stable_counts_and_nested_slots_are_deterministic() {
        let mut counts = [
            StableSymbolCount {
                stable_source_index: 2,
                reference: Ref {
                    source_index: 0,
                    inner_index: 2,
                },
                count: 1,
            },
            StableSymbolCount {
                stable_source_index: 1,
                reference: Ref {
                    source_index: 0,
                    inner_index: 1,
                },
                count: 2,
            },
        ];
        sort_stable_symbol_counts(&mut counts);
        assert_eq!(counts[0].reference.inner_index, 1);

        let top = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let nested = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let child = Arc::new(Mutex::new(Scope {
            members: HashMap::from([(
                "nested".into(),
                ScopeMember {
                    reference: nested,
                    loc: Loc::default(),
                },
            )]),
            ..Scope::default()
        }));
        let module = Arc::new(Mutex::new(Scope {
            members: HashMap::from([(
                "top".into(),
                ScopeMember {
                    reference: top,
                    loc: Loc::default(),
                },
            )]),
            children: vec![child],
            ..Scope::default()
        }));
        let mut symbols = vec![
            Symbol::new(SymbolKind::Other, "top"),
            Symbol::new(SymbolKind::Other, "nested"),
        ];
        let slots = assign_nested_scope_slots(&module, &mut symbols);
        assert_eq!(slots.0[0], 1);
        assert!(!symbols[0].nested_scope_slot.is_valid());
        assert_eq!(symbols[1].nested_scope_slot.get_index(), 0);
    }

    #[test]
    fn export_renamer_avoids_collisions() {
        let mut renamer = ExportRenamer::default();
        assert_eq!(renamer.next_renamed_name("x"), "x");
        assert_eq!(renamer.next_renamed_name("x"), "x2");
        assert_eq!(renamer.next_renamed_name("x"), "x3");
        assert_eq!(renamer.next_minified_name(), "a");
        assert_eq!(renamer.next_minified_name(), "b");
    }

    #[test]
    fn numbered_renamer_tracks_scope_collisions_and_reuses_sibling_names() {
        let references: Vec<_> = (0..5)
            .map(|inner_index| Ref {
                source_index: 0,
                inner_index,
            })
            .collect();
        let mut jsx = Symbol::new(SymbolKind::Other, "widget");
        jsx.flags |= SymbolFlags::MUST_START_WITH_CAPITAL_LETTER_FOR_JSX;
        let symbols = symbol_map(vec![
            Symbol::new(SymbolKind::Other, "foo"),
            Symbol::new(SymbolKind::Other, "bar"),
            Symbol::new(SymbolKind::Other, "bar"),
            jsx,
            Symbol::new(SymbolKind::PrivateField, "#secret"),
        ]);
        let mut renamer = NumberRenamer::new(symbols, HashMap::from([("foo".into(), 1)]));
        renamer.add_top_level_symbol(references[0]);
        renamer.add_top_level_symbol(references[3]);
        renamer.add_top_level_symbol(references[4]);
        assert_eq!(renamer.name_for_symbol(references[0]), "foo2");
        assert_eq!(renamer.name_for_symbol(references[3]), "Widget");
        assert_eq!(renamer.name_for_symbol(references[4]), "#secret");

        let sibling = |reference| {
            Arc::new(Mutex::new(Scope {
                members: HashMap::from([(
                    "bar".into(),
                    ScopeMember {
                        reference,
                        loc: Loc::default(),
                    },
                )]),
                ..Scope::default()
            }))
        };
        renamer.assign_names_by_scope(&HashMap::from([(
            0,
            vec![sibling(references[1]), sibling(references[2])],
        )]));
        assert_eq!(renamer.name_for_symbol(references[1]), "bar");
        assert_eq!(renamer.name_for_symbol(references[2]), "bar");
    }

    #[test]
    fn numbered_renamer_uses_linear_collision_counters() {
        let symbols = symbol_map(
            (0..4)
                .map(|_| Symbol::new(SymbolKind::Other, "item"))
                .collect(),
        );
        let mut renamer = NumberRenamer::new(symbols, HashMap::new());
        for inner_index in 0..4 {
            renamer.add_top_level_symbol(Ref {
                source_index: 0,
                inner_index,
            });
        }
        assert_eq!(
            (0..4)
                .map(|inner_index| renamer.name_for_symbol(Ref {
                    source_index: 0,
                    inner_index,
                }))
                .collect::<Vec<_>>(),
            ["item", "item2", "item3", "item4"]
        );
    }
}
