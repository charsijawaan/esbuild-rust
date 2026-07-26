//! Port of upstream `internal/linker`.
//!
//! The linker is esbuild's second bundling phase. This initial section ports
//! the output-piece representation and the final path substitution machinery
//! used after chunks have been generated.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::BuildHasher;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::internal::{
    ast::{
        CharFreq, DEFAULT_NAME_MINIFIER_CSS, DEFAULT_NAME_MINIFIER_JS, INVALID_REF,
        ImportItemStatus, ImportKind, ImportRecord, ImportRecordFlags, Index32, LocRef,
        NamespaceAlias, Ref, SymbolKind,
    },
    bundler::{hash_for_file_name, path_relative_to_outbase},
    config::{
        Format, LegalComments, Loader, Mode, Options, PathPlaceholder, PathPlaceholders,
        PathTemplate, SourceMap as SourceMapMode, has_placeholder, substitute_template,
        template_to_string,
    },
    css_ast::{
        Ast as CssAst, AtImportRule, AtLayerRule, AtMediaRule, ImportConditions, KnownAtRule, Rule,
        RuleData, clone_media_queries_with_import_records, clone_tokens_with_import_records,
        media_queries_equal_ignoring_whitespace, tokens_equal_ignoring_whitespace,
    },
    css_lexer::TokenKind,
    css_parser, css_printer,
    fs::Fs,
    graph::{
        EntryPoint, ExportData, ImportData, InputFile, InputFileRepr, LinkerGraph, OutputFile,
        SideEffectsKind, WrapKind, clone_linker_graph,
    },
    helpers::{
        BitSet, Joiner, encode_string_as_shortest_data_url, escape_closing_tag, quote_for_json,
        string_array_arrays_equal, string_to_utf16, utf16_to_string,
    },
    js_ast::{self, ExportsKind},
    logger::{Log, Path, Range},
    sourcemap::{
        Chunk as SourceMapChunk, LineColumnOffset, MappingsBuffer, SourceMapPieces, SourceMapShift,
        SourceMapState, append_source_map_chunk,
    },
    xxhash,
};

const CIRCULAR_CHUNK_IMPORT_ERROR: &str =
    "Internal error: generated chunks contain a circular import";

/// Configure entry-point export semantics before linker scanning begins.
pub fn configure_entry_point_exports(graph: &mut LinkerGraph, options: &Options) {
    for entry_point in graph.entry_points().to_vec() {
        let Some(InputFileRepr::Js(repr)) = graph.files[entry_point.source_index as usize]
            .input_file
            .repr
            .as_mut()
        else {
            continue;
        };
        if repr.ast.has_lazy_export
            && (options.mode == Mode::PassThrough
                || (options.mode == Mode::ConvertFormat
                    && !options.output_format.keep_esm_import_export_syntax()))
        {
            repr.ast.exports_kind = ExportsKind::CommonJs;
        }
        if repr.ast.export_keyword.len > 0
            && (options.output_format == Format::CommonJs
                || (options.output_format == Format::Iife && !options.global_name.is_empty()))
        {
            repr.ast.uses_exports_ref = true;
            repr.meta.force_include_exports_for_entry_point = true;
        }
    }
}

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
    pub source_map_results: Vec<CompileResultForSourceMap>,
    pub metadata_imports: Vec<IntermediateOutput>,
    pub metadata_inputs: Vec<MetadataInput>,
    pub external_legal_comments: Vec<u8>,
    pub isolated_hash: Vec<u8>,
    pub entry_point_bit: usize,
    pub source_index: u32,
    pub css_chunk_index: Index32,
    pub is_entry_point: bool,
    pub is_css: bool,
    pub is_executable: bool,
    pub imports_in_css_order: Vec<CssImportOrder>,
}

#[derive(Debug, Default)]
pub struct MetadataInput {
    pub source_index: u32,
    pub outputs: Vec<IntermediateOutput>,
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
    NoMatch,
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
    pub is_missing: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportMatchIssue {
    pub import_ref: Ref,
    pub result: MatchImportResult,
    pub suggestion: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AmbiguousReExport {
    pub alias: String,
    pub source_index: u32,
    pub name_loc: crate::internal::logger::Loc,
    pub other_source_index: u32,
    pub other_name_loc: crate::internal::logger::Loc,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanRuntimeRefs {
    pub export_ref: Ref,
    pub common_js_ref: Ref,
    pub esm_ref: Ref,
    pub to_common_js_ref: Ref,
    pub unbound_module_ref: Ref,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScanImportsAndExportsResult {
    pub import_issues: Vec<(u32, ImportMatchIssue)>,
    pub ambiguous_re_exports: Vec<(u32, AmbiguousReExport)>,
    pub arbitrary_namespace_issues: Vec<ArbitraryNamespaceIssue>,
}

#[derive(Debug, Default)]
pub struct PreparedLinkerGraph {
    pub graph: LinkerGraph,
    pub chunks: Vec<ChunkInfo>,
    pub scan_result: ScanImportsAndExportsResult,
    pub unbound_module_ref: Ref,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArbitraryNamespaceIssue {
    pub kind: String,
    pub source_index: u32,
    pub name_loc: crate::internal::logger::Loc,
    pub alias: String,
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
                if status == ImportStatus::CommonJsWithoutExports {
                    result.is_missing = true;
                    result.source_index = next_tracker.source_index;
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

            ImportStatus::NoMatch => {
                result = MatchImportResult {
                    kind: MatchImportKind::NoMatch,
                    source_index: next_tracker.source_index,
                    ..MatchImportResult::default()
                };
            }

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

/// Suggest an obvious one-character correction for a missing export.
pub fn maybe_correct_export_typo(
    graph: &mut LinkerGraph,
    source_index: u32,
    name: &str,
) -> Option<String> {
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_mut()
    else {
        return None;
    };
    if repr.meta.resolved_export_typos.is_none() {
        let mut valid = repr
            .meta
            .resolved_exports
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        valid.sort_unstable();
        repr.meta.resolved_export_typos = Some(crate::internal::helpers::TypoDetector::new(&valid));
    }
    repr.meta
        .resolved_export_typos
        .as_ref()
        .and_then(|detector| detector.maybe_correct_typo(name))
        .map(str::to_owned)
}

/// Validate entry-point export aliases against the configured target.
#[must_use]
pub fn arbitrary_namespace_export_issues(
    graph: &LinkerGraph,
    source_index: u32,
    options: &Options,
) -> Vec<ArbitraryNamespaceIssue> {
    if options.output_format != Format::EsModule
        || !options
            .unsupported_js_features
            .contains(crate::internal::compat::JsFeature::ARBITRARY_MODULE_NAMESPACE_NAMES)
        || !graph.files[source_index as usize].is_entry_point()
    {
        return Vec::new();
    }
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_ref()
    else {
        return Vec::new();
    };
    repr.meta
        .sorted_and_filtered_export_aliases
        .iter()
        .filter(|alias| !js_ast::is_identifier(alias))
        .map(|alias| {
            let export = &repr.meta.resolved_exports[alias];
            ArbitraryNamespaceIssue {
                kind: "export".into(),
                source_index: export.source_index,
                name_loc: export.name_loc,
                alias: alias.clone(),
            }
        })
        .collect()
}

/// Match and bind all named imports in one file in deterministic symbol order.
/// Cycles and ambiguities are returned for the diagnostic layer.
///
/// # Panics
///
/// Panics when the source or its import symbols violate linker graph
/// invariants.
#[must_use]
#[allow(clippy::too_many_lines)]
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

        if result.is_missing {
            graph.symbols.get_mut(import_ref).import_item_status = ImportItemStatus::Missing;
            issues.push(ImportMatchIssue {
                import_ref,
                result: result.clone(),
                suggestion: None,
            });
        }

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
            MatchImportKind::NoMatch => {
                if graph.symbols.get(import_ref).import_item_status == ImportItemStatus::Generated {
                    graph.symbols.get_mut(import_ref).import_item_status =
                        ImportItemStatus::Missing;
                }
                let alias = {
                    let Some(InputFileRepr::Js(repr)) =
                        graph.files[source_index as usize].input_file.repr.as_ref()
                    else {
                        unreachable!();
                    };
                    repr.ast.named_imports[&import_ref].alias.clone()
                };
                let suggestion = maybe_correct_export_typo(graph, result.source_index, &alias);
                issues.push(ImportMatchIssue {
                    import_ref,
                    result,
                    suggestion,
                });
            }
            MatchImportKind::Cycle | MatchImportKind::Ambiguous => {
                issues.push(ImportMatchIssue {
                    import_ref,
                    result,
                    suggestion: None,
                });
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

/// Populate the synthetic namespace-export part for one JavaScript file.
///
/// This generates the export getter object, tracks the declaring parts for
/// each exported symbol, and initializes the namespace object when needed.
///
/// # Panics
///
/// Panics when source or runtime symbol references violate linker graph
/// invariants.
#[allow(clippy::too_many_lines)]
pub fn create_exports_for_file(
    graph: &mut LinkerGraph,
    source_index: u32,
    export_runtime_ref: Ref,
    entry_point_refs: Option<EntryPointTailRefs>,
    options: &Options,
) {
    let (aliases, resolved_exports, exports_ref, needs_exports_variable) = {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            panic!("export source must be JavaScript");
        };
        (
            repr.meta.sorted_and_filtered_export_aliases.clone(),
            repr.meta.resolved_exports.clone(),
            repr.ast.exports_ref,
            repr.meta.needs_exports_variable,
        )
    };
    let mut properties = Vec::with_capacity(aliases.len());
    let mut dependencies = Vec::new();
    let mut symbol_uses = HashMap::new();

    for alias in aliases {
        let mut export = resolved_exports[&alias].clone();
        let import_data = {
            let Some(InputFileRepr::Js(repr)) = graph.files[export.source_index as usize]
                .input_file
                .repr
                .as_ref()
            else {
                panic!("export target must be JavaScript");
            };
            repr.meta.imports_to_bind.get(&export.reference).cloned()
        };
        if let Some(import_data) = import_data {
            export.reference = import_data.reference;
            export.source_index = import_data.source_index;
            dependencies.extend(import_data.re_exports);
        }

        let value = if graph
            .symbols
            .get(export.reference)
            .namespace_alias
            .is_some()
        {
            js_ast::Expr::new(
                crate::internal::logger::Loc::default(),
                js_ast::ExprData::ImportIdentifier(js_ast::ImportIdentifierExpr {
                    reference: export.reference,
                    ..js_ast::ImportIdentifierExpr::default()
                }),
            )
        } else {
            identifier_expr(export.reference, crate::internal::logger::Loc::default())
        };
        let body = js_ast::FunctionBody {
            block: js_ast::BlockStmt {
                statements: vec![js_ast::Stmt::new(
                    value.loc,
                    js_ast::StmtData::Return(js_ast::ReturnStmt {
                        value_or_nil: value,
                    }),
                )],
                ..js_ast::BlockStmt::default()
            },
            ..js_ast::FunctionBody::default()
        };
        let getter = if options
            .unsupported_js_features
            .contains(crate::internal::compat::JsFeature::ARROW)
        {
            js_ast::Expr::new(
                crate::internal::logger::Loc::default(),
                js_ast::ExprData::Function(js_ast::FunctionExpr {
                    function: js_ast::Function {
                        body,
                        ..js_ast::Function::default()
                    },
                    ..js_ast::FunctionExpr::default()
                }),
            )
        } else {
            js_ast::Expr::new(
                crate::internal::logger::Loc::default(),
                js_ast::ExprData::Arrow(js_ast::ArrowExpr {
                    body,
                    prefer_expr: true,
                    ..js_ast::ArrowExpr::default()
                }),
            )
        };
        let flags = if alias == "__proto__"
            && !options
                .unsupported_js_features
                .contains(crate::internal::compat::JsFeature::OBJECT_EXTENSIONS)
        {
            js_ast::PropertyFlags::IS_COMPUTED
        } else {
            js_ast::PropertyFlags::NONE
        };
        properties.push(js_ast::Property {
            flags,
            key: js_ast::Expr::new(
                crate::internal::logger::Loc::default(),
                js_ast::ExprData::String(js_ast::StringExpr {
                    value: string_to_utf16(alias.as_bytes()),
                    ..js_ast::StringExpr::default()
                }),
            ),
            value_or_nil: getter,
            ..js_ast::Property::default()
        });
        symbol_uses.insert(export.reference, js_ast::SymbolUse { count_estimate: 1 });

        let Some(InputFileRepr::Js(repr)) = graph.files[export.source_index as usize]
            .input_file
            .repr
            .as_ref()
        else {
            panic!("export target must be JavaScript");
        };
        dependencies.extend(
            repr.top_level_symbol_to_parts(export.reference)
                .unwrap_or_default()
                .iter()
                .map(|&part_index| js_ast::Dependency {
                    source_index: export.source_index,
                    part_index,
                }),
        );
    }

    let mut declared_symbols = Vec::new();
    let mut statements = Vec::new();
    if needs_exports_variable {
        statements.push(js_ast::Stmt::new(
            crate::internal::logger::Loc::default(),
            js_ast::StmtData::Local(js_ast::LocalStmt {
                declarations: vec![js_ast::Decl {
                    binding: identifier_binding(exports_ref),
                    value_or_nil: js_ast::Expr::new(
                        crate::internal::logger::Loc::default(),
                        js_ast::ExprData::Object(js_ast::ObjectExpr::default()),
                    ),
                }],
                ..js_ast::LocalStmt::default()
            }),
        ));
        declared_symbols.push(js_ast::DeclaredSymbol {
            reference: exports_ref,
            is_top_level: true,
        });
    }

    let used_export_runtime = !properties.is_empty();
    if used_export_runtime {
        statements.push(expr_statement(call_with_args(
            export_runtime_ref,
            vec![
                identifier_expr(exports_ref, crate::internal::logger::Loc::default()),
                js_ast::Expr::new(
                    crate::internal::logger::Loc::default(),
                    js_ast::ExprData::Object(js_ast::ObjectExpr {
                        properties,
                        ..js_ast::ObjectExpr::default()
                    }),
                ),
            ],
            crate::internal::logger::Loc::default(),
        )));
        let Some(InputFileRepr::Js(runtime)) = graph.files
            [crate::internal::runtime::SOURCE_INDEX as usize]
            .input_file
            .repr
            .as_ref()
        else {
            panic!("runtime must be JavaScript");
        };
        dependencies.extend(
            runtime
                .top_level_symbol_to_parts(export_runtime_ref)
                .unwrap_or_default()
                .iter()
                .map(|&part_index| js_ast::Dependency {
                    source_index: crate::internal::runtime::SOURCE_INDEX,
                    part_index,
                }),
        );
    }

    let force_common_js_entry = {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            panic!("export source must be JavaScript");
        };
        repr.meta.force_include_exports_for_entry_point && options.output_format == Format::CommonJs
    };
    if force_common_js_entry {
        let refs = entry_point_refs
            .expect("CommonJS entry export generation requires runtime symbol references");
        statements.push(expr_statement(js_ast::assign(
            module_exports_expr(
                refs.unbound_module_ref,
                crate::internal::logger::Loc::default(),
            ),
            call_with_args(
                refs.to_common_js_ref,
                vec![identifier_expr(
                    exports_ref,
                    crate::internal::logger::Loc::default(),
                )],
                crate::internal::logger::Loc::default(),
            ),
        )));
    }

    if statements.is_empty() {
        return;
    }
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_mut()
    else {
        panic!("export source must be JavaScript");
    };
    repr.ast.parts[js_ast::NS_EXPORT_PART_INDEX as usize] = js_ast::Part {
        statements,
        symbol_uses,
        dependencies,
        declared_symbols,
        can_be_removed_if_unused: true,
        force_tree_shaking: true,
        ..js_ast::Part::default()
    };
    if used_export_runtime {
        repr.ast.uses_exports_ref = true;
        repr.meta.needs_export_symbol_from_runtime = true;
    }
}

/// Finalize per-part symbol uses and local dependency edges for one file.
///
/// This runs after imports and exports have been matched, so TypeScript enum
/// property accesses and calls to functions that will be inlined can be
/// excluded from reachability counts.
///
/// # Panics
///
/// Panics when source symbols, imported bindings, or part indexes violate
/// linker graph invariants.
#[allow(clippy::too_many_lines)]
pub fn finalize_part_dependencies_for_file(graph: &mut LinkerGraph, source_index: u32) {
    let repr = graph.files[source_index as usize]
        .input_file
        .repr
        .take()
        .expect("dependency source must have a representation");
    let InputFileRepr::Js(mut repr) = repr else {
        panic!("dependency source must be JavaScript");
    };
    let mut local_dependencies = HashMap::new();

    for part_index in 0..repr.ast.parts.len() {
        {
            let part = &mut repr.ast.parts[part_index];
            for (reference, properties) in &part.import_symbol_property_uses {
                let mut symbol_use = part.symbol_uses.get(reference).copied().unwrap_or_default();
                let mut handled_as_enum = false;
                if let Some(import_data) = repr.meta.imports_to_bind.get(reference)
                    && graph.symbols.get(import_data.reference).kind == SymbolKind::TsEnum
                    && let Some(enum_values) = graph.ts_enums.get(&import_data.reference)
                {
                    let mut found_non_inlined_enum = false;
                    for (name, property_use) in properties {
                        if !enum_values.contains_key(name) {
                            found_non_inlined_enum = true;
                            symbol_use.count_estimate += property_use.count_estimate;
                        }
                    }
                    if found_non_inlined_enum {
                        part.symbol_uses.insert(*reference, symbol_use);
                    }
                    handled_as_enum = true;
                }
                if !handled_as_enum {
                    for property_use in properties.values() {
                        symbol_use.count_estimate += property_use.count_estimate;
                    }
                    part.symbol_uses.insert(*reference, symbol_use);
                }
            }

            for (reference, call_use) in &part.symbol_call_uses {
                let mut symbol_use = part.symbol_uses.get(reference).copied().unwrap_or_default();
                let mut symbol = graph.symbols.get(*reference);
                if symbol.kind == SymbolKind::Import
                    && let Some(import_data) = repr.meta.imports_to_bind.get(reference)
                {
                    symbol = graph.symbols.get(import_data.reference);
                }
                let flags = symbol.flags;
                if flags.contains(crate::internal::ast::SymbolFlags::IS_EMPTY_FUNCTION)
                    && !flags
                        .contains(crate::internal::ast::SymbolFlags::COULD_POTENTIALLY_BE_MUTATED)
                {
                    continue;
                }
                let mut call_count = call_use.call_count_estimate;
                if flags.contains(crate::internal::ast::SymbolFlags::IS_IDENTITY_FUNCTION)
                    && !flags
                        .contains(crate::internal::ast::SymbolFlags::COULD_POTENTIALLY_BE_MUTATED)
                {
                    call_count = call_count
                        .checked_sub(call_use.single_arg_non_spread_call_count_estimate)
                        .expect("single-argument call count must not exceed total call count");
                    if call_count == 0 {
                        continue;
                    }
                }
                symbol_use.count_estimate += call_count;
                part.symbol_uses.insert(*reference, symbol_use);
            }
        }

        let references = repr.ast.parts[part_index]
            .symbol_uses
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for reference in references {
            if let Some(import_data) = repr.meta.imports_to_bind.get(&reference)
                && graph.const_values.contains_key(&import_data.reference)
            {
                repr.ast.parts[part_index]
                    .symbol_uses
                    .remove(&import_data.reference);
                continue;
            }

            let declaring_parts = repr
                .top_level_symbol_to_parts(reference)
                .unwrap_or_default()
                .to_vec();
            for declaring_part_index in declaring_parts {
                let current_part_index = u32::try_from(part_index).expect("part index fits in u32");
                if local_dependencies.get(&declaring_part_index) != Some(&current_part_index) {
                    local_dependencies.insert(declaring_part_index, current_part_index);
                    repr.ast.parts[part_index]
                        .dependencies
                        .push(js_ast::Dependency {
                            source_index,
                            part_index: declaring_part_index,
                        });
                }
            }

            if let Some(named_import) = repr.ast.named_imports.get_mut(&reference) {
                named_import
                    .local_parts_with_uses
                    .push(u32::try_from(part_index).expect("part index fits in u32"));
            }
        }
    }

    graph.files[source_index as usize].input_file.repr = Some(InputFileRepr::Js(repr));
}

/// Bind matched imports to their declaring parts and merge symbol identities.
///
/// This is the first half of upstream linker scan step 6.
///
/// # Panics
///
/// Panics when matched imports or their declaring parts violate linker graph
/// invariants.
pub fn bind_imports_to_parts_for_file(
    graph: &mut LinkerGraph,
    source_index: u32,
    export_runtime_ref: Ref,
) {
    let (needs_export_runtime, wrap, exports_kind, exports_ref, module_ref, wrapper_ref) = {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            panic!("import source must be JavaScript");
        };
        (
            repr.meta.needs_export_symbol_from_runtime,
            repr.meta.wrap,
            repr.ast.exports_kind,
            repr.ast.exports_ref,
            repr.ast.module_ref,
            repr.ast.wrapper_ref,
        )
    };
    if needs_export_runtime {
        graph.generate_symbol_import_and_use(
            source_index,
            js_ast::NS_EXPORT_PART_INDEX,
            export_runtime_ref,
            1,
            crate::internal::runtime::SOURCE_INDEX,
        );
    }

    let imports_to_bind = {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            unreachable!("import source was checked above");
        };
        repr.meta.imports_to_bind.clone()
    };
    for (import_ref, import_data) in imports_to_bind {
        let declaring_parts = {
            let Some(InputFileRepr::Js(repr)) = graph.files[import_data.source_index as usize]
                .input_file
                .repr
                .as_ref()
            else {
                panic!("import target must be JavaScript");
            };
            repr.top_level_symbol_to_parts(import_data.reference)
                .unwrap_or_default()
                .to_vec()
        };
        let local_parts_with_uses = {
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_ref()
            else {
                unreachable!("import source was checked above");
            };
            repr.ast
                .named_imports
                .get(&import_ref)
                .map(|named_import| named_import.local_parts_with_uses.clone())
                .unwrap_or_default()
        };
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_mut()
        else {
            unreachable!("import source was checked above");
        };
        for part_index in local_parts_with_uses {
            let part = &mut repr.ast.parts[part_index as usize];
            part.dependencies
                .extend(
                    declaring_parts
                        .iter()
                        .map(|&declaring_part_index| js_ast::Dependency {
                            source_index: import_data.source_index,
                            part_index: declaring_part_index,
                        }),
                );
            part.dependencies
                .extend(import_data.re_exports.iter().copied());
        }
        graph
            .symbols
            .merge_symbols(import_ref, import_data.reference);
    }

    let identifier_name = graph.files[source_index as usize]
        .input_file
        .source
        .identifier_name
        .clone();
    if wrap == WrapKind::Esm {
        graph.symbols.get_mut(wrapper_ref).original_name = format!("init_{identifier_name}");
    }
    if wrap != WrapKind::Cjs && exports_kind != ExportsKind::CommonJs {
        graph.symbols.get_mut(exports_ref).original_name = format!("{identifier_name}_exports");
        graph.symbols.get_mut(module_ref).original_name = format!("{identifier_name}_module");
    }
}

/// Pre-generate local symbols used to re-export `CommonJS` values from an ESM
/// entry point.
///
/// # Panics
///
/// Panics when the source lacks the parser module scope required for generated
/// symbols.
pub fn generate_common_js_export_copies(
    graph: &mut LinkerGraph,
    source_index: u32,
    options: &Options,
) {
    if !graph.files[source_index as usize].is_entry_point()
        || options.output_format != Format::EsModule
    {
        return;
    }
    let aliases = {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            panic!("entry point must be JavaScript");
        };
        repr.meta.sorted_and_filtered_export_aliases.clone()
    };
    let copies = aliases
        .into_iter()
        .map(|alias| {
            graph.generate_new_symbol(source_index, SymbolKind::Other, format!("export_{alias}"))
        })
        .collect();
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_mut()
    else {
        unreachable!("entry point was checked above");
    };
    repr.meta.cjs_export_copies = copies;
}

/// Create the synthetic part that keeps an entry point's public surface live.
///
/// # Panics
///
/// Panics when resolved exports, wrapper parts, or runtime references violate
/// linker graph invariants.
pub fn create_entry_point_part(graph: &mut LinkerGraph, source_index: u32, to_common_js_ref: Ref) {
    assert!(
        graph.files[source_index as usize].is_entry_point(),
        "entry point part requires an entry point"
    );
    let (aliases, resolved_exports, force_exports, wrap, wrapper_part_index) = {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            panic!("entry point must be JavaScript");
        };
        (
            repr.meta.sorted_and_filtered_export_aliases.clone(),
            repr.meta.resolved_exports.clone(),
            repr.meta.force_include_exports_for_entry_point,
            repr.meta.wrap,
            repr.meta.wrapper_part_index,
        )
    };
    let mut dependencies = Vec::new();
    for alias in aliases {
        let mut export = resolved_exports[&alias].clone();
        let import_data = {
            let Some(InputFileRepr::Js(repr)) = graph.files[export.source_index as usize]
                .input_file
                .repr
                .as_ref()
            else {
                panic!("entry export target must be JavaScript");
            };
            repr.meta.imports_to_bind.get(&export.reference).cloned()
        };
        if let Some(import_data) = import_data {
            export.source_index = import_data.source_index;
            export.reference = import_data.reference;
            dependencies.extend(import_data.re_exports);
        }
        let Some(InputFileRepr::Js(repr)) = graph.files[export.source_index as usize]
            .input_file
            .repr
            .as_ref()
        else {
            panic!("entry export target must be JavaScript");
        };
        dependencies.extend(
            repr.top_level_symbol_to_parts(export.reference)
                .unwrap_or_default()
                .iter()
                .map(|&part_index| js_ast::Dependency {
                    source_index: export.source_index,
                    part_index,
                }),
        );
    }
    if force_exports {
        dependencies.push(js_ast::Dependency {
            source_index,
            part_index: js_ast::NS_EXPORT_PART_INDEX,
        });
    }
    if wrap != WrapKind::None {
        dependencies.push(js_ast::Dependency {
            source_index,
            part_index: wrapper_part_index.get_index(),
        });
    }

    let part_index = graph.add_part_to_file(
        source_index,
        js_ast::Part {
            dependencies,
            can_be_removed_if_unused: false,
            ..js_ast::Part::default()
        },
    );
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_mut()
    else {
        unreachable!("entry point was checked above");
    };
    repr.meta.entry_point_part_index = Index32::new(part_index);
    if force_exports {
        graph.generate_symbol_import_and_use(
            source_index,
            part_index,
            to_common_js_ref,
            1,
            crate::internal::runtime::SOURCE_INDEX,
        );
    }
}

/// Encode wrapper and runtime-helper constraints for all imports in one file.
///
/// # Panics
///
/// Panics when import records or resolved targets violate linker graph
/// invariants.
#[allow(clippy::too_many_lines)]
pub fn encode_import_constraints_for_file(
    graph: &mut LinkerGraph,
    source_index: u32,
    options: &Options,
) {
    let (part_count, is_entry_point) = {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            panic!("import source must be JavaScript");
        };
        (
            repr.ast.parts.len(),
            graph.files[source_index as usize].is_entry_point(),
        )
    };

    for part_index in 0..part_count {
        let part_index_u32 = u32::try_from(part_index).expect("part index fits in u32");
        let import_record_indices = {
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_ref()
            else {
                unreachable!();
            };
            repr.ast.parts[part_index].import_record_indices.clone()
        };
        let mut to_esm_uses = 0;
        let mut to_common_js_uses = 0;
        let mut runtime_require_uses = 0;

        for record_index in import_record_indices {
            let record = {
                let Some(InputFileRepr::Js(repr)) =
                    graph.files[source_index as usize].input_file.repr.as_ref()
                else {
                    unreachable!();
                };
                repr.ast.import_records[record_index as usize].clone()
            };
            if !record.source_index.is_valid()
                || is_external_dynamic_import(graph, options, &record, source_index)
            {
                let becomes_require = record.kind == ImportKind::Require
                    || !options.output_format.keep_esm_import_export_syntax()
                    || (record.kind == ImportKind::Dynamic
                        && options
                            .unsupported_js_features
                            .contains(crate::internal::compat::JsFeature::DYNAMIC_IMPORT));
                if becomes_require {
                    let Some(InputFileRepr::Js(repr)) =
                        graph.files[source_index as usize].input_file.repr.as_mut()
                    else {
                        unreachable!();
                    };
                    let record = &mut repr.ast.import_records[record_index as usize];
                    if crate::internal::config::should_call_runtime_require(
                        options.mode,
                        options.output_format,
                    ) {
                        record.flags |= ImportRecordFlags::CALL_RUNTIME_REQUIRE;
                        runtime_require_uses += 1;
                    }
                    if record.kind != ImportKind::Require
                        && (record.kind != ImportKind::Stmt
                            || record
                                .flags
                                .contains(ImportRecordFlags::CONTAINS_IMPORT_STAR)
                            || record
                                .flags
                                .contains(ImportRecordFlags::CONTAINS_DEFAULT_ALIAS)
                            || record
                                .flags
                                .contains(ImportRecordFlags::CONTAINS_ES_MODULE_ALIAS))
                    {
                        record.flags |= ImportRecordFlags::WRAP_WITH_TO_ESM;
                        to_esm_uses += 1;
                    }
                }
                continue;
            }

            let other_source_index = record.source_index.get_index();
            let (other_wrap, other_kind, other_wrapper_ref, other_exports_ref) = {
                let Some(InputFileRepr::Js(other)) = graph.files[other_source_index as usize]
                    .input_file
                    .repr
                    .as_ref()
                else {
                    panic!("resolved import target must be JavaScript");
                };
                (
                    other.meta.wrap,
                    other.ast.exports_kind,
                    other.ast.wrapper_ref,
                    other.ast.exports_ref,
                )
            };
            if other_wrap != WrapKind::None {
                graph.generate_symbol_import_and_use(
                    source_index,
                    part_index_u32,
                    other_wrapper_ref,
                    1,
                    other_source_index,
                );
                if record.kind != ImportKind::Require && other_kind == ExportsKind::CommonJs {
                    let Some(InputFileRepr::Js(repr)) =
                        graph.files[source_index as usize].input_file.repr.as_mut()
                    else {
                        unreachable!();
                    };
                    repr.ast.import_records[record_index as usize].flags |=
                        ImportRecordFlags::WRAP_WITH_TO_ESM;
                    to_esm_uses += 1;
                }
                if other_wrap == WrapKind::Esm && record.kind != ImportKind::Stmt {
                    graph.generate_symbol_import_and_use(
                        source_index,
                        part_index_u32,
                        other_exports_ref,
                        1,
                        other_source_index,
                    );
                    if record.kind == ImportKind::Require {
                        let Some(InputFileRepr::Js(repr)) =
                            graph.files[source_index as usize].input_file.repr.as_mut()
                        else {
                            unreachable!();
                        };
                        repr.ast.import_records[record_index as usize].flags |=
                            ImportRecordFlags::WRAP_WITH_TO_CJS;
                        to_common_js_uses += 1;
                    }
                }
            } else if record.kind == ImportKind::Stmt
                && other_kind == ExportsKind::EsmWithDynamicFallback
            {
                graph.generate_symbol_import_and_use(
                    source_index,
                    part_index_u32,
                    other_exports_ref,
                    1,
                    other_source_index,
                );
            }
        }
        graph.generate_runtime_symbol_import_and_use(
            source_index,
            part_index_u32,
            "__toESM",
            to_esm_uses,
        );
        graph.generate_runtime_symbol_import_and_use(
            source_index,
            part_index_u32,
            "__toCommonJS",
            to_common_js_uses,
        );
        graph.generate_runtime_symbol_import_and_use(
            source_index,
            part_index_u32,
            "__require",
            runtime_require_uses,
        );

        let (export_stars, own_exports_ref) = {
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_ref()
            else {
                unreachable!();
            };
            (
                repr.ast.export_star_import_records.clone(),
                repr.ast.exports_ref,
            )
        };
        let mut re_export_uses = 0;
        for record_index in export_stars {
            let record = {
                let Some(InputFileRepr::Js(repr)) =
                    graph.files[source_index as usize].input_file.repr.as_ref()
                else {
                    unreachable!();
                };
                repr.ast.import_records[record_index as usize].clone()
            };
            let mut happens_at_run_time = !record.source_index.is_valid()
                && (!is_entry_point || !options.output_format.keep_esm_import_export_syntax());
            if record.source_index.is_valid() {
                let other_source_index = record.source_index.get_index();
                let (other_kind, other_exports_ref) = {
                    let Some(InputFileRepr::Js(other)) = graph.files[other_source_index as usize]
                        .input_file
                        .repr
                        .as_ref()
                    else {
                        panic!("resolved export-star target must be JavaScript");
                    };
                    (other.ast.exports_kind, other.ast.exports_ref)
                };
                if other_source_index != source_index && other_kind.is_dynamic() {
                    happens_at_run_time = true;
                }
                if other_kind == ExportsKind::EsmWithDynamicFallback {
                    graph.generate_symbol_import_and_use(
                        source_index,
                        part_index_u32,
                        other_exports_ref,
                        1,
                        other_source_index,
                    );
                }
            }
            if happens_at_run_time {
                graph.generate_symbol_import_and_use(
                    source_index,
                    part_index_u32,
                    own_exports_ref,
                    1,
                    source_index,
                );
                let Some(InputFileRepr::Js(repr)) =
                    graph.files[source_index as usize].input_file.repr.as_mut()
                else {
                    unreachable!();
                };
                repr.ast.import_records[record_index as usize].flags |=
                    ImportRecordFlags::CALLS_RUN_TIME_RE_EXPORT_FN;
                repr.ast.uses_exports_ref = true;
                re_export_uses += 1;
            }
        }
        graph.generate_runtime_symbol_import_and_use(
            source_index,
            part_index_u32,
            "__reExport",
            re_export_uses,
        );
    }
}

/// Run the ported linker import/export scan stages in upstream order.
///
/// Diagnostics are returned as structured issues for the API layer.
///
/// # Panics
///
/// Panics when the scanner-provided graph violates linker invariants.
#[allow(clippy::too_many_lines)]
pub fn scan_imports_and_exports<S: BuildHasher>(
    graph: &mut LinkerGraph,
    options: &Options,
    unique_key_prefix: &str,
    local_names: &HashMap<Ref, String, S>,
    runtime_refs: ScanRuntimeRefs,
) -> ScanImportsAndExportsResult {
    configure_entry_point_exports(graph, options);
    inline_linked_assets(graph, unique_key_prefix);
    classify_module_wrappers(graph, options);
    propagate_wrappers_and_dynamic_exports(graph, options);
    generate_code_for_lazy_exports(graph, options, local_names);
    resolve_export_stars(graph);

    let reachable = graph.reachable_files.clone();
    let mut result = ScanImportsAndExportsResult::default();
    for &source_index in &reachable {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            continue;
        };
        if !repr.ast.named_imports.is_empty() {
            result.import_issues.extend(
                bind_imports_to_exports_for_file(graph, source_index, options.output_format)
                    .into_iter()
                    .map(|issue| (source_index, issue)),
            );
        }

        let (exports_kind, wrap, exports_ref, module_ref, force_exports) = {
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_ref()
            else {
                unreachable!();
            };
            (
                repr.ast.exports_kind,
                repr.meta.wrap,
                repr.ast.exports_ref,
                repr.ast.module_ref,
                repr.meta.force_include_exports_for_entry_point,
            )
        };
        if graph.files[source_index as usize].is_entry_point()
            && exports_kind == ExportsKind::CommonJs
            && wrap == WrapKind::None
            && matches!(options.output_format, Format::Preserve | Format::CommonJs)
        {
            let exports_ref = graph.symbols.follow_symbols(exports_ref);
            let module_ref = graph.symbols.follow_symbols(module_ref);
            graph.symbols.get_mut(exports_ref).kind = SymbolKind::Unbound;
            graph.symbols.get_mut(module_ref).kind = SymbolKind::Unbound;
        } else if force_exports || exports_kind != ExportsKind::CommonJs {
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_mut()
            else {
                unreachable!();
            };
            repr.meta.needs_exports_variable = true;
        }
        create_wrapper_for_file(
            graph,
            source_index,
            runtime_refs.common_js_ref,
            runtime_refs.esm_ref,
        );
    }

