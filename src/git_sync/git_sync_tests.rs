use super::guards::verify_commit_scope;
use super::inspect::current_head;
use super::test_support::*;
use super::*;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn detect_finds_a_repo() {
    let work = init_repo_without_remote();
    assert!(GitSync::detect(&work.join("tracked.md")).is_ok());
}

#[test]
fn a_poisoned_latest_hash_does_not_take_down_the_main_thread() {
    let work = init_repo_without_remote();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

    let hash = Arc::clone(&sync.latest_requested_hash);
    let poisoner = std::thread::spawn(move || {
        let _guard = hash.lock().unwrap();
        panic!("poison the mutex");
    });
    assert!(poisoner.join().is_err(), "the poisoning thread must panic");
    assert!(sync.latest_requested_hash.is_poisoned());

    sync.request(pending_sync("- [x] one\n", "after poisoning"));
    assert_eq!(
        *lock_hash(&sync.latest_requested_hash),
        hash_bytes(b"- [x] one\n")
    );
}

#[test]
fn detect_says_why_outside_a_repo() {
    let dir = unique_dir("not-a-repo");
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        GitSync::detect(&dir.join("file.md")).err().as_deref(),
        Some("the file is not in a git repository")
    );
}

#[test]
fn detect_says_why_inside_the_git_directory() {
    // `rev-parse --is-inside-work-tree` succeeds there but prints `false`.
    let work = init_repo_without_remote();
    assert_eq!(
        GitSync::detect(&work.join(".git").join("file.md"))
            .err()
            .as_deref(),
        Some("the file is not in a git work tree")
    );
}

#[test]
fn detect_says_why_for_a_path_without_a_parent() {
    assert_eq!(
        GitSync::detect(Path::new("/")).err().as_deref(),
        Some("the file has no parent directory")
    );
}

#[test]
fn detect_spawn_error_names_a_missing_git() {
    let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
    assert_eq!(detect_spawn_error(&missing), "git not found on PATH");
    let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "git command timed out");
    assert_eq!(
        detect_spawn_error(&timed_out),
        "could not run git: git command timed out"
    );
}

#[test]
fn detect_git_error_keeps_gits_own_fix_hint() {
    let dubious = b"fatal: detected dubious ownership in repository at '/srv/runbooks'\n\
            To add an exception for this directory, call:\n\n\
            \tgit config --global --add safe.directory /srv/runbooks\n";
    assert_eq!(
        detect_git_error(dubious),
        "fatal: detected dubious ownership in repository at '/srv/runbooks' \
             To add an exception for this directory, call: \
             git config --global --add safe.directory /srv/runbooks"
    );
    assert_eq!(
        detect_git_error(b"fatal: not a git repository (or any of the parent directories): .git\n"),
        "the file is not in a git repository"
    );
    assert_eq!(
        detect_git_error(b"\n"),
        "git could not inspect the file's directory"
    );
}

#[test]
fn repeated_requests_against_an_unreachable_remote_never_deadlock_or_lose_edits() {
    // A nonexistent remote makes `git push` fail fast.
    let work = init_repo_without_remote();
    run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
    run(&work, &["config", "branch.main.remote", "origin"]);
    run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    sync.request(pending_sync("- [x] one\n", "first"));
    fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();
    sync.request(pending_sync("- [x] one\n- [x] two\n", "second"));
    fs::write(
        work.join("tracked.md"),
        "- [x] one\n- [x] two\n- [x] three\n",
    )
    .unwrap();
    sync.request(pending_sync("- [x] one\n- [x] two\n- [x] three\n", "third"));
    assert!(sync.busy, "still mid-flight on the first attempt");

    let outcomes = settle(&mut sync);

    // "second" is always overwritten by "third" before it runs.
    assert!(
        outcomes.len() <= 2,
        "at most 2 sync attempts, never one per request: {outcomes:?}"
    );
    assert!(!sync.busy, "must settle back to idle, not stuck busy");
    assert!(sync.queued.is_none(), "no request left dangling");
    assert!(
        matches!(
            outcomes.first(),
            Some(SyncOutcome::CommittedNotPushed { .. })
        ),
        "first attempt has a real, unpushed change: {outcomes:?}"
    );

    let log = crate::test_support::git()
        .current_dir(&work)
        .args(["log", "--format=%s"])
        .output()
        .unwrap();
    let subjects = String::from_utf8_lossy(&log.stdout);
    assert!(
        subjects.contains("tracked.md: first"),
        "first edit committed locally: {subjects}"
    );
    let final_contents = fs::read_to_string(work.join("tracked.md")).unwrap();
    assert!(
        final_contents.contains("three"),
        "the latest edit is on disk regardless of push failing: {final_contents}"
    );

    let remote = init_repo_without_remote();
    run(
        &work,
        &["remote", "set-url", "origin", remote.to_str().unwrap()],
    );
    run(&work, &["push", "-q", "origin", "main:recovered"]);
    let remote_log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "--format=%s", "recovered"])
        .output()
        .unwrap();
    let remote_subjects = String::from_utf8_lossy(&remote_log.stdout);
    assert!(
        remote_subjects.contains("tracked.md: first"),
        "catch-up push carries every commit made while offline: {remote_subjects}"
    );
}

