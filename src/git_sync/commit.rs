//! Building the commit through a temporary index seeded from the parent, so
//! unrelated staged content never rides along. An ordinary `git commit`, so
//! hooks and `commit.gpgsign` apply.

use std::io;
use std::path::Path;
use std::time::Duration;

use super::guards::{repo_sync_blocked, verify_commit_scope};
use super::inspect::{blob_at, current_head, descends_directly_from, git_dir, index_blob};
use super::process::{PLUMBING_TIMEOUT, command_error, git_command, run_with_timeout};
use super::{RacePoint, race_point};

/// Re-checks that `HEAD` is still `parent` before committing, so a tree
/// based on a stale parent can't revert a concurrent commit. The real index
/// is only touched afterwards, for `relpath` alone.
pub(super) fn commit_via_temp_index(
    repo_root: &Path,
    parent: &Option<String>,
    mode: &str,
    blob: &str,
    relpath: &str,
    message: &str,
) -> Result<String, String> {
    let git_dir = git_dir(repo_root)
        .ok_or_else(|| "could not find the repository's .git directory".to_string())?;
    let temp_index = git_dir.join(format!(
        "markcheck-index-{}-{:x}",
        std::process::id(),
        crate::writer::random_suffix()
    ));
    // The baseline `align_real_index_entry` compares against, so a version the
    // user stages meanwhile is never overwritten.
    let index_before = index_blob(repo_root, relpath);
    let result = (|| {
        if let Some(head) = parent {
            populate_temp_index(repo_root, &temp_index, head)?;
        }
        stage_into_temp_index(repo_root, &temp_index, mode, blob, relpath)?;
        if &current_head(repo_root) != parent {
            return Err("the repository changed while the sync was running".to_string());
        }
        // Re-checked here because the earlier check in `run_sync` is only a
        // snapshot.
        if let Some(reason) = repo_sync_blocked(repo_root) {
            return Err(reason);
        }
        race_point(RacePoint::BeforeCommit);
        let created_commit = commit_temp_index(
            repo_root,
            &temp_index,
            message,
            parent,
            relpath,
            blob,
            PLUMBING_TIMEOUT,
        )?;
        verify_commit_scope(repo_root, parent, relpath, &created_commit)?;
        race_point(RacePoint::BeforeIndexAlignment);
        align_real_index_entry(repo_root, mode, blob, relpath, index_before.as_deref());
        Ok(created_commit)
    })();
    let _ = std::fs::remove_file(&temp_index);
    // A subprocess killed mid-write can leave `<index>.lock` behind.
    let mut lock_file = temp_index.into_os_string();
    lock_file.push(".lock");
    let _ = std::fs::remove_file(lock_file);
    result
}
fn populate_temp_index(repo_root: &Path, temp_index: &Path, parent: &str) -> Result<(), String> {
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index)
        .args(["read-tree", parent]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git read-tree failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git read-tree", &output));
    }
    Ok(())
}
fn stage_into_temp_index(
    repo_root: &Path,
    temp_index: &Path,
    mode: &str,
    blob: &str,
    relpath: &str,
) -> Result<(), String> {
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index).args([
        "update-index",
        "--add",
        "--cacheinfo",
        mode,
        blob,
        relpath,
    ]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git update-index failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git update-index", &output));
    }
    Ok(())
}
/// Points the real index's entry for `relpath` at the committed blob, as a
/// normal `git commit -- <path>` would; otherwise `git status` shows the file
/// as both staged and unstaged after every sync. Skipped when the entry no
/// longer matches `expected_entry`, since the user has staged something of
/// their own. Best-effort: the commit has already succeeded.
fn align_real_index_entry(
    repo_root: &Path,
    mode: &str,
    blob: &str,
    relpath: &str,
    expected_entry: Option<&str>,
) {
    if index_blob(repo_root, relpath).as_deref() != expected_entry {
        return;
    }
    let mut cmd = git_command(repo_root);
    cmd.args(["update-index", "--add", "--cacheinfo", mode, blob, relpath]);
    let _ = run_with_timeout(cmd, PLUMBING_TIMEOUT);
}
/// Returns the new commit's SHA, captured once for `verify_commit_scope` and
/// `undo_commit`.
///
/// On a timeout, a `post-commit` hook may still be running after the commit
/// became durable, so the commit is adopted if the new `HEAD` holds `blob`
/// at `relpath` and its only parent is `parent`. Both checks are needed: a
/// concurrent commit on `parent` fails the first, one on top of ours the
/// second. Anything else is reported as a failure, never rewound.
fn commit_temp_index(
    repo_root: &Path,
    temp_index: &Path,
    message: &str,
    parent: &Option<String>,
    relpath: &str,
    blob: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index)
        .args(["commit", "-m", message]);
    match run_with_timeout(cmd, timeout) {
        Ok(output) if output.status.success() => current_head(repo_root)
            .ok_or_else(|| "could not read the repository's state after committing".to_string()),
        Ok(output) => Err(command_error("git commit", &output)),
        Err(err) if err.kind() == io::ErrorKind::TimedOut => match current_head(repo_root) {
            Some(head)
                if Some(&head) != parent.as_ref()
                    && blob_at(repo_root, &head, relpath).as_deref() == Some(blob)
                    && descends_directly_from(repo_root, &head, parent) =>
            {
                Ok(head)
            }
            _ => Err(format!("git commit failed: {err}")),
        },
        Err(err) => Err(format!("git commit failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::inspect::hash_object;
    use super::super::test_support::*;
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn commit_via_temp_index_refuses_when_head_moved_since_parent_was_read() {
        // A commit landing after `parent` was captured must make the sync refuse.
        let work = init_repo_without_remote();
        let stale_parent = current_head(&work);

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);

        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        let result = commit_via_temp_index(
            &work,
            &stale_parent,
            "100644",
            &blob,
            "tracked.md",
            "should not commit",
        );
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("changed while the sync was running")),
            "{result:?}"
        );

        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "concurrent change\ninit"
        );
        let leftover = fs::read_dir(work.join(".git")).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("markcheck-index-")
        });
        assert!(!leftover, "temp index file was not cleaned up");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn commit_via_temp_index_refuses_when_repo_becomes_blocked_after_the_head_check() {
        // A marker present at call time exercises the re-check.
        let work = init_repo_without_remote();
        let parent = current_head(&work);
        fs::write(work.join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();

        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        let result = commit_via_temp_index(
            &work,
            &parent,
            "100644",
            &blob,
            "tracked.md",
            "should not commit",
        );
        assert!(
            result.as_ref().is_err_and(|e| e.contains("merge")),
            "{result:?}"
        );

        let leftover = fs::read_dir(work.join(".git")).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("markcheck-index-")
        });
        assert!(!leftover, "temp index file was not cleaned up");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn stage_into_temp_index_reports_failure_for_an_invalid_mode() {
        let work = init_repo_without_remote();
        let blob = hash_object(&work, "tracked.md", "content").unwrap();
        let temp_index = work.join(".git").join("scratch-test-index");
        assert!(
            stage_into_temp_index(&work, &temp_index, "not-a-mode", &blob, "tracked.md").is_err()
        );
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn populate_temp_index_reports_failure_for_an_unresolvable_parent() {
        let work = init_repo_without_remote();
        let temp_index = work.join(".git").join("scratch-test-index");
        assert!(populate_temp_index(&work, &temp_index, "not-a-commit-id").is_err());
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn commit_temp_index_reports_failure_for_a_tree_unchanged_from_head() {
        let work = init_repo_without_remote();
        let temp_index = work.join(".git").join("scratch-test-index");
        let head = current_head(&work).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [ ] one\n").unwrap();
        populate_temp_index(&work, &temp_index, &head).unwrap();
        assert!(
            commit_temp_index(
                &work,
                &temp_index,
                "empty",
                &Some(head.clone()),
                "tracked.md",
                &blob,
                PLUMBING_TIMEOUT
            )
            .is_err()
        );
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn commit_temp_index_reconciles_when_a_post_commit_hook_outlives_the_timeout() {
        // A `post-commit` hook outliving the timeout: the commit already
        // succeeded.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-commit");
        fs::write(&hook_path, "#!/bin/sh\nsleep 2\n").unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-timeout");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "reconciled despite timeout",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            Duration::from_millis(300),
        );
        let new_head = current_head(&work);
        assert!(
            new_head.is_some() && new_head != Some(parent),
            "the commit itself must have actually landed: {new_head:?}"
        );
        assert_eq!(
            result.as_deref().ok(),
            new_head.as_deref(),
            "a timeout after the ref already moved must report success, not a false failure: {result:?}"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn commit_temp_index_still_fails_when_the_commit_never_lands() {
        // A `pre-commit` hook outliving the timeout: the commit never happened.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(&hook_path, "#!/bin/sh\nsleep 2\n").unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-never-lands");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "should not land",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            Duration::from_millis(300),
        );
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            current_head(&work),
            Some(parent),
            "the commit must never have happened"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn a_timed_out_commit_signals_its_hook_with_sigterm_first() {
        // A `pre-commit` hook past the timeout: SIGTERM lets git and the hook
        // clean up their locks, which SIGKILL never would.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let marker = work.parent().unwrap().join("hook-got-sigterm");
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(
            &hook_path,
            format!(
                "#!/bin/sh\ntrap 'touch {}; exit 1' TERM\nsleep 5 &\nwait\n",
                shell_words::quote(marker.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-sigterm");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "should not land",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            Duration::from_millis(300),
        );
        assert!(result.is_err(), "{result:?}");
        assert!(marker.exists(), "the hook must have received SIGTERM");
        assert!(
            !work.join(".git").join("index.lock").exists(),
            "no lock may be left in the user's repository"
        );
        assert_eq!(current_head(&work), Some(parent));

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn commit_temp_index_does_not_adopt_an_unrelated_commit_after_a_timeout() {
        // A `pre-commit` hook commits something unrelated and sleeps past the
        // timeout, so `HEAD` moves without our commit landing. `--no-verify` stops
        // the nested commit from recursing into the hook.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(
            &hook_path,
            format!(
                "#!/bin/sh\n\
                 echo unrelated > other.md\n\
                 env -u GIT_INDEX_FILE git add other.md\n\
                 env -u GIT_INDEX_FILE git commit -q --no-verify -m 'unrelated concurrent commit'\n\
                 sleep {HOOK_RACE_SLEEP}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-unrelated");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "should not adopt the unrelated commit",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            HOOK_RACE_TIMEOUT,
        );

        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("git commit failed")),
            "must not adopt the unrelated commit as its own: {result:?}"
        );
        let head_after = current_head(&work).unwrap();
        assert_ne!(
            head_after, parent,
            "the unrelated commit must still exist, untouched"
        );
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("unrelated concurrent commit"),
            "{}",
            String::from_utf8_lossy(&log.stdout)
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn commit_temp_index_does_not_adopt_a_commit_that_landed_on_top_of_its_own() {
        // Our commit lands and a `post-commit` hook commits on top without touching
        // `relpath`, so only the lineage check catches it. `read-tree HEAD` keeps
        // the nested commit from reverting our blob.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-commit");
        fs::write(
            &hook_path,
            format!(
                "#!/bin/sh\n\
                 env -u GIT_INDEX_FILE git read-tree HEAD\n\
                 echo unrelated > other.md\n\
                 env -u GIT_INDEX_FILE git add other.md\n\
                 env -u GIT_INDEX_FILE git commit -q --no-verify -m 'unrelated commit on top'\n\
                 sleep {HOOK_RACE_SLEEP}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-on-top");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "ours, then raced",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            HOOK_RACE_TIMEOUT,
        );

        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("git commit failed")),
            "must not adopt the commit that landed on top of ours: {result:?}"
        );
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(subjects.contains("unrelated commit on top"), "{subjects}");
        assert!(subjects.contains("ours, then raced"), "{subjects}");
        assert_eq!(
            blob_at(&work, "HEAD", "tracked.md").as_deref(),
            Some(blob.as_str()),
            "test setup: the content check must be the one that would pass"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn align_real_index_entry_leaves_a_newer_staged_version_alone() {
        // The user stages a newer version while the sync is in flight.
        let (work, _remote) = init_repo_with_remote();
        let before = index_blob(&work, "tracked.md");
        assert!(before.is_some(), "test setup: the file is tracked");

        fs::write(work.join("tracked.md"), "- [x] staged by the user\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        let user_staged = index_blob(&work, "tracked.md").unwrap();
        assert_ne!(Some(&user_staged), before.as_ref(), "test setup");

        let committed = hash_object(&work, "tracked.md", "- [x] committed by markcheck\n").unwrap();
        align_real_index_entry(&work, "100644", &committed, "tracked.md", before.as_deref());

        assert_eq!(
            index_blob(&work, "tracked.md"),
            Some(user_staged),
            "the user's staged version must survive untouched"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn align_real_index_entry_still_realigns_its_own_untouched_entry() {
        let (work, _remote) = init_repo_with_remote();
        let before = index_blob(&work, "tracked.md");

        let committed = hash_object(&work, "tracked.md", "- [x] committed by markcheck\n").unwrap();
        align_real_index_entry(&work, "100644", &committed, "tracked.md", before.as_deref());

        assert_eq!(
            index_blob(&work, "tracked.md"),
            Some(committed),
            "an untouched entry is still advanced to the committed blob"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
