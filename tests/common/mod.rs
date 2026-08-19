//! Shared helpers for the integration test binaries (`cli.rs`, `pty.rs`).
//! A `tests/common/mod.rs` (rather than `tests/common.rs`) is the standard
//! way to share code between `tests/` binaries without Cargo treating this
//! file as its own (empty) test target.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A unique path under the system temp dir, not created — callers that need
/// a real file or directory on disk do that themselves. `module` names the
/// calling test binary (`"cli"` or `"pty"`), so leftover files from a failed
/// run are still traceable to their origin; `hint` further disambiguates
/// within that binary's own tests. `ext` appends a `.{ext}` suffix (e.g.
/// `"md"`) when the path names a file.
pub fn unique_temp_path(module: &str, hint: &str, ext: Option<&str>) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = format!("markcheck-{module}-{hint}-{pid}-{n}");
    match ext {
        Some(ext) => std::env::temp_dir().join(format!("{name}.{ext}")),
        None => std::env::temp_dir().join(name),
    }
}
