use super::*;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(10);

fn receive<T>(receiver: &Receiver<T>) -> T {
    receiver
        .recv_timeout(TIMEOUT)
        .expect("coordinated callback completed")
}

struct Gate {
    enabled: AtomicBool,
    entered: Sender<()>,
    release: Mutex<Receiver<()>>,
}

impl Gate {
    fn new() -> (Arc<Self>, Receiver<()>, Sender<()>) {
        let (entered, observed) = mpsc::channel();
        let (release, released) = mpsc::channel();
        (
            Arc::new(Self {
                enabled: AtomicBool::new(true),
                entered,
                release: Mutex::new(released),
            }),
            observed,
            release,
        )
    }

    fn block(&self) {
        if self.enabled.swap(false, Ordering::SeqCst) {
            self.entered.send(()).unwrap();
            receive(&self.release.lock().unwrap());
        }
    }
}

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "esbuild-native-cancel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std_fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn options(&self, plugin: Plugin) -> BuildOptions {
        BuildOptions {
            bundle: true,
            format: BuildFormat::EsModule,
            stdin: Some(BuildStdin {
                contents: "foo()".into(),
                sourcefile: "entry.js".into(),
                ..BuildStdin::default()
            }),
            outfile: "out.js".into(),
            abs_working_dir: self.0.to_string_lossy().into_owned(),
            plugins: vec![plugin],
            ..BuildOptions::default()
        }
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std_fs::remove_dir_all(&self.0);
    }
}

fn rebuild(context: &BuildContext) -> JoinHandle<BuildResult> {
    let context = context.clone();
    thread::spawn(move || context.rebuild())
}

fn cancel(context: &BuildContext) -> JoinHandle<()> {
    let context = context.clone();
    let (observed, waiting) = mpsc::channel();
    let task = thread::spawn(move || context.cancel_with(|| observed.send(()).unwrap()));
    receive(&waiting);
    task
}

fn assert_canceled(result: &BuildResult) {
    assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
    let error = &result.errors[0];
    assert_eq!(error.text, "The build was canceled");
    assert_eq!(error.kind, MessageKind::Error);
    assert!(error.id.is_empty());
    assert!(error.plugin_name.is_empty());
    assert!(error.location.is_none());
    assert!(error.notes.is_empty());
    assert!(result.output_files.is_empty());
    assert!(result.metafile.is_empty());
    assert!(result.mangle_cache.is_none());
}

#[test]
fn cancel_on_start_waits_through_on_end_prevents_writes_and_resets() {
    let directory = Directory::new();
    let (start_gate, started, release_start) = Gate::new();
    let (end_gate, ended, release_end) = Gate::new();
    let (results, end_results) = mpsc::channel();
    let plugin = Plugin::new("coordinated", move |build| {
        let start_gate = start_gate.clone();
        build.on_start(move || {
            start_gate.block();
            Ok(OnStartResult {
                warnings: vec![Message {
                    text: "start warning".into(),
                    ..Message::default()
                }],
                ..OnStartResult::default()
            })
        });
        let end_gate = end_gate.clone();
        let results = results.clone();
        build.on_end(move |result| {
            results.send(result.clone()).unwrap();
            end_gate.block();
            Ok(OnEndResult::default())
        });
        Ok(())
    });
    let context = context(BuildOptions {
        write: true,
        metafile: true,
        mangle_cache: Some(HashMap::new()),
        ..directory.options(plugin)
    })
    .unwrap();
    let output = directory.0.join("out.js");
    std_fs::write(&output, "previous output").unwrap();
    let build = rebuild(&context);
    receive(&started);
    let canceled = cancel(&context);
    assert!(!canceled.is_finished());
    release_start.send(()).unwrap();
    receive(&ended);
    let on_end = receive(&end_results);
    assert_canceled(&on_end);
    assert_eq!(on_end.warnings.len(), 1);
    assert_eq!(on_end.warnings[0].text, "start warning");
    assert!(!canceled.is_finished(), "cancel must also wait for onEnd");
    assert_eq!(std_fs::read(&output).unwrap(), b"previous output");
    release_end.send(()).unwrap();
    assert_canceled(&build.join().unwrap());
    canceled.join().unwrap();
    let next = context.rebuild();
    assert!(next.errors.is_empty(), "{:?}", next.errors);
    assert_eq!(next.output_files.len(), 1);
    assert!(!next.metafile.is_empty());
    assert!(next.mangle_cache.is_some());
    assert_eq!(
        std_fs::read(&output).unwrap(),
        next.output_files[0].contents
    );
    context.dispose();
}

