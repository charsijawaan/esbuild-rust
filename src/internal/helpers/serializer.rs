// Port of upstream internal/helpers/serializer.go.

use std::sync::{Condvar, Mutex};

/// Ensures `enter(index)` does not return before `leave(index - 1)`.
#[derive(Debug)]
pub struct Serializer {
    flags: Vec<Flag>,
}

#[derive(Debug, Default)]
struct Flag {
    done: Mutex<bool>,
    changed: Condvar,
}

impl Serializer {
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            flags: (0..count).map(|_| Flag::default()).collect(),
        }
    }

    /// # Panics
    ///
    /// Panics if `index` is greater than the count passed to [`Self::new`] or
    /// if a synchronization primitive is poisoned.
    pub fn enter(&self, index: usize) {
        if index > 0 {
            let flag = &self.flags[index - 1];
            let mut done = flag.done.lock().expect("serializer mutex was poisoned");
            while !*done {
                done = flag
                    .changed
                    .wait(done)
                    .expect("serializer mutex was poisoned");
            }
        }
    }

    /// # Panics
    ///
    /// Panics if `index` is outside the count passed to [`Self::new`], if this
    /// index was already left, or if a synchronization primitive is poisoned.
    pub fn leave(&self, index: usize) {
        let flag = &self.flags[index];
        let mut done = flag.done.lock().expect("serializer mutex was poisoned");
        assert!(!*done, "serializer index was left twice");
        *done = true;
        flag.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::Serializer;
    use std::sync::{Arc, Mutex};

    #[test]
    fn serializes_parallel_work_by_index() {
        let serializer = Arc::new(Serializer::new(3));
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut threads = Vec::new();
        for index in (0..3).rev() {
            let serializer = Arc::clone(&serializer);
            let order = Arc::clone(&order);
            threads.push(std::thread::spawn(move || {
                serializer.enter(index);
                order.lock().unwrap().push(index);
                serializer.leave(index);
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), [0, 1, 2]);
    }
}
