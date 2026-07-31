//! Port of upstream `internal/graph`.

use std::{collections::HashMap, sync::Arc};

use crate::internal::{
    ast::{ImportKind, ImportRecord, Index32, Ref, Symbol, SymbolKind, SymbolMap},
    config::Loader,
    css_ast,
    helpers::{BitSet, TypoDetector},
    js_ast,
    logger::{LineColumnTracker, Loc, Source},
    resolver::SideEffectsData,
    runtime,
    sourcemap::{LineOffsetTable, SourceMap},
};

#[derive(Clone, Debug, Default)]
pub struct InputFile {
    pub repr: Option<InputFileRepr>,
    pub input_source_map: Option<SourceMap>,
    pub source_map_line_offset_tables: Arc<[LineOffsetTable]>,
    pub additional_files: Vec<OutputFile>,
    pub unique_key_for_additional_file: String,
    pub side_effects: SideEffects,
    pub source: Source,
    pub loader: Loader,
    pub omit_from_source_maps_and_metafile: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutputFile {
    pub json_metadata_chunk: String,
    pub abs_path: String,
    pub contents: Vec<u8>,
    pub is_executable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SideEffects {
    pub data: Option<SideEffectsData>,
    pub kind: SideEffectsKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SideEffectsKind {
    #[default]
    HasSideEffects,
    NoSideEffectsPackageJson,
    NoSideEffectsEmptyAst,
    NoSideEffectsPureData,
    NoSideEffectsPureDataFromPlugin,
}

#[derive(Clone, Debug)]
pub enum InputFileRepr {
    Js(Box<JsRepr>),
    Css(Box<CssRepr>),
    Copy(CopyRepr),
}

impl InputFileRepr {
    pub fn import_records_mut(&mut self) -> Option<&mut Vec<ImportRecord>> {
        match self {
            Self::Js(repr) => Some(&mut repr.ast.import_records),
            Self::Css(repr) => Some(&mut repr.ast.import_records),
            Self::Copy(_) => None,
        }
    }

    #[must_use]
    pub fn import_records(&self) -> Option<&[ImportRecord]> {
        match self {
            Self::Js(repr) => Some(&repr.ast.import_records),
            Self::Css(repr) => Some(&repr.ast.import_records),
            Self::Copy(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct JsRepr {
    pub meta: JsReprMeta,
    pub ast: js_ast::Ast,
    pub css_source_index: Index32,
}

impl JsRepr {
    #[must_use]
    pub fn top_level_symbol_to_parts(&self, reference: Ref) -> Option<&[u32]> {
        self.meta
            .top_level_symbol_to_parts_overlay
            .get(&reference)
            .or_else(|| {
                self.ast
                    .top_level_symbol_to_parts_from_parser
                    .get(&reference)
            })
            .map(Vec::as_slice)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CssRepr {
    pub ast: css_ast::Ast,
    pub js_source_index: Index32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CopyRepr {
    pub url_for_code: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WrapKind {
    #[default]
    None,
    Cjs,
    Esm,
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct JsReprMeta {
    pub is_probably_type_script_type: HashMap<Ref, bool>,
    pub imports_to_bind: HashMap<Ref, ImportData>,
    pub resolved_exports: HashMap<String, ExportData>,
    pub resolved_export_star: Option<ExportData>,
    pub resolved_export_typos: Option<TypoDetector>,
    pub sorted_and_filtered_export_aliases: Vec<String>,
    pub top_level_symbol_to_parts_overlay: HashMap<Ref, Vec<u32>>,
    pub cjs_export_copies: Vec<Ref>,
    pub wrapper_part_index: Index32,
    pub entry_point_part_index: Index32,
    pub is_async_or_has_async_dependency: bool,
    pub wrap: WrapKind,
    pub needs_exports_variable: bool,
    pub force_include_exports_for_entry_point: bool,
    pub needs_export_symbol_from_runtime: bool,
    pub did_wrap_dependencies: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ImportData {
    pub re_exports: Vec<js_ast::Dependency>,
    pub name_loc: Loc,
    pub reference: Ref,
    pub source_index: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ExportData {
    pub potentially_ambiguous_export_star_refs: Vec<ImportData>,
    pub reference: Ref,
    pub name_loc: Loc,
    pub source_index: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EntryPointKind {
    #[default]
    None,
    UserSpecified,
    DynamicImport,
}

#[derive(Clone, Debug)]
pub struct LinkerFile {
    pub entry_bits: BitSet,
    lazy_line_column_tracker: Option<LineColumnTracker>,
    pub input_file: InputFile,
    pub distance_from_entry_point: u32,
    pub entry_point_chunk_index: u32,
    entry_point_kind: EntryPointKind,
    pub is_live: bool,
}

impl Default for LinkerFile {
    fn default() -> Self {
        Self {
            entry_bits: BitSet::new(0),
            lazy_line_column_tracker: None,
            input_file: InputFile::default(),
            distance_from_entry_point: u32::MAX,
            entry_point_chunk_index: 0,
            entry_point_kind: EntryPointKind::None,
            is_live: false,
        }
    }
}

impl LinkerFile {
    #[must_use]
    pub const fn is_entry_point(&self) -> bool {
        !matches!(self.entry_point_kind, EntryPointKind::None)
    }

    #[must_use]
    pub const fn is_user_specified_entry_point(&self) -> bool {
        matches!(self.entry_point_kind, EntryPointKind::UserSpecified)
    }

    pub fn line_column_tracker(&mut self) -> &mut LineColumnTracker {
        self.lazy_line_column_tracker
            .get_or_insert_with(|| LineColumnTracker::new(Some(&self.input_file.source)))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EntryPoint {
    pub output_path: String,
    pub source_index: u32,
    pub output_path_was_auto_generated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LinkerGraph {
    pub files: Vec<LinkerFile>,
    entry_points: Vec<EntryPoint>,
    pub symbols: SymbolMap,
    pub ts_enums: HashMap<Ref, HashMap<String, js_ast::TsEnumValue>>,
    pub const_values: HashMap<Ref, js_ast::ConstValue>,
    pub reachable_files: Vec<u32>,
    pub stable_source_indices: Vec<u32>,
}

/// Clone the scan-phase graph into the mutable representation used by one
/// linker invocation.
///
/// # Panics
///
/// Panics when a source index is outside `input_files`, matching upstream.
#[must_use]
pub fn clone_linker_graph(
    input_files: &[InputFile],
    reachable_files: &[u32],
    original_entry_points: &[EntryPoint],
    code_splitting: bool,
) -> LinkerGraph {
    let mut entry_points = original_entry_points.to_vec();
    let mut symbols = SymbolMap::new(input_files.len());
    let mut files = vec![LinkerFile::default(); input_files.len()];

    for entry_point in &entry_points {
        files[entry_point.source_index as usize].entry_point_kind = EntryPointKind::UserSpecified;
    }

    let mut dynamic_import_entry_points = Vec::new();
    let mut stable_source_indices = vec![0; input_files.len()];
    for (stable_index, &source_index) in reachable_files.iter().enumerate() {
        let source_index_usize = source_index as usize;
        stable_source_indices[source_index_usize] =
            u32::try_from(stable_index).expect("reachable file count fits in u32");

        let file = &mut files[source_index_usize];
        file.input_file = input_files[source_index_usize].clone();
        file.distance_from_entry_point = u32::MAX;

        match file.input_file.repr.as_mut() {
            Some(InputFileRepr::Js(repr)) => {
                symbols.symbols_for_source[source_index_usize] =
                    std::mem::take(&mut repr.ast.symbols);

                if code_splitting {
                    for record in &mut repr.ast.import_records {
                        if record.source_index.is_valid()
                            && matches!(record.kind, ImportKind::Dynamic)
                        {
                            dynamic_import_entry_points.push(record.source_index.get_index());
                            record.assert_or_with = None;
                        }
                    }
                }

                repr.meta.resolved_exports = repr
                    .ast
                    .named_exports
                    .iter()
                    .map(|(alias, export)| {
                        (
                            alias.clone(),
                            ExportData {
                                reference: export.reference,
                                source_index,
                                name_loc: export.alias_loc,
                                ..ExportData::default()
                            },
                        )
                    })
                    .collect();
                repr.meta.is_probably_type_script_type = HashMap::new();
                repr.meta.imports_to_bind = HashMap::new();

                if let Some(scope) = &repr.ast.module_scope {
                    let cloned_scope = scope
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    repr.ast.module_scope =
                        Some(std::sync::Arc::new(std::sync::Mutex::new(cloned_scope)));
                }
            }
            Some(InputFileRepr::Css(repr)) => {
                symbols.symbols_for_source[source_index_usize] =
                    std::mem::take(&mut repr.ast.symbols);
            }
            Some(InputFileRepr::Copy(_)) | None => {}
        }
    }

    let mut stable_entry_points = Vec::new();
    for source_index in dynamic_import_entry_points {
        let file = &mut files[source_index as usize];
        if matches!(file.entry_point_kind, EntryPointKind::None) {
            stable_entry_points.push(stable_source_indices[source_index as usize]);
            file.entry_point_kind = EntryPointKind::DynamicImport;
        }
    }
    stable_entry_points.sort_unstable();
    stable_entry_points.dedup();
    for stable_index in stable_entry_points {
        entry_points.push(EntryPoint {
            source_index: reachable_files[stable_index as usize],
            ..EntryPoint::default()
        });
    }

    let mut ts_enums = HashMap::new();
    let mut const_values = HashMap::new();
    for &source_index in reachable_files {
        let file = &mut files[source_index as usize];
        file.entry_bits = BitSet::new(entry_points.len());
        if let Some(InputFileRepr::Js(repr)) = &file.input_file.repr {
            ts_enums.extend(repr.ast.ts_enums.clone());
            const_values.extend(repr.ast.const_values.clone());
        }
    }

    LinkerGraph {
        files,
        entry_points,
        symbols,
        ts_enums,
        const_values,
        reachable_files: reachable_files.to_vec(),
        stable_source_indices,
    }
}

impl LinkerGraph {
    #[must_use]
    pub fn entry_points(&self) -> &[EntryPoint] {
        &self.entry_points
    }

    /// # Panics
    ///
    /// Panics unless `source_index` refers to a JavaScript representation.
    pub fn add_part_to_file(&mut self, source_index: u32, part: js_ast::Part) -> u32 {
        let InputFileRepr::Js(repr) = self.files[source_index as usize]
            .input_file
            .repr
            .as_mut()
            .expect("JavaScript representation")
        else {
            panic!("JavaScript representation");
        };
        let part_index = u32::try_from(repr.ast.parts.len()).expect("part count fits in u32");

        for declared_symbol in &part.declared_symbols {
            if declared_symbol.is_top_level {
                let part_indices = repr
                    .meta
                    .top_level_symbol_to_parts_overlay
                    .entry(declared_symbol.reference)
                    .or_insert_with(|| {
                        repr.ast
                            .top_level_symbol_to_parts_from_parser
                            .get(&declared_symbol.reference)
                            .cloned()
                            .unwrap_or_default()
                    });
                part_indices.push(part_index);
            }
        }
        repr.ast.parts.push(part);
        part_index
    }

    /// # Panics
    ///
    /// Panics unless `source_index` refers to a JavaScript representation with
    /// a module scope.
    pub fn generate_new_symbol(
        &mut self,
        source_index: u32,
        kind: SymbolKind,
        original_name: impl Into<String>,
    ) -> Ref {
        let source_symbols = &mut self.symbols.symbols_for_source[source_index as usize];
        let reference = Ref {
            source_index,
            inner_index: u32::try_from(source_symbols.len()).expect("symbol count fits in u32"),
        };
        source_symbols.push(Symbol::new(kind, original_name));

        let InputFileRepr::Js(repr) = self.files[source_index as usize]
            .input_file
            .repr
            .as_mut()
            .expect("JavaScript representation")
        else {
            panic!("JavaScript representation");
        };
        repr.ast
            .module_scope
            .as_ref()
            .expect("JavaScript module scope")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generated
            .push(reference);
        reference
    }

    /// # Panics
    ///
    /// Panics unless both source indices refer to JavaScript representations.
    pub fn generate_symbol_import_and_use(
        &mut self,
        source_index: u32,
        part_index: u32,
        reference: Ref,
        use_count: u32,
        source_index_to_import_from: u32,
    ) {
        if use_count == 0 {
            return;
        }

        let target_parts = {
            let InputFileRepr::Js(target_repr) = self.files[source_index_to_import_from as usize]
                .input_file
                .repr
                .as_ref()
                .expect("JavaScript representation")
            else {
                panic!("JavaScript representation");
            };
            target_repr
                .top_level_symbol_to_parts(reference)
                .unwrap_or_default()
                .to_vec()
        };

        let InputFileRepr::Js(repr) = self.files[source_index as usize]
            .input_file
            .repr
            .as_mut()
            .expect("JavaScript representation")
        else {
            panic!("JavaScript representation");
        };
        let part = &mut repr.ast.parts[part_index as usize];
        part.symbol_uses
            .entry(reference)
            .or_default()
            .count_estimate += use_count;

        if reference == repr.ast.exports_ref {
            repr.ast.uses_exports_ref = true;
        }
        if reference == repr.ast.module_ref {
            repr.ast.uses_module_ref = true;
        }
        if source_index_to_import_from != source_index {
            repr.meta.imports_to_bind.insert(
                reference,
                ImportData {
                    source_index: source_index_to_import_from,
                    reference,
                    ..ImportData::default()
                },
            );
        }
        part.dependencies.extend(
            target_parts
                .into_iter()
                .map(|part_index| js_ast::Dependency {
                    source_index: source_index_to_import_from,
                    part_index,
                }),
        );
    }

    /// # Panics
    ///
    /// Panics unless the runtime and importing files have JavaScript
    /// representations and `name` is a runtime export.
    pub fn generate_runtime_symbol_import_and_use(
        &mut self,
        source_index: u32,
        part_index: u32,
        name: &str,
        use_count: u32,
    ) {
        if use_count == 0 {
            return;
        }
        let InputFileRepr::Js(runtime_repr) = self.files[runtime::SOURCE_INDEX as usize]
            .input_file
            .repr
            .as_ref()
            .expect("runtime JavaScript representation")
        else {
            panic!("runtime JavaScript representation");
        };
        let reference = runtime_repr
            .ast
            .named_exports
            .get(name)
            .unwrap_or_else(|| panic!("missing runtime export {name:?}"))
            .reference;
        self.generate_symbol_import_and_use(
            source_index,
            part_index,
            reference,
            use_count,
            runtime::SOURCE_INDEX,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::{CopyRepr, EntryPoint, InputFile, InputFileRepr, JsRepr, clone_linker_graph};
    use crate::internal::{
        ast::{ImportAssertOrWith, ImportKind, ImportRecord, Index32, Ref, Symbol, SymbolKind},
        js_ast,
    };

    #[test]
    fn representations_expose_import_records_like_upstream() {
        let mut js = InputFileRepr::Js(Box::default());
        js.import_records_mut()
            .expect("JavaScript has import records")
            .push(ImportRecord::default());
        assert_eq!(js.import_records().map(<[ImportRecord]>::len), Some(1));

        let mut copy = InputFileRepr::Copy(CopyRepr::default());
        assert!(copy.import_records_mut().is_none());
    }

    #[test]
    fn linker_overlay_takes_precedence_over_parser_parts() {
        let reference = Ref {
            source_index: 1,
            inner_index: 2,
        };
        let mut repr = JsRepr {
            ast: js_ast::Ast {
                top_level_symbol_to_parts_from_parser: HashMap::from([(reference, vec![1, 2])]),
                ..js_ast::Ast::default()
            },
            ..JsRepr::default()
        };
        assert_eq!(repr.top_level_symbol_to_parts(reference), Some(&[1, 2][..]));
        repr.meta
            .top_level_symbol_to_parts_overlay
            .insert(reference, vec![3]);
        assert_eq!(repr.top_level_symbol_to_parts(reference), Some(&[3][..]));
    }

    #[test]
    fn clone_linker_graph_isolates_mutable_linker_state() {
        let module_scope = Arc::new(Mutex::new(js_ast::Scope::default()));
        let input_files = vec![
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast: js_ast::Ast {
                        symbols: vec![Symbol::new(SymbolKind::Other, "runtime")],
                        module_scope: Some(Arc::clone(&module_scope)),
                        import_records: vec![ImportRecord {
                            source_index: Index32::new(1),
                            kind: ImportKind::Dynamic,
                            assert_or_with: Some(ImportAssertOrWith::default()),
                            ..ImportRecord::default()
                        }],
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                ..InputFile::default()
            },
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast: js_ast::Ast {
                        module_scope: Some(Arc::new(Mutex::new(js_ast::Scope::default()))),
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                ..InputFile::default()
            },
            InputFile::default(),
        ];

        let graph = clone_linker_graph(
            &input_files,
            &[0, 1, 2],
            &[EntryPoint {
                source_index: 2,
                ..EntryPoint::default()
            }],
            true,
        );

        assert_eq!(
            graph
                .entry_points()
                .iter()
                .map(|entry| entry.source_index)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert!(graph.files[2].is_user_specified_entry_point());
        assert!(graph.files[1].is_entry_point());
        assert_eq!(graph.symbols.symbols_for_source[0].len(), 1);

        let Some(InputFileRepr::Js(original)) = &input_files[0].repr else {
            panic!("JavaScript representation");
        };
        let Some(InputFileRepr::Js(cloned)) = &graph.files[0].input_file.repr else {
            panic!("JavaScript representation");
        };
        assert_eq!(original.ast.symbols.len(), 1);
        assert!(original.ast.import_records[0].assert_or_with.is_some());
        assert!(cloned.ast.symbols.is_empty());
        assert!(cloned.ast.import_records[0].assert_or_with.is_none());
        assert!(!Arc::ptr_eq(
            original.ast.module_scope.as_ref().expect("original scope"),
            cloned.ast.module_scope.as_ref().expect("cloned scope")
        ));
    }

    #[test]
    fn linker_graph_helpers_preserve_part_and_symbol_invariants() {
        let target_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let generated_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let input_files = vec![
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast: js_ast::Ast {
                        parts: vec![js_ast::Part::default()],
                        top_level_symbol_to_parts_from_parser: HashMap::from([(
                            target_ref,
                            vec![0],
                        )]),
                        module_scope: Some(Arc::new(Mutex::new(js_ast::Scope::default()))),
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                ..InputFile::default()
            },
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast: js_ast::Ast {
                        parts: vec![js_ast::Part::default()],
                        top_level_symbol_to_parts_from_parser: HashMap::from([(
                            generated_ref,
                            vec![0],
                        )]),
                        module_scope: Some(Arc::new(Mutex::new(js_ast::Scope::default()))),
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                ..InputFile::default()
            },
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &[], false);

        graph.generate_symbol_import_and_use(1, 0, target_ref, 2, 0);
        let new_ref = graph.generate_new_symbol(1, SymbolKind::Other, "generated");
        assert_eq!(new_ref, generated_ref);

        let added_part_index = graph.add_part_to_file(
            1,
            js_ast::Part {
                declared_symbols: vec![js_ast::DeclaredSymbol {
                    reference: generated_ref,
                    is_top_level: true,
                }],
                ..js_ast::Part::default()
            },
        );
        assert_eq!(added_part_index, 1);

        let Some(InputFileRepr::Js(repr)) = &graph.files[1].input_file.repr else {
            panic!("JavaScript representation");
        };
        assert_eq!(
            repr.ast.parts[0]
                .symbol_uses
                .get(&target_ref)
                .expect("symbol use")
                .count_estimate,
            2
        );
        assert_eq!(
            repr.ast.parts[0].dependencies,
            vec![js_ast::Dependency {
                source_index: 0,
                part_index: 0,
            }]
        );
        assert_eq!(
            repr.meta
                .top_level_symbol_to_parts_overlay
                .get(&generated_ref),
            Some(&vec![0, 1])
        );
        assert_eq!(
            repr.ast
                .module_scope
                .as_ref()
                .expect("module scope")
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generated,
            vec![generated_ref]
        );
    }
}
