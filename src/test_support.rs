//! Shared `#[cfg(test)]`-only helpers, used across this crate's unit test
//! modules so each doesn't hand-roll its own unique-temp-path counter.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A unique, not-yet-created path under the temp dir. `module` names the
/// caller so leftovers are traceable; `ext` adds a suffix.
pub fn unique_temp_path(module: &str, hint: &str, ext: Option<&str>) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = if hint.is_empty() {
        format!("markcheck-{module}-test-{pid}-{n}")
    } else {
        format!("markcheck-{module}-test-{hint}-{pid}-{n}")
    };
    match ext {
        Some(ext) => std::env::temp_dir().join(format!("{name}.{ext}")),
        None => std::env::temp_dir().join(name),
    }
}