#[test]
fn retry_push_if_due_noops_when_nothing_is_pending() {
    let work = init_repo_without_remote();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    sync.retry_push_if_due(Instant::now());
    assert!(!sync.busy, "nothing pending, nothing to retry");
}

#[test]
fn retry_push_if_due_noops_before_the_backoff_interval_elapses() {
    let work = init_repo_without_remote();
    let commit = current_head(&work).unwrap();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    let last_attempt = Instant::now();
    sync.retry = Some((commit, last_attempt));

    sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL - Duration::from_millis(1));

    assert!(!sync.busy, "backoff interval hasn't elapsed yet");
}

#[test]
fn retry_push_if_due_noops_while_busy() {
    let work = init_repo_without_remote();
    let commit = current_head(&work).unwrap();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    sync.busy = true;
    sync.retry = Some((commit, Instant::now() - PUSH_RETRY_INTERVAL));

    sync.retry_push_if_due(Instant::now());

    assert!(sync.busy);
}

#[test]
fn retry_push_if_due_retries_once_the_backoff_interval_elapses() {
    let work = init_repo_without_remote();
    run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
    run(&work, &["config", "branch.main.remote", "origin"]);
    run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

    // Committed directly rather than through `run_sync`.
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "Check \"one\""]);
    let commit = current_head(&work).unwrap();

    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    let last_attempt = Instant::now();
    sync.retry = Some((commit, last_attempt));

    sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL);
    assert!(
        sync.busy,
        "the backoff interval elapsed, a retry should have spawned"
    );

    let outcomes = settle(&mut sync);
    assert!(
        matches!(
            outcomes.as_slice(),
            [SyncOutcome::CommittedNotPushed { .. }]
        ),
        "still no remote to push to, so this retry fails the same way: {outcomes:?}"
    );
}

#[test]
fn retry_push_if_due_clears_the_retry_when_head_has_moved_on() {
    // An abandoned retry must be cleared, or it fires again every call.
    let work = init_repo_without_remote();
    run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
    run(&work, &["config", "branch.main.remote", "origin"]);
    run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "Check \"one\""]);
    let stale_commit = current_head(&work).unwrap();

    fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();
    run(&work, &["commit", "-q", "-am", "newer commit"]);

    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    let last_attempt = Instant::now();
    sync.retry = Some((stale_commit, last_attempt));

    sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL);
    assert!(
        sync.busy,
        "the backoff interval elapsed, a retry should have spawned"
    );

    assert_eq!(settle(&mut sync), vec![SyncOutcome::RetryAbandoned]);
    assert!(
        sync.retry.is_none(),
        "the stale retry must be cleared, not left armed forever"
    );

    sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL * 2);
    assert!(!sync.busy, "nothing left to retry");
}

#[test]
fn apply_outcome_covers_every_variant_s_effect_on_the_retry_state() {
    let work = init_repo_without_remote();
    let commit = current_head(&work).unwrap();
    let armed = |sync: &mut GitSync, at: Instant| sync.retry = Some((commit.clone(), at));

    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    armed(&mut sync, Instant::now());
    sync.apply_outcome(&SyncOutcome::Synced);
    assert!(sync.retry.is_none(), "Synced clears the retry");

    armed(&mut sync, Instant::now());
    sync.apply_outcome(&SyncOutcome::RetryAbandoned);
    assert!(sync.retry.is_none(), "an abandoned retry clears it");

    let old = Instant::now() - PUSH_RETRY_INTERVAL * 4;
    armed(&mut sync, old);
    sync.apply_outcome(&SyncOutcome::CommittedNotPushed {
        message: "nope".to_string(),
        commit: commit.clone(),
    });
    let (_, at) = sync.retry.clone().expect("still armed");
    assert!(at > old, "CommittedNotPushed restarts the backoff");

    armed(&mut sync, old);
    sync.apply_outcome(&SyncOutcome::RetryFailed("boom".to_string()));
    let (_, at) = sync
        .retry
        .clone()
        .expect("RetryFailed keeps the retry armed");
    assert!(
        at > old,
        "RetryFailed must restart the backoff, not leave it stale"
    );

    armed(&mut sync, old);
    sync.apply_outcome(&SyncOutcome::Failed("boom".to_string()));
    let (_, at) = sync.retry.clone().expect("Failed keeps the retry armed");
    assert_eq!(at, old, "Failed must leave the retry's backoff alone");

    for outcome in [SyncOutcome::Skipped, SyncOutcome::SkippedUntracked] {
        armed(&mut sync, old);
        sync.apply_outcome(&outcome);
        let (_, at) = sync.retry.clone().expect("left armed");
        assert_eq!(at, old, "{outcome:?} must leave an unrelated retry alone");
    }
}

