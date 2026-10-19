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
/// Kept in a directory private to the user (see [`lock_dir`]), not beside
/// the checklist, so a `git add -A` hook can't commit it and another user
/// can't plant one; it therefore only serialises one user's instances on
/// one machine. A stale lock is recovered only when its owner is provably gone,
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
        let Some(path) = lock_path(target) else {
            return LockOutcome::Unavailable;
        };
        match Self::try_create(&path) {
            Ok(lock) => LockOutcome::Acquired(lock),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                match read_lock_token(&path) {
                    Err(err) if is_not_regular(&err) => return LockOutcome::Unavailable,
                    Ok(token) if token_owner_is_gone(&token) => {}
                    _ => return LockOutcome::Busy(path),
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
        use std::os::unix::fs::OpenOptionsExt;
        let token = format!("markcheck {} {:x}", std::process::id(), random_suffix());
        // The path is derivable by anyone, so keep it unreadable to others.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        Ok(WriteLock {
            path: path.to_path_buf(),
            token,
        })
    }

    /// Whether the lock file still holds this guard's own token.
    fn still_ours(&self) -> bool {
        read_lock_token(&self.path).is_ok_and(|held| held == self.token)
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
pub fn hard_link_count(target: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(target).ok().map(|meta| meta.nlink())
}

/// The UID owning `target` when it isn't the user running markcheck, who
/// can't keep it: `write_back`'s rename makes the file theirs. `None` when
/// the owner is kept (it already is the current user, or that user is
/// root) or either side can't be told. Warned about at startup, like a hard
/// link, since a teammate may then lose write access.
pub fn foreign_owner(target: &Path) -> Option<u32> {
    foreign_owner_for(target, current_uid()?)
}

fn foreign_owner_for(target: &Path, current_uid: u32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let owner = fs::metadata(target).ok()?.uid();
    (current_uid != 0 && owner != current_uid).then_some(owner)
}

/// The UID a file created by this process gets — exactly what a save's
/// temp file will be owned by — read off a throwaway probe file, as std
/// exposes no `geteuid`.
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
/// `None` when there is no private directory to put it in.
fn lock_path(target: &Path) -> Option<PathBuf> {
    Some(lock_dir()?.join(format!("markcheck-write-lock-{}", path_digest(target))))
}

/// The hex digest of `target`'s canonical path, shared by the write lock and
/// the session markers, so every spelling of one file shares them.
fn path_digest(target: &Path) -> String {
    let resolved = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let digest = crate::model::hash_bytes(resolved.as_os_str().as_encoded_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Another markcheck session that has the same checklist open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherSession {
    pub pid: u32,
    /// How long ago it opened the checklist, when that could be read.
    pub open_for: Option<std::time::Duration>,
}

/// This session's marker that it has a checklist open, removed on drop, so
/// a second session on the same checklist can warn at startup (#101). Same
/// user and machine only, like the write lock.
pub struct SessionMarker {
    path: Option<PathBuf>,
}

impl SessionMarker {
    /// Records this session and returns the others with `target` open,
    /// removing markers left by sessions that are provably gone. Without a
    /// private directory nothing is recorded and no one is reported.
    pub fn register(target: &Path) -> (SessionMarker, Vec<OtherSession>) {
        let Some(dir) = lock_dir() else {
            return (SessionMarker { path: None }, Vec::new());
        };
        let prefix = format!("markcheck-open-{}-", path_digest(target));
        let mut others = Vec::new();
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .filter(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            if pid == std::process::id().to_string() {
                continue;
            }
            if pid_is_gone(pid) {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            let open_for = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|opened| opened.elapsed().ok());
            if let Ok(pid) = pid.parse() {
                others.push(OtherSession { pid, open_for });
            }
        }
        others.sort_by_key(|other| other.pid);
        let path = dir.join(format!("{prefix}{}", std::process::id()));
        let recorded = write_new_file(&path, 0o600, "", |_| Ok(())).is_ok()
            || (path.exists() && fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()));
        (
            SessionMarker {
                path: recorded.then_some(path),
            },
            others,
        )
    }
}

impl Drop for SessionMarker {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

/// Where this user's write locks live, so another user can neither plant a
/// lock that blocks saving nor read one.
fn lock_dir() -> Option<PathBuf> {
    lock_dir_in(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        &std::env::temp_dir(),
        current_uid()?,
    )
}

/// `$XDG_RUNTIME_DIR` when it is private to `uid`, otherwise
/// `<temp>/markcheck-<uid>`, created 0700. `None` when the directory that
/// would be used isn't a real directory owned by `uid` and closed to
/// everyone else, which a squatter in a shared temp directory could arrange.
fn lock_dir_in(runtime_dir: Option<&std::ffi::OsStr>, temp: &Path, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let private = |dir: &Path| {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(dir)
            .is_ok_and(|meta| meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0)
    };
    if let Some(runtime_dir) = runtime_dir.map(Path::new)
        && runtime_dir.is_absolute()
        && private(runtime_dir)
    {
        return Some(runtime_dir.to_path_buf());
    }
    let dir = temp.join(format!("markcheck-{uid}"));
    let _ = fs::DirBuilder::new().mode(0o700).create(&dir);
    private(&dir).then_some(dir)
}

/// Opens `path` without following a symlink or blocking on a FIFO, and only
/// once the open handle is a regular file.
fn open_regular(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => return Err(refused("a symlink")),
        Err(err) => return Err(err),
    };
    let file_type = file.metadata()?.file_type();
    if !file_type.is_file() {
        return Err(not_regular(file_type));
    }
    Ok(file)
}

/// A lock file's token, read through [`open_regular`] and capped well above
/// a real token's length, so a planted file can't block or exhaust memory.
fn read_lock_token(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut token = String::new();
    open_regular(path)?.take(256).read_to_string(&mut token)?;
    Ok(token)
}

/// Whether a lock's owner is demonstrably gone; anything short of proof
/// is `false`. Never `kill -0`, which can't tell a dead process from
/// another user's.
fn token_owner_is_gone(token: &str) -> bool {
    token.split_whitespace().nth(1).is_some_and(pid_is_gone)
}

/// Whether no process `pid` (decimal digits) exists, on positive evidence
/// only. Never this process, whatever the evidence.
fn pid_is_gone(pid: &str) -> bool {
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if pid == std::process::id().to_string() {
        return false;
    }
    if let Some(gone) = proc_reports_gone(pid) {
        return gone;
    }
    ps_reports_gone(Command::new("ps"), pid)
}

/// Removes the regular files in `dir` named `<prefix><pid>-<hex>…` (more
/// `-<hex>` groups and a final `.lock` allowed) whose process is provably
/// gone: what a markcheck killed mid-save or mid-sync leaves behind, which
/// nothing else would ever remove. Returns how many were removed.
pub(crate) fn remove_leftovers(dir: &Path, prefix: &str) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let Some(rest) = name.to_str().and_then(|name| name.strip_prefix(prefix)) else {
                return false;
            };
            let Some((pid, suffix)) = rest.split_once('-') else {
                return false;
            };
            let suffix = suffix.strip_suffix(".lock").unwrap_or(suffix);
            let well_formed = suffix
                .split('-')
                .all(|group| !group.is_empty() && group.bytes().all(|b| b.is_ascii_hexdigit()));
            well_formed
                && entry.file_type().is_ok_and(|kind| kind.is_file())
                && pid_is_gone(pid)
                && fs::remove_file(entry.path()).is_ok()
        })
        .count()
}

