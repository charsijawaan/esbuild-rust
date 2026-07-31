//! Parsed-file and filesystem caches shared across builds.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use crate::internal::{
    css_ast::Ast as CssAst,
    css_parser,
    fs::{Fs, ModKey, ReadFileResult},
    js_ast::{Ast, Expr},
    js_parser::{JsonOptions, Options},
    logger::{DeferLogKind, Log, Msg, Path, Source},
    runtime,
};

#[derive(Default)]
pub struct CacheSet {
    pub css_cache: CssCache,
    pub fs_cache: FsCache,
    pub json_cache: JsonCache,
    pub js_cache: JsCache,
    pub source_index_cache: SourceIndexCache,
}

#[must_use]
pub fn make_cache_set() -> CacheSet {
    CacheSet {
        css_cache: CssCache::default(),
        fs_cache: FsCache::default(),
        json_cache: JsonCache::default(),
        js_cache: JsCache::default(),
        source_index_cache: SourceIndexCache::new(),
    }
}

#[derive(Default)]
pub struct CssCache {
    entries: Mutex<HashMap<Path, CssCacheEntry>>,
}

#[derive(Clone)]
struct CssCacheEntry {
    source: Source,
    messages: Vec<Msg>,
    ast: CssAst,
    options: css_parser::Options,
}

impl CssCache {
    #[must_use]
    pub fn parse(&self, log: &Log, source: Source, options: css_parser::Options) -> CssAst {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&source.key_path)
            .filter(|entry| entry.source == source && entry.options == options)
            .cloned()
        {
            replay_messages(log, &entry.messages);
            return entry.ast;
        }

        let temporary_log = Log::new_defer(DeferLogKind::All, log.overrides.as_ref().clone());
        let ast = css_parser::parse(temporary_log.clone(), source.clone(), options);
        let messages = temporary_log.done();
        replay_messages(log, &messages);
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                source.key_path.clone(),
                CssCacheEntry {
                    source,
                    messages,
                    ast: ast.clone(),
                    options,
                },
            );
        ast
    }
}

#[derive(Debug)]
pub struct SourceIndexCache {
    state: Mutex<SourceIndexState>,
}

#[derive(Debug)]
struct SourceIndexState {
    glob_entries: HashMap<u64, u32>,
    entries: HashMap<SourceIndexKey, u32>,
    next_source_index: u32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum SourceIndexKind {
    Normal,
    JsStubForCss,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SourceIndexKey {
    path: Path,
    kind: SourceIndexKind,
}

impl SourceIndexCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SourceIndexState {
                glob_entries: HashMap::new(),
                entries: HashMap::new(),
                next_source_index: runtime::SOURCE_INDEX + 1,
            }),
        }
    }

    #[must_use]
    pub fn len_hint(&self) -> u32 {
        const EXTRA_ROOM: u32 = 16;
        self.state().next_source_index + EXTRA_ROOM
    }

    pub fn get(&self, path: Path, kind: SourceIndexKind) -> u32 {
        let mut state = self.state();
        let key = SourceIndexKey { path, kind };
        if let Some(source_index) = state.entries.get(&key) {
            return *source_index;
        }
        let source_index = state.next_source_index;
        state.next_source_index += 1;
        state.entries.insert(key, source_index);
        source_index
    }

    pub fn get_glob(&self, parent_source_index: u32, glob_index: u32) -> u32 {
        let key = (u64::from(parent_source_index) << 32) | u64::from(glob_index);
        let mut state = self.state();
        if let Some(source_index) = state.glob_entries.get(&key) {
            return *source_index;
        }
        let source_index = state.next_source_index;
        state.next_source_index += 1;
        state.glob_entries.insert(key, source_index);
        source_index
    }

    fn state(&self) -> MutexGuard<'_, SourceIndexState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for SourceIndexCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct FsCache {
    entries: Mutex<HashMap<String, FsEntry>>,
}

#[derive(Clone, Debug)]
struct FsEntry {
    contents: Vec<u8>,
    mod_key: ModKey,
    is_mod_key_usable: bool,
}

