//! Polling file watcher used by the context API.
//!
//! This follows esbuild's portable watcher design: recently changed paths are
//! checked on every interval, while the remaining paths are shuffled and
//! spread over at most 20 intervals.

use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, Weak},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::internal::fs::{WatchCallback, WatchData};

const WATCH_INTERVAL: Duration = Duration::from_millis(100);
const MAX_RECENT_ITEM_COUNT: usize = 16;
const MIN_ITEM_COUNT_PER_ITERATION: usize = 64;
const MAX_INTERVALS_BEFORE_UPDATE: usize = 20;

pub(super) struct Watcher {
    delay: Duration,
    state: Mutex<WatcherState>,
    changed: Condvar,
    worker: Mutex<Option<JoinHandle<()>>>,
}

struct WatcherState {
    data: WatchData,
    generation: u64,
    recent_items: Vec<String>,
    items_to_scan: Vec<String>,
    items_per_iteration: usize,
    should_stop: bool,
    random: XorShift64,
}

#[derive(Clone)]
struct ScanCandidate {
    path: String,
    callback: WatchCallback,
    was_recent: bool,
}

impl Watcher {
    pub(super) fn new(delay_ms: i64) -> Arc<Self> {
        let delay_ms = u64::try_from(delay_ms).unwrap_or(0);
        Arc::new(Self {
            delay: Duration::from_millis(delay_ms),
            state: Mutex::new(WatcherState {
                data: WatchData::default(),
                generation: 0,
                recent_items: Vec::new(),
                items_to_scan: Vec::new(),
                items_per_iteration: 0,
                should_stop: false,
                random: XorShift64::new(random_seed()),
            }),
            changed: Condvar::new(),
            worker: Mutex::new(None),
        })
    }

    pub(super) fn set_watch_data(&self, data: WatchData) {
        let mut state = lock_unpoisoned(&self.state);
        state.generation = state.generation.wrapping_add(1);
        state
            .recent_items
            .retain(|path| data.paths.contains_key(path));
        state.data = data;
        state.items_to_scan.clear();
        state.items_per_iteration = 0;
    }

    pub(super) fn start(self: &Arc<Self>, rebuild: Arc<dyn Fn() + Send + Sync>) {
        // Holding this lock until the handle has been stored makes a concurrent
        // call to "stop" reliably take and join the newly-created worker.
        let mut worker = lock_unpoisoned(&self.worker);
        if worker.is_some() || lock_unpoisoned(&self.state).should_stop {
            return;
        }

        let watcher = Arc::downgrade(self);
        *worker = Some(thread::spawn(move || worker_loop(&watcher, &rebuild)));
    }