/// Removes the temp files a save (`.<name>.markcheck-tmp-…`) or `--new`
/// (`.<name>.markcheck-new-…`) left beside `checklist` when markcheck died
/// before renaming them into place. Returns how many were removed.
pub fn sweep_interrupted_saves(checklist: &Path) -> usize {
    let (Some(dir), Some(name)) = (checklist.parent(), checklist.file_name()) else {
        return 0;
    };
    let name = name.to_string_lossy();
    remove_leftovers(dir, &save_temp_prefix(&name)) + remove_leftovers(dir, &new_temp_prefix(&name))
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

/// Replaces the line's task checkbox (`[ ]`, `[x]`, `[X]`, `[/]`, `[-]`) with
/// `target`. The leftmost marker is the checkbox, so a marker token in the
/// body is never the one rewritten. A checkbox already in the target state
/// is left as written, so an untouched `[X]` keeps its casing.
fn set_task_marker(line: &mut String, target: &str) {
    let leftmost = ["[ ]", "[x]", "[X]", "[/]", "[-]"]
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

/// Why a checklist path was refused: it is no longer a regular file but a
/// symlink, FIFO, device or directory put in its place. Holds what it is.
#[derive(Debug)]
pub struct NotRegularFile(&'static str);

impl std::fmt::Display for NotRegularFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a regular file: it is {}", self.0)
    }
}

impl std::error::Error for NotRegularFile {}

fn not_regular(file_type: fs::FileType) -> io::Error {
    use std::os::unix::fs::FileTypeExt;
    let what = if file_type.is_dir() {
        "a directory"
    } else if file_type.is_symlink() {
        "a symlink"
    } else if file_type.is_fifo() {
        "a named pipe"
    } else {
        "a device or socket"
    };
    refused(what)
}

fn refused(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, NotRegularFile(what))
}

