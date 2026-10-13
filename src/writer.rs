use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::{Document, ItemKind, TaskState};

/// A per-call random `u64` widening temp-file names beyond the PID, drawn
/// from `RandomState`'s OS-seeded keys to avoid a `rand` dependency.
pub(crate) fn random_suffix() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish()
}

/// What `WriteLock::acquire` managed to do.
pub(crate) enum LockOutcome {
    /// Held until the returned guard drops.
    Acquired(WriteLock),
    /// Another markcheck instance is inside its own check-and-write. Carries
    /// the lock's path so the message can name it: recovery is manual in the
    /// one case this refuses forever (see `acquire`).
    Busy(PathBuf),
    /// The lock itself is unusable here (no writable temp directory, say).
    /// The caller proceeds **unlocked**: this is a safety net over an
    /// already-existing content check, not a gate that may deny writes.
    Unavailable,
}

/// An advisory lock held across `AppState::commit_write`'s hash check and
/// write, turning them into one compare-and-swap between instances.
///
/// Kept in the temp directory, not beside the checklist, so a `git add -A`
/// hook can't commit it; it therefore only serialises instances on one
/// machine. A stale lock is recovered only when its owner is provably gone,
/// never by age, so a slow live holder is never displaced; a recycled PID
/// blocks saving until the file is removed by hand.
pub(crate) struct WriteLock {
    path: PathBuf,
    /// Unique per guard, written into the lock file. Ownership is checked
    /// against it before this guard ever removes the file.
    token: String,
}

impl WriteLock {
    /// Acquires the lock for `target`. An existing lock is honoured unless
    /// its owner can be *shown* to be gone — see the type's doc comment for
    /// why elapsed time is no longer part of that decision.
    pub(crate) fn acquire(target: &Path) -> LockOutcome {
        let path = lock_path(target);
        match Self::try_create(&path) {
            Ok(lock) => LockOutcome::Acquired(lock),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if !owner_is_gone(&path) {
                    return LockOutcome::Busy(path);
                }
                // The owner is gone. Clear it and make exactly one attempt to
                // take over; a loser of that race sees `AlreadyExists` again
                // and reports `Busy`, which is the correct answer for it.
                let _ = fs::remove_file(&path);
                match Self::try_create(&path) {
                    // Read back before trusting it: another instance
                    // recovering the same dead owner may have removed this
                    // one and written its own between the create and now.
                    Ok(lock) if lock.still_ours() => LockOutcome::Acquired(lock),
                    Ok(_) => LockOutcome::Busy(path),
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                        LockOutcome::Busy(path)
                    }
                    Err(_) => LockOutcome::Unavailable,
                }
            }
            Err(_) => LockOutcome::Unavailable,
        }
    }

    fn try_create(path: &Path) -> io::Result<WriteLock> {
        use std::io::Write;
        let token = format!("markcheck {} {:x}", std::process::id(), random_suffix());
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        // The path is derivable by anyone, so keep it unreadable to others.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        Ok(WriteLock {
            path: path.to_path_buf(),
            token,
        })
    }

    /// Whether the lock file still holds this guard's own token.
    fn still_ours(&self) -> bool {
        fs::read_to_string(&self.path).is_ok_and(|held| held == self.token)
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        // Only ever remove a lock still holding this guard's token. If
        // another instance took the file over in the meantime, it is theirs
        // and deleting it would silently strip their mutual exclusion.
        if self.still_ours() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// How many directory entries point at `target`'s inode, or `None` when it
/// can't be told. Above 1, `write_back`'s rename will break the hard link,
/// which is warned about at startup rather than refused, as atomic-save
/// editors do the same.
#[cfg(unix)]
pub fn hard_link_count(target: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(target).ok().map(|meta| meta.nlink())
}

#[cfg(not(unix))]
pub fn hard_link_count(_target: &Path) -> Option<u64> {
    None
}

/// The UID owning `target` when it isn't the user running markcheck, who
/// can't keep it: `write_back`'s rename makes the file theirs. `None` when
/// the owner is kept (it already is the current user, or that user is
/// root) or either side can't be told. Warned about at startup, like a hard
/// link, since a teammate may then lose write access.
#[cfg(unix)]
pub fn foreign_owner(target: &Path) -> Option<u32> {
    foreign_owner_for(target, current_uid()?)
}

#[cfg(not(unix))]
pub fn foreign_owner(_target: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn foreign_owner_for(target: &Path, current_uid: u32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let owner = fs::metadata(target).ok()?.uid();
    (current_uid != 0 && owner != current_uid).then_some(owner)
}

/// The UID a file created by this process gets — exactly what a save's
/// temp file will be owned by — read off a throwaway probe file, as std
/// exposes no `geteuid`.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let probe = std::env::temp_dir().join(format!(
        "markcheck-uid-probe-{}-{:x}",
        std::process::id(),
        random_suffix()
    ));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .ok()?;
    let uid = file.metadata().ok().map(|meta| meta.uid());
    let _ = fs::remove_file(&probe);
    uid
}

