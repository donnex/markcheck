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
    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    fs::remove_dir_all(work.parent().unwrap()).ok();
}

#[test]
fn detect_says_why_outside_a_repo() {
    let dir = unique_dir("not-a-repo");
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        GitSync::detect(&dir.join("file.md")).err().as_deref(),
        Some("the file is not in a git repository")
    );
    fs::remove_dir_all(&dir).ok();
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
    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut outcomes = Vec::new();
    loop {
        if let Some(outcome) = sync.poll() {
            outcomes.push(outcome);
        }
        if !sync.busy && !outcomes.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // "second" is always overwritten by "third" before it runs.
    assert!(
        outcomes.len() <= 2,
        "at most 2 sync attempts, never one per request: {outcomes:?}"
    );
    assert!(!sync.busy, "must settle back to idle, not stuck busy");
    assert!(sync.pending.is_none(), "no request left dangling");
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

    fs::remove_dir_all(work.parent().unwrap()).ok();
    fs::remove_dir_all(remote.parent().unwrap()).ok();
}

#[test]
fn retry_push_if_due_noops_when_nothing_is_pending() {
    let work = init_repo_without_remote();
    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    sync.retry_push_if_due(Instant::now());
    assert!(!sync.busy, "nothing pending, nothing to retry");
    fs::remove_dir_all(work.parent().unwrap()).ok();
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
    fs::remove_dir_all(work.parent().unwrap()).ok();
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
    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut outcome = None;
    while Instant::now() < deadline {
        if let Some(o) = sync.poll() {
            outcome = Some(o);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        matches!(outcome, Some(SyncOutcome::CommittedNotPushed { .. })),
        "still no remote to push to, so this retry fails the same way: {outcome:?}"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut outcome = None;
    while Instant::now() < deadline {
        if let Some(o) = sync.poll() {
            outcome = Some(o);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(outcome, Some(SyncOutcome::RetryAbandoned), "{outcome:?}");
    assert!(
        sync.retry.is_none(),
        "the stale retry must be cleared, not left armed forever"
    );

    sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL * 2);
    assert!(!sync.busy, "nothing left to retry");

    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    fs::remove_dir_all(work.parent().unwrap()).ok();
}

#[test]
fn request_and_poll_roundtrip_reports_synced() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();

    let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    sync.request(pending_sync("- [x] one\n", "Check \"one\""));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut result = None;
    while Instant::now() < deadline {
        if let Some(outcome) = sync.poll() {
            result = Some(outcome);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(result, Some(SyncOutcome::Synced));

    fs::remove_dir_all(work.parent().unwrap()).ok();
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
        sync.pending.as_ref().map(|p| p.description.as_str()),
        Some("second")
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut outcomes = Vec::new();
    while Instant::now() < deadline && outcomes.len() < 2 {
        if let Some(outcome) = sync.poll() {
            outcomes.push(outcome);
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(outcomes, vec![SyncOutcome::Synced, SyncOutcome::Skipped]);

    let log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "tracked.md: first"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    fs::remove_dir_all(work.parent().unwrap()).ok();
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
}

#[test]
fn exit_report_stays_quiet_when_the_final_sync_landed_or_had_nothing_to_do() {
    assert_eq!(exit_report(None, false), None);
    assert_eq!(exit_report(Some(&SyncOutcome::Synced), false), None);
    assert_eq!(exit_report(Some(&SyncOutcome::Skipped), false), None);
    assert_eq!(exit_report(Some(&SyncOutcome::RetryAbandoned), false), None);
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
fn exit_report_flags_a_sync_cut_off_by_the_exit_wait() {
    for last in [None, Some(&SyncOutcome::Synced)] {
        assert!(exit_report(last, true).is_some_and(|m| m.contains("still running")));
    }
}

#[test]
fn sweep_interrupted_syncs_removes_a_dead_sync_s_temp_index() {
    let work = init_repo_without_remote();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let git_dir = work.join(".git");
    fs::write(git_dir.join(format!("markcheck-index-{dead}-1f")), "").unwrap();
    fs::write(git_dir.join(format!("markcheck-index-{dead}-1f.lock")), "").unwrap();

    let sync = GitSync::detect(&work.join("tracked.md")).unwrap();
    assert_eq!(sync.sweep_interrupted_syncs(), 2);
    assert_eq!(sync.sweep_interrupted_syncs(), 0);
    fs::remove_dir_all(work.parent().unwrap()).ok();
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

    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(started.exists(), "the hook never started");
    assert!(sync.abandon(false), "the commit's git was running");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut outcome = None;
    while outcome.is_none() && Instant::now() < deadline {
        outcome = sync.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        matches!(outcome, Some(SyncOutcome::Failed(_))),
        "the abandoned sync ends instead of waiting out its hook: {outcome:?}"
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

    sync.request(pending_sync("- [ ] one\n", "Uncheck"));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut outcome = None;
    while outcome.is_none() && Instant::now() < deadline {
        outcome = sync.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        matches!(&outcome, Some(SyncOutcome::Failed(message)) if message.contains("abandoned")),
        "no git may start after abandoning: {outcome:?}"
    );
    fs::remove_dir_all(work.parent().unwrap()).ok();
}