#[test]
fn repeated_push_failures_grow_the_retry_interval_up_to_a_cap() {
    let work = init_repo_without_remote();
    let commit = current_head(&work).unwrap();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    let not_pushed = |commit: &str| SyncOutcome::CommittedNotPushed {
        message: "offline".to_string(),
        commit: commit.to_string(),
    };

    sync.apply_outcome(&not_pushed(&commit));
    assert_eq!(sync.retry_interval, PUSH_RETRY_INTERVAL);

    sync.apply_outcome(&not_pushed(&commit));
    assert_eq!(sync.retry_interval, PUSH_RETRY_INTERVAL * 2);
    sync.apply_outcome(&SyncOutcome::RetryFailed("offline".to_string()));
    assert_eq!(sync.retry_interval, PUSH_RETRY_INTERVAL * 4);

    let (_, last_attempt) = sync.retry.clone().unwrap();
    sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL * 3);
    assert!(!sync.busy, "the grown interval hasn't elapsed yet");

    for _ in 0..10 {
        sync.apply_outcome(&not_pushed(&commit));
    }
    assert_eq!(sync.retry_interval, PUSH_RETRY_MAX_INTERVAL);

    sync.apply_outcome(&not_pushed("a-newer-commit"));
    assert_eq!(
        sync.retry_interval, PUSH_RETRY_INTERVAL,
        "a new commit starts the backoff over"
    );

    sync.apply_outcome(&not_pushed("a-newer-commit"));
    sync.apply_outcome(&SyncOutcome::Synced);
    assert_eq!(sync.retry_interval, PUSH_RETRY_INTERVAL, "a push resets it");
}

#[test]
fn an_unrelated_content_failure_does_not_starve_an_armed_retry() {
    // A repeatedly failing content sync must not postpone a pushable commit.
    let work = init_repo_without_remote();
    let commit = current_head(&work).unwrap();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

    let due = Instant::now() - PUSH_RETRY_INTERVAL * 4;
    sync.retry = Some((commit.clone(), due));
    sync.apply_outcome(&SyncOutcome::Failed("unrelated content sync".to_string()));

    let (_, at) = sync.retry.clone().expect("still armed");
    assert_eq!(
        at, due,
        "a content sync's failure must leave the retry's backoff alone"
    );

    sync.retry = Some((commit, due));
    sync.apply_outcome(&SyncOutcome::RetryFailed("retry blew up".to_string()));
    let (_, at) = sync.retry.clone().expect("still armed");
    assert!(at > due, "a failed retry restarts its own backoff");
}

#[test]
fn a_failed_retry_does_not_fire_again_on_the_very_next_frame() {
    // A permanently failing retry must not fire on every frame.
    let work = init_repo_without_remote();
    let commit = current_head(&work).unwrap();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    let last_attempt = Instant::now() - PUSH_RETRY_INTERVAL * 2;
    sync.retry = Some((commit, last_attempt));

    sync.retry_push_if_due(Instant::now());
    assert!(sync.busy, "a retry should have spawned");
    sync.busy = false;
    sync.apply_outcome(&SyncOutcome::RetryFailed(
        "git rev-parse: not a git repository".to_string(),
    ));

    sync.retry_push_if_due(Instant::now());
    assert!(
        !sync.busy,
        "a failed retry must respect the backoff instead of firing every frame"
    );
    assert!(
        sync.retry.is_some(),
        "the commit is still unpushed, so the retry stays armed for later"
    );
}

#[test]
fn request_and_poll_roundtrip_reports_synced() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();

    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    sync.request(pending_sync("- [x] one\n", "Check \"one\""));

    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);
}

#[test]
fn a_request_while_busy_is_coalesced_and_runs_after() {
    // The change is written before either request, so "first" has something
    // to commit and "second" finds nothing left.
    let (work, remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

    sync.request(pending_sync("- [x] one\n", "first"));
    assert!(sync.busy, "first request should mark the worker busy");
    sync.request(pending_sync("- [x] one\n", "second"));
    assert_eq!(
        sync.queued.as_ref().map(|q| q.descriptions.clone()),
        Some(vec!["second".to_string()])
    );

    assert_eq!(
        settle(&mut sync),
        vec![SyncOutcome::Synced, SyncOutcome::Skipped]
    );

    let log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "tracked.md: first"
    );
}

#[test]
fn an_unverifiable_commit_scope_says_the_commit_was_left_behind() {
    // A failed scope check must say a commit was made and kept.
    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();
    let bogus = "0".repeat(40);

    let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &bogus);

    let err = result.expect_err("an unresolvable commit cannot be verified");
    assert!(
        err.contains(&bogus) && err.contains("left on the branch"),
        "the message must name the commit and say it survived: {err}"
    );
}