/// Whether `err` is [`NotRegularFile`].
pub fn is_not_regular(err: &io::Error) -> bool {
    err.get_ref()
        .is_some_and(|inner| inner.is::<NotRegularFile>())
}

/// Reads the checklist without following a symlink or blocking on a FIFO,
/// and only once the open handle is a regular file, so a path swapped after
/// startup can neither leak another file's contents nor hang the UI.
pub fn read_checklist(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    open_regular(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Why a save failed, in plain words for the common causes.
pub fn describe_write_error(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::PermissionDenied => {
            "no permission to write the file or its folder".to_string()
        }
        io::ErrorKind::StorageFull => "the disk is full".to_string(),
        io::ErrorKind::QuotaExceeded => "your disk quota is used up".to_string(),
        io::ErrorKind::ReadOnlyFilesystem => "the disk is mounted read-only".to_string(),
        io::ErrorKind::NotFound => "the file or its folder no longer exists".to_string(),
        io::ErrorKind::InvalidFilename => "the file name is too long to save next to".to_string(),
        _ => err.to_string(),
    }
}

/// True when the file's own permission bits deny all writing (`chmod a-w`),
/// which `write_back` honors by refusing to save. False when it can't be read.
pub fn is_read_only(target: &Path) -> bool {
    fs::metadata(target).is_ok_and(|metadata| metadata.permissions().readonly())
}

/// What a successful [`write_back`] wrote, and how.
#[derive(Debug, PartialEq, Eq)]
pub struct Saved {
    pub contents: String,
    /// Written over the file in place rather than renamed into place,
    /// because no temp file could be created beside it (a directory the user
    /// can't write to). A crash mid-write could then leave it truncated.
    pub in_place: bool,
}

/// Writes to a temp file in the same directory, fsyncs it, and renames it
/// over the target, so a crash can't leave the checklist truncated; the
/// parent directory is fsynced too on unix. Permission bits (but never
/// setuid, setgid or sticky) and, where the user may set it, the group are
/// kept; hard links and another user's ownership
/// are not. A read-only file is refused untouched. When the directory
/// refuses the temp file but the file itself is writable, it is written in
/// place instead (`Saved::in_place`).
pub fn write_back(document: &Document) -> io::Result<Saved> {
    let mut lines = document.raw_lines.clone();
    for list in &document.lists {
        for item in &list.items {
            if let ItemKind::Checkbox(state) = item.kind {
                let target = match state {
                    TaskState::NotStarted => "[ ]",
                    TaskState::Started => "[/]",
                    TaskState::Done => "[x]",
                    TaskState::Skipped => "[-]",
                };
                set_task_marker(&mut lines[item.line_number - 1], target);
            }
        }
    }
    let contents = join_lines(&lines);

    // Read the source metadata before touching anything. Any error —
    // including NotFound — aborts before writing, so a deleted file is never
    // silently recreated. A 0600 file can also never be widened. Not
    // followed, so a symlink swapped in can't lend its target's mode.
    let source = fs::symlink_metadata(&document.file_path)?;
    if !source.is_file() {
        return Err(not_regular(source.file_type()));
    }
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
        "{}{}-{:x}",
        save_temp_prefix(&file_name),
        std::process::id(),
        random_suffix()
    ));

    // `write_temp` cleans up after its own failures.
    let temp_path = match write_temp(&temp_path_base, &contents, &source) {
        Ok(path) => path,
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            // The original error is the one worth reporting if this fails too.
            write_in_place(&document.file_path, &contents).map_err(|_| err)?;
            return Ok(Saved {
                contents,
                in_place: true,
            });
        }
        Err(err) => return Err(err),
    };
    if let Err(err) = fs::rename(&temp_path, &document.file_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    sync_parent_dir(&document.file_path);
    Ok(Saved {
        contents,
        in_place: false,
    })
}

