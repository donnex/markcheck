use super::super::inspect::hash_object;
use super::super::test_support::*;
use super::*;
use std::fs;

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

    let log = crate::test_support::git()
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
}
#[test]
fn stage_into_temp_index_reports_failure_for_an_invalid_mode() {
    let work = init_repo_without_remote();
    let blob = hash_object(&work, "tracked.md", "content").unwrap();
    let temp_index = work.join(".git").join("scratch-test-index");
    assert!(stage_into_temp_index(&work, &temp_index, "not-a-mode", &blob, "tracked.md").is_err());
    let _ = fs::remove_file(&temp_index);
}
#[test]
fn populate_temp_index_reports_failure_for_an_unresolvable_parent() {
    let work = init_repo_without_remote();
    let temp_index = work.join(".git").join("scratch-test-index");
    assert!(populate_temp_index(&work, &temp_index, "not-a-commit-id").is_err());
    let _ = fs::remove_file(&temp_index);
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
}
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
}
#[test]
fn a_failing_pre_commit_hook_is_reported_by_its_last_line() {
    use std::os::unix::fs::PermissionsExt;

    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();
    let hooks_dir = work.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join("pre-commit");
    fs::write(
            &hook_path,
            "#!/bin/sh\necho '[INFO] Initializing environment' >&2\necho 'lint: tracked.md is malformed'\nexit 1\n",
        )
        .unwrap();
    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

    let temp_index = work.join(".git").join("scratch-test-index-hook-fails");
    populate_temp_index(&work, &temp_index, &parent).unwrap();
    let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
    stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

    let result = commit_temp_index(
        &work,
        &temp_index,
        "refused by the hook",
        &Some(parent.clone()),
        "tracked.md",
        &blob,
        COMMIT_TIMEOUT,
    );
    assert_eq!(
        result,
        Err("git commit: lint: tracked.md is malformed".to_string())
    );
    assert_eq!(current_head(&work), Some(parent));

    let _ = fs::remove_file(&temp_index);
}
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
}
#[test]
fn a_timed_out_commit_signals_its_hook_with_sigterm_first() {
    // A `pre-commit` hook past the timeout: SIGTERM lets git and the hook
    // clean up their locks, which SIGKILL never would. On a loaded machine the
    // signal can beat the hook's trap, or the grace period can end before the
    // trap runs, so such an attempt is retried with a longer timeout.
    for timeout_ms in [300, 1_000, 3_000, 10_000] {
        if sigterm_attempt(Duration::from_millis(timeout_ms)) {
            return;
        }
    }
    panic!("no attempt saw the hook ready and signalled within the grace period");
}
fn sigterm_attempt(timeout: Duration) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();
    let ready = work.parent().unwrap().join("hook-ready");
    let marker = work.parent().unwrap().join("hook-got-sigterm");
    let hooks_dir = work.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join("pre-commit");
    fs::write(
        &hook_path,
        format!(
            "#!/bin/sh\ntrap ': > {}; exit 1' TERM\n: > {}\nsleep {HOOK_RACE_SLEEP} &\nwait\n",
            shell_words::quote(marker.to_str().unwrap()),
            shell_words::quote(ready.to_str().unwrap())
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
        timeout,
    );
    let force_killed = matches!(&result, Err(err) if err.contains("force-killed"));
    let counted = ready.exists() && !force_killed;
    if counted {
        assert!(result.is_err(), "{result:?}");
        // git exits on the signal without waiting for the hook's trap.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists(), "the hook must have received SIGTERM");
        assert!(
            !work.join(".git").join("index.lock").exists(),
            "no lock may be left in the user's repository"
        );
        assert_eq!(current_head(&work), Some(parent));
    }

    let _ = fs::remove_file(&temp_index);
    counted
}
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
    let log = crate::test_support::git()
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
}
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
            .is_err_and(|e| e.contains("landed on top of markcheck's commit")),
        "must not adopt the commit that landed on top of ours: {result:?}"
    );
    assert_eq!(
        git_stdout(&work, &["log", "--format=%s"]),
        "unrelated commit on top\nours, then raced\ninit",
        "both commits must stay on the branch"
    );
    assert_eq!(
        blob_at(&work, "HEAD", "tracked.md").as_deref(),
        Some(blob.as_str()),
        "test setup: the content check must be the one that would pass"
    );

    let _ = fs::remove_file(&temp_index);
}

fn install_hook(work: &Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;

    let hooks_dir = work.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join(name);
    fs::write(&hook_path, format!("#!/bin/sh\n{script}")).unwrap();
    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();
}
#[test]
fn a_post_commit_hook_committing_on_top_is_never_rewound() {
    // The hook finishes in time, so only the lineage check tells its commit
    // from markcheck's; trusting `HEAD` undid the hook's commit instead.
    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();
    install_hook(
        &work,
        "post-commit",
        "env -u GIT_INDEX_FILE git read-tree HEAD\n\
         echo generated > other.md\n\
         env -u GIT_INDEX_FILE git add other.md\n\
         env -u GIT_INDEX_FILE git commit -q --no-verify -m 'hook follow-up'\n",
    );
    let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();

    let result = commit_via_temp_index(
        &work,
        &Some(parent.clone()),
        "100644",
        &blob,
        "tracked.md",
        "tracked.md: Check \"one\"",
    );

    let err = result.expect_err("a commit on top of markcheck's must not be adopted");
    assert!(err.contains("landed on top of markcheck's commit"), "{err}");
    assert!(!err.contains("undone"), "nothing was undone: {err}");
    assert_eq!(
        git_stdout(&work, &["log", "--format=%s"]),
        "hook follow-up\ntracked.md: Check \"one\"\ninit",
        "the hook's commit must stay on the branch, on top of markcheck's"
    );
}
#[test]
fn a_commit_hook_removing_markchecks_commit_leaves_the_branch_alone() {
    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();
    install_hook(&work, "post-commit", "git update-ref HEAD HEAD~1\n");
    let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();

    let result = commit_via_temp_index(
        &work,
        &Some(parent.clone()),
        "100644",
        &blob,
        "tracked.md",
        "tracked.md: Check \"one\"",
    );

    let err = result.expect_err("a commit that is gone can't be adopted");
    assert!(err.contains("could not be found on the branch"), "{err}");
    assert_eq!(current_head(&work), Some(parent));
}
#[test]
fn a_root_commit_is_found_by_its_lineage() {
    let dir = unique_dir("root-commit-lineage");
    fs::create_dir_all(&dir).unwrap();
    run(&dir, &["init", "-q", "-b", "main"]);
    run(&dir, &["config", "user.email", "test@example.com"]);
    run(&dir, &["config", "user.name", "test"]);
    let blob = hash_object(&dir, "tracked.md", "- [x] one\n").unwrap();

    let result = commit_via_temp_index(&dir, &None, "100644", &blob, "tracked.md", "first");

    assert_eq!(result.ok(), current_head(&dir));
    assert_eq!(git_stdout(&dir, &["log", "--format=%s"]), "first");
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
}