#[test]
fn status_message_stays_quiet_for_a_landed_or_no_op_sync() {
    assert_eq!(status_message(&SyncOutcome::Synced), None);
    assert_eq!(status_message(&SyncOutcome::Skipped), None);
    assert_eq!(status_message(&SyncOutcome::RetryAbandoned), None);
}

#[test]
fn status_message_names_each_problem() {
    assert_eq!(
        status_message(&SyncOutcome::SkippedUntracked).as_deref(),
        Some("Git sync skipped: file is not tracked in git (run `git add` on it)")
    );
    assert_eq!(
        status_message(&SyncOutcome::CommittedNotPushed {
            message: "remote moved".into(),
            commit: "abc".into(),
        })
        .as_deref(),
        Some("Committed, not pushed: remote moved (will keep retrying)")
    );
    assert_eq!(
        status_message(&SyncOutcome::Failed("hook said no".into())).as_deref(),
        Some("Git sync failed: hook said no")
    );
    assert_eq!(
        status_message(&SyncOutcome::RetryFailed("offline".into())).as_deref(),
        Some("Push retry failed: offline (will keep retrying)")
    );
    assert_eq!(
        status_message(&SyncOutcome::RemoteAhead("pull first".into())).as_deref(),
        Some("Git sync: pull first")
    );
}

#[test]
fn exit_report_stays_quiet_when_the_final_sync_landed_or_had_nothing_to_do() {
    assert_eq!(exit_report(None, false), None);
    assert_eq!(exit_report(Some(&SyncOutcome::Synced), false), None);
    assert_eq!(exit_report(Some(&SyncOutcome::Skipped), false), None);
    assert_eq!(exit_report(Some(&SyncOutcome::RetryAbandoned), false), None);
    assert_eq!(
        exit_report(Some(&SyncOutcome::RemoteAhead("pull".into())), false),
        None
    );
}

#[test]
fn exit_report_names_each_problem_with_the_final_sync() {
    let failed = exit_report(Some(&SyncOutcome::Failed("hook said no".into())), false);
    assert!(failed.is_some_and(
        |m| m.contains("git sync failed: hook said no") && m.contains("not committed")
    ));

    let unpushed = exit_report(
        Some(&SyncOutcome::CommittedNotPushed {
            message: "remote moved".into(),
            commit: "abc".into(),
        }),
        false,
    );
    assert!(unpushed.is_some_and(|m| m.contains("committed but not pushed: remote moved")));

    let retry = exit_report(Some(&SyncOutcome::RetryFailed("offline".into())), false);
    assert!(retry.is_some_and(|m| m.contains("committed but not pushed: offline")));

    let untracked = exit_report(Some(&SyncOutcome::SkippedUntracked), false);
    assert!(untracked.is_some_and(|m| m.contains("not tracked in git")));
}

#[test]
fn exit_report_drops_terminal_controls_from_git_output() {
    let failed = exit_report(
        Some(&SyncOutcome::Failed(
            "remote: no\x1b]52;c;AA==\x07\nremote:\tbye".into(),
        )),
        false,
    );
    assert_eq!(
        failed.as_deref(),
        Some(
            "markcheck: git sync failed: remote: no]52;c;AA==\nremote:\tbye; \
             the file is saved, but not committed"
        )
    );
}

#[test]
fn exit_report_flags_a_sync_cut_off_by_the_exit_wait() {
    for last in [None, Some(&SyncOutcome::Synced)] {
        assert!(exit_report(last, true).is_some_and(|m| m.contains("still running")));
    }
}

/// Hands `outcome` to `poll` as if a worker had just finished.
fn deliver(sync: &mut GitSync, outcome: SyncOutcome) {
    sync.sender.send(outcome).unwrap();
    sync.poll().expect("the outcome just sent");
}

#[test]
fn a_failure_polled_before_quitting_is_still_reported_on_exit() {
    let work = init_repo_without_remote();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    assert_eq!(exit_report(sync.unresolved(), false), None);

    deliver(&mut sync, SyncOutcome::Failed("hook said no".into()));
    assert!(sync.wait_for_outcomes(Duration::ZERO).is_empty());
    let report = exit_report(sync.unresolved(), sync.is_busy());
    assert!(
        report.is_some_and(|m| m.contains("git sync failed: hook said no")),
        "a failure seen before the quit must still be reported"
    );
}

