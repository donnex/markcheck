use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::{Document, ItemKind, TaskState};

/// A per-call random `u64`, used to widen a temp file's name beyond just its
/// PID so two processes racing to create a same-named temp file (or a
/// crashed run whose PID gets reused) is vanishingly unlikely rather than
/// merely unlikely. `RandomState`'s keys are freshly drawn from the OS RNG
/// on each `new()` (the same mechanism `HashMap`'s default hasher uses for
/// DoS resistance), so hashing nothing and reading `finish()` is a
/// dependency-free way to get process-random entropy without pulling in a
/// dedicated `rand` crate for one temp-file suffix. `pub(crate)`: also used
/// by `scaffold::create_new_checklist`'s equivalent temp file.
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

/// An advisory lock covering a checklist's read-check-then-write sequence.
///
/// `AppState::commit_write` verifies the file on disk still hashes to what
/// was last seen and *then* replaces it. Those are two operations, so two
/// markcheck instances could both pass the check and both rename, and the
/// second silently discards the first's toggle. The content hash narrows
/// that window a great deal but cannot close it, because it is not a
/// compare-and-swap. Holding this across both makes it one.
///
/// **Deliberately in the temp directory, not beside the checklist.** A
/// sibling lock file lands inside the user's repository, where a
/// `git add -A` pre-commit hook can sweep it into a commit — which
/// `verify_commit_scope` would then correctly abort the sync over. Keying a
/// temp-dir path by the digest of the canonical checklist path keeps it
/// entirely out of git's way. The trade is that it only serialises
/// instances that share a temp directory: same machine, same user. Two
/// machines writing one file over a network share are not covered, and were
/// not covered before either.
///
/// **Recovery is by proven death, never by elapsed time.** External review
/// of `d885c88`: the previous version treated a lock older than 30 seconds
/// as abandoned, which meant a *live* holder could be displaced simply for
/// being slow — a stalled `sync_all`, a FUSE or network filesystem, storage
/// under recovery. Both instances then entered the critical section and the
/// second write silently won, which is precisely the loss this lock exists
/// to prevent; the unique token stopped one from deleting the other's file
/// but never stopped both from writing. Age is no longer consulted at all.
/// A lock is only recovered when its recorded owner is *demonstrably* gone,
/// and anything else — owner alive, owner unknowable, file unreadable —
/// refuses. That fails closed in the one direction that matters.
///
/// The cost is the honest one: if the owning process died and its PID was
/// recycled by an unrelated live process, the lock is honoured forever and
/// saving stays blocked. The refusal names the file so it can be removed by
/// hand. That is strictly better than the alternative it replaces, where the
/// same uncertainty silently permitted two writers.
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
        // The lock lives in a shared temp directory at a path anyone can
        // derive (it is the digest of the checklist's path), so it is
        // readable by every account on the machine unless narrowed. Nothing
        // secret is in it — a PID and a random token — but there is no
        // reason for it to be legible either, and the two files this module
        // creates that *do* hold user content are already 0600.
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

/// How many directory entries point at `target`'s inode, when that can be
/// determined. `Some(1)` is the ordinary case; anything greater means the
/// checklist is hard-linked and `write_back`'s rename will break the link.
///
/// Reported rather than enforced. The atomic temp-then-rename in
/// `write_back` gives the path a **new inode**, so a hard-linked alias stops
/// tracking the checklist the moment anything is toggled — verified: two
/// links to one inode become two independent files with different content,
/// and neither the content fingerprint nor the write lock notices, since
/// both reason about paths while this is about inodes. External review of
/// `5c51d81` raised it.
///
/// Refusing the write was the reviewer's preference and is not what this
/// does, deliberately. Every atomic-save editor breaks hard links the same
/// way — vim with `backupcopy=no`, emacs, VS Code — so refusing would leave
/// markcheck unable to open a file the user edits elsewhere without trouble,
/// and with no flag to override it the checklist would simply be unusable.
/// No data is lost either: both aliases keep valid, readable content, they
/// just stop being the same file. The defect the review names is that this
/// happens *silently*, and a warning at startup — before anything is
/// written, while quitting is still free — removes the silence without
/// removing the capability.
///
/// `None` on a platform or filesystem that cannot answer, which is treated
/// as "nothing to warn about" rather than guessed at.
#[cfg(unix)]
pub fn hard_link_count(target: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(target).ok().map(|meta| meta.nlink())
}

#[cfg(not(unix))]
pub fn hard_link_count(_target: &Path) -> Option<u64> {
    None
}

