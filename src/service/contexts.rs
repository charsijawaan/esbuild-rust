//! Native context ownership and reader-side operation admission.
//!
//! Mirrors activeBuild's disposal barrier in pinned cmd/esbuild/service.go.
//! Registry/state locks are never held while compiling, resolving, disposing,
//! or waiting for JavaScript. Rebuild coalescing belongs to `api::BuildContext`.

use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{options::LogSettings, plugins::PluginBridge};
use crate::api;

#[derive(Default)]
pub(super) struct Contexts {
    builds: Mutex<HashMap<u32, Arc<Context>>>,
}

struct State {
    native: Option<api::BuildContext>,
    bridge: Option<Arc<PluginBridge>>,
    admitted: usize,
    phase: Phase,
    creating: bool,
    rebuild_group: Option<Arc<RebuildGroup>>,
}

#[derive(PartialEq, Eq)]
enum Phase {
    Live,
    Disposing,
    Cleaning,
    Disposed,
}

pub(super) struct Context {
    state: Mutex<State>,
    changed: Condvar,
    pub log_settings: LogSettings,
}

impl Contexts {
    pub fn create(&self, key: u32, log_settings: LogSettings) -> Result<Creation<'_>, String> {
        let mut builds = self
            .builds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if builds.contains_key(&key) {
            return Err(format!(
                "Cannot create context: build key {key} is already active"
            ));
        }
        let context = Arc::new(Context {
            state: Mutex::new(State {
                native: None,
                bridge: None,
                admitted: 0,
                phase: Phase::Live,
                creating: true,
                rebuild_group: None,
            }),
            changed: Condvar::new(),
            log_settings,
        });
        builds.insert(key, context.clone());
        Ok(Creation {
            contexts: self,
            key,
            context,
            complete: false,
        })
    }

    fn get(&self, key: u32) -> Option<Arc<Context>> {
        self.builds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
    }

    pub fn rebuild(&self, key: u32) -> Result<Operation, String> {
        self.get(key)
            .ok_or_else(|| "Cannot rebuild".to_string())?
            .admit("Cannot rebuild", true)
    }

    pub fn watch(&self, key: u32) -> Result<Operation, String> {
        self.get(key)
            .ok_or_else(|| "Cannot watch".to_string())?
            .admit("Cannot watch", false)
    }

    // Standalone plugin builds use the plugin registry without a native context.
    pub fn resolve(&self, key: u32) -> Option<Operation> {
        let context = self.get(key)?;
        let mut state = context
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Go keeps pluginResolve available after ctx is cleared for disposal.
        // In particular, an already-running onStart/onEnd can resolve imports
        // while disposal waits for that build. Retain this admitted operation
        // until it finishes; only native teardown retires the capability.
        let native = state.native.clone()?;
        state.admitted += 1;
        drop(state);
        Some(Operation {
            context,
            native,
            group: None,
        })
    }

    pub fn cancel(&self, key: u32) -> Option<Cancel> {
        let context = self.get(key)?;
        // Go treats inactive/disposed contexts as a genuine no-op.
        let operation = context.admit("Cannot cancel", false).ok()?;
        let group = context
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rebuild_group
            .clone();
        if let Some(group) = &group {
            group.cancelled.store(true, Ordering::SeqCst);
        }
        Some(Cancel { operation, group })
    }

    pub fn begin_dispose(&self, key: u32) -> Option<Arc<Context>> {
        let context = self.get(key)?;
        let mut state = context
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase == Phase::Live {
            state.phase = Phase::Disposing;
        }
        drop(state);
        Some(context)
    }

    pub fn finish_dispose(&self, key: u32, context: &Arc<Context>) {
        context.dispose();
        let mut builds = self
            .builds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if builds
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, context))
        {
            builds.remove(&key);
        }
    }

    // Called only after closing the callback transport and draining admitted
    // workers. The host can no longer acknowledge callbacks after stdin EOF.
    pub fn shutdown(&self) {
        let builds = std::mem::take(
            &mut *self
                .builds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for context in builds.into_values() {
            context.dispose();
        }
    }
}

impl Context {
    pub fn has_rebuild(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rebuild_group
            .is_some()
    }