#[test]
fn a_later_sync_that_lands_resolves_the_standing_problem() {
    let work = init_repo_without_remote();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    for resolving in [SyncOutcome::Synced, SyncOutcome::Skipped] {
        deliver(&mut sync, SyncOutcome::SkippedUntracked);
        deliver(&mut sync, resolving);
        assert_eq!(sync.unresolved(), None);
    }

    let not_pushed = SyncOutcome::CommittedNotPushed {
        message: "offline".into(),
        commit: "abc".into(),
    };
    deliver(&mut sync, not_pushed.clone());
    assert_eq!(sync.unresolved(), Some(&not_pushed));
    deliver(&mut sync, SyncOutcome::RetryFailed("still offline".into()));
    assert_eq!(
        sync.unresolved(),
        Some(&SyncOutcome::RetryFailed("still offline".into()))
    );
    deliver(&mut sync, SyncOutcome::NotCaughtUp("someone else's".into()));
    assert!(sync.unresolved().is_some(), "NotCaughtUp resolves nothing");
    deliver(&mut sync, SyncOutcome::RemoteAhead("pull".into()));
    assert!(sync.unresolved().is_some(), "RemoteAhead resolves nothing");
    deliver(&mut sync, SyncOutcome::RetryAbandoned);
    assert_eq!(sync.unresolved(), None);

    deliver(&mut sync, not_pushed);
    deliver(&mut sync, SyncOutcome::CaughtUp { pushed: 1 });
    assert_eq!(sync.unresolved(), None);
}

#[test]
fn a_push_outcome_does_not_hide_a_content_sync_that_failed() {
    let work = init_repo_without_remote();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    let failed = SyncOutcome::Failed("hook said no".into());
    deliver(&mut sync, failed.clone());
    for push in [
        SyncOutcome::RetryFailed("offline".into()),
        SyncOutcome::RetryAbandoned,
        SyncOutcome::CaughtUp { pushed: 1 },
    ] {
        deliver(&mut sync, push);
        assert_eq!(sync.unresolved(), Some(&failed));
    }
}

#[test]
fn sweep_interrupted_syncs_removes_a_dead_sync_s_temp_files() {
    let work = init_repo_without_remote();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let git_dir = work.join(".git");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 60);
    for name in [
        format!("markcheck-index-{dead}-1f"),
        format!("markcheck-index-{dead}-1f.lock"),
        format!("markcheck-own-commits-{dead}-2e"),
        "markcheck-own-commits".to_string(),
    ] {
        let file = fs::File::create(git_dir.join(name)).unwrap();
        file.set_modified(old).unwrap();
    }

    let sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    assert_eq!(sync.sweep_interrupted_syncs(), 3);
    assert!(
        git_dir.join("markcheck-own-commits").exists(),
        "the record itself is kept"
    );
    assert_eq!(sync.sweep_interrupted_syncs(), 0);
}

#[test]
fn sweep_interrupted_scratch_removes_a_dead_sync_s_stdio_files() {
    let work = init_repo_without_remote();
    let temp = work.parent().unwrap().join("scratch-temp");
    fs::create_dir_all(&temp).unwrap();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 60);
    for name in [
        format!("markcheck-git-stdin-{dead}-1f"),
        format!("markcheck-git-stdout-{dead}-2e"),
        format!("markcheck-git-stderr-{dead}-3d"),
        format!("markcheck-git-other-{dead}-4c"),
    ] {
        let file = fs::File::create(temp.join(name)).unwrap();
        file.set_modified(old).unwrap();
    }
    let young = temp.join(format!("markcheck-git-stdout-{dead}-5b"));
    fs::File::create(&young).unwrap();

    assert_eq!(GitSync::sweep_interrupted_scratch(&temp), 3);
    assert_eq!(GitSync::sweep_interrupted_scratch(&temp), 0);
    assert!(young.exists());
    assert!(temp.join(format!("markcheck-git-other-{dead}-4c")).exists());
}

#[test]
fn abandon_stops_the_running_git_and_starts_no_more() {
    let work = init_repo_without_remote();
    let hooks = work.join(".git").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let started = work.parent().unwrap().join("hook-started");
    let hook = hooks.join("pre-commit");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\n: > {}\nsleep {HOOK_RACE_SLEEP}\n",
            shell_words::quote(started.to_str().unwrap())
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let file = work.join("tracked.md");
    fs::write(&file, "- [x] one\n").unwrap();
    let mut sync = GitSync::detect(&file).unwrap();
    assert!(!sync.abandon(false), "nothing is running yet");
    let mut sync = GitSync::detect(&file).unwrap();
    sync.request(pending_sync("- [x] one\n", "Check"));

    let deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(started.exists(), "the hook never started");
    assert_eq!(sync.wait_for_outcomes(Duration::from_millis(100)), vec![]);
    assert!(
        sync.is_busy(),
        "the wait gives up while the hook still runs"
    );
    let asked = Instant::now();
    assert_eq!(
        sync.wait_for_outcomes_unless(Duration::from_secs(60), || true),
        vec![]
    );
    assert!(
        sync.is_busy() && asked.elapsed() < Duration::from_secs(30),
        "a wait told to give up returns at once, the sync still running"
    );
    assert!(sync.abandon(false), "the commit's git was running");

    let outcomes = settle(&mut sync);
    assert!(
        matches!(outcomes.as_slice(), [SyncOutcome::Failed(_)]),
        "the abandoned sync ends instead of waiting out its hook: {outcomes:?}"
    );
    let left: Vec<_> = fs::read_dir(work.join(".git"))
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("markcheck-index-")
        })
        .collect();
    assert!(left.is_empty(), "the worker removed its temp index");

    process::track_children_on_this_thread(Arc::clone(&sync.children));
    let err = run_with_timeout(git_command(&work), PLUMBING_TIMEOUT).unwrap_err();
    assert!(
        err.to_string().contains("abandoned"),
        "no git may start after abandoning: {err}"
    );
}