/// Where `target`'s lock lives: one file per checklist, named by the digest
/// of its path so two different checklists never share a lock and the name
/// can't collide with anything the user owns.
///
/// **The path is canonicalized first, so every spelling of one file maps to
/// one lock.** Mutual exclusion keyed on an unresolved path is exclusion in
/// name only: `/checklists/server.md` and `/current/server.md` (a symlink to
/// it), or a path carrying `..`, hash differently and would each get their
/// own lock while writing the same bytes — two writers inside the section
/// that exists to hold one. External review of `5c51d81` raised exactly
/// this.
///
/// `main.rs` already canonicalizes at startup, which is why that scenario
/// does not reproduce against the real binary today, and this is defence in
/// depth rather than the primary mechanism. It is worth having anyway: an
/// invariant maintained in a different module is one refactor away from
/// being silently dropped, and a lock is the wrong place to discover that.
/// Resolving it here makes the guarantee local to the code that depends on
/// it.
///
/// Falling back to the given path when canonicalization fails is safe rather
/// than lax: it fails when the target does not exist, and `write_back` reads
/// the file's metadata before writing anything, so a vanished checklist is
/// refused there instead.
fn lock_path(target: &Path) -> PathBuf {
    let resolved = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let digest = crate::model::hash_bytes(resolved.as_os_str().as_encoded_bytes());
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    std::env::temp_dir().join(format!("markcheck-write-lock-{name}"))
}

/// Whether the process that wrote the lock at `path` is **demonstrably** no
/// longer running. Anything short of proof — an unreadable or unparseable
/// lock, a liveness check that cannot be run — answers `false`, so the lock
/// is honoured. Only a definite "that process does not exist" recovers it.
///
/// `ps -p <pid>` rather than `kill -0`: `kill` cannot distinguish "no such
/// process" from "not permitted to signal it", so a lock held by another
/// user's markcheck would look dead and be stolen. `ps` answers regardless
/// of ownership, and exists on both platforms this project supports.
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
    match Command::new("ps")
        .args(["-p", &pid, "-o", "pid="])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        // A clean non-zero exit is `ps` reporting no such process.
        Ok(status) => !status.success(),
        // `ps` could not be run, so nothing was demonstrated.
        Err(_) => false,
    }
}