#[test]
fn concurrent_cancels_coalesced_rebuild_and_dispose_wait_for_one_flight() {
    let directory = Directory::new();
    let (gate, started, release) = Gate::new();
    let disposed = Arc::new(AtomicUsize::new(0));
    let dispose_count = disposed.clone();
    let (dispose_callback, callback_observed) = mpsc::channel();
    let plugin = Plugin::new("concurrent", move |build| {
        let gate = gate.clone();
        build.on_start(move || {
            gate.block();
            Ok(OnStartResult::default())
        });
        let count = dispose_count.clone();
        let dispose_callback = dispose_callback.clone();
        build.on_dispose(move || {
            count.fetch_add(1, Ordering::SeqCst);
            dispose_callback.send(()).unwrap();
        });
        Ok(())
    });
    let context = context(directory.options(plugin)).unwrap();
    let first = rebuild(&context);
    receive(&started);
    let (joined, join_observed) = mpsc::channel();
    let joining_context = context.clone();
    let joined_build = thread::spawn(move || {
        joining_context.rebuild_with(
            |_| panic!("overlapping rebuild must coalesce"),
            || joined.send(()).unwrap(),
        )
    });
    receive(&join_observed);
    let cancels: Vec<_> = (0..4).map(|_| cancel(&context)).collect();
    assert!(cancels.iter().all(|task| !task.is_finished()));
    let (waiting, observed) = mpsc::channel();
    let disposing_context = context.clone();
    let disposing =
        thread::spawn(move || disposing_context.dispose_with(|| waiting.send(()).unwrap()));
    receive(&observed);
    assert!(!disposing.is_finished());
    context.cancel(); // Go ignores cancellation after disposal starts.
    assert!(context.rebuild().output_files.is_empty());
    release.send(()).unwrap();
    assert_canceled(&first.join().unwrap());
    assert_canceled(&joined_build.join().unwrap());
    for task in cancels {
        task.join().unwrap();
    }
    disposing.join().unwrap();
    context.dispose();
    receive(&callback_observed);
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
}

#[test]
fn cancel_after_publication_waits_without_retroactively_canceling_or_poisoning_idle() {
    let directory = Directory::new();
    let (gate, ended, release) = Gate::new();
    let plugin = Plugin::new("late", move |build| {
        let gate = gate.clone();
        build.on_end(move |_| {
            gate.block();
            Ok(OnEndResult::default())
        });
        Ok(())
    });
    let context = context(BuildOptions {
        write: true,
        ..directory.options(plugin)
    })
    .unwrap();
    context.cancel(); // Idle before the first build.
    let build = rebuild(&context);
    receive(&ended);
    assert!(
        std_fs::read_to_string(directory.0.join("out.js"))
            .unwrap()
            .contains("foo()")
    );
    let cancellation = cancel(&context);
    assert!(!cancellation.is_finished());
    assert!(
        context
            .inner
            .state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .unwrap()
            .cancellation
            .flag
            .did_cancel()
    );
    release.send(()).unwrap();
    let result = build.join().unwrap();
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.output_files.len(), 1);
    cancellation.join().unwrap();
    let idle: Vec<_> = (0..4)
        .map(|_| {
            let context = context.clone();
            thread::spawn(move || context.cancel())
        })
        .collect();
    for task in idle {
        task.join().unwrap();
    }
    assert!(context.rebuild().errors.is_empty());
    context.dispose();
    context.cancel();
}

#[test]
fn disposal_that_precedes_cancel_does_not_cancel_its_active_build() {
    let directory = Directory::new();
    let (gate, started, release) = Gate::new();
    let plugin = Plugin::new("dispose-first", move |build| {
        let gate = gate.clone();
        build.on_start(move || {
            gate.block();
            Ok(OnStartResult::default())
        });
        Ok(())
    });
    let context = context(directory.options(plugin)).unwrap();
    let build = rebuild(&context);
    receive(&started);
    let (waiting, observed) = mpsc::channel();
    let disposing_context = context.clone();
    let disposing =
        thread::spawn(move || disposing_context.dispose_with(|| waiting.send(()).unwrap()));
    receive(&observed);
    context.cancel();
    release.send(()).unwrap();
    assert!(build.join().unwrap().errors.is_empty());
    disposing.join().unwrap();
}