    for &source_index in &reachable {
        if !matches!(
            graph.files[source_index as usize].input_file.repr.as_ref(),
            Some(InputFileRepr::Js(_))
        ) {
            continue;
        }
        result.ambiguous_re_exports.extend(
            sort_and_filter_export_aliases(graph, source_index)
                .into_iter()
                .map(|issue| (source_index, issue)),
        );
        result
            .arbitrary_namespace_issues
            .extend(arbitrary_namespace_export_issues(
                graph,
                source_index,
                options,
            ));
        create_exports_for_file(
            graph,
            source_index,
            runtime_refs.export_ref,
            Some(EntryPointTailRefs {
                to_common_js_ref: runtime_refs.to_common_js_ref,
                unbound_module_ref: runtime_refs.unbound_module_ref,
            }),
            options,
        );
        finalize_part_dependencies_for_file(graph, source_index);
    }

    for &source_index in &reachable {
        if !matches!(
            graph.files[source_index as usize].input_file.repr.as_ref(),
            Some(InputFileRepr::Js(_))
        ) {
            continue;
        }
        generate_common_js_export_copies(graph, source_index, options);
        bind_imports_to_parts_for_file(graph, source_index, runtime_refs.export_ref);
        if graph.files[source_index as usize].is_entry_point() {
            create_entry_point_part(graph, source_index, runtime_refs.to_common_js_ref);
        }
        encode_import_constraints_for_file(graph, source_index, options);
    }
    result
}

/// Clone scanner output and run the ordered linker preparation stages through
/// cross-chunk statement generation.
///
/// # Panics
///
/// Panics when scanner output violates linker invariants or the parsed runtime
/// module is missing required exports.
#[must_use]
pub fn prepare_linker_graph<S: BuildHasher>(
    input_files: &[InputFile],
    reachable_files: &[u32],
    entry_points: &[EntryPoint],
    options: &Options,
    unique_key_prefix: &str,
    local_names: &HashMap<Ref, String, S>,
) -> PreparedLinkerGraph {
    let mut graph = clone_linker_graph(
        input_files,
        reachable_files,
        entry_points,
        options.code_splitting,
    );
    let unbound_module_ref = if options.output_format == Format::CommonJs {
        graph.generate_new_symbol(
            crate::internal::runtime::SOURCE_INDEX,
            SymbolKind::Unbound,
            "module",
        )
    } else {
        INVALID_REF
    };
    let runtime_refs = scan_runtime_refs_from_graph(&graph, unbound_module_ref);
    let scan_result = scan_imports_and_exports(
        &mut graph,
        options,
        unique_key_prefix,
        local_names,
        runtime_refs,
    );
    tree_shaking_and_code_splitting(&mut graph, options);
    if options.mode == Mode::PassThrough {
        for entry_point in graph.entry_points().to_vec() {
            prevent_exports_from_being_renamed(&mut graph, entry_point.source_index);
        }
    }
    let mut chunks = compute_chunks(&mut graph, options, unique_key_prefix);
    compute_cross_chunk_dependencies(&mut graph, &mut chunks, options);
    if options.code_splitting {
        generate_cross_chunk_stmts(&graph, &mut chunks, options);
    }
    PreparedLinkerGraph {
        graph,
        chunks,
        scan_result,
        unbound_module_ref,
    }
}

/// Preserve externally observable symbol names in pass-through mode.
///
/// Explicit exports are preserved for modules. Script-style files preserve all
/// module-scope bindings because they may be observed from a classic script
/// tag.
pub fn prevent_exports_from_being_renamed(graph: &mut LinkerGraph, source_index: u32) {
    let Some(repr) = graph.files[source_index as usize].input_file.repr.take() else {
        return;
    };
    let InputFileRepr::Js(mut repr) = repr else {
        graph.files[source_index as usize].input_file.repr = Some(repr);
        return;
    };
    let mut has_import_or_export = false;
    for part in &mut repr.ast.parts {
        for statement in &mut part.statements {
            match statement.data.as_deref_mut() {
                Some(js_ast::StmtData::Import(import)) => {
                    if !repr.ast.import_records[import.import_record_index as usize]
                        .source_index
                        .is_valid()
                    {
                        has_import_or_export = true;
                    }
                }
                Some(js_ast::StmtData::Local(local)) if local.is_export => {
                    js_ast::for_each_identifier_binding_in_decls(
                        &mut local.declarations,
                        &mut |_location, binding| {
                            graph.symbols.get_mut(binding.reference).flags |=
                                crate::internal::ast::SymbolFlags::MUST_NOT_BE_RENAMED;
                        },
                    );
                    has_import_or_export = true;
                }
                Some(js_ast::StmtData::Function(function)) if function.is_export => {
                    if let Some(name) = function.function.name {
                        graph.symbols.get_mut(name.reference).kind = SymbolKind::Unbound;
                    }
                    has_import_or_export = true;
                }
                Some(js_ast::StmtData::Class(class)) if class.is_export => {
                    if let Some(name) = class.class.name {
                        graph.symbols.get_mut(name.reference).kind = SymbolKind::Unbound;
                    }
                    has_import_or_export = true;
                }
                Some(
                    js_ast::StmtData::ExportClause(_)
                    | js_ast::StmtData::ExportDefault(_)
                    | js_ast::StmtData::ExportStar(_)
                    | js_ast::StmtData::ExportFrom(_),
                ) => {
                    has_import_or_export = true;
                }
                _ => {}
            }
        }
    }
    if !has_import_or_export && let Some(scope) = &repr.ast.module_scope {
        for member in scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .values()
        {
            graph.symbols.get_mut(member.reference).flags |=
                crate::internal::ast::SymbolFlags::MUST_NOT_BE_RENAMED;
        }
    }
    graph.files[source_index as usize].input_file.repr = Some(InputFileRepr::Js(repr));
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
    if options.mode == Mode::PassThrough {
        for entry_point in entry_points {
            prevent_exports_from_being_renamed(graph, entry_point.source_index);
        }
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

/// Compute JavaScript chunks and their CSS companion chunks together.
///
/// CSS imported from JavaScript is modeled as a secondary entry-point chunk
/// sharing the JavaScript chunk's entry bits. Direct CSS entry points instead
/// own their primary chunk.
///
/// # Panics
///
/// Panics when graph entry points or reachable files violate linker
/// invariants.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn compute_chunks(
    graph: &mut LinkerGraph,
    options: &Options,
    unique_key_prefix: &str,
) -> Vec<ChunkInfo> {
    let entry_points = graph.entry_points().to_vec();
    let mut js_chunks = HashMap::<Vec<u8>, ChunkInfo>::new();
    let mut css_chunks = HashMap::<Vec<u8>, ChunkInfo>::new();

    for (entry_point_bit, entry_point) in entry_points.iter().enumerate() {
        let mut entry_bits = BitSet::new(entry_points.len());
        entry_bits.set_bit(entry_point_bit);
        let key = entry_bits.as_bytes().to_vec();
        match graph.files[entry_point.source_index as usize]
            .input_file
            .repr
            .as_ref()
        {
            Some(InputFileRepr::Js(_)) => {
                js_chunks.insert(
                    key.clone(),
                    ChunkInfo {
                        entry_bits: entry_bits.clone(),
                        is_entry_point: true,
                        source_index: entry_point.source_index,
                        entry_point_bit,
                        ..ChunkInfo::default()
                    },
                );

                let css_source_indices =
                    find_imported_css_files_in_js_order(graph, entry_point.source_index);
                if !css_source_indices.is_empty() {
                    let imports_in_css_order =
                        find_imported_files_in_css_order(graph, &css_source_indices);
                    let files_with_parts_in_chunk = imports_in_css_order
                        .iter()
                        .filter_map(|entry| {
                            (entry.kind == CssImportKind::SourceIndex).then_some(entry.source_index)
                        })
                        .collect();
                    css_chunks.insert(
                        key,
                        ChunkInfo {
                            entry_bits,
                            files_with_parts_in_chunk,
                            is_entry_point: true,
                            source_index: entry_point.source_index,
                            entry_point_bit,
                            is_css: true,
                            imports_in_css_order,
                            ..ChunkInfo::default()
                        },
                    );
                }
            }
            Some(InputFileRepr::Css(_)) => {
                let imports_in_css_order =
                    find_imported_files_in_css_order(graph, &[entry_point.source_index]);
                let files_with_parts_in_chunk = imports_in_css_order
                    .iter()
                    .filter_map(|entry| {
                        (entry.kind == CssImportKind::SourceIndex).then_some(entry.source_index)
                    })
                    .collect();
                css_chunks.insert(
                    key,
                    ChunkInfo {
                        entry_bits,
                        files_with_parts_in_chunk,
                        is_entry_point: true,
                        source_index: entry_point.source_index,
                        entry_point_bit,
                        is_css: true,
                        imports_in_css_order,
                        ..ChunkInfo::default()
                    },
                );
            }
            Some(InputFileRepr::Copy(_)) | None => {}
        }
    }

    for source_index in graph.reachable_files.clone() {
        let file = &graph.files[source_index as usize];
        if file.is_live && matches!(file.input_file.repr.as_ref(), Some(InputFileRepr::Js(_))) {
            let key = file.entry_bits.as_bytes().to_vec();
            js_chunks
                .entry(key)
                .or_insert_with(|| ChunkInfo {
                    entry_bits: file.entry_bits.clone(),
                    ..ChunkInfo::default()
                })
                .files_with_parts_in_chunk
                .insert(source_index);
        }
    }

    let mut js_chunks = js_chunks.into_iter().collect::<Vec<_>>();
    js_chunks.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut chunks = Vec::with_capacity(js_chunks.len() + css_chunks.len());
    let mut js_chunk_indices_for_css = HashMap::<Vec<u8>, usize>::new();
    for (key, chunk) in js_chunks {
        if css_chunks.contains_key(&key) {
            js_chunk_indices_for_css.insert(key, chunks.len());
        }
        chunks.push(chunk);
    }

    let mut css_chunks = css_chunks.into_iter().collect::<Vec<_>>();
    css_chunks.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for (key, chunk) in css_chunks {
        if let Some(&js_chunk_index) = js_chunk_indices_for_css.get(&key) {
            chunks[js_chunk_index].css_chunk_index =
                Index32::new(u32::try_from(chunks.len()).expect("chunk count fits in u32"));
        }
        chunks.push(chunk);
    }

    for (chunk_index, chunk) in chunks.iter().enumerate() {
        if chunk.is_entry_point {
            let is_secondary_css_chunk = chunk.is_css
                && matches!(
                    graph.files[chunk.source_index as usize]
                        .input_file
                        .repr
                        .as_ref(),
                    Some(InputFileRepr::Js(_))
                );
            if !is_secondary_css_chunk {
                graph.files[chunk.source_index as usize].entry_point_chunk_index =
                    u32::try_from(chunk_index).expect("chunk index fits in u32");
            }
        }
    }

    for chunk in &mut chunks {
        if !chunk.is_css {
            (chunk.files_in_chunk_in_order, chunk.parts_in_chunk_in_order) =
                find_imported_parts_in_js_order(graph, options, chunk);
        }
    }
    for (chunk_index, chunk) in chunks.iter_mut().enumerate() {
        chunk.unique_key = format!("{unique_key_prefix}C{chunk_index:08}");
    }
    chunks
}

/// Merge and assign minified names to JavaScript property symbols.
///
/// Existing cache entries are preserved and reserved names are never reused.
///
/// # Panics
///
/// Panics when graph source or stable-index tables are inconsistent, or when
/// a mangle-cache value is neither a boolean nor a string.
pub fn mangle_props(
    graph: &mut LinkerGraph,
    mangle_cache: &mut crate::internal::config::MangleCache,
) -> HashMap<Ref, String> {
    let mut reserved_props = crate::internal::js_lexer::KEYWORDS
        .iter()
        .map(|keyword| ((*keyword).to_string(), true))
        .collect::<HashMap<_, _>>();
    for (original, remapped) in mangle_cache.iter() {
        if let Some(value) = remapped.downcast_ref::<bool>() {
            if !*value {
                reserved_props.insert(original.clone(), true);
            }
        } else if let Some(value) = remapped.downcast_ref::<String>() {
            reserved_props.insert(value.clone(), true);
        } else {
            panic!("mangle cache values must be booleans or strings");
        }
    }

    let mut frequency = CharFreq::default();
    let mut merged_props = HashMap::<String, Ref>::new();
    for source_index in graph.reachable_files.clone() {
        if source_index == crate::internal::runtime::SOURCE_INDEX {
            continue;
        }
        let (reserved, mangled, char_freq) = {
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_ref()
            else {
                continue;
            };
            (
                repr.ast.reserved_props.keys().cloned().collect::<Vec<_>>(),
                repr.ast.mangled_props.clone(),
                repr.ast.char_freq,
            )
        };
        for property in reserved {
            reserved_props.insert(property, true);
        }
        for (name, reference) in mangled {
            if let Some(existing) = merged_props.get(&name).copied() {
                let canonical = graph.symbols.merge_symbols(reference, existing);
                merged_props.insert(name, canonical);
            } else {
                merged_props.insert(name, reference);
            }
        }
        if let Some(char_freq) = char_freq {
            frequency.include(&char_freq);
        }
    }

    let mut sorted = merged_props
        .into_values()
        .map(|reference| crate::internal::renamer::StableSymbolCount {
            stable_source_index: graph.stable_source_indices[reference.source_index as usize],
            count: graph.symbols.get(reference).use_count_estimate,
            reference,
        })
        .collect::<Vec<_>>();
    crate::internal::renamer::sort_stable_symbol_counts(&mut sorted);

    let minifier = DEFAULT_NAME_MINIFIER_JS.shuffle_by_char_freq(frequency);
    let mut next_name = 0;
    let mut result = HashMap::new();
    for symbol_count in sorted {
        let original_name = graph
            .symbols
            .get(symbol_count.reference)
            .original_name
            .clone();
        if let Some(existing) = mangle_cache.get(&original_name) {
            if let Some(existing) = existing.downcast_ref::<String>() {
                result.insert(symbol_count.reference, existing.clone());
            } else if existing.downcast_ref::<bool>().is_none() {
                panic!("mangle cache values must be booleans or strings");
            }
            continue;
        }

        let mut name = minifier.number_to_minified_name(next_name);
        next_name += 1;
        while reserved_props.contains_key(&name) {
            name = minifier.number_to_minified_name(next_name);
            next_name += 1;
        }
        mangle_cache.insert(original_name, std::sync::Arc::new(name.clone()));
        result.insert(symbol_count.reference, name);
    }
    result
}

/// Assign globally collision-free names to all local CSS symbols.
///
/// # Panics
///
/// Panics if graph source, symbol, or stable-index tables are inconsistent or
/// contain more than `u32::MAX` symbols for one source.
#[must_use]
pub fn mangle_local_css<S: BuildHasher>(
    graph: &LinkerGraph,
    options: &Options,
    used_local_names: &mut HashSet<String, S>,
) -> HashMap<Ref, String> {
    let mut global_names = HashSet::<String>::new();
    let mut local_names = HashSet::<Ref>::new();
    let mut frequency = CharFreq::default();

    for &source_index in &graph.reachable_files {
        let Some(InputFileRepr::Css(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            continue;
        };
        for (inner_index, symbol) in graph.symbols.symbols_for_source[source_index as usize]
            .iter()
            .enumerate()
        {
            if symbol.kind == SymbolKind::GlobalCss {
                global_names.insert(symbol.original_name.clone());
            } else {
                local_names.insert(graph.symbols.follow_symbols_const(Ref {
                    source_index,
                    inner_index: u32::try_from(inner_index).expect("symbol index fits in u32"),
                }));
            }
        }
        if let Some(char_freq) = repr.ast.char_freq {
            frequency.include(&char_freq);
        }
    }

    let mut sorted = local_names
        .into_iter()
        .map(|reference| crate::internal::renamer::StableSymbolCount {
            stable_source_index: graph.stable_source_indices[reference.source_index as usize],
            count: graph.symbols.get(reference).use_count_estimate,
            reference,
        })
        .collect::<Vec<_>>();
    crate::internal::renamer::sort_stable_symbol_counts(&mut sorted);

    let mut result = HashMap::new();
    if options.minify_identifiers {
        let minifier = DEFAULT_NAME_MINIFIER_CSS.shuffle_by_char_freq(frequency);
        let mut next_name = 0;
        for symbol in sorted {
            let mut name = minifier.number_to_minified_name(next_name);
            while global_names.contains(&name) || used_local_names.contains(&name) {
                next_name += 1;
                name = minifier.number_to_minified_name(next_name);
            }
            used_local_names.insert(name.clone());
            result.insert(symbol.reference, name);
        }
    } else {
        let mut name_counts = HashMap::<String, u32>::new();
        for symbol in sorted {
            let original_name = &graph.symbols.get(symbol.reference).original_name;
            let identifier_name = &graph.files[symbol.reference.source_index as usize]
                .input_file
                .source
                .identifier_name;
            let mut name = format!("{identifier_name}_{original_name}");
            if global_names.contains(&name) || used_local_names.contains(&name) {
                let prefix = name.clone();
                let counter = name_counts.entry(prefix.clone()).or_insert(1);
                loop {
                    *counter += 1;
                    name = format!("{prefix}{counter}");
                    if !global_names.contains(&name) && !used_local_names.contains(&name) {
                        break;
                    }
                }
            }
            used_local_names.insert(name.clone());
            result.insert(symbol.reference, name);
        }
    }
    result
}

/// Build the JavaScript object exported by a local-CSS JavaScript stub.
///
/// # Panics
///
/// Panics if the CSS graph, compose records, symbol table, or local scopes are
/// internally inconsistent.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn generate_css_module_exports<S: BuildHasher>(
    graph: &LinkerGraph,
    css_source_index: u32,
    local_names: &HashMap<Ref, String, S>,
) -> js_ast::Expr {
    fn final_name<S: BuildHasher>(
        graph: &LinkerGraph,
        local_names: &HashMap<Ref, String, S>,
        reference: Ref,
    ) -> String {
        let reference = graph.symbols.follow_symbols_const(reference);
        local_names
            .get(&reference)
            .cloned()
            .unwrap_or_else(|| graph.symbols.get(reference).original_name.clone())
    }

    fn visit_name<S: BuildHasher>(
        graph: &LinkerGraph,
        local_names: &HashMap<Ref, String, S>,
        repr: &crate::internal::graph::CssRepr,
        reference: Ref,
        visited: &mut HashSet<Ref>,
        names: &mut Vec<String>,
    ) {
        if visited.insert(reference) {
            visit_composes(graph, local_names, repr, reference, visited, names);
            names.push(final_name(graph, local_names, reference));
        }
    }

    fn visit_composes<S: BuildHasher>(
        graph: &LinkerGraph,
        local_names: &HashMap<Ref, String, S>,
        repr: &crate::internal::graph::CssRepr,
        reference: Ref,
        visited: &mut HashSet<Ref>,
        names: &mut Vec<String>,
    ) {
        let Some(composes) = repr.ast.composes.get(&reference) else {
            return;
        };
        for imported in &composes.imported_names {
            let record = &repr.ast.import_records[imported.import_record_index as usize];
            if record.source_index.is_valid() {
                let Some(InputFileRepr::Css(other_repr)) = graph.files
                    [record.source_index.get_index() as usize]
                    .input_file
                    .repr
                    .as_ref()
                else {
                    continue;
                };
                if let Some(other_name) = other_repr.ast.local_scope.get(&imported.alias) {
                    visit_name(
                        graph,
                        local_names,
                        other_repr,
                        other_name.reference,
                        visited,
                        names,
                    );
                }
            }
        }
        for local in &composes.names {
            visit_name(graph, local_names, repr, local.reference, visited, names);
        }
    }

    let Some(InputFileRepr::Css(repr)) = graph.files[css_source_index as usize]
        .input_file
        .repr
        .as_ref()
    else {
        panic!("CSS module export generation requires a CSS file");
    };
    let mut properties = Vec::with_capacity(repr.ast.local_symbols.len());
    for local in &repr.ast.local_symbols {
        let mut visited = HashSet::from([local.reference]);
        let mut names = Vec::new();
        visit_composes(
            graph,
            local_names,
            repr,
            local.reference,
            &mut visited,
            &mut names,
        );
        names.push(final_name(graph, local_names, local.reference));
        let original_name = graph.symbols.get(local.reference).original_name.clone();
        properties.push(js_ast::Property {
            key: js_ast::Expr::new(
                local.loc,
                js_ast::ExprData::String(js_ast::StringExpr {
                    value: string_to_utf16(original_name.as_bytes()),
                    ..js_ast::StringExpr::default()
                }),
            ),
            value_or_nil: js_ast::Expr::new(
                local.loc,
                js_ast::ExprData::String(js_ast::StringExpr {
                    value: string_to_utf16(names.join(" ").as_bytes()),
                    ..js_ast::StringExpr::default()
                }),
            ),
            ..js_ast::Property::default()
        });
    }
    js_ast::Expr::new(
        crate::internal::logger::Loc::default(),
        js_ast::ExprData::Object(js_ast::ObjectExpr {
            properties,
            ..js_ast::ObjectExpr::default()
        }),
    )
}

/// Replace a CSS JavaScript stub's lazy value with its final CSS-module object.
///
/// # Panics
///
/// Panics if the source is not a CSS JavaScript stub with the expected lazy
/// export statement shape.
pub fn populate_css_stub_lazy_export<S: BuildHasher>(
    graph: &mut LinkerGraph,
    stub_source_index: u32,
    local_names: &HashMap<Ref, String, S>,
) {
    let css_source_index = match graph.files[stub_source_index as usize]
        .input_file
        .repr
        .as_ref()
    {
        Some(InputFileRepr::Js(repr)) if repr.css_source_index.is_valid() => {
            repr.css_source_index.get_index()
        }
        _ => panic!("CSS JavaScript stub is missing its CSS source"),
    };
    let value = generate_css_module_exports(graph, css_source_index, local_names);
    let Some(InputFileRepr::Js(repr)) = graph.files[stub_source_index as usize]
        .input_file
        .repr
        .as_mut()
    else {
        unreachable!("CSS JavaScript stub representation was checked above");
    };
    let part = repr.ast.parts.last_mut().expect("CSS stub has a lazy part");
    let [statement] = part.statements.as_mut_slice() else {
        panic!("CSS stub lazy part must contain one statement");
    };
    let Some(js_ast::StmtData::LazyExport(export)) = statement.data.as_deref_mut() else {
        panic!("CSS stub must end in a lazy export");
    };
    export.value = value;
}

/// Lower a `CommonJS` lazy export to `module.exports = value`.
///
/// CSS stubs are populated with their CSS-module object immediately before
/// lowering.
///
/// # Panics
///
/// Panics if the source is not a `CommonJS` lazy-export JavaScript file with the
/// expected part shape.
pub fn lower_common_js_lazy_export<S: BuildHasher>(
    graph: &mut LinkerGraph,
    source_index: u32,
    local_names: &HashMap<Ref, String, S>,
) {
    let should_populate_css = matches!(
        graph.files[source_index as usize]
            .input_file
            .repr
            .as_ref(),
        Some(InputFileRepr::Js(repr)) if repr.css_source_index.is_valid()
    );
    if should_populate_css {
        populate_css_stub_lazy_export(graph, source_index, local_names);
    }

    let (module_ref, value) = {
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_mut()
        else {
            panic!("lazy export lowering requires JavaScript");
        };
        assert_eq!(
            repr.ast.exports_kind,
            ExportsKind::CommonJs,
            "CommonJS lazy export lowering requires CommonJS exports"
        );
        let module_ref = repr.ast.module_ref;
        let part = repr.ast.parts.last_mut().expect("lazy export has a part");
        let [statement] = part.statements.as_mut_slice() else {
            panic!("lazy export part must contain one statement");
        };
        let Some(js_ast::StmtData::LazyExport(export)) = statement.data.as_deref_mut() else {
            panic!("lazy export part must contain a lazy export");
        };
        (module_ref, std::mem::take(&mut export.value))
    };
    let location = value.loc;
    let assignment = expr_statement(js_ast::assign(
        module_exports_expr(module_ref, location),
        value,
    ));
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_mut()
    else {
        unreachable!("lazy export representation was checked above");
    };
    repr.ast
        .parts
        .last_mut()
        .expect("lazy export has a part")
        .statements = vec![assignment];
    graph.generate_symbol_import_and_use(source_index, 0, module_ref, 1, source_index);
}

fn generate_esm_lazy_export(
    graph: &mut LinkerGraph,
    source_index: u32,
    module_ref: Ref,
    location: crate::internal::logger::Loc,
    name: String,
    alias: String,
) -> (Ref, u32) {
    let reference = graph.generate_new_symbol(source_index, SymbolKind::Other, name);
    let part_index = graph.add_part_to_file(
        source_index,
        js_ast::Part {
            declared_symbols: vec![js_ast::DeclaredSymbol {
                reference,
                is_top_level: true,
            }],
            can_be_removed_if_unused: true,
            ..js_ast::Part::default()
        },
    );
    graph.generate_symbol_import_and_use(source_index, part_index, module_ref, 1, source_index);
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_mut()
    else {
        unreachable!("lazy export lowering requires JavaScript");
    };
    repr.meta
        .top_level_symbol_to_parts_overlay
        .insert(reference, vec![part_index]);
    repr.meta.resolved_exports.insert(
        alias,
        ExportData {
            reference,
            name_loc: location,
            source_index,
            ..ExportData::default()
        },
    );
    (reference, part_index)
}

/// Lower a non-`CommonJS` lazy export to tree-shakeable ES module exports.
///
/// Object properties become independent named exports where allowed, while the
/// original lazy value is retained as the default export. CSS stubs are
/// populated immediately before lowering.
///
/// # Panics
///
/// Panics if the source is not a non-`CommonJS` lazy-export JavaScript file
/// with the expected part shape.
#[allow(clippy::too_many_lines)]
pub fn lower_esm_lazy_export<S: BuildHasher>(
    graph: &mut LinkerGraph,
    source_index: u32,
    options: &Options,
    local_names: &HashMap<Ref, String, S>,
) {
    let should_populate_css = matches!(
        graph.files[source_index as usize]
            .input_file
            .repr
            .as_ref(),
        Some(InputFileRepr::Js(repr)) if repr.css_source_index.is_valid()
    );
    if should_populate_css {
        populate_css_stub_lazy_export(graph, source_index, local_names);
    }

    let (module_ref, loader, identifier_name, is_entry_point, lazy_value) = {
        let file = &mut graph.files[source_index as usize];
        let loader = file.input_file.loader;
        let identifier_name = file.input_file.source.identifier_name.clone();
        let is_entry_point = file.is_entry_point();
        let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_mut() else {
            panic!("lazy export lowering requires JavaScript");
        };
        assert_ne!(
            repr.ast.exports_kind,
            ExportsKind::CommonJs,
            "ES module lazy export lowering requires non-CommonJS exports"
        );
        let module_ref = repr.ast.module_ref;
        let part = repr.ast.parts.last_mut().expect("lazy export has a part");
        let [statement] = part.statements.as_mut_slice() else {
            panic!("lazy export part must contain one statement");
        };
        let Some(js_ast::StmtData::LazyExport(export)) = statement.data.as_deref_mut() else {
            panic!("lazy export part must contain a lazy export");
        };
        let lazy_value = std::mem::take(&mut export.value);
        part.statements.clear();
        (
            module_ref,
            loader,
            identifier_name,
            is_entry_point,
            lazy_value,
        )
    };

    if loader != Loader::WithTypeJson
        && let Some(js_ast::ExprData::Object(object)) = lazy_value.data.as_deref()
    {
        for property in &object.properties {
            let Some(js_ast::ExprData::String(key)) = property.key.data.as_deref() else {
                continue;
            };
            if is_entry_point
                && !js_ast::is_identifier_utf16(&key.value)
                && options
                    .unsupported_js_features
                    .contains(crate::internal::compat::JsFeature::ARBITRARY_MODULE_NAMESPACE_NAMES)
            {
                continue;
            }
            let name = String::from_utf8_lossy(&utf16_to_string(&key.value)).into_owned();
            if name == "default" {
                continue;
            }
            let (reference, part_index) = generate_esm_lazy_export(
                graph,
                source_index,
                module_ref,
                property.key.loc,
                name.clone(),
                name,
            );
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_mut()
            else {
                unreachable!("lazy export lowering requires JavaScript");
            };
            repr.ast.parts[part_index as usize].statements = vec![js_ast::Stmt::new(
                property.key.loc,
                js_ast::StmtData::Local(js_ast::LocalStmt {
                    declarations: vec![js_ast::Decl {
                        binding: identifier_binding(reference),
                        value_or_nil: property.value_or_nil.clone(),
                    }],
                    is_export: true,
                    ..js_ast::LocalStmt::default()
                }),
            )];
        }
    }

    let location = lazy_value.loc;
    let (reference, part_index) = generate_esm_lazy_export(
        graph,
        source_index,
        module_ref,
        location,
        format!("{identifier_name}_default"),
        "default".into(),
    );
    let Some(InputFileRepr::Js(repr)) = graph.files[source_index as usize].input_file.repr.as_mut()
    else {
        unreachable!("lazy export lowering requires JavaScript");
    };
    repr.ast.parts[part_index as usize].statements = vec![js_ast::Stmt::new(
        location,
        js_ast::StmtData::ExportDefault(js_ast::ExportDefaultStmt {
            default_name: LocRef {
                loc: location,
                reference,
            },
            value: expr_statement(lazy_value),
        }),
    )];
}

/// Generate code for all expression-style loaders in linker scan phase 3.
///
/// This must run after module wrapper classification finalizes each file's
/// export kind and before export-star resolution or import binding.
///
/// # Panics
///
/// Panics if a reachable lazy-export file violates JavaScript linker graph
/// invariants.
pub fn generate_code_for_lazy_exports<S: BuildHasher>(
    graph: &mut LinkerGraph,
    options: &Options,
    local_names: &HashMap<Ref, String, S>,
) {
    let lazy_exports = graph
        .reachable_files
        .iter()
        .filter_map(|&source_index| {
            let Some(InputFileRepr::Js(repr)) =
                graph.files[source_index as usize].input_file.repr.as_ref()
            else {
                return None;
            };
            repr.ast
                .has_lazy_export
                .then_some((source_index, repr.ast.exports_kind))
        })
        .collect::<Vec<_>>();

    for (source_index, exports_kind) in lazy_exports {
        if exports_kind == ExportsKind::CommonJs {
            lower_common_js_lazy_export(graph, source_index, local_names);
        } else {
            lower_esm_lazy_export(graph, source_index, options, local_names);
        }
    }
}

/// Find CSS companion files reachable from a JavaScript entry point.
///
/// JavaScript dependencies are traversed once in depth-first postorder, which
/// mirrors JavaScript module evaluation order before top-level await.
///
/// # Panics
///
/// Panics when a reachable file in the JavaScript dependency graph is not
/// represented as JavaScript or an import source index is out of bounds.
#[must_use]
pub fn find_imported_css_files_in_js_order(graph: &LinkerGraph, entry_point: u32) -> Vec<u32> {
    fn visit(
        graph: &LinkerGraph,
        source_index: u32,
        visited: &mut HashSet<u32>,
        order: &mut Vec<u32>,
    ) {
        if !visited.insert(source_index) {
            return;
        }
        let Some(InputFileRepr::Js(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            panic!("JavaScript CSS discovery reached a non-JavaScript file");
        };
        for part in &repr.ast.parts {
            for &import_record_index in &part.import_record_indices {
                let record = &repr.ast.import_records[import_record_index as usize];
                if record.source_index.is_valid() {
                    visit(graph, record.source_index.get_index(), visited, order);
                }
            }
        }
        if repr.css_source_index.is_valid() {
            order.push(repr.css_source_index.get_index());
        }
    }

    let mut visited = HashSet::new();
    let mut order = Vec::new();
    visit(graph, entry_point, &mut visited, &mut order);
    order
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CssImportKind {
    #[default]
    None,
    SourceIndex,
    ExternalPath,
    Layers,
}

#[derive(Clone, Debug, Default)]
pub struct CssImportOrder {
    pub conditions: Vec<ImportConditions>,
    pub condition_import_records: Vec<ImportRecord>,
    pub layers: Vec<Vec<String>>,
    pub external_path: Path,
    pub source_index: u32,
    pub kind: CssImportKind,
}

/// Find CSS files reachable from one or more CSS entry points in browser
/// evaluation order.
///
/// Unlike JavaScript, CSS may evaluate the same file more than once. The last
/// evaluation supplies declarations, while the first evaluation supplies
/// cascade-layer ordering. This traversal preserves those two distinct effects,
/// hoists external imports, and carries nested import conditions.
///
/// # Panics
///
/// Panics if a reachable internal CSS dependency is not represented as CSS.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn find_imported_files_in_css_order(
    graph: &LinkerGraph,
    entry_points: &[u32],
) -> Vec<CssImportOrder> {
    fn visit(
        graph: &LinkerGraph,
        source_index: u32,
        visited: &[u32],
        wrapping_conditions: &[ImportConditions],
        wrapping_import_records: &[ImportRecord],
        order: &mut Vec<CssImportOrder>,
        has_external_import: &mut bool,
    ) {
        if visited.contains(&source_index) {
            return;
        }
        let mut visited = visited.to_vec();
        visited.push(source_index);

        let Some(InputFileRepr::Css(repr)) =
            graph.files[source_index as usize].input_file.repr.as_ref()
        else {
            panic!("CSS import traversal reached a non-CSS file");
        };

        if !repr.ast.layers_pre_import.is_empty() {
            order.push(CssImportOrder {
                kind: CssImportKind::Layers,
                layers: repr.ast.layers_pre_import.clone(),
                conditions: wrapping_conditions.to_vec(),
                condition_import_records: wrapping_import_records.to_vec(),
                ..CssImportOrder::default()
            });
        }

        for rule in &repr.ast.rules {
            let crate::internal::css_ast::RuleData::AtImport(at_import) = &rule.data else {
                continue;
            };
            let record = &repr.ast.import_records[at_import.import_record_index as usize];

            if record.source_index.is_valid() {
                let mut nested_conditions = wrapping_conditions.to_vec();
                let mut nested_import_records = wrapping_import_records.to_vec();
                if let Some(import_conditions) = &at_import.import_conditions {
                    nested_conditions.push(import_conditions.clone_with_import_records(
                        &repr.ast.import_records,
                        &mut nested_import_records,
                    ));
                }
                visit(
                    graph,
                    record.source_index.get_index(),
                    &visited,
                    &nested_conditions,
                    &nested_import_records,
                    order,
                    has_external_import,
                );
                continue;
            }

            if !record
                .flags
                .contains(ImportRecordFlags::WAS_LOADED_WITH_EMPTY_LOADER)
            {
                let mut conditions = wrapping_conditions.to_vec();
                let mut condition_import_records = wrapping_import_records.to_vec();
                if let Some(import_conditions) = &at_import.import_conditions {
                    conditions.push(import_conditions.clone_with_import_records(
                        &repr.ast.import_records,
                        &mut condition_import_records,
                    ));
                }
                order.push(CssImportOrder {
                    kind: CssImportKind::ExternalPath,
                    external_path: record.path.clone(),
                    conditions,
                    condition_import_records,
                    ..CssImportOrder::default()
                });
                *has_external_import = true;
            }
        }

        for record in &repr.ast.import_records {
            if record.kind == ImportKind::ComposesFrom && record.source_index.is_valid() {
                visit(
                    graph,
                    record.source_index.get_index(),
                    &visited,
                    wrapping_conditions,
                    wrapping_import_records,
                    order,
                    has_external_import,
                );
            }
        }

        order.push(CssImportOrder {
            kind: CssImportKind::SourceIndex,
            source_index,
            conditions: wrapping_conditions.to_vec(),
            condition_import_records: wrapping_import_records.to_vec(),
            ..CssImportOrder::default()
        });
    }

    let mut order = Vec::new();
    let mut has_external_import = false;
    let visited = [crate::internal::runtime::SOURCE_INDEX; 16];
    for &source_index in entry_points {
        visit(
            graph,
            source_index,
            &visited,
            &[],
            &[],
            &mut order,
            &mut has_external_import,
        );
    }

    if has_external_import {
        let mut reordered = Vec::with_capacity(order.len());
        let mut is_at_layer_prefix = true;
        for entry in &order {
            if (entry.kind == CssImportKind::Layers && is_at_layer_prefix)
                || entry.kind == CssImportKind::ExternalPath
            {
                reordered.push(entry.clone());
            }
            if entry.kind != CssImportKind::Layers {
                is_at_layer_prefix = false;
            }
        }
        is_at_layer_prefix = true;
        for entry in order {
            let kind = entry.kind;
            if (entry.kind != CssImportKind::Layers || !is_at_layer_prefix)
                && entry.kind != CssImportKind::ExternalPath
            {
                reordered.push(entry);
            }
            if kind != CssImportKind::Layers {
                is_at_layer_prefix = false;
            }
        }
        order = reordered;
    }

    optimize_css_import_order(graph, order)
}

fn css_layers_post_import(graph: &LinkerGraph, source_index: u32) -> Vec<Vec<String>> {
    let Some(InputFileRepr::Css(repr)) =
        graph.files[source_index as usize].input_file.repr.as_ref()
    else {
        panic!("CSS import optimization reached a non-CSS file");
    };
    repr.ast.layers_post_import.clone()
}

#[derive(Default)]
struct CssLayerDuplicate {
    layers: Vec<Vec<String>>,
    indices: Vec<usize>,
}

#[allow(clippy::too_many_lines)]
fn optimize_css_import_order(
    graph: &LinkerGraph,
    mut order: Vec<CssImportOrder>,
) -> Vec<CssImportOrder> {
    let mut source_duplicates: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut external_duplicates: HashMap<Path, Vec<usize>> = HashMap::new();

    for index in (0..order.len()).rev() {
        match order[index].kind {
            CssImportKind::SourceIndex => {
                let source_index = order[index].source_index;
                let duplicates = source_duplicates
                    .get(&source_index)
                    .cloned()
                    .unwrap_or_default();
                if duplicates.iter().any(|&duplicate| {
                    is_conditional_import_redundant(
                        &order[index].conditions,
                        &order[duplicate].conditions,
                    )
                }) {
                    order[index].kind = CssImportKind::Layers;
                    order[index].layers = css_layers_post_import(graph, source_index);
                } else {
                    source_duplicates
                        .entry(source_index)
                        .or_default()
                        .push(index);
                }
            }
            CssImportKind::ExternalPath => {
                let external_path = order[index].external_path.clone();
                let duplicates = external_duplicates
                    .get(&external_path)
                    .cloned()
                    .unwrap_or_default();
                if duplicates.iter().any(|&duplicate| {
                    is_conditional_import_redundant(
                        &order[index].conditions,
                        &order[duplicate].conditions,
                    )
                }) {
                    order[index].kind = CssImportKind::Layers;
                } else {
                    external_duplicates
                        .entry(external_path)
                        .or_default()
                        .push(index);
                }
            }
            CssImportKind::None | CssImportKind::Layers => {}
        }
    }

    let mut optimized = Vec::<CssImportOrder>::with_capacity(order.len());
    let mut layer_duplicates = Vec::<CssLayerDuplicate>::new();

    'next_entry: for mut entry in order {
        if entry.kind == CssImportKind::Layers {
            if let Some(anonymous_layer_index) = entry.conditions.iter().position(|condition| {
                condition.layers.len() == 1 && condition.layers[0].children.is_none()
            }) {
                entry.conditions.truncate(anonymous_layer_index);
                entry.layers.clear();
            }

            if entry.layers.is_empty() {
                while entry
                    .conditions
                    .last()
                    .is_some_and(|condition| condition.layers.is_empty())
                {
                    entry.conditions.pop();
                }
            }

            if entry.conditions.is_empty() && entry.layers.is_empty() {
                continue;
            }
        }

        let layers_key = if entry.kind == CssImportKind::SourceIndex {
            css_layers_post_import(graph, entry.source_index)
        } else {
            entry.layers.clone()
        };
        let duplicate_set_index = layer_duplicates
            .iter()
            .position(|duplicate| string_array_arrays_equal(&layers_key, &duplicate.layers))
            .unwrap_or_else(|| {
                layer_duplicates.push(CssLayerDuplicate {
                    layers: layers_key,
                    indices: Vec::new(),
                });
                layer_duplicates.len() - 1
            });

        let mut duplicates = layer_duplicates[duplicate_set_index].indices.clone();
        for (reverse_index, &optimized_index) in duplicates.iter().enumerate().rev() {
            if !is_conditional_import_redundant(
                &entry.conditions,
                &optimized[optimized_index].conditions,
            ) {
                continue;
            }

            if entry.kind != CssImportKind::Layers {
                if reverse_index + 1 == duplicates.len()
                    && optimized_index + 1 == optimized.len()
                    && optimized[optimized_index].kind == CssImportKind::Layers
                    && import_conditions_are_equal(
                        &entry.conditions,
                        &optimized[optimized_index].conditions,
                    )
                {
                    duplicates.truncate(reverse_index);
                    optimized.truncate(optimized_index);
                } else {
                    optimized.push(entry);
                }
            }
            layer_duplicates[duplicate_set_index].indices = duplicates;
            continue 'next_entry;
        }

        duplicates.push(optimized.len());
        layer_duplicates[duplicate_set_index].indices = duplicates;
        optimized.push(entry);
    }

    let mut merged = Vec::<CssImportOrder>::with_capacity(optimized.len());
    for entry in optimized {
        if entry.kind == CssImportKind::Layers
            && let Some(previous) = merged.last_mut()
            && previous.kind == CssImportKind::Layers
            && import_conditions_are_equal(&previous.conditions, &entry.conditions)
        {
            previous.layers.extend(entry.layers);
        } else {
            merged.push(entry);
        }
    }
    merged
}

/// Wrap CSS rules in the nested conditions accumulated from `@import` edges.
///
/// # Panics
///
/// Panics if a URL token in an import condition refers to a missing condition
/// import record.
#[must_use]
pub fn wrap_rules_with_conditions(
    mut rules: Vec<Rule>,
    mut import_records: Vec<ImportRecord>,
    conditions: &[ImportConditions],
    condition_import_records: &[ImportRecord],
) -> (Vec<Rule>, Vec<ImportRecord>) {
    for condition in conditions.iter().rev() {
        for token in &condition.layers {
            if rules.is_empty() {
                if token.children.is_none() {
                    continue;
                }
                rules.clear();
            }
            let prelude = token.children.as_deref().unwrap_or_default();
            let prelude = clone_tokens_with_import_records(
                prelude,
                condition_import_records,
                &mut import_records,
            );
            rules = vec![Rule {
                data: RuleData::KnownAt(KnownAtRule {
                    at_token: "layer".into(),
                    prelude,
                    rules,
                    ..KnownAtRule::default()
                }),
                loc: crate::internal::logger::Loc::default(),
            }];
        }

        if !rules.is_empty() {
            for token in &condition.supports {
                let mut token = token.clone();
                token.kind = TokenKind::OpenParen;
                token.text = "(".into();
                let prelude = clone_tokens_with_import_records(
                    &[token],
                    condition_import_records,
                    &mut import_records,
                );
                rules = vec![Rule {
                    data: RuleData::KnownAt(KnownAtRule {
                        at_token: "supports".into(),
                        prelude,
                        rules,
                        ..KnownAtRule::default()
                    }),
                    loc: crate::internal::logger::Loc::default(),
                }];
            }
        }

        if !rules.is_empty() && !condition.queries.is_empty() {
            let queries = clone_media_queries_with_import_records(
                &condition.queries,
                condition_import_records,
                &mut import_records,
            );
            rules = vec![Rule {
                data: RuleData::AtMedia(AtMediaRule {
                    queries,
                    rules,
                    ..AtMediaRule::default()
                }),
                loc: crate::internal::logger::Loc::default(),
            }];
        }
    }
    (rules, import_records)
}

#[derive(Clone, Debug, Default)]
pub struct PreparedCssAst {
    pub ast: CssAst,
    pub source_index: Index32,
    pub has_charset: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompiledCssAst {
    pub css: Vec<u8>,
    pub extracted_legal_comments: Vec<String>,
    pub json_metadata_imports: Vec<String>,
    pub source_map_chunk: SourceMapChunk,
    pub source_index: Index32,
    pub has_charset: bool,
}

/// Convert CSS import-order entries into standalone ASTs ready for printing.
///
/// This removes rules that the linker represents separately and encodes nested
/// external import conditions as data-URL stylesheets, matching the only CSS
/// representation that preserves arbitrary condition nesting.
///
/// # Panics
///
/// Panics if a source entry is not CSS or if condition import records are
/// internally inconsistent.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn prepare_css_asts(
    graph: &LinkerGraph,
    order: &[CssImportOrder],
    options: &Options,
) -> Vec<PreparedCssAst> {
    let mut prepared: Vec<_> = order
        .iter()
        .map(|entry| match entry.kind {
            CssImportKind::None => PreparedCssAst::default(),
            CssImportKind::Layers => {
                let rules = if entry.layers.is_empty() {
                    Vec::new()
                } else {
                    vec![Rule {
                        data: RuleData::AtLayer(AtLayerRule {
                            names: entry.layers.clone(),
                            ..AtLayerRule::default()
                        }),
                        loc: crate::internal::logger::Loc::default(),
                    }]
                };
                let (rules, import_records) = wrap_rules_with_conditions(
                    rules,
                    Vec::new(),
                    &entry.conditions,
                    &entry.condition_import_records,
                );
                PreparedCssAst {
                    ast: CssAst {
                        import_records,
                        rules,
                        ..CssAst::default()
                    },
                    ..PreparedCssAst::default()
                }
            }
            CssImportKind::ExternalPath => {
                let mut external_path = entry.external_path.clone();
                for condition_index in (1..entry.conditions.len()).rev() {
                    let mut import_records = entry.condition_import_records.clone();
                    let import_record_index = u32::try_from(import_records.len())
                        .expect("CSS import record count fits in u32");
                    import_records.push(ImportRecord {
                        kind: ImportKind::At,
                        path: external_path,
                        ..ImportRecord::default()
                    });
                    let ast = CssAst {
                        import_records,
                        rules: vec![Rule {
                            data: RuleData::AtImport(AtImportRule {
                                import_record_index,
                                import_conditions: Some(entry.conditions[condition_index].clone()),
                            }),
                            loc: crate::internal::logger::Loc::default(),
                        }],
                        ..CssAst::default()
                    };
                    let result = css_printer::print(
                        &ast,
                        &graph.symbols,
                        css_printer::Options {
                            minify_whitespace: options.minify_whitespace,
                            ascii_only: options.ascii_only,
                            ..css_printer::Options::default()
                        },
                    );
                    external_path = Path {
                        text: encode_string_as_shortest_data_url(
                            "text/css",
                            result.css.trim_ascii(),
                        ),
                        ..Path::default()
                    };
                }

                let mut import_records = entry.condition_import_records.clone();
                let import_record_index = u32::try_from(import_records.len())
                    .expect("CSS import record count fits in u32");
                import_records.push(ImportRecord {
                    kind: ImportKind::At,
                    path: external_path,
                    ..ImportRecord::default()
                });
                PreparedCssAst {
                    ast: CssAst {
                        import_records,
                        rules: vec![Rule {
                            data: RuleData::AtImport(AtImportRule {
                                import_record_index,
                                import_conditions: entry.conditions.first().cloned(),
                            }),
                            loc: crate::internal::logger::Loc::default(),
                        }],
                        ..CssAst::default()
                    },
                    ..PreparedCssAst::default()
                }
            }
            CssImportKind::SourceIndex => {
                let Some(InputFileRepr::Css(repr)) = graph.files[entry.source_index as usize]
                    .input_file
                    .repr
                    .as_ref()
                else {
                    panic!("CSS AST preparation reached a non-CSS file");
                };
                let mut ast = repr.ast.clone();
                let mut rules = Vec::with_capacity(ast.rules.len());
                let mut has_charset = false;
                let mut did_find_at_import = false;
                let mut did_find_at_layer = false;
                for rule in ast.rules {
                    match &rule.data {
                        RuleData::AtCharset(_) => {
                            has_charset = true;
                            continue;
                        }
                        RuleData::AtLayer(_) => did_find_at_layer = true,
                        RuleData::AtImport(_) => {
                            if !did_find_at_import {
                                did_find_at_import = true;
                                if did_find_at_layer {
                                    rules.retain(|rule: &Rule| {
                                        !matches!(rule.data, RuleData::AtLayer(_))
                                    });
                                }
                            }
                            continue;
                        }
                        _ => {}
                    }
                    rules.push(rule);
                }
                let (rules, import_records) = wrap_rules_with_conditions(
                    rules,
                    ast.import_records,
                    &entry.conditions,
                    &entry.condition_import_records,
                );
                ast.rules = rules;
                ast.import_records = import_records;
                PreparedCssAst {
                    ast,
                    source_index: Index32::new(entry.source_index),
                    has_charset,
                }
            }
        })
        .collect();
    if options.minify_syntax {
        let mut remover = css_parser::make_dead_rule_mangler(graph.symbols.clone());
        for item in prepared.iter_mut().rev() {
            if item.source_index.is_valid() {
                item.ast.rules = remover.remove_dead_rules_in_place(
                    item.source_index.get_index(),
                    std::mem::take(&mut item.ast.rules),
                    &item.ast.import_records,
                );
            }
        }
    }
    prepared
}

/// Print each prepared CSS AST independently before chunk concatenation.
#[must_use]
pub fn compile_prepared_css_asts(
    graph: &LinkerGraph,
    prepared: &[PreparedCssAst],
    options: &Options,
) -> Vec<CompiledCssAst> {
    let local_names = mangle_local_css(graph, options, &mut HashSet::new());
    prepared
        .iter()
        .map(|item| {
            let input_source_index = if item.source_index.is_valid() {
                item.source_index.get_index()
            } else {
                0
            };
            let (input_source_map, line_offset_tables, add_source_mappings) =
                if item.source_index.is_valid() && options.source_map != SourceMapMode::None {
                    let file = &graph.files[input_source_index as usize];
                    (
                        file.input_file
                            .input_source_map
                            .clone()
                            .map(std::sync::Arc::new),
                        crate::internal::sourcemap::generate_line_offset_tables(
                            &file.input_file.source.contents,
                            item.ast.approximate_line_count.max(0),
                        ),
                        file.input_file.loader.can_have_source_map(),
                    )
                } else {
                    (None, Vec::new(), false)
                };
            let printed = css_printer::print(
                &item.ast,
                &graph.symbols,
                css_printer::Options {
                    input_source_map,
                    line_offset_tables,
                    local_names: local_names.clone(),
                    line_limit: options.line_limit,
                    input_source_index,
                    minify_whitespace: options.minify_whitespace,
                    ascii_only: options.ascii_only,
                    legal_comments: options.legal_comments,
                    needs_metafile: options.needs_metafile,
                    metafile_format: options.metafile_format,
                    source_map: options.source_map,
                    add_source_mappings,
                },
            );
            CompiledCssAst {
                css: printed.css,
                extracted_legal_comments: printed.extracted_legal_comments,
                json_metadata_imports: printed.json_metadata_imports,
                source_map_chunk: printed.source_map_chunk,
                source_index: item.source_index,
                has_charset: item.has_charset,
            }
        })
        .collect()
}

/// Prepare, print, and assemble one CSS chunk.
///
/// # Panics
///
/// Panics if the supplied chunk is not CSS or contains invalid CSS import
/// ordering data.
pub fn generate_css_chunk(
    graph: &LinkerGraph,
    chunk: &mut ChunkInfo,
    options: &Options,
    output_paths: &OutputPathContext<'_>,
) {
    assert!(chunk.is_css, "CSS generation requires a CSS chunk");
    let prepared = prepare_css_asts(graph, &chunk.imports_in_css_order, options);
    let compiled = compile_prepared_css_asts(graph, &prepared, options);
    assemble_css_chunk(graph, chunk, &compiled, options, output_paths);
}

/// Concatenate printed CSS files into one chunk and split temporary output
/// paths into intermediate pieces.
#[allow(clippy::too_many_lines)]
pub fn assemble_css_chunk(
    graph: &LinkerGraph,
    chunk: &mut ChunkInfo,
    compiled: &[CompiledCssAst],
    options: &Options,
    output_paths: &OutputPathContext<'_>,
) {
    let mut joiner = Joiner::default();
    let mut newline_before_comment = false;
    let mut metadata_inputs = Vec::new();
    let mut metadata_imports = Vec::new();
    let mut legal_comment_list = Vec::new();
    let mut source_map_results = Vec::new();
    let mut previous_offset = LineColumnOffset::default();
    chunk.external_legal_comments.clear();
    chunk.metadata_imports.clear();
    chunk.metadata_inputs.clear();
    chunk.source_map_results.clear();

    if !options.css_banner.is_empty() {
        previous_offset.advance_string(&options.css_banner);
        joiner.add_string(options.css_banner.clone());
        previous_offset.advance_string("\n");
        joiner.add_string("\n");
    }

    if compiled.iter().any(|item| item.has_charset) {
        let charset = css_printer::print(
            &CssAst {
                rules: vec![Rule {
                    data: RuleData::AtCharset(crate::internal::css_ast::AtCharsetRule {
                        encoding: "UTF-8".into(),
                    }),
                    loc: crate::internal::logger::Loc::default(),
                }],
                ..CssAst::default()
            },
            &graph.symbols,
            css_printer::Options {
                line_limit: options.line_limit,
                minify_whitespace: options.minify_whitespace,
                ascii_only: options.ascii_only,
                ..css_printer::Options::default()
            },
        );
        if !charset.css.is_empty() {
            previous_offset.advance_bytes(&charset.css);
            joiner.add_bytes(charset.css);
            newline_before_comment = true;
        }
    }

    for item in compiled {
        if options.needs_metafile {
            metadata_imports.extend(
                item.json_metadata_imports
                    .iter()
                    .map(|json| output_paths.break_output_into_pieces(json.as_bytes().to_vec())),
            );
        }
        if item.source_index.is_valid() && !item.extracted_legal_comments.is_empty() {
            legal_comment_list.push(LegalCommentEntry {
                source_index: item.source_index.get_index(),
                comments: item.extracted_legal_comments.clone(),
            });
        }
        if options.needs_metafile
            && item.source_index.is_valid()
            && !graph.files[item.source_index.get_index() as usize]
                .input_file
                .omit_from_source_maps_and_metafile
        {
            metadata_inputs.push(MetadataInput {
                source_index: item.source_index.get_index(),
                outputs: vec![output_paths.break_output_into_pieces(item.css.clone())],
            });
        }
        if options.mode == Mode::Bundle
            && !options.minify_whitespace
            && item.source_index.is_valid()
        {
            if newline_before_comment {
                previous_offset.advance_string("\n");
                joiner.add_string("\n");
            }
            let source_index = item.source_index.get_index();
            let path = graph.files[source_index as usize]
                .input_file
                .source
                .pretty_paths
                .select(options.code_path_style);
            let comment = format!("/* {path} */\n");
            previous_offset.advance_string(&comment);
            joiner.add_string(comment);
        }
        if !item.css.is_empty() {
            newline_before_comment = true;
            let generated_offset = previous_offset;
            if options.source_map != SourceMapMode::None && item.source_index.is_valid() {
                if item.source_map_chunk.should_ignore {
                    previous_offset.advance_bytes(&item.css);
                    if source_map_results
                        .last()
                        .is_none_or(|result: &CompileResultForSourceMap| !result.is_null_entry)
                    {
                        source_map_results.push(CompileResultForSourceMap {
                            generated_offset,
                            source_index: item.source_index.get_index(),
                            is_null_entry: true,
                            ..CompileResultForSourceMap::default()
                        });
                    }
                } else {
                    source_map_results.push(CompileResultForSourceMap {
                        source_map_chunk: item.source_map_chunk.clone(),
                        generated_offset,
                        source_index: item.source_index.get_index(),
                        is_null_entry: false,
                    });
                    previous_offset = LineColumnOffset::default();
                }
            } else {
                previous_offset.advance_bytes(&item.css);
            }
            joiner.add_bytes(item.css.clone());
        }
    }

    joiner.ensure_newline_at_end();
    let slash_tag = if options
        .unsupported_css_features
        .contains(crate::internal::compat::CssFeature::INLINE_STYLE)
    {
        ""
    } else {
        "/style"
    };
    append_legal_comments(
        graph,
        options.legal_comments,
        &legal_comment_list,
        chunk,
        &mut joiner,
        slash_tag,
    );
    if !options.css_footer.is_empty() {
        joiner.add_string(options.css_footer.clone());
        joiner.add_string("\n");
    }
    chunk.intermediate_output = output_paths.break_joiner_into_pieces(joiner);
    chunk.metadata_imports = metadata_imports;
    chunk.metadata_inputs = metadata_inputs;
    chunk.source_map_results = source_map_results;
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
    pub json_metadata_imports: Vec<String>,
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
        indent: usize::from(options.output_format == Format::Iife),
        minify_syntax: options.minify_syntax,
        minify_whitespace: options.minify_whitespace,
        ascii_only: options.ascii_only,
        legal_comments: options.legal_comments,
        needs_metafile: options.needs_metafile,
        metafile_format: options.metafile_format,
    };
    let prefix_result = crate::internal::js_printer::print(
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
    );
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
    PrintedCrossChunkBindings {
        prefix: prefix_result.js,
        suffix,
        json_metadata_imports: prefix_result.json_metadata_imports,
    }
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
    pub arbitrary_namespace_issues: Vec<ArbitraryNamespaceIssue>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeReExportContext {
    pub re_export_ref: Ref,
    pub unbound_module_ref: Option<Ref>,
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
/// an `export *` statement requires a missing runtime re-export context.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn convert_stmts_for_chunk(
    graph: &LinkerGraph,
    options: &Options,
    source_index: u32,
    part_statements: &[js_ast::Stmt],
    runtime_re_export: Option<RuntimeReExportContext>,
) -> ConvertedStmts {
    let file = &graph.files[source_index as usize];
    let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_ref() else {
        panic!("statement source must be JavaScript");
    };
    let should_strip_exports = options.mode != Mode::PassThrough || !file.is_entry_point();
    let extract_esm_from_wrapper = repr.meta.wrap != WrapKind::None;
    let validate_arbitrary_names = options
        .unsupported_js_features
        .contains(crate::internal::compat::JsFeature::ARBITRARY_MODULE_NAMESPACE_NAMES);
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
                if validate_arbitrary_names && let Some(items) = &import.items {
                    result.arbitrary_namespace_issues.extend(
                        items
                            .iter()
                            .filter(|item| !js_ast::is_identifier(&item.alias))
                            .map(|item| ArbitraryNamespaceIssue {
                                kind: "import".into(),
                                source_index,
                                name_loc: item.alias_loc,
                                alias: item.alias.clone(),
                            }),
                    );
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
                if validate_arbitrary_names {
                    for item in &export.items {
                        if !js_ast::is_identifier(&item.original_name) {
                            result
                                .arbitrary_namespace_issues
                                .push(ArbitraryNamespaceIssue {
                                    kind: "import".into(),
                                    source_index,
                                    name_loc: item.name.loc,
                                    alias: item.original_name.clone(),
                                });
                        }
                        if !should_strip_exports
                            && item.alias_loc != item.name.loc
                            && !js_ast::is_identifier(&item.alias)
                        {
                            result
                                .arbitrary_namespace_issues
                                .push(ArbitraryNamespaceIssue {
                                    kind: "export".into(),
                                    source_index,
                                    name_loc: item.alias_loc,
                                    alias: item.alias.clone(),
                                });
                        }
                    }
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
                if validate_arbitrary_names
                    && let Some(alias) = &export.alias
                    && !js_ast::is_identifier(&alias.original_name)
                {
                    result
                        .arbitrary_namespace_issues
                        .push(ArbitraryNamespaceIssue {
                            kind: "export".into(),
                            source_index,
                            name_loc: alias.loc,
                            alias: alias.original_name.clone(),
                        });
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
                let namespace_ref = export.namespace_ref;
                let import_record_index = export.import_record_index;
                let calls_runtime = record
                    .flags
                    .contains(ImportRecordFlags::CALLS_RUN_TIME_RE_EXPORT_FN);
                if !record.source_index.is_valid()
                    && options.output_format.keep_esm_import_export_syntax()
                {
                    if calls_runtime {
                        statement.data =
                            Some(Box::new(js_ast::StmtData::Import(js_ast::ImportStmt {
                                namespace_ref,
                                star_name_loc: Some(statement.loc),
                                import_record_index,
                                ..js_ast::ImportStmt::default()
                            })));
                        result
                            .inside_wrapper_prefix
                            .push(runtime_re_export_statement(
                                runtime_re_export
                                    .expect("runtime export-star conversion requires runtime refs"),
                                repr.ast.exports_ref,
                                js_ast::Expr::new(
                                    statement.loc,
                                    js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                                        reference: namespace_ref,
                                        ..js_ast::IdentifierExpr::default()
                                    }),
                                ),
                                options.output_format == Format::CommonJs && file.is_entry_point(),
                                statement.loc,
                            ));
                    }
                    result.push_esm_statement(statement, extract_esm_from_wrapper);
                } else if record.source_index.is_valid() {
                    let target_file = &graph.files[record.source_index.get_index() as usize];
                    let Some(InputFileRepr::Js(target)) = target_file.input_file.repr.as_ref()
                    else {
                        panic!("export-star target must be JavaScript");
                    };
                    if target.meta.wrap == WrapKind::Esm {
                        result.inside_wrapper_prefix.push(js_ast::Stmt::new(
                            statement.loc,
                            js_ast::StmtData::Expr(js_ast::ExprStmt {
                                value: wrapper_call(target.ast.wrapper_ref, statement.loc),
                                ..js_ast::ExprStmt::default()
                            }),
                        ));
                    }
                    if calls_runtime {
                        let target_expr =
                            if target.ast.exports_kind == ExportsKind::EsmWithDynamicFallback {
                                js_ast::Expr::new(
                                    record.range.loc,
                                    js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                                        reference: target.ast.exports_ref,
                                        ..js_ast::IdentifierExpr::default()
                                    }),
                                )
                            } else {
                                js_ast::Expr::new(
                                    record.range.loc,
                                    js_ast::ExprData::RequireString(js_ast::RequireStringExpr {
                                        import_record_index,
                                        ..js_ast::RequireStringExpr::default()
                                    }),
                                )
                            };
                        result
                            .inside_wrapper_prefix
                            .push(runtime_re_export_statement(
                                runtime_re_export
                                    .expect("runtime export-star conversion requires runtime refs"),
                                repr.ast.exports_ref,
                                target_expr,
                                options.output_format == Format::CommonJs && file.is_entry_point(),
                                statement.loc,
                            ));
                    }
                } else if calls_runtime {
                    result
                        .inside_wrapper_prefix
                        .push(runtime_re_export_statement(
                            runtime_re_export
                                .expect("runtime export-star conversion requires runtime refs"),
                            repr.ast.exports_ref,
                            js_ast::Expr::new(
                                record.range.loc,
                                js_ast::ExprData::RequireString(js_ast::RequireStringExpr {
                                    import_record_index,
                                    ..js_ast::RequireStringExpr::default()
                                }),
                            ),
                            options.output_format == Format::CommonJs && file.is_entry_point(),
                            statement.loc,
                        ));
                }
                continue;
            }
            Some(js_ast::StmtData::ExportClause(_)) if should_strip_exports => continue,
            Some(js_ast::StmtData::ExportClause(export)) => {
                if validate_arbitrary_names {
                    result.arbitrary_namespace_issues.extend(
                        export
                            .items
                            .iter()
                            .filter(|item| !js_ast::is_identifier(&item.alias))
                            .map(|item| ArbitraryNamespaceIssue {
                                kind: "export".into(),
                                source_index,
                                name_loc: item.alias_loc,
                                alias: item.alias.clone(),
                            }),
                    );
                }
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

fn wrapper_call(wrapper_ref: Ref, location: crate::internal::logger::Loc) -> js_ast::Expr {
    js_ast::Expr::new(
        location,
        js_ast::ExprData::Call(js_ast::CallExpr {
            target: js_ast::Expr::new(
                location,
                js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                    reference: wrapper_ref,
                    ..js_ast::IdentifierExpr::default()
                }),
            ),
            ..js_ast::CallExpr::default()
        }),
    )
}