/// One lock file per checklist, named by the digest of its canonical path so
/// every spelling of one file (symlink, `..`) shares a lock. Falls back to
/// the given path when canonicalization fails; `write_back` then refuses
/// the missing file.
fn lock_path(target: &Path) -> PathBuf {
    let resolved = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let digest = crate::model::hash_bytes(resolved.as_os_str().as_encoded_bytes());
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    std::env::temp_dir().join(format!("markcheck-write-lock-{name}"))
}

/// Whether the lock's owner is demonstrably gone; anything short of proof
/// is `false`. Never `kill -0`, which can't tell a dead process from
/// another user's.
fn owner_is_gone(path: &Path) -> bool {
    let Some(pid) = fs::read_to_string(path)
        .ok()
        .and_then(|token| token.split_whitespace().nth(1).map(str::to_string))
    else {
        return false;
    };
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if let Some(gone) = proc_reports_gone(&pid) {
        return gone;
    }
    ps_reports_gone(Command::new("ps"), &pid)
}

/// Linux answers from `/proc`, with no external `ps` to be missing or to
/// lack `-p` (BusyBox). `None` when `/proc` isn't mounted, so nothing was
/// shown either way.
#[cfg(target_os = "linux")]
fn proc_reports_gone(pid: &str) -> Option<bool> {
    let proc = Path::new("/proc");
    proc.join("self").exists().then(|| !proc.join(pid).exists())
}

#[cfg(not(target_os = "linux"))]
fn proc_reports_gone(_pid: &str) -> Option<bool> {
    None
}

/// Runs `ps` (given as a `Command` so a test can stand one in) with
/// `-p <pid> -o pid=`. Gone only on positive evidence: an empty listing,
/// nothing on stderr, and an exit of 0 or 1. A usage error, any other
/// stderr output or exit code, or a `ps` that can't run proves nothing, so
/// the lock is kept.
fn ps_reports_gone(mut ps: Command, pid: &str) -> bool {
    let Ok(output) = ps
        .args(["-p", pid, "-o", "pid="])
        .stdin(Stdio::null())
        .output()
    else {
        return false;
    };
    matches!(output.status.code(), Some(0 | 1))
        && output.stderr.is_empty()
        && output.stdout.iter().all(u8::is_ascii_whitespace)
}

/// Replaces the line's task checkbox (`[ ]`, `[x]`, `[X]`, `[/]`) with
/// `target`. The leftmost marker is the checkbox, so a marker token in the
/// body is never the one rewritten. A checkbox already in the target state
/// is left as written, so an untouched `[X]` keeps its casing.
fn set_task_marker(line: &mut String, target: &str) {
    let leftmost = ["[ ]", "[x]", "[X]", "[/]"]
        .into_iter()
        .filter_map(|marker| line.find(marker).map(|pos| (pos, marker)))
        .min_by_key(|&(pos, _)| pos);
    if let Some((pos, marker)) = leftmost
        && !marker.eq_ignore_ascii_case(target)
    {
        line.replace_range(pos..pos + marker.len(), target);
    }
}

/// Rejoins `lines` into the file's exact bytes. Each line carries its own
/// terminator (see `Document::raw_lines`), so this is a plain concatenation
/// — every line ending, and the presence or absence of a final one, comes
/// straight from the source rather than from a whole-file assumption.
fn join_lines(lines: &[String]) -> String {
    lines.concat()
}

/// The document's content as it stands in memory.
pub fn document_contents(document: &Document) -> String {
    join_lines(&document.raw_lines)
}