    pub(super) fn stop(&self) {
        {
            let mut state = lock_unpoisoned(&self.state);
            state.should_stop = true;
            self.changed.notify_all();
        }

        let worker = lock_unpoisoned(&self.worker).take();
        if let Some(worker) = worker {
            // A rebuild callback is allowed to stop its own watcher. Joining
            // the current thread is impossible, but setting "should_stop"
            // above still makes the worker exit immediately after the callback.
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }

    fn wait_or_stopped(&self, duration: Duration) -> bool {
        let state = lock_unpoisoned(&self.state);
        if state.should_stop || duration.is_zero() {
            return state.should_stop;
        }
        let (state, _) = self
            .changed
            .wait_timeout_while(state, duration, |state| !state.should_stop)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.should_stop
    }

    fn try_to_find_dirty_path(&self) -> Option<String> {
        let (generation, candidates) = self.scan_candidates();

        // Watch callbacks can touch the file system and may be comparatively
        // expensive, so don't hold the watcher mutex while invoking them.
        for candidate in candidates {
            let dirty_path = (candidate.callback)();
            if !self.generation_is_current(generation) {
                return None;
            }
            if !dirty_path.is_empty() {
                if self.record_dirty(generation, &candidate) {
                    return Some(dirty_path);
                }
                return None;
            }
        }
        None
    }

    fn scan_candidates(&self) -> (u64, Vec<ScanCandidate>) {
        let mut state = lock_unpoisoned(&self.state);

        // If all ordinary items were consumed, fill the list back up in a
        // random order. This is a local Fisher-Yates shuffle instead of a
        // dependency on a general-purpose random-number generator.
        if state.items_to_scan.is_empty() {
            let mut items = state.data.paths.keys().cloned().collect::<Vec<_>>();
            state.random.shuffle(&mut items);
            state.items_per_iteration = items
                .len()
                .div_ceil(MAX_INTERVALS_BEFORE_UPDATE)
                .max(MIN_ITEM_COUNT_PER_ITERATION);
            state.items_to_scan = items;
        }

        let generation = state.generation;
        let mut candidates =
            Vec::with_capacity(state.recent_items.len() + state.items_per_iteration);
        candidates.extend(state.recent_items.iter().filter_map(|path| {
            state.data.paths.get(path).map(|callback| ScanCandidate {
                path: path.clone(),
                callback: Arc::clone(callback),
                was_recent: true,
            })
        }));

        let first = state
            .items_to_scan
            .len()
            .saturating_sub(state.items_per_iteration);
        let ordinary = state.items_to_scan.split_off(first);
        candidates.extend(ordinary.into_iter().filter_map(|path| {
            state.data.paths.get(&path).map(|callback| ScanCandidate {
                path,
                callback: Arc::clone(callback),
                was_recent: false,
            })
        }));
        (generation, candidates)
    }

    fn generation_is_current(&self, generation: u64) -> bool {
        lock_unpoisoned(&self.state).generation == generation
    }

    fn record_dirty(&self, generation: u64, candidate: &ScanCandidate) -> bool {
        let mut state = lock_unpoisoned(&self.state);
        if state.generation != generation
            || !state
                .data
                .paths
                .get(&candidate.path)
                .is_some_and(|callback| Arc::ptr_eq(callback, &candidate.callback))
        {
            return false;
        }

        if candidate.was_recent {
            if let Some(index) = state
                .recent_items
                .iter()
                .position(|path| path == &candidate.path)
            {
                let path = state.recent_items.remove(index);
                state.recent_items.push(path);
            }
        } else {
            state.recent_items.push(candidate.path.clone());
            if state.recent_items.len() > MAX_RECENT_ITEM_COUNT {
                state.recent_items.remove(0);
            }
        }
        true
    }
}

fn worker_loop(watcher: &Weak<Watcher>, rebuild: &Arc<dyn Fn() + Send + Sync>) {
    loop {
        let Some(watcher) = watcher.upgrade() else {
            return;
        };
        if watcher.wait_or_stopped(WATCH_INTERVAL) {
            return;
        }

        if watcher.try_to_find_dirty_path().is_some() {
            if watcher.wait_or_stopped(watcher.delay) {
                return;
            }

            // The watcher holds no mutex while rebuilding. The callback can
            // safely update watch data or stop the watcher itself.
            rebuild();
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let upper = u64::try_from(index + 1).expect("slice length fits in u64");
            let shuffled =
                usize::try_from(self.next() % upper).expect("shuffle index fits in usize");
            items.swap(index, shuffled);
        }
    }
}

fn random_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let low = u64::try_from(nanos & u128::from(u64::MAX)).expect("masked value fits in u64");
    let high = u64::try_from(nanos >> u64::BITS).expect("shifted value fits in u64");
    low ^ high ^ u64::from(std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    fn data_with_callbacks(
        callbacks: impl IntoIterator<Item = (String, WatchCallback)>,
    ) -> WatchData {
        WatchData {
            paths: callbacks.into_iter().collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn ordinary_items_are_spread_over_at_most_twenty_intervals() {
        let watcher = Watcher::new(0);
        let calls = Arc::new(AtomicUsize::new(0));
        let data = data_with_callbacks((0..1_281).map(|index| {
            let calls = Arc::clone(&calls);
            let callback: WatchCallback = Arc::new(move || {
                calls.fetch_add(1, Ordering::Relaxed);
                String::new()
            });
            (format!("/file-{index}"), callback)
        }));
        watcher.set_watch_data(data);

        for _ in 0..MAX_INTERVALS_BEFORE_UPDATE {
            assert_eq!(watcher.try_to_find_dirty_path(), None);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1_281);
    }

    #[test]
    fn dirty_items_are_kept_in_a_bounded_recent_list() {
        let watcher = Watcher::new(0);
        let dirty = (0..17)
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect::<Vec<_>>();
        let data = data_with_callbacks(dirty.iter().enumerate().map(|(index, dirty)| {
            let dirty = Arc::clone(dirty);
            let path = format!("/file-{index}");
            let callback_path = path.clone();
            let callback: WatchCallback = Arc::new(move || {
                if dirty.swap(false, Ordering::Relaxed) {
                    callback_path.clone()
                } else {
                    String::new()
                }
            });
            (path, callback)
        }));
        watcher.set_watch_data(data);

        for (index, dirty) in dirty.iter().enumerate() {
            dirty.store(true, Ordering::Relaxed);
            assert_eq!(
                watcher.try_to_find_dirty_path(),
                Some(format!("/file-{index}"))
            );
        }

        let state = lock_unpoisoned(&watcher.state);
        assert_eq!(state.recent_items.len(), MAX_RECENT_ITEM_COUNT);
        assert_eq!(
            state.recent_items.first().map(String::as_str),
            Some("/file-1")
        );
        assert_eq!(
            state.recent_items.last().map(String::as_str),
            Some("/file-16")
        );
    }

    #[test]
    fn replacing_data_discards_the_old_scan_and_prunes_recent_items() {
        let watcher = Watcher::new(0);
        let old_dirty = Arc::new(AtomicBool::new(true));
        let old_callback: WatchCallback = {
            let old_dirty = Arc::clone(&old_dirty);
            Arc::new(move || {
                if old_dirty.swap(false, Ordering::Relaxed) {
                    "/keep".into()
                } else {
                    String::new()
                }
            })
        };
        watcher.set_watch_data(data_with_callbacks([
            ("/keep".into(), old_callback),
            ("/remove".into(), Arc::new(String::new) as WatchCallback),
        ]));
        assert_eq!(watcher.try_to_find_dirty_path(), Some("/keep".into()));

        let new_calls = Arc::new(AtomicUsize::new(0));
        let new_callback: WatchCallback = {
            let new_calls = Arc::clone(&new_calls);
            Arc::new(move || {
                new_calls.fetch_add(1, Ordering::Relaxed);
                String::new()
            })
        };
        watcher.set_watch_data(data_with_callbacks([
            ("/keep".into(), Arc::clone(&new_callback)),
            ("/new".into(), new_callback),
        ]));

        {
            let state = lock_unpoisoned(&watcher.state);
            assert!(state.items_to_scan.is_empty());
            assert_eq!(state.recent_items, ["/keep"]);
        }
        assert_eq!(watcher.try_to_find_dirty_path(), None);
        assert_eq!(new_calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn stop_wakes_and_joins_the_worker() {
        let watcher = Watcher::new(0);
        let rebuilt = Arc::new((Mutex::new(false), Condvar::new()));
        watcher.set_watch_data(data_with_callbacks([(
            "/dirty".into(),
            Arc::new(|| "/dirty".into()) as WatchCallback,
        )]));
        watcher.start({
            let rebuilt = Arc::clone(&rebuilt);
            Arc::new(move || {
                let (lock, changed) = &*rebuilt;
                *lock_unpoisoned(lock) = true;
                changed.notify_all();
            })
        });

        let (lock, changed) = &*rebuilt;
        let rebuilt = lock_unpoisoned(lock);
        let (rebuilt, _) = changed
            .wait_timeout_while(rebuilt, Duration::from_secs(2), |rebuilt| !*rebuilt)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(*rebuilt);
        drop(rebuilt);

        watcher.stop();
        assert!(lock_unpoisoned(&watcher.worker).is_none());
        assert!(lock_unpoisoned(&watcher.state).should_stop);
    }
}