fn runtime_re_export_statement(
    context: RuntimeReExportContext,
    exports_ref: Ref,
    target: js_ast::Expr,
    include_module_exports: bool,
    location: crate::internal::logger::Loc,
) -> js_ast::Stmt {
    let mut arguments = vec![
        js_ast::Expr::new(
            location,
            js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                reference: exports_ref,
                ..js_ast::IdentifierExpr::default()
            }),
        ),
        target,
    ];
    if include_module_exports {
        let module_ref = context
            .unbound_module_ref
            .expect("CommonJS entry re-export requires the unbound module ref");
        arguments.push(js_ast::Expr::new(
            location,
            js_ast::ExprData::Dot(js_ast::DotExpr {
                target: js_ast::Expr::new(
                    location,
                    js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                        reference: module_ref,
                        ..js_ast::IdentifierExpr::default()
                    }),
                ),
                name: "exports".into(),
                ..js_ast::DotExpr::default()
            }),
        ));
    }
    js_ast::Stmt::new(
        location,
        js_ast::StmtData::Expr(js_ast::ExprStmt {
            value: js_ast::Expr::new(
                location,
                js_ast::ExprData::Call(js_ast::CallExpr {
                    target: js_ast::Expr::new(
                        location,
                        js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                            reference: context.re_export_ref,
                            ..js_ast::IdentifierExpr::default()
                        }),
                    ),
                    args: arguments,
                    ..js_ast::CallExpr::default()
                }),
            ),
            ..js_ast::ExprStmt::default()
        }),
    )
}

/// Wrap `CommonJS` module statements in the generated `__commonJS` initializer.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn wrap_common_js_stmts(
    ast: &js_ast::Ast,
    body: Vec<js_ast::Stmt>,
    mut outside_wrapper_prefix: Vec<js_ast::Stmt>,
    common_js_runtime_ref: Ref,
    options: &Options,
    pretty_path: &str,
) -> Vec<js_ast::Stmt> {
    let mut arguments = Vec::new();
    if ast.uses_exports_ref || ast.uses_module_ref {
        arguments.push(js_ast::Arg {
            binding: identifier_binding(ast.exports_ref),
            ..js_ast::Arg::default()
        });
        if ast.uses_module_ref {
            arguments.push(js_ast::Arg {
                binding: identifier_binding(ast.module_ref),
                ..js_ast::Arg::default()
            });
        }
    }
    let function_body = js_ast::FunctionBody {
        block: js_ast::BlockStmt {
            statements: body,
            ..js_ast::BlockStmt::default()
        },
        ..js_ast::FunctionBody::default()
    };
    let initializer = if options.profiler_names {
        let kind = if options
            .unsupported_js_features
            .contains(crate::internal::compat::JsFeature::OBJECT_EXTENSIONS)
        {
            js_ast::PropertyKind::Field
        } else {
            js_ast::PropertyKind::Method
        };
        js_ast::Expr::new(
            crate::internal::logger::Loc::default(),
            js_ast::ExprData::Object(js_ast::ObjectExpr {
                properties: vec![js_ast::Property {
                    kind,
                    key: js_ast::Expr::new(
                        crate::internal::logger::Loc::default(),
                        js_ast::ExprData::String(js_ast::StringExpr {
                            value: crate::internal::helpers::string_to_utf16(
                                pretty_path.as_bytes(),
                            ),
                            ..js_ast::StringExpr::default()
                        }),
                    ),
                    value_or_nil: js_ast::Expr::new(
                        crate::internal::logger::Loc::default(),
                        js_ast::ExprData::Function(js_ast::FunctionExpr {
                            function: js_ast::Function {
                                args: arguments,
                                body: function_body,
                                ..js_ast::Function::default()
                            },
                            ..js_ast::FunctionExpr::default()
                        }),
                    ),
                    ..js_ast::Property::default()
                }],
                ..js_ast::ObjectExpr::default()
            }),
        )
    } else if options
        .unsupported_js_features
        .contains(crate::internal::compat::JsFeature::ARROW)
    {
        js_ast::Expr::new(
            crate::internal::logger::Loc::default(),
            js_ast::ExprData::Function(js_ast::FunctionExpr {
                function: js_ast::Function {
                    args: arguments,
                    body: function_body,
                    ..js_ast::Function::default()
                },
                ..js_ast::FunctionExpr::default()
            }),
        )
    } else {
        js_ast::Expr::new(
            crate::internal::logger::Loc::default(),
            js_ast::ExprData::Arrow(js_ast::ArrowExpr {
                args: arguments,
                body: function_body,
                ..js_ast::ArrowExpr::default()
            }),
        )
    };
    let value = js_ast::Expr::new(
        crate::internal::logger::Loc::default(),
        js_ast::ExprData::Call(js_ast::CallExpr {
            target: js_ast::Expr::new(
                crate::internal::logger::Loc::default(),
                js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                    reference: common_js_runtime_ref,
                    ..js_ast::IdentifierExpr::default()
                }),
            ),
            args: vec![initializer],
            ..js_ast::CallExpr::default()
        }),
    );
    outside_wrapper_prefix.push(js_ast::Stmt::new(
        crate::internal::logger::Loc::default(),
        js_ast::StmtData::Local(js_ast::LocalStmt {
            declarations: vec![js_ast::Decl {
                binding: identifier_binding(ast.wrapper_ref),
                value_or_nil: value,
            }],
            ..js_ast::LocalStmt::default()
        }),
    ));
    outside_wrapper_prefix
}

fn identifier_binding(reference: Ref) -> js_ast::Binding {
    js_ast::Binding {
        data: Some(Box::new(js_ast::BindingData::Identifier(
            js_ast::IdentifierBinding { reference },
        ))),
        ..js_ast::Binding::default()
    }
}