impl FsCache {
    pub fn read_file(&self, fs: &dyn Fs, path: &str) -> ReadFileResult {
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned();
        let mod_key = fs.mod_key(path);
        if let (Some(entry), Ok(mod_key)) = (&entry, &mod_key)
            && entry.is_mod_key_usable
            && entry.mod_key == *mod_key
        {
            return (entry.contents.clone(), None, None);
        }

        let (contents, canonical_error, original_error) = fs.read_file(path);
        if canonical_error.is_some() {
            return (contents, canonical_error, original_error);
        }
        let is_mod_key_usable = mod_key.is_ok();
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                path.to_owned(),
                FsEntry {
                    contents: contents.clone(),
                    mod_key: mod_key.unwrap_or_default(),
                    is_mod_key_usable,
                },
            );
        (contents, None, original_error)
    }
}

#[derive(Default)]
pub struct JsonCache {
    entries: Mutex<HashMap<Path, JsonCacheEntry>>,
}

#[derive(Clone)]
struct JsonCacheEntry {
    expression: Expr,
    messages: Vec<Msg>,
    source: Source,
    options: JsonOptions,
    ok: bool,
}

impl JsonCache {
    pub fn parse(&self, log: &Log, source: Source, options: JsonOptions) -> (Expr, bool) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&source.key_path)
            .filter(|entry| entry.source == source && entry.options == options)
            .cloned()
        {
            replay_messages(log, &entry.messages);
            return (entry.expression, entry.ok);
        }

        let temporary_log = Log::new_defer(DeferLogKind::All, log.overrides.as_ref().clone());
        let (expression, ok) = crate::internal::js_parser::parse_json(
            temporary_log.clone(),
            source.clone(),
            options.clone(),
        );
        let messages = temporary_log.done();
        replay_messages(log, &messages);
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                source.key_path.clone(),
                JsonCacheEntry {
                    expression: expression.clone(),
                    messages,
                    source,
                    options,
                    ok,
                },
            );
        (expression, ok)
    }
}

#[derive(Default)]
pub struct JsCache {
    entries: Mutex<HashMap<Path, JsCacheEntry>>,
}

#[derive(Clone)]
struct JsCacheEntry {
    source: Source,
    messages: Vec<Msg>,
    options: Options,
    ast: Ast,
    ok: bool,
}

impl JsCache {
    pub fn parse(&self, log: &Log, source: Source, options: Options) -> (Ast, bool) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&source.key_path)
            .filter(|entry| entry.source == source && entry.options.equal(&options))
            .cloned()
        {
            replay_messages(log, &entry.messages);
            return (entry.ast, entry.ok);
        }

        let temporary_log = Log::new_defer(DeferLogKind::All, log.overrides.as_ref().clone());
        let (ast, ok) = crate::internal::js_parser::parse(
            temporary_log.clone(),
            source.clone(),
            options.clone(),
        );
        let messages = temporary_log.done();
        replay_messages(log, &messages);
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                source.key_path.clone(),
                JsCacheEntry {
                    source,
                    messages,
                    options,
                    ast: ast.clone(),
                    ok,
                },
            );
        (ast, ok)
    }
}