#[test]
fn cancel_cuts_off_an_unbounded_plugin_dependency_graph_and_next_build_succeeds() {
    let directory = Directory::new();
    let (gate, loaded, release) = Gate::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let looping = Arc::new(AtomicBool::new(true));
    let load_count = calls.clone();
    let loop_forever = looping.clone();
    let blocking_gate = gate.clone();
    let plugin = Plugin::new("infinite", move |build| {
        build.on_resolve(
            OnResolveOptions {
                filter: ".*".into(),
                ..OnResolveOptions::default()
            },
            |args| {
                Ok(OnResolveResult {
                    path: args.path,
                    namespace: "virtual".into(),
                    ..OnResolveResult::default()
                })
            },
        );
        let gate = blocking_gate.clone();
        let calls = load_count.clone();
        let looping = loop_forever.clone();
        build.on_load(
            OnLoadOptions {
                filter: ".*".into(),
                ..OnLoadOptions::default()
            },
            move |args| {
                calls.fetch_add(1, Ordering::SeqCst);
                gate.block();
                Ok(OnLoadResult {
                    contents: Some(if looping.load(Ordering::SeqCst) {
                        format!("import {:?}", args.path + ".")
                    } else {
                        "foo()".into()
                    }),
                    ..OnLoadResult::default()
                })
            },
        );
        Ok(())
    });
    let mut options = directory.options(plugin);
    options.stdin = None;
    options.entry_points = vec!["entry".into()];
    let context = context(options).unwrap();
    for expected_calls in 1..=2 {
        gate.enabled.store(true, Ordering::SeqCst);
        let build = rebuild(&context);
        receive(&loaded);
        let canceled = cancel(&context);
        release.send(()).unwrap();
        assert_canceled(&build.join().unwrap());
        canceled.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
    looping.store(false, Ordering::SeqCst);
    let next = context.rebuild();
    assert!(next.errors.is_empty(), "{:?}", next.errors);
    assert_eq!(next.output_files[0].contents, b"// virtual:entry\nfoo();\n");
    context.dispose();
}

#[test]
fn canceled_watch_build_retains_watch_data_and_later_edits_rebuild() {
    let directory = Directory::new();
    let entry = directory.0.join("entry.js");
    std_fs::write(&entry, "before()").unwrap();
    let (gate, loaded, release) = Gate::new();
    let (results, observed) = mpsc::channel();
    let plugin = Plugin::new("watch-cancel", move |build| {
        let gate = gate.clone();
        build.on_load(
            OnLoadOptions {
                filter: "entry\\.js$".into(),
                ..OnLoadOptions::default()
            },
            move |_| {
                gate.block();
                Ok(OnLoadResult::default())
            },
        );
        let results = results.clone();
        build.on_end(move |result| {
            results.send(result.clone()).unwrap();
            Ok(OnEndResult::default())
        });
        Ok(())
    });
    let mut options = directory.options(plugin);
    options.stdin = None;
    options.entry_points = vec!["entry.js".into()];
    options.write = true;
    let context = context(options).unwrap();
    context.watch(WatchOptions::default()).unwrap();
    receive(&loaded);
    let canceled = cancel(&context);
    release.send(()).unwrap();
    assert_canceled(&receive(&observed));
    canceled.join().unwrap();
    assert!(!directory.0.join("out.js").exists());
    std_fs::write(&entry, "after_watch_cancellation()").unwrap();
    let next = receive(&observed);
    assert!(next.errors.is_empty(), "{:?}", next.errors);
    assert!(
        String::from_utf8_lossy(&next.output_files[0].contents)
            .contains("after_watch_cancellation")
    );
    context.dispose();
}

#[test]
fn cancellation_preserves_existing_plugin_errors_without_a_duplicate_diagnostic() {
    let directory = Directory::new();
    let (gate, started, release) = Gate::new();
    let plugin = Plugin::new("start-error", move |build| {
        let gate = gate.clone();
        build.on_start(move || {
            gate.block();
            Err(PluginError::new("start failed"))
        });
        Ok(())
    });
    let context = context(directory.options(plugin)).unwrap();
    let build = rebuild(&context);
    receive(&started);
    let canceled = cancel(&context);
    release.send(()).unwrap();
    let result = build.join().unwrap();
    canceled.join().unwrap();
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].text, "start failed");
    assert_eq!(result.errors[0].plugin_name, "start-error");
    assert!(result.output_files.is_empty());
    context.dispose();
}

