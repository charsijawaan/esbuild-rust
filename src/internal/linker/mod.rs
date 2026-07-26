//! Port of upstream `internal/linker`.
//!
//! The linker is esbuild's second bundling phase. This initial section ports
//! the output-piece representation and the final path substitution machinery
//! used after chunks have been generated.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

use crate::internal::{
    ast::{ImportKind, ImportRecordFlags, Index32, Ref},
    config::{Format, Loader, Options},
    css_ast::{
        ImportConditions, media_queries_equal_ignoring_whitespace, tokens_equal_ignoring_whitespace,
    },
    fs::Fs,
    graph::{ExportData, ImportData, InputFileRepr, LinkerGraph, WrapKind},
    helpers::Joiner,
    js_ast::ExportsKind,
    logger::{Log, Range},
    sourcemap::{LineColumnOffset, SourceMapShift},
};

const CIRCULAR_CHUNK_IMPORT_ERROR: &str =
    "Internal error: generated chunks contain a circular import";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PartRange {
    pub source_index: u32,
    pub part_index_begin: u32,
    pub part_index_end: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChunkImport {
    pub chunk_index: u32,
    pub import_kind: ImportKind,
}

#[derive(Debug, Default)]
pub struct ChunkInfo {
    pub unique_key: String,
    pub cross_chunk_imports: Vec<ChunkImport>,
    pub final_rel_path: String,
    pub intermediate_output: IntermediateOutput,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CrossChunkImportItem {
    pub export_alias: String,
    pub reference: Ref,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CrossChunkImport {
    pub sorted_import_items: Vec<CrossChunkImportItem>,
    pub chunk_index: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StableRef {
    pub stable_source_index: u32,
    pub reference: Ref,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum OutputPieceIndexKind {
    #[default]
    None,
    AssetIndex,
    ChunkIndex,
}

/// A span of generated output followed by a temporary asset or chunk key.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutputPiece {
    pub data: Vec<u8>,
    pub index: u32,
    pub kind: OutputPieceIndexKind,
}

/// Generated output before temporary asset and chunk keys are substituted.
#[derive(Debug, Default)]
pub struct IntermediateOutput {
    pieces: Option<Vec<OutputPiece>>,
    joiner: Joiner,
}

impl IntermediateOutput {
    #[must_use]
    pub fn pieces(&self) -> Option<&[OutputPiece]> {
        self.pieces.as_deref()
    }

    #[must_use]
    pub fn without_substitutions(joiner: Joiner) -> Self {
        Self {
            pieces: None,
            joiner,
        }
    }
}

/// The path data associated with one file-loader asset.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssetPath {
    pub unique_key: String,
    pub rel_path: String,
}

/// The path data associated with one generated chunk.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChunkPath {
    pub unique_key: String,
    pub final_rel_path: String,
}

/// The portion of linker state needed to assemble generated output files.
#[derive(Clone, Copy, Debug)]
pub struct OutputPathContext<'a> {
    unique_key_prefix: &'a str,
    unique_key_prefix_bytes: &'a [u8],
    assets: &'a [Option<AssetPath>],
    chunks: &'a [ChunkPath],
}

impl<'a> OutputPathContext<'a> {
    #[must_use]
    pub fn new(
        unique_key_prefix: &'a str,
        assets: &'a [Option<AssetPath>],
        chunks: &'a [ChunkPath],
    ) -> Self {
        Self {
            unique_key_prefix,
            unique_key_prefix_bytes: unique_key_prefix.as_bytes(),
            assets,
            chunks,
        }
    }

    #[must_use]
    pub fn break_joiner_into_pieces(&self, joiner: Joiner) -> IntermediateOutput {
        if !joiner.contains(self.unique_key_prefix, self.unique_key_prefix_bytes) {
            return IntermediateOutput::without_substitutions(joiner);
        }
        self.break_output_into_pieces(joiner.done())
    }

    #[must_use]
    pub fn break_output_into_pieces(&self, mut output: Vec<u8>) -> IntermediateOutput {
        let mut pieces = Vec::new();
        let prefix = self.unique_key_prefix_bytes;

        loop {
            let mut boundary = find_bytes(&output, prefix);
            let mut kind = OutputPieceIndexKind::None;
            let mut index = 0_u32;

            if let Some(found) = boundary {
                let start = found + prefix.len();
                if start + 9 > output.len() {
                    boundary = None;
                } else {
                    kind = match output[start] {
                        b'A' => OutputPieceIndexKind::AssetIndex,
                        b'C' => OutputPieceIndexKind::ChunkIndex,
                        _ => OutputPieceIndexKind::None,
                    };
                    for digit in &output[start + 1..start + 9] {
                        if !digit.is_ascii_digit() {
                            boundary = None;
                            break;
                        }
                        index = index * 10 + u32::from(*digit - b'0');
                    }
                }
            }

            let index_is_in_range =
                |length: usize| usize::try_from(index).is_ok_and(|index| index < length);
            match kind {
                OutputPieceIndexKind::AssetIndex if index_is_in_range(self.assets.len()) => {}
                OutputPieceIndexKind::ChunkIndex if index_is_in_range(self.chunks.len()) => {}
                _ => boundary = None,
            }

            let Some(boundary) = boundary else {
                pieces.push(OutputPiece {
                    data: output,
                    ..OutputPiece::default()
                });
                break;
            };

            let remaining = output.split_off(boundary);
            pieces.push(OutputPiece {
                data: output,
                index,
                kind,
            });
            output = remaining[prefix.len() + 9..].to_vec();
        }

        IntermediateOutput {
            pieces: Some(pieces),
            joiner: Joiner::default(),
        }
    }

    /// Substitute final paths and return the source-map offsets caused by each
    /// replacement.
    ///
    /// # Panics
    ///
    /// Panics when an asset marker refers to a graph file without exactly one
    /// associated output asset, matching upstream's internal invariant.
    #[must_use]
    pub fn substitute_final_paths(
        &self,
        intermediate_output: IntermediateOutput,
        mut modify_path: impl FnMut(&str) -> String,
    ) -> (Joiner, Vec<SourceMapShift>) {
        let Some(pieces) = intermediate_output.pieces else {
            return (intermediate_output.joiner, vec![SourceMapShift::default()]);
        };

        let mut joiner = Joiner::default();
        let mut shift = SourceMapShift::default();
        let mut shifts = Vec::with_capacity(pieces.len());
        shifts.push(shift);

        for piece in pieces {
            let mut data_offset = LineColumnOffset::default();
            data_offset.advance_bytes(&piece.data);
            joiner.add_bytes(piece.data);
            shift.before.add(data_offset);
            shift.after.add(data_offset);

            match piece.kind {
                OutputPieceIndexKind::AssetIndex => {
                    let asset = self.assets[piece.index as usize]
                        .as_ref()
                        .expect("Internal error: asset marker must reference one output file");
                    let import_path = modify_path(&asset.rel_path.replace('\\', "/"));
                    joiner.add_string(import_path.clone());
                    shift.before.advance_string(&asset.unique_key);
                    shift.after.advance_string(&import_path);
                    shifts.push(shift);
                }
                OutputPieceIndexKind::ChunkIndex => {
                    let chunk = &self.chunks[piece.index as usize];
                    let import_path = modify_path(&chunk.final_rel_path);
                    joiner.add_string(import_path.clone());
                    shift.before.advance_string(&chunk.unique_key);
                    shift.after.advance_string(&import_path);
                    shifts.push(shift);
                }
                OutputPieceIndexKind::None => {}
            }
        }

        (joiner, shifts)
    }

    /// # Panics
    ///
    /// Panics when an asset marker refers to a graph file without exactly one
    /// associated output asset, matching upstream's internal invariant.
    #[must_use]
    pub fn accurate_final_byte_count(
        &self,
        output: &IntermediateOutput,
        mut modify_path: impl FnMut(&str) -> String,
    ) -> usize {
        if output.pieces.is_none() {
            return output.joiner.len() as usize;
        }

        output
            .pieces
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|piece| {
                piece.data.len()
                    + match piece.kind {
                        OutputPieceIndexKind::AssetIndex => {
                            let asset = self.assets[piece.index as usize].as_ref().expect(
                                "Internal error: asset marker must reference one output file",
                            );
                            modify_path(&asset.rel_path.replace('\\', "/")).len()
                        }
                        OutputPieceIndexKind::ChunkIndex => {
                            modify_path(&self.chunks[piece.index as usize].final_rel_path).len()
                        }
                        OutputPieceIndexKind::None => 0,
                    }
            })
            .sum()
    }
}