#[test]
fn a_checklist_in_a_subdirectory_syncs_from_the_repository_root() {
    let (work, remote) = init_repo_with_remote();
    let sub = work.join("docs");
    fs::create_dir_all(&sub).unwrap();
    let file = sub.join("list.md");
    fs::write(&file, "- [ ] one\n").unwrap();
    run(&work, &["add", "docs/list.md"]);
    run(&work, &["commit", "-q", "-m", "add nested checklist"]);
    run(&work, &["push", "-q", "origin", "main"]);

    let mut sync = GitSync::detect(&file).unwrap();
    assert_eq!(
        sync.repo_root,
        fs::canonicalize(&work).unwrap(),
        "detect stores the root, not the checklist's directory"
    );
    fs::write(&file, "- [x] one\n").unwrap();
    sync.request(pending_sync("- [x] one\n", "Check \"one\""));
    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);
    assert_eq!(
        git_stdout(&remote, &["show", "HEAD:docs/list.md"]),
        "- [x] one"
    );
}

#[test]
fn requests_inside_the_quiet_period_become_one_commit() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let mut sync = GitSync::detect(&file).unwrap();
    sync.set_delay(Duration::from_secs(60));

    fs::write(&file, "- [x] one\n").unwrap();
    sync.request(pending_sync("- [x] one\n", "Check \"one\""));
    fs::write(&file, "- [/] one\n").unwrap();
    sync.request(pending_sync("- [/] one\n", "Start \"one\""));
    assert!(
        !sync.is_busy() && sync.is_waiting(),
        "held for the quiet period"
    );

    sync.tick(Instant::now() + Duration::from_secs(30));
    assert!(sync.is_waiting(), "the second request restarted the timer");
    sync.tick(Instant::now() + Duration::from_secs(61));
    assert!(sync.is_busy());

    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);
    assert_eq!(
        git_stdout(&remote, &["log", "-1", "--format=%B", "main"]),
        "tracked.md: 2 changes\n\n- Check \"one\"\n- Start \"one\"\n\nSynced-by: markcheck"
    );
    assert_eq!(git_stdout(&remote, &["rev-list", "--count", "main"]), "2");
}

#[test]
fn a_cancelled_request_never_runs() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let mut sync = GitSync::detect(&file).unwrap();
    sync.set_delay(Duration::from_secs(60));

    fs::write(&file, "- [x] one\n").unwrap();
    sync.request(pending_sync("- [x] one\n", "Check \"one\""));
    assert!(sync.cancel_queued(), "a waiting request was dropped");
    assert!(!sync.cancel_queued(), "nothing is left to drop");
    assert!(!sync.is_waiting());

    sync.tick(Instant::now() + Duration::from_secs(61));
    sync.flush();
    assert!(!sync.is_busy(), "no sync runs for the dropped request");
    assert_eq!(git_stdout(&remote, &["rev-list", "--count", "main"]), "1");
}

#[test]
fn a_delay_too_long_to_schedule_waits_for_the_flush() {
    let (work, _remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let mut sync = GitSync::detect(&file).unwrap();
    sync.set_delay(Duration::MAX);

    fs::write(&file, "- [x] one\n").unwrap();
    sync.request(pending_sync("- [x] one\n", "Check \"one\""));
    sync.tick(Instant::now() + Duration::from_secs(86_400 * 365));
    assert!(sync.is_waiting() && !sync.is_busy(), "never due on its own");
    sync.flush();
    assert!(sync.is_busy(), "the flush still sends it");
    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);
}

#[test]
fn an_immediate_request_or_a_flush_skips_the_quiet_period() {
    let (work, _remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let mut sync = GitSync::detect(&file).unwrap();
    sync.set_delay(Duration::from_secs(60));

    fs::write(&file, "- [x] one\n").unwrap();
    let mut edited = pending_sync("- [x] one\n", "Edited in vi");
    edited.immediate = true;
    sync.request(edited);
    assert!(sync.is_busy(), "an editor's edit doesn't wait");
    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);

    fs::write(&file, "- [ ] one\n").unwrap();
    sync.request(pending_sync("- [ ] one\n", "Uncheck \"one\""));
    assert!(sync.is_waiting());
    sync.flush();
    assert!(sync.is_busy(), "quitting flushes the waiting change");
    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);
    assert_eq!(
        git_stdout(&work, &["log", "-1", "--format=%s"]),
        "tracked.md: Uncheck \"one\""
    );
}