    fn admit(self: &Arc<Self>, error: &str, rebuild: bool) -> Result<Operation, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase != Phase::Live {
            return Err(error.into());
        }
        let native = state.native.clone().ok_or_else(|| error.to_string())?;
        state.admitted += 1;
        let group = rebuild.then(|| {
            let group = state
                .rebuild_group
                .get_or_insert_with(|| Arc::new(RebuildGroup::default()))
                .clone();
            *group
                .count
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
            group
        });
        Ok(Operation {
            context: self.clone(),
            native,
            group,
        })
    }

    pub fn cancel_on_start_plugin(self: &Arc<Self>) -> api::Plugin {
        let context = Arc::downgrade(self);
        api::Plugin::new("service cancellation", move |build| {
            let context = context.clone();
            build.on_start(move || {
                if let Some(context) = context.upgrade() {
                    let state = context
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let native = if state
                        .rebuild_group
                        .as_ref()
                        .is_some_and(|group| group.cancelled.load(Ordering::SeqCst))
                    {
                        state.native.clone()
                    } else {
                        None
                    };
                    drop(state);
                    if let Some(native) = native {
                        // Apply the cancellation before allowing this onStart
                        // to finish, but wait for the flight on a separate worker.
                        // Holding a state lock or calling cancel here deadlocks.
                        let (armed, wait) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            native.cancel_with(|| {
                                let _ = armed.send(());
                            });
                        });
                        let _ = wait.recv();
                    }
                }
                Ok(api::OnStartResult::default())
            });
            Ok(())
        })
    }

    fn dispose(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase == Phase::Disposed {
            return;
        }
        if state.phase == Phase::Cleaning {
            drop(
                self.changed
                    .wait_while(state, |state| state.phase != Phase::Disposed)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            return;
        }
        state.phase = Phase::Cleaning;
        let mut state = self
            .changed
            .wait_while(state, |state| state.creating || state.admitted != 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let native = state.native.take();
        let bridge = state.bridge.take();
        drop(state);
        // Every disposer waits for this cleanup, including concurrent callers.
        // Mark completion even if a native onDispose callback panics.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(native) = native {
                native.dispose();
            }
            drop(bridge);
        }));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.phase = Phase::Disposed;
        self.changed.notify_all();
        drop(state);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}

pub(super) struct Operation {
    context: Arc<Context>,
    pub native: api::BuildContext,
    group: Option<Arc<RebuildGroup>>,
}

impl Operation {
    pub fn log_settings(&self) -> &LogSettings {
        &self.context.log_settings
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        let mut state = self
            .context
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.admitted -= 1;
        if let Some(group) = &self.group {
            let mut count = group
                .count
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *count -= 1;
            if *count == 0 {
                state.rebuild_group = None;
                group.changed.notify_all();
            }
        }
        self.context.changed.notify_all();
    }
}

#[derive(Default)]
struct RebuildGroup {
    count: Mutex<usize>,
    changed: Condvar,
    cancelled: AtomicBool,
}

pub(super) struct Cancel {
    operation: Operation,
    group: Option<Arc<RebuildGroup>>,
}

impl Cancel {
    pub fn run(self) {
        self.operation.native.cancel();
        if let Some(group) = &self.group {
            let count = group
                .count
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(
                group
                    .changed
                    .wait_while(count, |count| *count != 0)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
    }
}

// An unfinished native context (including panic/error paths) cannot strand a
// disposing worker or leave its key registered.
pub(super) struct Creation<'a> {
    contexts: &'a Contexts,
    key: u32,
    pub context: Arc<Context>,
    complete: bool,
}

impl Creation<'_> {
    pub fn complete(mut self, native: api::BuildContext, bridge: Arc<PluginBridge>) {
        let mut state = self
            .context
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.native = Some(native);
        state.bridge = Some(bridge);
        state.creating = false;
        self.complete = true;
        self.context.changed.notify_all();
    }
}

