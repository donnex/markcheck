use super::super::test_support::*;
use super::*;
use std::fs;

/// markcheck must stage the blob git itself would stage.
#[test]
fn a_staged_blob_matches_what_git_add_would_have_stored() {
    let work = init_repo_with_crlf_filter();
    let content = "- [x] one\r\n- [ ] two\r\n";

    let ours = hash_object(&work, "tracked.md", content).unwrap();

    fs::write(work.join("tracked.md"), content).unwrap();
    run(&work, &["add", "tracked.md"]);
    let theirs = index_blob(&work, "tracked.md").unwrap();

    assert_eq!(
        ours, theirs,
        "the blob markcheck stages must be the one `git add` stores"
    );
}
#[test]
fn index_entry_refuses_a_pathspec_that_matches_more_than_one_file() {
    // A directory is the one pathspec that still matches several files
    // literally.
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("other.md"), "x\n").unwrap();
    run(&work, &["add", "other.md"]);

    let err = index_entry(&work, &work).unwrap_err();
    assert!(err.contains("more than one tracked file"), "{err}");
}
#[test]
fn index_entry_matches_glob_characters_literally() {
    let (work, _remote) = init_repo_with_remote_named("notes[1].md");
    fs::write(work.join("notes1.md"), "x\n").unwrap();
    run(&work, &["add", "notes1.md"]);

    let entry = index_entry(&work, &work.join("notes[1].md"))
        .unwrap()
        .unwrap();
    assert_eq!(
        (entry.mode.as_str(), entry.path.as_str()),
        ("100644", "notes[1].md")
    );
    assert_eq!(Some(entry.blob), index_blob(&work, "notes[1].md"));
    assert_ne!(
        index_blob(&work, "notes[1].md"),
        index_blob(&work, "notes1.md")
    );
}
#[test]
fn index_entry_refuses_a_file_name_that_is_not_utf8() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let (work, _remote) = init_repo_with_remote();
    let file = work.join(OsStr::from_bytes(b"list-\xff.md"));
    fs::write(&file, "- [ ] one\n").unwrap();
    let status = crate::test_support::git()
        .current_dir(&work)
        .arg("add")
        .arg(&file)
        .status()
        .unwrap();
    assert!(status.success());

    let err = index_entry(&work, &file).unwrap_err();
    assert!(err.contains("not a UTF-8 file name"), "{err}");
}
#[test]
fn index_entry_reports_failure_outside_a_git_repo() {
    let dir = unique_dir("not-a-repo-index-entry");
    fs::create_dir_all(&dir).unwrap();
    assert!(index_entry(&dir, &dir.join("nope.md")).is_err());
}
#[test]
fn index_entry_reports_an_absent_path_as_untracked_not_an_error() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("untracked.md"), "- [ ] new\n").unwrap();

    assert_eq!(index_entry(&work, &work.join("untracked.md")), Ok(None));
    assert!(matches!(
        index_entry(&work, &work.join("tracked.md")),
        Ok(Some(_))
    ));
}
#[test]
fn hash_object_reports_failure_outside_a_git_repo() {
    let dir = unique_dir("not-a-repo-hash-object");
    fs::create_dir_all(&dir).unwrap();
    assert!(hash_object(&dir, "tracked.md", "content").is_err());
}
#[test]
fn git_dir_resolves_to_an_absolute_path() {
    let work = init_repo_without_remote();
    let dir = git_dir(&work).unwrap();
    assert!(dir.is_absolute());
    assert!(dir.ends_with(".git"));
}
#[test]
fn git_dir_resolves_correctly_from_a_subdirectory() {
    // From a subdirectory `--git-dir` is already absolute.
    let work = init_repo_without_remote();
    let sub = work.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let dir = git_dir(&sub).unwrap();
    assert!(dir.is_absolute());
    assert!(dir.ends_with(".git"));
}
#[test]
fn git_dir_none_outside_a_repo() {
    let dir = unique_dir("not-a-repo-git-dir");
    fs::create_dir_all(&dir).unwrap();
    assert!(git_dir(&dir).is_none());
}
#[test]
fn resolve_parent_is_ok_none_for_a_genuinely_empty_repo() {
    let work = unique_dir("repo-empty-resolve-parent").join("work");
    fs::create_dir_all(&work).unwrap();
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);

    assert_eq!(resolve_parent(&work), Ok(None));
}
#[test]
fn resolve_parent_is_ok_some_for_a_normal_repo() {
    let work = init_repo_without_remote();
    let expected = current_head(&work).unwrap();

    assert_eq!(resolve_parent(&work), Ok(Some(expected)));
}
#[test]
fn resolve_parent_is_err_outside_a_repo() {
    let dir = unique_dir("not-a-repo-resolve-parent");
    fs::create_dir_all(&dir).unwrap();
    assert!(resolve_parent(&dir).is_err());
}
#[test]
fn remote_branch_state_ignores_refs_that_only_end_in_the_branch_ref() {
    let (work, remote) = init_repo_with_remote();
    let main_sha = current_head(&work).unwrap();
    run(&work, &["checkout", "-q", "-b", "side"]);
    let decoy = commit_unrelated(&work, "decoy.md");
    run(
        &work,
        &[
            "push",
            "-q",
            &remote.to_string_lossy(),
            &format!("{decoy}:refs/heads/a/refs/heads/main"),
        ],
    );

    assert!(matches!(
        remote_branch_state(&work, "origin", "refs/heads/main"),
        RemoteBranch::Present(sha) if sha == main_sha
    ));

    run(&work, &["push", "-q", "origin", "side"]);
    run(&remote, &["symbolic-ref", "HEAD", "refs/heads/side"]);
    run(&work, &["push", "-q", "origin", "--delete", "main"]);
    assert!(matches!(
        remote_branch_state(&work, "origin", "refs/heads/main"),
        RemoteBranch::Absent
    ));
}
#[test]
fn remote_branch_state_is_unknown_when_the_remote_is_unreachable() {
    let work = init_repo_without_remote();
    assert!(matches!(
        remote_branch_state(&work, "no-such-remote", "refs/heads/main"),
        RemoteBranch::Unknown
    ));
}
#[test]
fn parse_first_parent_tells_a_root_commit_from_unreadable_output() {
    assert_eq!(
        parse_first_parent(""),
        None,
        "no output at all must not read as a root commit"
    );
    assert_eq!(parse_first_parent("   \n"), None, "nor whitespace only");
    assert_eq!(
        parse_first_parent("abc123\n"),
        Some(None),
        "a lone commit really is a root commit"
    );
    assert_eq!(
        parse_first_parent("abc123 def456\n"),
        Some(Some("def456".to_string()))
    );
    assert_eq!(
        parse_first_parent("abc123 def456 789abc\n"),
        Some(Some("def456".to_string())),
        "a merge's *first* parent is the one to rewind to"
    );
}