#[test]
fn parse_git_version_reads_release_and_vendor_builds() {
    assert_eq!(
        parse_git_version("git version 2.39.2\n"),
        Some(((2, 39), "2.39.2"))
    );
    assert_eq!(
        parse_git_version("git version 2.40.0.windows.1\n"),
        Some(((2, 40), "2.40.0.windows.1"))
    );
    assert_eq!(
        parse_git_version("git version 2.39.3 (Apple Git-146)\n"),
        Some(((2, 39), "2.39.3"))
    );
    assert_eq!(
        parse_git_version("git version 2.46.0.rc0"),
        Some(((2, 46), "2.46.0.rc0"))
    );
    assert_eq!(parse_git_version("hub version 2.14.2"), None);
    assert_eq!(parse_git_version("git version two"), None);
    assert_eq!(parse_git_version("git version 2"), None);
    assert_eq!(parse_git_version(""), None);
}

#[test]
fn check_git_version_refuses_a_git_without_the_needed_format_atoms() {
    assert_eq!(
        check_git_version("git version 2.17.1\n"),
        Err("git 2.22 or newer is needed (found 2.17.1)".to_string())
    );
    assert_eq!(
        check_git_version("git version 1.8.3.1\n"),
        Err("git 2.22 or newer is needed (found 1.8.3.1)".to_string())
    );
    assert_eq!(
        check_git_version("git version 2.21.9\n"),
        Err("git 2.22 or newer is needed (found 2.21.9)".to_string())
    );
    assert_eq!(check_git_version("git version 2.22.0\n"), Ok(()));
    assert_eq!(check_git_version("git version 3.0.0\n"), Ok(()));
    assert_eq!(check_git_version("some wrapper banner\n"), Ok(()));
}

fn own_toggle(content: &str, epoch: u64, base: &str) -> PendingSync {
    PendingSync {
        previous_content: Some(base.to_string()),
        base: SyncBase::Own {
            epoch,
            content: Some(base.to_string()),
        },
        ..pending_sync(content, "Check \"one\"")
    }
}

const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Every outcome until the worker is idle, failing the test with what was
/// seen if it is still busy after [`SETTLE_TIMEOUT`].
fn settle(sync: &mut GitSync) -> Vec<SyncOutcome> {
    let outcomes = sync.wait_for_outcomes(SETTLE_TIMEOUT);
    assert!(
        !sync.busy,
        "sync still busy after {SETTLE_TIMEOUT:?}; outcomes so far: {outcomes:?}"
    );
    outcomes
}

fn failing_pre_commit_hook(work: &Path) -> PathBuf {
    let hooks = work.join(".git").join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    hook
}

#[test]
fn toggling_again_after_a_refused_sync_syncs_markcheck_s_own_changes() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let hook = failing_pre_commit_hook(&work);
    let mut sync = GitSync::detect(&file).unwrap();

    fs::write(&file, "- [x] one\n").unwrap();
    sync.request(own_toggle("- [x] one\n", 1, "- [ ] one\n"));
    let outcomes = settle(&mut sync);
    assert!(
        matches!(outcomes.as_slice(), [SyncOutcome::Failed(_)]),
        "{outcomes:?}"
    );

    fs::remove_file(&hook).unwrap();
    fs::write(&file, "- [x] one\n- [x] two\n").unwrap();
    sync.request(own_toggle("- [x] one\n- [x] two\n", 1, "- [ ] one\n"));
    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);
    assert_eq!(
        git_stdout(&remote, &["show", "main:tracked.md"]),
        "- [x] one\n- [x] two"
    );
}

#[test]
fn a_toggle_queued_behind_a_landing_sync_is_checked_against_what_landed() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let mut sync = GitSync::detect(&file).unwrap();

    fs::write(&file, "- [x] one\n").unwrap();
    sync.request(own_toggle("- [x] one\n", 1, "- [ ] one\n"));
    assert!(sync.busy);
    fs::write(&file, "- [x] one\n- [x] two\n").unwrap();
    sync.request(own_toggle("- [x] one\n- [x] two\n", 1, "- [ ] one\n"));

    assert_eq!(
        settle(&mut sync),
        vec![SyncOutcome::Synced, SyncOutcome::Synced]
    );
    assert_eq!(
        git_stdout(&remote, &["show", "main:tracked.md"]),
        "- [x] one\n- [x] two"
    );
}