#[test]
fn canceled_rebuild_deletes_tracked_chunks_clears_hashes_and_next_build_succeeds() {
    let directory = Directory::new();
    std_fs::write(
        directory.0.join("entry.js"),
        "import('./lazy.js').then(console.log)",
    )
    .unwrap();
    std_fs::write(directory.0.join("lazy.js"), "export default 123").unwrap();
    let (gate, started, release) = Gate::new();
    gate.enabled.store(false, Ordering::SeqCst);
    let start_gate = gate.clone();
    let plugin = Plugin::new("chunks", move |build| {
        let gate = start_gate.clone();
        build.on_start(move || {
            gate.block();
            Ok(OnStartResult::default())
        });
        Ok(())
    });
    let mut options = directory.options(plugin);
    options.stdin = None;
    options.entry_points = vec!["entry.js".into()];
    options.outfile.clear();
    options.outdir = "out".into();
    options.splitting = true;
    options.write = true;
    let context = context(options).unwrap();
    let first = context.rebuild();
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(first.output_files.len(), 2);
    assert_eq!(context.inner.state.lock().unwrap().latest_hashes.len(), 2);
    std_fs::write(directory.0.join("entry.js"), "after_cancellation()").unwrap();
    gate.enabled.store(true, Ordering::SeqCst);
    let build = rebuild(&context);
    receive(&started);
    let canceled = cancel(&context);
    release.send(()).unwrap();
    assert_canceled(&build.join().unwrap());
    canceled.join().unwrap();
    for output in &first.output_files {
        assert!(!FsPath::new(&output.path).exists());
    }
    assert!(context.inner.state.lock().unwrap().latest_hashes.is_empty());
    let next = context.rebuild();
    assert!(next.errors.is_empty(), "{:?}", next.errors);
    assert_eq!(next.output_files.len(), 1);
    assert!(String::from_utf8_lossy(&next.output_files[0].contents).contains("after_cancellation"));
    for output in &first.output_files {
        if output.path != next.output_files[0].path {
            assert!(!FsPath::new(&output.path).exists());
        }
    }
    context.dispose();
}

fn cancel_after_compile(linker_error: bool) -> (Directory, BuildContext, BuildResult) {
    let directory = Directory::new();
    let source = if linker_error {
        std_fs::write(directory.0.join("dep.js"), "export const other = 1").unwrap();
        "import { missing } from './dep.js'; console.log(missing)"
    } else {
        "foo()"
    };
    let (start_gate, started, release_start) = Gate::new();
    let plugin = Plugin::new("post-compile", move |build| {
        let gate = start_gate.clone();
        build.on_start(move || {
            gate.block();
            Ok(OnStartResult::default())
        });
        Ok(())
    });
    let mut options = directory.options(plugin);
    options.stdin.as_mut().unwrap().contents = source.into();
    options.stdin.as_mut().unwrap().resolve_dir = directory.0.to_string_lossy().into_owned();
    options.write = true;
    options.metafile = true;
    options.mangle_cache = Some(HashMap::new());
    let context = context(options).unwrap();
    let build = rebuild(&context);
    receive(&started);
    let (compile_gate, compiled, release_compile) = Gate::new();
    let flight = context.inner.state.lock().unwrap().active.clone().unwrap();
    // This hook belongs to this flight, so parallel tests cannot intercept it.
    *flight.cancellation.after_compile.lock().unwrap() =
        Some(Box::new(move || compile_gate.block()));
    release_start.send(()).unwrap();
    receive(&compiled);
    assert!(!directory.0.join("out.js").exists());
    let canceled = cancel(&context);
    release_compile.send(()).unwrap();
    let result = build.join().unwrap();
    canceled.join().unwrap();
    assert!(result.metafile.is_empty());
    assert!(result.mangle_cache.is_none());
    (directory, context, result)
}