/// Truncates and rewrites `path` through a handle that refuses a symlink and
/// is checked to be a regular file, then fsyncs it.
fn write_in_place(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let file_type = file.metadata()?.file_type();
    if !file_type.is_file() {
        return Err(not_regular(file_type));
    }
    file.set_len(0)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

/// The checklist name as it appears in a temp file's name, cut to a bounded
/// prefix: with the dot, the tag, a PID and two hex suffixes added, a long
/// name would otherwise pass the usual 255-byte limit and the save fail.
pub(crate) fn temp_name_stem(file_name: &str) -> &str {
    const MAX_STEM_BYTES: usize = 160;
    if file_name.len() <= MAX_STEM_BYTES {
        return file_name;
    }
    let mut end = MAX_STEM_BYTES;
    while !file_name.is_char_boundary(end) {
        end -= 1;
    }
    &file_name[..end]
}

/// `.<name>.markcheck-tmp-`, which a save's temp file starts with.
fn save_temp_prefix(file_name: &str) -> String {
    format!(".{}.markcheck-tmp-", temp_name_stem(file_name))
}

/// `.<name>.markcheck-new-`, which `--new`'s temp file starts with.
pub(crate) fn new_temp_prefix(file_name: &str) -> String {
    format!(".{}.markcheck-new-", temp_name_stem(file_name))
}

/// Best-effort fsync of `path`'s parent directory so the rename survives a
/// crash. Errors are ignored: the rename already succeeded.
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
/// (immune to umask) and fsyncs. Returns the path written.
fn write_temp(base_temp_path: &Path, contents: &str, source: &fs::Metadata) -> io::Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, fchown};

    write_new_temp(base_temp_path, 0o600, contents, |file| {
        // Best effort: keeping the UID needs root, but any group the
        // user belongs to may be set, which keeps a shared group
        // checklist writable by the group rather than handing it to the
        // saver's primary group.
        if fchown(file, Some(source.uid()), Some(source.gid())).is_err() {
            let _ = fchown(file, None, Some(source.gid()));
        }
        // Only the ordinary bits: the new file belongs to the saver, so
        // copying setuid/setgid/sticky would bypass the kernel clearing
        // them when someone other than the owner writes the file.
        file.set_permissions(fs::Permissions::from_mode(source.mode() & 0o777))
    })
}

/// [`write_new_file`] at `base`, or once more at a fresh name if `base` is
/// taken, never deleting the file there, which may belong to a live writer.
/// Returns the path written.
pub(crate) fn write_new_temp(
    base: &Path,
    mode: u32,
    contents: &str,
    prepare: impl FnOnce(&fs::File) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let mut retry_path = base.as_os_str().to_os_string();
    retry_path.push(format!("-{:x}", random_suffix()));
    let retry_path = PathBuf::from(retry_path);
    let mut prepare = Some(prepare);
    let mut attempt = |path: &Path| {
        write_new_file(path, mode, contents, |file| {
            prepare.take().map_or(Ok(()), |prepare| prepare(file))
        })
    };
    match attempt(base) {
        Ok(()) => Ok(base.to_path_buf()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            attempt(&retry_path)?;
            Ok(retry_path)
        }
        Err(err) => Err(err),
    }
}

/// Creates `path` with `mode` (before the umask), failing if it exists,
/// writes `contents`, runs `prepare` on the handle and fsyncs, removing the
/// file again if any of that fails.
pub(crate) fn write_new_file(
    path: &Path,
    mode: u32,
    contents: &str,
    prepare: impl FnOnce(&fs::File) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    let result = (|| {
        file.write_all(contents.as_bytes())?;
        prepare(&file)?;
        file.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