fn replay_messages(log: &Log, messages: &[Msg]) {
    for message in messages {
        log.add_msg(message.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::{CssCache, FsCache, SourceIndexCache, SourceIndexKind};
    use crate::internal::{
        fs::{
            EntryKind, Fs, MockFs, MockKind, ModKey, OpenFileResult, ReadDirectoryResult,
            ReadFileResult, WatchData, mock_fs,
        },
        js_parser::{JsonOptions, Options},
        logger::{DeferLogKind, Log, Path, Source},
    };

    struct CountingFs {
        inner: MockFs,
        reads: AtomicUsize,
    }

    impl Fs for CountingFs {
        fn read_directory(&self, path: &str) -> ReadDirectoryResult {
            self.inner.read_directory(path)
        }

        fn read_file(&self, path: &str) -> ReadFileResult {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.read_file(path)
        }

        fn open_file(&self, path: &str) -> OpenFileResult {
            self.inner.open_file(path)
        }

        fn mod_key(&self, _path: &str) -> Result<ModKey, crate::internal::fs::FsError> {
            Ok(ModKey {
                inode: 1,
                size: 7,
                ..ModKey::default()
            })
        }

        fn is_abs(&self, path: &str) -> bool {
            self.inner.is_abs(path)
        }

        fn abs(&self, path: &str) -> Option<String> {
            self.inner.abs(path)
        }

        fn dir(&self, path: &str) -> String {
            self.inner.dir(path)
        }

        fn base(&self, path: &str) -> String {
            self.inner.base(path)
        }

        fn ext(&self, path: &str) -> String {
            self.inner.ext(path)
        }

        fn join(&self, parts: &[&str]) -> String {
            self.inner.join(parts)
        }

        fn cwd(&self) -> &str {
            self.inner.cwd()
        }

        fn rel(&self, base: &str, target: &str) -> Option<String> {
            self.inner.rel(base, target)
        }

        fn eval_symlinks(&self, path: &str) -> Option<String> {
            self.inner.eval_symlinks(path)
        }

        fn kind(&self, dir: &str, base: &str) -> (String, EntryKind) {
            self.inner.kind(dir, base)
        }

        fn watch_data(&self) -> WatchData {
            self.inner.watch_data()
        }
    }

    #[test]
    fn source_indices_are_stable_and_distinguish_kinds_and_globs() {
        let cache = SourceIndexCache::new();
        let path = Path {
            text: "/entry.js".into(),
            ..Path::default()
        };
        let normal = cache.get(path.clone(), SourceIndexKind::Normal);
        assert_eq!(normal, crate::internal::runtime::SOURCE_INDEX + 1);
        assert_eq!(cache.get(path.clone(), SourceIndexKind::Normal), normal);

        let css_stub = cache.get(path, SourceIndexKind::JsStubForCss);
        assert_ne!(css_stub, normal);
        let glob = cache.get_glob(normal, 3);
        assert_eq!(cache.get_glob(normal, 3), glob);
        assert_ne!(glob, cache.get_glob(normal, 4));
        assert_eq!(cache.len_hint(), glob + 18);

        let set = super::make_cache_set();
        assert_eq!(
            set.source_index_cache
                .get(Path::default(), SourceIndexKind::Normal),
            crate::internal::runtime::SOURCE_INDEX + 1
        );
    }

    #[test]
    fn file_cache_reuses_contents_when_the_modification_key_is_stable() {
        let fs = CountingFs {
            inner: mock_fs(
                &HashMap::from([("/entry.js".into(), "value();".into())]),
                MockKind::Unix,
                "/",
            ),
            reads: AtomicUsize::new(0),
        };
        let cache = FsCache::default();
        assert_eq!(cache.read_file(&fs, "/entry.js").0, b"value();");
        assert_eq!(cache.read_file(&fs, "/entry.js").0, b"value();");
        assert_eq!(fs.reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn syntax_caches_replay_messages_and_return_independent_asts() {
        let css_cache = CssCache::default();
        let css_source = Source {
            contents: Arc::from(&b".entry { color: red }"[..]),
            key_path: Path {
                text: "/entry.css".into(),
                ..Path::default()
            },
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut first = css_cache.parse(
            &log,
            css_source.clone(),
            crate::internal::css_parser::Options::default(),
        );
        assert!(log.done().is_empty());
        first.rules.clear();

        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let second = css_cache.parse(
            &log,
            css_source,
            crate::internal::css_parser::Options::default(),
        );
        assert!(log.done().is_empty());
        assert!(!second.rules.is_empty());

        let json_cache = super::JsonCache::default();
        let json_source = Source {
            contents: Arc::from(&b"{"[..]),
            key_path: Path {
                text: "/data.json".into(),
                ..Path::default()
            },
            ..Source::default()
        };
        for _ in 0..2 {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let (_, ok) = json_cache.parse(&log, json_source.clone(), JsonOptions::default());
            assert!(!ok);
            assert_eq!(log.done().len(), 1);
        }

        let js_cache = super::JsCache::default();
        let js_source = Source {
            contents: Arc::from(&b"const value = 1;"[..]),
            key_path: Path {
                text: "/entry.js".into(),
                ..Path::default()
            },
            ..Source::default()
        };
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let (mut first, ok) = js_cache.parse(&log, js_source.clone(), Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        first.parts[1].statements.clear();

        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let (second, ok) = js_cache.parse(&log, js_source, Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        assert!(!second.parts[1].statements.is_empty());
    }
}
