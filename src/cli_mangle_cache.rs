use std::{fs, path::Path, sync::Arc};

use esbuild_rs::{
    api::{MangleCache, OnEndResult, Plugin},
    internal::{
        fs::{Fs, FsErrorKind, RealFsOptions, real_fs},
        helpers::{quote_for_json, utf16_to_string},
        js_ast::ExprData,
        js_lexer::range_of_identifier,
        js_parser::{JsonOptions, parse_json},
        logger::{
            LineColumnTracker, Log, Path as LogPath, PrettyPaths, Range, Source, new_stderr_log,
            output_options_for_args, print_error_to_stderr,
        },
    },
};

#[derive(Clone)]
pub(super) struct CacheFile {
    path: String,
    order: Vec<String>,
    ascii_only: bool,
}

impl CacheFile {
    pub(super) fn load(
        path: String,
        ascii_only: bool,
        arguments: &[String],
    ) -> Result<(Self, MangleCache), String> {
        let log = new_stderr_log(output_options_for_args(arguments));
        let result = real_fs(RealFsOptions::default())
            .ok()
            .and_then(|file_system| Self::read(&path, ascii_only, &log, file_system.as_ref()));
        let _ = log.done();
        result.ok_or_else(String::new)
    }

    fn read(
        path: &str,
        ascii_only: bool,
        log: &Log,
        file_system: &dyn Fs,
    ) -> Option<(Self, MangleCache)> {
        let absolute = file_system.abs(path)?;
        let relative = file_system
            .rel(file_system.cwd(), &absolute)
            .unwrap_or_else(|| absolute.clone())
            .replace('\\', "/");
        let cache_file = Self {
            path: absolute.clone(),
            order: Vec::new(),
            ascii_only,
        };
        let (contents, error, original_error) = file_system.read_file(&absolute);
        if let Some(error) = error {
            if error.kind == FsErrorKind::NotFound {
                return Some((cache_file, MangleCache::new()));
            }
            {
                let error = original_error.as_ref().unwrap_or(&error);
                let error = error
                    .message
                    .split(" (os error ")
                    .next()
                    .unwrap_or(&error.message);
                let error = if cfg!(unix) {
                    error.to_lowercase()
                } else {
                    error.to_string()
                };
                log.add_error(
                    None,
                    Range::default(),
                    format!("Failed to read from mangle cache file {relative:?}: {error}"),
                );
                return None;
            }
        }
        let source = Source {
            key_path: LogPath {
                text: absolute.clone(),
                namespace: "file".into(),
                ..LogPath::default()
            },
            pretty_paths: PrettyPaths {
                abs: absolute,
                rel: relative,
            },
            contents: Arc::from(contents),
            ..Source::default()
        };
        let (root, ok) = parse_json(log.clone(), source.clone(), JsonOptions::default());
        if !ok || log.has_errors() {
            return None;
        }
        let mut tracker = LineColumnTracker::new(Some(&source));
        let Some(ExprData::Object(object)) = root.data.as_deref() else {
            log.add_error(
                Some(&mut tracker),
                Range {
                    loc: root.loc,
                    len: 0,
                },
                "Expected a top-level object in mangle cache file",
            );
            return None;
        };
        let mut cache = MangleCache::new();
        let mut order = Vec::new();
        for property in &object.properties {
            let Some(ExprData::String(key)) = property.key.data.as_deref() else {
                continue;
            };
            let key = String::from_utf8_lossy(&utf16_to_string(&key.value)).into_owned();
            order.push(key.clone());
            let value = &property.value_or_nil;
            match value.data.as_deref() {
                Some(ExprData::String(value)) => {
                    cache.insert(
                        key,
                        serde_json::Value::String(
                            String::from_utf8_lossy(&utf16_to_string(&value.value)).into_owned(),
                        ),
                    );
                }
                Some(ExprData::Boolean(false)) => {
                    cache.insert(key, serde_json::Value::Bool(false));
                }
                _ => {
                    let range = if matches!(value.data.as_deref(), Some(ExprData::Boolean(_))) {
                        range_of_identifier(&source, value.loc)
                    } else {
                        Range {
                            loc: value.loc,
                            len: 0,
                        }
                    };
                    log.add_error(Some(&mut tracker), range, format!("Expected {key:?} in mangle cache file to map to either a string or false"));
                }
            }
        }
        if log.has_errors() {
            None
        } else {
            Some((
                Self {
                    order,
                    ..cache_file
                },
                cache,
            ))
        }
    }

    fn print(&self, cache: &MangleCache) -> Vec<u8> {
        let mut order = self.order.clone();
        if cache.len() > order.len() {
            if order.windows(2).all(|pair| pair[0] <= pair[1]) {
                order = cache.keys().cloned().collect();
                order.sort();
            } else {
                let mut new = cache
                    .keys()
                    .filter(|key| !order.contains(key))
                    .cloned()
                    .collect::<Vec<_>>();
                new.sort();
                order.extend(new);
            }
        }
        let mut bytes = b"{".to_vec();
        for (index, key) in order.iter().enumerate() {
            bytes.extend_from_slice(if index == 0 { b"\n  " } else { b",\n  " });
            bytes.extend(quote_for_json(key.as_bytes(), self.ascii_only));
            bytes.extend_from_slice(b": ");
            if let Some(value) = cache[key].as_str() {
                bytes.extend(quote_for_json(value.as_bytes(), self.ascii_only));
            } else {
                bytes.extend_from_slice(b"false");
            }
        }
        if !order.is_empty() {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(b"}\n");
        bytes
    }

    pub(super) fn write(&self, cache: Option<&MangleCache>, arguments: &[String]) {
        let Some(cache) = cache else {
            return;
        };
        let path = Path::new(&self.path);
        if let Some(parent) = path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                print_error_to_stderr(
                    arguments,
                    format!("Failed to create output directory: {error}"),
                );
                return;
            }
        }
        if let Err(error) = fs::write(path, self.print(cache)) {
            print_error_to_stderr(
                arguments,
                format!("Failed to write to output file: {error}"),
            );
        }
    }

    pub(super) fn watch_plugin(self, arguments: &[String]) -> Plugin {
        let arguments = arguments.to_vec();
        Plugin::new("MangleCache", move |build| {
            let cache_file = self.clone();
            let arguments = arguments.clone();
            build.on_end(move |result| {
                if result.errors.is_empty() {
                    cache_file.write(result.mangle_cache.as_ref(), &arguments);
                }
                Ok(OnEndResult::default())
            });
            Ok(())
        })
    }
}