/// Merge an adjacent part into the final range, or append a new range.
pub fn append_or_extend_part_range(
    ranges: &mut Vec<PartRange>,
    source_index: u32,
    part_index: u32,
) {
    if let Some(range) = ranges.last_mut()
        && range.source_index == source_index
        && range.part_index_end == part_index
    {
        range.part_index_end = part_index.wrapping_add(1);
        return;
    }

    ranges.push(PartRange {
        source_index,
        part_index_begin: part_index,
        part_index_end: part_index.wrapping_add(1),
    });
}

/// Reject static cycles in the generated chunk import graph. Dynamic import
/// cycles are intentionally allowed because they do not imply eager
/// initialization-order problems.
pub fn enforce_no_cyclic_chunk_imports(log: &Log, chunks: &[ChunkInfo]) {
    fn validate(log: &Log, chunks: &[ChunkInfo], chunk_index: usize, colors: &mut [u8]) -> bool {
        if colors[chunk_index] == 1 {
            log.add_error(None, Range::default(), CIRCULAR_CHUNK_IMPORT_ERROR);
            return true;
        }
        if colors[chunk_index] == 2 {
            return false;
        }

        colors[chunk_index] = 1;
        for chunk_import in &chunks[chunk_index].cross_chunk_imports {
            if chunk_import.import_kind != ImportKind::Dynamic {
                let imported_chunk_index = usize::try_from(chunk_import.chunk_index)
                    .expect("chunk index must fit in usize");
                if validate(log, chunks, imported_chunk_index, colors) {
                    return true;
                }
            }
        }
        colors[chunk_index] = 2;
        false
    }

    let mut colors = vec![0; chunks.len()];
    for chunk_index in 0..chunks.len() {
        if validate(log, chunks, chunk_index, &mut colors) {
            break;
        }
    }
}

/// Inline final asset and copy-loader URLs into import records before chunks
/// are computed. This is the asset-rewriting portion of upstream linker scan
/// step 1.
///
/// # Panics
///
/// Panics if a resolved CSS URL does not point to a JavaScript representation,
/// or a copy index does not point to a copy representation. Both are linker
/// graph invariants established by the scanner.
pub fn inline_linked_assets(graph: &mut LinkerGraph, unique_key_prefix: &str) {
    for source_index in graph.reachable_files.clone() {
        let source_index = source_index as usize;
        let mut repr = graph.files[source_index]
            .input_file
            .repr
            .take()
            .expect("reachable file must have a representation");
        let mut additional_files =
            std::mem::take(&mut graph.files[source_index].input_file.additional_files);

        match &mut repr {
            InputFileRepr::Css(css) => {
                for record in &mut css.ast.import_records {
                    if record.source_index.is_valid() {
                        let other_source_index = record.source_index.get_index() as usize;
                        let other_file = &graph.files[other_source_index].input_file;
                        let InputFileRepr::Js(other) = other_file
                            .repr
                            .as_ref()
                            .expect("resolved CSS URL target must have a representation")
                        else {
                            panic!("resolved CSS URL target must be JavaScript");
                        };
                        record.path.text.clone_from(&other.ast.url_for_css);
                        record.path.namespace.clear();
                        record.source_index = Index32::default();
                        if other_file.loader == Loader::Empty {
                            record.flags |= ImportRecordFlags::WAS_LOADED_WITH_EMPTY_LOADER;
                        } else {
                            record.flags |= ImportRecordFlags::SHOULD_NOT_BE_EXTERNAL_IN_METAFILE;
                        }
                        if other.ast.url_for_css.contains(unique_key_prefix) {
                            record.flags |= ImportRecordFlags::CONTAINS_UNIQUE_KEY;
                        }
                        additional_files.extend(other_file.additional_files.iter().cloned());
                    } else if record.copy_source_index.is_valid() {
                        let other_source_index = record.copy_source_index.get_index() as usize;
                        let other_file = &graph.files[other_source_index].input_file;
                        let InputFileRepr::Copy(other) = other_file
                            .repr
                            .as_ref()
                            .expect("copy target must have a representation")
                        else {
                            panic!("copy target must use the copy representation");
                        };
                        record.path.text.clone_from(&other.url_for_code);
                        record.path.namespace.clear();
                        record.copy_source_index = Index32::default();
                        record.flags |= ImportRecordFlags::SHOULD_NOT_BE_EXTERNAL_IN_METAFILE
                            | ImportRecordFlags::CONTAINS_UNIQUE_KEY;
                        additional_files.extend(other_file.additional_files.iter().cloned());
                    }
                }
            }
            InputFileRepr::Js(js) => {
                for record in &mut js.ast.import_records {
                    if !record.source_index.is_valid() && record.copy_source_index.is_valid() {
                        let other_source_index = record.copy_source_index.get_index() as usize;
                        let other_file = &graph.files[other_source_index].input_file;
                        let InputFileRepr::Copy(other) = other_file
                            .repr
                            .as_ref()
                            .expect("copy target must have a representation")
                        else {
                            panic!("copy target must use the copy representation");
                        };
                        record.path.text.clone_from(&other.url_for_code);
                        record.path.namespace.clear();
                        record.copy_source_index = Index32::default();
                        record.flags |= ImportRecordFlags::SHOULD_NOT_BE_EXTERNAL_IN_METAFILE
                            | ImportRecordFlags::CONTAINS_UNIQUE_KEY;
                        additional_files.extend(other_file.additional_files.iter().cloned());
                    }
                }
            }
            InputFileRepr::Copy(_) => {}
        }

        graph.files[source_index].input_file.additional_files = additional_files;
        graph.files[source_index].input_file.repr = Some(repr);
    }
}

/// Determine which JavaScript modules must receive `CommonJS` or ESM wrappers.
/// This is the module-classification portion of upstream linker scan step 1.
///
/// # Panics
///
/// Panics if a resolved JavaScript import does not point to a JavaScript
/// representation, matching the scanner/linker graph invariant.
pub fn classify_module_wrappers(graph: &mut LinkerGraph, options: &Options) {
    for source_index in graph.reachable_files.clone() {
        let source_index = source_index as usize;
        let is_entry_point = graph.files[source_index].is_entry_point();
        let repr = graph.files[source_index]
            .input_file
            .repr
            .take()
            .expect("reachable file must have a representation");
        let mut repr = match repr {
            InputFileRepr::Js(repr) => repr,
            other => {
                graph.files[source_index].input_file.repr = Some(other);
                continue;
            }
        };

        for record_index in 0..repr.ast.import_records.len() {
            let record = &repr.ast.import_records[record_index];
            if !record.source_index.is_valid() {
                continue;
            }
            let target_source_index = record.source_index.get_index() as usize;
            let kind = record.kind;
            let flags = record.flags;

            let classify = |other: &mut crate::internal::graph::JsRepr| match kind {
                ImportKind::Stmt => {
                    if (flags.contains(ImportRecordFlags::CONTAINS_IMPORT_STAR)
                        || flags.contains(ImportRecordFlags::CONTAINS_DEFAULT_ALIAS))
                        && other.ast.exports_kind == ExportsKind::None
                        && !other.ast.has_lazy_export
                    {
                        other.meta.wrap = WrapKind::Cjs;
                        other.ast.exports_kind = ExportsKind::CommonJs;
                    }
                }
                ImportKind::Require => {
                    if other.ast.exports_kind == ExportsKind::Esm {
                        other.meta.wrap = WrapKind::Esm;
                    } else {
                        other.meta.wrap = WrapKind::Cjs;
                        other.ast.exports_kind = ExportsKind::CommonJs;
                    }
                }
                ImportKind::Dynamic if !options.code_splitting => {
                    if other.ast.exports_kind == ExportsKind::Esm {
                        other.meta.wrap = WrapKind::Esm;
                    } else {
                        other.meta.wrap = WrapKind::Cjs;
                        other.ast.exports_kind = ExportsKind::CommonJs;
                    }
                }
                _ => {}
            };

            if target_source_index == source_index {
                classify(&mut repr);
            } else {
                let InputFileRepr::Js(other) = graph.files[target_source_index]
                    .input_file
                    .repr
                    .as_mut()
                    .expect("resolved JavaScript target must have a representation")
                else {
                    panic!("resolved JavaScript target must be JavaScript");
                };
                classify(other);
            }
        }

        if repr.ast.exports_kind == ExportsKind::CommonJs
            && (!is_entry_point || matches!(options.output_format, Format::Iife | Format::EsModule))
        {
            repr.meta.wrap = WrapKind::Cjs;
        }
        graph.files[source_index].input_file.repr = Some(InputFileRepr::Js(repr));
    }
}

