//! Port of upstream `internal/linker`.
//!
//! The linker is esbuild's second bundling phase. This initial section ports
//! the output-piece representation and the final path substitution machinery
//! used after chunks have been generated.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

use crate::internal::{
    ast::{
        INVALID_REF, ImportItemStatus, ImportKind, ImportRecordFlags, Index32, LocRef,
        NamespaceAlias, Ref, SymbolKind,
    },
    bundler::{hash_for_file_name, path_relative_to_outbase},
    config::{
        Format, Loader, Mode, Options, PathPlaceholder, PathPlaceholders, PathTemplate,
        has_placeholder, substitute_template, template_to_string,
    },
    css_ast::{
        ImportConditions, media_queries_equal_ignoring_whitespace, tokens_equal_ignoring_whitespace,
    },
    fs::Fs,
    graph::{ExportData, ImportData, InputFileRepr, LinkerGraph, SideEffectsKind, WrapKind},
    helpers::{BitSet, Joiner},
    js_ast::{self, ExportsKind},
    logger::{Log, Range},
    sourcemap::{LineColumnOffset, SourceMapPieces, SourceMapShift},
    xxhash,
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
    pub files_with_parts_in_chunk: HashSet<u32>,
    pub entry_bits: BitSet,
    pub cross_chunk_imports: Vec<ChunkImport>,
    pub exports_to_other_chunks: HashMap<Ref, String>,
    pub imports_from_other_chunks: HashMap<u32, Vec<CrossChunkImportItem>>,
    pub sorted_cross_chunk_imports: Vec<CrossChunkImport>,
    pub cross_chunk_prefix_stmts: Vec<js_ast::Stmt>,
    pub cross_chunk_suffix_stmts: Vec<js_ast::Stmt>,
    pub files_in_chunk_in_order: Vec<u32>,
    pub parts_in_chunk_in_order: Vec<PartRange>,
    pub final_template: Vec<PathTemplate>,
    pub final_rel_path: String,
    pub intermediate_output: IntermediateOutput,
    pub output_source_map: SourceMapPieces,
    pub isolated_hash: Vec<u8>,
    pub entry_point_bit: usize,
    pub source_index: u32,
    pub is_entry_point: bool,
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
pub struct ImportTracker {
    pub source_index: u32,
    pub name_loc: crate::internal::logger::Loc,
    pub import_ref: Ref,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum ImportStatus {
    #[default]
    NoMatch,
    Found,
    CommonJs,
    DynamicFallback,
    CommonJsWithoutExports,
    Disabled,
    External,
    ProbablyTypeScriptType,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum MatchImportKind {
    #[default]
    Ignore,
    Normal,
    Namespace,
    NormalAndNamespace,
    Cycle,
    ProbablyTypeScriptType,
    Ambiguous,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MatchImportResult {
    pub alias: String,
    pub kind: MatchImportKind,
    pub namespace_ref: Ref,
    pub source_index: u32,
    pub name_loc: crate::internal::logger::Loc,
    pub other_source_index: u32,
    pub other_name_loc: crate::internal::logger::Loc,
    pub reference: Ref,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportMatchIssue {
    pub import_ref: Ref,
    pub result: MatchImportResult,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AmbiguousReExport {
    pub alias: String,
    pub source_index: u32,
    pub name_loc: crate::internal::logger::Loc,
    pub other_source_index: u32,
    pub other_name_loc: crate::internal::logger::Loc,
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

/// Advance import resolution by one re-export edge.
///
/// # Panics
///
/// Panics if the tracker does not reference a named import or if a resolved
/// import points to a non-JavaScript representation, matching linker graph
/// invariants.
#[must_use]
pub fn advance_import_tracker(
    graph: &LinkerGraph,
    tracker: ImportTracker,
) -> (ImportTracker, ImportStatus, Vec<ImportData>) {
    let file = &graph.files[tracker.source_index as usize];
    let InputFileRepr::Js(repr) = file
        .input_file
        .repr
        .as_ref()
        .expect("import tracker source must have a representation")
    else {
        panic!("import tracker source must be JavaScript");
    };
    let named_import = &repr.ast.named_imports[&tracker.import_ref];
    let record = &repr.ast.import_records[named_import.import_record_index as usize];
    if !record.source_index.is_valid() {
        return (ImportTracker::default(), ImportStatus::External, Vec::new());
    }

    let other_source_index = record.source_index.get_index();
    let InputFileRepr::Js(other) = graph.files[other_source_index as usize]
        .input_file
        .repr
        .as_ref()
        .expect("resolved import target must have a representation")
    else {
        panic!("resolved import target must be JavaScript");
    };

    if !named_import.alias_is_star
        && !other.ast.has_lazy_export
        && other.ast.export_keyword.len == 0
        && named_import.alias != "default"
        && !other.ast.uses_exports_ref
        && !other.ast.uses_module_ref
    {
        return (
            ImportTracker {
                source_index: other_source_index,
                import_ref: INVALID_REF,
                ..ImportTracker::default()
            },
            ImportStatus::CommonJsWithoutExports,
            Vec::new(),
        );
    }

    if other.ast.exports_kind == ExportsKind::CommonJs {
        return (
            ImportTracker {
                source_index: other_source_index,
                import_ref: INVALID_REF,
                ..ImportTracker::default()
            },
            ImportStatus::CommonJs,
            Vec::new(),
        );
    }

    let matching_export = if named_import.alias_is_star {
        other.meta.resolved_export_star.as_ref()
    } else {
        other.meta.resolved_exports.get(&named_import.alias)
    };
    if let Some(matching_export) = matching_export {
        return (
            ImportTracker {
                source_index: matching_export.source_index,
                import_ref: matching_export.reference,
                name_loc: matching_export.name_loc,
            },
            ImportStatus::Found,
            matching_export
                .potentially_ambiguous_export_star_refs
                .clone(),
        );
    }

    if other.ast.exports_kind == ExportsKind::EsmWithDynamicFallback {
        return (
            ImportTracker {
                source_index: other_source_index,
                import_ref: other.ast.exports_ref,
                ..ImportTracker::default()
            },
            ImportStatus::DynamicFallback,
            Vec::new(),
        );
    }

    if file.input_file.loader.is_type_script() && named_import.is_exported {
        return (
            ImportTracker::default(),
            ImportStatus::ProbablyTypeScriptType,
            Vec::new(),
        );
    }

    (
        ImportTracker {
            source_index: other_source_index,
            ..ImportTracker::default()
        },
        ImportStatus::NoMatch,
        Vec::new(),
    )
}

/// Follow an import through all re-export edges until it reaches its final
/// binding or a terminal import status.
///
/// # Panics
///
/// Panics when import trackers violate JavaScript linker graph invariants.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn match_import_with_export(
    graph: &LinkerGraph,
    mut tracker: ImportTracker,
    mut re_exports: Vec<crate::internal::js_ast::Dependency>,
    cycle_detector: &mut Vec<ImportTracker>,
    output_format: Format,
) -> (MatchImportResult, Vec<crate::internal::js_ast::Dependency>) {
    let mut result = MatchImportResult::default();
    let mut ambiguous_results = Vec::new();

    loop {
        if cycle_detector.contains(&tracker) {
            result.kind = MatchImportKind::Cycle;
            break;
        }
        cycle_detector.push(tracker);

        let (next_tracker, status, potentially_ambiguous) = advance_import_tracker(graph, tracker);
        match status {
            ImportStatus::CommonJs
            | ImportStatus::CommonJsWithoutExports
            | ImportStatus::External
            | ImportStatus::Disabled => {
                if status != ImportStatus::External
                    || !output_format.keep_esm_import_export_syntax()
                {
                    let InputFileRepr::Js(repr) = graph.files[tracker.source_index as usize]
                        .input_file
                        .repr
                        .as_ref()
                        .expect("import tracker source must have a representation")
                    else {
                        panic!("import tracker source must be JavaScript");
                    };
                    let named_import = &repr.ast.named_imports[&tracker.import_ref];
                    if named_import.namespace_ref != INVALID_REF {
                        if result.kind == MatchImportKind::Normal {
                            result.kind = MatchImportKind::NormalAndNamespace;
                            result.namespace_ref = named_import.namespace_ref;
                            result.alias.clone_from(&named_import.alias);
                        } else {
                            result = MatchImportResult {
                                kind: MatchImportKind::Namespace,
                                namespace_ref: named_import.namespace_ref,
                                alias: named_import.alias.clone(),
                                ..MatchImportResult::default()
                            };
                        }
                    }
                }
            }

            ImportStatus::DynamicFallback => {
                let InputFileRepr::Js(repr) = graph.files[tracker.source_index as usize]
                    .input_file
                    .repr
                    .as_ref()
                    .expect("import tracker source must have a representation")
                else {
                    panic!("import tracker source must be JavaScript");
                };
                let named_import = &repr.ast.named_imports[&tracker.import_ref];
                if result.kind == MatchImportKind::Normal {
                    result.kind = MatchImportKind::NormalAndNamespace;
                    result.namespace_ref = next_tracker.import_ref;
                    result.alias.clone_from(&named_import.alias);
                } else {
                    result = MatchImportResult {
                        kind: MatchImportKind::Namespace,
                        namespace_ref: next_tracker.import_ref,
                        alias: named_import.alias.clone(),
                        ..MatchImportResult::default()
                    };
                }
            }

            ImportStatus::NoMatch => {}

            ImportStatus::ProbablyTypeScriptType => {
                result = MatchImportResult {
                    kind: MatchImportKind::ProbablyTypeScriptType,
                    ..MatchImportResult::default()
                };
            }

            ImportStatus::Found => {
                for ambiguous in potentially_ambiguous {
                    let InputFileRepr::Js(ambiguous_repr) = graph.files
                        [ambiguous.source_index as usize]
                        .input_file
                        .repr
                        .as_ref()
                        .expect("ambiguous export source must have a representation")
                    else {
                        panic!("ambiguous export source must be JavaScript");
                    };
                    if ambiguous_repr
                        .ast
                        .named_imports
                        .contains_key(&ambiguous.reference)
                    {
                        let mut nested_cycle_detector = cycle_detector.clone();
                        let (ambiguous_result, new_re_exports) = match_import_with_export(
                            graph,
                            ImportTracker {
                                source_index: ambiguous.source_index,
                                import_ref: ambiguous.reference,
                                ..ImportTracker::default()
                            },
                            re_exports,
                            &mut nested_cycle_detector,
                            output_format,
                        );
                        ambiguous_results.push(ambiguous_result);
                        re_exports = new_re_exports;
                    } else {
                        ambiguous_results.push(MatchImportResult {
                            kind: MatchImportKind::Normal,
                            source_index: ambiguous.source_index,
                            reference: ambiguous.reference,
                            name_loc: ambiguous.name_loc,
                            ..MatchImportResult::default()
                        });
                    }
                }

                result = MatchImportResult {
                    kind: MatchImportKind::Normal,
                    source_index: next_tracker.source_index,
                    reference: next_tracker.import_ref,
                    name_loc: next_tracker.name_loc,
                    ..MatchImportResult::default()
                };

                let InputFileRepr::Js(repr) = graph.files[tracker.source_index as usize]
                    .input_file
                    .repr
                    .as_ref()
                    .expect("import tracker source must have a representation")
                else {
                    panic!("import tracker source must be JavaScript");
                };
                if let Some(part_indices) = repr.top_level_symbol_to_parts(tracker.import_ref) {
                    re_exports.extend(part_indices.iter().map(|&part_index| {
                        crate::internal::js_ast::Dependency {
                            source_index: tracker.source_index,
                            part_index,
                        }
                    }));
                }

                let InputFileRepr::Js(next_repr) = graph.files[next_tracker.source_index as usize]
                    .input_file
                    .repr
                    .as_ref()
                    .expect("resolved export source must have a representation")
                else {
                    panic!("resolved export source must be JavaScript");
                };
                if next_repr
                    .ast
                    .named_imports
                    .contains_key(&next_tracker.import_ref)
                {
                    tracker = next_tracker;
                    continue;
                }
            }
        }
        break;
    }

    for ambiguous_result in ambiguous_results {
        if ambiguous_result != result {
            if result.kind == MatchImportKind::Normal
                && ambiguous_result.kind == MatchImportKind::Normal
                && result.name_loc.start != 0
                && ambiguous_result.name_loc.start != 0
            {
                return (
                    MatchImportResult {
                        kind: MatchImportKind::Ambiguous,
                        source_index: result.source_index,
                        name_loc: result.name_loc,
                        other_source_index: ambiguous_result.source_index,
                        other_name_loc: ambiguous_result.name_loc,
                        ..MatchImportResult::default()
                    },
                    Vec::new(),
                );
            }
            return (
                MatchImportResult {
                    kind: MatchImportKind::Ambiguous,
                    ..MatchImportResult::default()
                },
                Vec::new(),
            );
        }
    }

    (result, re_exports)
}

/// Match and bind all named imports in one file in deterministic symbol order.
/// Cycles and ambiguities are returned for the diagnostic layer.
///
/// # Panics
///
/// Panics when the source or its import symbols violate linker graph
/// invariants.
#[must_use]
pub fn bind_imports_to_exports_for_file(
    graph: &mut LinkerGraph,
    source_index: u32,
    output_format: Format,
) -> Vec<ImportMatchIssue> {
    let mut import_refs = {
        let InputFileRepr::Js(repr) = graph.files[source_index as usize]
            .input_file
            .repr
            .as_ref()
            .expect("import source must have a representation")
        else {
            panic!("import source must be JavaScript");
        };
        repr.ast.named_imports.keys().copied().collect::<Vec<_>>()
    };
    import_refs.sort_unstable_by_key(|reference| reference.inner_index);

    let mut issues = Vec::new();
    for import_ref in import_refs {
        let (result, re_exports) = match_import_with_export(
            graph,
            ImportTracker {
                source_index,
                import_ref,
                ..ImportTracker::default()
            },
            Vec::new(),
            &mut Vec::new(),
            output_format,
        );

        if matches!(
            result.kind,
            MatchImportKind::Normal | MatchImportKind::NormalAndNamespace
        ) {
            let InputFileRepr::Js(repr) = graph.files[source_index as usize]
                .input_file
                .repr
                .as_mut()
                .expect("import source must have a representation")
            else {
                unreachable!();
            };
            repr.meta.imports_to_bind.insert(
                import_ref,
                ImportData {
                    re_exports,
                    source_index: result.source_index,
                    reference: result.reference,
                    ..ImportData::default()
                },
            );
        }

        if matches!(
            result.kind,
            MatchImportKind::Namespace | MatchImportKind::NormalAndNamespace
        ) {
            graph.symbols.get_mut(import_ref).namespace_alias = Some(NamespaceAlias {
                namespace_ref: result.namespace_ref,
                alias: result.alias.clone(),
            });
        }

        match result.kind {
            MatchImportKind::ProbablyTypeScriptType => {
                let InputFileRepr::Js(repr) = graph.files[source_index as usize]
                    .input_file
                    .repr
                    .as_mut()
                    .expect("import source must have a representation")
                else {
                    unreachable!();
                };
                repr.meta
                    .is_probably_type_script_type
                    .insert(import_ref, true);
            }
            MatchImportKind::Cycle | MatchImportKind::Ambiguous => {
                issues.push(ImportMatchIssue { import_ref, result });
            }
            _ => {}
        }
    }
    issues
}

/// Sort resolved export aliases and remove ambiguous or type-only re-exports.
///
/// # Panics
///
/// Panics when resolved exports point to non-JavaScript representations,
/// matching linker graph invariants.
#[must_use]
pub fn sort_and_filter_export_aliases(
    graph: &mut LinkerGraph,
    source_index: u32,
) -> Vec<AmbiguousReExport> {
    let resolved_exports = {
        let InputFileRepr::Js(repr) = graph.files[source_index as usize]
            .input_file
            .repr
            .as_ref()
            .expect("export source must have a representation")
        else {
            panic!("export source must be JavaScript");
        };
        repr.meta.resolved_exports.clone()
    };

    let mut aliases = Vec::with_capacity(resolved_exports.len());
    let mut ambiguous = Vec::new();
    for (alias, export) in resolved_exports {
        if !export.potentially_ambiguous_export_star_refs.is_empty() {
            let InputFileRepr::Js(main_repr) = graph.files[export.source_index as usize]
                .input_file
                .repr
                .as_ref()
                .expect("export target must have a representation")
            else {
                panic!("export target must be JavaScript");
            };
            let (main_ref, main_loc) = main_repr
                .meta
                .imports_to_bind
                .get(&export.reference)
                .map_or((export.reference, export.name_loc), |import| {
                    (import.reference, import.name_loc)
                });

            let mut is_ambiguous = false;
            for candidate in &export.potentially_ambiguous_export_star_refs {
                let InputFileRepr::Js(candidate_repr) = graph.files
                    [candidate.source_index as usize]
                    .input_file
                    .repr
                    .as_ref()
                    .expect("ambiguous export target must have a representation")
                else {
                    panic!("ambiguous export target must be JavaScript");
                };
                let (candidate_ref, candidate_loc) = candidate_repr
                    .meta
                    .imports_to_bind
                    .get(&candidate.reference)
                    .map_or((candidate.reference, candidate.name_loc), |import| {
                        (import.reference, import.name_loc)
                    });
                if main_ref != candidate_ref {
                    ambiguous.push(AmbiguousReExport {
                        alias: alias.clone(),
                        source_index: export.source_index,
                        name_loc: main_loc,
                        other_source_index: candidate.source_index,
                        other_name_loc: candidate_loc,
                    });
                    is_ambiguous = true;
                    break;
                }
            }
            if is_ambiguous {
                continue;
            }
        }

        let InputFileRepr::Js(other) = graph.files[export.source_index as usize]
            .input_file
            .repr
            .as_ref()
            .expect("export target must have a representation")
        else {
            panic!("export target must be JavaScript");
        };
        if other
            .meta
            .is_probably_type_script_type
            .get(&export.reference)
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        aliases.push(alias);
    }
    aliases.sort_unstable();

    let InputFileRepr::Js(repr) = graph.files[source_index as usize]
        .input_file
        .repr
        .as_mut()
        .expect("export source must have a representation")
    else {
        unreachable!();
    };
    repr.meta.sorted_and_filtered_export_aliases = aliases;
    ambiguous
}

/// Create the synthetic wrapper part used by wrapped `CommonJS` and ESM files.
///
/// # Panics
///
/// Panics when the source, runtime, symbols, or runtime part maps violate
/// linker graph invariants.
pub fn create_wrapper_for_file(
    graph: &mut LinkerGraph,
    source_index: u32,
    cjs_runtime_ref: Ref,
    esm_runtime_ref: Ref,
) {
    let (wrap, exports_ref, module_ref, wrapper_ref) = {
        let InputFileRepr::Js(repr) = graph.files[source_index as usize]
            .input_file
            .repr
            .as_ref()
            .expect("wrapper source must have a representation")
        else {
            panic!("wrapper source must be JavaScript");
        };
        (
            repr.meta.wrap,
            repr.ast.exports_ref,
            repr.ast.module_ref,
            repr.ast.wrapper_ref,
        )
    };
    let runtime_ref = match wrap {
        WrapKind::Cjs => cjs_runtime_ref,
        WrapKind::Esm => esm_runtime_ref,
        WrapKind::None => return,
    };
    let runtime_parts = {
        let InputFileRepr::Js(runtime) = graph.files
            [crate::internal::runtime::SOURCE_INDEX as usize]
            .input_file
            .repr
            .as_ref()
            .expect("runtime must have a representation")
        else {
            panic!("runtime must be JavaScript");
        };
        runtime
            .top_level_symbol_to_parts(runtime_ref)
            .unwrap_or_default()
            .to_vec()
    };
    let declared_symbols = match wrap {
        WrapKind::Cjs => vec![
            crate::internal::js_ast::DeclaredSymbol {
                reference: exports_ref,
                is_top_level: true,
            },
            crate::internal::js_ast::DeclaredSymbol {
                reference: module_ref,
                is_top_level: true,
            },
            crate::internal::js_ast::DeclaredSymbol {
                reference: wrapper_ref,
                is_top_level: true,
            },
        ],
        WrapKind::Esm => vec![crate::internal::js_ast::DeclaredSymbol {
            reference: wrapper_ref,
            is_top_level: true,
        }],
        WrapKind::None => unreachable!(),
    };
    let part_index = graph.add_part_to_file(
        source_index,
        crate::internal::js_ast::Part {
            symbol_uses: HashMap::from([(
                wrapper_ref,
                crate::internal::js_ast::SymbolUse { count_estimate: 1 },
            )]),
            declared_symbols,
            dependencies: runtime_parts
                .iter()
                .map(|&part_index| crate::internal::js_ast::Dependency {
                    source_index: crate::internal::runtime::SOURCE_INDEX,
                    part_index,
                })
                .collect(),
            ..crate::internal::js_ast::Part::default()
        },
    );
    let InputFileRepr::Js(repr) = graph.files[source_index as usize]
        .input_file
        .repr
        .as_mut()
        .expect("wrapper source must have a representation")
    else {
        unreachable!();
    };
    repr.meta.wrapper_part_index = Index32::new(part_index);
    graph.generate_symbol_import_and_use(
        source_index,
        part_index,
        runtime_ref,
        1,
        crate::internal::runtime::SOURCE_INDEX,
    );
}

enum TreeShakingReprSnapshot {
    Js {
        css_source_index: Index32,
        import_records: Vec<crate::internal::ast::ImportRecord>,
        parts: Vec<crate::internal::js_ast::Part>,
    },
    Css(Vec<crate::internal::ast::ImportRecord>),
    Copy,
}

enum CodeSplittingReprSnapshot {
    Js {
        css_source_index: Index32,
        import_records: Vec<crate::internal::ast::ImportRecord>,
        dependencies: Vec<crate::internal::js_ast::Dependency>,
    },
    Css(Vec<crate::internal::ast::ImportRecord>),
    Copy,
}

#[must_use]
pub fn is_external_dynamic_import(
    graph: &LinkerGraph,
    options: &Options,
    record: &crate::internal::ast::ImportRecord,
    source_index: u32,
) -> bool {
    options.code_splitting
        && record.kind == ImportKind::Dynamic
        && graph.files[record.source_index.get_index() as usize].is_entry_point()
        && record.source_index.get_index() != source_index
}

/// Mark one JavaScript part and its dependency closure as live.
///
/// # Panics
///
/// Panics when the part dependency graph violates linker graph invariants.
pub fn mark_part_live_for_tree_shaking(
    graph: &mut LinkerGraph,
    options: &Options,
    source_index: u32,
    part_index: u32,
) {
    let dependencies = {
        let InputFileRepr::Js(repr) = graph.files[source_index as usize]
            .input_file
            .repr
            .as_mut()
            .expect("live part source must have a representation")
        else {
            panic!("live part source must be JavaScript");
        };
        let part = &mut repr.ast.parts[part_index as usize];
        if part.is_live {
            return;
        }
        part.is_live = true;
        part.dependencies.clone()
    };

    mark_file_live_for_tree_shaking(graph, options, source_index);
    for dependency in dependencies {
        mark_part_live_for_tree_shaking(
            graph,
            options,
            dependency.source_index,
            dependency.part_index,
        );
    }
}

/// Mark one file and all side-effect-relevant contents as live.
///
/// # Panics
///
/// Panics when import records or dependency parts violate linker graph
/// invariants.
pub fn mark_file_live_for_tree_shaking(
    graph: &mut LinkerGraph,
    options: &Options,
    source_index: u32,
) {
    let file = &mut graph.files[source_index as usize];
    if file.is_live {
        return;
    }
    file.is_live = true;
    let is_entry_point = file.is_entry_point();

    let snapshot = match file
        .input_file
        .repr
        .as_ref()
        .expect("live file must have a representation")
    {
        InputFileRepr::Js(repr) => TreeShakingReprSnapshot::Js {
            css_source_index: repr.css_source_index,
            import_records: repr.ast.import_records.clone(),
            parts: repr.ast.parts.clone(),
        },
        InputFileRepr::Css(repr) => TreeShakingReprSnapshot::Css(repr.ast.import_records.clone()),
        InputFileRepr::Copy(_) => TreeShakingReprSnapshot::Copy,
    };

    match snapshot {
        TreeShakingReprSnapshot::Js {
            css_source_index,
            import_records,
            parts,
        } => {
            if css_source_index.is_valid() {
                mark_file_live_for_tree_shaking(graph, options, css_source_index.get_index());
            }
            for (part_index, part) in parts.iter().enumerate() {
                let mut can_be_removed_if_unused = part.can_be_removed_if_unused;
                for &record_index in &part.import_record_indices {
                    let record = &import_records[record_index as usize];
                    if record.kind != ImportKind::Stmt {
                        continue;
                    }
                    if record.source_index.is_valid() {
                        let other_source_index = record.source_index.get_index();
                        if graph.files[other_source_index as usize]
                            .input_file
                            .side_effects
                            .kind
                            != SideEffectsKind::HasSideEffects
                            && !options.ignore_dce_annotations
                        {
                            continue;
                        }
                        mark_file_live_for_tree_shaking(graph, options, other_source_index);
                    } else if record
                        .flags
                        .contains(ImportRecordFlags::IS_EXTERNAL_WITHOUT_SIDE_EFFECTS)
                    {
                        continue;
                    }
                    can_be_removed_if_unused = false;
                }

                if !can_be_removed_if_unused
                    || (!part.force_tree_shaking && !options.tree_shaking && is_entry_point)
                {
                    mark_part_live_for_tree_shaking(
                        graph,
                        options,
                        source_index,
                        u32::try_from(part_index).expect("part index fits in u32"),
                    );
                }
            }
        }
        TreeShakingReprSnapshot::Css(import_records) => {
            for record in import_records {
                if record.source_index.is_valid() {
                    mark_file_live_for_tree_shaking(
                        graph,
                        options,
                        record.source_index.get_index(),
                    );
                }
            }
        }
        TreeShakingReprSnapshot::Copy => {}
    }
}

/// Propagate an entry-point bit and minimum distance through all live files.
///
/// # Panics
///
/// Panics when entry bits, imports, or part dependencies violate linker graph
/// invariants.
pub fn mark_file_reachable_for_code_splitting(
    graph: &mut LinkerGraph,
    options: &Options,
    source_index: u32,
    entry_point_bit: usize,
    distance_from_entry_point: u32,
) {
    let file = &mut graph.files[source_index as usize];
    if !file.is_live {
        return;
    }
    let mut traverse_again = false;
    if distance_from_entry_point < file.distance_from_entry_point {
        file.distance_from_entry_point = distance_from_entry_point;
        traverse_again = true;
    }
    let distance_from_entry_point = distance_from_entry_point.wrapping_add(1);
    if file.entry_bits.has_bit(entry_point_bit) && !traverse_again {
        return;
    }
    file.entry_bits.set_bit(entry_point_bit);

    let snapshot = match file
        .input_file
        .repr
        .as_ref()
        .expect("reachable file must have a representation")
    {
        InputFileRepr::Js(repr) => CodeSplittingReprSnapshot::Js {
            css_source_index: repr.css_source_index,
            import_records: repr.ast.import_records.clone(),
            dependencies: repr
                .ast
                .parts
                .iter()
                .flat_map(|part| part.dependencies.iter().copied())
                .collect(),
        },
        InputFileRepr::Css(repr) => CodeSplittingReprSnapshot::Css(repr.ast.import_records.clone()),
        InputFileRepr::Copy(_) => CodeSplittingReprSnapshot::Copy,
    };

    match snapshot {
        CodeSplittingReprSnapshot::Js {
            css_source_index,
            import_records,
            dependencies,
        } => {
            if css_source_index.is_valid() {
                mark_file_reachable_for_code_splitting(
                    graph,
                    options,
                    css_source_index.get_index(),
                    entry_point_bit,
                    distance_from_entry_point,
                );
            }
            for record in import_records {
                if record.source_index.is_valid()
                    && !is_external_dynamic_import(graph, options, &record, source_index)
                {
                    mark_file_reachable_for_code_splitting(
                        graph,
                        options,
                        record.source_index.get_index(),
                        entry_point_bit,
                        distance_from_entry_point,
                    );
                }
            }
            for dependency in dependencies {
                if dependency.source_index != source_index {
                    mark_file_reachable_for_code_splitting(
                        graph,
                        options,
                        dependency.source_index,
                        entry_point_bit,
                        distance_from_entry_point,
                    );
                }
            }
        }
        CodeSplittingReprSnapshot::Css(import_records) => {
            for record in import_records {
                if record.source_index.is_valid() {
                    mark_file_reachable_for_code_splitting(
                        graph,
                        options,
                        record.source_index.get_index(),
                        entry_point_bit,
                        distance_from_entry_point,
                    );
                }
            }
        }
        CodeSplittingReprSnapshot::Copy => {}
    }
}

pub fn tree_shaking_and_code_splitting(graph: &mut LinkerGraph, options: &Options) {
    let entry_points = graph.entry_points().to_vec();
    for entry_point in &entry_points {
        mark_file_live_for_tree_shaking(graph, options, entry_point.source_index);
    }
    for (entry_point_bit, entry_point) in entry_points.iter().enumerate() {
        mark_file_reachable_for_code_splitting(
            graph,
            options,
            entry_point.source_index,
            entry_point_bit,
            0,
        );
    }
}

#[must_use]
/// # Panics
///
/// Panics when a resolved internal JavaScript import points to a non-JavaScript
/// representation.
pub fn should_include_part(
    graph: &LinkerGraph,
    repr: &crate::internal::graph::JsRepr,
    part: &crate::internal::js_ast::Part,
) -> bool {
    if part.statements.len() == 1
        && let Some(crate::internal::js_ast::StmtData::Import(statement)) =
            part.statements[0].data.as_deref()
    {
        let record = &repr.ast.import_records[statement.import_record_index as usize];
        if record.source_index.is_valid() {
            let Some(InputFileRepr::Js(other)) = graph.files
                [record.source_index.get_index() as usize]
                .input_file
                .repr
                .as_ref()
            else {
                panic!("internal JavaScript import must target JavaScript");
            };
            if other.meta.wrap == WrapKind::None {
                return false;
            }
        }
    }
    true
}

/// Linearize files and live parts in dependency-first JavaScript order.
///
/// # Panics
///
/// Panics when chunk membership, imports, or parts violate linker graph
/// invariants.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn find_imported_parts_in_js_order(
    graph: &LinkerGraph,
    options: &Options,
    chunk: &ChunkInfo,
) -> (Vec<u32>, Vec<PartRange>) {
    #[allow(clippy::too_many_arguments)]
    fn visit(
        graph: &LinkerGraph,
        options: &Options,
        chunk: &ChunkInfo,
        source_index: u32,
        visited: &mut HashSet<u32>,
        files: &mut Vec<u32>,
        prefix: &mut Vec<PartRange>,
        parts: &mut Vec<PartRange>,
    ) {
        if !visited.insert(source_index) {
            return;
        }
        let file = &graph.files[source_index as usize];
        let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_ref() else {
            return;
        };
        let mut is_file_in_this_chunk = chunk.entry_bits == file.entry_bits;
        let can_file_be_split = repr.meta.wrap == WrapKind::None;

        if can_file_be_split
            && is_file_in_this_chunk
            && repr.ast.parts[crate::internal::js_ast::NS_EXPORT_PART_INDEX as usize].is_live
        {
            append_or_extend_part_range(
                parts,
                source_index,
                crate::internal::js_ast::NS_EXPORT_PART_INDEX,
            );
        }

        for (part_index, part) in repr.ast.parts.iter().enumerate() {
            let is_part_in_this_chunk = is_file_in_this_chunk && part.is_live;
            for &record_index in &part.import_record_indices {
                let record = &repr.ast.import_records[record_index as usize];
                if record.source_index.is_valid()
                    && (record.kind == ImportKind::Stmt || is_part_in_this_chunk)
                {
                    if is_external_dynamic_import(graph, options, record, source_index) {
                        continue;
                    }
                    visit(
                        graph,
                        options,
                        chunk,
                        record.source_index.get_index(),
                        visited,
                        files,
                        prefix,
                        parts,
                    );
                }
            }

            if is_part_in_this_chunk {
                is_file_in_this_chunk = true;
                let part_index = u32::try_from(part_index).expect("part index fits in u32");
                if can_file_be_split
                    && part_index != crate::internal::js_ast::NS_EXPORT_PART_INDEX
                    && should_include_part(graph, repr, part)
                {
                    if source_index == crate::internal::runtime::SOURCE_INDEX {
                        append_or_extend_part_range(prefix, source_index, part_index);
                    } else {
                        append_or_extend_part_range(parts, source_index, part_index);
                    }
                }
            }
        }

        if is_file_in_this_chunk {
            files.push(source_index);
            if !can_file_be_split {
                prefix.push(PartRange {
                    source_index,
                    part_index_begin: 0,
                    part_index_end: u32::try_from(repr.ast.parts.len())
                        .expect("part count fits in u32"),
                });
            }
        }
    }

    let mut sorted: Vec<_> = chunk
        .files_with_parts_in_chunk
        .iter()
        .map(|&source_index| {
            (
                graph.files[source_index as usize].distance_from_entry_point,
                graph.stable_source_indices[source_index as usize],
                source_index,
            )
        })
        .collect();
    sorted.sort_unstable();

    let mut visited = HashSet::new();
    let mut files = Vec::new();
    let mut prefix = Vec::new();
    let mut parts = Vec::new();
    if graph.files.len() > crate::internal::runtime::SOURCE_INDEX as usize {
        visit(
            graph,
            options,
            chunk,
            crate::internal::runtime::SOURCE_INDEX,
            &mut visited,
            &mut files,
            &mut prefix,
            &mut parts,
        );
    }
    for (_, _, source_index) in sorted {
        visit(
            graph,
            options,
            chunk,
            source_index,
            &mut visited,
            &mut files,
            &mut prefix,
            &mut parts,
        );
    }
    prefix.extend(parts);
    (files, prefix)
}

/// Group live JavaScript files into chunks by identical entry-point bitsets.
///
/// CSS companion chunks and path templates are added by later linker stages.
///
/// # Panics
///
/// Panics when entry points or live files violate linker graph invariants.
#[must_use]
pub fn compute_js_chunks(
    graph: &mut LinkerGraph,
    options: &Options,
    unique_key_prefix: &str,
) -> Vec<ChunkInfo> {
    let entry_points = graph.entry_points().to_vec();
    let mut chunks_by_key: HashMap<Vec<u8>, ChunkInfo> = HashMap::new();

    for (entry_point_bit, entry_point) in entry_points.iter().enumerate() {
        if !matches!(
            graph.files[entry_point.source_index as usize]
                .input_file
                .repr
                .as_ref(),
            Some(InputFileRepr::Js(_))
        ) {
            continue;
        }
        let mut entry_bits = BitSet::new(entry_points.len());
        entry_bits.set_bit(entry_point_bit);
        chunks_by_key.insert(
            entry_bits.as_bytes().to_vec(),
            ChunkInfo {
                entry_bits,
                is_entry_point: true,
                source_index: entry_point.source_index,
                entry_point_bit,
                ..ChunkInfo::default()
            },
        );
    }

    for source_index in graph.reachable_files.clone() {
        let file = &graph.files[source_index as usize];
        if file.is_live && matches!(file.input_file.repr.as_ref(), Some(InputFileRepr::Js(_))) {
            let key = file.entry_bits.as_bytes().to_vec();
            chunks_by_key
                .entry(key)
                .or_insert_with(|| ChunkInfo {
                    entry_bits: file.entry_bits.clone(),
                    ..ChunkInfo::default()
                })
                .files_with_parts_in_chunk
                .insert(source_index);
        }
    }

    let mut chunks: Vec<_> = chunks_by_key.into_iter().collect();
    chunks.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut chunks: Vec<_> = chunks.into_iter().map(|(_, chunk)| chunk).collect();

    for (chunk_index, chunk) in chunks.iter_mut().enumerate() {
        chunk.unique_key = format!("{unique_key_prefix}C{chunk_index:08}");
        if chunk.is_entry_point {
            graph.files[chunk.source_index as usize].entry_point_chunk_index =
                u32::try_from(chunk_index).expect("chunk index fits in u32");
        }
        (chunk.files_in_chunk_in_order, chunk.parts_in_chunk_in_order) =
            find_imported_parts_in_js_order(graph, options, chunk);
    }
    chunks
}

/// Discover symbol edges that cross JavaScript chunk boundaries and assign
/// deterministic export aliases.
///
/// # Panics
///
/// Panics when live parts, symbols, entry bits, or chunk ownership violate
/// linker graph invariants.
#[allow(clippy::too_many_lines)]
pub fn compute_cross_chunk_dependencies(
    graph: &mut LinkerGraph,
    chunks: &mut [ChunkInfo],
    options: &Options,
) {
    if !options.code_splitting {
        return;
    }

    let mut imports_by_chunk = vec![HashSet::<Ref>::new(); chunks.len()];
    let mut exports_by_chunk = vec![HashSet::<Ref>::new(); chunks.len()];
    let mut dynamic_imports_by_chunk = vec![HashSet::<u32>::new(); chunks.len()];

    for (chunk_index, chunk) in chunks.iter().enumerate() {
        for &source_index in &chunk.files_with_parts_in_chunk {
            let (wrap, wrapper_ref, imports_to_bind, import_records, parts) = {
                let Some(InputFileRepr::Js(repr)) =
                    graph.files[source_index as usize].input_file.repr.as_ref()
                else {
                    continue;
                };
                (
                    repr.meta.wrap,
                    repr.ast.wrapper_ref,
                    repr.meta.imports_to_bind.clone(),
                    repr.ast.import_records.clone(),
                    repr.ast.parts.clone(),
                )
            };
            for part in parts.iter().filter(|part| part.is_live) {
                for &import_record_index in &part.import_record_indices {
                    let record = &import_records[import_record_index as usize];
                    if record.kind == ImportKind::Dynamic && record.source_index.is_valid() {
                        let target_chunk = graph.files[record.source_index.get_index() as usize]
                            .entry_point_chunk_index;
                        if target_chunk
                            != u32::try_from(chunk_index).expect("chunk index fits in u32")
                        {
                            dynamic_imports_by_chunk[chunk_index].insert(target_chunk);
                        }
                    }
                }
                for declared in &part.declared_symbols {
                    if declared.is_top_level {
                        graph.symbols.get_mut(declared.reference).chunk_index = Index32::new(
                            u32::try_from(chunk_index).expect("chunk index fits in u32"),
                        );
                    }
                }
                for &reference in part.symbol_uses.keys() {
                    let symbol = graph.symbols.get(reference);
                    if symbol.kind == SymbolKind::Unbound
                        || symbol.import_item_status == ImportItemStatus::Missing
                    {
                        continue;
                    }
                    let mut target = reference;
                    if let Some(import) = imports_to_bind.get(&reference) {
                        target = import.reference;
                    } else if wrap == WrapKind::Cjs && reference != wrapper_ref {
                        continue;
                    }
                    if let Some(alias) = &graph.symbols.get(target).namespace_alias {
                        target = alias.namespace_ref;
                    }
                    imports_by_chunk[chunk_index].insert(target);
                }
            }
        }

        if chunk.is_entry_point {
            let Some(InputFileRepr::Js(repr)) = graph.files[chunk.source_index as usize]
                .input_file
                .repr
                .as_ref()
            else {
                continue;
            };
            if repr.meta.wrap != WrapKind::Cjs {
                for alias in &repr.meta.sorted_and_filtered_export_aliases {
                    let export = &repr.meta.resolved_exports[alias];
                    let mut target = export.reference;
                    let Some(InputFileRepr::Js(target_repr)) = graph.files
                        [export.source_index as usize]
                        .input_file
                        .repr
                        .as_ref()
                    else {
                        panic!("entry export target must be JavaScript");
                    };
                    if let Some(import) = target_repr.meta.imports_to_bind.get(&target) {
                        target = import.reference;
                    }
                    if let Some(namespace) = &graph.symbols.get(target).namespace_alias {
                        target = namespace.namespace_ref;
                    }
                    imports_by_chunk[chunk_index].insert(target);
                }
            }
            if repr.meta.force_include_exports_for_entry_point {
                imports_by_chunk[chunk_index].insert(repr.ast.exports_ref);
            }
            if repr.meta.wrap != WrapKind::None {
                imports_by_chunk[chunk_index].insert(repr.ast.wrapper_ref);
            }
        }
    }

    for chunk_index in 0..chunks.len() {
        let chunk_index_u32 = u32::try_from(chunk_index).expect("chunk index fits in u32");
        let mut imports_from_other_chunks = HashMap::new();
        for &reference in &imports_by_chunk[chunk_index] {
            let owner = graph.symbols.get(reference).chunk_index;
            if owner.is_valid() {
                let owner = owner.get_index();
                if owner != chunk_index_u32 {
                    imports_from_other_chunks
                        .entry(owner)
                        .or_insert_with(Vec::new)
                        .push(CrossChunkImportItem {
                            reference,
                            ..CrossChunkImportItem::default()
                        });
                    exports_by_chunk[owner as usize].insert(reference);
                }
            }
        }

        if chunks[chunk_index].is_entry_point {
            for (other_chunk_index, other_chunk) in chunks.iter().enumerate() {
                if chunk_index != other_chunk_index
                    && other_chunk
                        .entry_bits
                        .has_bit(chunks[chunk_index].entry_point_bit)
                {
                    imports_from_other_chunks
                        .entry(u32::try_from(other_chunk_index).expect("chunk index fits in u32"))
                        .or_insert_with(Vec::new);
                }
            }
        }
        chunks[chunk_index].imports_from_other_chunks = imports_from_other_chunks;
    }

    for (chunk_index, chunk) in chunks.iter_mut().enumerate() {
        let mut renamer = crate::internal::renamer::ExportRenamer::default();
        for export in sorted_cross_chunk_export_items(
            &exports_by_chunk[chunk_index],
            &graph.stable_source_indices,
        ) {
            let alias = if options.minify_identifiers {
                renamer.next_minified_name()
            } else {
                renamer.next_renamed_name(&graph.symbols.get(export.reference).original_name)
            };
            chunk
                .exports_to_other_chunks
                .insert(export.reference, alias);
        }
    }

    let exports_to_other_chunks: Vec<_> = chunks
        .iter()
        .map(|chunk| chunk.exports_to_other_chunks.clone())
        .collect();
    for (chunk_index, chunk) in chunks.iter_mut().enumerate() {
        let mut dynamic_imports: Vec<_> = dynamic_imports_by_chunk[chunk_index]
            .iter()
            .copied()
            .collect();
        dynamic_imports.sort_unstable();
        chunk
            .cross_chunk_imports
            .extend(dynamic_imports.into_iter().map(|chunk_index| ChunkImport {
                chunk_index,
                import_kind: ImportKind::Dynamic,
            }));
        let imports = std::mem::take(&mut chunk.imports_from_other_chunks);
        chunk.sorted_cross_chunk_imports =
            sorted_cross_chunk_imports(imports, &exports_to_other_chunks);
        chunk
            .cross_chunk_imports
            .extend(
                chunk
                    .sorted_cross_chunk_imports
                    .iter()
                    .map(|import| ChunkImport {
                        chunk_index: import.chunk_index,
                        import_kind: ImportKind::Stmt,
                    }),
            );
    }
}

/// Convert cross-chunk symbol tables into generated ESM import and export
/// statements for the JavaScript printer.
///
/// # Panics
///
/// Panics when code splitting is used with a non-ESM output format or when the
/// cross-chunk import table and import-record table are inconsistent.
pub fn generate_cross_chunk_stmts(
    graph: &LinkerGraph,
    chunks: &mut [ChunkInfo],
    options: &Options,
) {
    assert_eq!(
        options.output_format,
        Format::EsModule,
        "Internal error: code splitting requires ESM output"
    );

    for chunk in chunks {
        let exports: HashSet<_> = chunk.exports_to_other_chunks.keys().copied().collect();
        let export_items = sorted_cross_chunk_export_items(&exports, &graph.stable_source_indices)
            .into_iter()
            .map(|export| js_ast::ClauseItem {
                alias: chunk.exports_to_other_chunks[&export.reference].clone(),
                name: LocRef {
                    reference: export.reference,
                    ..LocRef::default()
                },
                ..js_ast::ClauseItem::default()
            })
            .collect::<Vec<_>>();
        chunk.cross_chunk_suffix_stmts = if export_items.is_empty() {
            Vec::new()
        } else {
            vec![js_ast::Stmt::new(
                crate::internal::logger::Loc::default(),
                js_ast::StmtData::ExportClause(js_ast::ExportClauseStmt {
                    items: export_items,
                    ..js_ast::ExportClauseStmt::default()
                }),
            )]
        };

        let import_record_start = chunk
            .cross_chunk_imports
            .len()
            .checked_sub(chunk.sorted_cross_chunk_imports.len())
            .expect("cross-chunk imports must have corresponding import records");
        chunk.cross_chunk_prefix_stmts = chunk
            .sorted_cross_chunk_imports
            .iter()
            .enumerate()
            .map(|(import_index, import)| {
                let items = import
                    .sorted_import_items
                    .iter()
                    .map(|item| js_ast::ClauseItem {
                        alias: item.export_alias.clone(),
                        name: LocRef {
                            reference: item.reference,
                            ..LocRef::default()
                        },
                        ..js_ast::ClauseItem::default()
                    })
                    .collect::<Vec<_>>();
                js_ast::Stmt::new(
                    crate::internal::logger::Loc::default(),
                    js_ast::StmtData::Import(js_ast::ImportStmt {
                        items: (!items.is_empty()).then_some(items),
                        import_record_index: u32::try_from(import_record_start + import_index)
                            .expect("import record index fits in u32"),
                        ..js_ast::ImportStmt::default()
                    }),
                )
            })
            .collect();
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrintedCrossChunkBindings {
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
}

/// Print the generated cross-chunk import/export statements using temporary
/// chunk keys. Final paths are substituted after all chunk hashes are known.
///
/// # Panics
///
/// Panics when a cross-chunk import references an invalid chunk index or the
/// generated AST violates JavaScript-printer invariants.
#[must_use]
pub fn print_cross_chunk_bindings(
    chunks: &[ChunkInfo],
    chunk_index: usize,
    renamer: &dyn crate::internal::renamer::Renamer,
    options: &Options,
) -> PrintedCrossChunkBindings {
    let chunk = &chunks[chunk_index];
    let import_records = chunk
        .cross_chunk_imports
        .iter()
        .map(|chunk_import| crate::internal::ast::ImportRecord {
            kind: chunk_import.import_kind,
            path: crate::internal::logger::Path {
                text: chunks[chunk_import.chunk_index as usize].unique_key.clone(),
                ..crate::internal::logger::Path::default()
            },
            flags: ImportRecordFlags::SHOULD_NOT_BE_EXTERNAL_IN_METAFILE
                | ImportRecordFlags::CONTAINS_UNIQUE_KEY,
            ..crate::internal::ast::ImportRecord::default()
        })
        .collect();
    let print_options = crate::internal::js_printer::Options {
        unsupported_features: options.unsupported_js_features,
        line_limit: options.line_limit,
        minify_syntax: options.minify_syntax,
        minify_whitespace: options.minify_whitespace,
        ascii_only: options.ascii_only,
    };
    let prefix = crate::internal::js_printer::print(
        &js_ast::Ast {
            import_records,
            parts: vec![js_ast::Part {
                statements: chunk.cross_chunk_prefix_stmts.clone(),
                ..js_ast::Part::default()
            }],
            ..js_ast::Ast::default()
        },
        renamer,
        print_options,
    )
    .js;
    let suffix = crate::internal::js_printer::print(
        &js_ast::Ast {
            parts: vec![js_ast::Part {
                statements: chunk.cross_chunk_suffix_stmts.clone(),
                ..js_ast::Part::default()
            }],
            ..js_ast::Ast::default()
        },
        renamer,
        print_options,
    )
    .js;
    PrintedCrossChunkBindings { prefix, suffix }
}

/// Merge adjacent variable declarations of the same kind and export status.
#[must_use]
pub fn merge_adjacent_local_stmts(statements: Vec<js_ast::Stmt>) -> Vec<js_ast::Stmt> {
    let mut result: Vec<js_ast::Stmt> = Vec::with_capacity(statements.len());
    for statement in statements {
        let can_merge = matches!(
            (
                result.last().and_then(|statement| statement.data.as_deref()),
                statement.data.as_deref(),
            ),
            (
                Some(js_ast::StmtData::Local(before)),
                Some(js_ast::StmtData::Local(after)),
            ) if before.kind == after.kind && before.is_export == after.is_export
        );
        if can_merge {
            let Some(js_ast::StmtData::Local(before)) = result
                .last_mut()
                .and_then(|statement| statement.data.as_deref_mut())
            else {
                unreachable!("merge predicate checked the previous statement");
            };
            let Some(js_ast::StmtData::Local(after)) = statement.data.as_deref() else {
                unreachable!("merge predicate checked the next statement");
            };
            before.declarations.extend(after.declarations.clone());
        } else {
            result.push(statement);
        }
    }
    result
}

/// Remove ESM export syntax from statements that are being absorbed into a
/// bundle while preserving the underlying declarations.
///
/// # Panics
///
/// Panics when a default export contains a statement kind other than an
/// expression, function, or class.
#[must_use]
pub fn strip_exports_from_stmts(statements: &[js_ast::Stmt]) -> Vec<js_ast::Stmt> {
    let mut result = Vec::with_capacity(statements.len());
    for original in statements {
        let mut statement = original.clone();
        match statement.data.as_deref_mut() {
            Some(js_ast::StmtData::ExportClause(_)) => continue,
            Some(js_ast::StmtData::Function(function)) => function.is_export = false,
            Some(js_ast::StmtData::Class(class)) => class.is_export = false,
            Some(js_ast::StmtData::Local(local)) => local.is_export = false,
            Some(js_ast::StmtData::ExportDefault(export)) => {
                let default_name = export.default_name;
                let value = export.value.clone();
                statement = match value.data.as_deref() {
                    Some(js_ast::StmtData::Expr(expression)) => js_ast::Stmt::new(
                        original.loc,
                        js_ast::StmtData::Local(js_ast::LocalStmt {
                            declarations: vec![js_ast::Decl {
                                binding: js_ast::Binding {
                                    data: Some(Box::new(js_ast::BindingData::Identifier(
                                        js_ast::IdentifierBinding {
                                            reference: default_name.reference,
                                        },
                                    ))),
                                    loc: default_name.loc,
                                },
                                value_or_nil: expression.value.clone(),
                            }],
                            ..js_ast::LocalStmt::default()
                        }),
                    ),
                    Some(js_ast::StmtData::Function(function)) => {
                        let mut function = function.clone();
                        function.function.name = Some(default_name);
                        js_ast::Stmt::new(value.loc, js_ast::StmtData::Function(function))
                    }
                    Some(js_ast::StmtData::Class(class)) => {
                        let mut class = class.clone();
                        class.class.name = Some(default_name);
                        js_ast::Stmt::new(value.loc, js_ast::StmtData::Class(class))
                    }
                    _ => panic!("Internal error: invalid default export"),
                };
            }
            _ => {}
        }
        result.push(statement);
    }
    result
}

#[derive(Clone, Debug, Default)]
pub struct ImportConversion {
    pub keep_original: bool,
    pub prefix_statement: Option<js_ast::Stmt>,
}

/// Convert one import/export-from statement for its target module's wrapper
/// state.
///
/// # Panics
///
/// Panics when the source or target file is not JavaScript or an import record
/// index is invalid.
#[must_use]
pub fn convert_import_for_chunk(
    graph: &LinkerGraph,
    source_index: u32,
    statement_loc: crate::internal::logger::Loc,
    namespace_ref: Ref,
    import_record_index: u32,
    output_format: Format,
) -> ImportConversion {
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_ref()
    else {
        panic!("import source must be JavaScript");
    };
    let record = &repr.ast.import_records[import_record_index as usize];
    if !record.source_index.is_valid() {
        if output_format.keep_esm_import_export_syntax() {
            return ImportConversion {
                keep_original: true,
                ..ImportConversion::default()
            };
        }
        return ImportConversion {
            prefix_statement: Some(require_namespace_statement(
                statement_loc,
                record.range.loc,
                namespace_ref,
                import_record_index,
            )),
            ..ImportConversion::default()
        };
    }

    if repr.ast.exports_kind == ExportsKind::CommonJs
        && graph.symbols.follow_symbols_const(namespace_ref) == repr.ast.exports_ref
    {
        return ImportConversion::default();
    }

    let target_file = &graph.files[record.source_index.get_index() as usize];
    let Some(InputFileRepr::Js(target)) = target_file.input_file.repr.as_ref() else {
        panic!("import target must be JavaScript");
    };
    match target.meta.wrap {
        WrapKind::None => ImportConversion::default(),
        WrapKind::Cjs => ImportConversion {
            prefix_statement: Some(require_namespace_statement(
                statement_loc,
                record.range.loc,
                namespace_ref,
                import_record_index,
            )),
            ..ImportConversion::default()
        },
        WrapKind::Esm if !target_file.is_live => ImportConversion::default(),
        WrapKind::Esm => {
            let call = js_ast::Expr::new(
                statement_loc,
                js_ast::ExprData::Call(js_ast::CallExpr {
                    target: js_ast::Expr::new(
                        statement_loc,
                        js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                            reference: target.ast.wrapper_ref,
                            ..js_ast::IdentifierExpr::default()
                        }),
                    ),
                    ..js_ast::CallExpr::default()
                }),
            );
            let value = if target.meta.is_async_or_has_async_dependency {
                js_ast::Expr::new(
                    statement_loc,
                    js_ast::ExprData::Await(js_ast::AwaitExpr { value: call }),
                )
            } else {
                call
            };
            ImportConversion {
                prefix_statement: Some(js_ast::Stmt::new(
                    statement_loc,
                    js_ast::StmtData::Expr(js_ast::ExprStmt {
                        value,
                        ..js_ast::ExprStmt::default()
                    }),
                )),
                ..ImportConversion::default()
            }
        }
    }
}

fn require_namespace_statement(
    statement_loc: crate::internal::logger::Loc,
    value_loc: crate::internal::logger::Loc,
    namespace_ref: Ref,
    import_record_index: u32,
) -> js_ast::Stmt {
    js_ast::Stmt::new(
        statement_loc,
        js_ast::StmtData::Local(js_ast::LocalStmt {
            declarations: vec![js_ast::Decl {
                binding: js_ast::Binding {
                    data: Some(Box::new(js_ast::BindingData::Identifier(
                        js_ast::IdentifierBinding {
                            reference: namespace_ref,
                        },
                    ))),
                    loc: statement_loc,
                },
                value_or_nil: js_ast::Expr::new(
                    value_loc,
                    js_ast::ExprData::RequireString(js_ast::RequireStringExpr {
                        import_record_index,
                        ..js_ast::RequireStringExpr::default()
                    }),
                ),
            }],
            ..js_ast::LocalStmt::default()
        }),
    )
}

#[derive(Clone, Debug, Default)]
pub struct ConvertedStmts {
    pub inside_wrapper_prefix: Vec<js_ast::Stmt>,
    pub inside_wrapper_suffix: Vec<js_ast::Stmt>,
    pub outside_wrapper_prefix: Vec<js_ast::Stmt>,
}

impl ConvertedStmts {
    fn push_esm_statement(&mut self, statement: js_ast::Stmt, extract_from_wrapper: bool) {
        if extract_from_wrapper {
            self.outside_wrapper_prefix.push(statement);
        } else {
            self.inside_wrapper_suffix.push(statement);
        }
    }
}

/// Convert a list of JavaScript statements for inclusion in a bundle chunk.
///
/// This ports the non-runtime-re-export cases from upstream's
/// `convertStmtsForChunk`, including import hoisting, wrapper boundaries, and
/// export syntax lowering.
///
/// # Panics
///
/// Panics when the source is not JavaScript, an import record is invalid, or
/// an `export *` statement requires the runtime re-export helper. Runtime
/// re-export generation is handled by the subsequent linker phase.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn convert_stmts_for_chunk(
    graph: &LinkerGraph,
    options: &Options,
    source_index: u32,
    part_statements: &[js_ast::Stmt],
) -> ConvertedStmts {
    let file = &graph.files[source_index as usize];
    let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_ref() else {
        panic!("statement source must be JavaScript");
    };
    let should_strip_exports = options.mode != Mode::PassThrough || !file.is_entry_point();
    let extract_esm_from_wrapper = repr.meta.wrap != WrapKind::None;
    let mut result = ConvertedStmts::default();

    for original in part_statements {
        let mut statement = original.clone();
        match statement.data.as_deref_mut() {
            Some(js_ast::StmtData::Import(import)) => {
                let conversion = convert_import_for_chunk(
                    graph,
                    source_index,
                    statement.loc,
                    import.namespace_ref,
                    import.import_record_index,
                    options.output_format,
                );
                if let Some(prefix) = conversion.prefix_statement {
                    result.inside_wrapper_prefix.push(prefix);
                }
                if !conversion.keep_original {
                    continue;
                }
                result.push_esm_statement(statement, extract_esm_from_wrapper);
                continue;
            }
            Some(js_ast::StmtData::ExportFrom(export)) => {
                let conversion = convert_import_for_chunk(
                    graph,
                    source_index,
                    statement.loc,
                    export.namespace_ref,
                    export.import_record_index,
                    options.output_format,
                );
                if let Some(prefix) = conversion.prefix_statement {
                    result.inside_wrapper_prefix.push(prefix);
                }
                if !conversion.keep_original {
                    continue;
                }
                if should_strip_exports {
                    for item in &mut export.items {
                        item.alias.clone_from(&item.original_name);
                    }
                    statement.data = Some(Box::new(js_ast::StmtData::Import(js_ast::ImportStmt {
                        namespace_ref: export.namespace_ref,
                        items: Some(export.items.clone()),
                        import_record_index: export.import_record_index,
                        is_single_line: export.is_single_line,
                        ..js_ast::ImportStmt::default()
                    })));
                }
                result.push_esm_statement(statement, extract_esm_from_wrapper);
                continue;
            }
            Some(js_ast::StmtData::ExportStar(export)) if export.alias.is_some() => {
                let conversion = convert_import_for_chunk(
                    graph,
                    source_index,
                    statement.loc,
                    export.namespace_ref,
                    export.import_record_index,
                    options.output_format,
                );
                if let Some(prefix) = conversion.prefix_statement {
                    result.inside_wrapper_prefix.push(prefix);
                }
                if !conversion.keep_original {
                    continue;
                }
                if should_strip_exports {
                    statement.data = Some(Box::new(js_ast::StmtData::Import(js_ast::ImportStmt {
                        namespace_ref: export.namespace_ref,
                        star_name_loc: export.alias.as_ref().map(|alias| alias.loc),
                        import_record_index: export.import_record_index,
                        ..js_ast::ImportStmt::default()
                    })));
                }
                result.push_esm_statement(statement, extract_esm_from_wrapper);
                continue;
            }
            Some(js_ast::StmtData::ExportStar(export)) if should_strip_exports => {
                let record = &repr.ast.import_records[export.import_record_index as usize];
                if record
                    .flags
                    .contains(ImportRecordFlags::CALLS_RUN_TIME_RE_EXPORT_FN)
                {
                    panic!("runtime export-star conversion must run in the runtime binding phase");
                }
                if !record.source_index.is_valid()
                    && options.output_format.keep_esm_import_export_syntax()
                {
                    result.push_esm_statement(statement, extract_esm_from_wrapper);
                } else if record.source_index.is_valid() {
                    let conversion = convert_import_for_chunk(
                        graph,
                        source_index,
                        original.loc,
                        export.namespace_ref,
                        export.import_record_index,
                        options.output_format,
                    );
                    if let Some(prefix) = conversion.prefix_statement {
                        result.inside_wrapper_prefix.push(prefix);
                    }
                }
                continue;
            }
            Some(js_ast::StmtData::ExportClause(_)) if should_strip_exports => continue,
            Some(js_ast::StmtData::ExportClause(_)) => {
                result.push_esm_statement(statement, extract_esm_from_wrapper);
                continue;
            }
            Some(
                js_ast::StmtData::Function(_)
                | js_ast::StmtData::Class(_)
                | js_ast::StmtData::Local(_)
                | js_ast::StmtData::ExportDefault(_),
            ) if should_strip_exports => {
                result
                    .inside_wrapper_suffix
                    .extend(strip_exports_from_stmts(std::slice::from_ref(&statement)));
                continue;
            }
            _ => {}
        }
        result.inside_wrapper_suffix.push(statement);
    }
    result
}

/// Assign the output path template for every JavaScript chunk, leaving the
/// content-hash placeholder unresolved until final hashing.
///
/// # Panics
///
/// Panics when an entry-point chunk does not have a corresponding graph entry
/// point or an explicit output file extension cannot be removed from its base.
pub fn assign_chunk_path_templates(
    file_system: &dyn Fs,
    graph: &LinkerGraph,
    chunks: &mut [ChunkInfo],
    options: &Options,
) {
    for chunk in chunks {
        let standard_extension = options.output_extension_js.clone();
        let (directory, base, extension, mut template) = if chunk.is_entry_point {
            let file = &graph.files[chunk.source_index as usize];
            let template = if file.is_user_specified_entry_point() {
                options.entry_path_template.clone()
            } else {
                options.chunk_path_template.clone()
            };

            if options.abs_output_file.is_empty() {
                let (directory, base) = path_relative_to_outbase(
                    &file.input_file,
                    options,
                    file_system,
                    !file.is_user_specified_entry_point(),
                    &graph.entry_points()[chunk.entry_point_bit].output_path,
                );
                (directory, base, standard_extension, template)
            } else {
                let mut base = file_system.base(&options.abs_output_file);
                let original_extension = file_system.ext(&base);
                base.truncate(
                    base.len()
                        .checked_sub(original_extension.len())
                        .expect("output extension must be a suffix of the base name"),
                );
                let extension =
                    if matches!(file.input_file.repr.as_ref(), Some(InputFileRepr::Css(_)))
                        || standard_extension != options.output_extension_css
                    {
                        original_extension
                    } else {
                        standard_extension
                    };
                ("/".into(), base, extension, template)
            }
        } else {
            (
                "/".into(),
                "chunk".into(),
                standard_extension,
                options.chunk_path_template.clone(),
            )
        };

        let extension_without_dot = extension.strip_prefix('.').unwrap_or(&extension).to_owned();
        template.push(PathTemplate {
            data: extension,
            ..PathTemplate::default()
        });
        chunk.final_template = substitute_template(
            &template,
            &PathPlaceholders {
                dir: Some(directory),
                name: Some(base),
                ext: Some(extension_without_dot),
                ..PathPlaceholders::default()
            },
        );
    }
}

fn hash_write_u32(hash: &mut xxhash::Digest, value: u32) {
    hash.write(&value.to_le_bytes());
}

fn hash_write_length_prefixed(hash: &mut xxhash::Digest, bytes: &[u8]) {
    hash_write_u32(
        hash,
        u32::try_from(bytes.len()).expect("hash input must fit in 32 bits"),
    );
    hash.write(bytes);
}

/// Generate the content hash for this chunk without incorporating the hashes
/// of chunks imported by it.
///
/// # Panics
///
/// Panics when generated data or a source path is too large for esbuild's
/// 32-bit length encoding.
pub fn generate_isolated_hash(graph: &LinkerGraph, chunk: &mut ChunkInfo, options: &Options) {
    let mut hash = xxhash::Digest::new();

    for part_range in &chunk.parts_in_chunk_in_order {
        let source = &graph.files[part_range.source_index as usize]
            .input_file
            .source;
        let file_path = if source.key_path.namespace == "file" {
            &source.pretty_paths.rel
        } else {
            &source.key_path.text
        };
        hash_write_length_prefixed(&mut hash, source.key_path.namespace.as_bytes());
        hash_write_length_prefixed(&mut hash, file_path.as_bytes());
        hash_write_u32(&mut hash, part_range.part_index_begin);
        hash_write_u32(&mut hash, part_range.part_index_end);
    }

    for part in &chunk.final_template {
        hash_write_length_prefixed(&mut hash, part.data.as_bytes());
    }

    if !options.public_path.is_empty() {
        hash_write_length_prefixed(&mut hash, options.public_path.as_bytes());
    }

    if let Some(pieces) = &chunk.intermediate_output.pieces {
        for piece in pieces {
            hash_write_length_prefixed(&mut hash, &piece.data);
        }
    } else {
        let output = std::mem::take(&mut chunk.intermediate_output.joiner).done();
        hash_write_length_prefixed(&mut hash, &output);
        chunk.intermediate_output.joiner.add_bytes(output);
    }

    hash_write_length_prefixed(&mut hash, &chunk.output_source_map.prefix);
    hash_write_length_prefixed(&mut hash, &chunk.output_source_map.mappings);
    hash_write_length_prefixed(&mut hash, &chunk.output_source_map.suffix);
    chunk.isolated_hash = hash.sum(&[]);
}

struct FinalHashTraversal<'a> {
    file_system: &'a dyn Fs,
    graph: &'a LinkerGraph,
    chunks: &'a [ChunkInfo],
    options: &'a Options,
    visited: &'a mut [u32],
    visited_key: u32,
}

impl FinalHashTraversal<'_> {
    fn append(&mut self, hash: &mut xxhash::Digest, chunk_index: u32) {
        if self.visited[chunk_index as usize] == self.visited_key {
            return;
        }
        self.visited[chunk_index as usize] = self.visited_key;
        let chunk = &self.chunks[chunk_index as usize];

        for chunk_import in &chunk.cross_chunk_imports {
            self.append(hash, chunk_import.chunk_index);
        }

        if let Some(pieces) = &chunk.intermediate_output.pieces {
            for piece in pieces {
                if piece.kind != OutputPieceIndexKind::AssetIndex {
                    continue;
                }
                let file = &self.graph.files[piece.index as usize].input_file;
                assert_eq!(
                    file.additional_files.len(),
                    1,
                    "Internal error: asset marker must reference one output file"
                );
                let relative_path = self
                    .file_system
                    .rel(
                        &self.options.abs_output_dir,
                        &file.additional_files[0].abs_path,
                    )
                    .unwrap_or_default()
                    .replace('\\', "/");
                hash_write_length_prefixed(hash, relative_path.as_bytes());
            }
        }

        hash.write(&chunk.isolated_hash);
    }
}

/// Compute dependency-aware hashes and substitute the final `[hash]`
/// placeholder in each chunk path.
///
/// # Panics
///
/// Panics when a chunk or asset index is invalid, an asset marker does not
/// reference exactly one output file, or a generated path exceeds 32 bits.
pub fn finalize_chunk_paths(
    file_system: &dyn Fs,
    graph: &LinkerGraph,
    chunks: &mut [ChunkInfo],
    options: &Options,
) {
    let mut visited = vec![0_u32; chunks.len()];
    for chunk_index in 0..chunks.len() {
        let hash = if has_placeholder(&chunks[chunk_index].final_template, PathPlaceholder::Hash) {
            let mut digest = xxhash::Digest::new();
            FinalHashTraversal {
                file_system,
                graph,
                chunks,
                options,
                visited: &mut visited,
                visited_key: !u32::try_from(chunk_index).expect("chunk index fits in u32"),
            }
            .append(
                &mut digest,
                u32::try_from(chunk_index).expect("chunk index fits in u32"),
            );
            Some(hash_for_file_name(&digest.sum(&[])))
        } else {
            None
        };
        chunks[chunk_index].final_rel_path = template_to_string(&substitute_template(
            &chunks[chunk_index].final_template,
            &PathPlaceholders {
                hash,
                ..PathPlaceholders::default()
            },
        ));
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
        AmbiguousReExport, AssetPath, ChunkImport, ChunkInfo, ChunkPath, CrossChunkImport,
        CrossChunkImportItem, ImportStatus, ImportTracker, MatchImportKind, OutputPathContext,
        OutputPiece, OutputPieceIndexKind, PartRange, StableRef, add_exports_for_export_star,
        advance_import_tracker, append_or_extend_part_range, assign_chunk_path_templates,
        bind_imports_to_exports_for_file, classify_module_wrappers,
        compute_cross_chunk_dependencies, compute_js_chunks, convert_import_for_chunk,
        convert_stmts_for_chunk, create_wrapper_for_file, enforce_no_cyclic_chunk_imports,
        finalize_chunk_paths, generate_cross_chunk_stmts, generate_isolated_hash,
        has_dynamic_exports_due_to_export_star, import_conditions_are_equal, inline_linked_assets,
        is_conditional_import_redundant, join_with_public_path, mark_file_live_for_tree_shaking,
        match_import_with_export, merge_adjacent_local_stmts, path_between_chunks,
        print_cross_chunk_bindings, propagate_wrappers_and_dynamic_exports,
        recursively_wrap_dependencies, resolve_export_stars, sort_and_filter_export_aliases,
        sorted_cross_chunk_export_items, sorted_cross_chunk_imports, strip_exports_from_stmts,
        tree_shaking_and_code_splitting,
    };
    use crate::internal::{
        ast::{ImportKind, ImportRecord, ImportRecordFlags, Index32, Ref, Symbol, SymbolKind},
        config::{
            Format, Loader, Mode, Options, PathPlaceholder, PathTemplate, template_to_string,
        },
        css_ast::{
            ImportConditions, MediaArbitraryTokensQuery, MediaQuery, MediaQueryData, Token,
            WhitespaceFlags,
        },
        css_lexer::TokenKind,
        fs::{MockKind, mock_fs},
        graph::{
            CopyRepr, CssRepr, EntryPoint, InputFile, InputFileRepr, JsRepr, OutputFile,
            SideEffects, SideEffectsKind, WrapKind, clone_linker_graph,
        },
        helpers::Joiner,
        js_ast::{self, ExportsKind, NamedExport, NamedImport},
        logger::{DeferLogKind, Loc, Log, Path, PrettyPaths, Range, Source},
        sourcemap::{LineColumnOffset, SourceMapPieces, SourceMapShift},
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
    fn isolated_hash_includes_identity_and_excludes_temporary_paths() {
        let mut input = js_file(js_ast::Ast::default());
        input.source = Source {
            pretty_paths: PrettyPaths {
                rel: "src/input.js".into(),
                ..PrettyPaths::default()
            },
            key_path: Path {
                text: "/machine-specific/input.js".into(),
                namespace: "file".into(),
                ..Path::default()
            },
            ..Source::default()
        };
        let graph = clone_linker_graph(&[input], &[0], &[], false);
        let chunks = [ChunkPath::default(), ChunkPath::default()];
        let options = Options {
            public_path: "/assets/".into(),
            ..Options::default()
        };
        let make_chunk = |marker: &[u8]| {
            let mut output = b"before".to_vec();
            output.extend_from_slice(marker);
            output.extend_from_slice(b"after");
            ChunkInfo {
                parts_in_chunk_in_order: vec![PartRange {
                    source_index: 0,
                    part_index_begin: 2,
                    part_index_end: 5,
                }],
                final_template: vec![PathTemplate {
                    data: "chunks/".into(),
                    placeholder: PathPlaceholder::Hash,
                }],
                intermediate_output: context(&[], &chunks).break_output_into_pieces(output),
                output_source_map: SourceMapPieces {
                    prefix: b"{\"mappings\":\"".to_vec(),
                    mappings: b"AAAA".to_vec(),
                    suffix: b"\"}".to_vec(),
                },
                ..ChunkInfo::default()
            }
        };

        let mut first = make_chunk(b"UNIQUEC00000000");
        let mut second = make_chunk(b"UNIQUEC00000001");
        generate_isolated_hash(&graph, &mut first, &options);
        generate_isolated_hash(&graph, &mut second, &options);
        assert_eq!(first.isolated_hash, second.isolated_hash);
        assert_eq!(first.isolated_hash.len(), 8);

        second.output_source_map.mappings.push(b';');
        generate_isolated_hash(&graph, &mut second, &options);
        assert_ne!(first.isolated_hash, second.isolated_hash);

        let mut second = make_chunk(b"UNIQUEC00000001");
        generate_isolated_hash(&graph, &mut second, &Options::default());
        assert_ne!(first.isolated_hash, second.isolated_hash);
    }

    #[test]
    fn isolated_hash_length_prefixes_generated_spans() {
        let graph = clone_linker_graph(&[], &[], &[], false);
        let chunks = [ChunkPath::default()];
        let mut left = ChunkInfo {
            intermediate_output: context(&[], &chunks)
                .break_output_into_pieces(b"aUNIQUEC00000000bc".to_vec()),
            ..ChunkInfo::default()
        };
        let mut right = ChunkInfo {
            intermediate_output: context(&[], &chunks)
                .break_output_into_pieces(b"abUNIQUEC00000000c".to_vec()),
            ..ChunkInfo::default()
        };
        generate_isolated_hash(&graph, &mut left, &Options::default());
        generate_isolated_hash(&graph, &mut right, &Options::default());
        assert_ne!(left.isolated_hash, right.isolated_hash);
    }

    #[test]
    fn isolated_hash_preserves_unsplit_joiner_output() {
        let graph = clone_linker_graph(&[], &[], &[], false);
        let mut joiner = Joiner::default();
        joiner.add_string("console.log(1)");
        let mut chunk = ChunkInfo {
            intermediate_output: super::IntermediateOutput::without_substitutions(joiner),
            ..ChunkInfo::default()
        };
        generate_isolated_hash(&graph, &mut chunk, &Options::default());
        let first_hash = chunk.isolated_hash.clone();
        generate_isolated_hash(&graph, &mut chunk, &Options::default());
        assert_eq!(chunk.isolated_hash, first_hash);
        let (joiner, _) =
            context(&[], &[]).substitute_final_paths(chunk.intermediate_output, str::to_owned);
        assert_eq!(joiner.done(), b"console.log(1)");
    }

    #[test]
    fn final_chunk_hash_includes_transitive_dependencies() {
        let graph = clone_linker_graph(&[], &[], &[], false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let make_chunks = |dependency_hash: &[u8]| {
            vec![
                ChunkInfo {
                    cross_chunk_imports: vec![ChunkImport {
                        chunk_index: 1,
                        import_kind: ImportKind::Stmt,
                    }],
                    final_template: vec![
                        PathTemplate {
                            data: "entry-".into(),
                            placeholder: PathPlaceholder::Hash,
                        },
                        PathTemplate {
                            data: ".js".into(),
                            ..PathTemplate::default()
                        },
                    ],
                    isolated_hash: b"entry".to_vec(),
                    ..ChunkInfo::default()
                },
                ChunkInfo {
                    final_template: vec![
                        PathTemplate {
                            data: "dependency-".into(),
                            placeholder: PathPlaceholder::Hash,
                        },
                        PathTemplate {
                            data: ".js".into(),
                            ..PathTemplate::default()
                        },
                    ],
                    isolated_hash: dependency_hash.to_vec(),
                    ..ChunkInfo::default()
                },
            ]
        };

        let mut first = make_chunks(b"dependency-a");
        finalize_chunk_paths(&file_system, &graph, &mut first, &Options::default());
        assert_eq!(first[0].final_rel_path.len(), "entry-.js".len() + 8);
        assert_eq!(first[1].final_rel_path.len(), "dependency-.js".len() + 8);

        let mut second = make_chunks(b"dependency-b");
        finalize_chunk_paths(&file_system, &graph, &mut second, &Options::default());
        assert_ne!(first[0].final_rel_path, second[0].final_rel_path);
        assert_ne!(first[1].final_rel_path, second[1].final_rel_path);
    }

    #[test]
    fn final_chunk_hash_handles_dynamic_import_cycles() {
        let graph = clone_linker_graph(&[], &[], &[], false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let template = vec![PathTemplate {
            data: "chunk-".into(),
            placeholder: PathPlaceholder::Hash,
        }];
        let mut chunks = vec![
            ChunkInfo {
                cross_chunk_imports: vec![ChunkImport {
                    chunk_index: 1,
                    import_kind: ImportKind::Dynamic,
                }],
                final_template: template.clone(),
                isolated_hash: b"a".to_vec(),
                ..ChunkInfo::default()
            },
            ChunkInfo {
                cross_chunk_imports: vec![ChunkImport {
                    chunk_index: 0,
                    import_kind: ImportKind::Dynamic,
                }],
                final_template: template,
                isolated_hash: b"b".to_vec(),
                ..ChunkInfo::default()
            },
        ];
        finalize_chunk_paths(&file_system, &graph, &mut chunks, &Options::default());
        assert_eq!(chunks[0].final_rel_path.len(), "chunk-".len() + 8);
        assert_eq!(chunks[1].final_rel_path.len(), "chunk-".len() + 8);
    }

    #[test]
    fn assigns_non_entry_chunk_path_template() {
        let graph = clone_linker_graph(&[], &[], &[], false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let options = Options {
            chunk_path_template: vec![
                PathTemplate {
                    data: "assets/".into(),
                    placeholder: PathPlaceholder::Name,
                },
                PathTemplate {
                    data: "-".into(),
                    placeholder: PathPlaceholder::Hash,
                },
            ],
            output_extension_js: ".mjs".into(),
            ..Options::default()
        };
        let mut chunks = vec![ChunkInfo::default()];
        assign_chunk_path_templates(&file_system, &graph, &mut chunks, &options);
        assert_eq!(
            template_to_string(&chunks[0].final_template),
            "assets/chunk-[hash].mjs"
        );
    }

    #[test]
    fn explicit_outfile_controls_entry_chunk_name_and_extension() {
        let input_files = [js_file(js_ast::Ast::default())];
        let entry_points = [EntryPoint::default()];
        let graph = clone_linker_graph(&input_files, &[0], &entry_points, false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let options = Options {
            abs_output_file: "/out/custom.cjs".into(),
            entry_path_template: vec![
                PathTemplate {
                    placeholder: PathPlaceholder::Name,
                    ..PathTemplate::default()
                },
                PathTemplate {
                    data: "-".into(),
                    placeholder: PathPlaceholder::Hash,
                },
            ],
            output_extension_js: ".js".into(),
            output_extension_css: ".css".into(),
            ..Options::default()
        };
        let mut chunks = vec![ChunkInfo {
            is_entry_point: true,
            ..ChunkInfo::default()
        }];
        assign_chunk_path_templates(&file_system, &graph, &mut chunks, &options);
        assert_eq!(
            template_to_string(&chunks[0].final_template),
            "custom-[hash].cjs"
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
    fn adjacent_local_statements_merge_by_kind_and_export_status() {
        let local = |kind, count, is_export| {
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Local(js_ast::LocalStmt {
                    declarations: vec![js_ast::Decl::default(); count],
                    kind,
                    is_export,
                    ..js_ast::LocalStmt::default()
                }),
            )
        };
        let statements = merge_adjacent_local_stmts(vec![
            local(js_ast::LocalKind::Var, 1, false),
            local(js_ast::LocalKind::Var, 2, false),
            local(js_ast::LocalKind::Var, 1, true),
            local(js_ast::LocalKind::Let, 1, false),
            local(js_ast::LocalKind::Let, 2, false),
        ]);
        assert_eq!(statements.len(), 3);
        let declaration_counts = statements
            .iter()
            .map(|statement| {
                let Some(js_ast::StmtData::Local(local)) = statement.data.as_deref() else {
                    panic!("local statement");
                };
                local.declarations.len()
            })
            .collect::<Vec<_>>();
        assert_eq!(declaration_counts, [3, 1, 3]);
    }

    #[test]
    fn bundled_statements_strip_exports_without_mutating_inputs() {
        let default_ref = Ref {
            source_index: 0,
            inner_index: 9,
        };
        let statements = vec![
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::ExportClause(js_ast::ExportClauseStmt::default()),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Local(js_ast::LocalStmt {
                    declarations: vec![js_ast::Decl::default()],
                    is_export: true,
                    ..js_ast::LocalStmt::default()
                }),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Function(js_ast::FunctionStmt {
                    is_export: true,
                    ..js_ast::FunctionStmt::default()
                }),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Class(js_ast::ClassStmt {
                    is_export: true,
                    ..js_ast::ClassStmt::default()
                }),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::ExportDefault(js_ast::ExportDefaultStmt {
                    default_name: crate::internal::ast::LocRef {
                        reference: default_ref,
                        ..crate::internal::ast::LocRef::default()
                    },
                    value: js_ast::Stmt::new(
                        Loc::default(),
                        js_ast::StmtData::Expr(js_ast::ExprStmt::default()),
                    ),
                }),
            ),
        ];
        let stripped = strip_exports_from_stmts(&statements);
        assert_eq!(stripped.len(), 4);
        assert!(matches!(
            statements[1].data.as_deref(),
            Some(js_ast::StmtData::Local(local)) if local.is_export
        ));
        assert!(matches!(
            stripped[0].data.as_deref(),
            Some(js_ast::StmtData::Local(local)) if !local.is_export
        ));
        assert!(matches!(
            stripped[1].data.as_deref(),
            Some(js_ast::StmtData::Function(function)) if !function.is_export
        ));
        assert!(matches!(
            stripped[2].data.as_deref(),
            Some(js_ast::StmtData::Class(class)) if !class.is_export
        ));
        let Some(js_ast::StmtData::Local(default_local)) = stripped[3].data.as_deref() else {
            panic!("default expression must become a local declaration");
        };
        let Some(js_ast::BindingData::Identifier(binding)) =
            default_local.declarations[0].binding.data.as_deref()
        else {
            panic!("default binding must be an identifier");
        };
        assert_eq!(binding.reference, default_ref);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn imports_convert_according_to_target_wrapper_kind() {
        let namespace_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast {
                import_records: vec![
                    ImportRecord::default(),
                    ImportRecord {
                        source_index: Index32::new(1),
                        ..ImportRecord::default()
                    },
                    ImportRecord {
                        source_index: Index32::new(2),
                        ..ImportRecord::default()
                    },
                    ImportRecord {
                        source_index: Index32::new(3),
                        ..ImportRecord::default()
                    },
                ],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                wrapper_ref: Ref {
                    source_index: 3,
                    inner_index: 0,
                },
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2, 3], &[], false);
        let Some(InputFileRepr::Js(common_js)) = graph.files[2].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        common_js.meta.wrap = WrapKind::Cjs;
        let Some(InputFileRepr::Js(esm)) = graph.files[3].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        esm.meta.wrap = WrapKind::Esm;
        esm.meta.is_async_or_has_async_dependency = true;
        graph.files[3].is_live = true;

        assert!(
            convert_import_for_chunk(
                &graph,
                0,
                Loc::default(),
                namespace_ref,
                0,
                Format::EsModule,
            )
            .keep_original
        );
        let external_require = convert_import_for_chunk(
            &graph,
            0,
            Loc::default(),
            namespace_ref,
            0,
            Format::CommonJs,
        );
        assert!(matches!(
            external_require
                .prefix_statement
                .as_ref()
                .and_then(|statement| statement.data.as_deref()),
            Some(js_ast::StmtData::Local(_))
        ));
        assert!(
            convert_import_for_chunk(
                &graph,
                0,
                Loc::default(),
                namespace_ref,
                1,
                Format::EsModule,
            )
            .prefix_statement
            .is_none()
        );
        assert!(matches!(
            convert_import_for_chunk(
                &graph,
                0,
                Loc::default(),
                namespace_ref,
                2,
                Format::EsModule,
            )
            .prefix_statement
            .as_ref()
            .and_then(|statement| statement.data.as_deref()),
            Some(js_ast::StmtData::Local(_))
        ));
        let esm_init = convert_import_for_chunk(
            &graph,
            0,
            Loc::default(),
            namespace_ref,
            3,
            Format::EsModule,
        );
        let Some(js_ast::StmtData::Expr(expression)) = esm_init
            .prefix_statement
            .as_ref()
            .and_then(|statement| statement.data.as_deref())
        else {
            panic!("ESM wrapper initializer");
        };
        assert!(matches!(
            expression.value.data.as_deref(),
            Some(js_ast::ExprData::Await(_))
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn statement_conversion_preserves_wrapper_boundaries_and_order() {
        let namespace_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast {
                import_records: vec![
                    ImportRecord::default(),
                    ImportRecord {
                        source_index: Index32::new(1),
                        ..ImportRecord::default()
                    },
                ],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast::default()),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &[EntryPoint::default()], false);
        let Some(InputFileRepr::Js(source)) = graph.files[0].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        source.meta.wrap = WrapKind::Cjs;
        let Some(InputFileRepr::Js(target)) = graph.files[1].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        target.meta.wrap = WrapKind::Cjs;
        let statements = vec![
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Import(js_ast::ImportStmt {
                    namespace_ref,
                    import_record_index: 0,
                    ..js_ast::ImportStmt::default()
                }),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Import(js_ast::ImportStmt {
                    namespace_ref,
                    import_record_index: 1,
                    ..js_ast::ImportStmt::default()
                }),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::ExportFrom(js_ast::ExportFromStmt {
                    items: vec![js_ast::ClauseItem {
                        alias: "bar".into(),
                        original_name: "foo".into(),
                        ..js_ast::ClauseItem::default()
                    }],
                    namespace_ref,
                    ..js_ast::ExportFromStmt::default()
                }),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::ExportClause(js_ast::ExportClauseStmt::default()),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Local(js_ast::LocalStmt {
                    is_export: true,
                    ..js_ast::LocalStmt::default()
                }),
            ),
        ];
        let converted = convert_stmts_for_chunk(
            &graph,
            &Options {
                mode: Mode::Bundle,
                output_format: Format::EsModule,
                ..Options::default()
            },
            0,
            &statements,
        );
        assert_eq!(converted.inside_wrapper_prefix.len(), 1);
        assert!(matches!(
            converted.inside_wrapper_prefix[0].data.as_deref(),
            Some(js_ast::StmtData::Local(_))
        ));
        assert_eq!(converted.outside_wrapper_prefix.len(), 2);
        assert!(
            converted
                .outside_wrapper_prefix
                .iter()
                .all(|statement| matches!(
                    statement.data.as_deref(),
                    Some(js_ast::StmtData::Import(_))
                ))
        );
        let Some(js_ast::StmtData::Import(re_export)) =
            converted.outside_wrapper_prefix[1].data.as_deref()
        else {
            panic!("re-export must become an import");
        };
        assert_eq!(re_export.items.as_ref().expect("items")[0].alias, "foo");
        assert_eq!(converted.inside_wrapper_suffix.len(), 1);
        assert!(matches!(
            converted.inside_wrapper_suffix[0].data.as_deref(),
            Some(js_ast::StmtData::Local(local)) if !local.is_export
        ));
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

    fn import_tracker_graph(
        importer_loader: Loader,
        named_import: NamedImport,
        record: ImportRecord,
        target: JsRepr,
    ) -> (crate::internal::graph::LinkerGraph, ImportTracker) {
        let import_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let input_files = vec![
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast: js_ast::Ast {
                        import_records: vec![record],
                        named_imports: HashMap::from([(import_ref, named_import)]),
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                loader: importer_loader,
                ..InputFile::default()
            },
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(target))),
                loader: Loader::Js,
                ..InputFile::default()
            },
        ];
        (
            clone_linker_graph(&input_files, &[0, 1], &[EntryPoint::default()], false),
            ImportTracker {
                source_index: 0,
                import_ref,
                ..ImportTracker::default()
            },
        )
    }

    #[test]
    fn import_tracker_identifies_external_and_empty_modules() {
        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "missing".into(),
                ..NamedImport::default()
            },
            ImportRecord::default(),
            JsRepr::default(),
        );
        assert_eq!(
            advance_import_tracker(&graph, tracker).1,
            ImportStatus::External
        );

        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "missing".into(),
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr::default(),
        );
        let (next, status, _) = advance_import_tracker(&graph, tracker);
        assert_eq!(status, ImportStatus::CommonJsWithoutExports);
        assert_eq!(next.source_index, 1);
        assert_eq!(next.import_ref, crate::internal::ast::INVALID_REF);
    }

    #[test]
    fn import_tracker_identifies_common_js_and_dynamic_fallbacks() {
        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "value".into(),
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    exports_kind: ExportsKind::CommonJs,
                    uses_exports_ref: true,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        assert_eq!(
            advance_import_tracker(&graph, tracker).1,
            ImportStatus::CommonJs
        );

        let namespace_ref = Ref {
            source_index: 1,
            inner_index: 7,
        };
        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "value".into(),
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    exports_kind: ExportsKind::EsmWithDynamicFallback,
                    exports_ref: namespace_ref,
                    uses_exports_ref: true,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        let (next, status, _) = advance_import_tracker(&graph, tracker);
        assert_eq!(status, ImportStatus::DynamicFallback);
        assert_eq!(next.import_ref, namespace_ref);
    }

    #[test]
    fn import_tracker_matches_named_and_namespace_exports() {
        let export_ref = Ref {
            source_index: 1,
            inner_index: 3,
        };
        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "value".into(),
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    named_exports: HashMap::from([(
                        "value".into(),
                        NamedExport {
                            reference: export_ref,
                            alias_loc: Loc { start: 9 },
                        },
                    )]),
                    export_keyword: Range {
                        loc: Loc { start: 1 },
                        len: 6,
                    },
                    exports_kind: ExportsKind::Esm,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        let (next, status, _) = advance_import_tracker(&graph, tracker);
        assert_eq!(status, ImportStatus::Found);
        assert_eq!(next.source_index, 1);
        assert_eq!(next.import_ref, export_ref);
        assert_eq!(next.name_loc, Loc { start: 9 });

        let namespace_ref = Ref {
            source_index: 1,
            inner_index: 8,
        };
        let (mut graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "*".into(),
                alias_is_star: true,
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    exports_ref: namespace_ref,
                    exports_kind: ExportsKind::Esm,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        resolve_export_stars(&mut graph);
        let (next, status, _) = advance_import_tracker(&graph, tracker);
        assert_eq!(status, ImportStatus::Found);
        assert_eq!(next.import_ref, namespace_ref);
    }

    #[test]
    fn import_tracker_distinguishes_typescript_types_from_missing_exports() {
        let missing_target = || JsRepr {
            ast: js_ast::Ast {
                export_keyword: Range {
                    loc: Loc { start: 1 },
                    len: 6,
                },
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            },
            ..JsRepr::default()
        };
        let (graph, tracker) = import_tracker_graph(
            Loader::Ts,
            NamedImport {
                alias: "Type".into(),
                is_exported: true,
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            missing_target(),
        );
        assert_eq!(
            advance_import_tracker(&graph, tracker).1,
            ImportStatus::ProbablyTypeScriptType
        );

        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "missing".into(),
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            missing_target(),
        );
        let (next, status, _) = advance_import_tracker(&graph, tracker);
        assert_eq!(status, ImportStatus::NoMatch);
        assert_eq!(next.source_index, 1);
    }

    #[test]
    fn import_matcher_handles_normal_namespace_and_external_results() {
        let export_ref = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "value".into(),
                namespace_ref: crate::internal::ast::INVALID_REF,
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    named_exports: HashMap::from([("value".into(), named_export(export_ref))]),
                    export_keyword: Range {
                        loc: Loc { start: 1 },
                        len: 6,
                    },
                    exports_kind: ExportsKind::Esm,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        let (result, _) = match_import_with_export(
            &graph,
            tracker,
            Vec::new(),
            &mut Vec::new(),
            Format::EsModule,
        );
        assert_eq!(result.kind, MatchImportKind::Normal);
        assert_eq!(result.reference, export_ref);

        let namespace_ref = Ref {
            source_index: 0,
            inner_index: 9,
        };
        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "value".into(),
                namespace_ref,
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    exports_kind: ExportsKind::CommonJs,
                    uses_exports_ref: true,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        let (result, _) = match_import_with_export(
            &graph,
            tracker,
            Vec::new(),
            &mut Vec::new(),
            Format::CommonJs,
        );
        assert_eq!(result.kind, MatchImportKind::Namespace);
        assert_eq!(result.namespace_ref, namespace_ref);
        assert_eq!(result.alias, "value");

        let (graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "external".into(),
                namespace_ref,
                ..NamedImport::default()
            },
            ImportRecord::default(),
            JsRepr::default(),
        );
        let (preserved, _) = match_import_with_export(
            &graph,
            tracker,
            Vec::new(),
            &mut Vec::new(),
            Format::EsModule,
        );
        assert_eq!(preserved.kind, MatchImportKind::Ignore);
        let (converted, _) = match_import_with_export(
            &graph,
            tracker,
            Vec::new(),
            &mut Vec::new(),
            Format::CommonJs,
        );
        assert_eq!(converted.kind, MatchImportKind::Namespace);
    }

    #[test]
    fn import_matcher_follows_reexports_and_collects_dependencies() {
        let root_import = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let middle_import = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let leaf_export = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(1),
                    ..ImportRecord::default()
                }],
                named_imports: HashMap::from([(
                    root_import,
                    NamedImport {
                        alias: "public".into(),
                        namespace_ref: crate::internal::ast::INVALID_REF,
                        ..NamedImport::default()
                    },
                )]),
                top_level_symbol_to_parts_from_parser: HashMap::from([(root_import, vec![5])]),
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    ..ImportRecord::default()
                }],
                named_imports: HashMap::from([(
                    middle_import,
                    NamedImport {
                        alias: "leaf".into(),
                        namespace_ref: crate::internal::ast::INVALID_REF,
                        ..NamedImport::default()
                    },
                )]),
                named_exports: HashMap::from([("public".into(), named_export(middle_import))]),
                top_level_symbol_to_parts_from_parser: HashMap::from([(middle_import, vec![6])]),
                export_keyword: Range {
                    loc: Loc { start: 1 },
                    len: 6,
                },
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                named_exports: HashMap::from([("leaf".into(), named_export(leaf_export))]),
                export_keyword: Range {
                    loc: Loc { start: 1 },
                    len: 6,
                },
                exports_kind: ExportsKind::Esm,
                ..js_ast::Ast::default()
            }),
        ];
        let graph = clone_linker_graph(&input_files, &[0, 1, 2], &[EntryPoint::default()], false);
        let (result, dependencies) = match_import_with_export(
            &graph,
            ImportTracker {
                source_index: 0,
                import_ref: root_import,
                ..ImportTracker::default()
            },
            Vec::new(),
            &mut Vec::new(),
            Format::EsModule,
        );
        assert_eq!(result.kind, MatchImportKind::Normal);
        assert_eq!(result.source_index, 2);
        assert_eq!(result.reference, leaf_export);
        assert_eq!(
            dependencies,
            vec![
                js_ast::Dependency {
                    source_index: 0,
                    part_index: 5,
                },
                js_ast::Dependency {
                    source_index: 1,
                    part_index: 6,
                },
            ]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn import_matcher_detects_cycles_and_divergent_ambiguity() {
        let first = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let second = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(1),
                    ..ImportRecord::default()
                }],
                named_imports: HashMap::from([(
                    first,
                    NamedImport {
                        alias: "a".into(),
                        ..NamedImport::default()
                    },
                )]),
                named_exports: HashMap::from([("a".into(), named_export(first))]),
                export_keyword: Range {
                    loc: Loc { start: 1 },
                    len: 6,
                },
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(0),
                    ..ImportRecord::default()
                }],
                named_imports: HashMap::from([(
                    second,
                    NamedImport {
                        alias: "a".into(),
                        ..NamedImport::default()
                    },
                )]),
                named_exports: HashMap::from([("a".into(), named_export(second))]),
                export_keyword: Range {
                    loc: Loc { start: 1 },
                    len: 6,
                },
                ..js_ast::Ast::default()
            }),
        ];
        let graph = clone_linker_graph(&input_files, &[0, 1], &[EntryPoint::default()], false);
        let (cycle, _) = match_import_with_export(
            &graph,
            ImportTracker {
                source_index: 0,
                import_ref: first,
                ..ImportTracker::default()
            },
            Vec::new(),
            &mut Vec::new(),
            Format::EsModule,
        );
        assert_eq!(cycle.kind, MatchImportKind::Cycle);

        let import_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let first_export = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let second_export = Ref {
            source_index: 3,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(1),
                    ..ImportRecord::default()
                }],
                named_imports: HashMap::from([(
                    import_ref,
                    NamedImport {
                        alias: "same".into(),
                        ..NamedImport::default()
                    },
                )]),
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                export_keyword: Range {
                    loc: Loc { start: 1 },
                    len: 6,
                },
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast::default()),
        ];
        let mut graph =
            clone_linker_graph(&input_files, &[0, 1, 2, 3], &[EntryPoint::default()], false);
        let InputFileRepr::Js(target) = graph.files[1]
            .input_file
            .repr
            .as_mut()
            .expect("JavaScript representation")
        else {
            panic!("JavaScript representation");
        };
        target.meta.resolved_exports.insert(
            "same".into(),
            crate::internal::graph::ExportData {
                source_index: 2,
                reference: first_export,
                name_loc: Loc { start: 10 },
                potentially_ambiguous_export_star_refs: vec![crate::internal::graph::ImportData {
                    source_index: 3,
                    reference: second_export,
                    name_loc: Loc { start: 20 },
                    ..crate::internal::graph::ImportData::default()
                }],
            },
        );
        let (ambiguous, dependencies) = match_import_with_export(
            &graph,
            ImportTracker {
                source_index: 0,
                import_ref,
                ..ImportTracker::default()
            },
            Vec::new(),
            &mut Vec::new(),
            Format::EsModule,
        );
        assert_eq!(ambiguous.kind, MatchImportKind::Ambiguous);
        assert_eq!(ambiguous.source_index, 2);
        assert_eq!(ambiguous.name_loc, Loc { start: 10 });
        assert_eq!(ambiguous.other_source_index, 3);
        assert_eq!(ambiguous.other_name_loc, Loc { start: 20 });
        assert!(dependencies.is_empty());
    }

    #[test]
    fn phase_four_binder_populates_graph_metadata() {
        let export_ref = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let (mut graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "value".into(),
                namespace_ref: crate::internal::ast::INVALID_REF,
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    named_exports: HashMap::from([("value".into(), named_export(export_ref))]),
                    export_keyword: Range {
                        loc: Loc { start: 1 },
                        len: 6,
                    },
                    exports_kind: ExportsKind::Esm,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        assert!(bind_imports_to_exports_for_file(&mut graph, 0, Format::EsModule).is_empty());
        let import = &js_repr(&graph, 0).meta.imports_to_bind[&tracker.import_ref];
        assert_eq!(import.source_index, 1);
        assert_eq!(import.reference, export_ref);

        let namespace_ref = Ref {
            source_index: 0,
            inner_index: 7,
        };
        let (mut graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "property".into(),
                namespace_ref,
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    exports_kind: ExportsKind::CommonJs,
                    uses_exports_ref: true,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        graph.symbols.symbols_for_source[0].push(Symbol::new(SymbolKind::Import, "property"));
        assert!(bind_imports_to_exports_for_file(&mut graph, 0, Format::CommonJs).is_empty());
        let alias = graph
            .symbols
            .get(tracker.import_ref)
            .namespace_alias
            .as_ref()
            .expect("namespace alias");
        assert_eq!(alias.namespace_ref, namespace_ref);
        assert_eq!(alias.alias, "property");

        let (mut graph, tracker) = import_tracker_graph(
            Loader::Ts,
            NamedImport {
                alias: "Type".into(),
                is_exported: true,
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr {
                ast: js_ast::Ast {
                    export_keyword: Range {
                        loc: Loc { start: 1 },
                        len: 6,
                    },
                    exports_kind: ExportsKind::Esm,
                    ..js_ast::Ast::default()
                },
                ..JsRepr::default()
            },
        );
        assert!(bind_imports_to_exports_for_file(&mut graph, 0, Format::EsModule).is_empty());
        assert!(js_repr(&graph, 0).meta.is_probably_type_script_type[&tracker.import_ref]);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn phase_five_sorts_and_filters_resolved_exports() {
        let good_ref = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let type_ref = Ref {
            source_index: 2,
            inner_index: 1,
        };
        let bad_ref = Ref {
            source_index: 3,
            inner_index: 0,
        };
        let proxy_a = Ref {
            source_index: 2,
            inner_index: 2,
        };
        let proxy_b = Ref {
            source_index: 3,
            inner_index: 2,
        };
        let shared_ref = Ref {
            source_index: 4,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast::default()),
        ];
        let mut graph = clone_linker_graph(
            &input_files,
            &[0, 1, 2, 3, 4],
            &[EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            }],
            false,
        );
        let InputFileRepr::Js(source) = graph.files[1]
            .input_file
            .repr
            .as_mut()
            .expect("JavaScript representation")
        else {
            panic!("JavaScript representation");
        };
        source.meta.resolved_exports = HashMap::from([
            (
                "good".into(),
                crate::internal::graph::ExportData {
                    source_index: 2,
                    reference: good_ref,
                    ..crate::internal::graph::ExportData::default()
                },
            ),
            (
                "typeOnly".into(),
                crate::internal::graph::ExportData {
                    source_index: 2,
                    reference: type_ref,
                    ..crate::internal::graph::ExportData::default()
                },
            ),
            (
                "ambiguous".into(),
                crate::internal::graph::ExportData {
                    source_index: 2,
                    reference: good_ref,
                    name_loc: Loc { start: 10 },
                    potentially_ambiguous_export_star_refs: vec![
                        crate::internal::graph::ImportData {
                            source_index: 3,
                            reference: bad_ref,
                            name_loc: Loc { start: 20 },
                            ..crate::internal::graph::ImportData::default()
                        },
                    ],
                },
            ),
            (
                "same".into(),
                crate::internal::graph::ExportData {
                    source_index: 2,
                    reference: proxy_a,
                    potentially_ambiguous_export_star_refs: vec![
                        crate::internal::graph::ImportData {
                            source_index: 3,
                            reference: proxy_b,
                            ..crate::internal::graph::ImportData::default()
                        },
                    ],
                    ..crate::internal::graph::ExportData::default()
                },
            ),
        ]);
        let InputFileRepr::Js(first) = graph.files[2]
            .input_file
            .repr
            .as_mut()
            .expect("JavaScript representation")
        else {
            panic!("JavaScript representation");
        };
        first
            .meta
            .is_probably_type_script_type
            .insert(type_ref, true);
        first.meta.imports_to_bind.insert(
            proxy_a,
            crate::internal::graph::ImportData {
                source_index: 4,
                reference: shared_ref,
                ..crate::internal::graph::ImportData::default()
            },
        );
        let InputFileRepr::Js(second) = graph.files[3]
            .input_file
            .repr
            .as_mut()
            .expect("JavaScript representation")
        else {
            panic!("JavaScript representation");
        };
        second.meta.imports_to_bind.insert(
            proxy_b,
            crate::internal::graph::ImportData {
                source_index: 4,
                reference: shared_ref,
                ..crate::internal::graph::ImportData::default()
            },
        );

        assert_eq!(
            sort_and_filter_export_aliases(&mut graph, 1),
            vec![AmbiguousReExport {
                alias: "ambiguous".into(),
                source_index: 2,
                name_loc: Loc { start: 10 },
                other_source_index: 3,
                other_name_loc: Loc { start: 20 },
            }]
        );
        assert_eq!(
            js_repr(&graph, 1).meta.sorted_and_filtered_export_aliases,
            ["good", "same"]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn wrapper_parts_encode_runtime_dependencies() {
        let cjs_runtime_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let esm_runtime_ref = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let input_files = vec![
            js_file(js_ast::Ast {
                symbols: vec![
                    Symbol::new(SymbolKind::Other, "__commonJS"),
                    Symbol::new(SymbolKind::Other, "__esm"),
                ],
                parts: vec![js_ast::Part::default(), js_ast::Part::default()],
                top_level_symbol_to_parts_from_parser: HashMap::from([
                    (cjs_runtime_ref, vec![0]),
                    (esm_runtime_ref, vec![1]),
                ]),
                ..js_ast::Ast::default()
            }),
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    meta: crate::internal::graph::JsReprMeta {
                        wrap: WrapKind::Cjs,
                        ..crate::internal::graph::JsReprMeta::default()
                    },
                    ast: js_ast::Ast {
                        symbols: vec![
                            Symbol::new(SymbolKind::Other, "exports"),
                            Symbol::new(SymbolKind::Other, "module"),
                            Symbol::new(SymbolKind::Other, "require_file"),
                        ],
                        exports_ref: Ref {
                            source_index: 1,
                            inner_index: 0,
                        },
                        module_ref: Ref {
                            source_index: 1,
                            inner_index: 1,
                        },
                        wrapper_ref: Ref {
                            source_index: 1,
                            inner_index: 2,
                        },
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                loader: Loader::Js,
                ..InputFile::default()
            },
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    meta: crate::internal::graph::JsReprMeta {
                        wrap: WrapKind::Esm,
                        ..crate::internal::graph::JsReprMeta::default()
                    },
                    ast: js_ast::Ast {
                        symbols: vec![
                            Symbol::new(SymbolKind::Other, "exports"),
                            Symbol::new(SymbolKind::Other, "module"),
                            Symbol::new(SymbolKind::Other, "init_file"),
                        ],
                        wrapper_ref: Ref {
                            source_index: 2,
                            inner_index: 2,
                        },
                        ..js_ast::Ast::default()
                    },
                    ..JsRepr::default()
                }))),
                loader: Loader::Js,
                ..InputFile::default()
            },
        ];
        let mut graph = clone_linker_graph(
            &input_files,
            &[0, 1, 2],
            &[EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            }],
            false,
        );
        create_wrapper_for_file(&mut graph, 1, cjs_runtime_ref, esm_runtime_ref);
        create_wrapper_for_file(&mut graph, 2, cjs_runtime_ref, esm_runtime_ref);

        let cjs = js_repr(&graph, 1);
        assert_eq!(cjs.meta.wrapper_part_index.get_index(), 0);
        assert_eq!(cjs.ast.parts[0].declared_symbols.len(), 3);
        assert_eq!(cjs.ast.parts[0].dependencies.len(), 2);
        assert_eq!(cjs.meta.imports_to_bind[&cjs_runtime_ref].source_index, 0);
        let esm = js_repr(&graph, 2);
        assert_eq!(esm.meta.wrapper_part_index.get_index(), 0);
        assert_eq!(esm.ast.parts[0].declared_symbols.len(), 1);
        assert_eq!(esm.ast.parts[0].dependencies.len(), 2);
        assert_eq!(esm.meta.imports_to_bind[&esm_runtime_ref].source_index, 0);
    }

    fn tree_shaking_fixture() -> Vec<InputFile> {
        vec![
            js_file(js_ast::Ast {
                import_records: vec![
                    ImportRecord {
                        source_index: Index32::new(2),
                        kind: ImportKind::Stmt,
                        ..ImportRecord::default()
                    },
                    ImportRecord {
                        source_index: Index32::new(3),
                        kind: ImportKind::Stmt,
                        ..ImportRecord::default()
                    },
                ],
                parts: vec![
                    js_ast::Part {
                        dependencies: vec![js_ast::Dependency {
                            source_index: 1,
                            part_index: 0,
                        }],
                        ..js_ast::Part::default()
                    },
                    js_ast::Part {
                        can_be_removed_if_unused: true,
                        ..js_ast::Part::default()
                    },
                    js_ast::Part {
                        import_record_indices: vec![0],
                        can_be_removed_if_unused: true,
                        ..js_ast::Part::default()
                    },
                    js_ast::Part {
                        import_record_indices: vec![1],
                        can_be_removed_if_unused: true,
                        ..js_ast::Part::default()
                    },
                ],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part {
                    can_be_removed_if_unused: true,
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
            InputFile {
                side_effects: SideEffects {
                    kind: SideEffectsKind::NoSideEffectsPureData,
                    ..SideEffects::default()
                },
                ..js_file(js_ast::Ast {
                    parts: vec![js_ast::Part::default()],
                    ..js_ast::Ast::default()
                })
            },
        ]
    }

    #[test]
    fn tree_shaking_marks_only_side_effect_relevant_parts() {
        let input_files = tree_shaking_fixture();
        let mut graph =
            clone_linker_graph(&input_files, &[0, 1, 2, 3], &[EntryPoint::default()], false);
        mark_file_live_for_tree_shaking(
            &mut graph,
            &Options {
                tree_shaking: true,
                ..Options::default()
            },
            0,
        );

        let entry = js_repr(&graph, 0);
        assert!(entry.ast.parts[0].is_live);
        assert!(!entry.ast.parts[1].is_live);
        assert!(entry.ast.parts[2].is_live);
        assert!(!entry.ast.parts[3].is_live);
        assert!(graph.files[1].is_live);
        assert!(js_repr(&graph, 1).ast.parts[0].is_live);
        assert!(graph.files[2].is_live);
        assert!(js_repr(&graph, 2).ast.parts[0].is_live);
        assert!(!graph.files[3].is_live);

        let mut graph =
            clone_linker_graph(&input_files, &[0, 1, 2, 3], &[EntryPoint::default()], false);
        mark_file_live_for_tree_shaking(
            &mut graph,
            &Options {
                tree_shaking: true,
                ignore_dce_annotations: true,
                ..Options::default()
            },
            0,
        );
        assert!(js_repr(&graph, 0).ast.parts[3].is_live);
        assert!(graph.files[3].is_live);
    }

    #[test]
    fn code_splitting_propagates_entry_bits_but_skips_dynamic_entries() {
        let input_files = vec![
            js_file(js_ast::Ast {
                import_records: vec![
                    ImportRecord {
                        source_index: Index32::new(1),
                        kind: ImportKind::Stmt,
                        ..ImportRecord::default()
                    },
                    ImportRecord {
                        source_index: Index32::new(2),
                        kind: ImportKind::Dynamic,
                        ..ImportRecord::default()
                    },
                ],
                parts: vec![js_ast::Part {
                    import_record_indices: vec![0, 1],
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(1),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                parts: vec![js_ast::Part {
                    import_record_indices: vec![0],
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            }),
        ];
        let entry_points = [
            EntryPoint {
                source_index: 0,
                ..EntryPoint::default()
            },
            EntryPoint {
                source_index: 2,
                ..EntryPoint::default()
            },
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &entry_points, true);
        let options = Options {
            code_splitting: true,
            tree_shaking: true,
            ..Options::default()
        };
        tree_shaking_and_code_splitting(&mut graph, &options);

        assert!(graph.files[0].entry_bits.has_bit(0));
        assert!(!graph.files[0].entry_bits.has_bit(1));
        assert!(graph.files[2].entry_bits.has_bit(1));
        assert!(!graph.files[2].entry_bits.has_bit(0));
        assert!(graph.files[1].entry_bits.has_bit(0));
        assert!(graph.files[1].entry_bits.has_bit(1));
        assert_eq!(graph.files[0].distance_from_entry_point, 0);
        assert_eq!(graph.files[2].distance_from_entry_point, 0);
        assert_eq!(graph.files[1].distance_from_entry_point, 1);

        let mut chunks = compute_js_chunks(&mut graph, &options, PREFIX);
        compute_cross_chunk_dependencies(&mut graph, &mut chunks, &options);
        assert!(chunks[0].cross_chunk_imports.contains(&ChunkImport {
            chunk_index: 1,
            import_kind: ImportKind::Dynamic,
        }));
    }

    #[test]
    fn live_files_are_grouped_into_deterministic_js_chunks() {
        let input_files = vec![
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                parts: vec![js_ast::Part {
                    import_record_indices: vec![0],
                    dependencies: vec![js_ast::Dependency {
                        source_index: 0,
                        part_index: 0,
                    }],
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                parts: vec![js_ast::Part {
                    import_record_indices: vec![0],
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            }),
        ];
        let entry_points = [
            EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            },
            EntryPoint {
                source_index: 3,
                ..EntryPoint::default()
            },
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2, 3], &entry_points, true);
        let options = Options {
            code_splitting: true,
            tree_shaking: true,
            ..Options::default()
        };
        tree_shaking_and_code_splitting(&mut graph, &options);
        let chunks = compute_js_chunks(&mut graph, &options, PREFIX);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].unique_key, "UNIQUEC00000000");
        assert_eq!(chunks[1].unique_key, "UNIQUEC00000001");
        assert_eq!(chunks[2].unique_key, "UNIQUEC00000002");
        assert!(chunks[0].is_entry_point);
        assert_eq!(chunks[0].source_index, 1);
        assert_eq!(chunks[0].entry_bits.as_bytes(), &[1]);
        assert_eq!(chunks[0].files_with_parts_in_chunk, HashSet::from([0, 1]));
        assert_eq!(chunks[0].files_in_chunk_in_order, [0, 1]);
        assert!(chunks[1].is_entry_point);
        assert_eq!(chunks[1].source_index, 3);
        assert_eq!(chunks[1].entry_bits.as_bytes(), &[2]);
        assert_eq!(chunks[1].files_with_parts_in_chunk, HashSet::from([3]));
        assert!(!chunks[2].is_entry_point);
        assert_eq!(chunks[2].entry_bits.as_bytes(), &[3]);
        assert_eq!(chunks[2].files_with_parts_in_chunk, HashSet::from([2]));
        assert_eq!(graph.files[1].entry_point_chunk_index, 0);
        assert_eq!(graph.files[3].entry_point_chunk_index, 1);
    }

    #[test]
    fn empty_entry_points_still_get_chunks() {
        let input_files = vec![
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(
            &input_files,
            &[0, 1],
            &[EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            }],
            false,
        );
        let chunks = compute_js_chunks(&mut graph, &Options::default(), PREFIX);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_entry_point);
        assert!(chunks[0].files_with_parts_in_chunk.is_empty());
        assert!(chunks[0].files_in_chunk_in_order.is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn cross_chunk_symbol_edges_get_deterministic_aliases() {
        let shared_ref = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let input_files = vec![
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                parts: vec![js_ast::Part {
                    import_record_indices: vec![0],
                    symbol_uses: HashMap::from([(
                        shared_ref,
                        js_ast::SymbolUse { count_estimate: 1 },
                    )]),
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "shared")],
                parts: vec![js_ast::Part {
                    declared_symbols: vec![js_ast::DeclaredSymbol {
                        reference: shared_ref,
                        is_top_level: true,
                    }],
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    kind: ImportKind::Stmt,
                    ..ImportRecord::default()
                }],
                parts: vec![js_ast::Part {
                    import_record_indices: vec![0],
                    symbol_uses: HashMap::from([(
                        shared_ref,
                        js_ast::SymbolUse { count_estimate: 1 },
                    )]),
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            }),
        ];
        let entry_points = [
            EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            },
            EntryPoint {
                source_index: 3,
                ..EntryPoint::default()
            },
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2, 3], &entry_points, true);
        let options = Options {
            code_splitting: true,
            output_format: Format::EsModule,
            tree_shaking: true,
            ..Options::default()
        };
        tree_shaking_and_code_splitting(&mut graph, &options);
        let mut chunks = compute_js_chunks(&mut graph, &options, PREFIX);
        compute_cross_chunk_dependencies(&mut graph, &mut chunks, &options);
        generate_cross_chunk_stmts(&graph, &mut chunks, &options);

        assert_eq!(graph.symbols.get(shared_ref).chunk_index.get_index(), 2);
        assert_eq!(chunks[2].exports_to_other_chunks[&shared_ref], "shared");
        for chunk in &chunks[..2] {
            assert_eq!(chunk.sorted_cross_chunk_imports.len(), 1);
            assert_eq!(chunk.sorted_cross_chunk_imports[0].chunk_index, 2);
            assert_eq!(
                chunk.sorted_cross_chunk_imports[0].sorted_import_items,
                vec![CrossChunkImportItem {
                    export_alias: "shared".into(),
                    reference: shared_ref,
                }]
            );
            assert_eq!(
                chunk.cross_chunk_imports,
                vec![ChunkImport {
                    chunk_index: 2,
                    import_kind: ImportKind::Stmt,
                }]
            );
            let Some(js_ast::StmtData::Import(import)) =
                chunk.cross_chunk_prefix_stmts[0].data.as_deref()
            else {
                panic!("cross-chunk prefix must be an import");
            };
            assert_eq!(import.import_record_index, 0);
            let items = import.items.as_ref().expect("named import");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].alias, "shared");
            assert_eq!(items[0].name.reference, shared_ref);
        }
        let Some(js_ast::StmtData::ExportClause(export)) =
            chunks[2].cross_chunk_suffix_stmts[0].data.as_deref()
        else {
            panic!("cross-chunk suffix must be an export");
        };
        assert_eq!(export.items.len(), 1);
        assert_eq!(export.items[0].alias, "shared");
        assert_eq!(export.items[0].name.reference, shared_ref);

        let renamer = crate::internal::renamer::new_no_op_renamer(graph.symbols.clone());
        let entry_bindings = print_cross_chunk_bindings(&chunks, 0, &renamer, &options);
        assert_eq!(
            entry_bindings.prefix,
            b"import { shared } from \"UNIQUEC00000002\";\n"
        );
        assert!(entry_bindings.suffix.is_empty());
        let shared_bindings = print_cross_chunk_bindings(&chunks, 2, &renamer, &options);
        assert!(shared_bindings.prefix.is_empty());
        assert_eq!(shared_bindings.suffix, b"export { shared };\n");
    }
}