/// Wrap an ESM module in the generated `__esm` initializer.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn wrap_esm_stmts(
    ast: &js_ast::Ast,
    body: Vec<js_ast::Stmt>,
    mut outside_wrapper_prefix: Vec<js_ast::Stmt>,
    esm_runtime_ref: Ref,
    options: &Options,
    pretty_path: &str,
    is_async: bool,
) -> Vec<js_ast::Stmt> {
    let hoisted = std::cell::RefCell::new(Vec::new());
    let wrap_identifier = |location, reference| {
        hoisted.borrow_mut().push(js_ast::Decl {
            binding: js_ast::Binding {
                loc: location,
                ..identifier_binding(reference)
            },
            ..js_ast::Decl::default()
        });
        js_ast::Expr::new(
            location,
            js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                reference,
                ..js_ast::IdentifierExpr::default()
            }),
        )
    };
    let mut wrapped_body = Vec::with_capacity(body.len());
    for mut statement in body {
        match statement.data.as_deref() {
            Some(js_ast::StmtData::Local(local)) => {
                let mut value = js_ast::Expr::default();
                for declaration in &local.declarations {
                    let binding = js_ast::convert_binding_to_expr(
                        &declaration.binding,
                        Some(&wrap_identifier),
                    );
                    if declaration.value_or_nil.data.is_some() {
                        value = js_ast::join_with_comma(
                            value,
                            js_ast::assign(binding, declaration.value_or_nil.clone()),
                        );
                    }
                }
                if value.data.is_none() {
                    continue;
                }
                statement.data = Some(Box::new(js_ast::StmtData::Expr(js_ast::ExprStmt {
                    value,
                    ..js_ast::ExprStmt::default()
                })));
            }
            Some(js_ast::StmtData::Function(_)) => {
                outside_wrapper_prefix.push(statement);
                continue;
            }
            _ => {}
        }
        wrapped_body.push(statement);
    }
    let mut declarations = hoisted.into_inner();
    let function_body = js_ast::FunctionBody {
        block: js_ast::BlockStmt {
            statements: wrapped_body,
            ..js_ast::BlockStmt::default()
        },
        ..js_ast::FunctionBody::default()
    };
    let initializer = if options.profiler_names {
        let kind = if options
            .unsupported_js_features
            .contains(crate::internal::compat::JsFeature::OBJECT_EXTENSIONS)
        {
            js_ast::PropertyKind::Field
        } else {
            js_ast::PropertyKind::Method
        };
        js_ast::Expr::new(
            crate::internal::logger::Loc::default(),
            js_ast::ExprData::Object(js_ast::ObjectExpr {
                properties: vec![js_ast::Property {
                    kind,
                    key: js_ast::Expr::new(
                        crate::internal::logger::Loc::default(),
                        js_ast::ExprData::String(js_ast::StringExpr {
                            value: crate::internal::helpers::string_to_utf16(
                                pretty_path.as_bytes(),
                            ),
                            ..js_ast::StringExpr::default()
                        }),
                    ),
                    value_or_nil: js_ast::Expr::new(
                        crate::internal::logger::Loc::default(),
                        js_ast::ExprData::Function(js_ast::FunctionExpr {
                            function: js_ast::Function {
                                body: function_body,
                                is_async,
                                ..js_ast::Function::default()
                            },
                            ..js_ast::FunctionExpr::default()
                        }),
                    ),
                    ..js_ast::Property::default()
                }],
                ..js_ast::ObjectExpr::default()
            }),
        )
    } else if options
        .unsupported_js_features
        .contains(crate::internal::compat::JsFeature::ARROW)
    {
        js_ast::Expr::new(
            crate::internal::logger::Loc::default(),
            js_ast::ExprData::Function(js_ast::FunctionExpr {
                function: js_ast::Function {
                    body: function_body,
                    is_async,
                    ..js_ast::Function::default()
                },
                ..js_ast::FunctionExpr::default()
            }),
        )
    } else {
        js_ast::Expr::new(
            crate::internal::logger::Loc::default(),
            js_ast::ExprData::Arrow(js_ast::ArrowExpr {
                body: function_body,
                is_async,
                ..js_ast::ArrowExpr::default()
            }),
        )
    };
    let value = js_ast::Expr::new(
        crate::internal::logger::Loc::default(),
        js_ast::ExprData::Call(js_ast::CallExpr {
            target: js_ast::Expr::new(
                crate::internal::logger::Loc::default(),
                js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
                    reference: esm_runtime_ref,
                    ..js_ast::IdentifierExpr::default()
                }),
            ),
            args: vec![initializer],
            ..js_ast::CallExpr::default()
        }),
    );

    if !options.minify_syntax && !declarations.is_empty() {
        outside_wrapper_prefix.push(js_ast::Stmt::new(
            crate::internal::logger::Loc::default(),
            js_ast::StmtData::Local(js_ast::LocalStmt {
                declarations,
                ..js_ast::LocalStmt::default()
            }),
        ));
        declarations = Vec::new();
    }
    declarations.push(js_ast::Decl {
        binding: identifier_binding(ast.wrapper_ref),
        value_or_nil: value,
    });
    outside_wrapper_prefix.push(js_ast::Stmt::new(
        crate::internal::logger::Loc::default(),
        js_ast::StmtData::Local(js_ast::LocalStmt {
            declarations,
            ..js_ast::LocalStmt::default()
        }),
    ));
    outside_wrapper_prefix
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ChunkRuntimeRefs {
    pub common_js_ref: Ref,
    pub esm_ref: Ref,
    pub to_common_js_ref: Ref,
    pub to_esm_ref: Ref,
    pub runtime_require_ref: Ref,
    pub re_export: Option<RuntimeReExportContext>,
}

/// Look up an exported helper in the parsed runtime module.
///
/// # Panics
///
/// Panics when the runtime file is missing, is not JavaScript, or does not
/// export `name`.
#[must_use]
pub fn runtime_symbol_ref(graph: &LinkerGraph, name: &str) -> Ref {
    let Some(InputFileRepr::Js(runtime)) = graph.files
        [crate::internal::runtime::SOURCE_INDEX as usize]
        .input_file
        .repr
        .as_ref()
    else {
        panic!("runtime must be JavaScript");
    };
    runtime
        .ast
        .named_exports
        .get(name)
        .unwrap_or_else(|| panic!("missing runtime export {name:?}"))
        .reference
}

/// Build the runtime reference bundle used by the linker scan stages.
#[must_use]
pub fn scan_runtime_refs_from_graph(
    graph: &LinkerGraph,
    unbound_module_ref: Ref,
) -> ScanRuntimeRefs {
    ScanRuntimeRefs {
        export_ref: runtime_symbol_ref(graph, "__export"),
        common_js_ref: runtime_symbol_ref(graph, "__commonJS"),
        esm_ref: runtime_symbol_ref(graph, "__esm"),
        to_common_js_ref: runtime_symbol_ref(graph, "__toCommonJS"),
        unbound_module_ref,
    }
}