/// Recursively wrap a module and all of its dependencies so evaluation can be
/// deferred behind the generated wrapper.
///
/// # Panics
///
/// Panics if `source_index` or one of its resolved dependencies is not a
/// JavaScript representation, matching the linker's graph invariant.
pub fn recursively_wrap_dependencies(graph: &mut LinkerGraph, source_index: u32) {
    let dependencies = {
        let InputFileRepr::Js(repr) = graph.files[source_index as usize]
            .input_file
            .repr
            .as_mut()
            .expect("wrapped dependency must have a representation")
        else {
            panic!("wrapped dependency must be JavaScript");
        };
        if repr.meta.did_wrap_dependencies {
            return;
        }
        repr.meta.did_wrap_dependencies = true;

        if source_index == crate::internal::runtime::SOURCE_INDEX {
            return;
        }
        if repr.meta.wrap == WrapKind::None {
            repr.meta.wrap = if repr.ast.exports_kind == ExportsKind::CommonJs {
                WrapKind::Cjs
            } else {
                WrapKind::Esm
            };
        }
        repr.ast
            .import_records
            .iter()
            .filter(|record| record.source_index.is_valid())
            .map(|record| record.source_index.get_index())
            .collect::<Vec<_>>()
    };

    for dependency in dependencies {
        recursively_wrap_dependencies(graph, dependency);
    }
}

/// Detect whether an `export *` chain reaches exports that can only be
/// determined at run time.
///
/// # Panics
///
/// Panics if the export-star graph contains a non-JavaScript representation,
/// matching the linker's graph invariant.
#[must_use]
pub fn has_dynamic_exports_due_to_export_star<Hasher: BuildHasher>(
    graph: &mut LinkerGraph,
    source_index: u32,
    visited: &mut HashSet<u32, Hasher>,
    output_format: Format,
) -> bool {
    let (exports_kind, export_stars) = {
        let InputFileRepr::Js(repr) = graph.files[source_index as usize]
            .input_file
            .repr
            .as_ref()
            .expect("export-star source must have a representation")
        else {
            panic!("export-star source must be JavaScript");
        };
        (
            repr.ast.exports_kind,
            repr.ast
                .export_star_import_records
                .iter()
                .map(|&index| repr.ast.import_records[index as usize].source_index)
                .collect::<Vec<_>>(),
        )
    };

    if matches!(
        exports_kind,
        ExportsKind::CommonJs | ExportsKind::EsmWithDynamicFallback
    ) {
        return true;
    }
    if !visited.insert(source_index) {
        return false;
    }

    for target in export_stars {
        let is_dynamic = if target.is_valid() {
            let target = target.get_index();
            target != source_index
                && has_dynamic_exports_due_to_export_star(graph, target, visited, output_format)
        } else {
            !graph.files[source_index as usize].is_entry_point()
                || !output_format.keep_esm_import_export_syntax()
        };
        if is_dynamic {
            let InputFileRepr::Js(repr) = graph.files[source_index as usize]
                .input_file
                .repr
                .as_mut()
                .expect("export-star source must have a representation")
            else {
                panic!("export-star source must be JavaScript");
            };
            repr.ast.exports_kind = ExportsKind::EsmWithDynamicFallback;
            return true;
        }
    }
    false
}

/// Run upstream linker scan phase 2: propagate wrappers and dynamic export
/// status throughout the module graph.
///
/// # Panics
///
/// Panics if resolved JavaScript imports point to non-JavaScript
/// representations, matching the linker's graph invariant.
pub fn propagate_wrappers_and_dynamic_exports(graph: &mut LinkerGraph, options: &Options) {
    for source_index in graph.reachable_files.clone() {
        let (wrap, has_export_stars, imports) = {
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_ref()
            else {
                continue;
            };
            (
                repr.meta.wrap,
                !repr.ast.export_star_import_records.is_empty(),
                repr.ast
                    .import_records
                    .iter()
                    .filter(|record| record.source_index.is_valid())
                    .map(|record| record.source_index.get_index())
                    .collect::<Vec<_>>(),
            )
        };

        if wrap != WrapKind::None {
            recursively_wrap_dependencies(graph, source_index);
        }
        if has_export_stars {
            let mut visited = HashSet::new();
            let _ = has_dynamic_exports_due_to_export_star(
                graph,
                source_index,
                &mut visited,
                options.output_format,
            );
        }

        for target in imports {
            let InputFileRepr::Js(other) = graph.files[target as usize]
                .input_file
                .repr
                .as_ref()
                .expect("resolved JavaScript target must have a representation")
            else {
                panic!("resolved JavaScript target must be JavaScript");
            };
            if other.ast.exports_kind == ExportsKind::CommonJs {
                recursively_wrap_dependencies(graph, target);
            }
        }
    }
}

/// Recursively accumulate statically discoverable exports from `export *`
/// statements into one root module's resolved export map.
///
/// # Panics
///
/// Panics if the export-star graph contains a non-JavaScript representation,
/// or an import record index is invalid, matching linker graph invariants.
#[allow(clippy::too_many_lines)]
pub fn add_exports_for_export_star<Hasher: BuildHasher>(
    graph: &mut LinkerGraph,
    resolved_exports: &mut HashMap<String, ExportData, Hasher>,
    source_index: u32,
    source_index_stack: &mut Vec<u32>,
) {
    if source_index_stack.contains(&source_index) {
        return;
    }
    source_index_stack.push(source_index);

    let targets = {
        let InputFileRepr::Js(repr) = graph.files[source_index as usize]
            .input_file
            .repr
            .as_ref()
            .expect("export-star source must have a representation")
        else {
            panic!("export-star source must be JavaScript");
        };
        repr.ast
            .export_star_import_records
            .iter()
            .map(|&record_index| repr.ast.import_records[record_index as usize].source_index)
            .filter(|target| target.is_valid())
            .map(Index32::get_index)
            .collect::<Vec<_>>()
    };

    for other_source_index in targets {
        let (exports_kind, named_exports) = {
            let InputFileRepr::Js(other) = graph.files[other_source_index as usize]
                .input_file
                .repr
                .as_ref()
                .expect("export-star target must have a representation")
            else {
                panic!("export-star target must be JavaScript");
            };
            (other.ast.exports_kind, other.ast.named_exports.clone())
        };
        if exports_kind == ExportsKind::CommonJs {
            continue;
        }

        for (alias, named_export) in named_exports {
            if alias == "default" {
                continue;
            }
            let is_shadowed = source_index_stack.iter().any(|&previous_source_index| {
                let InputFileRepr::Js(previous) = graph.files[previous_source_index as usize]
                    .input_file
                    .repr
                    .as_ref()
                    .expect("export-star ancestor must have a representation")
                else {
                    panic!("export-star ancestor must be JavaScript");
                };
                previous.ast.named_exports.contains_key(&alias)
            });
            if is_shadowed {
                continue;
            }

            if let Some(existing) = resolved_exports.get_mut(&alias) {
                if existing.source_index != other_source_index {
                    existing
                        .potentially_ambiguous_export_star_refs
                        .push(ImportData {
                            source_index: other_source_index,
                            reference: named_export.reference,
                            name_loc: named_export.alias_loc,
                            ..ImportData::default()
                        });
                }
            } else {
                resolved_exports.insert(
                    alias,
                    ExportData {
                        reference: named_export.reference,
                        source_index: other_source_index,
                        name_loc: named_export.alias_loc,
                        ..ExportData::default()
                    },
                );
                let InputFileRepr::Js(repr) = graph.files[source_index as usize]
                    .input_file
                    .repr
                    .as_mut()
                    .expect("export-star source must have a representation")
                else {
                    panic!("export-star source must be JavaScript");
                };
                repr.meta.imports_to_bind.insert(
                    named_export.reference,
                    ImportData {
                        reference: named_export.reference,
                        source_index: other_source_index,
                        ..ImportData::default()
                    },
                );
            }
        }

        add_exports_for_export_star(
            graph,
            resolved_exports,
            other_source_index,
            source_index_stack,
        );
    }

    source_index_stack.pop();
}

