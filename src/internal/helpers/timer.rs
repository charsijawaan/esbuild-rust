// Port of upstream internal/helpers/timer.go.

use crate::internal::logger::{Log, MsgData, MsgId, MsgKind, Range};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Default)]
pub struct Timer {
    data: Mutex<Vec<TimerData>>,
}

#[derive(Clone, Debug)]
struct TimerData {
    time: Instant,
    name: String,
    is_end: bool,
}

impl Timer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// # Panics
    ///
    /// Panics if the private event mutex is poisoned.
    pub fn begin(&self, name: impl Into<String>) {
        self.data
            .lock()
            .expect("timer mutex was poisoned")
            .push(TimerData {
                name: name.into(),
                time: Instant::now(),
                is_end: false,
            });
    }

    /// # Panics
    ///
    /// Panics if the private event mutex is poisoned.
    pub fn end(&self, name: impl Into<String>) {
        self.data
            .lock()
            .expect("timer mutex was poisoned")
            .push(TimerData {
                name: name.into(),
                time: Instant::now(),
                is_end: true,
            });
    }

    #[must_use]
    pub fn fork(&self) -> Self {
        Self::new()
    }

    /// # Panics
    ///
    /// Panics if either timer's private event mutex is poisoned.
    pub fn join(&self, other: &Self) {
        if std::ptr::eq(self, other) {
            let mut data = self.data.lock().expect("timer mutex was poisoned");
            let duplicate = data.clone();
            data.extend(duplicate);
            return;
        }
        let other_data = other.data.lock().expect("timer mutex was poisoned").clone();
        self.data
            .lock()
            .expect("timer mutex was poisoned")
            .extend(other_data);
    }

    /// # Panics
    ///
    /// Panics if timing events are not properly nested, or if the private
    /// event mutex is poisoned.
    pub fn log(&self, log: &Log) {
        let data = self.data.lock().expect("timer mutex was poisoned");
        let mut notes: Vec<MsgData> = Vec::new();
        let mut stack: Vec<(TimerData, usize)> = Vec::new();
        let mut indent = 0;

        for item in data.iter() {
            if item.is_end {
                indent -= 1;
                let (top, note_index) = stack.pop().expect("timer end without matching begin");
                assert_eq!(item.name, top.name, "Internal error");
                notes[note_index].text = format!(
                    "{}{}: {}ms",
                    "  ".repeat(indent),
                    top.name,
                    item.time.duration_since(top.time).as_millis()
                );
            } else {
                let note_index = notes.len();
                notes.push(MsgData {
                    disable_maximum_width: true,
                    ..MsgData::default()
                });
                stack.push((item.clone(), note_index));
                indent += 1;
            }
        }
        assert!(stack.is_empty(), "timer begin without matching end");

        log.add_id_with_notes(
            MsgId::None,
            MsgKind::Info,
            None,
            Range::default(),
            "Timing information (times may not nest hierarchically due to parallelism)",
            notes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Timer;
    use crate::internal::logger::{DeferLogKind, Log, MsgKind};
    use std::collections::HashMap;

    #[test]
    fn emits_nested_timing_notes() {
        let timer = Timer::new();
        timer.begin("outer");
        timer.begin("inner");
        timer.end("inner");
        timer.end("outer");

        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        timer.log(&log);
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Info);
        assert!(messages[0].notes[0].text.starts_with("outer: "));
        assert!(messages[0].notes[1].text.starts_with("  inner: "));
        assert!(
            messages[0]
                .notes
                .iter()
                .all(|note| note.disable_maximum_width)
        );
    }

    #[test]
    fn joins_forked_timing_data() {
        let timer = Timer::new();
        let fork = timer.fork();
        fork.begin("parallel");
        fork.end("parallel");
        timer.join(&fork);

        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        timer.log(&log);
        assert_eq!(log.done()[0].notes.len(), 1);
    }
}