impl Drop for Creation<'_> {
    fn drop(&mut self) {
        if !self.complete {
            let mut state = self
                .context
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.creating = false;
            if state.phase == Phase::Live {
                state.phase = Phase::Disposing;
            }
            self.context.changed.notify_all();
            drop(state);
            self.contexts.finish_dispose(self.key, &self.context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::protocol::Value;
    use super::*;
    use std::{io, sync::mpsc, thread, time::Duration};

    fn native_context(contexts: &Contexts, plugins: Vec<api::Plugin>) {
        let creation = contexts.create(1, LogSettings::default()).unwrap();
        let mut options = api::BuildOptions {
            write: false,
            stdin: Some(api::BuildStdin {
                contents: "export const value = 1".into(),
                ..api::BuildStdin::default()
            }),
            plugins,
            ..api::BuildOptions::default()
        };
        options
            .plugins
            .push(creation.context.cancel_on_start_plugin());
        let native = api::context(options).unwrap();
        let bridge = PluginBridge::new(
            1,
            &Value::Array(vec![]),
            Arc::new(|_| Err(io::Error::other("unexpected host callback"))),
            Arc::default(),
        )
        .unwrap();
        creation.complete(native, Arc::new(bridge));
    }

    #[test]
    fn every_disposer_waits_for_real_native_cleanup_once() {
        let contexts = Arc::new(Contexts::default());
        let (entered, reached) = mpsc::channel();
        let (release, unblock) = mpsc::channel();
        let unblock = Arc::new(Mutex::new(unblock));
        let (cleaned, cleanup) = mpsc::channel();
        native_context(
            &contexts,
            vec![api::Plugin::new("cleanup barrier", move |build| {
                let entered = entered.clone();
                let unblock = unblock.clone();
                build.on_start(move || {
                    entered.send(()).unwrap();
                    unblock.lock().unwrap().recv().unwrap();
                    Ok(api::OnStartResult::default())
                });
                let cleaned = cleaned.clone();
                build.on_dispose(move || {
                    cleaned.send(()).unwrap();
                });
                Ok(())
            })],
        );
        // Enter a flight directly so dispose waits inside the native API,
        // independent of the service's operation-admission lease.
        let native = contexts
            .get(1)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .native
            .clone()
            .unwrap();
        let builder = thread::spawn(move || native.rebuild());
        reached.recv_timeout(Duration::from_secs(5)).unwrap();
        let record = contexts.begin_dispose(1).unwrap();
        let other = contexts.begin_dispose(1).unwrap();
        let (done, wait) = mpsc::channel();
        let first = {
            let contexts = contexts.clone();
            let done = done.clone();
            thread::spawn(move || {
                contexts.finish_dispose(1, &record);
                done.send(()).unwrap();
            })
        };
        let (started, admitted) = mpsc::channel();
        let second = {
            let contexts = contexts.clone();
            thread::spawn(move || {
                started.send(()).unwrap();
                contexts.finish_dispose(1, &other);
                done.send(()).unwrap();
            })
        };
        admitted.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(wait.recv_timeout(Duration::from_millis(100)).is_err());
        release.send(()).unwrap();
        for _ in 0..2 {
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        first.join().unwrap();
        second.join().unwrap();
        assert!(builder.join().unwrap().errors.is_empty());
        cleanup.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(cleanup.try_recv().is_err(), "native onDispose runs once");
        assert!(contexts.get(1).is_none());
    }

    #[test]
    fn reader_admitted_rebuild_remains_live_until_worker_finishes() {
        let contexts = Arc::new(Contexts::default());
        native_context(&contexts, vec![]);
        let admitted = contexts.rebuild(1).unwrap();
        let record = contexts.begin_dispose(1).unwrap();
        assert!(contexts.rebuild(1).is_err());
        let (done, wait) = mpsc::channel();
        let disposer = {
            let contexts = contexts.clone();
            thread::spawn(move || {
                contexts.finish_dispose(1, &record);
                done.send(()).unwrap();
            })
        };
        assert!(wait.recv_timeout(Duration::from_millis(100)).is_err());
        let result = admitted.native.rebuild();
        assert!(result.errors.is_empty());
        assert_eq!(
            result.output_files.len(),
            1,
            "admitted build cannot see native disposal/default result"
        );
        drop(admitted);
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        disposer.join().unwrap();
    }

    #[test]
    fn reader_admitted_watch_start_remains_live_until_worker_finishes() {
        let contexts = Arc::new(Contexts::default());
        native_context(&contexts, vec![]);
        let admitted = contexts.watch(1).unwrap();
        let record = contexts.begin_dispose(1).unwrap();
        assert!(contexts.watch(1).is_err());
        let (done, wait) = mpsc::channel();
        let disposer = {
            let contexts = contexts.clone();
            thread::spawn(move || {
                contexts.finish_dispose(1, &record);
                done.send(()).unwrap();
            })
        };
        assert!(wait.recv_timeout(Duration::from_millis(100)).is_err());
        admitted.native.watch(api::WatchOptions::default()).unwrap();
        drop(admitted);
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        disposer.join().unwrap();
        assert!(contexts.get(1).is_none());
    }

    #[test]
    fn cancel_before_native_flight_is_armed_in_on_start_and_not_sticky() {
        let contexts = Contexts::default();
        native_context(&contexts, vec![]);
        let admitted = contexts.rebuild(1).unwrap();
        let cancel = contexts.cancel(1).unwrap();
        let (done, wait) = mpsc::channel();
        let worker = thread::spawn(move || {
            cancel.run();
            done.send(()).unwrap();
        });
        assert!(wait.recv_timeout(Duration::from_millis(100)).is_err());
        let result = admitted.native.rebuild();
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].text, "The build was canceled");
        assert!(result.output_files.is_empty());
        drop(admitted);
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        let admitted = contexts.rebuild(1).unwrap();
        let result = admitted.native.rebuild();
        assert!(result.errors.is_empty());
        assert_eq!(result.output_files.len(), 1);
        drop(admitted);
        let record = contexts.begin_dispose(1).unwrap();
        contexts.finish_dispose(1, &record);
    }
}