/// Resolve export-star chains and create the special namespace export used by
/// import-star bindings. This is the static export-resolution portion of
/// upstream linker scan phase 3.
///
/// # Panics
///
/// Panics if an export-star graph violates JavaScript linker graph invariants.
pub fn resolve_export_stars(graph: &mut LinkerGraph) {
    for source_index in graph.reachable_files.clone() {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            continue;
        };
        let has_export_stars = !repr.ast.export_star_import_records.is_empty();
        let exports_ref = repr.ast.exports_ref;

        if has_export_stars {
            let mut resolved_exports = {
                let InputFileRepr::Js(repr) = graph.files[source_index as usize]
                    .input_file
                    .repr
                    .as_mut()
                    .expect("JavaScript representation")
                else {
                    unreachable!();
                };
                std::mem::take(&mut repr.meta.resolved_exports)
            };
            add_exports_for_export_star(
                graph,
                &mut resolved_exports,
                source_index,
                &mut Vec::new(),
            );
            let InputFileRepr::Js(repr) = graph.files[source_index as usize]
                .input_file
                .repr
                .as_mut()
                .expect("JavaScript representation")
            else {
                unreachable!();
            };
            repr.meta.resolved_exports = resolved_exports;
        }

        let InputFileRepr::Js(repr) = graph.files[source_index as usize]
            .input_file
            .repr
            .as_mut()
            .expect("JavaScript representation")
        else {
            unreachable!();
        };
        repr.meta.resolved_export_star = Some(ExportData {
            reference: exports_ref,
            source_index,
            ..ExportData::default()
        });
    }
}

#[must_use]
pub fn import_conditions_are_equal(left: &[ImportConditions], right: &[ImportConditions]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            tokens_equal_ignoring_whitespace(&left.layers, &right.layers)
                && tokens_equal_ignoring_whitespace(&left.supports, &right.supports)
                && media_queries_equal_ignoring_whitespace(&left.queries, &right.queries)
        })
}

/// Return whether a later import masks an earlier import of the same file.
///
/// Layers are handled separately by the linker. For unlayered styles, the
/// later condition list must be a prefix of the earlier one, and each later
/// condition must apply everywhere its corresponding earlier condition does.
#[must_use]
pub fn is_conditional_import_redundant(
    earlier: &[ImportConditions],
    later: &[ImportConditions],
) -> bool {
    if later.len() > earlier.len() {
        return false;
    }

    for (earlier, later) in earlier.iter().zip(later) {
        if tokens_equal_ignoring_whitespace(&earlier.layers, &later.layers) {
            let same_supports =
                tokens_equal_ignoring_whitespace(&earlier.supports, &later.supports);
            let same_media =
                media_queries_equal_ignoring_whitespace(&earlier.queries, &later.queries);

            if same_supports && same_media {
                continue;
            }
            if same_media && later.supports.is_empty() {
                continue;
            }
            if same_supports && later.queries.is_empty() {
                continue;
            }
        }
        return false;
    }

    true
}

/// Sort imports first by source chunk and then by the finalized export alias.
///
/// # Panics
///
/// Panics when an imported reference has no finalized export alias in its
/// source chunk, matching the linker's internal invariant.
#[must_use]
pub fn sorted_cross_chunk_imports<ImportHasher: BuildHasher, ExportHasher: BuildHasher>(
    imports_from_other_chunks: HashMap<u32, Vec<CrossChunkImportItem>, ImportHasher>,
    exports_to_other_chunks: &[HashMap<Ref, String, ExportHasher>],
) -> Vec<CrossChunkImport> {
    let mut result = Vec::with_capacity(imports_from_other_chunks.len());

    for (other_chunk_index, mut import_items) in imports_from_other_chunks {
        let exports = &exports_to_other_chunks[other_chunk_index as usize];
        for item in &mut import_items {
            item.export_alias = exports
                .get(&item.reference)
                .expect("cross-chunk import must have a finalized export alias")
                .clone();
        }
        import_items.sort_unstable_by(|left, right| left.export_alias.cmp(&right.export_alias));
        result.push(CrossChunkImport {
            chunk_index: other_chunk_index,
            sorted_import_items: import_items,
        });
    }

    result.sort_unstable_by_key(|item| item.chunk_index);
    result
}

/// Sort cross-chunk exports using DFS-stable source indices instead of
/// concurrently allocated graph source indices.
///
/// # Panics
///
/// Panics when an export reference points outside `stable_source_indices`.
#[must_use]
pub fn sorted_cross_chunk_export_items<Hasher: BuildHasher>(
    export_refs: &HashSet<Ref, Hasher>,
    stable_source_indices: &[u32],
) -> Vec<StableRef> {
    let mut result: Vec<_> = export_refs
        .iter()
        .map(|reference| StableRef {
            stable_source_index: stable_source_indices[reference.source_index as usize],
            reference: *reference,
        })
        .collect();
    result.sort_unstable_by(|left, right| {
        left.stable_source_index
            .cmp(&right.stable_source_index)
            .then_with(|| left.reference.inner_index.cmp(&right.reference.inner_index))
    });
    result
}

/// Join a generated relative path to the configured public path.
#[must_use]
pub fn join_with_public_path(public_path: &str, mut rel_path: &str) -> String {
    if let Some(without_dot) = rel_path.strip_prefix("./") {
        rel_path = without_dot;
        loop {
            if let Some(without_slash) = rel_path.strip_prefix('/') {
                rel_path = without_slash;
            } else if let Some(without_dot) = rel_path.strip_prefix("./") {
                rel_path = without_dot;
            } else {
                break;
            }
        }
    }

    let public_path = if public_path.is_empty() {
        "."
    } else {
        public_path
    };
    let slash = if public_path.ends_with('/') { "" } else { "/" };
    format!("{public_path}{slash}{rel_path}")
}