/// Replaces the task checkbox on the line with `target`, leaving everything
/// else untouched. Recognizes `[ ]`, `[x]`, `[X]` (pulldown parses uppercase
/// `[X]` as done, so we must be able to rewrite it), and `[/]`; rewriting
/// normalizes the marker to `target`'s casing.
///
/// The checkbox is the *leftmost* marker on the line — it sits right after the
/// list bullet, before any body text. We must replace it by position, not the
/// first marker *type* in some fixed order: a body containing a literal marker
/// token (e.g. ``press the `[ ]` button``) would otherwise get its body token
/// rewritten while the real checkbox is left untouched, silently desyncing the
/// on-disk state from the app and corrupting the body.
fn set_task_marker(line: &mut String, target: &str) {
    let leftmost = ["[ ]", "[x]", "[X]", "[/]"]
        .into_iter()
        .filter_map(|marker| line.find(marker).map(|pos| (pos, marker)))
        .min_by_key(|&(pos, _)| pos);
    if let Some((pos, marker)) = leftmost {
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

/// The document's current content exactly as it stands in memory — no
/// checkbox mutation. Used to capture the expected git-sync snapshot for a
/// change that already landed on disk outside `write_back` (e.g. an external
/// editor edit picked up by a reload).
pub fn document_contents(document: &Document) -> String {
    join_lines(&document.raw_lines)
}

/// Writes to a temp file in the same directory, fsyncs it, and renames it
/// over the target so a crash or full disk mid-write can't leave the
/// checklist truncated or corrupted; the original content stays intact
/// until the rename (atomic on the same filesystem) succeeds. The temp
/// file's data is fsynced before the rename (`write_temp`), and on unix its
/// parent directory is fsynced (best-effort) after the rename, so the
/// rename itself survives a crash — a bare temp-then-rename without fsync
/// can still lose the write on a crash/power-loss at the wrong moment on
/// many filesystems (ext4 `data=ordered` included). The original file's
/// permissions are applied to the temp file before the rename so they
/// survive the swap. Hard links and ownership are not preserved — the
/// rename gives the path a new inode, the same trade-off atomic-save
/// editors make. Returns the exact content that was written, so callers
/// (git-sync) can capture it as the expected snapshot for a later commit.
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

    // Read the source permissions before touching anything. Any error —
    // including NotFound — aborts before writing, so a deleted file is never
    // silently recreated. A 0600 file can also never be widened.
    let source_perms = {
        let meta = fs::metadata(&document.file_path)?;
        Some(meta.permissions())
    };

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

    // `write_temp` returns the *actual* path it wrote to, which can differ
    // from `temp_path_base` if that name was already taken (see its own
    // doc comment) — cleans up after itself on any failure past file
    // creation, so there's no separate `fs::remove_file` needed here on
    // that path, only for the rename step below.
    let temp_path = write_temp(&temp_path_base, &contents, source_perms)?;
    if let Err(err) = fs::rename(&temp_path, &document.file_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    #[cfg(unix)]
    sync_parent_dir(&document.file_path);
    Ok(contents)
}

/// Best-effort fsync of `path`'s parent directory so the rename's directory
/// entry itself survives a crash, not just the file's data (which
/// `write_temp` already fsyncs before the rename). Errors are ignored: by
/// this point the rename has already succeeded and the content is safely on
/// disk under normal operation, and some filesystems don't support or need
/// directory fsync at all — this is a best-effort strengthening of the
/// crash-safety guarantee, not a correctness requirement. `pub(crate)`: also
/// used by `scaffold::create_new_checklist`, whose hard-link-into-place has
/// the identical need to make a fresh directory entry durable.
#[cfg(unix)]
pub(crate) fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Creates the temp file at `base_temp_path` (or, if that name is already
/// taken, at a freshly-suffixed retry path — see below) with mode 0600
/// (never more permissive than the most restrictive plausible source, even
/// for an instant), writes the contents, applies the exact source
/// permissions through the file handle — fchmod semantics, immune to the
/// process umask — then fsyncs the data to disk before returning the
/// *actual* path written to, so the caller's rename always targets a
/// flushed temp file at the right name regardless of which path was used.
///
/// The PID+random-suffix name makes a genuine collision with another live
/// process astronomically unlikely; a stale temp left behind by a crashed
/// prior run reusing the same PID is the only realistic cause. Retried
/// once at a freshly-suffixed path — never by deleting the existing file
/// and reusing its name: a *genuine* collision with another live,
/// legitimate writer using this exact name would otherwise have its temp
/// file deleted out from under it. The crashed-run scenario is inherently
/// racy to simulate deterministically, so this path is exercised by
/// inspection rather than a dedicated regression test.
#[cfg(unix)]
fn write_temp(
    base_temp_path: &Path,
    contents: &str,
    perms: Option<fs::Permissions>,
) -> io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

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
        if let Some(perms) = perms {
            file.set_permissions(perms)?;
        }
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
fn write_temp(
    temp_path: &Path,
    contents: &str,
    perms: Option<fs::Permissions>,
) -> io::Result<PathBuf> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp_path)?;
    file.write_all(contents.as_bytes())?;
    if let Some(perms) = perms {
        file.set_permissions(perms)?;
    }
    file.sync_all()?;
    Ok(temp_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_document;
    use std::path::PathBuf;

    const EXAMPLE: &str = "\
## Prepare workspace

- **Steps for the first workspace**
- [ ] `refresh-cache`
- [ ] `refresh-cache`
- [ ] `restart-service`

## Second workspace

- [ ] `refresh-cache`
";

    fn write_temp_file(contents: &str) -> PathBuf {
        let path = crate::test_support::unique_temp_path("writer", "", Some("md"));
        fs::write(&path, contents).unwrap();
        path
    }

    // --- Advisory write lock ---

    /// Writes a lock file by hand claiming to be held by `pid`.
    fn plant_lock(target: &Path, pid: u32) -> PathBuf {
        let path = lock_path(target);
        fs::write(&path, format!("markcheck {pid} deadbeef")).unwrap();
        path
    }

    /// A PID that has certainly exited: spawned, waited for, and reaped.
    fn a_dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// The premise behind the startup hard-link warning, pinned so it cannot
    /// quietly stop being true: an ordinary checklist has one link, a
    /// hard-linked one reports more, and the atomic write really does break
    /// the alias rather than updating both names.
    #[test]
    fn a_hard_linked_checklist_is_detected_and_the_write_breaks_the_alias() {
        let target = write_temp_file(EXAMPLE);
        assert_eq!(
            hard_link_count(&target),
            Some(1),
            "an ordinary checklist has exactly one name"
        );

        let alias = target.with_extension("alias.md");
        fs::hard_link(&target, &alias).unwrap();
        assert_eq!(
            hard_link_count(&target),
            Some(2),
            "the second name must be visible before anything is written"
        );

        // The divergence itself: write through one name, and the other is
        // left holding the old content. This is what the warning is for.
        let mut document = parse_document(target.clone()).unwrap();
        document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();
        assert!(
            fs::read_to_string(&target).unwrap().contains("[x]"),
            "the write must land on the path that was written"
        );
        assert_eq!(
            fs::read_to_string(&alias).unwrap(),
            EXAMPLE,
            "the alias keeps the old content — it is now a separate file"
        );
        assert_eq!(
            hard_link_count(&target),
            Some(1),
            "and the link count drops, because the rename made a new inode"
        );

        fs::remove_file(&alias).ok();
        fs::remove_file(&target).ok();
    }

    /// External review of `5c51d81`: mutual exclusion keyed on an unresolved
    /// path excludes nothing when two callers spell the same file
    /// differently. Each spelling here must map to the *same* lock file.
    #[test]
    fn every_spelling_of_one_checklist_maps_to_the_same_lock() {
        let target = write_temp_file(EXAMPLE);
        let dir = target.parent().unwrap();
        let name = target.file_name().unwrap();
        let expected = lock_path(&target);

        // A `.` component.
        assert_eq!(
            lock_path(&dir.join(".").join(name)),
            expected,
            "a `.` component must not create a second lock"
        );

        // A `..` component that walks out of the directory and back in.
        let round_trip = dir.join("sub").join("..").join(name);
        fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(
            lock_path(&round_trip),
            expected,
            "a `..` component must not create a second lock"
        );

        // A symlink pointing at the checklist — the case the review called
        // out as most important, and the one canonicalization is really for.
        let link = dir.join(format!(
            "link-to-{}",
            crate::test_support::unique_temp_path("l", "", None)
                .file_name()
                .unwrap()
                .to_string_lossy()
        ));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            lock_path(&link),
            expected,
            "a symlink to the checklist must share its lock"
        );

        // Sanity: the guard only means something if distinct files still get
        // distinct locks. Without this the test passes for a `lock_path`
        // that returns one constant.
        let other = write_temp_file(EXAMPLE);
        assert_ne!(
            lock_path(&other),
            expected,
            "two genuinely different checklists must not share a lock"
        );

        fs::remove_file(&link).ok();
        fs::remove_dir(dir.join("sub")).ok();
        fs::remove_file(&target).ok();
        fs::remove_file(&other).ok();
    }

    /// The end the review actually cares about: not that the names match,
    /// but that a second writer reaching the file by another spelling is
    /// genuinely kept out of the critical section.
    #[test]
    fn a_writer_arriving_through_a_symlink_is_locked_out_by_the_first() {
        let target = write_temp_file(EXAMPLE);
        let dir = target.parent().unwrap();
        let link = dir.join(format!(
            "aliased-{}.md",
            std::process::id() as u64 + random_suffix()
        ));
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let held = WriteLock::acquire(&target);
        assert!(matches!(held, LockOutcome::Acquired(_)));
        assert!(
            matches!(WriteLock::acquire(&link), LockOutcome::Busy(_)),
            "the same file through a symlink must be refused, not handed a second lock"
        );

        drop(held);
        assert!(
            matches!(WriteLock::acquire(&link), LockOutcome::Acquired(_)),
            "and it must become available once the first writer is done"
        );

        fs::remove_file(&link).ok();
        fs::remove_file(&target).ok();
    }

    /// The lock sits in a shared temp directory at a path any account on the
    /// machine can derive, so it should not be readable by all of them.
    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let target = write_temp_file(EXAMPLE);
        let held = WriteLock::acquire(&target);
        let LockOutcome::Acquired(lock) = &held else {
            panic!("test setup: the lock must be acquired, got something else");
        };

        let mode = fs::metadata(&lock.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "lock file mode should be 0600, was {mode:o}");

        drop(held);
        fs::remove_file(&target).ok();
    }

    #[test]
    fn a_second_acquire_is_busy_until_the_first_guard_drops() {
        let path = write_temp_file(EXAMPLE);

        let first = WriteLock::acquire(&path);
        assert!(matches!(first, LockOutcome::Acquired(_)));
        assert!(
            matches!(WriteLock::acquire(&path), LockOutcome::Busy(_)),
            "a second instance must not get in while the first holds it"
        );

        drop(first);
        assert!(
            matches!(WriteLock::acquire(&path), LockOutcome::Acquired(_)),
            "releasing the guard must free the lock"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn a_live_owner_is_never_displaced_however_long_it_holds_the_lock() {
        // External review of `d885c88`. Recovery used to be a pure age test,
        // so a *live* holder could be displaced simply for being slow — a
        // stalled `sync_all`, a FUSE or network filesystem, storage under
        // recovery. Both instances then entered the critical section and the
        // second write silently won, which is exactly the loss this lock
        // exists to prevent. The unique token stopped one from deleting the
        // other's file; it never stopped both from writing.
        //
        // Our own PID is the one process guaranteed to be alive here. No
        // sleeping is needed to make the point any more, which is itself the
        // fix: elapsed time is no longer part of the decision.
        let path = write_temp_file(EXAMPLE);
        let lock_file = plant_lock(&path, std::process::id());

        assert!(
            matches!(WriteLock::acquire(&path), LockOutcome::Busy(_)),
            "a lock whose owner is alive must be honoured regardless of its age"
        );
        assert!(lock_file.exists(), "and must not have been taken over");

        let _ = fs::remove_file(&lock_file);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn a_lock_whose_owner_has_died_is_recovered() {
        // The other half: a crash while holding the lock must not wedge every
        // future write. Recovery is immediate once the owner is *shown* to be
        // gone, rather than waiting out a timeout.
        let path = write_temp_file(EXAMPLE);
        plant_lock(&path, a_dead_pid());

        assert!(
            matches!(WriteLock::acquire(&path), LockOutcome::Acquired(_)),
            "a lock left behind by a dead process must be recoverable"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn an_unreadable_owner_is_treated_as_alive() {
        // Fails closed: anything short of proof that the owner is gone
        // honours the lock. Silently permitting a second writer on an
        // unparseable lock is the failure this whole change is about.
        let path = write_temp_file(EXAMPLE);
        let lock_file = lock_path(&path);
        fs::write(&lock_file, "not a token at all").unwrap();

        assert!(
            matches!(WriteLock::acquire(&path), LockOutcome::Busy(_)),
            "an unparseable lock must be honoured, not stolen"
        );

        let _ = fs::remove_file(&lock_file);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn the_lock_file_is_removed_when_the_guard_drops() {
        let path = write_temp_file(EXAMPLE);
        let lock_file = lock_path(&path);

        {
            let _held = WriteLock::acquire(&path);
            assert!(lock_file.exists(), "the lock file exists while held");
        }
        assert!(!lock_file.exists(), "and is cleaned up on drop");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn dropping_a_guard_never_removes_a_lock_someone_else_now_holds() {
        // Recovery removes then re-creates, which is not atomic, so a guard
        // must never delete a file it no longer owns.
        let path = write_temp_file(EXAMPLE);
        let lock_file = lock_path(&path);

        let mine = WriteLock::acquire(&path);
        assert!(matches!(mine, LockOutcome::Acquired(_)));

        // Another instance takes over, as the recovery path would.
        fs::remove_file(&lock_file).unwrap();
        fs::write(&lock_file, "markcheck 999999 cafe").unwrap();

        drop(mine);

        assert!(
            lock_file.exists(),
            "dropping a guard must not delete a lock another instance now holds"
        );
        assert_eq!(
            fs::read_to_string(&lock_file).unwrap(),
            "markcheck 999999 cafe",
            "and certainly must not replace its contents"
        );

        let _ = fs::remove_file(&lock_file);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn two_checklists_do_not_share_a_lock() {
        let a = write_temp_file(EXAMPLE);
        let b = write_temp_file(EXAMPLE);

        let held_a = WriteLock::acquire(&a);
        assert!(matches!(held_a, LockOutcome::Acquired(_)));
        assert!(
            matches!(WriteLock::acquire(&b), LockOutcome::Acquired(_)),
            "a different checklist must not be blocked by this one"
        );

        fs::remove_file(&a).ok();
        fs::remove_file(&b).ok();
    }

    #[test]
    fn toggling_completed_item_updates_only_that_line() {
        let path = write_temp_file(EXAMPLE);
        let mut document = parse_document(path.clone()).unwrap();

        // Complete the third checkbox item ("restart-service"), leave others untouched.
        let item = document.lists[0]
            .items
            .iter_mut()
            .find(|i| i.display_text == "restart-service")
            .unwrap();
        item.kind = ItemKind::Checkbox(TaskState::Done);

        write_back(&document).unwrap();

        let reparsed = parse_document(path.clone()).unwrap();
        let restart_item = reparsed.lists[0]
            .items
            .iter()
            .find(|i| i.display_text == "restart-service")
            .unwrap();
        assert_eq!(restart_item.kind, ItemKind::Checkbox(TaskState::Done));

        // All other checkbox items remain unaffected.
        let other_completed = reparsed.lists[0]
            .items
            .iter()
            .filter(|i| i.display_text != "restart-service")
            .filter(|i| matches!(i.kind, ItemKind::Checkbox(TaskState::Done)))
            .count();
        assert_eq!(other_completed, 0);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn uppercase_x_marker_is_rewritten_on_toggle() {
        // pulldown parses `[X]` as done; the writer must be able to rewrite it,
        // else toggling it off would silently not persist.
        let mut line = "- [X] done".to_string();
        set_task_marker(&mut line, "[ ]");
        assert_eq!(line, "- [ ] done");

        // End-to-end: a `[X]` task toggled off round-trips to `[ ]`.
        let path = write_temp_file("## S\n\n- [X] done\n");
        let mut document = parse_document(path.clone()).unwrap();
        assert_eq!(
            document.lists[0].items[0].kind,
            ItemKind::Checkbox(TaskState::Done),
            "[X] parses as done"
        );
        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::NotStarted);
        write_back(&document).unwrap();
        let reparsed = parse_document(path.clone()).unwrap();
        assert_eq!(
            reparsed.lists[0].items[0].kind,
            ItemKind::Checkbox(TaskState::NotStarted),
            "toggling [X] off now persists"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn set_task_marker_targets_the_checkbox_not_a_body_token() {
        // Only the leading checkbox (the leftmost marker) is rewritten; a
        // literal marker token in the body must be left exactly as-is.
        let mut line = "- [x] press the `[ ]` button".to_string();
        set_task_marker(&mut line, "[/]");
        assert_eq!(line, "- [/] press the `[ ]` button");

        // The checkbox is still the leftmost marker even when it's `[ ]` and a
        // `[x]` appears in the body.
        let mut line = "- [ ] type `[x]` to confirm".to_string();
        set_task_marker(&mut line, "[x]");
        assert_eq!(line, "- [x] type `[x]` to confirm");

        // No marker at all → line untouched.
        let mut line = "- a plain bullet".to_string();
        set_task_marker(&mut line, "[x]");
        assert_eq!(line, "- a plain bullet");
    }

    #[test]
    fn write_back_leaves_inline_body_marker_intact() {
        // End-to-end: toggling a done task whose body carries a literal
        // `[ ]` token flips the checkbox and leaves the body token untouched.
        let path = write_temp_file("## S\n\n- [x] press the `[ ]` button\n");
        let mut document = parse_document(path.clone()).unwrap();
        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Started);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("- [/] press the `[ ]` button"),
            "checkbox toggled, body marker preserved; got: {written:?}"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn write_back_leaves_no_temp_file_behind() {
        let path = write_temp_file(EXAMPLE);
        let document = parse_document(path.clone()).unwrap();

        write_back(&document).unwrap();

        let dir = path.parent().unwrap();
        let file_name = path.file_name().unwrap().to_string_lossy();
        let leftover = fs::read_dir(dir).unwrap().any(|entry| {
            let name = entry.unwrap().file_name();
            name.to_string_lossy()
                .contains(&format!("{file_name}.markcheck-tmp-"))
        });
        assert!(!leftover, "temp file was not cleaned up after rename");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trip_preserves_untouched_content() {
        let path = write_temp_file(EXAMPLE);
        let document = parse_document(path.clone()).unwrap();

        write_back(&document).unwrap();
        let written = fs::read_to_string(&path).unwrap();

        assert_eq!(written.trim_end(), EXAMPLE.trim_end());

        fs::remove_file(&path).ok();
    }

    #[test]
    fn write_back_on_an_empty_document_leaves_the_file_empty() {
        // Not reachable through normal app use (there's nothing to toggle
        // with zero items), but write_back itself makes no assumption that
        // the document is non-empty — an empty source (no lists, no
        // raw_lines, no trailing newline) must round-trip to exactly the
        // same empty file, not gain a stray newline from an unconditional
        // join-and-push.
        let path = write_temp_file("");
        let document = parse_document(path.clone()).unwrap();
        assert!(document.lists.is_empty());
        assert!(document.raw_lines.is_empty());

        write_back(&document).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trip_does_not_add_a_missing_trailing_newline() {
        // A source file with no final newline must not gain one on toggle —
        // write_back's "only the checkbox changes" guarantee (documented in
        // README.md) otherwise silently breaks on the very first write.
        let no_trailing_newline = "## S\n\n- [ ] `task`";
        let path = write_temp_file(no_trailing_newline);
        let mut document = parse_document(path.clone()).unwrap();
        assert!(
            !document.raw_lines.last().unwrap().ends_with('\n'),
            "source has no final newline"
        );

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written, "## S\n\n- [x] `task`");
        assert!(!written.ends_with('\n'), "must not gain a trailing newline");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trip_preserves_an_existing_trailing_newline() {
        let with_trailing_newline = "## S\n\n- [ ] `task`\n";
        let path = write_temp_file(with_trailing_newline);
        let mut document = parse_document(path.clone()).unwrap();
        assert!(
            document.raw_lines.last().unwrap().ends_with('\n'),
            "source has a final newline"
        );

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written, "## S\n\n- [x] `task`\n");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn a_toggle_changes_one_marker_and_no_other_byte_for_any_line_ending() {
        // Deep review, round 2. The old whole-file `uses_crlf` flag was only
        // ever right for an all-LF or all-CRLF file:
        //
        //   * CR-only: `str::lines()` doesn't split on a lone `\r`, so every
        //     item landed on `raw_lines[0]` and the per-item writes clobbered
        //     each other -- the toggle was silently discarded entirely.
        //   * mixed: one CRLF anywhere rewrote *every* line to CRLF, so a
        //     one-marker change produced a whole-file diff (and, under
        //     git-sync, an automatically pushed whole-file commit).
        //
        // Each line now carries its own terminator, so the guarantee README
        // states -- only the checkbox changes -- holds for all four.
        for (name, source, expected) in [
            (
                "LF",
                "## S\n\n- [ ] alpha\n- [ ] beta\n",
                "## S\n\n- [x] alpha\n- [ ] beta\n",
            ),
            (
                "CRLF",
                "## S\r\n\r\n- [ ] alpha\r\n- [ ] beta\r\n",
                "## S\r\n\r\n- [x] alpha\r\n- [ ] beta\r\n",
            ),
            (
                "CR-only",
                "## S\r\r- [ ] alpha\r- [ ] beta\r",
                "## S\r\r- [x] alpha\r- [ ] beta\r",
            ),
            (
                "mixed",
                "## S\r\n\r\n- [ ] alpha\n- [ ] beta\r\n",
                "## S\r\n\r\n- [x] alpha\n- [ ] beta\r\n",
            ),
            (
                "no final newline",
                "## S\n\n- [ ] alpha\n- [ ] beta",
                "## S\n\n- [x] alpha\n- [ ] beta",
            ),
        ] {
            let path = write_temp_file(source);
            let mut document = parse_document(path.clone()).unwrap();
            let alpha = document
                .lists
                .iter_mut()
                .flat_map(|list| list.items.iter_mut())
                .find(|i| i.display_text == "alpha")
                .unwrap_or_else(|| panic!("{name}: alpha must parse as an item"));
            alpha.kind = ItemKind::Checkbox(TaskState::Done);

            write_back(&document).unwrap();

            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                expected,
                "{name}: exactly one marker may change, every other byte \
                 (terminators included) must be identical"
            );
            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn round_trip_preserves_crlf_line_endings() {
        // A file authored with CRLF line endings must keep them after a
        // toggle — write_back must not silently flip the whole file to LF
        // just because one marker byte changed.
        let crlf_example = EXAMPLE.replace('\n', "\r\n");
        let path = write_temp_file(&crlf_example);
        let mut document = parse_document(path.clone()).unwrap();
        assert!(
            document.raw_lines.iter().all(|l| l.ends_with("\r\n")),
            "CRLF source must keep every CRLF ending"
        );

        let item = document
            .lists
            .iter_mut()
            .flat_map(|list| list.items.iter_mut())
            .find(|i| i.display_text == "refresh-cache")
            .unwrap();
        item.kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(
            written.matches('\n').count(),
            written.matches("\r\n").count(),
            "every line ending must be \\r\\n, not bare \\n; got: {written:?}"
        );
        assert!(
            written.contains("- [x] `refresh-cache`\r\n"),
            "toggled line keeps its CRLF ending; got: {written:?}"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn toggling_second_duplicate_item_updates_correct_line() {
        let path = write_temp_file(EXAMPLE);
        let mut document = parse_document(path.clone()).unwrap();

        // There are two "refresh-cache" items in list 0; complete only the second.
        let target_line = document.lists[0]
            .items
            .iter()
            .filter(|i| i.display_text == "refresh-cache")
            .nth(1)
            .unwrap()
            .line_number;

        for item in document.lists[0].items.iter_mut() {
            if item.line_number == target_line {
                item.kind = ItemKind::Checkbox(TaskState::Done);
            }
        }

        write_back(&document).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        let written_lines: Vec<&str> = written.lines().collect();

        assert!(written_lines[target_line - 1].contains("[x]"));

        let first_duplicate_line = document.lists[0]
            .items
            .iter()
            .find(|i| i.display_text == "refresh-cache")
            .unwrap()
            .line_number;
        assert!(written_lines[first_duplicate_line - 1].contains("[ ]"));

        fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_back_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        for mode in [0o600u32, 0o640] {
            let path = write_temp_file(EXAMPLE);
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();

            let mut document = parse_document(path.clone()).unwrap();
            document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
            write_back(&document).unwrap();

            let actual = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(actual, mode, "permissions changed for mode {mode:o}");

            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn write_back_errors_on_deleted_source_file() {
        // write_back must refuse to recreate a deleted file rather than
        // silently overwriting the deletion. The app layer detects
        // deletion and blocks the call before we get here; this test guards
        // the writer itself.
        let path = write_temp_file(EXAMPLE);
        let mut document = parse_document(path.clone()).unwrap();
        document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);

        fs::remove_file(&path).unwrap();
        let result = write_back(&document);

        assert!(result.is_err(), "write_back must fail on a deleted file");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!path.exists(), "file must not be recreated");
    }

    /// Mirrors main.rs's startup canonicalization: with the resolved path,
    /// toggling through a symlink must update the target and leave the
    /// link itself intact.
    #[cfg(unix)]
    #[test]
    fn write_back_through_canonicalized_symlink_keeps_link_intact() {
        use std::os::unix::fs::PermissionsExt;

        let target = write_temp_file(EXAMPLE);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        let link_dir = std::env::temp_dir().join(format!(
            "markcheck-writer-test-linkdir-{}",
            std::process::id()
        ));
        fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("linked.md");
        fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let canonical = fs::canonicalize(&link).unwrap();
        let mut document = parse_document(canonical).unwrap();
        document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink must survive write-back"
        );
        assert!(fs::read_to_string(&target).unwrap().contains("[x]"));
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "target permissions must survive");

        fs::remove_file(&link).ok();
        fs::remove_dir(&link_dir).ok();
        fs::remove_file(&target).ok();
    }

    #[test]
    fn untoggling_reverts_checkbox() {
        let path = write_temp_file("- [x] `already done`\n");
        let mut document = parse_document(path.clone()).unwrap();

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::NotStarted);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("[ ]"));
        assert!(!written.contains("[x]"));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn writes_started_marker() {
        let path = write_temp_file("- [ ] `task`\n");
        let mut document = parse_document(path.clone()).unwrap();

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Started);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("[/]"),
            "started marker written: {written:?}"
        );
        assert!(!written.contains("[ ]") && !written.contains("[x]"));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn started_marker_transitions_to_done_and_not_started() {
        let path = write_temp_file("- [/] `task`\n");
        let mut document = parse_document(path.clone()).unwrap();
        assert_eq!(
            document.lists[0].items[0].kind,
            ItemKind::Checkbox(TaskState::Started)
        );

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("[x]"));

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::NotStarted);
        write_back(&document).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("[ ]"));
        assert!(!written.contains("[/]") && !written.contains("[x]"));

        fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_temp_retries_at_a_new_name_instead_of_deleting_a_collision() {
        // External review: the old behavior removed whatever file was
        // already at the generated name before retrying at that *same*
        // name -- conceptually wrong regardless of how unlikely a genuine
        // collision with another live process is, since that file could
        // belong to one. Deterministically forceable now that write_temp
        // takes an explicit base path: pre-create a file there and confirm
        // write_temp writes somewhere else instead, leaving it untouched.
        let base = crate::test_support::unique_temp_path("writer", "collision", None);
        fs::write(&base, "a live process's own temp file, not markcheck's\n").unwrap();

        let written_path = write_temp(&base, "checklist content\n", None).unwrap();

        assert_ne!(
            written_path, base,
            "must not have written to (or reused) the colliding name"
        );
        assert_eq!(
            fs::read_to_string(&base).unwrap(),
            "a live process's own temp file, not markcheck's\n",
            "the pre-existing file at the colliding name must be completely untouched"
        );
        assert_eq!(
            fs::read_to_string(&written_path).unwrap(),
            "checklist content\n"
        );

        fs::remove_file(&base).ok();
        fs::remove_file(&written_path).ok();
    }
}
