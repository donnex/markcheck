//! Read-only git queries. Several return three-way answers because "could
//! not tell" must not be confused with "no".

use std::path::{Path, PathBuf};

use super::process::{
    PLUMBING_TIMEOUT, PUSH_TIMEOUT, command_error, git_command, literal_pathspec_command,
    run_with_timeout, run_with_timeout_and_stdin,
};

/// Fails open (`true`), so a push is attempted and its real failure
/// reported rather than the sync going quiet.
pub(super) fn ahead_of_upstream(repo_root: &Path, tip: &str) -> bool {
    commits_ahead_of_upstream(repo_root, tip).unwrap_or(1) > 0
}
/// `None` when it can't be answered. Unlike `ahead_of_upstream`, the
/// startup catch-up push acts only on a positively-known answer.
pub(super) fn commits_ahead_of_upstream(repo_root: &Path, tip: &str) -> Option<u64> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--count", &format!("@{{u}}..{tip}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}
/// Whether a branch exists on the remote, asked of the remote itself: the
/// local tracking ref survives an upstream deletion, and pushing an explicit
/// refspec then silently recreates the branch.
pub(super) enum RemoteBranch {
    /// The SHA becomes the lease the push is made against.
    Present(String),
    Absent,
    Unknown,
}
/// `location` is a remote name or URL.
pub(super) fn remote_branch_state(
    repo_dir: &Path,
    location: &str,
    branch_ref: &str,
) -> RemoteBranch {
    let mut cmd = git_command(repo_dir);
    cmd.args(["ls-remote", "--heads", location, branch_ref]);
    match run_with_timeout(cmd, PUSH_TIMEOUT) {
        Ok(output) if output.status.success() => {
            // No output means the branch is not there.
            let stdout = String::from_utf8_lossy(&output.stdout);
            match stdout.split_whitespace().next() {
                Some(sha) => RemoteBranch::Present(sha.to_string()),
                None => RemoteBranch::Absent,
            }
        }
        _ => RemoteBranch::Unknown,
    }
}