/// Build the runtime reference bundle used while compiling JavaScript chunks.
#[must_use]
pub fn chunk_runtime_refs_from_graph(
    graph: &LinkerGraph,
    unbound_module_ref: Option<Ref>,
) -> ChunkRuntimeRefs {
    ChunkRuntimeRefs {
        common_js_ref: runtime_symbol_ref(graph, "__commonJS"),
        esm_ref: runtime_symbol_ref(graph, "__esm"),
        to_common_js_ref: runtime_symbol_ref(graph, "__toCommonJS"),
        to_esm_ref: runtime_symbol_ref(graph, "__toESM"),
        runtime_require_ref: runtime_symbol_ref(graph, "__require"),
        re_export: Some(RuntimeReExportContext {
            re_export_ref: runtime_symbol_ref(graph, "__reExport"),
            unbound_module_ref,
        }),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompiledPartRange {
    pub source_index: u32,
    pub js: Vec<u8>,
    pub extracted_legal_comments: Vec<String>,
    pub json_metadata_imports: Vec<String>,
    pub source_map_chunk: SourceMapChunk,
}

/// Compile one ordered range of live parts into JavaScript for a chunk.
///
/// # Panics
///
/// Panics when the part range is invalid, the source is not JavaScript, a
/// wrapper marker conflicts with its wrapper kind, or printing encounters an
/// unsupported AST invariant.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn compile_part_range_for_chunk(
    graph: &LinkerGraph,
    options: &Options,
    part_range: PartRange,
    runtime_refs: ChunkRuntimeRefs,
    renamer: &dyn crate::internal::renamer::Renamer,
) -> CompiledPartRange {
    let file = &graph.files[part_range.source_index as usize];
    let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_ref() else {
        panic!("part range source must be JavaScript");
    };
    let mut converted = ConvertedStmts::default();
    if repr.meta.wrap != WrapKind::None && !file.is_entry_point() {
        converted
            .inside_wrapper_prefix
            .extend(repr.ast.directives.iter().map(|directive| {
                js_ast::Stmt::new(
                    crate::internal::logger::Loc::default(),
                    js_ast::StmtData::Directive(js_ast::DirectiveStmt {
                        value: crate::internal::helpers::string_to_utf16(directive.as_bytes()),
                        ..js_ast::DirectiveStmt::default()
                    }),
                )
            }));
    }

    let namespace_part_index = js_ast::NS_EXPORT_PART_INDEX;
    if namespace_part_index >= part_range.part_index_begin
        && namespace_part_index < part_range.part_index_end
        && repr.ast.parts[namespace_part_index as usize].is_live
    {
        let namespace = convert_stmts_for_chunk(
            graph,
            options,
            part_range.source_index,
            &repr.ast.parts[namespace_part_index as usize].statements,
            runtime_refs.re_export,
        );
        converted
            .inside_wrapper_prefix
            .extend(namespace.inside_wrapper_prefix);
        converted
            .outside_wrapper_prefix
            .extend(namespace.outside_wrapper_prefix);
        if repr.meta.wrap == WrapKind::Esm {
            converted
                .outside_wrapper_prefix
                .extend(namespace.inside_wrapper_suffix);
        } else {
            converted
                .inside_wrapper_prefix
                .extend(namespace.inside_wrapper_suffix);
        }
    }

    let mut needs_wrapper = false;
    for part_index in part_range.part_index_begin..part_range.part_index_end {
        let part = &repr.ast.parts[part_index as usize];
        if !part.is_live || part_index == namespace_part_index {
            continue;
        }
        if repr.meta.wrapper_part_index.is_valid()
            && part_index == repr.meta.wrapper_part_index.get_index()
        {
            needs_wrapper = true;
            continue;
        }
        let part_converted = convert_stmts_for_chunk(
            graph,
            options,
            part_range.source_index,
            &part.statements,
            runtime_refs.re_export,
        );
        converted
            .inside_wrapper_prefix
            .extend(part_converted.inside_wrapper_prefix);
        converted
            .inside_wrapper_suffix
            .extend(part_converted.inside_wrapper_suffix);
        converted
            .outside_wrapper_prefix
            .extend(part_converted.outside_wrapper_prefix);
    }

    let mut body = converted.inside_wrapper_prefix;
    body.extend(converted.inside_wrapper_suffix);
    if options.minify_syntax {
        body = merge_adjacent_local_stmts(body);
    }
    let statements = if needs_wrapper {
        match repr.meta.wrap {
            WrapKind::Cjs => wrap_common_js_stmts(
                &repr.ast,
                body,
                converted.outside_wrapper_prefix,
                runtime_refs.common_js_ref,
                options,
                file.input_file
                    .source
                    .pretty_paths
                    .select(options.code_path_style),
            ),
            WrapKind::Esm => wrap_esm_stmts(
                &repr.ast,
                body,
                converted.outside_wrapper_prefix,
                runtime_refs.esm_ref,
                options,
                file.input_file
                    .source
                    .pretty_paths
                    .select(options.code_path_style),
                repr.meta.is_async_or_has_async_dependency,
            ),
            WrapKind::None => panic!("wrapper marker requires a wrapper kind"),
        }
    } else {
        let mut statements = converted.outside_wrapper_prefix;
        statements.extend(body);
        statements
    };

    let mut tree = repr.ast.clone();
    tree.directives.clear();
    tree.hashbang.clear();
    tree.parts = vec![js_ast::Part {
        statements,
        ..js_ast::Part::default()
    }];
    let print_options = crate::internal::js_printer::Options {
        unsupported_features: options.unsupported_js_features,
        line_limit: options.line_limit,
        indent: usize::from(options.output_format == Format::Iife),
        minify_syntax: options.minify_syntax,
        minify_whitespace: options.minify_whitespace,
        ascii_only: options.ascii_only,
        legal_comments: options.legal_comments,
        needs_metafile: options.needs_metafile,
        metafile_format: options.metafile_format,
    };
    let require_or_import_meta_for_source =
        |source_index| require_or_import_meta_for_source(graph, source_index);
    let linker_options = crate::internal::js_printer::LinkerOptions {
        require_or_import_meta_for_source: &require_or_import_meta_for_source,
        to_common_js_ref: runtime_refs.to_common_js_ref,
        to_esm_ref: runtime_refs.to_esm_ref,
        runtime_require_ref: runtime_refs.runtime_require_ref,
    };
    let printed = if options.source_map == crate::internal::config::SourceMap::None {
        crate::internal::js_printer::print_linked(&tree, renamer, print_options, linker_options)
    } else {
        crate::internal::js_printer::print_linked_with_source_map(
            &tree,
            renamer,
            print_options,
            linker_options,
            file.input_file
                .input_source_map
                .clone()
                .map(std::sync::Arc::new),
            crate::internal::sourcemap::generate_line_offset_tables(
                &file.input_file.source.contents,
                repr.ast.approximate_line_count.max(0),
            ),
        )
    };
    CompiledPartRange {
        source_index: part_range.source_index,
        js: printed.js,
        extracted_legal_comments: printed.extracted_legal_comments,
        json_metadata_imports: printed.json_metadata_imports,
        source_map_chunk: printed.source_map_chunk,
    }
}

/// Compile every ordered JavaScript part range assigned to `chunk`.
#[must_use]
pub fn compile_part_ranges_for_chunk(
    graph: &LinkerGraph,
    options: &Options,
    chunk: &ChunkInfo,
    runtime_refs: ChunkRuntimeRefs,
    renamer: &dyn crate::internal::renamer::Renamer,
) -> Vec<CompiledPartRange> {
    chunk
        .parts_in_chunk_in_order
        .iter()
        .copied()
        .map(|part_range| {
            compile_part_range_for_chunk(graph, options, part_range, runtime_refs, renamer)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EntryPointTailRefs {
    pub to_common_js_ref: Ref,
    pub unbound_module_ref: Ref,
}

/// Generate and print the format-specific tail for a JavaScript entry point.
///
/// # Panics
///
/// Panics when the source is not JavaScript, resolved exports are inconsistent,
/// or the printer encounters an unsupported AST invariant.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn generate_entry_point_tail(
    graph: &LinkerGraph,
    options: &Options,
    source_index: u32,
    refs: EntryPointTailRefs,
    renamer: &dyn crate::internal::renamer::Renamer,
) -> Vec<u8> {
    let file = &graph.files[source_index as usize];
    let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_ref() else {
        panic!("entry point must be JavaScript");
    };
    let location = crate::internal::logger::Loc::default();
    let mut statements = Vec::new();
    match options.output_format {
        Format::Preserve => {
            if repr.meta.wrap != WrapKind::None {
                statements.push(expr_statement(wrapper_call(repr.ast.wrapper_ref, location)));
            }
        }
        Format::Iife => {
            if repr.meta.wrap == WrapKind::Cjs {
                let call = wrapper_call(repr.ast.wrapper_ref, location);
                statements.push(if options.global_name.is_empty() {
                    expr_statement(call)
                } else {
                    js_ast::Stmt::new(
                        location,
                        js_ast::StmtData::Return(js_ast::ReturnStmt { value_or_nil: call }),
                    )
                });
            } else {
                if repr.meta.wrap == WrapKind::Esm {
                    statements.push(expr_statement(wrapper_call(repr.ast.wrapper_ref, location)));
                }
                if repr.meta.force_include_exports_for_entry_point {
                    statements.push(js_ast::Stmt::new(
                        location,
                        js_ast::StmtData::Return(js_ast::ReturnStmt {
                            value_or_nil: call_with_args(
                                refs.to_common_js_ref,
                                vec![identifier_expr(repr.ast.exports_ref, location)],
                                location,
                            ),
                        }),
                    ));
                }
            }
        }
        Format::CommonJs => {
            if repr.meta.wrap == WrapKind::Cjs {
                statements.push(js_ast::assign_stmt(
                    module_exports_expr(refs.unbound_module_ref, location),
                    wrapper_call(repr.ast.wrapper_ref, location),
                ));
            } else if repr.meta.wrap == WrapKind::Esm {
                statements.push(expr_statement(wrapper_call(repr.ast.wrapper_ref, location)));
            }
            if options.platform == crate::internal::config::Platform::Node {
                append_node_common_js_export_annotations(
                    &mut statements,
                    repr,
                    refs.unbound_module_ref,
                    options,
                );
            }
        }
        Format::EsModule => {
            if repr.meta.wrap == WrapKind::Cjs {
                statements.push(js_ast::Stmt::new(
                    location,
                    js_ast::StmtData::ExportDefault(js_ast::ExportDefaultStmt {
                        value: expr_statement(wrapper_call(repr.ast.wrapper_ref, location)),
                        ..js_ast::ExportDefaultStmt::default()
                    }),
                ));
            } else {
                if repr.meta.wrap == WrapKind::Esm {
                    let call = wrapper_call(repr.ast.wrapper_ref, location);
                    statements.push(expr_statement(
                        if repr.meta.is_async_or_has_async_dependency {
                            js_ast::Expr::new(
                                location,
                                js_ast::ExprData::Await(js_ast::AwaitExpr { value: call }),
                            )
                        } else {
                            call
                        },
                    ));
                }
                let mut items = Vec::new();
                for (index, alias) in repr
                    .meta
                    .sorted_and_filtered_export_aliases
                    .iter()
                    .enumerate()
                {
                    let mut export = repr.meta.resolved_exports[alias].clone();
                    let Some(InputFileRepr::Js(target)) = graph.files[export.source_index as usize]
                        .input_file
                        .repr
                        .as_ref()
                    else {
                        panic!("resolved export target must be JavaScript");
                    };
                    if let Some(import) = target.meta.imports_to_bind.get(&export.reference) {
                        export.reference = import.reference;
                        export.source_index = import.source_index;
                    }
                    if graph
                        .symbols
                        .get(export.reference)
                        .namespace_alias
                        .is_some()
                    {
                        let temp_ref = repr.meta.cjs_export_copies[index];
                        statements.push(js_ast::Stmt::new(
                            location,
                            js_ast::StmtData::Local(js_ast::LocalStmt {
                                declarations: vec![js_ast::Decl {
                                    binding: identifier_binding(temp_ref),
                                    value_or_nil: js_ast::Expr::new(
                                        location,
                                        js_ast::ExprData::ImportIdentifier(
                                            js_ast::ImportIdentifierExpr {
                                                reference: export.reference,
                                                ..js_ast::ImportIdentifierExpr::default()
                                            },
                                        ),
                                    ),
                                }],
                                ..js_ast::LocalStmt::default()
                            }),
                        ));
                        items.push(js_ast::ClauseItem {
                            name: LocRef {
                                reference: temp_ref,
                                ..LocRef::default()
                            },
                            alias: alias.clone(),
                            ..js_ast::ClauseItem::default()
                        });
                    } else {
                        items.push(js_ast::ClauseItem {
                            name: LocRef {
                                reference: export.reference,
                                ..LocRef::default()
                            },
                            alias: alias.clone(),
                            ..js_ast::ClauseItem::default()
                        });
                    }
                }
                if !items.is_empty() {
                    statements.push(js_ast::Stmt::new(
                        location,
                        js_ast::StmtData::ExportClause(js_ast::ExportClauseStmt {
                            items,
                            ..js_ast::ExportClauseStmt::default()
                        }),
                    ));
                }
            }
        }
    }
    if statements.is_empty() {
        return Vec::new();
    }
    let mut tree = repr.ast.clone();
    tree.directives.clear();
    tree.hashbang.clear();
    tree.parts = vec![js_ast::Part {
        statements,
        ..js_ast::Part::default()
    }];
    crate::internal::js_printer::print(
        &tree,
        renamer,
        crate::internal::js_printer::Options {
            unsupported_features: options.unsupported_js_features,
            line_limit: options.line_limit,
            indent: usize::from(options.output_format == Format::Iife),
            minify_syntax: options.minify_syntax,
            minify_whitespace: options.minify_whitespace,
            ascii_only: options.ascii_only,
            legal_comments: options.legal_comments,
            needs_metafile: options.needs_metafile,
            metafile_format: options.metafile_format,
        },
    )
    .js
}

fn expr_statement(value: js_ast::Expr) -> js_ast::Stmt {
    js_ast::Stmt::new(
        value.loc,
        js_ast::StmtData::Expr(js_ast::ExprStmt {
            value,
            ..js_ast::ExprStmt::default()
        }),
    )
}

fn identifier_expr(reference: Ref, location: crate::internal::logger::Loc) -> js_ast::Expr {
    js_ast::Expr::new(
        location,
        js_ast::ExprData::Identifier(js_ast::IdentifierExpr {
            reference,
            ..js_ast::IdentifierExpr::default()
        }),
    )
}

fn call_with_args(
    reference: Ref,
    args: Vec<js_ast::Expr>,
    location: crate::internal::logger::Loc,
) -> js_ast::Expr {
    js_ast::Expr::new(
        location,
        js_ast::ExprData::Call(js_ast::CallExpr {
            target: identifier_expr(reference, location),
            args,
            ..js_ast::CallExpr::default()
        }),
    )
}

fn module_exports_expr(reference: Ref, location: crate::internal::logger::Loc) -> js_ast::Expr {
    js_ast::Expr::new(
        location,
        js_ast::ExprData::Dot(js_ast::DotExpr {
            target: identifier_expr(reference, location),
            name: "exports".into(),
            ..js_ast::DotExpr::default()
        }),
    )
}

fn append_node_common_js_export_annotations(
    statements: &mut Vec<js_ast::Stmt>,
    repr: &crate::internal::graph::JsRepr,
    module_ref: Ref,
    options: &Options,
) {
    let mut properties = Vec::new();
    for alias in &repr.meta.sorted_and_filtered_export_aliases {
        if alias == "default" {
            continue;
        }
        let value_or_nil = if crate::internal::js_lexer::KEYWORDS.contains(&alias.as_str())
            || !js_ast::is_identifier(alias)
        {
            js_ast::Expr::new(
                crate::internal::logger::Loc::default(),
                js_ast::ExprData::Null,
            )
        } else {
            js_ast::Expr::default()
        };
        properties.push(js_ast::Property {
            key: js_ast::Expr::new(
                crate::internal::logger::Loc::default(),
                js_ast::ExprData::String(js_ast::StringExpr {
                    value: crate::internal::helpers::string_to_utf16(alias.as_bytes()),
                    ..js_ast::StringExpr::default()
                }),
            ),
            value_or_nil,
            ..js_ast::Property::default()
        });
    }
    for &import_record_index in &repr.ast.export_star_import_records {
        if !repr.ast.import_records[import_record_index as usize]
            .source_index
            .is_valid()
        {
            properties.push(js_ast::Property {
                kind: js_ast::PropertyKind::Spread,
                value_or_nil: js_ast::Expr::new(
                    crate::internal::logger::Loc::default(),
                    js_ast::ExprData::RequireString(js_ast::RequireStringExpr {
                        import_record_index,
                        ..js_ast::RequireStringExpr::default()
                    }),
                ),
                ..js_ast::Property::default()
            });
        }
    }
    if properties.is_empty() {
        return;
    }
    if !options.minify_whitespace {
        statements.push(js_ast::Stmt::new(
            crate::internal::logger::Loc::default(),
            js_ast::StmtData::Comment(js_ast::CommentStmt {
                text: "// Annotate the CommonJS export names for ESM import in node:".into(),
                ..js_ast::CommentStmt::default()
            }),
        ));
    }
    let location = crate::internal::logger::Loc::default();
    statements.push(expr_statement(js_ast::Expr::new(
        location,
        js_ast::ExprData::Binary(js_ast::BinaryExpr {
            left: js_ast::Expr::new(location, js_ast::ExprData::Number(0.0)),
            right: js_ast::assign(
                module_exports_expr(module_ref, location),
                js_ast::Expr::new(
                    location,
                    js_ast::ExprData::Object(js_ast::ObjectExpr {
                        properties,
                        ..js_ast::ObjectExpr::default()
                    }),
                ),
            ),
            op: js_ast::OpCode::BinaryLogicalAnd,
        }),
    )));
}

/// Generate the assignment prefix for an IIFE global name.
///
/// # Panics
///
/// Panics when the configured global name is empty.
#[must_use]
pub fn generate_global_name_prefix(options: &Options) -> String {
    let mut names = options.global_name.iter();
    let mut prefix = names.next().expect("global name must not be empty").clone();
    let mut names: Vec<_> = names.cloned().collect();
    let space = if options.minify_whitespace { "" } else { " " };
    let join = if options.minify_whitespace {
        ";"
    } else {
        ";\n"
    };
    let mut text = String::new();
    let mut is_existing_object = prefix == "this";
    if prefix == "import" && names.first().is_some_and(|name| name == "meta") {
        prefix = "import.meta".into();
        names.remove(0);
        is_existing_object = true;
    }

    if !names.is_empty()
        && !options
            .unsupported_js_features
            .contains(crate::internal::compat::JsFeature::LOGICAL_ASSIGNMENT)
    {
        if !is_existing_object {
            if can_use_global_name_identifier(&prefix) {
                prefix = escaped_global_name_identifier(&prefix, options);
                write!(text, "var {prefix}{join}").expect("writing to a string cannot fail");
            } else {
                prefix = format!("this[{}]", quoted_global_name(&prefix, options));
            }
        }
        for name in names {
            let accessor = global_name_accessor(&name, options);
            if is_existing_object {
                prefix.push_str(&accessor);
                is_existing_object = false;
            } else {
                prefix = format!("({prefix}{space}||={space}{{}}){accessor}");
            }
        }
        return format!("{text}{prefix}{space}={space}");
    }

    if is_existing_object {
        text = format!("{prefix}{space}={space}");
    } else if can_use_global_name_identifier(&prefix) {
        prefix = escaped_global_name_identifier(&prefix, options);
        text = format!("var {prefix}{space}={space}");
    } else {
        prefix = format!("this[{}]", quoted_global_name(&prefix, options));
        text = format!("{prefix}{space}={space}");
    }
    for name in names {
        let old_prefix = prefix.clone();
        prefix.push_str(&global_name_accessor(&name, options));
        write!(
            text,
            "{old_prefix}{space}||{space}{{}}{join}{prefix}{space}={space}"
        )
        .expect("writing to a string cannot fail");
    }
    text
}

fn can_use_global_name_identifier(name: &str) -> bool {
    js_ast::is_identifier(name) && !crate::internal::js_lexer::KEYWORDS.contains(&name)
}

fn escaped_global_name_identifier(name: &str, options: &Options) -> String {
    if options.ascii_only {
        String::from_utf8(crate::internal::js_printer::quote_identifier(
            Vec::new(),
            name,
            options.unsupported_js_features,
        ))
        .expect("quoted identifier is UTF-8")
    } else {
        name.into()
    }
}

fn quoted_global_name(name: &str, options: &Options) -> String {
    String::from_utf8(crate::internal::helpers::quote_for_json(
        name.as_bytes(),
        options.ascii_only,
    ))
    .expect("quoted JSON is UTF-8")
}

fn global_name_accessor(name: &str, options: &Options) -> String {
    if can_use_global_name_identifier(name) {
        format!(".{}", escaped_global_name_identifier(name, options))
    } else {
        format!("[{}]", quoted_global_name(name, options))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompileResultForSourceMap {
    pub source_map_chunk: SourceMapChunk,
    pub generated_offset: LineColumnOffset,
    pub source_index: u32,
    pub is_null_entry: bool,
}

#[derive(Clone, Debug)]
struct SourceMapItem {
    source: String,
    quoted_contents: Vec<u8>,
}

fn quoted_source_content(
    content: Option<&crate::internal::sourcemap::SourceContent>,
    ascii_only: bool,
) -> Vec<u8> {
    let Some(content) = content else {
        return b"null".to_vec();
    };
    if !content.quoted.is_empty() {
        return content.quoted.as_bytes().to_vec();
    }
    if content.value.is_empty() {
        b"null".to_vec()
    } else {
        quote_for_json(&utf16_to_string(&content.value), ascii_only)
    }
}

/// Compose per-file source-map chunks into one source map for an output chunk.
///
/// The returned map may be split around its mappings array so generated-column
/// offsets can be adjusted later when temporary output paths are substituted.
///
/// # Panics
///
/// Panics when source indexes or source-map chunks violate linker invariants,
/// or when source-map indexes exceed their signed 32-bit representation.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn generate_source_map_for_chunk(
    file_system: &dyn Fs,
    graph: &LinkerGraph,
    results: &[CompileResultForSourceMap],
    chunk_abs_dir: &str,
    options: &Options,
    can_have_shifts: bool,
) -> SourceMapPieces {
    let mut joiner = Joiner::default();
    joiner.add_string("{\n  \"version\": 3");

    let mut source_index_to_sources_index = HashMap::<u32, usize>::new();
    let mut items = Vec::<SourceMapItem>::new();
    let mut next_sources_index = 0;
    for result in results {
        if result.is_null_entry || source_index_to_sources_index.contains_key(&result.source_index)
        {
            continue;
        }
        source_index_to_sources_index.insert(result.source_index, next_sources_index);
        let file = &graph.files[result.source_index as usize].input_file;
        if let Some(source_map) = &file.input_source_map {
            for (index, source) in source_map.sources.iter().enumerate() {
                items.push(SourceMapItem {
                    source: source.clone(),
                    quoted_contents: if options.exclude_sources_content {
                        Vec::new()
                    } else {
                        quoted_source_content(
                            source_map.sources_content.get(index),
                            options.ascii_only,
                        )
                    },
                });
            }
            next_sources_index += source_map.sources.len();
        } else {
            let source = if file.source.key_path.namespace == "file" {
                file_system
                    .rel(chunk_abs_dir, &file.source.key_path.text)
                    .unwrap_or_else(|| file.source.key_path.text.clone())
                    .replace('\\', "/")
            } else if file.source.key_path.namespace.is_empty() {
                file.source.key_path.text.clone()
            } else {
                format!(
                    "{}:{}",
                    file.source.key_path.namespace, file.source.key_path.text
                )
            };
            items.push(SourceMapItem {
                source,
                quoted_contents: if options.exclude_sources_content {
                    Vec::new()
                } else {
                    quote_for_json(&file.source.contents, options.ascii_only)
                },
            });
            next_sources_index += 1;
        }
    }

    joiner.add_string(",\n  \"sources\": [");
    for (index, item) in items.iter().enumerate() {
        if index != 0 {
            joiner.add_string(", ");
        }
        joiner.add_bytes(quote_for_json(item.source.as_bytes(), options.ascii_only));
    }
    joiner.add_string("]");
    if !options.source_root.is_empty() {
        joiner.add_string(",\n  \"sourceRoot\": ");
        joiner.add_bytes(quote_for_json(
            options.source_root.as_bytes(),
            options.ascii_only,
        ));
    }
    if !options.exclude_sources_content {
        joiner.add_string(",\n  \"sourcesContent\": [");
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                joiner.add_string(", ");
            }
            joiner.add_bytes(item.quoted_contents.clone());
        }
        joiner.add_string("]");
    }

    joiner.add_string(",\n  \"mappings\": \"");
    let mappings_start = joiner.len() as usize;
    let mut previous_end_state = SourceMapState::default();
    let mut previous_column_offset = 0;
    let mut total_quoted_name_len = 0;
    for result in results {
        let mut chunk = result.source_map_chunk.clone();
        let offset = result.generated_offset;
        let sources_index = source_index_to_sources_index
            .get(&result.source_index)
            .copied()
            .unwrap_or_else(|| {
                assert!(
                    result.is_null_entry,
                    "missing source index for mapped chunk"
                );
                0
            });
        assert!(
            !chunk.should_ignore,
            "ignored source-map chunks must be filtered before composition"
        );
        let mut start_state = SourceMapState {
            source_index: i32::try_from(sources_index).expect("source index fits in i32"),
            generated_line: offset.lines,
            generated_column: offset.columns,
            original_name: total_quoted_name_len,
            ..SourceMapState::default()
        };
        if offset.lines == 0 {
            start_state.generated_column += previous_column_offset;
        }

        if result.is_null_entry {
            chunk.buffer = MappingsBuffer {
                data: b"A".to_vec(),
                ..MappingsBuffer::default()
            };
            append_source_map_chunk(&mut joiner, previous_end_state, start_state, &chunk.buffer);
            previous_end_state.generated_line = start_state.generated_line;
            previous_end_state.generated_column = start_state.generated_column;
        } else {
            append_source_map_chunk(&mut joiner, previous_end_state, start_state, &chunk.buffer);
            let previous_original_name = previous_end_state.original_name;
            previous_end_state = chunk.end_state;
            previous_end_state.source_index +=
                i32::try_from(sources_index).expect("source index fits in i32");
            if chunk.buffer.first_name_offset.is_valid() {
                previous_end_state.original_name += total_quoted_name_len;
            } else {
                previous_end_state.original_name = previous_original_name;
            }
            previous_column_offset = chunk.final_generated_column;
            total_quoted_name_len +=
                i32::try_from(chunk.quoted_names.len()).expect("name count fits in i32");
        }
        if previous_end_state.generated_line == 0 {
            previous_end_state.generated_column += start_state.generated_column;
            previous_column_offset += start_state.generated_column;
        }
    }
    let mappings_end = joiner.len() as usize;

    joiner.add_string("\",\n  \"names\": [");
    let mut is_first_name = true;
    for result in results {
        for quoted_name in &result.source_map_chunk.quoted_names {
            if is_first_name {
                is_first_name = false;
            } else {
                joiner.add_string(", ");
            }
            joiner.add_bytes(quoted_name.clone());
        }
    }
    joiner.add_string("]\n}\n");
    let bytes = joiner.done();
    if can_have_shifts {
        SourceMapPieces {
            prefix: bytes[..mappings_start].to_vec(),
            mappings: bytes[mappings_start..mappings_end].to_vec(),
            suffix: bytes[mappings_end..].to_vec(),
        }
    } else {
        SourceMapPieces {
            prefix: bytes,
            ..SourceMapPieces::default()
        }
    }
}

#[derive(Clone, Debug)]
struct LegalCommentEntry {
    source_index: u32,
    comments: Vec<String>,
}

#[derive(Clone, Debug)]
struct ThirdPartyLegalCommentEntry {
    package_paths: Vec<String>,
    comments: Vec<String>,
}

fn package_path_for_legal_comment(graph: &LinkerGraph, source_index: u32) -> String {
    let source = &graph.files[source_index as usize].input_file.source;
    if source.key_path.namespace == "dataurl" {
        return String::new();
    }
    let normalized = source.key_path.text.replace('\\', "/");
    let components: Vec<_> = normalized.split('/').collect();
    let Some(index) = components
        .iter()
        .rposition(|component| *component == "node_modules")
    else {
        return String::new();
    };
    if index + 1 == components.len() {
        String::new()
    } else {
        components[index + 1..].join("/")
    }
}

fn escape_url_path(path: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut escaped = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
                    | b'/'
            )
        {
            escaped.push(char::from(byte));
        } else {
            escaped.push('%');
            escaped.push(char::from(HEX[usize::from(byte >> 4)]));
            escaped.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    escaped
}

fn append_legal_comments(
    graph: &LinkerGraph,
    legal_comments: LegalComments,
    legal_comment_list: &[LegalCommentEntry],
    chunk: &mut ChunkInfo,
    joiner: &mut Joiner,
    slash_tag: &str,
) {
    if matches!(legal_comments, LegalComments::None | LegalComments::Inline) {
        return;
    }

    let mut unique_first_party_comments = Vec::new();
    let mut third_party_comments = Vec::<ThirdPartyLegalCommentEntry>::new();
    let mut has_first_party_comment = HashSet::new();
    for entry in legal_comment_list {
        let package_path = package_path_for_legal_comment(graph, entry.source_index);
        if package_path.is_empty() {
            for comment in &entry.comments {
                if has_first_party_comment.insert(comment.clone()) {
                    unique_first_party_comments.push(comment.clone());
                }
            }
        } else {
            third_party_comments.push(ThirdPartyLegalCommentEntry {
                package_paths: vec![package_path],
                comments: entry.comments.clone(),
            });
        }
    }

    let mut identical = HashMap::<String, usize>::new();
    let mut merged_third_party_comments = Vec::<ThirdPartyLegalCommentEntry>::new();
    for entry in third_party_comments {
        let key = entry.comments.join("\0");
        if let Some(&index) = identical.get(&key) {
            merged_third_party_comments[index]
                .package_paths
                .extend(entry.package_paths);
        } else {
            identical.insert(key, merged_third_party_comments.len());
            merged_third_party_comments.push(entry);
        }
    }

    match legal_comments {
        LegalComments::EndOfFile => {
            for comment in unique_first_party_comments {
                joiner.add_string(escape_closing_tag(&comment, slash_tag));
                joiner.add_string("\n");
            }
            if !merged_third_party_comments.is_empty() {
                joiner.add_string("/*! Bundled license information:\n");
                for entry in merged_third_party_comments {
                    joiner.add_string("\n");
                    for package_path in entry.package_paths {
                        joiner.add_string(format!(
                            "{}:\n",
                            escape_closing_tag(&package_path, slash_tag)
                        ));
                    }
                    for comment in entry.comments {
                        let comment = escape_closing_tag(&comment, slash_tag);
                        if let Some(comment) = comment.strip_prefix("//") {
                            joiner.add_string(format!("  (*{comment} *)\n"));
                        } else if comment.starts_with("/*") && comment.ends_with("*/") {
                            let comment = comment[1..comment.len() - 1].replace('\n', "\n  ");
                            joiner.add_string(format!("  ({comment})\n"));
                        }
                    }
                }
                joiner.add_string("*/\n");
            }
        }
        LegalComments::LinkedWithComment | LegalComments::ExternalWithoutComment => {
            let mut comments = Joiner::default();
            for comment in unique_first_party_comments {
                comments.add_string(comment);
                comments.add_string("\n");
            }
            if !merged_third_party_comments.is_empty() {
                if !comments.is_empty() {
                    comments.add_string("\n");
                }
                comments.add_string("Bundled license information:\n");
                for entry in merged_third_party_comments {
                    comments.add_string("\n");
                    for package_path in entry.package_paths {
                        comments.add_string(format!("{package_path}:\n"));
                    }
                    for comment in entry.comments {
                        comments.add_string(format!("  {}\n", comment.replace('\n', "\n  ")));
                    }
                }
            }
            chunk.external_legal_comments = comments.done();
        }
        LegalComments::None | LegalComments::Inline => unreachable!(),
    }
}

/// Join all generated JavaScript fragments for one chunk and split temporary
/// asset/chunk paths into intermediate output pieces.
///
/// Returns whether the chunk is executable due to an entry-point hashbang.
///
/// # Panics
///
/// Panics when an entry-point chunk is not JavaScript.
#[allow(clippy::too_many_lines)]
pub fn assemble_javascript_chunk(
    graph: &LinkerGraph,
    chunk: &mut ChunkInfo,
    compiled_parts: &[CompiledPartRange],
    bindings: &PrintedCrossChunkBindings,
    entry_point_tail: &[u8],
    options: &Options,
    output_paths: &OutputPathContext<'_>,
) -> bool {
    let mut joiner = Joiner::default();
    let mut legal_comment_list = Vec::new();
    let mut source_map_results = Vec::new();
    let mut metadata_imports = Vec::new();
    let mut metadata_inputs = Vec::<MetadataInput>::new();
    let mut metadata_input_indices = HashMap::<u32, usize>::new();
    let mut previous_offset = LineColumnOffset::default();
    let newline = if options.minify_whitespace { "" } else { "\n" };
    let space = if options.minify_whitespace { "" } else { " " };
    let mut newline_before_comment = false;
    let mut is_executable = false;
    chunk.external_legal_comments.clear();
    chunk.source_map_results.clear();
    chunk.metadata_imports.clear();
    chunk.metadata_inputs.clear();
    if options.needs_metafile {
        metadata_imports.extend(
            bindings
                .json_metadata_imports
                .iter()
                .map(|json| output_paths.break_output_into_pieces(json.as_bytes().to_vec())),
        );
    }

    if chunk.is_entry_point {
        let Some(InputFileRepr::Js(repr)) = graph.files[chunk.source_index as usize]
            .input_file
            .repr
            .as_ref()
        else {
            panic!("JavaScript entry chunk must reference JavaScript");
        };
        if !repr.ast.hashbang.is_empty() {
            let text = format!("#!{}\n", repr.ast.hashbang);
            previous_offset.advance_string(&text);
            joiner.add_string(text);
            newline_before_comment = true;
            is_executable = true;
        }
    }
    if !options.js_banner.is_empty() {
        previous_offset.advance_string(&options.js_banner);
        joiner.add_string(options.js_banner.clone());
        previous_offset.advance_string("\n");
        joiner.add_string("\n");
        newline_before_comment = true;
    }
    if chunk.is_entry_point {
        let Some(InputFileRepr::Js(repr)) = graph.files[chunk.source_index as usize]
            .input_file
            .repr
            .as_ref()
        else {
            unreachable!("entry chunk representation was checked above");
        };
        for directive in &repr.ast.directives {
            if directive != "use strict" || options.output_format != Format::EsModule {
                let quoted = crate::internal::helpers::quote_for_json(
                    directive.as_bytes(),
                    options.ascii_only,
                );
                previous_offset.advance_bytes(&quoted);
                joiner.add_bytes(quoted);
                previous_offset.advance_string(";");
                joiner.add_string(";");
                previous_offset.advance_string(newline);
                joiner.add_string(newline);
                newline_before_comment = true;
            }
        }
    }
    if options.output_format == Format::Iife {
        let mut opening = if options.global_name.is_empty() {
            String::new()
        } else {
            generate_global_name_prefix(options)
        };
        if options
            .unsupported_js_features
            .contains(crate::internal::compat::JsFeature::ARROW)
        {
            write!(opening, "(function(){space}{{{newline}")
                .expect("writing to a string cannot fail");
        } else {
            write!(opening, "((){space}=>{space}{{{newline}")
                .expect("writing to a string cannot fail");
        }
        previous_offset.advance_string(&opening);
        joiner.add_string(opening);
        newline_before_comment = false;
    }
    if !bindings.prefix.is_empty() {
        previous_offset.advance_bytes(&bindings.prefix);
        joiner.add_bytes(bindings.prefix.clone());
        newline_before_comment = true;
    }

    let mut previous_source = None;
    for compiled in compiled_parts {
        if options.needs_metafile {
            metadata_imports.extend(
                compiled
                    .json_metadata_imports
                    .iter()
                    .map(|json| output_paths.break_output_into_pieces(json.as_bytes().to_vec())),
            );
            if !graph.files[compiled.source_index as usize]
                .input_file
                .omit_from_source_maps_and_metafile
            {
                let index = *metadata_input_indices
                    .entry(compiled.source_index)
                    .or_insert_with(|| {
                        let index = metadata_inputs.len();
                        metadata_inputs.push(MetadataInput {
                            source_index: compiled.source_index,
                            ..MetadataInput::default()
                        });
                        index
                    });
                metadata_inputs[index]
                    .outputs
                    .push(output_paths.break_output_into_pieces(compiled.js.clone()));
            }
        }
        if !compiled.extracted_legal_comments.is_empty() {
            legal_comment_list.push(LegalCommentEntry {
                source_index: compiled.source_index,
                comments: compiled.extracted_legal_comments.clone(),
            });
        }
        if options.mode == Mode::Bundle
            && !options.minify_whitespace
            && previous_source != Some(compiled.source_index)
            && !compiled.js.is_empty()
        {
            if newline_before_comment {
                previous_offset.advance_string("\n");
                joiner.add_string("\n");
            }
            let path = graph.files[compiled.source_index as usize]
                .input_file
                .source
                .pretty_paths
                .select(options.code_path_style)
                .replace('\r', "\\r")
                .replace('\n', "\\n")
                .replace('\u{2028}', "\\u2028")
                .replace('\u{2029}', "\\u2029");
            let indent = if options.output_format == Format::Iife {
                "  "
            } else {
                ""
            };
            let text = format!("{indent}// {path}\n");
            previous_offset.advance_string(&text);
            joiner.add_string(text);
            previous_source = Some(compiled.source_index);
        }
        if !compiled.js.is_empty() {
            if options.source_map != SourceMapMode::None
                && !graph.files[compiled.source_index as usize]
                    .input_file
                    .omit_from_source_maps_and_metafile
            {
                if compiled.source_map_chunk.should_ignore {
                    let generated_offset = previous_offset;
                    previous_offset.advance_bytes(&compiled.js);
                    if source_map_results
                        .last()
                        .is_none_or(|result: &CompileResultForSourceMap| !result.is_null_entry)
                    {
                        source_map_results.push(CompileResultForSourceMap {
                            generated_offset,
                            source_index: compiled.source_index,
                            is_null_entry: true,
                            ..CompileResultForSourceMap::default()
                        });
                    }
                } else {
                    source_map_results.push(CompileResultForSourceMap {
                        source_map_chunk: compiled.source_map_chunk.clone(),
                        generated_offset: previous_offset,
                        source_index: compiled.source_index,
                        is_null_entry: false,
                    });
                    previous_offset = LineColumnOffset::default();
                }
            } else {
                previous_offset.advance_bytes(&compiled.js);
            }
            joiner.add_bytes(compiled.js.clone());
            newline_before_comment = true;
        }
    }
    if !entry_point_tail.is_empty() {
        joiner.add_bytes(entry_point_tail.to_vec());
        newline_before_comment = true;
    }
    if !bindings.suffix.is_empty() {
        if newline_before_comment {
            joiner.add_string(newline);
        }
        joiner.add_bytes(bindings.suffix.clone());
    }
    if options.output_format == Format::Iife {
        joiner.add_string(format!("}})();{newline}"));
    }
    joiner.ensure_newline_at_end();
    let slash_tag = if options
        .unsupported_js_features
        .contains(crate::internal::compat::JsFeature::INLINE_SCRIPT)
    {
        ""
    } else {
        "/script"
    };
    append_legal_comments(
        graph,
        options.legal_comments,
        &legal_comment_list,
        chunk,
        &mut joiner,
        slash_tag,
    );
    if !options.js_footer.is_empty() {
        joiner.add_string(options.js_footer.clone());
        joiner.add_string("\n");
    }
    chunk.intermediate_output = output_paths.break_joiner_into_pieces(joiner);
    chunk.source_map_results = source_map_results;
    chunk.metadata_imports = metadata_imports;
    chunk.metadata_inputs = metadata_inputs;
    chunk.is_executable = is_executable;
    is_executable
}

/// Compile and assemble one JavaScript chunk from its ordered linker state.
///
/// # Panics
///
/// Panics when `chunk_index` is invalid, the selected chunk is CSS, or any
/// underlying compilation invariant is violated.
#[allow(clippy::too_many_arguments)]
pub fn generate_javascript_chunk(
    graph: &LinkerGraph,
    chunks: &mut [ChunkInfo],
    chunk_index: usize,
    options: &Options,
    runtime_refs: ChunkRuntimeRefs,
    entry_point_refs: EntryPointTailRefs,
    renamer: &dyn crate::internal::renamer::Renamer,
    output_paths: &OutputPathContext<'_>,
) -> bool {
    assert!(!chunks[chunk_index].is_css, "chunk must be JavaScript");
    let bindings = print_cross_chunk_bindings(chunks, chunk_index, renamer, options);
    let compiled_parts =
        compile_part_ranges_for_chunk(graph, options, &chunks[chunk_index], runtime_refs, renamer);
    let entry_point_tail = if chunks[chunk_index].is_entry_point {
        generate_entry_point_tail(
            graph,
            options,
            chunks[chunk_index].source_index,
            entry_point_refs,
            renamer,
        )
    } else {
        Vec::new()
    };
    assemble_javascript_chunk(
        graph,
        &mut chunks[chunk_index],
        &compiled_parts,
        &bindings,
        &entry_point_tail,
        options,
        output_paths,
    )
}

/// Compile and assemble every JavaScript chunk in index order.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn generate_javascript_chunks(
    graph: &LinkerGraph,
    chunks: &mut [ChunkInfo],
    options: &Options,
    runtime_refs: ChunkRuntimeRefs,
    entry_point_refs: EntryPointTailRefs,
    renamer: &dyn crate::internal::renamer::Renamer,
    output_paths: &OutputPathContext<'_>,
) -> Vec<bool> {
    let mut executable = vec![false; chunks.len()];
    for chunk_index in 0..chunks.len() {
        if !chunks[chunk_index].is_css {
            executable[chunk_index] = generate_javascript_chunk(
                graph,
                chunks,
                chunk_index,
                options,
                runtime_refs,
                entry_point_refs,
                renamer,
                output_paths,
            );
        }
    }
    executable
}

/// Assign paths, hash, substitute temporary keys, and emit generated
/// JavaScript chunks as concrete output files.
///
/// # Panics
///
/// Panics if a CSS chunk is present or an output-path invariant is violated.
#[must_use]
pub fn finalize_generated_javascript_chunks(
    file_system: &dyn Fs,
    graph: &LinkerGraph,
    chunks: &mut [ChunkInfo],
    assets: &[Option<AssetPath>],
    options: &Options,
) -> Vec<OutputFile> {
    assert!(
        chunks.iter().all(|chunk| !chunk.is_css),
        "CSS chunks must be generated by the CSS pipeline"
    );
    assign_chunk_path_templates(file_system, graph, chunks, options);
    for chunk in &mut *chunks {
        generate_isolated_hash(graph, chunk, options);
    }
    finalize_javascript_chunk_outputs(file_system, graph, chunks, assets, options)
}

/// Assign the output path template for every JavaScript and CSS chunk, leaving the
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
        let standard_extension = if chunk.is_css {
            options.output_extension_css.clone()
        } else {
            options.output_extension_js.clone()
        };
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

fn append_source_map_outputs(
    file_system: &dyn Fs,
    chunk: &mut ChunkInfo,
    options: &Options,
    final_directory: &str,
    shifts: &[SourceMapShift],
    joiner: &mut Joiner,
    output_files: &mut Vec<OutputFile>,
) {
    if options.source_map == SourceMapMode::None || !chunk.output_source_map.has_content() {
        return;
    }
    let source_map = std::mem::take(&mut chunk.output_source_map).finalize(shifts);
    let source_map_rel_path = format!("{}.map", chunk.final_rel_path);
    let (comment_prefix, comment_suffix) = if chunk.is_css {
        ("/*", " */")
    } else {
        ("//", "")
    };
    match options.source_map {
        SourceMapMode::LinkedWithComment => {
            let import_path = path_between_chunks(
                file_system,
                &options.public_path,
                final_directory,
                &source_map_rel_path,
            )
            .expect("source map output path must have a relative path");
            let import_path = import_path.strip_prefix("./").unwrap_or(&import_path);
            joiner.ensure_newline_at_end();
            joiner.add_string(format!(
                "{comment_prefix}# sourceMappingURL={}{comment_suffix}\n",
                escape_url_path(import_path),
            ));
        }
        SourceMapMode::Inline | SourceMapMode::InlineAndExternal => {
            joiner.ensure_newline_at_end();
            joiner.add_string(format!(
                "{comment_prefix}# sourceMappingURL=data:application/json;base64,"
            ));
            joiner.add_string(STANDARD.encode(&source_map));
            joiner.add_string(format!("{comment_suffix}\n"));
        }
        SourceMapMode::ExternalWithoutComment | SourceMapMode::None => {}
    }
    if matches!(
        options.source_map,
        SourceMapMode::LinkedWithComment
            | SourceMapMode::ExternalWithoutComment
            | SourceMapMode::InlineAndExternal
    ) {
        output_files.push(OutputFile {
            abs_path: file_system.join(&[
                options.abs_output_dir.as_str(),
                source_map_rel_path.as_str(),
            ]),
            json_metadata_chunk: options.metafile_format.maybe_remove_whitespace(&format!(
                "{{\n      \"imports\": [],\n      \"exports\": [],\n      \"inputs\": {{}},\n      \"bytes\": {}\n    }}",
                source_map.len()
            )),
            contents: source_map,
            ..OutputFile::default()
        });
    }
}

fn metafile_output_path(file_system: &dyn Fs, options: &Options, rel_path: &str) -> String {
    let absolute = file_system.join(&[&options.abs_output_dir, rel_path]);
    if options.metafile_path_style == crate::internal::logger::PathStyle::Absolute {
        absolute
    } else {
        file_system
            .rel(file_system.cwd(), &absolute)
            .unwrap_or(absolute)
            .replace('\\', "/")
    }
}

#[allow(clippy::too_many_lines)]
fn generate_javascript_metadata_chunk(
    file_system: &dyn Fs,
    graph: &LinkerGraph,
    chunk: &mut ChunkInfo,
    output_paths: &OutputPathContext<'_>,
    options: &Options,
    final_directory: &str,
    final_output_size: usize,
) -> String {
    if !options.needs_metafile {
        return String::new();
    }
    let fragment = |text: &str| options.metafile_format.maybe_remove_whitespace(text);
    let mut joiner = Joiner::default();
    joiner.add_string(fragment("{\n      \"imports\": ["));
    let mut is_first = true;
    for import in std::mem::take(&mut chunk.metadata_imports) {
        if is_first {
            is_first = false;
        } else {
            joiner.add_string(",");
        }
        let (import, _) = output_paths.substitute_final_paths(import, |target_path| {
            metafile_output_path(file_system, options, target_path)
        });
        joiner.add_bytes(import.done());
    }
    if !is_first {
        joiner.add_string(fragment("\n      "));
    }

    joiner.add_string(fragment("],\n      \"exports\": ["));
    let mut exports = Vec::new();
    if options.output_format.keep_esm_import_export_syntax() {
        if chunk.is_entry_point {
            if let Some(InputFileRepr::Js(repr)) = graph.files[chunk.source_index as usize]
                .input_file
                .repr
                .as_ref()
            {
                if repr.meta.wrap == WrapKind::Cjs {
                    exports.push("default".to_string());
                } else {
                    exports.extend(repr.meta.resolved_exports.keys().cloned());
                }
            }
        } else {
            exports.extend(chunk.exports_to_other_chunks.values().cloned());
        }
    }
    exports.sort();
    exports.dedup();
    for (index, alias) in exports.iter().enumerate() {
        if index != 0 {
            joiner.add_string(",");
        }
        joiner.add_string(fragment("\n        "));
        joiner.add_bytes(quote_for_json(alias.as_bytes(), options.ascii_only));
    }
    if !exports.is_empty() {
        joiner.add_string(fragment("\n      "));
    }
    joiner.add_string(fragment("],\n"));

    let include_entry_point = chunk.is_entry_point
        && (!chunk.is_css
            || matches!(
                graph.files[chunk.source_index as usize]
                    .input_file
                    .repr
                    .as_ref(),
                Some(InputFileRepr::Css(_))
            ));
    if include_entry_point {
        let entry_point = graph.files[chunk.source_index as usize]
            .input_file
            .source
            .pretty_paths
            .select(options.metafile_path_style);
        joiner.add_string(fragment("      \"entryPoint\": "));
        joiner.add_bytes(quote_for_json(entry_point.as_bytes(), options.ascii_only));
        joiner.add_string(fragment(",\n"));
    }
    if !chunk.is_css && chunk.css_chunk_index.is_valid() {
        let css_chunk = &output_paths.chunks[chunk.css_chunk_index.get_index() as usize];
        let css_bundle = metafile_output_path(file_system, options, &css_chunk.final_rel_path);
        joiner.add_string(fragment("      \"cssBundle\": "));
        joiner.add_bytes(quote_for_json(css_bundle.as_bytes(), options.ascii_only));
        joiner.add_string(fragment(",\n"));
    }
    joiner.add_string(fragment("      \"inputs\": {"));
    for (index, input) in chunk.metadata_inputs.iter().enumerate() {
        if index != 0 {
            joiner.add_string(",");
        }
        let bytes_in_output = input
            .outputs
            .iter()
            .map(|output| {
                output_paths.accurate_final_byte_count(output, |target_path| {
                    path_between_chunks(
                        file_system,
                        &options.public_path,
                        final_directory,
                        target_path,
                    )
                    .expect("metadata input paths must have a relative path")
                })
            })
            .sum::<usize>();
        let input_path = graph.files[input.source_index as usize]
            .input_file
            .source
            .pretty_paths
            .select(options.metafile_path_style);
        joiner.add_string(fragment("\n        "));
        joiner.add_bytes(quote_for_json(input_path.as_bytes(), options.ascii_only));
        joiner.add_string(fragment(": {\n          \"bytesInOutput\": "));
        joiner.add_string(bytes_in_output.to_string());
        joiner.add_string(fragment("\n        }"));
    }
    if !chunk.metadata_inputs.is_empty() {
        joiner.add_string(fragment("\n      "));
    }
    joiner.add_string(fragment("},\n      \"bytes\": "));
    joiner.add_string(final_output_size.to_string());
    joiner.add_string(fragment("\n    }"));
    String::from_utf8(joiner.done()).expect("metadata JSON is UTF-8")
}

/// Substitute final chunk and asset paths and emit concrete output files.
///
/// # Panics
///
/// Panics when output paths cannot be made relative, a temporary path index is
/// invalid, or an asset marker violates linker invariants.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn finalize_javascript_chunk_outputs(
    file_system: &dyn Fs,
    graph: &LinkerGraph,
    chunks: &mut [ChunkInfo],
    assets: &[Option<AssetPath>],
    options: &Options,
) -> Vec<OutputFile> {
    if options.source_map != SourceMapMode::None {
        for chunk in &mut *chunks {
            if !chunk.source_map_results.is_empty() {
                let tentative_rel_path = template_to_string(&substitute_template(
                    &chunk.final_template,
                    &PathPlaceholders::default(),
                ));
                let tentative_rel_dir = file_system.dir(&tentative_rel_path);
                let chunk_abs_dir =
                    file_system.join(&[&options.abs_output_dir, &tentative_rel_dir]);
                let results = std::mem::take(&mut chunk.source_map_results);
                chunk.output_source_map = generate_source_map_for_chunk(
                    file_system,
                    graph,
                    &results,
                    &chunk_abs_dir,
                    options,
                    chunk.intermediate_output.pieces.is_some(),
                );
            }
            if chunk.output_source_map.has_content() {
                generate_isolated_hash(graph, chunk, options);
            }
        }
    }
    finalize_chunk_paths(file_system, graph, chunks, options);
    let chunk_paths: Vec<_> = chunks
        .iter()
        .map(|chunk| ChunkPath {
            unique_key: chunk.unique_key.clone(),
            final_rel_path: chunk.final_rel_path.clone(),
        })
        .collect();
    let output_paths = OutputPathContext::new("", assets, &chunk_paths);
    let mut output_files = Vec::new();
    for chunk in chunks {
        if chunk.is_css {
            for entry in &chunk.imports_in_css_order {
                if entry.kind == CssImportKind::SourceIndex {
                    output_files.extend(
                        graph.files[entry.source_index as usize]
                            .input_file
                            .additional_files
                            .clone(),
                    );
                }
            }
        } else {
            for &source_index in &chunk.files_in_chunk_in_order {
                output_files.extend(
                    graph.files[source_index as usize]
                        .input_file
                        .additional_files
                        .clone(),
                );
            }
        }
        let final_directory = file_system.dir(&chunk.final_rel_path);
        let intermediate_output = std::mem::take(&mut chunk.intermediate_output);
        let (mut joiner, shifts) =
            output_paths.substitute_final_paths(intermediate_output, |target_path| {
                path_between_chunks(
                    file_system,
                    &options.public_path,
                    &final_directory,
                    target_path,
                )
                .expect("chunk output paths must have a relative path")
            });

        if !chunk.external_legal_comments.is_empty() {
            let legal_rel_path = format!("{}.LEGAL.txt", chunk.final_rel_path);
            if options.legal_comments == LegalComments::LinkedWithComment {
                let import_path = path_between_chunks(
                    file_system,
                    &options.public_path,
                    &final_directory,
                    &legal_rel_path,
                )
                .expect("legal comment output path must have a relative path");
                joiner.ensure_newline_at_end();
                joiner.add_string(format!(
                    "/*! For license information please see {} */\n",
                    import_path.strip_prefix("./").unwrap_or(&import_path)
                ));
            }
            let legal_contents = chunk.external_legal_comments.clone();
            output_files.push(OutputFile {
                abs_path: file_system.join(&[
                    options.abs_output_dir.as_str(),
                    legal_rel_path.as_str(),
                ]),
                json_metadata_chunk: options.metafile_format.maybe_remove_whitespace(&format!(
                    "{{\n      \"imports\": [],\n      \"exports\": [],\n      \"inputs\": {{}},\n      \"bytes\": {}\n    }}",
                    legal_contents.len()
                )),
                contents: legal_contents,
                ..OutputFile::default()
            });
        }

        append_source_map_outputs(
            file_system,
            chunk,
            options,
            &final_directory,
            &shifts,
            &mut joiner,
            &mut output_files,
        );

        let json_metadata_chunk = generate_javascript_metadata_chunk(
            file_system,
            graph,
            chunk,
            &output_paths,
            options,
            &final_directory,
            joiner.len() as usize,
        );
        output_files.push(OutputFile {
            abs_path: file_system.join(&[
                options.abs_output_dir.as_str(),
                chunk.final_rel_path.as_str(),
            ]),
            contents: joiner.done(),
            json_metadata_chunk,
            is_executable: chunk.is_executable,
        });
    }
    output_files
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

/// Return the wrapper metadata the JavaScript printer needs when lowering a
/// linked `require()` or `import()` expression.
///
/// # Panics
///
/// Panics when `source_index` does not refer to a JavaScript input file.
#[must_use]
pub fn require_or_import_meta_for_source(
    graph: &LinkerGraph,
    source_index: u32,
) -> crate::internal::js_printer::RequireOrImportMeta {
    let file = &graph.files[usize::try_from(source_index).expect("source index fits in usize")];
    let Some(InputFileRepr::Js(repr)) = file.input_file.repr.as_ref() else {
        panic!("linked require/import target must be JavaScript");
    };
    crate::internal::js_printer::RequireOrImportMeta {
        wrapper_ref: repr.ast.wrapper_ref,
        exports_ref: if repr.meta.wrap == WrapKind::Esm {
            repr.ast.exports_ref
        } else {
            INVALID_REF
        },
        is_wrapper_async: repr.meta.is_async_or_has_async_dependency,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::{
        AmbiguousReExport, ArbitraryNamespaceIssue, AssetPath, ChunkImport, ChunkInfo, ChunkPath,
        ChunkRuntimeRefs, CompileResultForSourceMap, CompiledCssAst, CrossChunkImport,
        CrossChunkImportItem, CssImportKind, EntryPointTailRefs, ImportStatus, ImportTracker,
        MatchImportKind, OutputPathContext, OutputPiece, OutputPieceIndexKind, PartRange,
        PreparedCssAst, RuntimeReExportContext, StableRef, add_exports_for_export_star,
        advance_import_tracker, append_or_extend_part_range, arbitrary_namespace_export_issues,
        assemble_css_chunk, assemble_javascript_chunk, assign_chunk_path_templates,
        bind_imports_to_exports_for_file, bind_imports_to_parts_for_file,
        chunk_runtime_refs_from_graph, classify_module_wrappers, compile_part_range_for_chunk,
        compile_part_ranges_for_chunk, compile_prepared_css_asts, compute_chunks,
        compute_cross_chunk_dependencies, compute_js_chunks, configure_entry_point_exports,
        convert_import_for_chunk, convert_stmts_for_chunk, create_entry_point_part,
        create_exports_for_file, create_wrapper_for_file, encode_import_constraints_for_file,
        enforce_no_cyclic_chunk_imports, finalize_chunk_paths,
        finalize_generated_javascript_chunks, finalize_javascript_chunk_outputs,
        finalize_part_dependencies_for_file, find_imported_css_files_in_js_order,
        find_imported_files_in_css_order, generate_code_for_lazy_exports,
        generate_common_js_export_copies, generate_cross_chunk_stmts, generate_css_chunk,
        generate_css_module_exports, generate_entry_point_tail, generate_global_name_prefix,
        generate_isolated_hash, generate_javascript_chunk, generate_javascript_chunks,
        generate_source_map_for_chunk, has_dynamic_exports_due_to_export_star,
        import_conditions_are_equal, inline_linked_assets, is_conditional_import_redundant,
        join_with_public_path, lower_common_js_lazy_export, lower_esm_lazy_export,
        mangle_local_css, mangle_props, mark_file_live_for_tree_shaking, match_import_with_export,
        maybe_correct_export_typo, merge_adjacent_local_stmts, path_between_chunks,
        populate_css_stub_lazy_export, prepare_css_asts, prevent_exports_from_being_renamed,
        print_cross_chunk_bindings, propagate_wrappers_and_dynamic_exports,
        recursively_wrap_dependencies, require_or_import_meta_for_source, resolve_export_stars,
        runtime_symbol_ref, scan_runtime_refs_from_graph, sort_and_filter_export_aliases,
        sorted_cross_chunk_export_items, sorted_cross_chunk_imports, strip_exports_from_stmts,
        tree_shaking_and_code_splitting, wrap_common_js_stmts, wrap_esm_stmts,
        wrap_rules_with_conditions,
    };
    use crate::internal::{
        ast::{
            ImportItemStatus, ImportKind, ImportRecord, ImportRecordFlags, Index32, Ref, Symbol,
            SymbolKind,
        },
        config::{
            Format, LegalComments, Loader, MetafileFormat, Mode, Options, PathPlaceholder,
            PathTemplate, SourceMap as SourceMapMode, template_to_string,
        },
        css_ast::{
            AtImportRule, ImportConditions, MediaArbitraryTokensQuery, MediaQuery, MediaQueryData,
            Rule, RuleData, Token, WhitespaceFlags,
        },
        css_lexer::TokenKind,
        fs::{MockKind, mock_fs},
        graph::{
            CopyRepr, CssRepr, EntryPoint, InputFile, InputFileRepr, JsRepr, OutputFile,
            SideEffects, SideEffectsKind, WrapKind, clone_linker_graph,
        },
        helpers::Joiner,
        js_ast::{self, ExportsKind, NamedExport, NamedImport},
        js_parser,
        logger::{DeferLogKind, Loc, Log, Path, PrettyPaths, Range, Source},
        sourcemap::{
            LineColumnOffset, SourceMapPieces, SourceMapShift, generate_line_offset_tables,
            make_chunk_builder,
        },
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
    fn entry_point_export_configuration_matches_output_semantics() {
        let input_files = [js_file(js_ast::Ast {
            has_lazy_export: true,
            export_keyword: Range {
                loc: Loc::default(),
                len: 6,
            },
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(
            &input_files,
            &[0],
            &[EntryPoint {
                source_index: 0,
                ..EntryPoint::default()
            }],
            false,
        );

        configure_entry_point_exports(
            &mut graph,
            &Options {
                mode: Mode::ConvertFormat,
                output_format: Format::CommonJs,
                ..Options::default()
            },
        );

        let repr = js_repr(&graph, 0);
        assert_eq!(repr.ast.exports_kind, ExportsKind::CommonJs);
        assert!(repr.ast.uses_exports_ref);
        assert!(repr.meta.force_include_exports_for_entry_point);
    }

    #[test]
    fn pass_through_preserves_module_exports_and_script_globals() {
        let exported_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let input_files = [js_file(js_ast::Ast {
            symbols: vec![Symbol::new(SymbolKind::Other, "exported")],
            parts: vec![js_ast::Part {
                statements: vec![js_ast::Stmt::new(
                    Loc::default(),
                    js_ast::StmtData::Local(js_ast::LocalStmt {
                        declarations: vec![js_ast::Decl {
                            binding: js_ast::Binding {
                                data: Some(Box::new(js_ast::BindingData::Identifier(
                                    js_ast::IdentifierBinding {
                                        reference: exported_ref,
                                    },
                                ))),
                                ..js_ast::Binding::default()
                            },
                            ..js_ast::Decl::default()
                        }],
                        is_export: true,
                        ..js_ast::LocalStmt::default()
                    }),
                )],
                ..js_ast::Part::default()
            }],
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(&input_files, &[0], &[], false);
        prevent_exports_from_being_renamed(&mut graph, 0);
        assert!(
            graph
                .symbols
                .get(exported_ref)
                .flags
                .contains(crate::internal::ast::SymbolFlags::MUST_NOT_BE_RENAMED)
        );

        let global_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let module_scope = std::sync::Arc::new(std::sync::Mutex::new(js_ast::Scope {
            members: HashMap::from([(
                "global".into(),
                js_ast::ScopeMember {
                    reference: global_ref,
                    ..js_ast::ScopeMember::default()
                },
            )]),
            ..js_ast::Scope::default()
        }));
        let input_files = [js_file(js_ast::Ast {
            symbols: vec![Symbol::new(SymbolKind::Other, "global")],
            module_scope: Some(module_scope),
            parts: vec![js_ast::Part::default()],
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(&input_files, &[0], &[], false);
        prevent_exports_from_being_renamed(&mut graph, 0);
        assert!(
            graph
                .symbols
                .get(global_ref)
                .flags
                .contains(crate::internal::ast::SymbolFlags::MUST_NOT_BE_RENAMED)
        );
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
    fn css_chunks_use_the_configured_css_extension() {
        let graph = clone_linker_graph(&[], &[], &[], false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let mut chunks = vec![ChunkInfo {
            is_css: true,
            ..ChunkInfo::default()
        }];
        assign_chunk_path_templates(
            &file_system,
            &graph,
            &mut chunks,
            &Options {
                chunk_path_template: vec![PathTemplate {
                    placeholder: PathPlaceholder::Name,
                    ..PathTemplate::default()
                }],
                output_extension_css: ".bundle.css".into(),
                ..Options::default()
            },
        );
        assert_eq!(
            template_to_string(&chunks[0].final_template),
            "chunk.bundle.css"
        );
    }

    #[test]
    fn secondary_css_chunk_ignores_javascript_outfile_extension() {
        let input_files = [js_file(js_ast::Ast::default())];
        let entry_points = [EntryPoint::default()];
        let graph = clone_linker_graph(&input_files, &[0], &entry_points, false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let mut chunks = vec![ChunkInfo {
            is_entry_point: true,
            is_css: true,
            ..ChunkInfo::default()
        }];
        assign_chunk_path_templates(
            &file_system,
            &graph,
            &mut chunks,
            &Options {
                abs_output_file: "/out/custom.cjs".into(),
                entry_path_template: vec![PathTemplate {
                    placeholder: PathPlaceholder::Name,
                    ..PathTemplate::default()
                }],
                output_extension_css: ".css".into(),
                ..Options::default()
            },
        );
        assert_eq!(template_to_string(&chunks[0].final_template), "custom.css");
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
            None,
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
    fn statement_conversion_reports_arbitrary_import_names() {
        let input_files = [js_file(js_ast::Ast {
            import_records: vec![ImportRecord::default()],
            ..js_ast::Ast::default()
        })];
        let graph = clone_linker_graph(&input_files, &[0], &[EntryPoint::default()], false);
        let statements = [js_ast::Stmt::new(
            Loc::default(),
            js_ast::StmtData::Import(js_ast::ImportStmt {
                items: Some(vec![js_ast::ClauseItem {
                    alias: "not-valid".into(),
                    alias_loc: Loc { start: 9 },
                    ..js_ast::ClauseItem::default()
                }]),
                ..js_ast::ImportStmt::default()
            }),
        )];

        let converted = convert_stmts_for_chunk(
            &graph,
            &Options {
                mode: Mode::PassThrough,
                output_format: Format::EsModule,
                unsupported_js_features:
                    crate::internal::compat::JsFeature::ARBITRARY_MODULE_NAMESPACE_NAMES,
                ..Options::default()
            },
            0,
            &statements,
            None,
        );

        assert_eq!(converted.arbitrary_namespace_issues.len(), 1);
        assert_eq!(
            converted.arbitrary_namespace_issues[0],
            ArbitraryNamespaceIssue {
                kind: "import".into(),
                source_index: 0,
                name_loc: Loc { start: 9 },
                alias: "not-valid".into(),
            }
        );
    }

    #[test]
    fn runtime_export_star_uses_namespace_and_commonjs_targets() {
        let exports_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let namespace_ref = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let runtime_ref = Ref {
            source_index: 0,
            inner_index: 2,
        };
        let module_ref = Ref {
            source_index: 0,
            inner_index: 3,
        };
        let input_files = [js_file(js_ast::Ast {
            exports_ref,
            import_records: vec![ImportRecord {
                flags: ImportRecordFlags::CALLS_RUN_TIME_RE_EXPORT_FN,
                ..ImportRecord::default()
            }],
            ..js_ast::Ast::default()
        })];
        let graph = clone_linker_graph(&input_files, &[0], &[EntryPoint::default()], false);
        let statements = [js_ast::Stmt::new(
            Loc::default(),
            js_ast::StmtData::ExportStar(js_ast::ExportStarStmt {
                namespace_ref,
                ..js_ast::ExportStarStmt::default()
            }),
        )];
        let context = RuntimeReExportContext {
            re_export_ref: runtime_ref,
            unbound_module_ref: Some(module_ref),
        };

        let esm = convert_stmts_for_chunk(
            &graph,
            &Options {
                mode: Mode::Bundle,
                output_format: Format::EsModule,
                ..Options::default()
            },
            0,
            &statements,
            Some(context),
        );
        assert_eq!(esm.inside_wrapper_prefix.len(), 1);
        assert!(matches!(
            esm.inside_wrapper_suffix[0].data.as_deref(),
            Some(js_ast::StmtData::Import(import))
                if import.namespace_ref == namespace_ref && import.star_name_loc.is_some()
        ));
        let Some(js_ast::StmtData::Expr(call_statement)) =
            esm.inside_wrapper_prefix[0].data.as_deref()
        else {
            panic!("runtime re-export call");
        };
        let Some(js_ast::ExprData::Call(call)) = call_statement.value.data.as_deref() else {
            panic!("runtime re-export call expression");
        };
        assert_eq!(call.args.len(), 2);
        assert!(matches!(
            call.args[1].data.as_deref(),
            Some(js_ast::ExprData::Identifier(identifier))
                if identifier.reference == namespace_ref
        ));

        let common_js = convert_stmts_for_chunk(
            &graph,
            &Options {
                mode: Mode::Bundle,
                output_format: Format::CommonJs,
                ..Options::default()
            },
            0,
            &statements,
            Some(context),
        );
        assert!(common_js.inside_wrapper_suffix.is_empty());
        let Some(js_ast::StmtData::Expr(call_statement)) =
            common_js.inside_wrapper_prefix[0].data.as_deref()
        else {
            panic!("runtime re-export call");
        };
        let Some(js_ast::ExprData::Call(call)) = call_statement.value.data.as_deref() else {
            panic!("runtime re-export call expression");
        };
        assert_eq!(call.args.len(), 3);
        assert!(matches!(
            call.args[1].data.as_deref(),
            Some(js_ast::ExprData::RequireString(_))
        ));
        assert!(matches!(
            call.args[2].data.as_deref(),
            Some(js_ast::ExprData::Dot(dot)) if dot.name == "exports"
        ));
    }

    #[test]
    fn commonjs_wrapper_selects_arguments_and_function_shape() {
        let ast = js_ast::Ast {
            exports_ref: Ref {
                source_index: 0,
                inner_index: 0,
            },
            module_ref: Ref {
                source_index: 0,
                inner_index: 1,
            },
            wrapper_ref: Ref {
                source_index: 0,
                inner_index: 2,
            },
            uses_exports_ref: true,
            uses_module_ref: true,
            ..js_ast::Ast::default()
        };
        let runtime_ref = Ref {
            source_index: 0,
            inner_index: 3,
        };
        let wrapper = |options: &Options| {
            let statements = wrap_common_js_stmts(
                &ast,
                vec![js_ast::Stmt::default()],
                Vec::new(),
                runtime_ref,
                options,
                "src/file.js",
            );
            let Some(js_ast::StmtData::Local(local)) = statements[0].data.as_deref() else {
                panic!("wrapper declaration");
            };
            let Some(js_ast::ExprData::Call(call)) =
                local.declarations[0].value_or_nil.data.as_deref()
            else {
                panic!("runtime call");
            };
            call.args[0].clone()
        };

        let arrow = wrapper(&Options::default());
        assert!(matches!(
            arrow.data.as_deref(),
            Some(js_ast::ExprData::Arrow(arrow)) if arrow.args.len() == 2
        ));

        let function = wrapper(&Options {
            unsupported_js_features: crate::internal::compat::JsFeature::ARROW,
            ..Options::default()
        });
        assert!(matches!(
            function.data.as_deref(),
            Some(js_ast::ExprData::Function(function)) if function.function.args.len() == 2
        ));

        let profiled = wrapper(&Options {
            profiler_names: true,
            ..Options::default()
        });
        let Some(js_ast::ExprData::Object(object)) = profiled.data.as_deref() else {
            panic!("profiled wrapper object");
        };
        assert_eq!(object.properties.len(), 1);
        assert_eq!(object.properties[0].kind, js_ast::PropertyKind::Method);
        assert!(matches!(
            object.properties[0].value_or_nil.data.as_deref(),
            Some(js_ast::ExprData::Function(function)) if function.function.args.len() == 2
        ));
    }

    #[test]
    fn esm_wrapper_hoists_declarations_and_preserves_async() {
        let local_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let wrapper_ref = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let ast = js_ast::Ast {
            wrapper_ref,
            ..js_ast::Ast::default()
        };
        let body = vec![
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Function(js_ast::FunctionStmt::default()),
            ),
            js_ast::Stmt::new(
                Loc::default(),
                js_ast::StmtData::Local(js_ast::LocalStmt {
                    declarations: vec![js_ast::Decl {
                        binding: js_ast::Binding {
                            data: Some(Box::new(js_ast::BindingData::Identifier(
                                js_ast::IdentifierBinding {
                                    reference: local_ref,
                                },
                            ))),
                            ..js_ast::Binding::default()
                        },
                        value_or_nil: js_ast::Expr::new(
                            Loc::default(),
                            js_ast::ExprData::Number(1.0),
                        ),
                    }],
                    ..js_ast::LocalStmt::default()
                }),
            ),
        ];
        let runtime_ref = Ref {
            source_index: 0,
            inner_index: 2,
        };
        let wrapped = wrap_esm_stmts(
            &ast,
            body.clone(),
            Vec::new(),
            runtime_ref,
            &Options::default(),
            "src/file.js",
            true,
        );
        assert_eq!(wrapped.len(), 3);
        assert!(matches!(
            wrapped[0].data.as_deref(),
            Some(js_ast::StmtData::Function(_))
        ));
        let Some(js_ast::StmtData::Local(hoisted)) = wrapped[1].data.as_deref() else {
            panic!("hoisted declarations");
        };
        assert_eq!(hoisted.declarations.len(), 1);
        let Some(js_ast::StmtData::Local(initializer)) = wrapped[2].data.as_deref() else {
            panic!("initializer declaration");
        };
        let Some(js_ast::ExprData::Call(call)) =
            initializer.declarations[0].value_or_nil.data.as_deref()
        else {
            panic!("initializer call");
        };
        assert!(matches!(
            call.args[0].data.as_deref(),
            Some(js_ast::ExprData::Arrow(arrow))
                if arrow.is_async
                    && matches!(
                        arrow.body.block.statements[0].data.as_deref(),
                        Some(js_ast::StmtData::Expr(_))
                    )
        ));

        let minified = wrap_esm_stmts(
            &ast,
            body,
            Vec::new(),
            runtime_ref,
            &Options {
                minify_syntax: true,
                ..Options::default()
            },
            "src/file.js",
            false,
        );
        assert_eq!(minified.len(), 2);
        let Some(js_ast::StmtData::Local(combined)) = minified[1].data.as_deref() else {
            panic!("combined declaration");
        };
        assert_eq!(combined.declarations.len(), 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn part_range_compilation_filters_dead_parts_and_emits_wrappers() {
        let input_files = [js_file(js_ast::Ast {
            parts: vec![
                js_ast::Part::default(),
                js_ast::Part {
                    statements: vec![
                        js_ast::Stmt::new(
                            Loc::default(),
                            js_ast::StmtData::Comment(js_ast::CommentStmt {
                                text: "/*! legal */".into(),
                                is_legal_comment: true,
                            }),
                        ),
                        js_ast::Stmt::new(
                            Loc::default(),
                            js_ast::StmtData::Expr(js_ast::ExprStmt {
                                value: js_ast::Expr::new(
                                    Loc::default(),
                                    js_ast::ExprData::Number(1.0),
                                ),
                                ..js_ast::ExprStmt::default()
                            }),
                        ),
                    ],
                    is_live: true,
                    ..js_ast::Part::default()
                },
                js_ast::Part {
                    statements: vec![js_ast::Stmt::new(
                        Loc::default(),
                        js_ast::StmtData::Expr(js_ast::ExprStmt {
                            value: js_ast::Expr::new(Loc::default(), js_ast::ExprData::Number(2.0)),
                            ..js_ast::ExprStmt::default()
                        }),
                    )],
                    ..js_ast::Part::default()
                },
            ],
            ..js_ast::Ast::default()
        })];
        let graph = clone_linker_graph(&input_files, &[0], &[], false);
        let renamer = crate::internal::renamer::new_no_op_renamer(graph.symbols.clone());
        let result = compile_part_range_for_chunk(
            &graph,
            &Options {
                mode: Mode::Bundle,
                legal_comments: LegalComments::EndOfFile,
                source_map: crate::internal::config::SourceMap::ExternalWithoutComment,
                ..Options::default()
            },
            PartRange {
                source_index: 0,
                part_index_begin: 1,
                part_index_end: 3,
            },
            ChunkRuntimeRefs::default(),
            &renamer,
        );
        assert_eq!(result.js, b"1;\n");
        assert_eq!(result.extracted_legal_comments, ["/*! legal */"]);
        assert!(!result.source_map_chunk.should_ignore);

        let wrapper_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let runtime_ref = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let input_files = [js_file(js_ast::Ast {
            symbols: vec![
                Symbol::new(SymbolKind::Other, "require_file"),
                Symbol::new(SymbolKind::Other, "__commonJS"),
            ],
            wrapper_ref,
            parts: vec![
                js_ast::Part::default(),
                js_ast::Part {
                    statements: vec![js_ast::Stmt::new(
                        Loc::default(),
                        js_ast::StmtData::Expr(js_ast::ExprStmt {
                            value: js_ast::Expr::new(Loc::default(), js_ast::ExprData::Number(1.0)),
                            ..js_ast::ExprStmt::default()
                        }),
                    )],
                    is_live: true,
                    ..js_ast::Part::default()
                },
                js_ast::Part {
                    is_live: true,
                    ..js_ast::Part::default()
                },
            ],
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(&input_files, &[0], &[], false);
        let Some(InputFileRepr::Js(repr)) = graph.files[0].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.wrap = WrapKind::Cjs;
        repr.meta.wrapper_part_index = Index32::new(2);
        let renamer = crate::internal::renamer::new_no_op_renamer(graph.symbols.clone());
        let result = compile_part_range_for_chunk(
            &graph,
            &Options {
                mode: Mode::Bundle,
                ..Options::default()
            },
            PartRange {
                source_index: 0,
                part_index_begin: 0,
                part_index_end: 3,
            },
            ChunkRuntimeRefs {
                common_js_ref: runtime_ref,
                ..ChunkRuntimeRefs::default()
            },
            &renamer,
        );
        let output = String::from_utf8(result.js).expect("UTF-8");
        assert!(output.contains("var require_file = __commonJS("));
        assert!(output.contains("1;"));
    }

    #[test]
    fn entry_point_tail_covers_all_output_formats() {
        let wrapper_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let to_common_js_ref = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let module_ref = Ref {
            source_index: 0,
            inner_index: 2,
        };
        let input_files = [js_file(js_ast::Ast {
            symbols: vec![
                Symbol::new(SymbolKind::Other, "require_entry"),
                Symbol::new(SymbolKind::Other, "__toCommonJS"),
                Symbol::new(SymbolKind::Unbound, "module"),
            ],
            wrapper_ref,
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(&input_files, &[0], &[EntryPoint::default()], false);
        let Some(InputFileRepr::Js(repr)) = graph.files[0].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.wrap = WrapKind::Cjs;
        let renamer = crate::internal::renamer::new_no_op_renamer(graph.symbols.clone());
        let refs = EntryPointTailRefs {
            to_common_js_ref,
            unbound_module_ref: module_ref,
        };
        let tail = |format, global_name: Vec<String>| {
            String::from_utf8(generate_entry_point_tail(
                &graph,
                &Options {
                    output_format: format,
                    global_name,
                    ..Options::default()
                },
                0,
                refs,
                &renamer,
            ))
            .expect("UTF-8")
        };
        assert_eq!(tail(Format::Preserve, Vec::new()), "require_entry();\n");
        assert_eq!(
            tail(Format::Iife, vec!["Bundle".into()]),
            "  return require_entry();\n"
        );
        assert_eq!(
            tail(Format::CommonJs, Vec::new()),
            "module.exports = require_entry();\n"
        );
        assert_eq!(
            tail(Format::EsModule, Vec::new()),
            "export default require_entry();\n"
        );
    }

    #[test]
    fn esm_entry_point_tail_exports_resolved_symbols() {
        let export_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let input_files = [js_file(js_ast::Ast {
            symbols: vec![Symbol::new(SymbolKind::Other, "foo")],
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(&input_files, &[0], &[EntryPoint::default()], false);
        let Some(InputFileRepr::Js(repr)) = graph.files[0].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.sorted_and_filtered_export_aliases = vec!["foo".into()];
        repr.meta.resolved_exports.insert(
            "foo".into(),
            crate::internal::graph::ExportData {
                reference: export_ref,
                source_index: 0,
                ..crate::internal::graph::ExportData::default()
            },
        );
        let renamer = crate::internal::renamer::new_no_op_renamer(graph.symbols.clone());
        assert_eq!(
            generate_entry_point_tail(
                &graph,
                &Options {
                    output_format: Format::EsModule,
                    ..Options::default()
                },
                0,
                EntryPointTailRefs::default(),
                &renamer,
            ),
            b"export { foo };\n"
        );
    }

    #[test]
    fn global_name_prefix_matches_upstream_shapes() {
        let prefix = |global_name: &[&str], minify_whitespace| {
            generate_global_name_prefix(&Options {
                global_name: global_name.iter().map(|name| (*name).into()).collect(),
                minify_whitespace,
                ..Options::default()
            })
        };
        assert_eq!(prefix(&["Bundle"], false), "var Bundle = ");
        assert_eq!(
            prefix(&["Bundle", "lib"], false),
            "var Bundle;\n(Bundle ||= {}).lib = "
        );
        assert_eq!(prefix(&["this", "App"], false), "this.App = ");
        assert_eq!(
            prefix(&["import", "meta", "App"], false),
            "import.meta.App = "
        );
        assert_eq!(prefix(&["not-valid"], false), "this[\"not-valid\"] = ");
        assert_eq!(
            prefix(&["Bundle", "lib"], true),
            "var Bundle;(Bundle||={}).lib="
        );
    }

    #[test]
    fn assembles_javascript_chunk_in_upstream_order() {
        let mut input = js_file(js_ast::Ast {
            hashbang: "usr/bin/env node".into(),
            directives: vec!["use strict".into(), "custom".into()],
            ..js_ast::Ast::default()
        });
        input.source.pretty_paths.rel = "src/entry.js".into();
        let graph = clone_linker_graph(&[input], &[0], &[EntryPoint::default()], false);
        let mut chunk = ChunkInfo {
            is_entry_point: true,
            ..ChunkInfo::default()
        };
        let options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            global_name: vec!["Bundle".into()],
            js_banner: "/* banner */".into(),
            js_footer: "/* footer */".into(),
            ..Options::default()
        };
        let executable = assemble_javascript_chunk(
            &graph,
            &mut chunk,
            &[super::CompiledPartRange {
                source_index: 0,
                js: b"  work();\n".to_vec(),
                extracted_legal_comments: Vec::new(),
                json_metadata_imports: Vec::new(),
                source_map_chunk: super::SourceMapChunk::default(),
            }],
            &super::PrintedCrossChunkBindings::default(),
            b"  return 1;\n",
            &options,
            &context(&[], &[]),
        );
        assert!(executable);
        let (joiner, _) =
            context(&[], &[]).substitute_final_paths(chunk.intermediate_output, str::to_owned);
        let output = String::from_utf8(joiner.done()).expect("UTF-8");
        assert!(output.starts_with(
            "#!usr/bin/env node\n/* banner */\n\"use strict\";\n\"custom\";\nvar Bundle = (() => {\n"
        ));
        assert!(output.contains("  // src/entry.js\n  work();\n  return 1;\n"));
        assert!(output.ends_with("})();\n/* footer */\n"));
    }

    #[test]
    fn composes_per_file_source_map_chunks() {
        let mut input = js_file(js_ast::Ast::default());
        input.source = Source {
            contents: std::sync::Arc::from(b"let alpha = 1;\n".as_slice()),
            key_path: Path {
                text: "/project/src/input.js".into(),
                namespace: "file".into(),
                ..Path::default()
            },
            ..Source::default()
        };
        let graph = clone_linker_graph(&[input], &[0], &[], false);
        let mut builder = make_chunk_builder(
            None,
            generate_line_offset_tables(&graph.files[0].input_file.source.contents, 2),
            false,
        );
        builder.add_source_mapping(Loc::default(), "alpha", b"");
        let source_map_chunk = builder.generate_chunk(b"alpha");
        assert!(!source_map_chunk.should_ignore);
        let results = [
            CompileResultForSourceMap {
                source_map_chunk,
                generated_offset: LineColumnOffset {
                    lines: 1,
                    columns: 0,
                },
                source_index: 0,
                is_null_entry: false,
            },
            CompileResultForSourceMap {
                generated_offset: LineColumnOffset {
                    lines: 0,
                    columns: 2,
                },
                source_index: 0,
                is_null_entry: true,
                ..CompileResultForSourceMap::default()
            },
        ];
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let pieces = generate_source_map_for_chunk(
            &file_system,
            &graph,
            &results,
            "/project/dist",
            &Options {
                source_root: "/root".into(),
                ..Options::default()
            },
            true,
        );
        assert!(!pieces.prefix.is_empty());
        assert!(!pieces.mappings.is_empty());
        assert!(!pieces.suffix.is_empty());
        let map = String::from_utf8(pieces.finalize(&[SourceMapShift::default()]))
            .expect("source map is UTF-8");
        assert!(map.contains("\"version\": 3"));
        assert!(map.contains("\"sources\": [\"../src/input.js\"]"));
        assert!(map.contains("\"sourceRoot\": \"/root\""));
        assert!(map.contains("\"sourcesContent\": [\"let alpha = 1;\\n\"]"));
        assert!(map.contains("\"names\": [\"alpha\"]"));
        assert!(map.ends_with("]\n}\n"));
    }

    #[test]
    fn assembles_and_emits_linked_javascript_source_maps() {
        let mut input = js_file(js_ast::Ast::default());
        input.source = Source {
            contents: std::sync::Arc::from(b"run();\n".as_slice()),
            pretty_paths: PrettyPaths {
                rel: "src/input.js".into(),
                ..PrettyPaths::default()
            },
            key_path: Path {
                text: "/project/src/input.js".into(),
                namespace: "file".into(),
                ..Path::default()
            },
            ..Source::default()
        };
        let graph = clone_linker_graph(&[input], &[0], &[], false);
        let mut builder = make_chunk_builder(
            None,
            generate_line_offset_tables(&graph.files[0].input_file.source.contents, 1),
            false,
        );
        builder.add_source_mapping(Loc::default(), "", b"");
        let mut chunk = ChunkInfo {
            final_template: vec![PathTemplate {
                data: "dist/app file.js".into(),
                ..PathTemplate::default()
            }],
            ..ChunkInfo::default()
        };
        let options = Options {
            abs_output_dir: "/project/out".into(),
            mode: Mode::Bundle,
            js_banner: "/* banner */".into(),
            source_map: SourceMapMode::LinkedWithComment,
            ..Options::default()
        };
        assemble_javascript_chunk(
            &graph,
            &mut chunk,
            &[super::CompiledPartRange {
                source_index: 0,
                js: b"run();\n".to_vec(),
                source_map_chunk: builder.generate_chunk(b"run();\n"),
                ..super::CompiledPartRange::default()
            }],
            &super::PrintedCrossChunkBindings::default(),
            &[],
            &options,
            &context(&[], &[]),
        );
        assert_eq!(chunk.source_map_results.len(), 1);
        assert_eq!(
            chunk.source_map_results[0].generated_offset,
            LineColumnOffset {
                lines: 3,
                columns: 0
            }
        );

        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/project");
        let outputs = finalize_javascript_chunk_outputs(
            &file_system,
            &graph,
            std::slice::from_mut(&mut chunk),
            &[],
            &options,
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].abs_path, "/project/out/dist/app file.js.map");
        let source_map = String::from_utf8(outputs[0].contents.clone()).expect("source map UTF-8");
        assert!(source_map.contains("\"sources\": [\"../../src/input.js\"]"));
        assert!(!source_map.contains("\"mappings\": \"\""));
        assert_eq!(outputs[1].abs_path, "/project/out/dist/app file.js");
        assert!(
            outputs[1]
                .contents
                .ends_with(b"//# sourceMappingURL=app%20file.js.map\n")
        );
    }

    #[test]
    fn emits_inline_and_external_source_map_modes() {
        let graph = clone_linker_graph(&[], &[], &[], false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let make_chunk = || {
            let mut joiner = Joiner::default();
            joiner.add_string("code();");
            ChunkInfo {
                final_template: vec![PathTemplate {
                    data: "app.js".into(),
                    ..PathTemplate::default()
                }],
                intermediate_output: super::IntermediateOutput::without_substitutions(joiner),
                output_source_map: SourceMapPieces {
                    prefix: b"{\"version\":3,\"sources\":[],\"mappings\":\"\",\"names\":[]}\n"
                        .to_vec(),
                    ..SourceMapPieces::default()
                },
                ..ChunkInfo::default()
            }
        };
        for (mode, output_count, has_inline_comment) in [
            (SourceMapMode::Inline, 1, true),
            (SourceMapMode::InlineAndExternal, 2, true),
            (SourceMapMode::ExternalWithoutComment, 2, false),
        ] {
            let mut chunk = make_chunk();
            let outputs = finalize_javascript_chunk_outputs(
                &file_system,
                &graph,
                std::slice::from_mut(&mut chunk),
                &[],
                &Options {
                    abs_output_dir: "/out".into(),
                    source_map: mode,
                    ..Options::default()
                },
            );
            assert_eq!(outputs.len(), output_count);
            let main = outputs.last().expect("main output");
            assert_eq!(
                main.contents
                    .windows(b"sourceMappingURL=data:".len())
                    .any(|window| window == b"sourceMappingURL=data:"),
                has_inline_comment
            );
            if output_count == 2 {
                assert_eq!(outputs[0].abs_path, "/out/app.js.map");
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn finalizes_javascript_chunk_metafile_metadata() {
        let mut input = js_file(js_ast::Ast::default());
        input.source.pretty_paths.rel = "src/entry.js".into();
        let mut graph = clone_linker_graph(&[input], &[0], &[EntryPoint::default()], false);
        let Some(InputFileRepr::Js(repr)) = graph.files[0].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta
            .resolved_exports
            .insert("foo".into(), super::ExportData::default());
        let temporary_paths = [
            ChunkPath {
                unique_key: "UNIQUEC00000000".into(),
                ..ChunkPath::default()
            },
            ChunkPath {
                unique_key: "UNIQUEC00000001".into(),
                ..ChunkPath::default()
            },
        ];
        let options = Options {
            abs_output_dir: "/out".into(),
            mode: Mode::Bundle,
            output_format: Format::EsModule,
            needs_metafile: true,
            ..Options::default()
        };
        let mut entry = ChunkInfo {
            unique_key: temporary_paths[0].unique_key.clone(),
            final_template: vec![PathTemplate {
                data: "entry.js".into(),
                ..PathTemplate::default()
            }],
            is_entry_point: true,
            ..ChunkInfo::default()
        };
        assemble_javascript_chunk(
            &graph,
            &mut entry,
            &[super::CompiledPartRange {
                source_index: 0,
                js: b"load(\"UNIQUEC00000001\");\n".to_vec(),
                json_metadata_imports: vec![
                    "\n        {\n          \"path\": \"external package\",\n          \"kind\": \"dynamic-import\",\n          \"external\": true\n        }"
                        .into(),
                ],
                ..super::CompiledPartRange::default()
            }],
            &super::PrintedCrossChunkBindings {
                prefix: b"import \"UNIQUEC00000001\";\n".to_vec(),
                json_metadata_imports: vec![
                    "\n        {\n          \"path\": \"UNIQUEC00000001\",\n          \"kind\": \"import-statement\"\n        }"
                        .into(),
                ],
                ..super::PrintedCrossChunkBindings::default()
            },
            &[],
            &options,
            &context(&[], &temporary_paths),
        );
        let mut dependency_joiner = Joiner::default();
        dependency_joiner.add_string("export const dep = 1;\n");
        let mut chunks = vec![
            entry,
            ChunkInfo {
                unique_key: temporary_paths[1].unique_key.clone(),
                final_template: vec![PathTemplate {
                    data: "chunk file.js".into(),
                    ..PathTemplate::default()
                }],
                intermediate_output: context(&[], &temporary_paths)
                    .break_joiner_into_pieces(dependency_joiner),
                ..ChunkInfo::default()
            },
        ];
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let outputs =
            finalize_javascript_chunk_outputs(&file_system, &graph, &mut chunks, &[], &options);
        assert_eq!(outputs.len(), 2);
        let entry_output = &outputs[0];
        let metadata = &entry_output.json_metadata_chunk;
        assert!(metadata.contains("\"path\": \"out/chunk file.js\""));
        assert!(metadata.contains("\"path\": \"external package\""));
        assert!(metadata.contains("\"external\": true"));
        assert!(
            metadata.contains("\"exports\": [\n        \"foo\"\n      ]"),
            "{metadata}"
        );
        assert!(metadata.contains("\"entryPoint\": \"src/entry.js\""));
        assert!(metadata.contains("\"inputs\": {\n        \"src/entry.js\""));
        assert!(metadata.contains("\"bytesInOutput\": 25"));
        assert!(metadata.contains(&format!("\"bytes\": {}", entry_output.contents.len())));

        let mut minified_chunk = ChunkInfo {
            final_template: vec![PathTemplate {
                data: "min.js".into(),
                ..PathTemplate::default()
            }],
            ..ChunkInfo::default()
        };
        assemble_javascript_chunk(
            &graph,
            &mut minified_chunk,
            &[super::CompiledPartRange {
                source_index: 0,
                js: b"x();\n".to_vec(),
                ..super::CompiledPartRange::default()
            }],
            &super::PrintedCrossChunkBindings::default(),
            &[],
            &Options {
                needs_metafile: true,
                metafile_format: MetafileFormat::Minified,
                ..options.clone()
            },
            &context(&[], &[]),
        );
        let minified = finalize_javascript_chunk_outputs(
            &file_system,
            &graph,
            std::slice::from_mut(&mut minified_chunk),
            &[],
            &Options {
                needs_metafile: true,
                metafile_format: MetafileFormat::Minified,
                ..options
            },
        );
        assert!(!minified[0].json_metadata_chunk.contains('\n'));
        assert!(minified[0].json_metadata_chunk.contains("\"src/entry.js\""));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn groups_and_emits_javascript_legal_comments() {
        let input = |path: &str| {
            let mut file = js_file(js_ast::Ast::default());
            file.source = Source {
                key_path: Path {
                    text: path.into(),
                    namespace: "file".into(),
                    ..Path::default()
                },
                ..Source::default()
            };
            file
        };
        let graph = clone_linker_graph(
            &[
                input("/project/src/entry.js"),
                input("/project/node_modules/pkg-a/index.js"),
                input(r"C:\project\node_modules\pkg-b\main.js"),
            ],
            &[0, 1, 2],
            &[],
            false,
        );
        let compiled_parts = [
            super::CompiledPartRange {
                source_index: 0,
                extracted_legal_comments: vec![
                    "/*! first </script> */".into(),
                    "/*! first </script> */".into(),
                ],
                ..super::CompiledPartRange::default()
            },
            super::CompiledPartRange {
                source_index: 1,
                extracted_legal_comments: vec!["/*! dep */".into(), "// line".into()],
                ..super::CompiledPartRange::default()
            },
            super::CompiledPartRange {
                source_index: 2,
                extracted_legal_comments: vec!["/*! dep */".into(), "// line".into()],
                ..super::CompiledPartRange::default()
            },
        ];

        let mut end_of_file_chunk = ChunkInfo::default();
        assemble_javascript_chunk(
            &graph,
            &mut end_of_file_chunk,
            &compiled_parts,
            &super::PrintedCrossChunkBindings::default(),
            &[],
            &Options {
                legal_comments: LegalComments::EndOfFile,
                ..Options::default()
            },
            &context(&[], &[]),
        );
        let (joiner, _) = context(&[], &[])
            .substitute_final_paths(end_of_file_chunk.intermediate_output, str::to_owned);
        assert_eq!(
            joiner.done(),
            b"/*! first <\\/script> */\n/*! Bundled license information:\n\npkg-a/index.js:\npkg-b/main.js:\n  (*! dep *)\n  (* line *)\n*/\n"
        );

        let mut linked_chunk = ChunkInfo {
            final_template: vec![PathTemplate {
                data: "nested/app.js".into(),
                ..PathTemplate::default()
            }],
            ..ChunkInfo::default()
        };
        let linked_options = Options {
            abs_output_dir: "/out".into(),
            legal_comments: LegalComments::LinkedWithComment,
            ..Options::default()
        };
        assemble_javascript_chunk(
            &graph,
            &mut linked_chunk,
            &compiled_parts,
            &super::PrintedCrossChunkBindings::default(),
            &[],
            &linked_options,
            &context(&[], &[]),
        );
        assert_eq!(
            linked_chunk.external_legal_comments,
            b"/*! first </script> */\n\nBundled license information:\n\npkg-a/index.js:\npkg-b/main.js:\n  /*! dep */\n  // line\n"
        );
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let outputs = finalize_javascript_chunk_outputs(
            &file_system,
            &graph,
            std::slice::from_mut(&mut linked_chunk),
            &[],
            &linked_options,
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].abs_path, "/out/nested/app.js.LEGAL.txt");
        assert_eq!(
            outputs[0].contents,
            b"/*! first </script> */\n\nBundled license information:\n\npkg-a/index.js:\npkg-b/main.js:\n  /*! dep */\n  // line\n"
        );
        assert_eq!(outputs[1].abs_path, "/out/nested/app.js");
        assert_eq!(
            outputs[1].contents,
            b"/*! For license information please see app.js.LEGAL.txt */\n"
        );

        let mut external_chunk = ChunkInfo {
            final_template: vec![PathTemplate {
                data: "external.js".into(),
                ..PathTemplate::default()
            }],
            ..ChunkInfo::default()
        };
        let external_options = Options {
            abs_output_dir: "/out".into(),
            legal_comments: LegalComments::ExternalWithoutComment,
            ..Options::default()
        };
        assemble_javascript_chunk(
            &graph,
            &mut external_chunk,
            &compiled_parts,
            &super::PrintedCrossChunkBindings::default(),
            &[],
            &external_options,
            &context(&[], &[]),
        );
        let outputs = finalize_javascript_chunk_outputs(
            &file_system,
            &graph,
            std::slice::from_mut(&mut external_chunk),
            &[],
            &external_options,
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].abs_path, "/out/external.js.LEGAL.txt");
        assert_eq!(outputs[1].abs_path, "/out/external.js");
        assert!(outputs[1].contents.is_empty());
    }

    #[test]
    fn finalizes_temporary_chunk_paths_into_output_files() {
        let graph = clone_linker_graph(&[], &[], &[], false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let temporary_paths = [
            ChunkPath {
                unique_key: "UNIQUEC00000000".into(),
                ..ChunkPath::default()
            },
            ChunkPath {
                unique_key: "UNIQUEC00000001".into(),
                ..ChunkPath::default()
            },
        ];
        let mut entry_joiner = Joiner::default();
        entry_joiner.add_string("import \"UNIQUEC00000001\";\n");
        let mut dependency_joiner = Joiner::default();
        dependency_joiner.add_string("console.log(1);\n");
        let mut chunks = vec![
            ChunkInfo {
                unique_key: temporary_paths[0].unique_key.clone(),
                final_template: vec![PathTemplate {
                    data: "entry.js".into(),
                    ..PathTemplate::default()
                }],
                intermediate_output: context(&[], &temporary_paths)
                    .break_joiner_into_pieces(entry_joiner),
                is_executable: true,
                ..ChunkInfo::default()
            },
            ChunkInfo {
                unique_key: temporary_paths[1].unique_key.clone(),
                final_template: vec![PathTemplate {
                    data: "chunk.js".into(),
                    ..PathTemplate::default()
                }],
                intermediate_output: context(&[], &temporary_paths)
                    .break_joiner_into_pieces(dependency_joiner),
                ..ChunkInfo::default()
            },
        ];
        let outputs = finalize_javascript_chunk_outputs(
            &file_system,
            &graph,
            &mut chunks,
            &[],
            &Options {
                abs_output_dir: "/out".into(),
                ..Options::default()
            },
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].abs_path, "/out/entry.js");
        assert_eq!(outputs[0].contents, b"import \"./chunk.js\";\n");
        assert!(outputs[0].is_executable);
        assert_eq!(outputs[1].abs_path, "/out/chunk.js");
        assert_eq!(outputs[1].contents, b"console.log(1);\n");
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

    #[test]
    fn require_or_import_metadata_distinguishes_common_js_and_esm_wrappers() {
        let common_js_wrapper = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let common_js_exports = Ref {
            source_index: 0,
            inner_index: 2,
        };
        let esm_wrapper = Ref {
            source_index: 1,
            inner_index: 3,
        };
        let esm_exports = Ref {
            source_index: 1,
            inner_index: 4,
        };
        let input_files = [
            js_file(js_ast::Ast {
                wrapper_ref: common_js_wrapper,
                exports_ref: common_js_exports,
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                wrapper_ref: esm_wrapper,
                exports_ref: esm_exports,
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &[], false);
        let Some(InputFileRepr::Js(common_js)) = graph.files[0].input_file.repr.as_mut() else {
            panic!("JavaScript representation");
        };
        common_js.meta.wrap = WrapKind::Cjs;
        let Some(InputFileRepr::Js(esm)) = graph.files[1].input_file.repr.as_mut() else {
            panic!("JavaScript representation");
        };
        esm.meta.wrap = WrapKind::Esm;
        esm.meta.is_async_or_has_async_dependency = true;

        assert_eq!(
            require_or_import_meta_for_source(&graph, 0),
            crate::internal::js_printer::RequireOrImportMeta {
                wrapper_ref: common_js_wrapper,
                exports_ref: crate::internal::ast::INVALID_REF,
                is_wrapper_async: false,
            }
        );
        assert_eq!(
            require_or_import_meta_for_source(&graph, 1),
            crate::internal::js_printer::RequireOrImportMeta {
                wrapper_ref: esm_wrapper,
                exports_ref: esm_exports,
                is_wrapper_async: true,
            }
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn compiles_ordered_part_ranges_with_linked_import_metadata() {
        let wrapper_ref = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let input_files = [
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                import_records: vec![ImportRecord {
                    source_index: Index32::new(2),
                    kind: ImportKind::Require,
                    ..ImportRecord::default()
                }],
                parts: vec![
                    js_ast::Part::default(),
                    js_ast::Part {
                        statements: vec![js_ast::Stmt::new(
                            Loc::default(),
                            js_ast::StmtData::Expr(js_ast::ExprStmt {
                                value: js_ast::Expr::new(
                                    Loc::default(),
                                    js_ast::ExprData::RequireString(js_ast::RequireStringExpr {
                                        import_record_index: 0,
                                        ..js_ast::RequireStringExpr::default()
                                    }),
                                ),
                                ..js_ast::ExprStmt::default()
                            }),
                        )],
                        is_live: true,
                        ..js_ast::Part::default()
                    },
                ],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "require_dep")],
                wrapper_ref,
                parts: vec![
                    js_ast::Part::default(),
                    js_ast::Part {
                        statements: vec![js_ast::Stmt::new(
                            Loc::default(),
                            js_ast::StmtData::Expr(js_ast::ExprStmt {
                                value: js_ast::Expr::new(
                                    Loc::default(),
                                    js_ast::ExprData::Number(2.0),
                                ),
                                ..js_ast::ExprStmt::default()
                            }),
                        )],
                        is_live: true,
                        ..js_ast::Part::default()
                    },
                ],
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &[], false);
        let Some(InputFileRepr::Js(target)) = graph.files[2].input_file.repr.as_mut() else {
            panic!("JavaScript target");
        };
        target.meta.wrap = WrapKind::Cjs;
        let renamer = crate::internal::renamer::new_no_op_renamer(graph.symbols.clone());
        let chunk = ChunkInfo {
            parts_in_chunk_in_order: vec![
                PartRange {
                    source_index: 2,
                    part_index_begin: 1,
                    part_index_end: 2,
                },
                PartRange {
                    source_index: 1,
                    part_index_begin: 1,
                    part_index_end: 2,
                },
            ],
            ..ChunkInfo::default()
        };

        let compiled = compile_part_ranges_for_chunk(
            &graph,
            &Options::default(),
            &chunk,
            ChunkRuntimeRefs::default(),
            &renamer,
        );
        assert_eq!(compiled.len(), 2);
        assert_eq!(compiled[0].source_index, 2);
        assert_eq!(compiled[0].js, b"2;\n");
        assert_eq!(compiled[1].source_index, 1);
        assert_eq!(compiled[1].js, b"require_dep();\n");

        let mut chunks = vec![chunk];
        assert!(!generate_javascript_chunk(
            &graph,
            &mut chunks,
            0,
            &Options::default(),
            ChunkRuntimeRefs::default(),
            EntryPointTailRefs::default(),
            &renamer,
            &context(&[], &[]),
        ));
        assert_eq!(
            std::mem::take(&mut chunks[0].intermediate_output.joiner).done(),
            b"2;\nrequire_dep();\n"
        );
    }

    #[test]
    fn runtime_reference_bundles_come_from_runtime_exports() {
        let names = [
            "__export",
            "__commonJS",
            "__esm",
            "__toCommonJS",
            "__toESM",
            "__require",
            "__reExport",
        ];
        let references: Vec<_> = (0..names.len())
            .map(|inner_index| Ref {
                source_index: crate::internal::runtime::SOURCE_INDEX,
                inner_index: u32::try_from(inner_index).expect("small symbol index"),
            })
            .collect();
        let runtime_ast = js_ast::Ast {
            symbols: names
                .iter()
                .map(|name| Symbol::new(SymbolKind::Other, *name))
                .collect(),
            named_exports: names
                .iter()
                .zip(&references)
                .map(|(name, &reference)| {
                    (
                        (*name).to_string(),
                        NamedExport {
                            reference,
                            ..NamedExport::default()
                        },
                    )
                })
                .collect(),
            ..js_ast::Ast::default()
        };
        let graph = clone_linker_graph(&[js_file(runtime_ast)], &[0], &[], false);
        let unbound_module_ref = Ref {
            source_index: 1,
            inner_index: 9,
        };

        for (name, &reference) in names.iter().zip(&references) {
            assert_eq!(runtime_symbol_ref(&graph, name), reference);
        }
        let scan_refs = scan_runtime_refs_from_graph(&graph, unbound_module_ref);
        assert_eq!(scan_refs.export_ref, references[0]);
        assert_eq!(scan_refs.common_js_ref, references[1]);
        assert_eq!(scan_refs.esm_ref, references[2]);
        assert_eq!(scan_refs.to_common_js_ref, references[3]);
        assert_eq!(scan_refs.unbound_module_ref, unbound_module_ref);

        let chunk_refs = chunk_runtime_refs_from_graph(&graph, Some(unbound_module_ref));
        assert_eq!(chunk_refs.common_js_ref, references[1]);
        assert_eq!(chunk_refs.esm_ref, references[2]);
        assert_eq!(chunk_refs.to_common_js_ref, references[3]);
        assert_eq!(chunk_refs.to_esm_ref, references[4]);
        assert_eq!(chunk_refs.runtime_require_ref, references[5]);
        assert_eq!(
            chunk_refs.re_export,
            Some(RuntimeReExportContext {
                re_export_ref: references[6],
                unbound_module_ref: Some(unbound_module_ref),
            })
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn prepares_scanner_files_through_cross_chunk_generation() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut options = Options {
            mode: Mode::Bundle,
            output_format: Format::Iife,
            tree_shaking: true,
            abs_output_dir: "/out".into(),
            ..Options::default()
        };
        crate::internal::bundler::apply_option_defaults(&mut options);
        let runtime_result = crate::internal::bundler::parse_file(
            &log,
            crate::internal::runtime::source(options.unsupported_js_features),
            Loader::Js,
            &options,
        );
        let entry_result = crate::internal::bundler::parse_file(
            &log,
            Source {
                index: 1,
                key_path: Path {
                    text: "/entry.js".into(),
                    namespace: "file".into(),
                    ..Path::default()
                },
                pretty_paths: PrettyPaths {
                    abs: "/entry.js".into(),
                    rel: "entry.js".into(),
                },
                contents: std::sync::Arc::from(b"console.log(1);".as_slice()),
                ..Source::default()
            },
            Loader::Js,
            &options,
        );
        assert!(runtime_result.ok);
        assert!(entry_result.ok);
        assert!(log.done().is_empty());
        let scanned = crate::internal::bundler::ScannedBundle {
            files: vec![runtime_result.file, entry_result.file],
            entry_points: vec![EntryPoint {
                source_index: 1,
                ..EntryPoint::default()
            }],
        };

        let mut prepared = crate::internal::bundler::prepare_linker_graph(
            &scanned,
            &options,
            PREFIX,
            &HashMap::new(),
        );
        assert!(prepared.scan_result.import_issues.is_empty());
        assert!(prepared.scan_result.ambiguous_re_exports.is_empty());
        assert!(prepared.scan_result.arbitrary_namespace_issues.is_empty());
        assert_eq!(
            prepared.unbound_module_ref,
            crate::internal::ast::INVALID_REF
        );
        assert_eq!(prepared.chunks.len(), 1);
        assert!(prepared.chunks[0].is_entry_point);
        assert_eq!(prepared.chunks[0].source_index, 1);
        assert!(!prepared.chunks[0].parts_in_chunk_in_order.is_empty());
        assert!(prepared.graph.files[1].is_live);

        let runtime_refs = chunk_runtime_refs_from_graph(&prepared.graph, None);
        let renamer = crate::internal::renamer::new_no_op_renamer(prepared.graph.symbols.clone());
        let chunk_paths: Vec<_> = prepared
            .chunks
            .iter()
            .map(|chunk| ChunkPath {
                unique_key: chunk.unique_key.clone(),
                final_rel_path: chunk.final_rel_path.clone(),
            })
            .collect();
        let output_paths = OutputPathContext::new(PREFIX, &[], &chunk_paths);
        assert_eq!(
            generate_javascript_chunks(
                &prepared.graph,
                &mut prepared.chunks,
                &options,
                runtime_refs,
                EntryPointTailRefs {
                    to_common_js_ref: runtime_refs.to_common_js_ref,
                    unbound_module_ref: prepared.unbound_module_ref,
                },
                &renamer,
                &output_paths,
            ),
            [false]
        );
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let outputs = finalize_generated_javascript_chunks(
            &file_system,
            &prepared.graph,
            &mut prepared.chunks,
            &[],
            &options,
        );
        assert_eq!(outputs.len(), 1);
        assert!(outputs[0].abs_path.ends_with("/entry.js"));
        let output =
            String::from_utf8(outputs[0].contents.clone()).expect("generated JavaScript is UTF-8");
        assert!(output.contains("console.log(1);"));
        assert!(output.starts_with("(() => {\n"));
        assert!(output.ends_with("})();\n"));
    }

    #[test]
    fn discovers_css_companions_in_javascript_postorder() {
        let js_with_dependencies = |dependencies: &[u32], css_source_index: u32| {
            let mut file = js_file(js_ast::Ast {
                import_records: dependencies
                    .iter()
                    .map(|&source_index| ImportRecord {
                        source_index: Index32::new(source_index),
                        ..ImportRecord::default()
                    })
                    .collect(),
                parts: vec![js_ast::Part {
                    import_record_indices: (0..dependencies.len())
                        .map(|index| u32::try_from(index).expect("import index fits in u32"))
                        .collect(),
                    ..js_ast::Part::default()
                }],
                ..js_ast::Ast::default()
            });
            let Some(InputFileRepr::Js(repr)) = file.repr.as_mut() else {
                panic!("JavaScript");
            };
            repr.css_source_index = Index32::new(css_source_index);
            file
        };
        let css_file = || InputFile {
            repr: Some(InputFileRepr::Css(Box::<CssRepr>::default())),
            loader: Loader::Css,
            ..InputFile::default()
        };
        let input_files = [
            js_with_dependencies(&[1, 2], 4),
            js_with_dependencies(&[3], 5),
            js_with_dependencies(&[3], 6),
            js_with_dependencies(&[], 7),
            css_file(),
            css_file(),
            css_file(),
            css_file(),
        ];
        let graph = clone_linker_graph(&input_files, &[0, 1, 2, 3, 4, 5, 6, 7], &[], false);
        assert_eq!(find_imported_css_files_in_js_order(&graph, 0), [7, 5, 6, 4]);
    }

    fn css_file(
        imports: Vec<(ImportRecord, Option<ImportConditions>)>,
        layers_pre_import: Vec<Vec<String>>,
        layers_post_import: Vec<Vec<String>>,
    ) -> InputFile {
        let rules = imports
            .iter()
            .enumerate()
            .map(|(index, (_, import_conditions))| Rule {
                data: RuleData::AtImport(AtImportRule {
                    import_conditions: import_conditions.clone(),
                    import_record_index: u32::try_from(index).expect("import index fits in u32"),
                }),
                loc: Loc::default(),
            })
            .collect();
        InputFile {
            repr: Some(InputFileRepr::Css(Box::new(CssRepr {
                ast: crate::internal::css_ast::Ast {
                    import_records: imports.into_iter().map(|(record, _)| record).collect(),
                    rules,
                    layers_pre_import,
                    layers_post_import,
                    ..crate::internal::css_ast::Ast::default()
                },
                ..CssRepr::default()
            }))),
            loader: Loader::Css,
            ..InputFile::default()
        }
    }

    fn internal_css_import(source_index: u32) -> (ImportRecord, Option<ImportConditions>) {
        (
            ImportRecord {
                source_index: Index32::new(source_index),
                kind: ImportKind::At,
                ..ImportRecord::default()
            },
            None,
        )
    }

    #[test]
    fn css_order_uses_last_declaration_copy_and_stops_cycles() {
        let input_files = [
            css_file(vec![], vec![], vec![]),
            css_file(
                vec![internal_css_import(2), internal_css_import(3)],
                vec![],
                vec![],
            ),
            css_file(vec![internal_css_import(4)], vec![], vec![]),
            css_file(vec![internal_css_import(4)], vec![], vec![]),
            css_file(vec![internal_css_import(1)], vec![], vec![]),
        ];
        let graph = clone_linker_graph(&input_files, &[0, 1, 2, 3, 4], &[], false);
        let order = find_imported_files_in_css_order(&graph, &[1]);
        assert_eq!(
            order
                .iter()
                .map(|entry| (entry.kind, entry.source_index))
                .collect::<Vec<_>>(),
            [
                (CssImportKind::SourceIndex, 2),
                (CssImportKind::SourceIndex, 4),
                (CssImportKind::SourceIndex, 3),
                (CssImportKind::SourceIndex, 1),
            ]
        );
    }

    #[test]
    fn css_order_hoists_external_imports() {
        let input_files = [
            css_file(vec![], vec![], vec![]),
            css_file(
                vec![
                    internal_css_import(2),
                    (
                        ImportRecord {
                            path: Path {
                                text: "https://example.com/theme.css".into(),
                                ..Path::default()
                            },
                            kind: ImportKind::At,
                            ..ImportRecord::default()
                        },
                        None,
                    ),
                ],
                vec![],
                vec![],
            ),
            css_file(vec![], vec![], vec![]),
        ];
        let graph = clone_linker_graph(&input_files, &[0, 1, 2], &[], false);
        let order = find_imported_files_in_css_order(&graph, &[1]);
        assert_eq!(order[0].kind, CssImportKind::ExternalPath);
        assert_eq!(order[0].external_path.text, "https://example.com/theme.css");
        assert_eq!(
            order[1..]
                .iter()
                .map(|entry| entry.source_index)
                .collect::<Vec<_>>(),
            [2, 1]
        );
    }

    #[test]
    fn css_order_keeps_first_cascade_layer_effect_for_duplicate_files() {
        let input_files = [
            css_file(vec![], vec![], vec![]),
            css_file(
                vec![internal_css_import(2), internal_css_import(3)],
                vec![],
                vec![],
            ),
            css_file(vec![internal_css_import(4)], vec![], vec![]),
            css_file(vec![internal_css_import(4)], vec![], vec![]),
            css_file(vec![], vec![], vec![vec!["base".into()]]),
        ];
        let graph = clone_linker_graph(&input_files, &[0, 1, 2, 3, 4], &[], false);
        let order = find_imported_files_in_css_order(&graph, &[1]);
        assert_eq!(order[0].kind, CssImportKind::Layers);
        assert_eq!(order[0].layers, [vec!["base"]]);
        assert_eq!(
            order[1..]
                .iter()
                .map(|entry| entry.source_index)
                .collect::<Vec<_>>(),
            [2, 4, 3, 1]
        );
    }

    #[test]
    fn computes_javascript_chunks_with_css_companions() {
        let mut entry = js_file(js_ast::Ast::default());
        let Some(InputFileRepr::Js(repr)) = entry.repr.as_mut() else {
            panic!("expected JavaScript");
        };
        repr.css_source_index = Index32::new(2);
        let input_files = [
            js_file(js_ast::Ast::default()),
            entry,
            css_file(vec![], vec![], vec![]),
        ];
        let entry_points = [EntryPoint {
            source_index: 1,
            ..EntryPoint::default()
        }];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &entry_points, false);
        let chunks = compute_chunks(&mut graph, &Options::default(), PREFIX);
        assert_eq!(chunks.len(), 2);
        assert!(!chunks[0].is_css);
        assert_eq!(chunks[0].css_chunk_index.get_index(), 1);
        assert!(chunks[1].is_css);
        assert!(chunks[1].is_entry_point);
        assert_eq!(chunks[1].source_index, 1);
        assert_eq!(chunks[1].unique_key, "UNIQUEC00000001");
        assert_eq!(
            chunks[1]
                .imports_in_css_order
                .iter()
                .filter_map(|entry| {
                    (entry.kind == CssImportKind::SourceIndex).then_some(entry.source_index)
                })
                .collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(graph.files[1].entry_point_chunk_index, 0);
    }

    #[test]
    fn computes_direct_css_entry_point_chunks() {
        let input_files = [
            js_file(js_ast::Ast::default()),
            css_file(vec![internal_css_import(2)], vec![], vec![]),
            css_file(vec![], vec![], vec![]),
        ];
        let entry_points = [EntryPoint {
            source_index: 1,
            ..EntryPoint::default()
        }];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &entry_points, false);
        let chunks = compute_chunks(&mut graph, &Options::default(), PREFIX);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_css);
        assert!(chunks[0].is_entry_point);
        assert!(!chunks[0].css_chunk_index.is_valid());
        assert_eq!(
            chunks[0]
                .imports_in_css_order
                .iter()
                .map(|entry| entry.source_index)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(graph.files[1].entry_point_chunk_index, 0);
    }

    #[test]
    fn generates_css_chunks_and_preserves_additional_files() {
        let mut stylesheet = css_file(vec![], vec![], vec![]);
        let Some(InputFileRepr::Css(repr)) = stylesheet.repr.as_mut() else {
            panic!("expected CSS");
        };
        repr.ast.rules = vec![Rule {
            data: RuleData::Comment(crate::internal::css_ast::CommentRule {
                text: ".app{color:red}".into(),
            }),
            loc: Loc::default(),
        }];
        stylesheet.additional_files = vec![OutputFile {
            abs_path: "/out/image.png".into(),
            contents: b"image".to_vec(),
            ..OutputFile::default()
        }];
        let input_files = [js_file(js_ast::Ast::default()), stylesheet];
        let entry_points = [EntryPoint {
            source_index: 1,
            ..EntryPoint::default()
        }];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &entry_points, false);
        let mut chunks = compute_chunks(&mut graph, &Options::default(), PREFIX);
        assert_eq!(chunks.len(), 1);
        chunks[0].final_template = vec![PathTemplate {
            data: "app.css".into(),
            ..PathTemplate::default()
        }];
        generate_css_chunk(
            &graph,
            &mut chunks[0],
            &Options::default(),
            &context(&[], &[]),
        );
        let outputs = finalize_javascript_chunk_outputs(
            &mock_fs(&HashMap::new(), MockKind::Unix, "/"),
            &graph,
            &mut chunks,
            &[],
            &Options {
                abs_output_dir: "/out".into(),
                ..Options::default()
            },
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].abs_path, "/out/image.png");
        assert_eq!(outputs[0].contents, b"image");
        assert_eq!(outputs[1].abs_path, "/out/app.css");
        assert_eq!(outputs[1].contents, b".app{color:red}\n");
    }

    #[test]
    fn mangles_javascript_properties_with_cache_and_reservations() {
        let foo_a = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let bar = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let foo_b = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let input_files = [
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                symbols: vec![
                    Symbol::new(SymbolKind::MangledProp, "_foo"),
                    Symbol::new(SymbolKind::MangledProp, "_bar"),
                ],
                mangled_props: HashMap::from([("_foo".into(), foo_a), ("_bar".into(), bar)]),
                reserved_props: HashMap::from([("a".into(), true)]),
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::MangledProp, "_foo")],
                mangled_props: HashMap::from([("_foo".into(), foo_b)]),
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &[], false);
        graph.symbols.get_mut(foo_a).use_count_estimate = 5;
        graph.symbols.get_mut(foo_b).use_count_estimate = 3;
        graph.symbols.get_mut(bar).use_count_estimate = 1;
        let mut cache = crate::internal::config::MangleCache::from([(
            "_bar".into(),
            std::sync::Arc::new("z".to_string()) as std::sync::Arc<dyn std::any::Any + Send + Sync>,
        )]);

        let names = mangle_props(&mut graph, &mut cache);

        assert_eq!(graph.symbols.follow_symbols_const(foo_b), foo_a);
        assert_eq!(names[&foo_a], "b");
        assert_eq!(names[&bar], "z");
        assert_eq!(
            cache["_foo"].downcast_ref::<String>().map(String::as_str),
            Some("b")
        );
    }

    #[test]
    fn mangles_local_css_names_globally_without_collisions() {
        let css_with_symbols = |symbols: Vec<Symbol>| InputFile {
            repr: Some(InputFileRepr::Css(Box::new(CssRepr {
                ast: crate::internal::css_ast::Ast {
                    symbols,
                    ..crate::internal::css_ast::Ast::default()
                },
                ..CssRepr::default()
            }))),
            source: Source {
                identifier_name: "entry".into(),
                ..Source::default()
            },
            loader: Loader::LocalCss,
            ..InputFile::default()
        };
        let input_files = [
            js_file(js_ast::Ast::default()),
            css_with_symbols(vec![
                Symbol::new(SymbolKind::GlobalCss, "a"),
                Symbol::new(SymbolKind::LocalCss, "button"),
            ]),
            css_with_symbols(vec![Symbol::new(SymbolKind::LocalCss, "button")]),
        ];
        let graph = clone_linker_graph(&input_files, &[0, 1, 2], &[], false);
        let first = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let second = Ref {
            source_index: 2,
            inner_index: 0,
        };

        let names = mangle_local_css(&graph, &Options::default(), &mut HashSet::new());
        assert_eq!(names[&first], "entry_button");
        assert_eq!(names[&second], "entry_button2");
        let compiled = compile_prepared_css_asts(
            &graph,
            &[PreparedCssAst {
                ast: crate::internal::css_ast::Ast {
                    rules: vec![Rule {
                        data: RuleData::Selector(crate::internal::css_ast::SelectorRule {
                            selectors: vec![crate::internal::css_ast::ComplexSelector {
                                selectors: vec![crate::internal::css_ast::CompoundSelector {
                                    subclass_selectors: vec![
                                        crate::internal::css_ast::SubclassSelector {
                                            data: crate::internal::css_ast::SubclassData::Class(
                                                crate::internal::css_ast::ClassSelector {
                                                    name: crate::internal::ast::LocRef {
                                                        reference: first,
                                                        ..crate::internal::ast::LocRef::default()
                                                    },
                                                },
                                            ),
                                            range: Range::default(),
                                        },
                                    ],
                                    ..crate::internal::css_ast::CompoundSelector::default()
                                }],
                            }],
                            ..crate::internal::css_ast::SelectorRule::default()
                        }),
                        loc: Loc::default(),
                    }],
                    ..crate::internal::css_ast::Ast::default()
                },
                source_index: Index32::new(1),
                ..PreparedCssAst::default()
            }],
            &Options::default(),
        );
        assert_eq!(compiled[0].css, b".entry_button {\n}\n");

        let names = mangle_local_css(
            &graph,
            &Options {
                minify_identifiers: true,
                ..Options::default()
            },
            &mut HashSet::from(["b".into()]),
        );
        assert_eq!(names[&first], "c");
        assert_eq!(names[&second], "d");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn populates_css_module_stub_exports_with_composed_names() {
        let button = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let base = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let button_loc = crate::internal::ast::LocRef {
            reference: button,
            ..crate::internal::ast::LocRef::default()
        };
        let base_loc = crate::internal::ast::LocRef {
            reference: base,
            ..crate::internal::ast::LocRef::default()
        };
        let stylesheet = InputFile {
            repr: Some(InputFileRepr::Css(Box::new(CssRepr {
                ast: crate::internal::css_ast::Ast {
                    symbols: vec![
                        Symbol::new(SymbolKind::LocalCss, "button"),
                        Symbol::new(SymbolKind::LocalCss, "base"),
                    ],
                    local_symbols: vec![button_loc, base_loc],
                    composes: HashMap::from([(
                        button,
                        crate::internal::css_ast::Composes {
                            names: vec![base_loc],
                            ..crate::internal::css_ast::Composes::default()
                        },
                    )]),
                    ..crate::internal::css_ast::Ast::default()
                },
                js_source_index: Index32::new(2),
            }))),
            source: Source {
                identifier_name: "module".into(),
                ..Source::default()
            },
            loader: Loader::LocalCss,
            ..InputFile::default()
        };
        let module_ref = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let stub = js_file(js_ast::Ast {
            symbols: vec![Symbol::new(SymbolKind::Other, "module")],
            module_ref,
            exports_kind: ExportsKind::CommonJs,
            parts: vec![js_ast::Part {
                statements: vec![js_ast::Stmt::new(
                    Loc::default(),
                    js_ast::StmtData::LazyExport(js_ast::LazyExportStmt {
                        value: js_ast::Expr::new(Loc::default(), js_ast::ExprData::Null),
                    }),
                )],
                ..js_ast::Part::default()
            }],
            ..js_ast::Ast::default()
        });
        let input_files = [js_file(js_ast::Ast::default()), stylesheet, stub];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &[], false);
        let Some(InputFileRepr::Js(stub_repr)) = graph.files[2].input_file.repr.as_mut() else {
            panic!("expected stub");
        };
        stub_repr.css_source_index = Index32::new(1);

        let local_names = mangle_local_css(&graph, &Options::default(), &mut HashSet::new());
        let value = generate_css_module_exports(&graph, 1, &local_names);
        let Some(js_ast::ExprData::Object(object)) = value.data.as_deref() else {
            panic!("expected CSS module object");
        };
        let strings = object
            .properties
            .iter()
            .map(|property| {
                let Some(js_ast::ExprData::String(key)) = property.key.data.as_deref() else {
                    panic!("expected string key");
                };
                let Some(js_ast::ExprData::String(value)) = property.value_or_nil.data.as_deref()
                else {
                    panic!("expected string value");
                };
                (
                    crate::internal::helpers::utf16_to_string(&key.value),
                    crate::internal::helpers::utf16_to_string(&value.value),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            strings,
            [
                (b"button".to_vec(), b"module_base module_button".to_vec()),
                (b"base".to_vec(), b"module_base".to_vec()),
            ]
        );

        populate_css_stub_lazy_export(&mut graph, 2, &local_names);
        let Some(InputFileRepr::Js(stub_repr)) = graph.files[2].input_file.repr.as_ref() else {
            panic!("expected stub");
        };
        let Some(js_ast::StmtData::LazyExport(export)) =
            stub_repr.ast.parts[0].statements[0].data.as_deref()
        else {
            panic!("expected lazy export");
        };
        assert!(matches!(
            export.value.data.as_deref(),
            Some(js_ast::ExprData::Object(_))
        ));

        lower_common_js_lazy_export(&mut graph, 2, &local_names);
        let Some(InputFileRepr::Js(stub_repr)) = graph.files[2].input_file.repr.as_ref() else {
            panic!("expected stub");
        };
        let Some(js_ast::StmtData::Expr(statement)) =
            stub_repr.ast.parts[0].statements[0].data.as_deref()
        else {
            panic!("expected assignment statement");
        };
        let Some(js_ast::ExprData::Binary(assignment)) = statement.value.data.as_deref() else {
            panic!("expected assignment");
        };
        assert_eq!(assignment.op, js_ast::OpCode::BinaryAssign);
        assert!(matches!(
            assignment.left.data.as_deref(),
            Some(js_ast::ExprData::Dot(dot)) if dot.name == "exports"
        ));
        assert!(matches!(
            assignment.right.data.as_deref(),
            Some(js_ast::ExprData::Object(_))
        ));
        assert!(stub_repr.ast.uses_module_ref);
    }

    #[test]
    fn lowers_esm_lazy_object_exports_into_independent_parts() {
        let source = Source {
            index: 1,
            identifier_name: "data".into(),
            ..Source::default()
        };
        let property = |name: &str, value: f64| js_ast::Property {
            key: js_ast::Expr::new(
                Loc::default(),
                js_ast::ExprData::String(js_ast::StringExpr {
                    value: crate::internal::helpers::string_to_utf16(name.as_bytes()),
                    ..js_ast::StringExpr::default()
                }),
            ),
            value_or_nil: js_ast::Expr::new(Loc::default(), js_ast::ExprData::Number(value)),
            ..js_ast::Property::default()
        };
        let value = js_ast::Expr::new(
            Loc::default(),
            js_ast::ExprData::Object(js_ast::ObjectExpr {
                properties: vec![
                    property("foo", 1.0),
                    property("not-valid", 2.0),
                    property("default", 3.0),
                ],
                ..js_ast::ObjectExpr::default()
            }),
        );
        let ast = js_parser::lazy_export_ast(
            Log::new_defer(DeferLogKind::All, HashMap::new()),
            &source,
            js_parser::options_from_config(&Options::default()),
            value,
            None,
        );
        let input_files = [
            js_file(js_ast::Ast::default()),
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast,
                    ..JsRepr::default()
                }))),
                source,
                loader: Loader::Json,
                ..InputFile::default()
            },
        ];
        let entry_points = [EntryPoint {
            source_index: 1,
            ..EntryPoint::default()
        }];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &entry_points, false);
        let options = Options {
            unsupported_js_features:
                crate::internal::compat::JsFeature::ARBITRARY_MODULE_NAMESPACE_NAMES,
            ..Options::default()
        };

        generate_code_for_lazy_exports(&mut graph, &options, &HashMap::new());

        let Some(InputFileRepr::Js(repr)) = graph.files[1].input_file.repr.as_ref() else {
            panic!("expected JavaScript");
        };
        assert!(repr.ast.parts[1].statements.is_empty());
        assert_eq!(repr.ast.parts.len(), 4);
        assert_eq!(
            repr.meta
                .resolved_exports
                .keys()
                .cloned()
                .collect::<HashSet<_>>(),
            HashSet::from(["default".into(), "foo".into()])
        );
        assert!(repr.ast.parts[2].can_be_removed_if_unused);
        let Some(js_ast::StmtData::Local(local)) = repr.ast.parts[2].statements[0].data.as_deref()
        else {
            panic!("expected named export");
        };
        assert!(local.is_export);
        assert!(matches!(
            local.declarations[0].value_or_nil.data.as_deref(),
            Some(js_ast::ExprData::Number(value)) if value.to_bits() == 1.0_f64.to_bits()
        ));
        let Some(js_ast::StmtData::ExportDefault(default_export)) =
            repr.ast.parts[3].statements[0].data.as_deref()
        else {
            panic!("expected default export");
        };
        assert_eq!(
            graph
                .symbols
                .get(default_export.default_name.reference)
                .original_name,
            "data_default"
        );
        assert!(matches!(
            default_export.value.data.as_deref(),
            Some(js_ast::StmtData::Expr(statement))
                if matches!(statement.value.data.as_deref(), Some(js_ast::ExprData::Object(_)))
        ));
        assert!(repr.ast.uses_module_ref);
    }

    #[test]
    fn with_type_json_lazy_exports_only_default() {
        let source = Source {
            index: 1,
            identifier_name: "typed_json".into(),
            ..Source::default()
        };
        let value = js_ast::Expr::new(
            Loc::default(),
            js_ast::ExprData::Object(js_ast::ObjectExpr {
                properties: vec![js_ast::Property {
                    key: js_ast::Expr::new(
                        Loc::default(),
                        js_ast::ExprData::String(js_ast::StringExpr {
                            value: crate::internal::helpers::string_to_utf16(b"foo"),
                            ..js_ast::StringExpr::default()
                        }),
                    ),
                    value_or_nil: js_ast::Expr::new(Loc::default(), js_ast::ExprData::Number(1.0)),
                    ..js_ast::Property::default()
                }],
                ..js_ast::ObjectExpr::default()
            }),
        );
        let ast = js_parser::lazy_export_ast(
            Log::new_defer(DeferLogKind::All, HashMap::new()),
            &source,
            js_parser::options_from_config(&Options::default()),
            value,
            None,
        );
        let input_files = [
            js_file(js_ast::Ast::default()),
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast,
                    ..JsRepr::default()
                }))),
                source,
                loader: Loader::WithTypeJson,
                ..InputFile::default()
            },
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &[], false);

        lower_esm_lazy_export(&mut graph, 1, &Options::default(), &HashMap::new());

        let Some(InputFileRepr::Js(repr)) = graph.files[1].input_file.repr.as_ref() else {
            panic!("expected JavaScript");
        };
        assert_eq!(repr.ast.parts.len(), 3);
        assert_eq!(
            repr.meta.resolved_exports.keys().collect::<Vec<_>>(),
            vec![&"default".to_string()]
        );
        assert!(matches!(
            repr.ast.parts[2].statements[0].data.as_deref(),
            Some(js_ast::StmtData::ExportDefault(_))
        ));
    }

    #[test]
    fn pre_generates_common_js_export_copies_for_esm_entries() {
        let source = Source {
            index: 1,
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let ast = js_parser::lazy_export_ast(
            Log::new_defer(DeferLogKind::All, HashMap::new()),
            &source,
            js_parser::options_from_config(&Options::default()),
            js_ast::Expr::new(Loc::default(), js_ast::ExprData::Null),
            None,
        );
        let input_files = [
            js_file(js_ast::Ast::default()),
            InputFile {
                repr: Some(InputFileRepr::Js(Box::new(JsRepr {
                    ast,
                    ..JsRepr::default()
                }))),
                source,
                loader: Loader::Json,
                ..InputFile::default()
            },
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
        let Some(InputFileRepr::Js(repr)) = graph.files[1].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.sorted_and_filtered_export_aliases = vec!["default".into(), "foo".into()];

        generate_common_js_export_copies(
            &mut graph,
            1,
            &Options {
                output_format: Format::EsModule,
                ..Options::default()
            },
        );

        let copies = &js_repr(&graph, 1).meta.cjs_export_copies;
        assert_eq!(copies.len(), 2);
        assert_eq!(graph.symbols.get(copies[0]).original_name, "export_default");
        assert_eq!(graph.symbols.get(copies[1]).original_name, "export_foo");
    }

    #[test]
    fn wraps_css_rules_with_nested_import_conditions() {
        let base_rule = Rule {
            data: RuleData::Comment(crate::internal::css_ast::CommentRule {
                text: "contents".into(),
            }),
            loc: Loc::default(),
        };
        let conditions = [ImportConditions {
            layers: vec![Token {
                children: Some(vec![Token {
                    kind: TokenKind::Ident,
                    text: "theme".into(),
                    ..Token::default()
                }]),
                kind: TokenKind::Function,
                text: "layer".into(),
                ..Token::default()
            }],
            supports: vec![Token {
                children: Some(vec![Token {
                    kind: TokenKind::Ident,
                    text: "grid".into(),
                    ..Token::default()
                }]),
                kind: TokenKind::Function,
                text: "supports".into(),
                ..Token::default()
            }],
            queries: vec![MediaQuery {
                loc: Loc::default(),
                data: MediaQueryData::ArbitraryTokens(MediaArbitraryTokensQuery {
                    tokens: vec![Token {
                        kind: TokenKind::Ident,
                        text: "screen".into(),
                        ..Token::default()
                    }],
                }),
            }],
        }];
        let (rules, import_records) =
            wrap_rules_with_conditions(vec![base_rule], vec![], &conditions, &[]);
        assert!(import_records.is_empty());
        let RuleData::AtMedia(media) = &rules[0].data else {
            panic!("expected media wrapper");
        };
        let RuleData::KnownAt(supports) = &media.rules[0].data else {
            panic!("expected supports wrapper");
        };
        assert_eq!(supports.at_token, "supports");
        assert_eq!(supports.prelude[0].kind, TokenKind::OpenParen);
        let RuleData::KnownAt(layer) = &supports.rules[0].data else {
            panic!("expected layer wrapper");
        };
        assert_eq!(layer.at_token, "layer");
        assert_eq!(layer.prelude[0].text, "theme");
        assert!(matches!(layer.rules[0].data, RuleData::Comment(_)));
    }

    #[test]
    fn keeps_named_empty_layers_but_omits_anonymous_empty_layers() {
        let named_layer = ImportConditions {
            layers: vec![Token {
                children: Some(vec![Token {
                    kind: TokenKind::Ident,
                    text: "base".into(),
                    ..Token::default()
                }]),
                ..Token::default()
            }],
            ..ImportConditions::default()
        };
        let (rules, _) = wrap_rules_with_conditions(vec![], vec![], &[named_layer], &[]);
        let RuleData::KnownAt(layer) = &rules[0].data else {
            panic!("expected named layer");
        };
        assert_eq!(layer.at_token, "layer");
        assert_eq!(layer.prelude[0].text, "base");
        assert!(layer.rules.is_empty());

        let anonymous_layer = ImportConditions {
            layers: vec![Token::default()],
            ..ImportConditions::default()
        };
        let (rules, _) = wrap_rules_with_conditions(vec![], vec![], &[anonymous_layer], &[]);
        assert!(rules.is_empty());
    }

    #[test]
    fn remaps_url_records_inside_css_condition_wrappers() {
        let existing_record = ImportRecord {
            path: Path {
                text: "existing.png".into(),
                ..Path::default()
            },
            kind: ImportKind::Url,
            ..ImportRecord::default()
        };
        let condition_record = ImportRecord {
            path: Path {
                text: "condition.png".into(),
                ..Path::default()
            },
            kind: ImportKind::Url,
            ..ImportRecord::default()
        };
        let condition = ImportConditions {
            layers: vec![Token {
                children: Some(vec![Token {
                    kind: TokenKind::Url,
                    payload_index: 0,
                    ..Token::default()
                }]),
                ..Token::default()
            }],
            ..ImportConditions::default()
        };
        let base_rule = Rule {
            data: RuleData::Comment(crate::internal::css_ast::CommentRule::default()),
            loc: Loc::default(),
        };
        let (rules, import_records) = wrap_rules_with_conditions(
            vec![base_rule],
            vec![existing_record],
            &[condition],
            &[condition_record],
        );
        assert_eq!(import_records.len(), 2);
        assert_eq!(import_records[1].path.text, "condition.png");
        let RuleData::KnownAt(layer) = &rules[0].data else {
            panic!("expected layer wrapper");
        };
        assert_eq!(layer.prelude[0].payload_index, 1);
    }

    #[test]
    fn prepares_linked_css_by_filtering_linker_owned_rules() {
        let mut source = css_file(
            vec![(
                ImportRecord {
                    path: Path {
                        text: "dependency.css".into(),
                        ..Path::default()
                    },
                    kind: ImportKind::At,
                    ..ImportRecord::default()
                },
                None,
            )],
            vec![],
            vec![],
        );
        let Some(InputFileRepr::Css(repr)) = source.repr.as_mut() else {
            panic!("expected CSS");
        };
        repr.ast.rules = vec![
            Rule {
                data: RuleData::AtCharset(crate::internal::css_ast::AtCharsetRule {
                    encoding: "UTF-8".into(),
                }),
                loc: Loc::default(),
            },
            Rule {
                data: RuleData::AtLayer(crate::internal::css_ast::AtLayerRule {
                    names: vec![vec!["before".into()]],
                    ..crate::internal::css_ast::AtLayerRule::default()
                }),
                loc: Loc::default(),
            },
            Rule {
                data: RuleData::AtImport(AtImportRule {
                    import_record_index: 0,
                    ..AtImportRule::default()
                }),
                loc: Loc::default(),
            },
            Rule {
                data: RuleData::AtLayer(crate::internal::css_ast::AtLayerRule {
                    names: vec![vec!["after".into()]],
                    ..crate::internal::css_ast::AtLayerRule::default()
                }),
                loc: Loc::default(),
            },
            Rule {
                data: RuleData::Comment(crate::internal::css_ast::CommentRule {
                    text: "contents".into(),
                }),
                loc: Loc::default(),
            },
        ];
        let input_files = [css_file(vec![], vec![], vec![]), source];
        let graph = clone_linker_graph(&input_files, &[0, 1], &[], false);
        let prepared = prepare_css_asts(
            &graph,
            &[super::CssImportOrder {
                kind: CssImportKind::SourceIndex,
                source_index: 1,
                ..super::CssImportOrder::default()
            }],
            &Options::default(),
        );
        assert!(prepared[0].has_charset);
        assert_eq!(prepared[0].source_index.get_index(), 1);
        assert_eq!(prepared[0].ast.rules.len(), 2);
        let RuleData::AtLayer(layer) = &prepared[0].ast.rules[0].data else {
            panic!("expected post-import layer");
        };
        assert_eq!(layer.names, [vec!["after"]]);
        assert!(matches!(
            prepared[0].ast.rules[1].data,
            RuleData::Comment(_)
        ));
    }

    #[test]
    fn prepares_synthetic_css_layer_entries() {
        let input_files = [css_file(vec![], vec![], vec![])];
        let graph = clone_linker_graph(&input_files, &[0], &[], false);
        let prepared = prepare_css_asts(
            &graph,
            &[super::CssImportOrder {
                kind: CssImportKind::Layers,
                layers: vec![vec!["reset".into()], vec!["theme".into()]],
                ..super::CssImportOrder::default()
            }],
            &Options::default(),
        );
        let RuleData::AtLayer(layer) = &prepared[0].ast.rules[0].data else {
            panic!("expected synthetic layer");
        };
        assert_eq!(layer.names, [vec!["reset"], vec!["theme"]]);
        assert!(!prepared[0].source_index.is_valid());
    }

    #[test]
    fn prepares_nested_external_css_imports_as_data_urls() {
        let input_files = [css_file(vec![], vec![], vec![])];
        let graph = clone_linker_graph(&input_files, &[0], &[], false);
        let prepared = prepare_css_asts(
            &graph,
            &[super::CssImportOrder {
                kind: CssImportKind::ExternalPath,
                external_path: Path {
                    text: "https://example.com/theme.css".into(),
                    ..Path::default()
                },
                conditions: vec![
                    ImportConditions::default(),
                    ImportConditions {
                        queries: vec![MediaQuery {
                            loc: Loc::default(),
                            data: MediaQueryData::ArbitraryTokens(MediaArbitraryTokensQuery {
                                tokens: vec![Token {
                                    kind: TokenKind::Ident,
                                    text: "screen".into(),
                                    ..Token::default()
                                }],
                            }),
                        }],
                        ..ImportConditions::default()
                    },
                ],
                ..super::CssImportOrder::default()
            }],
            &Options::default(),
        );
        assert_eq!(prepared[0].ast.import_records.len(), 1);
        assert!(
            prepared[0].ast.import_records[0]
                .path
                .text
                .starts_with("data:text/css")
        );
        let RuleData::AtImport(at_import) = &prepared[0].ast.rules[0].data else {
            panic!("expected external import");
        };
        assert!(at_import.import_conditions.is_some());
    }

    #[test]
    fn prepares_css_with_cross_file_dead_rule_removal_when_minifying() {
        let duplicate_rule = Rule {
            data: RuleData::UnknownAt(crate::internal::css_ast::UnknownAtRule {
                at_token: "custom".into(),
                prelude: vec![Token {
                    kind: TokenKind::Ident,
                    text: "same".into(),
                    ..Token::default()
                }],
                ..crate::internal::css_ast::UnknownAtRule::default()
            }),
            loc: Loc::default(),
        };
        let mut earlier = css_file(vec![], vec![], vec![]);
        let Some(InputFileRepr::Css(repr)) = earlier.repr.as_mut() else {
            panic!("expected CSS");
        };
        repr.ast.rules = vec![duplicate_rule.clone()];
        let mut later = css_file(vec![], vec![], vec![]);
        let Some(InputFileRepr::Css(repr)) = later.repr.as_mut() else {
            panic!("expected CSS");
        };
        repr.ast.rules = vec![duplicate_rule];
        let input_files = [css_file(vec![], vec![], vec![]), earlier, later];
        let graph = clone_linker_graph(&input_files, &[0, 1, 2], &[], false);
        let prepared = prepare_css_asts(
            &graph,
            &[
                super::CssImportOrder {
                    kind: CssImportKind::SourceIndex,
                    source_index: 1,
                    ..super::CssImportOrder::default()
                },
                super::CssImportOrder {
                    kind: CssImportKind::SourceIndex,
                    source_index: 2,
                    ..super::CssImportOrder::default()
                },
            ],
            &Options {
                minify_syntax: true,
                ..Options::default()
            },
        );
        assert!(prepared[0].ast.rules.is_empty());
        assert_eq!(prepared[1].ast.rules.len(), 1);
    }

    #[test]
    fn compiles_prepared_css_asts_with_printer_options() {
        let input_files = [css_file(vec![], vec![], vec![])];
        let graph = clone_linker_graph(&input_files, &[0], &[], false);
        let compiled = compile_prepared_css_asts(
            &graph,
            &[PreparedCssAst {
                ast: crate::internal::css_ast::Ast {
                    rules: vec![Rule {
                        data: RuleData::Comment(crate::internal::css_ast::CommentRule {
                            text: "a{color:red}".into(),
                        }),
                        loc: Loc::default(),
                    }],
                    ..crate::internal::css_ast::Ast::default()
                },
                source_index: Index32::new(7),
                has_charset: true,
            }],
            &Options {
                minify_whitespace: true,
                ..Options::default()
            },
        );
        assert_eq!(compiled[0].css, b"a{color:red}");
        assert_eq!(compiled[0].source_index.get_index(), 7);
        assert!(compiled[0].has_charset);
    }

    #[test]
    fn carries_css_legal_comments_and_metafile_imports_through_assembly() {
        let mut source = css_file(vec![], vec![], vec![]);
        source.source.pretty_paths = PrettyPaths {
            abs: "/project/input.css".into(),
            rel: "input.css".into(),
        };
        let input_files = [css_file(vec![], vec![], vec![]), source];
        let graph = clone_linker_graph(&input_files, &[0, 1], &[], false);
        let options = Options {
            legal_comments: LegalComments::EndOfFile,
            needs_metafile: true,
            ..Options::default()
        };
        let prepared = [PreparedCssAst {
            ast: crate::internal::css_ast::Ast {
                import_records: vec![ImportRecord {
                    path: Path {
                        text: "theme.css".into(),
                        ..Path::default()
                    },
                    kind: ImportKind::At,
                    ..ImportRecord::default()
                }],
                rules: vec![
                    Rule {
                        data: RuleData::Comment(crate::internal::css_ast::CommentRule {
                            text: "/*! license */".into(),
                        }),
                        loc: Loc::default(),
                    },
                    Rule {
                        data: RuleData::AtImport(AtImportRule::default()),
                        loc: Loc::default(),
                    },
                ],
                ..crate::internal::css_ast::Ast::default()
            },
            source_index: Index32::new(1),
            ..PreparedCssAst::default()
        }];
        let compiled = compile_prepared_css_asts(&graph, &prepared, &options);
        assert_eq!(compiled[0].extracted_legal_comments, ["/*! license */"]);
        assert_eq!(compiled[0].json_metadata_imports.len(), 1);

        let mut chunk = ChunkInfo::default();
        assemble_css_chunk(&graph, &mut chunk, &compiled, &options, &context(&[], &[]));
        assert_eq!(chunk.metadata_imports.len(), 1);
        let (joiner, _) =
            context(&[], &[]).substitute_final_paths(chunk.intermediate_output, str::to_owned);
        assert_eq!(joiner.done(), b"@import \"theme.css\";\n/*! license */\n");
    }

    #[test]
    fn assembles_css_chunks_with_charset_boundaries_and_banners() {
        let mut source = css_file(vec![], vec![], vec![]);
        source.source.pretty_paths = PrettyPaths {
            abs: "/project/input.css".into(),
            rel: "input.css".into(),
        };
        let input_files = [css_file(vec![], vec![], vec![]), source];
        let graph = clone_linker_graph(&input_files, &[0, 1], &[], false);
        let mut chunk = ChunkInfo::default();
        assemble_css_chunk(
            &graph,
            &mut chunk,
            &[
                CompiledCssAst {
                    css: b"@import \"external.css\";\n".to_vec(),
                    ..CompiledCssAst::default()
                },
                CompiledCssAst {
                    css: b".a { color: red }\n".to_vec(),
                    source_index: Index32::new(1),
                    has_charset: true,
                    ..CompiledCssAst::default()
                },
            ],
            &Options {
                mode: Mode::Bundle,
                css_banner: "/* banner */".into(),
                css_footer: "/* footer */".into(),
                needs_metafile: true,
                ..Options::default()
            },
            &context(&[], &[]),
        );
        assert_eq!(chunk.metadata_inputs.len(), 1);
        assert_eq!(chunk.metadata_inputs[0].source_index, 1);
        let (joiner, shifts) =
            context(&[], &[]).substitute_final_paths(chunk.intermediate_output, str::to_owned);
        assert_eq!(
            joiner.done(),
            b"/* banner */\n@charset \"UTF-8\";\n@import \"external.css\";\n\n/* input.css */\n.a { color: red }\n/* footer */\n"
        );
        assert_eq!(shifts, [SourceMapShift::default()]);
    }

    #[test]
    fn css_chunk_assembly_splits_temporary_asset_paths() {
        let input_files = [css_file(vec![], vec![], vec![])];
        let graph = clone_linker_graph(&input_files, &[0], &[], false);
        let assets = [Some(AssetPath {
            unique_key: "UNIQUEA00000000".into(),
            rel_path: "image.png".into(),
        })];
        let mut chunk = ChunkInfo::default();
        assemble_css_chunk(
            &graph,
            &mut chunk,
            &[CompiledCssAst {
                css: b"a{background:url(UNIQUEA00000000)}".to_vec(),
                ..CompiledCssAst::default()
            }],
            &Options {
                minify_whitespace: true,
                ..Options::default()
            },
            &context(&assets, &[]),
        );
        assert!(chunk.intermediate_output.pieces().is_some());
        let (joiner, _) = context(&assets, &[])
            .substitute_final_paths(chunk.intermediate_output, |_| "assets/image.png".into());
        assert_eq!(joiner.done(), b"a{background:url(assets/image.png)}\n");
    }

    #[test]
    fn finalizes_css_companions_and_links_javascript_metadata() {
        let mut javascript = js_file(js_ast::Ast::default());
        javascript.source.pretty_paths = PrettyPaths {
            abs: "/project/app.js".into(),
            rel: "app.js".into(),
        };
        let mut stylesheet = css_file(vec![], vec![], vec![]);
        stylesheet.source.pretty_paths = PrettyPaths {
            abs: "/project/style.css".into(),
            rel: "style.css".into(),
        };
        let input_files = [javascript, stylesheet];
        let graph = clone_linker_graph(&input_files, &[0, 1], &[EntryPoint::default()], false);
        let file_system = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let options = Options {
            abs_output_dir: "/out".into(),
            needs_metafile: true,
            metafile_path_style: crate::internal::logger::PathStyle::Absolute,
            ..Options::default()
        };

        let mut javascript_joiner = Joiner::default();
        javascript_joiner.add_string("console.log(1);\n");
        let javascript_chunk = ChunkInfo {
            unique_key: "UNIQUEC00000000".into(),
            final_template: vec![PathTemplate {
                data: "app.js".into(),
                ..PathTemplate::default()
            }],
            intermediate_output: context(&[], &[]).break_joiner_into_pieces(javascript_joiner),
            css_chunk_index: Index32::new(1),
            is_entry_point: true,
            ..ChunkInfo::default()
        };

        let mut css_chunk = ChunkInfo {
            unique_key: "UNIQUEC00000001".into(),
            final_template: vec![PathTemplate {
                data: "app.css".into(),
                ..PathTemplate::default()
            }],
            source_index: 0,
            is_entry_point: true,
            is_css: true,
            ..ChunkInfo::default()
        };
        assemble_css_chunk(
            &graph,
            &mut css_chunk,
            &[CompiledCssAst {
                css: b".app{color:red}".to_vec(),
                source_index: Index32::new(1),
                ..CompiledCssAst::default()
            }],
            &options,
            &context(&[], &[]),
        );

        let mut chunks = vec![javascript_chunk, css_chunk];
        let outputs =
            finalize_javascript_chunk_outputs(&file_system, &graph, &mut chunks, &[], &options);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].abs_path, "/out/app.js");
        assert!(
            outputs[0]
                .json_metadata_chunk
                .contains("\"cssBundle\": \"/out/app.css\"")
        );
        assert!(
            outputs[0]
                .json_metadata_chunk
                .contains("\"entryPoint\": \"/project/app.js\"")
        );
        assert_eq!(outputs[1].abs_path, "/out/app.css");
        assert_eq!(outputs[1].contents, b".app{color:red}\n");
        assert!(!outputs[1].json_metadata_chunk.contains("\"entryPoint\""));
        assert!(
            outputs[1]
                .json_metadata_chunk
                .contains("\"/project/style.css\"")
        );
    }

    #[test]
    fn composes_and_emits_css_source_maps() {
        let mut stylesheet = css_file(vec![], vec![], vec![]);
        stylesheet.source.index = 1;
        stylesheet.source.contents = b".app{color:red}".to_vec().into();
        stylesheet.source.pretty_paths = PrettyPaths {
            abs: "/project/style.css".into(),
            rel: "style.css".into(),
        };
        let Some(InputFileRepr::Css(repr)) = stylesheet.repr.as_mut() else {
            panic!("expected CSS");
        };
        repr.ast.approximate_line_count = 1;
        repr.ast.rules = vec![Rule {
            data: RuleData::Comment(crate::internal::css_ast::CommentRule {
                text: ".app{color:red}".into(),
            }),
            loc: Loc::default(),
        }];
        let input_files = [js_file(js_ast::Ast::default()), stylesheet];
        let graph = clone_linker_graph(&input_files, &[0, 1], &[], false);
        let options = Options {
            abs_output_dir: "/out".into(),
            source_map: SourceMapMode::LinkedWithComment,
            ..Options::default()
        };
        let prepared = prepare_css_asts(
            &graph,
            &[super::CssImportOrder {
                kind: CssImportKind::SourceIndex,
                source_index: 1,
                ..super::CssImportOrder::default()
            }],
            &options,
        );
        let compiled = compile_prepared_css_asts(&graph, &prepared, &options);
        assert!(!compiled[0].source_map_chunk.should_ignore);
        let mut chunk = ChunkInfo {
            unique_key: "UNIQUEC00000000".into(),
            final_template: vec![PathTemplate {
                data: "app.css".into(),
                ..PathTemplate::default()
            }],
            is_css: true,
            ..ChunkInfo::default()
        };
        assemble_css_chunk(&graph, &mut chunk, &compiled, &options, &context(&[], &[]));
        assert_eq!(chunk.source_map_results.len(), 1);
        let outputs = finalize_javascript_chunk_outputs(
            &mock_fs(&HashMap::new(), MockKind::Unix, "/"),
            &graph,
            std::slice::from_mut(&mut chunk),
            &[],
            &options,
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].abs_path, "/out/app.css.map");
        assert!(outputs[0].contents.starts_with(b"{\n  \"version\": 3,"));
        assert_eq!(outputs[1].abs_path, "/out/app.css");
        assert!(
            outputs[1]
                .contents
                .ends_with(b"/*# sourceMappingURL=app.css.map */\n")
        );
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
        let (result, _) = match_import_with_export(
            &graph,
            tracker,
            Vec::new(),
            &mut Vec::new(),
            Format::EsModule,
        );
        assert_eq!(result.kind, MatchImportKind::NoMatch);
        assert_eq!(result.source_index, 1);
    }

    #[test]
    fn missing_export_typo_correction_is_cached_per_file() {
        let exported_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let input_files = [js_file(js_ast::Ast {
            symbols: vec![Symbol::new(SymbolKind::Other, "foobar")],
            named_exports: HashMap::from([("foobar".into(), named_export(exported_ref))]),
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(&input_files, &[0], &[], false);

        assert_eq!(
            maybe_correct_export_typo(&mut graph, 0, "foobr"),
            Some("foobar".into())
        );
        assert_eq!(maybe_correct_export_typo(&mut graph, 0, "other"), None);
        assert!(js_repr(&graph, 0).meta.resolved_export_typos.is_some());
    }

    #[test]
    fn rejects_arbitrary_entry_export_names_for_unsupported_targets() {
        let exported_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let input_files = [js_file(js_ast::Ast {
            symbols: vec![Symbol::new(SymbolKind::Other, "value")],
            named_exports: HashMap::from([(
                "not-valid".into(),
                NamedExport {
                    reference: exported_ref,
                    alias_loc: Loc { start: 7 },
                },
            )]),
            ..js_ast::Ast::default()
        })];
        let mut graph = clone_linker_graph(&input_files, &[0], &[EntryPoint::default()], false);
        assert!(sort_and_filter_export_aliases(&mut graph, 0).is_empty());

        let issues = arbitrary_namespace_export_issues(
            &graph,
            0,
            &Options {
                output_format: Format::EsModule,
                unsupported_js_features:
                    crate::internal::compat::JsFeature::ARBITRARY_MODULE_NAMESPACE_NAMES,
                ..Options::default()
            },
        );
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].alias, "not-valid");
        assert_eq!(issues[0].name_loc, Loc { start: 7 });
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
    #[allow(clippy::too_many_lines)]
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
            Loader::Js,
            NamedImport {
                alias: "missing".into(),
                namespace_ref,
                ..NamedImport::default()
            },
            ImportRecord {
                source_index: Index32::new(1),
                ..ImportRecord::default()
            },
            JsRepr::default(),
        );
        graph.symbols.symbols_for_source[0].push(Symbol::new(SymbolKind::Import, "missing"));
        let issues = bind_imports_to_exports_for_file(&mut graph, 0, Format::CommonJs);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].result.is_missing);
        assert_eq!(
            graph.symbols.get(tracker.import_ref).import_item_status,
            ImportItemStatus::Missing
        );

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

        let (mut graph, tracker) = import_tracker_graph(
            Loader::Js,
            NamedImport {
                alias: "missing".into(),
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
        let mut symbol = Symbol::new(SymbolKind::Import, "missing");
        symbol.import_item_status = ImportItemStatus::Generated;
        graph.symbols.symbols_for_source[0].push(symbol);
        let issues = bind_imports_to_exports_for_file(&mut graph, 0, Format::EsModule);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].result.kind, MatchImportKind::NoMatch);
        assert_eq!(
            graph.symbols.get(tracker.import_ref).import_item_status,
            ImportItemStatus::Missing
        );
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
    fn namespace_export_part_contains_getters_and_dependencies() {
        let export_runtime_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let exports_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let foo_ref = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let proto_ref = Ref {
            source_index: 1,
            inner_index: 2,
        };
        let input_files = vec![
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "__export")],
                parts: vec![js_ast::Part::default()],
                top_level_symbol_to_parts_from_parser: HashMap::from([(
                    export_runtime_ref,
                    vec![0],
                )]),
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![
                    Symbol::new(SymbolKind::Other, "exports"),
                    Symbol::new(SymbolKind::Other, "foo"),
                    Symbol::new(SymbolKind::Other, "proto"),
                ],
                exports_ref,
                named_exports: HashMap::from([
                    (
                        "foo".into(),
                        NamedExport {
                            reference: foo_ref,
                            ..NamedExport::default()
                        },
                    ),
                    (
                        "__proto__".into(),
                        NamedExport {
                            reference: proto_ref,
                            ..NamedExport::default()
                        },
                    ),
                ]),
                parts: vec![
                    js_ast::Part::default(),
                    js_ast::Part {
                        declared_symbols: vec![js_ast::DeclaredSymbol {
                            reference: foo_ref,
                            is_top_level: true,
                        }],
                        ..js_ast::Part::default()
                    },
                    js_ast::Part {
                        declared_symbols: vec![js_ast::DeclaredSymbol {
                            reference: proto_ref,
                            is_top_level: true,
                        }],
                        ..js_ast::Part::default()
                    },
                ],
                top_level_symbol_to_parts_from_parser: HashMap::from([
                    (foo_ref, vec![1]),
                    (proto_ref, vec![2]),
                ]),
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1], &[], false);
        let Some(InputFileRepr::Js(repr)) = graph.files[1].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.sorted_and_filtered_export_aliases = vec!["__proto__".into(), "foo".into()];
        repr.meta.needs_exports_variable = true;

        create_exports_for_file(&mut graph, 1, export_runtime_ref, None, &Options::default());

        let repr = js_repr(&graph, 1);
        let part = &repr.ast.parts[js_ast::NS_EXPORT_PART_INDEX as usize];
        assert!(part.can_be_removed_if_unused);
        assert!(part.force_tree_shaking);
        assert_eq!(part.statements.len(), 2);
        assert_eq!(part.declared_symbols[0].reference, exports_ref);
        assert_eq!(part.symbol_uses[&foo_ref].count_estimate, 1);
        assert_eq!(part.symbol_uses[&proto_ref].count_estimate, 1);
        assert_eq!(
            part.dependencies,
            [
                js_ast::Dependency {
                    source_index: 1,
                    part_index: 2,
                },
                js_ast::Dependency {
                    source_index: 1,
                    part_index: 1,
                },
                js_ast::Dependency {
                    source_index: 0,
                    part_index: 0,
                },
            ]
        );
        let Some(js_ast::StmtData::Local(local)) = part.statements[0].data.as_deref() else {
            panic!("exports declaration");
        };
        assert!(matches!(
            local.declarations[0].value_or_nil.data.as_deref(),
            Some(js_ast::ExprData::Object(_))
        ));
        let Some(js_ast::StmtData::Expr(statement)) = part.statements[1].data.as_deref() else {
            panic!("export call");
        };
        let Some(js_ast::ExprData::Call(call)) = statement.value.data.as_deref() else {
            panic!("export call expression");
        };
        let Some(js_ast::ExprData::Object(getters)) = call.args[1].data.as_deref() else {
            panic!("export getters");
        };
        assert_eq!(getters.properties.len(), 2);
        assert!(
            getters.properties[0]
                .flags
                .contains(js_ast::PropertyFlags::IS_COMPUTED)
        );
        assert!(matches!(
            getters.properties[0].value_or_nil.data.as_deref(),
            Some(js_ast::ExprData::Arrow(arrow)) if arrow.prefer_expr
        ));
        assert!(repr.ast.uses_exports_ref);
        assert!(repr.meta.needs_export_symbol_from_runtime);
    }

    #[test]
    fn common_js_entry_namespace_is_decorated_on_module_exports() {
        let export_runtime_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let to_common_js_ref = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let unbound_module_ref = Ref {
            source_index: 0,
            inner_index: 2,
        };
        let exports_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let input_files = [
            js_file(js_ast::Ast {
                symbols: vec![
                    Symbol::new(SymbolKind::Other, "__export"),
                    Symbol::new(SymbolKind::Other, "__toCommonJS"),
                    Symbol::new(SymbolKind::Unbound, "module"),
                ],
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "exports")],
                exports_ref,
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
        let Some(InputFileRepr::Js(repr)) = graph.files[1].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.needs_exports_variable = true;
        repr.meta.force_include_exports_for_entry_point = true;

        create_exports_for_file(
            &mut graph,
            1,
            export_runtime_ref,
            Some(EntryPointTailRefs {
                to_common_js_ref,
                unbound_module_ref,
            }),
            &Options {
                output_format: Format::CommonJs,
                ..Options::default()
            },
        );

        let part = &js_repr(&graph, 1).ast.parts[js_ast::NS_EXPORT_PART_INDEX as usize];
        assert_eq!(part.statements.len(), 2);
        let Some(js_ast::StmtData::Expr(statement)) = part.statements[1].data.as_deref() else {
            panic!("module exports assignment");
        };
        let Some(js_ast::ExprData::Binary(assignment)) = statement.value.data.as_deref() else {
            panic!("assignment expression");
        };
        assert_eq!(assignment.op, js_ast::OpCode::BinaryAssign);
        assert!(matches!(
            assignment.left.data.as_deref(),
            Some(js_ast::ExprData::Dot(dot))
                if dot.name == "exports"
                    && matches!(
                        dot.target.data.as_deref(),
                        Some(js_ast::ExprData::Identifier(identifier))
                            if identifier.reference == unbound_module_ref
                    )
        ));
        assert!(matches!(
            assignment.right.data.as_deref(),
            Some(js_ast::ExprData::Call(call))
                if matches!(
                    call.target.data.as_deref(),
                    Some(js_ast::ExprData::Identifier(identifier))
                        if identifier.reference == to_common_js_ref
                )
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn finalizes_inlined_uses_and_local_part_dependencies() {
        let declared_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let empty_ref = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let identity_ref = Ref {
            source_index: 1,
            inner_index: 2,
        };
        let enum_import_ref = Ref {
            source_index: 1,
            inner_index: 3,
        };
        let enum_ref = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let mut empty_symbol = Symbol::new(SymbolKind::Other, "empty");
        empty_symbol.flags |= crate::internal::ast::SymbolFlags::IS_EMPTY_FUNCTION;
        let mut identity_symbol = Symbol::new(SymbolKind::Other, "identity");
        identity_symbol.flags |= crate::internal::ast::SymbolFlags::IS_IDENTITY_FUNCTION;
        let input_files = [
            js_file(js_ast::Ast::default()),
            js_file(js_ast::Ast {
                symbols: vec![
                    Symbol::new(SymbolKind::Other, "declared"),
                    empty_symbol,
                    identity_symbol,
                    Symbol::new(SymbolKind::Import, "Enum"),
                ],
                named_imports: HashMap::from([(
                    enum_import_ref,
                    NamedImport {
                        alias: "Enum".into(),
                        ..NamedImport::default()
                    },
                )]),
                parts: vec![
                    js_ast::Part {
                        declared_symbols: vec![js_ast::DeclaredSymbol {
                            reference: declared_ref,
                            is_top_level: true,
                        }],
                        ..js_ast::Part::default()
                    },
                    js_ast::Part {
                        symbol_uses: HashMap::from([(
                            declared_ref,
                            js_ast::SymbolUse { count_estimate: 1 },
                        )]),
                        symbol_call_uses: HashMap::from([
                            (
                                empty_ref,
                                js_ast::SymbolCallUse {
                                    call_count_estimate: 2,
                                    ..js_ast::SymbolCallUse::default()
                                },
                            ),
                            (
                                identity_ref,
                                js_ast::SymbolCallUse {
                                    call_count_estimate: 3,
                                    single_arg_non_spread_call_count_estimate: 2,
                                },
                            ),
                        ]),
                        import_symbol_property_uses: HashMap::from([(
                            enum_import_ref,
                            HashMap::from([
                                ("Known".into(), js_ast::SymbolUse { count_estimate: 4 }),
                                ("Unknown".into(), js_ast::SymbolUse { count_estimate: 5 }),
                            ]),
                        )]),
                        ..js_ast::Part::default()
                    },
                ],
                top_level_symbol_to_parts_from_parser: HashMap::from([(declared_ref, vec![0])]),
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::TsEnum, "Enum")],
                ts_enums: HashMap::from([(
                    enum_ref,
                    HashMap::from([("Known".into(), js_ast::TsEnumValue::default())]),
                )]),
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &[], false);
        let Some(InputFileRepr::Js(repr)) = graph.files[1].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.imports_to_bind.insert(
            enum_import_ref,
            crate::internal::graph::ImportData {
                source_index: 2,
                reference: enum_ref,
                ..crate::internal::graph::ImportData::default()
            },
        );

        finalize_part_dependencies_for_file(&mut graph, 1);

        let repr = js_repr(&graph, 1);
        let part = &repr.ast.parts[1];
        assert_eq!(part.symbol_uses[&declared_ref].count_estimate, 1);
        assert!(!part.symbol_uses.contains_key(&empty_ref));
        assert_eq!(part.symbol_uses[&identity_ref].count_estimate, 1);
        assert_eq!(part.symbol_uses[&enum_import_ref].count_estimate, 5);
        assert_eq!(
            part.dependencies,
            [js_ast::Dependency {
                source_index: 1,
                part_index: 0,
            }]
        );
        assert_eq!(
            repr.ast.named_imports[&enum_import_ref].local_parts_with_uses,
            [1]
        );
    }

    #[test]
    fn binds_imports_to_declaring_parts_and_merges_symbols() {
        let import_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let exports_ref = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let module_ref = Ref {
            source_index: 1,
            inner_index: 2,
        };
        let wrapper_ref = Ref {
            source_index: 1,
            inner_index: 3,
        };
        let target_ref = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let mut source_file = js_file(js_ast::Ast {
            symbols: vec![
                Symbol::new(SymbolKind::Import, "value"),
                Symbol::new(SymbolKind::Other, "exports"),
                Symbol::new(SymbolKind::Other, "module"),
                Symbol::new(SymbolKind::Other, "require_entry"),
            ],
            exports_ref,
            module_ref,
            wrapper_ref,
            exports_kind: ExportsKind::Esm,
            named_imports: HashMap::from([(
                import_ref,
                NamedImport {
                    local_parts_with_uses: vec![1],
                    ..NamedImport::default()
                },
            )]),
            parts: vec![js_ast::Part::default(), js_ast::Part::default()],
            ..js_ast::Ast::default()
        });
        source_file.source.identifier_name = "entry".into();
        let input_files = [
            js_file(js_ast::Ast::default()),
            source_file,
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "value")],
                parts: vec![js_ast::Part {
                    declared_symbols: vec![js_ast::DeclaredSymbol {
                        reference: target_ref,
                        is_top_level: true,
                    }],
                    ..js_ast::Part::default()
                }],
                top_level_symbol_to_parts_from_parser: HashMap::from([(target_ref, vec![0])]),
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                parts: vec![js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2, 3], &[], false);
        let Some(InputFileRepr::Js(repr)) = graph.files[1].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.wrap = WrapKind::Esm;
        repr.meta.imports_to_bind.insert(
            import_ref,
            crate::internal::graph::ImportData {
                re_exports: vec![js_ast::Dependency {
                    source_index: 3,
                    part_index: 0,
                }],
                source_index: 2,
                reference: target_ref,
                ..crate::internal::graph::ImportData::default()
            },
        );

        bind_imports_to_parts_for_file(&mut graph, 1, crate::internal::ast::INVALID_REF);

        assert_eq!(
            js_repr(&graph, 1).ast.parts[1].dependencies,
            [
                js_ast::Dependency {
                    source_index: 2,
                    part_index: 0,
                },
                js_ast::Dependency {
                    source_index: 3,
                    part_index: 0,
                },
            ]
        );
        assert_eq!(graph.symbols.follow_symbols_const(import_ref), target_ref);
        assert_eq!(graph.symbols.get(wrapper_ref).original_name, "init_entry");
        assert_eq!(
            graph.symbols.get(exports_ref).original_name,
            "entry_exports"
        );
        assert_eq!(graph.symbols.get(module_ref).original_name, "entry_module");
    }

    #[test]
    fn entry_point_part_keeps_exports_namespace_and_wrapper_live() {
        let to_common_js_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let wrapper_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let target_ref = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let input_files = [
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "__toCommonJS")],
                parts: vec![js_ast::Part::default()],
                top_level_symbol_to_parts_from_parser: HashMap::from([(to_common_js_ref, vec![0])]),
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "init_entry")],
                wrapper_ref,
                parts: vec![js_ast::Part::default(), js_ast::Part::default()],
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "value")],
                parts: vec![js_ast::Part {
                    declared_symbols: vec![js_ast::DeclaredSymbol {
                        reference: target_ref,
                        is_top_level: true,
                    }],
                    ..js_ast::Part::default()
                }],
                top_level_symbol_to_parts_from_parser: HashMap::from([(target_ref, vec![0])]),
                ..js_ast::Ast::default()
            }),
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
        let Some(InputFileRepr::Js(repr)) = graph.files[1].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        repr.meta.sorted_and_filtered_export_aliases = vec!["value".into()];
        repr.meta.resolved_exports.insert(
            "value".into(),
            crate::internal::graph::ExportData {
                source_index: 2,
                reference: target_ref,
                ..crate::internal::graph::ExportData::default()
            },
        );
        repr.meta.force_include_exports_for_entry_point = true;
        repr.meta.wrap = WrapKind::Esm;
        repr.meta.wrapper_part_index = Index32::new(1);

        create_entry_point_part(&mut graph, 1, to_common_js_ref);

        let repr = js_repr(&graph, 1);
        assert_eq!(repr.meta.entry_point_part_index.get_index(), 2);
        let part = &repr.ast.parts[2];
        assert!(!part.can_be_removed_if_unused);
        assert_eq!(
            part.dependencies,
            [
                js_ast::Dependency {
                    source_index: 2,
                    part_index: 0,
                },
                js_ast::Dependency {
                    source_index: 1,
                    part_index: js_ast::NS_EXPORT_PART_INDEX,
                },
                js_ast::Dependency {
                    source_index: 1,
                    part_index: 1,
                },
                js_ast::Dependency {
                    source_index: 0,
                    part_index: 0,
                },
            ]
        );
        assert_eq!(part.symbol_uses[&to_common_js_ref].count_estimate, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn import_constraints_pull_in_wrappers_and_runtime_helpers() {
        let to_esm_ref = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let require_ref = Ref {
            source_index: 0,
            inner_index: 1,
        };
        let source_exports_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let wrapper_ref = Ref {
            source_index: 2,
            inner_index: 0,
        };
        let input_files = [
            js_file(js_ast::Ast {
                symbols: vec![
                    Symbol::new(SymbolKind::Other, "__toESM"),
                    Symbol::new(SymbolKind::Other, "__require"),
                ],
                named_exports: HashMap::from([
                    (
                        "__toESM".into(),
                        NamedExport {
                            reference: to_esm_ref,
                            ..NamedExport::default()
                        },
                    ),
                    (
                        "__require".into(),
                        NamedExport {
                            reference: require_ref,
                            ..NamedExport::default()
                        },
                    ),
                ]),
                parts: vec![js_ast::Part::default(), js_ast::Part::default()],
                top_level_symbol_to_parts_from_parser: HashMap::from([
                    (to_esm_ref, vec![0]),
                    (require_ref, vec![1]),
                ]),
                ..js_ast::Ast::default()
            }),
            js_file(js_ast::Ast {
                symbols: vec![Symbol::new(SymbolKind::Other, "exports")],
                exports_ref: source_exports_ref,
                import_records: vec![
                    ImportRecord {
                        source_index: Index32::new(2),
                        kind: ImportKind::Stmt,
                        ..ImportRecord::default()
                    },
                    ImportRecord {
                        kind: ImportKind::Require,
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
                symbols: vec![Symbol::new(SymbolKind::Other, "require_dep")],
                wrapper_ref,
                exports_kind: ExportsKind::CommonJs,
                parts: vec![js_ast::Part::default()],
                top_level_symbol_to_parts_from_parser: HashMap::from([(wrapper_ref, vec![0])]),
                ..js_ast::Ast::default()
            }),
        ];
        let mut graph = clone_linker_graph(&input_files, &[0, 1, 2], &[], false);
        let Some(InputFileRepr::Js(target)) = graph.files[2].input_file.repr.as_mut() else {
            panic!("JavaScript");
        };
        target.meta.wrap = WrapKind::Cjs;

        encode_import_constraints_for_file(
            &mut graph,
            1,
            &Options {
                mode: Mode::Bundle,
                output_format: Format::Preserve,
                ..Options::default()
            },
        );

        let repr = js_repr(&graph, 1);
        assert!(
            repr.ast.import_records[0]
                .flags
                .contains(ImportRecordFlags::WRAP_WITH_TO_ESM)
        );
        assert!(
            repr.ast.import_records[1]
                .flags
                .contains(ImportRecordFlags::CALL_RUNTIME_REQUIRE)
        );
        let part = &repr.ast.parts[0];
        assert_eq!(part.symbol_uses[&wrapper_ref].count_estimate, 1);
        assert_eq!(part.symbol_uses[&to_esm_ref].count_estimate, 1);
        assert_eq!(part.symbol_uses[&require_ref].count_estimate, 1);
        assert!(part.dependencies.contains(&js_ast::Dependency {
            source_index: 2,
            part_index: 0,
        }));
        assert!(part.dependencies.contains(&js_ast::Dependency {
            source_index: 0,
            part_index: 0,
        }));
        assert!(part.dependencies.contains(&js_ast::Dependency {
            source_index: 0,
            part_index: 1,
        }));
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
            needs_metafile: true,
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
        assert_eq!(entry_bindings.json_metadata_imports.len(), 1);
        assert!(entry_bindings.json_metadata_imports[0].contains("UNIQUEC00000002"));
        assert!(!entry_bindings.json_metadata_imports[0].contains("\"external\""));
        let shared_bindings = print_cross_chunk_bindings(&chunks, 2, &renamer, &options);
        assert!(shared_bindings.prefix.is_empty());
        assert_eq!(shared_bindings.suffix, b"export { shared };\n");
    }
}