#[test]
fn cancel_after_successful_compile_returns_and_writes_output_matching_go() {
    let (directory, context, result) = cancel_after_compile(false);
    assert_eq!(
        result
            .errors
            .iter()
            .map(|error| error.text.as_str())
            .collect::<Vec<_>>(),
        ["The build was canceled"]
    );
    assert_eq!(result.output_files.len(), 1);
    let output = &result.output_files[0];
    assert_eq!(
        std_fs::read(directory.0.join("out.js")).unwrap(),
        output.contents
    );
    assert!(String::from_utf8_lossy(&output.contents).ends_with("\nfoo();\n"));
    assert_eq!(
        context.inner.state.lock().unwrap().latest_hashes,
        HashMap::from([(output.path.clone(), output.hash.clone())])
    );
    let next = context.rebuild();
    assert!(next.errors.is_empty(), "{:?}", next.errors);
    assert_eq!(next.output_files[0].contents, output.contents);
    assert!(!next.metafile.is_empty());
    assert!(next.mangle_cache.is_some());
    context.dispose();
}

#[test]
fn cancel_after_compile_errors_adds_cancellation_alongside_linker_error_matching_go() {
    let (directory, context, result) = cancel_after_compile(true);
    assert_eq!(
        result
            .errors
            .iter()
            .map(|error| error.text.as_str())
            .collect::<Vec<_>>(),
        [
            "The build was canceled",
            "No matching export in \"dep.js\" for import \"missing\"",
        ]
    );
    assert!(result.errors[0].location.is_none());
    assert!(result.errors[0].plugin_name.is_empty());
    assert!(result.output_files.is_empty());
    assert!(!directory.0.join("out.js").exists());
    assert!(context.inner.state.lock().unwrap().latest_hashes.is_empty());
    let next = context.rebuild();
    assert_eq!(next.errors.len(), 1);
    assert_eq!(
        next.errors[0].text,
        "No matching export in \"dep.js\" for import \"missing\""
    );
    context.dispose();
}

#[test]
fn on_start_replays_an_earlier_idle_cancel_with_request_acknowledgment() {
    let directory = Directory::new();
    let native = Arc::new(Mutex::new(None::<Weak<BuildContextInner>>));
    let replay = Arc::new(AtomicBool::new(true));
    let (workers, observed_worker) = mpsc::channel();
    let active = native.clone();
    let plugin = Plugin::new("replay", move |build| {
        let active = active.clone();
        let replay = replay.clone();
        let workers = workers.clone();
        build.on_start(move || {
            if replay.swap(false, Ordering::SeqCst) {
                let inner = active.lock().unwrap().as_ref().unwrap().upgrade().unwrap();
                let (requested, acknowledged) = mpsc::channel();
                workers
                    .send(thread::spawn(move || {
                        BuildContext { inner }.cancel_with(|| requested.send(()).unwrap());
                    }))
                    .unwrap();
                // Waiting for the worker to finish here would deadlock. The
                // acknowledgment proves its request preceded onStart's return.
                receive(&acknowledged);
            }
            Ok(OnStartResult::default())
        });
        Ok(())
    });
    let context = context(directory.options(plugin)).unwrap();
    *native.lock().unwrap() = Some(Arc::downgrade(&context.inner));
    context.cancel(); // The first service cancel worker can encounter idle.
    assert_canceled(&context.rebuild());
    receive(&observed_worker).join().unwrap();
    assert!(context.rebuild().errors.is_empty());
    context.dispose();
}

#[test]
fn cancellation_is_per_flight_and_canceled_outputs_clear_previous_hashes() {
    let cancellation = Arc::new(BuildCancellation::default());
    let other = cancellation.clone();
    thread::spawn(move || other.cancel()).join().unwrap();
    assert!(cancellation.flag.did_cancel());
    cancellation.cancel();
    assert!(cancellation.flag.did_cancel());
    assert!(!BuildCancellation::default().flag.did_cancel());
    let old_hashes = HashMap::from([("/previous/chunk.js".into(), "old hash".into())]);
    let (result, hashes) = build_with_output_state(
        BuildOptions::default(),
        &CacheSet::default(),
        &PreparedPlugins::default(),
        Some(&old_hashes),
        None,
        Some(&cancellation),
    );
    assert_canceled(&result);
    assert!(hashes.is_empty());
}
