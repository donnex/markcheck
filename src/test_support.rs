//! Shared `#[cfg(test)]`-only helpers, used across this crate's unit test
//! modules so each doesn't hand-roll its own unique-temp-path counter.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A unique path under the system temp dir, not created — callers that need
/// a real file or directory on disk do that themselves. `module` names the
/// calling source file (e.g. `"watcher"`), so leftover files from a failed
/// test run are still traceable to their origin; `hint` further
/// disambiguates within a module's own tests, or is empty when a module has
/// no need for one. `ext` appends a `.{ext}` suffix (e.g. `"md"`, `"toml"`)
/// when the path names a file.
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
