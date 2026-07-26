//! Port of upstream `internal/linker`.
//!
//! The linker is esbuild's second bundling phase. This initial section ports
//! the output-piece representation and the final path substitution machinery
//! used after chunks have been generated.

use crate::internal::{
    ast::ImportKind,
    css_ast::{
        ImportConditions, media_queries_equal_ignoring_whitespace, tokens_equal_ignoring_whitespace,
    },
    fs::Fs,
    helpers::Joiner,
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
    use std::collections::HashMap;

    use super::{
        AssetPath, ChunkImport, ChunkInfo, ChunkPath, OutputPathContext, OutputPiece,
        OutputPieceIndexKind, PartRange, append_or_extend_part_range,
        enforce_no_cyclic_chunk_imports, import_conditions_are_equal,
        is_conditional_import_redundant, join_with_public_path, path_between_chunks,
    };
    use crate::internal::{
        ast::ImportKind,
        css_ast::{
            ImportConditions, MediaArbitraryTokensQuery, MediaQuery, MediaQueryData, Token,
            WhitespaceFlags,
        },
        css_lexer::TokenKind,
        fs::{MockKind, mock_fs},
        helpers::Joiner,
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
}