#[test]
fn a_toggle_after_an_outside_reload_does_not_publish_the_outside_edit() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let mut sync = GitSync::detect(&file).unwrap();

    fs::write(&file, "- [x] one\n").unwrap();
    sync.request(own_toggle("- [x] one\n", 1, "- [ ] one\n"));
    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);

    let reloaded = "- [x] one\n- [ ] DRAFT\n";
    fs::write(&file, "- [ ] one\n- [ ] DRAFT\n").unwrap();
    sync.request(own_toggle("- [ ] one\n- [ ] DRAFT\n", 2, reloaded));
    let outcomes = settle(&mut sync);
    assert!(
        matches!(outcomes.as_slice(), [SyncOutcome::Failed(msg)] if msg.contains("didn't make")),
        "{outcomes:?}"
    );
    assert_eq!(
        git_stdout(&remote, &["show", "main:tracked.md"]),
        "- [x] one"
    );
}

#[test]
fn a_toggle_after_a_saved_edit_that_failed_to_sync_publishes_the_edit() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let hook = failing_pre_commit_hook(&work);
    let mut sync = GitSync::detect(&file).unwrap();

    let edited = "- [ ] one\n- [ ] added in the editor\n";
    fs::write(&file, edited).unwrap();
    sync.request(PendingSync {
        immediate: true,
        base: SyncBase::Edited { epoch: 4 },
        ..pending_sync(edited, "Edited in vi")
    });
    let outcomes = settle(&mut sync);
    assert!(
        matches!(outcomes.as_slice(), [SyncOutcome::Failed(_)]),
        "{outcomes:?}"
    );

    fs::remove_file(&hook).unwrap();
    let toggled = "- [x] one\n- [ ] added in the editor\n";
    fs::write(&file, toggled).unwrap();
    sync.request(own_toggle(toggled, 4, edited));
    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);
    assert_eq!(
        git_stdout(&remote, &["show", "main:tracked.md"]),
        toggled.trim_end()
    );
}
#[test]
fn identity_warning_is_silent_with_a_configured_identity() {
    let work = init_repo_without_remote();
    let sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    assert_eq!(sync.identity_warning(), None);
}
#[test]
fn identity_warning_says_commits_are_refused_without_a_name() {
    let work = init_repo_without_remote();
    run(&work, &["config", "--unset", "user.name"]);
    let sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    assert_eq!(sync.identity_warning().as_deref(), Some(IDENTITY_REFUSED));
}
#[test]
fn identity_warning_says_commits_are_refused_without_an_email() {
    let work = init_repo_without_remote();
    run(&work, &["config", "--unset", "user.email"]);
    let sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    assert_eq!(sync.identity_warning().as_deref(), Some(IDENTITY_REFUSED));
}
const IDENTITY_REFUSED: &str = "Git sync: no git identity set — commits will be refused; \
                                set git config user.name and user.email";

#[test]
fn a_remote_check_reports_a_remote_that_moved_on_and_keeps_any_armed_retry() {
    let (work, remote) = init_repo_with_remote();
    push_from_another_clone(&remote);
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    sync.retry = Some(("abc".into(), Instant::now()));

    sync.request_remote_check();
    assert!(sync.is_busy());
    let outcomes = sync.wait_for_outcomes(Duration::from_secs(30));

    assert!(
        matches!(outcomes.as_slice(), [SyncOutcome::RemoteAhead(_)]),
        "{outcomes:?}"
    );
    assert!(sync.retry.is_some(), "a remote check settles no push");
}

#[test]
fn a_remote_check_waits_for_a_running_sync() {
    let work = init_repo_without_remote();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    sync.busy = true;

    sync.request_remote_check();

    assert!(sync.receiver.try_recv().is_err(), "nothing was started");
}

#[test]
fn a_toggle_retried_after_a_pull_publishes_on_top_of_the_pulled_commit() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    fs::write(&file, "## S\n\n- [ ] one\n- [ ] two\n").unwrap();
    run(&work, &["commit", "-q", "-am", "two steps"]);
    run(&work, &["push", "-q"]);
    let mut state =
        crate::model::AppState::new(crate::parser::parse_document(file.clone()).unwrap());

    fs::write(&file, "## S\n\n- [ ] one\n- [x] two\n").unwrap();
    run(&work, &["commit", "-q", "-am", "pulled: two done"]);
    run(&work, &["push", "-q"]);
    state.toggle_current();

    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "## S\n\n- [x] one\n- [x] two\n"
    );
    let pending = state
        .effects
        .iter()
        .find_map(|effect| match effect {
            crate::effect::Effect::GitSync(sync) => Some(sync.clone()),
            _ => None,
        })
        .expect("the retried toggle queues a sync");
    let mut sync = GitSync::detect(&file).unwrap();
    sync.request(pending);

    assert_eq!(settle(&mut sync), vec![SyncOutcome::Synced]);
    assert_eq!(
        git_stdout(&remote, &["log", "-1", "--format=%s", "main"]),
        "tracked.md: Check \"one\""
    );
    assert_eq!(
        git_stdout(&remote, &["show", "main:tracked.md"]),
        "## S\n\n- [x] one\n- [x] two"
    );
}