/// Compute the import path from one output chunk directory to another.
#[must_use]
pub fn path_between_chunks(
    file_system: &dyn Fs,
    public_path: &str,
    from_rel_dir: &str,
    to_rel_path: &str,
) -> Option<String> {
    if !public_path.is_empty() {
        return Some(join_with_public_path(public_path, to_rel_path));
    }

    let mut rel_path = file_system.rel(from_rel_dir, to_rel_path)?;
    rel_path = rel_path.replace('\\', "/");
    if !rel_path.starts_with("./") && !rel_path.starts_with("../") {
        rel_path.insert_str(0, "./");
    }
    Some(rel_path)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::{
        AssetPath, ChunkImport, ChunkInfo, ChunkPath, CrossChunkImport, CrossChunkImportItem,
        OutputPathContext, OutputPiece, OutputPieceIndexKind, PartRange, StableRef,
        add_exports_for_export_star, append_or_extend_part_range, classify_module_wrappers,
        enforce_no_cyclic_chunk_imports, has_dynamic_exports_due_to_export_star,
        import_conditions_are_equal, inline_linked_assets, is_conditional_import_redundant,
        join_with_public_path, path_between_chunks, propagate_wrappers_and_dynamic_exports,
        recursively_wrap_dependencies, resolve_export_stars, sorted_cross_chunk_export_items,
        sorted_cross_chunk_imports,
    };
    use crate::internal::{
        ast::{ImportKind, ImportRecord, ImportRecordFlags, Index32, Ref},
        config::{Format, Loader, Options},
        css_ast::{
            ImportConditions, MediaArbitraryTokensQuery, MediaQuery, MediaQueryData, Token,
            WhitespaceFlags,
        },
        css_lexer::TokenKind,
        fs::{MockKind, mock_fs},
        graph::{
            CopyRepr, CssRepr, EntryPoint, InputFile, InputFileRepr, JsRepr, OutputFile, WrapKind,
            clone_linker_graph,
        },
        helpers::Joiner,
        js_ast::{self, ExportsKind, NamedExport},
        logger::{DeferLogKind, Loc, Log},
        sourcemap::{LineColumnOffset, SourceMapShift},
    };

    const PREFIX: &str = "UNIQUE";

    fn context<'a>(
        assets: &'a [Option<AssetPath>],
        chunks: &'a [ChunkPath],
    ) -> OutputPathContext<'a> {
        OutputPathContext::new(PREFIX, assets, chunks)
    }

    #[test]
    fn output_is_left_in_joiner_when_there_are_no_keys() {
        let mut joiner = Joiner::default();
        joiner.add_string("console.log(0)");
        let output = context(&[], &[]).break_joiner_into_pieces(joiner);
        assert!(output.pieces().is_none());
        let (joiner, shifts) = context(&[], &[]).substitute_final_paths(output, str::to_owned);
        assert_eq!(joiner.done(), b"console.log(0)");
        assert_eq!(shifts, vec![SourceMapShift::default()]);
    }

    #[test]
    fn breaks_asset_and_chunk_markers_into_upstream_piece_shape() {
        let assets = [Some(AssetPath::default())];
        let chunks = [ChunkPath::default(), ChunkPath::default()];
        let output = context(&assets, &chunks)
            .break_output_into_pieces(b"aUNIQUEA00000000bUNIQUEC00000001c".to_vec());
        assert_eq!(
            output.pieces(),
            Some(
                [
                    OutputPiece {
                        data: b"a".to_vec(),
                        index: 0,
                        kind: OutputPieceIndexKind::AssetIndex,
                    },
                    OutputPiece {
                        data: b"b".to_vec(),
                        index: 1,
                        kind: OutputPieceIndexKind::ChunkIndex,
                    },
                    OutputPiece {
                        data: b"c".to_vec(),
                        ..OutputPiece::default()
                    },
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn malformed_or_out_of_range_marker_stops_scanning() {
        let chunks = [ChunkPath::default()];
        for input in [
            b"xUNIQUEC0000000xUNIQUec00000000".as_slice(),
            b"xUNIQUEX00000000UNIQUeC00000000".as_slice(),
            b"xUNIQUEC00000001UNIQUEC00000000".as_slice(),
            b"xUNIQUEC000".as_slice(),
        ] {
            let output = context(&[], &chunks).break_output_into_pieces(input.to_vec());
            assert_eq!(
                output.pieces(),
                Some(
                    [OutputPiece {
                        data: input.to_vec(),
                        ..OutputPiece::default()
                    }]
                    .as_slice()
                )
            );
        }
    }

    #[test]
    fn substitutes_paths_and_tracks_utf16_source_map_offsets() {
        let assets = [Some(AssetPath {
            unique_key: "UNIQUEA00000000".into(),
            rel_path: r"assets\logo.png".into(),
        })];
        let chunks = [ChunkPath {
            unique_key: "UNIQUEC00000000".into(),
            final_rel_path: "chunks/é.js".into(),
        }];
        let output = context(&assets, &chunks)
            .break_output_into_pieces("😀\nUNIQUEA00000000+UNIQUEC00000000".as_bytes().to_vec());
        assert_eq!(
            context(&assets, &chunks)
                .accurate_final_byte_count(&output, |path| { format!("<{path}>") }),
            "😀\n<assets/logo.png>+<chunks/é.js>".len()
        );

        let (joiner, shifts) =
            context(&assets, &chunks).substitute_final_paths(output, |path| format!("<{path}>"));
        assert_eq!(
            String::from_utf8(joiner.done()).expect("UTF-8"),
            "😀\n<assets/logo.png>+<chunks/é.js>"
        );
        assert_eq!(
            shifts,
            vec![
                SourceMapShift::default(),
                SourceMapShift {
                    before: LineColumnOffset {
                        lines: 1,
                        columns: 15,
                    },
                    after: LineColumnOffset {
                        lines: 1,
                        columns: 17,
                    },
                },
                SourceMapShift {
                    before: LineColumnOffset {
                        lines: 1,
                        columns: 31,
                    },
                    after: LineColumnOffset {
                        lines: 1,
                        columns: 31,
                    },
                },
            ]
        );
    }

    #[test]
    fn joins_public_paths_like_upstream() {
        let cases = [
            ("", "x/y", "./x/y"),
            ("", "./x/y", "./x/y"),
            ("/assets", "x/y", "/assets/x/y"),
            ("/assets/", "x/y", "/assets/x/y"),
            ("https://cdn.test", ".///././/x/y", "https://cdn.test/x/y"),
        ];
        for (public_path, rel_path, expected) in cases {
            assert_eq!(join_with_public_path(public_path, rel_path), expected);
        }
    }

    #[test]
    fn computes_relative_and_public_chunk_paths() {
        let unix = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        assert_eq!(
            path_between_chunks(&unix, "", "out/a", "out/b/chunk.js").as_deref(),
            Some("../b/chunk.js")
        );
        assert_eq!(
            path_between_chunks(&unix, "", "out", "out/chunk.js").as_deref(),
            Some("./chunk.js")
        );
        assert_eq!(
            path_between_chunks(&unix, "/public/", "ignored", "./out/chunk.js").as_deref(),
            Some("/public/out/chunk.js")
        );

        let windows = mock_fs(&HashMap::new(), MockKind::Windows, "C:\\");
        assert_eq!(
            path_between_chunks(&windows, "", r"C:\out\a", r"C:\out\b\chunk.js",).as_deref(),
            Some("../b/chunk.js")
        );
        assert_eq!(
            path_between_chunks(&windows, "", r"C:\out", r"D:\out\chunk.js"),
            None
        );
    }

    #[test]
    fn adjacent_parts_from_the_same_file_share_a_range() {
        let mut ranges = Vec::new();
        append_or_extend_part_range(&mut ranges, 1, 2);
        append_or_extend_part_range(&mut ranges, 1, 3);
        append_or_extend_part_range(&mut ranges, 2, 4);
        append_or_extend_part_range(&mut ranges, 1, 4);
        assert_eq!(
            ranges,
            vec![
                PartRange {
                    source_index: 1,
                    part_index_begin: 2,
                    part_index_end: 4,
                },
                PartRange {
                    source_index: 2,
                    part_index_begin: 4,
                    part_index_end: 5,
                },
                PartRange {
                    source_index: 1,
                    part_index_begin: 4,
                    part_index_end: 5,
                },
            ]
        );
    }

    #[test]
    fn static_chunk_import_cycles_are_rejected() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let chunks = [
            ChunkInfo {
                cross_chunk_imports: vec![ChunkImport {
                    chunk_index: 1,
                    import_kind: ImportKind::Stmt,
                }],
                ..ChunkInfo::default()
            },
            ChunkInfo {
                cross_chunk_imports: vec![ChunkImport {
                    chunk_index: 0,
                    import_kind: ImportKind::Require,
                }],
                ..ChunkInfo::default()
            },
        ];
        enforce_no_cyclic_chunk_imports(&log, &chunks);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].data.text,
            "Internal error: generated chunks contain a circular import"
        );
    }

    #[test]
    fn dynamic_chunk_import_cycles_are_allowed() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let chunks = [
            ChunkInfo {
                cross_chunk_imports: vec![ChunkImport {
                    chunk_index: 1,
                    import_kind: ImportKind::Dynamic,
                }],
                ..ChunkInfo::default()
            },
            ChunkInfo {
                cross_chunk_imports: vec![ChunkImport {
                    chunk_index: 0,
                    import_kind: ImportKind::Stmt,
                }],
                ..ChunkInfo::default()
            },
        ];
        enforce_no_cyclic_chunk_imports(&log, &chunks);
        assert!(!log.has_errors());
    }

    #[test]
    fn already_validated_chunk_subgraphs_are_not_revisited() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let chunks = [
            ChunkInfo {
                cross_chunk_imports: vec![
                    ChunkImport {
                        chunk_index: 1,
                        import_kind: ImportKind::Stmt,
                    },
                    ChunkImport {
                        chunk_index: 2,
                        import_kind: ImportKind::Stmt,
                    },
                ],
                ..ChunkInfo::default()
            },
            ChunkInfo {
                cross_chunk_imports: vec![ChunkImport {
                    chunk_index: 2,
                    import_kind: ImportKind::Stmt,
                }],
                ..ChunkInfo::default()
            },
            ChunkInfo::default(),
        ];
        enforce_no_cyclic_chunk_imports(&log, &chunks);
        assert!(!log.has_errors());
    }

    fn token(text: &str) -> Token {
        Token {
            text: text.into(),
            kind: TokenKind::Ident,
            ..Token::default()
        }
    }

    fn media(text: &str) -> MediaQuery {
        MediaQuery {
            loc: Loc::default(),
            data: MediaQueryData::ArbitraryTokens(MediaArbitraryTokensQuery {
                tokens: vec![token(text)],
            }),
        }
    }

    #[test]
    fn import_condition_equality_ignores_whitespace() {
        let mut left_layer = token("layer");
        left_layer.whitespace = WhitespaceFlags::BEFORE;
        let mut right_supports = token("supports");
        right_supports.whitespace = WhitespaceFlags::AFTER;
        let left = [ImportConditions {
            layers: vec![left_layer],
            supports: vec![token("supports")],
            queries: vec![media("screen")],
        }];
        let right = [ImportConditions {
            layers: vec![token("layer")],
            supports: vec![right_supports],
            queries: vec![media("screen")],
        }];
        assert!(import_conditions_are_equal(&left, &right));

        let different = [ImportConditions {
            layers: vec![token("other")],
            ..right[0].clone()
        }];
        assert!(!import_conditions_are_equal(&left, &different));
    }

    #[test]
    fn later_import_condition_prefix_masks_earlier_import() {
        let shared_outer = ImportConditions {
            supports: vec![token("grid")],
            ..ImportConditions::default()
        };
        let earlier = [
            shared_outer.clone(),
            ImportConditions {
                layers: vec![token("theme")],
                supports: vec![token("flex")],
                queries: vec![media("screen")],
            },
        ];

        assert!(is_conditional_import_redundant(
            &earlier,
            std::slice::from_ref(&shared_outer)
        ));
        assert!(is_conditional_import_redundant(
            &earlier,
            &[
                shared_outer.clone(),
                ImportConditions {
                    layers: vec![token("theme")],
                    queries: vec![media("screen")],
                    ..ImportConditions::default()
                },
            ]
        ));
        assert!(is_conditional_import_redundant(
            &earlier,
            &[
                shared_outer,
                ImportConditions {
                    layers: vec![token("theme")],
                    supports: vec![token("flex")],
                    ..ImportConditions::default()
                },
            ]
        ));
    }

    #[test]
    fn incompatible_or_longer_import_conditions_are_not_redundant() {
        let earlier = [ImportConditions {
            layers: vec![token("theme")],
            supports: vec![token("flex")],
            queries: vec![media("screen")],
        }];
        assert!(!is_conditional_import_redundant(
            &earlier,
            &[ImportConditions {
                layers: vec![token("other")],
                ..ImportConditions::default()
            }]
        ));
        assert!(!is_conditional_import_redundant(
            &earlier,
            &[earlier[0].clone(), ImportConditions::default(),]
        ));
    }

    #[test]
    fn cross_chunk_imports_are_sorted_by_chunk_then_alias() {
        let refs = [
            Ref {
                source_index: 3,
                inner_index: 0,
            },
            Ref {
                source_index: 4,
                inner_index: 0,
            },
            Ref {
                source_index: 5,
                inner_index: 0,
            },
        ];
        let imports = HashMap::from([
            (
                2,
                vec![
                    CrossChunkImportItem {
                        reference: refs[0],
                        ..CrossChunkImportItem::default()
                    },
                    CrossChunkImportItem {
                        reference: refs[1],
                        ..CrossChunkImportItem::default()
                    },
                ],
            ),
            (
                0,
                vec![CrossChunkImportItem {
                    reference: refs[2],
                    ..CrossChunkImportItem::default()
                }],
            ),
        ]);
        let exports = [
            HashMap::from([(refs[2], "middle".into())]),
            HashMap::new(),
            HashMap::from([(refs[0], "zebra".into()), (refs[1], "alpha".into())]),
        ];
        assert_eq!(
            sorted_cross_chunk_imports(imports, &exports),
            vec![
                CrossChunkImport {
                    chunk_index: 0,
                    sorted_import_items: vec![CrossChunkImportItem {
                        export_alias: "middle".into(),
                        reference: refs[2],
                    }],
                },
                CrossChunkImport {
                    chunk_index: 2,
                    sorted_import_items: vec![
                        CrossChunkImportItem {
                            export_alias: "alpha".into(),
                            reference: refs[1],
                        },
                        CrossChunkImportItem {
                            export_alias: "zebra".into(),
                            reference: refs[0],
                        },
                    ],
                },
            ]
        );
    }

    #[test]
    fn cross_chunk_exports_use_stable_dfs_source_order() {
        let refs = HashSet::from([
            Ref {
                source_index: 0,
                inner_index: 7,
            },
            Ref {
                source_index: 2,
                inner_index: 3,
            },
            Ref {
                source_index: 1,
                inner_index: 9,
            },
            Ref {
                source_index: 2,
                inner_index: 1,
            },
        ]);
        let stable_source_indices = [2, 0, 1];
        assert_eq!(
            sorted_cross_chunk_export_items(&refs, &stable_source_indices),
            vec![
                StableRef {
                    stable_source_index: 0,
                    reference: Ref {
                        source_index: 1,
                        inner_index: 9,
                    },
                },
                StableRef {
                    stable_source_index: 1,
                    reference: Ref {
                        source_index: 2,
                        inner_index: 1,
                    },
                },
                StableRef {
                    stable_source_index: 1,
                    reference: Ref {
                        source_index: 2,
                        inner_index: 3,
                    },
                },
                StableRef {
                    stable_source_index: 2,
                    reference: Ref {
                        source_index: 0,
                        inner_index: 7,
                    },
                },
            ]
        );
    }

    fn js_file(ast: js_ast::Ast) -> InputFile {
        InputFile {
            repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                ast,
                ..JsRepr::default()
            }))),
            loader: Loader::Js,
            ..InputFile::default()
        }
    }

    fn js_repr(graph: &crate::internal::graph::LinkerGraph, source_index: usize) -> &JsRepr {
        let InputFileRepr::Js(repr) = graph.files[source_index]
            .input_file
            .repr
            .as_ref()
            .expect("representation")
        else {
            panic!("JavaScript representation");
        };
        repr
    }

    #[test]
    fn scan_step_one_classifies_imported_module_wrappers() {
        let importer = js_file(js_ast::Ast {
            import_records: vec![
                ImportRecord {
                    source_index: Index32::new(1),
                    kind: ImportKind::Stmt,
                    flags: ImportRecordFlags::CONTAINS_IMPORT_STAR,
                    ..ImportRecord::default()
                },
                ImportRecord {
                    source_index: Index32::new(2),
                    kind: ImportKind::Require,
                    ..ImportRecord::default()
                },
                ImportRecord {
                    source_index: Index32::new(3),
                    kind: ImportKind::Dynamic,
                    ..ImportRecord::default()
                },
                ImportRecord {
                    source_index: Index32::new(4),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                },
            ],
            ..js_ast::Ast::default()
        });
        let input_files = vec![
            importer,
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast::default()),
        ];
        let mut graph = clone_linker_graph(
            &input_files,
            &[0, 1, 2, 3, 4],
            &[EntryPoint {
                source_index: 0,
                ..EntryPoint::default()
            }],
            false,
        );
        classify_module_wrappers(
            &mut graph,
            &Options {
                output_format: Format::CommonJs,
                ..Options::default()
            },
        );

        assert_eq!(js_repr(&graph, 1).meta.wrap, WrapKind::Cjs);
        assert_eq!(js_repr(&graph, 1).ast.exports_kind, ExportsKind::CommonJs);
        assert_eq!(js_repr(&graph, 2).meta.wrap, WrapKind::Esm);
        assert_eq!(js_repr(&graph, 2).ast.exports_kind, ExportsKind::Esm);
        assert_eq!(js_repr(&graph, 3).meta.wrap, WrapKind::Cjs);
        assert_eq!(js_repr(&graph, 3).ast.exports_kind, ExportsKind::CommonJs);
        assert_eq!(js_repr(&graph, 4).meta.wrap, WrapKind::None);
        assert_eq!(js_repr(&graph, 4).ast.exports_kind, ExportsKind::None);
    }

    #[test]
    fn code_splitting_keeps_dynamic_import_targets_unwrapped() {
        let input_files = vec![
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(1),
                    kind: ImportKind::Dynamic,
                    ..ImportRecord::default()
                }],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast::default()),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &[EntryPoint::default()], true);
        classify_module_wrappers(
            &mut graph,
            &Options {
                code_splitting: true,
                ..Options::default()
            },
        );
        assert_eq!(js_repr(&graph, 1).meta.wrap, WrapKind::None);
        assert_eq!(js_repr(&graph, 1).ast.exports_kind, ExportsKind::None);
    }

    #[test]
    fn common_js_entry_point_only_avoids_wrapper_for_common_js_output() {
        let input_files = vec![
            js_file(js_ast::Ast {
                exports_kind: ExportsKind::CommonJs,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                exports_kind: ExportsKind::CommonJs,
                ..js_ast::Ast::default()
            }),
        ];
        let entry_points = [EntryPoint {
            source_index: 0,
            ..EntryPoint::default()
        }];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &entry_points, false);
        classify_module_wrappers(
            &mut graph,
            &Options {
                output_format: Format::CommonJs,
                ..Options::default()
            },
        );
        assert_eq!(js_repr(&graph, 0).meta.wrap, WrapKind::None);
        assert_eq!(js_repr(&graph, 1).meta.wrap, WrapKind::Cjs);

        let mut graph = clone_linker_graph(&input_files, &[0, 1], &entry_points, false);
        classify_module_wrappers(
            &mut graph,
            &Options {
                output_format: Format::EsModule,
                ..Options::default()
            },
        );
        assert_eq!(js_repr(&graph, 0).meta.wrap, WrapKind::Cjs);
    }

    #[test]
    fn scan_step_one_inlines_asset_and_copy_urls() {
        let asset_output = OutputFile {
            abs_path: "/out/logo-123.png".into(),
            contents: b"logo".to_vec(),
            ..OutputFile::default()
        };
        let copy_output = OutputFile {
            abs_path: "/out/copy.txt".into(),
            contents: b"copy".to_vec(),
            ..OutputFile::default()
        };
        let input_files = vec![
            InputFile {
                repr: Some(InputFileRepr::Css(Box::new(CssRepr {
                    ast: crate::internal::css_ast::Ast {
                        import_records: vec![
                            ImportRecord {
                                source_index: Index32::new(1),
                                kind: ImportKind::Url,
                                ..ImportRecord::default()
                            },
                            ImportRecord {
                                copy_source_index: Index32::new(2),
                                kind: ImportKind::Url,
                                ..ImportRecord::default()
                            },
                        ],
                        ..crate::internal::css_ast::Ast::default()
                    },
                    ..CssRepr::default()
                }))),
                loader: Loader::Css,
                ..InputFile::default()
            },
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast: js_ast::Ast {
                        url_for_css: "UNIQUEA00000001".into(),
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                additional_files: vec![asset_output.clone()],
                loader: Loader::File,
                ..InputFile::default()
            },
            InputFile {
                repr: Some(InputFileRepr::Copy(CopyRepr {
                    url_for_code: "UNIQUEA00000002".into(),
                })),
                additional_files: vec![copy_output.clone()],
                loader: Loader::Copy,
                ..InputFile::default()
            },
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    copy_source_index: Index32::new(2),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph =
            clone_linker_graph(&input_files, &[0, 1, 2, 3], &[EntryPoint::default()], false);
        inline_linked_assets(&mut graph, PREFIX);

        let InputFileRepr::Css(css) = graph.files[0]
            .input_file
            .repr
            .as_ref()
            .expect("CSS representation")
        else {
            panic!("CSS representation");
        };
        let asset_record = &css.ast.import_records[0];
        assert_eq!(asset_record.path.text, "UNIQUEA00000001");
        assert!(!asset_record.source_index.is_valid());
        assert!(
            asset_record
                .flags
                .contains(ImportRecordFlags::SHOULD_NOT_BE_EXTERNAL_IN_METAFILE)
        );
        assert!(
            asset_record
                .flags
                .contains(ImportRecordFlags::CONTAINS_UNIQUE_KEY)
        );
        let copy_record = &css.ast.import_records[1];
        assert_eq!(copy_record.path.text, "UNIQUEA00000002");
        assert!(!copy_record.copy_source_index.is_valid());
        assert_eq!(
            graph.files[0].input_file.additional_files,
            vec![asset_output, copy_output.clone()]
        );

        let js = js_repr(&graph, 3);
        assert_eq!(js.ast.import_records[0].path.text, "UNIQUEA00000002");
        assert!(!js.ast.import_records[0].copy_source_index.is_valid());
        assert_eq!(
            graph.files[3].input_file.additional_files,
            vec![copy_output]
        );
    }

    #[test]
    fn scan_step_two_recursively_wraps_dependency_cycles() {
        let input_files = vec![
            js_file(js_ast::Ast::default()),
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    meta: crate::internal::graph::JsReprMeta {
                        wrap: WrapKind::Esm,
                        ..crate::internal::graph::JsReprMeta::default()
                    },
                    ast: js_ast::Ast {
                        import_records: vec![ImportRecord {
                            source_index: Index32::new(2),
                            kind: ImportKind::Stmt,
                            ..ImportRecord::default()
                        }],
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                loader: Loader::Js,
                ..InputFile::default()
            },
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(3),
                    kind: ImportKind::Require,
                    ..ImportRecord::default()
                }],
                exports_kind: ExportsKind::CommonJs,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(1),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(
            &input_files,
            &[0, 1, 2, 3],
            &[EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            }],
            false,
        );
        propagate_wrappers_and_dynamic_exports(&mut graph, &Options::default());

        assert_eq!(js_repr(&graph, 0).meta.wrap, WrapKind::None);
        assert!(!js_repr(&graph, 0).meta.did_wrap_dependencies);
        assert_eq!(js_repr(&graph, 1).meta.wrap, WrapKind::Esm);
        assert!(js_repr(&graph, 1).meta.did_wrap_dependencies);
        assert_eq!(js_repr(&graph, 2).meta.wrap, WrapKind::Cjs);
        assert!(js_repr(&graph, 2).meta.did_wrap_dependencies);
        assert_eq!(js_repr(&graph, 3).meta.wrap, WrapKind::Esm);
        assert!(js_repr(&graph, 3).meta.did_wrap_dependencies);
    }

    #[test]
    fn recursively_wrapping_runtime_only_marks_it_visited() {
        let input_files = vec![js_file(js_ast::Ast::default())];
        let mut graph = clone_linker_graph(&input_files, &[0], &[EntryPoint::default()], false);
        recursively_wrap_dependencies(&mut graph, crate::internal::runtime::SOURCE_INDEX);
        assert_eq!(js_repr(&graph, 0).meta.wrap, WrapKind::None);
        assert!(js_repr(&graph, 0).meta.did_wrap_dependencies);
    }

    #[test]
    fn export_star_chains_propagate_dynamic_exports() {
        let input_files = vec![
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                export_star_import_records: vec![0],
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(3),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                export_star_import_records: vec![0],
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                exports_kind: ExportsKind::CommonJs,
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(
            &input_files,
            &[0, 1, 2, 3],
            &[EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            }],
            false,
        );
        propagate_wrappers_and_dynamic_exports(
            &mut graph,
            &Options {
                output_format: Format::EsModule,
                ..Options::default()
            },
        );
        assert_eq!(
            js_repr(&graph, 1).ast.exports_kind,
            ExportsKind::EsmWithDynamicFallback
        );
        assert_eq!(
            js_repr(&graph, 2).ast.exports_kind,
            ExportsKind::EsmWithDynamicFallback
        );
        assert_eq!(js_repr(&graph, 3).meta.wrap, WrapKind::Cjs);
    }

    #[test]
    fn external_export_star_depends_on_entry_point_and_output_format() {
        let external_star = || {
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                export_star_import_records: vec![0],
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            })
        };
        let input_files = vec![
            js_file(js_ast::Ast::default()),
            external_star(),
            external_star(),
        ];
        let entry_points = [EntryPoint {
            source_index: 1,
            ..EntryPoint::default()
        }];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &entry_points, false);
        propagate_wrappers_and_dynamic_exports(
            &mut graph,
            &Options {
                output_format: Format::Preserve,
                ..Options::default()
            },
        );
        assert_eq!(js_repr(&graph, 1).ast.exports_kind, ExportsKind::Esm);
        assert_eq!(
            js_repr(&graph, 2).ast.exports_kind,
            ExportsKind::EsmWithDynamicFallback
        );

        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &entry_points, false);
        propagate_wrappers_and_dynamic_exports(
            &mut graph,
            &Options {
                output_format: Format::CommonJs,
                ..Options::default()
            },
        );
        assert_eq!(
            js_repr(&graph, 1).ast.exports_kind,
            ExportsKind::EsmWithDynamicFallback
        );
    }

    #[test]
    fn export_star_self_cycle_is_not_dynamic_by_itself() {
        let input_files = vec![js_file(js_ast::Ast {
            import_records: vec![ImportRecord {
                source_index: Index32::new(0),
                kind: ImportKind::Stmt,
                ..ImportRecord::default()
            }],
            export_star_import_records: vec![0],
            exports_kind: ExportsKind::Esm,
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(&input_files, &[0], &[EntryPoint::default()], false);
        assert!(!has_dynamic_exports_due_to_export_star(
            &mut graph,
            0,
            &mut HashSet::new(),
            Format::EsModule
        ));
        assert_eq!(js_repr(&graph, 0).ast.exports_kind, ExportsKind::Esm);
    }

    fn named_export(reference: Ref) -> NamedExport {
        NamedExport {
            reference,
            ..NamedExport::default()
        }
    }

    #[test]
    fn scan_step_three_resolves_export_star_chains() {
        let root_x = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let child_z = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let child_default = Ref {
            source_index: 2,
            inner_index: 1,
        };
        let leaf_y = Ref {
            source_index: 3,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                named_exports: HashMap::from([("x".into(), named_export(root_x))]),
                export_star_import_records: vec![0],
                exports_ref: Ref {
                    source_index: 1,
                    inner_index: 9,
                },
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(3),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                named_exports: HashMap::from([
                    ("z".into(), named_export(child_z)),
                    ("default".into(), named_export(child_default)),
                ]),
                export_star_import_records: vec![0],
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                named_exports: HashMap::from([("y".into(), named_export(leaf_y))]),
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(
            &input_files,
            &[0, 1, 2, 3],
            &[EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            }],
            false,
        );
        resolve_export_stars(&mut graph);

        let root = js_repr(&graph, 1);
        assert_eq!(
            root.meta
                .resolved_exports
                .keys()
                .cloned()
                .collect::<HashSet<_>>(),
            HashSet::from(["x".into(), "y".into(), "z".into()])
        );
        assert_eq!(root.meta.resolved_exports["x"].source_index, 1);
        assert_eq!(root.meta.resolved_exports["z"].source_index, 2);
        assert_eq!(root.meta.resolved_exports["y"].source_index, 3);
        assert!(!root.meta.resolved_exports.contains_key("default"));
        assert_eq!(root.meta.imports_to_bind[&child_z].source_index, 2);
        assert_eq!(
            root.meta
                .resolved_export_star
                .as_ref()
                .expect("namespace export")
                .reference,
            Ref {
                source_index: 1,
                inner_index: 9,
            }
        );
        assert_eq!(
            js_repr(&graph, 2).meta.imports_to_bind[&leaf_y].source_index,
            3
        );
    }

    #[test]
    fn local_exports_shadow_export_stars() {
        let local = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let imported = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    ..ImportRecord::default()
                }],
                named_exports: HashMap::from([("same".into(), named_export(local))]),
                export_star_import_records: vec![0],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                named_exports: HashMap::from([("same".into(), named_export(imported))]),
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph =
            clone_linker_graph(&input_files, &[0, 1, 2], &[EntryPoint::default()], false);
        resolve_export_stars(&mut graph);
        let root = js_repr(&graph, 1);
        assert_eq!(root.meta.resolved_exports["same"].reference, local);
        assert!(!root.meta.imports_to_bind.contains_key(&imported));
    }

    #[test]
    fn colliding_export_stars_are_recorded_as_ambiguous() {
        let first = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let second = Ref {
            source_index: 3,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                import_records: vec![
                    ImportRecord {
                        source_index: Index32::new(2),
                        ..ImportRecord::default()
                    },
                    ImportRecord {
                        source_index: Index32::new(3),
                        ..ImportRecord::default()
                    },
                ],
                export_star_import_records: vec![0, 1],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                named_exports: HashMap::from([("same".into(), named_export(first))]),
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                named_exports: HashMap::from([("same".into(), named_export(second))]),
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph =
            clone_linker_graph(&input_files, &[0, 1, 2, 3], &[EntryPoint::default()], false);
        resolve_export_stars(&mut graph);
        let export = &js_repr(&graph, 1).meta.resolved_exports["same"];
        assert_eq!(export.reference, first);
        assert_eq!(export.potentially_ambiguous_export_star_refs.len(), 1);
        assert_eq!(
            export.potentially_ambiguous_export_star_refs[0].source_index,
            3
        );
        assert_eq!(
            export.potentially_ambiguous_export_star_refs[0].reference,
            second
        );
    }

    #[test]
    fn common_js_export_stars_are_left_for_runtime() {
        let common_js_export = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    ..ImportRecord::default()
                }],
                export_star_import_records: vec![0],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                named_exports: HashMap::from([(
                    "runtimeOnly".into(),
                    named_export(common_js_export),
                )]),
                exports_kind: ExportsKind::CommonJs,
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph =
            clone_linker_graph(&input_files, &[0, 1, 2], &[EntryPoint::default()], false);
        let mut resolved = HashMap::new();
        add_exports_for_export_star(&mut graph, &mut resolved, 1, &mut Vec::new());
        assert!(resolved.is_empty());
    }
}