/// `false` when unanswerable (e.g. `ancestor` was never fetched), so a
/// fast-forward is never assumed.
pub(super) fn is_ancestor(repo_root: &Path, ancestor: &str, descendant: &str) -> bool {
    let mut cmd = git_command(repo_root);
    cmd.args(["merge-base", "--is-ancestor", ancestor, descendant]);
    run_with_timeout(cmd, PLUMBING_TIMEOUT).is_ok_and(|out| out.status.success())
}
/// `Ok(None)` when no upstream resolves (exit 1); any other failure is
/// `Err`.
pub(super) fn resolve_upstream(repo_root: &Path) -> Result<Option<String>, String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "--verify", "-q", "@{u}"]);
    match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Ok(output) if output.status.code() == Some(1) => Ok(None),
        Ok(output) => Err(command_error("git rev-parse", &output)),
        Err(err) => Err(format!("git rev-parse failed: {err}")),
    }
}
/// Read from `branch.<name>.remote`/`.merge`, since a remote name can't be
/// split reliably from `@{u}` when the branch name contains `/`.
pub(super) fn upstream_parts(repo_root: &Path) -> Option<(String, String)> {
    let branch_ref = current_branch_ref(repo_root)?;
    let branch = branch_ref.strip_prefix("refs/heads/")?;
    let remote = git_config(repo_root, &format!("branch.{branch}.remote"))?;
    let merge_ref = git_config(repo_root, &format!("branch.{branch}.merge"))?;
    Some((remote, merge_ref))
}
/// Whether a plain `git push` on this branch would publish to `remote` under
/// the upstream's own name. `@{push}` applies `branch.<name>.pushRemote`,
/// `remote.pushDefault`, `push.default` and push refspecs exactly as `git push`
/// does, and fails to resolve wherever `git push` would refuse.
pub(super) fn push_goes_to_upstream(repo_root: &Path, remote: &str) -> bool {
    let Some(branch_ref) = current_branch_ref(repo_root) else {
        return false;
    };
    let Some(push_remote) = git_stdout_line(
        repo_root,
        &["for-each-ref", "--format=%(push:remotename)", &branch_ref],
    ) else {
        return false;
    };
    let tracking =
        |spec: &str| git_stdout_line(repo_root, &["rev-parse", "--symbolic-full-name", spec]);
    match (tracking("@{push}"), tracking("@{u}")) {
        (Some(push), Some(upstream)) => {
            push_remote == remote && !push.is_empty() && push == upstream
        }
        _ => false,
    }
}
/// The URL `git push <remote>` sends to: `remote.<name>.pushurl` when set,
/// which can name a different repository than the fetch `url`.
pub(super) fn push_url(repo_root: &Path, remote: &str) -> Option<String> {
    git_stdout_line(repo_root, &["remote", "get-url", "--push", remote])
}
fn git_stdout_line(repo_root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(args);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
fn git_config(repo_root: &Path, key: &str) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["config", "--get", key]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// The blob SHA the real index holds for `relpath`.
pub(super) fn index_blob(repo_root: &Path, relpath: &str) -> Option<String> {
    let mut cmd = literal_pathspec_command(repo_root);
    cmd.args(["ls-files", "--stage", "-z", "--"]).arg(relpath);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    let record = output.stdout.split(|&b| b == 0).find(|r| !r.is_empty())?;
    let record = String::from_utf8_lossy(record);
    let (info, _path) = record.split_once('\t')?;
    info.split_whitespace().nth(1).map(str::to_string)
}
/// `None` when the listing is empty, which must not be read as a root
/// commit: undoing a root commit deletes the branch ref.
pub(super) fn parse_first_parent(listing: &str) -> Option<Option<String>> {
    let mut fields = listing.split_whitespace();
    fields.next()?;
    Some(fields.next().map(str::to_string))
}
/// Works before the first commit, since `git init` already points `HEAD` at
/// the default branch.
pub(super) fn current_branch_ref(repo_root: &Path) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["symbolic-ref", "-q", "HEAD"]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// Absolute even when git reports a relative path.
pub(super) fn git_dir(repo_root: &Path) -> Option<PathBuf> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "--git-dir"]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}
pub(super) fn index_entry(
    repo_dir: &Path,
    file_path: &Path,
) -> Result<Option<(String, String)>, String> {
    let mut cmd = literal_pathspec_command(repo_dir);
    cmd.args(["ls-files", "--stage", "--full-name", "-z", "--"])
        .arg(file_path);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git ls-files failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git ls-files", &output));
    }
    // `-z` keeps paths with newlines or quotable characters intact. No record
    // means untracked, which is not an error.
    let mut records = output.stdout.split(|&b| b == 0).filter(|r| !r.is_empty());
    let Some(record) = records.next() else {
        return Ok(None);
    };
    // More than one record means the pathspec matched other files (#70).
    if records.next().is_some() {
        return Err(format!(
            "git ls-files: {} matched more than one tracked file",
            file_path.display()
        ));
    }
    let record = String::from_utf8_lossy(record);
    let (info, path) = record
        .split_once('\t')
        .ok_or_else(|| format!("git ls-files: unexpected output {record:?}"))?;
    let mode = info
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("git ls-files: unexpected output {record:?}"))?;
    Ok(Some((mode.to_string(), path.to_string())))
}
pub(super) fn blob_bytes_at(repo_root: &Path, commit: &str, relpath: &str) -> Option<Vec<u8>> {
    let mut cmd = git_command(repo_root);
    cmd.args(["show", &format!("{commit}:{relpath}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output.status.success().then_some(output.stdout)
}
pub(super) fn blob_at(repo_root: &Path, commit: &str, relpath: &str) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", &format!("{commit}:{relpath}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// Exactly one parent equal to `parent`, or a root commit when `parent` is
/// `None`. Fails closed.
pub(super) fn descends_directly_from(
    repo_root: &Path,
    commit: &str,
    parent: &Option<String>,
) -> bool {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--parents", "-n", "1", commit]);
    let Ok(output) = run_with_timeout(cmd, PLUMBING_TIMEOUT) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(line) = stdout.lines().next() else {
        return false;
    };
    let parents: Vec<&str> = line.split_whitespace().skip(1).collect();
    match parent {
        Some(expected) => parents == [expected.as_str()],
        None => parents.is_empty(),
    }
}
pub(super) fn hash_object(
    repo_root: &Path,
    relpath: &str,
    content: &str,
) -> Result<String, String> {
    hash_object_inner(repo_root, relpath, content, true)
}
/// Computed without writing, to avoid littering loose blobs.
pub(super) fn blob_sha_for(repo_root: &Path, relpath: &str, content: &str) -> Option<String> {
    hash_object_inner(repo_root, relpath, content, false).ok()
}
/// `--path` applies the path's clean filter (`core.autocrlf`,
/// `.gitattributes`) exactly as `git add` would.
fn hash_object_inner(
    repo_root: &Path,
    relpath: &str,
    content: &str,
    write: bool,
) -> Result<String, String> {
    let mut cmd = git_command(repo_root);
    cmd.arg("hash-object");
    if write {
        cmd.arg("-w");
    }
    cmd.args(["--path", relpath, "--stdin"]);
    let output = run_with_timeout_and_stdin(cmd, PLUMBING_TIMEOUT, content)
        .map_err(|err| format!("git hash-object failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git hash-object", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// `None` is ambiguous between "no commits yet" and a failed check; see
/// `resolve_parent` where that matters.
pub(super) fn current_head(repo_root: &Path) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "HEAD"]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// Exit 1 from `rev-parse --verify -q HEAD` means no commits yet; any other
/// failure is `Err`. Treating a transient failure as "no commits" would build
/// a root commit containing only the checklist.
pub(super) fn resolve_parent(repo_root: &Path) -> Result<Option<String>, String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "--verify", "-q", "HEAD"]);
    match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Ok(output) if output.status.code() == Some(1) => Ok(None),
        Ok(output) => Err(command_error("git rev-parse", &output)),
        Err(err) => Err(format!("git rev-parse failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn index_entry_matches_glob_characters_literally() {
        let (work, _remote) = init_repo_with_remote_named("notes[1].md");
        fs::write(work.join("notes1.md"), "x\n").unwrap();
        run(&work, &["add", "notes1.md"]);

        assert_eq!(
            index_entry(&work, &work.join("notes[1].md")),
            Ok(Some(("100644".to_string(), "notes[1].md".to_string())))
        );
        assert_ne!(
            index_blob(&work, "notes[1].md"),
            index_blob(&work, "notes1.md")
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn index_entry_reports_failure_outside_a_git_repo() {
        let dir = unique_dir("not-a-repo-index-entry");
        fs::create_dir_all(&dir).unwrap();
        assert!(index_entry(&dir, &dir.join("nope.md")).is_err());
        fs::remove_dir_all(&dir).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn hash_object_reports_failure_outside_a_git_repo() {
        let dir = unique_dir("not-a-repo-hash-object");
        fs::create_dir_all(&dir).unwrap();
        assert!(hash_object(&dir, "tracked.md", "content").is_err());
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn descends_directly_from_matches_only_a_direct_child() {
        let work = init_repo_without_remote();
        let root = current_head(&work).unwrap();
        fs::write(work.join("other.md"), "second\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "second"]);
        let second = current_head(&work).unwrap();
        fs::write(work.join("third.md"), "third\n").unwrap();
        run(&work, &["add", "third.md"]);
        run(&work, &["commit", "-q", "-m", "third"]);
        let third = current_head(&work).unwrap();

        assert!(descends_directly_from(&work, &second, &Some(root.clone())));
        assert!(
            !descends_directly_from(&work, &third, &Some(root.clone())),
            "a grandchild is not a direct child"
        );
        assert!(
            descends_directly_from(&work, &root, &None),
            "a root commit has no parents, matching parent: None"
        );
        assert!(
            !descends_directly_from(&work, &second, &None),
            "a commit with a parent must not match the root-commit case"
        );
        assert!(
            !descends_directly_from(&work, "not-a-commit", &Some(root)),
            "fails closed when git can't answer"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn git_dir_resolves_to_an_absolute_path() {
        let work = init_repo_without_remote();
        let dir = git_dir(&work).unwrap();
        assert!(dir.is_absolute());
        assert!(dir.ends_with(".git"));
        fs::remove_dir_all(work.parent().unwrap()).ok();
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
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn git_dir_none_outside_a_repo() {
        let dir = unique_dir("not-a-repo-git-dir");
        fs::create_dir_all(&dir).unwrap();
        assert!(git_dir(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn resolve_parent_is_ok_none_for_a_genuinely_empty_repo() {
        let work = unique_dir("repo-empty-resolve-parent").join("work");
        fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);

        assert_eq!(resolve_parent(&work), Ok(None));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn resolve_parent_is_ok_some_for_a_normal_repo() {
        let work = init_repo_without_remote();
        let expected = current_head(&work).unwrap();

        assert_eq!(resolve_parent(&work), Ok(Some(expected)));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn resolve_parent_is_err_outside_a_repo() {
        let dir = unique_dir("not-a-repo-resolve-parent");
        fs::create_dir_all(&dir).unwrap();
        assert!(resolve_parent(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
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
}
