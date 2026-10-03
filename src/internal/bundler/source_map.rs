use std::sync::Arc;

use url::Url;

use crate::internal::{
    cache::CacheSet,
    config::{Options, SourceMap as SourceMapMode},
    fs::{Fs, FsErrorKind},
    graph::{InputFile, InputFileRepr},
    helpers::string_to_utf16,
    js_parser,
    logger::{
        DeferLogKind, LineColumnTracker, Log, MsgId, MsgKind, Path, PrettyPaths, Source, Span,
    },
    resolver::DataUrl,
    sourcemap::{SourceContent, SourceMap},
};

pub(super) fn load_input_source_map(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    options: &Options,
    abs_resolve_dir: &str,
    input: &mut InputFile,
) {
    if !input.loader.can_have_source_map() || options.source_map == SourceMapMode::None {
        return;
    }
    let comment = match &input.repr {
        Some(InputFileRepr::Js(repr)) => &repr.ast.source_map_comment,
        Some(InputFileRepr::Css(repr)) => &repr.ast.source_map_comment,
        _ => return,
    };
    if comment.text.is_empty() {
        return;
    }
    let mut tracker = LineColumnTracker::new(Some(&input.source));
    let Some((path, contents)) = extract_source_map(
        log,
        file_system,
        caches,
        options,
        &input.source,
        comment,
        abs_resolve_dir,
    ) else {
        return;
    };
    let pretty_paths = pretty_paths(file_system, &path);
    let map_log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, (*log.overrides).clone());
    let mut source_map = js_parser::parse_source_map(
        map_log.clone(),
        Source {
            key_path: path.clone(),
            pretty_paths: pretty_paths.clone(),
            contents: Arc::from(contents),
            ..Source::default()
        },
    );
    let note = tracker.msg_data(
        comment.range,
        if path.namespace == "file" {
            format!(
                "The source map {:?} was referenced by the file {:?} here:",
                pretty_paths.select(options.log_path_style),
                input.source.pretty_paths.select(options.log_path_style)
            )
        } else {
            format!(
                "This source map came from the file {:?} here:",
                input.source.pretty_paths.select(options.log_path_style)
            )
        },
    );
    for mut message in map_log.done() {
        message.notes.push(note.clone());
        log.add_msg(message);
    }
    if let Some(source_map) = &mut source_map
        && !options.exclude_sources_content
    {
        fill_sources_content(file_system, caches, &path, source_map);
    }
    input.input_source_map = source_map;
}

fn pretty_paths(file_system: &dyn Fs, path: &Path) -> PrettyPaths {
    let text = format!("{}{}", path.text, path.ignored_suffix);
    PrettyPaths {
        abs: text.clone(),
        rel: if path.namespace == "file" {
            file_system.rel(file_system.cwd(), &text).unwrap_or(text)
        } else {
            text
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_source_map(
    log: &Log,
    file_system: &dyn Fs,
    caches: &CacheSet,
    options: &Options,
    source: &Source,
    comment: &Span,
    abs_resolve_dir: &str,
) -> Option<(Path, Vec<u8>)> {
    let mut tracker = LineColumnTracker::new(Some(source));
    let unsupported = |kind, text: String| {
        log.add_id(
            MsgId::SourceMapUnsupportedSourceMapComment,
            kind,
            Some(&mut LineColumnTracker::new(Some(source))),
            comment.range,
            format!("Unsupported source map comment: {text}"),
        );
    };
    if let Some(data_url) = DataUrl::parse(&comment.text) {
        return match data_url.decode_data() {
            Ok(contents) => {
                let mut path = source.key_path.clone();
                path.ignored_suffix = "#sourceMappingURL".into();
                Some((path, contents))
            }
            Err(error) => {
                unsupported(MsgKind::Warning, error);
                None
            }
        };
    }
    let url = match Url::parse(&comment.text) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            if abs_resolve_dir.is_empty() {
                unsupported(
                    MsgKind::Debug,
                    "Cannot resolve relative URL without a resolve directory".into(),
                );
                return None;
            }
            let base = Url::from_directory_path(abs_resolve_dir).ok()?;
            match base.join(&comment.text) {
                Ok(url) => url,
                Err(error) => {
                    unsupported(MsgKind::Warning, error.to_string());
                    return None;
                }
            }
        }
        Err(error) => {
            unsupported(MsgKind::Warning, error.to_string());
            return None;
        }
    };
    if url.scheme() != "file" {
        unsupported(
            MsgKind::Debug,
            format!("Unsupported URL scheme {:?}", url.scheme()),
        );
        return None;
    }
    if let Some(host) = url.host_str()
        && !host.is_empty()
        && host != "localhost"
    {
        unsupported(
            MsgKind::Warning,
            format!("Unsupported host {host:?} in file URL"),
        );
        return None;
    }
    let abs_path = url.to_file_path().ok()?.to_string_lossy().into_owned();
    let path = Path {
        text: abs_path.clone(),
        namespace: "file".into(),
        ..Path::default()
    };
    let (contents, error, _) = caches.fs_cache.read_file(file_system, &abs_path);
    if let Some(error) = error {
        let (kind, text) = if error.kind == FsErrorKind::NotFound {
            (MsgKind::Debug, format!("Cannot read file: {abs_path}"))
        } else {
            (
                MsgKind::Warning,
                format!(
                    "Cannot read file {:?}: {}",
                    pretty_paths(file_system, &path).select(options.log_path_style),
                    error.message
                ),
            )
        };
        log.add_id(
            MsgId::SourceMapMissingSourceMap,
            kind,
            Some(&mut tracker),
            comment.range,
            text,
        );
        return None;
    }
    Some((path, contents))
}

fn fill_sources_content(file_system: &dyn Fs, caches: &CacheSet, path: &Path, map: &mut SourceMap) {
    map.sources_content
        .resize(map.sources.len(), SourceContent::default());
    for (source, content) in map.sources.iter_mut().zip(&mut map.sources_content) {
        if path.namespace == "file"
            && file_system.is_abs(source)
            && let Ok(url) = Url::from_file_path(&*source)
        {
            *source = url.to_string();
        }
        if content.quoted.is_empty()
            && content.value.is_empty()
            && let Ok(url) = Url::parse(source)
            && let Ok(path) = url.to_file_path()
        {
            let (contents, error, _) = caches
                .fs_cache
                .read_file(file_system, &path.to_string_lossy());
            if error.is_none() {
                content.value = string_to_utf16(&contents);
                if contents.is_empty() {
                    content.quoted = "\"\"".into();
                }
            }
        }
    }
}
