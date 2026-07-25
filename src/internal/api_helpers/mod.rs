//! Port of upstream `internal/api_helpers`.

use std::sync::atomic::AtomicBool;

// This is only checked by code that creates the root timer. Other code checks
// whether the timer itself is present, matching the upstream ownership rule.
pub static USE_TIMER: AtomicBool = AtomicBool::new(false);