/// True when the file's own permission bits deny all writing (`chmod a-w`),
/// which `write_back` honors by refusing to save. False when it can't be read.
pub fn is_read_only(target: &Path) -> bool {
    fs::metadata(target).is_ok_and(|metadata| metadata.permissions().readonly())
}

/// Writes to a temp file in the same directory, fsyncs it, and renames it
/// over the target, so a crash can't leave the checklist truncated; the
/// parent directory is fsynced too on unix. Permission bits (but never
/// setuid, setgid or sticky) and, where the user may set it, the group are
/// kept; hard links and another user's ownership
/// are not. A read-only file is refused untouched. Returns the content
/// written.
pub fn write_back(document: &Document) -> io::Result<String> {
    let mut lines = document.raw_lines.clone();
    for list in &document.lists {
        for item in &list.items {
            if let ItemKind::Checkbox(state) = item.kind {
                let target = match state {
                    TaskState::NotStarted => "[ ]",
                    TaskState::Started => "[/]",
                    TaskState::Done => "[x]",
                };
                set_task_marker(&mut lines[item.line_number - 1], target);
            }
        }
    }
    let contents = join_lines(&lines);

    // Read the source metadata before touching anything. Any error —
    // including NotFound — aborts before writing, so a deleted file is never
    // silently recreated. A 0600 file can also never be widened.
    let source = fs::metadata(&document.file_path)?;
    // The rename only needs a writable directory, so without this a file the
    // user write-protected would be replaced anyway.
    if source.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file is read-only",
        ));
    }

    let file_name = document
        .file_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file path has no file name"))?
        .to_string_lossy();
    let temp_path_base = document.file_path.with_file_name(format!(
        ".{file_name}.markcheck-tmp-{}-{:x}",
        std::process::id(),
        random_suffix()
    ));

    // `write_temp` cleans up after its own failures.
    let temp_path = write_temp(&temp_path_base, &contents, &source)?;
    if let Err(err) = fs::rename(&temp_path, &document.file_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    #[cfg(unix)]
    sync_parent_dir(&document.file_path);
    Ok(contents)
}

/// Best-effort fsync of `path`'s parent directory so the rename survives a
/// crash. Errors are ignored: the rename already succeeded.
#[cfg(unix)]
pub(crate) fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Creates the temp file 0600, writes the contents, gives it the source's
/// owner and group as far as this user may, applies the source's ordinary
/// permission bits (never setuid, setgid or sticky) through the handle
/// (immune to umask) and fsyncs. If the name
/// is taken it retries once at a fresh name, never deleting the existing
/// file, which may belong to a live writer. Returns the path written.
#[cfg(unix)]
fn write_temp(base_temp_path: &Path, contents: &str, source: &fs::Metadata) -> io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, fchown};

    let open = |path: &Path| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    let (path, mut file) = match open(base_temp_path) {
        Ok(file) => (base_temp_path.to_path_buf(), file),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let mut retry_path = base_temp_path.as_os_str().to_os_string();
            retry_path.push(format!("-{:x}", random_suffix()));
            let retry_path = PathBuf::from(retry_path);
            let file = open(&retry_path)?;
            (retry_path, file)
        }
        Err(err) => return Err(err),
    };

    let write_result = (|| {
        file.write_all(contents.as_bytes())?;
        // Best effort: keeping the UID needs root, but any group the
        // user belongs to may be set, which keeps a shared group
        // checklist writable by the group rather than handing it to the
        // saver's primary group.
        if fchown(&file, Some(source.uid()), Some(source.gid())).is_err() {
            let _ = fchown(&file, None, Some(source.gid()));
        }
        // Only the ordinary bits: the new file belongs to the saver, so
        // copying setuid/setgid/sticky would bypass the kernel clearing
        // them when someone other than the owner writes the file.
        file.set_permissions(fs::Permissions::from_mode(source.mode() & 0o777))?;
        file.sync_all()
    })();
    match write_result {
        Ok(()) => Ok(path),
        Err(err) => {
            let _ = fs::remove_file(&path);
            Err(err)
        }
    }
}

#[cfg(not(unix))]
fn write_temp(temp_path: &Path, contents: &str, source: &fs::Metadata) -> io::Result<PathBuf> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp_path)?;
    file.write_all(contents.as_bytes())?;
    file.set_permissions(source.permissions())?;
    file.sync_all()?;
    Ok(temp_path.to_path_buf())
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
