// Port of upstream internal/helpers/waitgroup.go.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

/// A wait group that permits `add` to run concurrently with `wait`.
///
/// Like the upstream implementation, this supports one waiter.
#[derive(Debug)]
pub struct ThreadSafeWaitGroup {
    counter: AtomicI32,
    sender: SyncSender<()>,
    receiver: Mutex<Receiver<()>>,
}

impl ThreadSafeWaitGroup {
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = sync_channel(1);
        Self {
            counter: AtomicI32::new(0),
            sender,
            receiver: Mutex::new(receiver),
        }
    }

    /// # Panics
    ///
    /// Panics if the counter becomes negative or if the internal notification
    /// channel is unexpectedly disconnected.
    pub fn add(&self, delta: i32) {
        let counter = self
            .counter
            .fetch_add(delta, Ordering::SeqCst)
            .wrapping_add(delta);
        if counter == 0 {
            self.sender
                .send(())
                .expect("wait-group notification channel disconnected");
        } else {
            assert!(counter > 0, "sync: negative WaitGroup counter");
        }
    }

    /// # Panics
    ///
    /// Panics if the counter is already zero.
    pub fn done(&self) {
        self.add(-1);
    }

    /// # Panics
    ///
    /// Panics if the internal notification channel is poisoned or
    /// unexpectedly disconnected.
    pub fn wait(&self) {
        self.receiver
            .lock()
            .expect("wait-group receiver mutex was poisoned")
            .recv()
            .expect("wait-group notification channel disconnected");
    }
}

impl Default for ThreadSafeWaitGroup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ThreadSafeWaitGroup;
    use std::sync::Arc;

    #[test]
    fn add_can_race_with_wait() {
        let wait_group = Arc::new(ThreadSafeWaitGroup::new());
        let worker_group = Arc::clone(&wait_group);
        let worker = std::thread::spawn(move || {
            worker_group.add(1);
            worker_group.done();
        });
        wait_group.wait();
        worker.join().unwrap();
    }

    #[test]
    #[should_panic(expected = "sync: negative WaitGroup counter")]
    fn negative_counter_panics() {
        ThreadSafeWaitGroup::new().done();
    }
}
