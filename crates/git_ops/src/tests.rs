mod history;

use super::*;
use std::fs;

fn repo(dir: &Path) -> LocalRepo {
    LocalRepo {
        dir: dir.into(),
        trusted: true,
    }
}

fn run(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let output = smol::block_on(git(&repo(repo_dir), args, &[]))?;
    Ok(String::from_utf8(output)?.trim().to_string())
}

fn write(repo_dir: &Path, name: &str, contents: &str) -> Result<()> {
    Ok(fs::write(repo_dir.join(name), contents)?)
}

fn read(repo_dir: &Path, name: &str) -> Result<String> {
    Ok(fs::read_to_string(repo_dir.join(name))?)
}

fn configure_identity(repo_dir: &Path) -> Result<()> {
    run(repo_dir, &["config", "user.email", "test@example.com"])?;
    run(repo_dir, &["config", "user.name", "test"])?;
    run(repo_dir, &["config", "commit.gpgsign", "false"])?;
    Ok(())
}

// Commit one adds a.txt, b.txt and root.txt. Commit two edits a.txt and b.txt.
fn repo_with_two_commits() -> Result<(tempfile::TempDir, String)> {
    let temp = tempfile::tempdir()?;
    let repo_dir = temp.path();
    run(repo_dir, &["init", "-q"])?;
    configure_identity(repo_dir)?;
    for name in ["a.txt", "b.txt", "root.txt"] {
        write(repo_dir, name, "1\n2\n3\n")?;
    }
    run(repo_dir, &["add", "."])?;
    run(repo_dir, &["commit", "-qm", "one"])?;
    for name in ["a.txt", "b.txt"] {
        write(repo_dir, name, "1\ntwo\n3\n")?;
    }
    run(repo_dir, &["commit", "-qam", "two"])?;
    let sha = run(repo_dir, &["rev-parse", "HEAD"])?;
    Ok((temp, sha))
}

#[test]
fn reverts_and_reapplies_a_single_file() -> Result<()> {
    let (temp, sha) = repo_with_two_commits()?;
    let repo_dir = temp.path();

    smol::block_on(apply_commit_file(
        &repo(repo_dir),
        &sha,
        "a.txt",
        Direction::Revert,
    ))?;
    assert_eq!(read(repo_dir, "a.txt")?, "1\n2\n3\n");
    assert_eq!(read(repo_dir, "b.txt")?, "1\ntwo\n3\n");

    smol::block_on(apply_commit_file(
        &repo(repo_dir),
        &sha,
        "a.txt",
        Direction::Apply,
    ))?;
    assert_eq!(read(repo_dir, "a.txt")?, "1\ntwo\n3\n");
    Ok(())
}

#[test]
fn reverts_file_added_by_root_commit() -> Result<()> {
    let (temp, _) = repo_with_two_commits()?;
    let repo_dir = temp.path();
    let root = run(repo_dir, &["rev-list", "--max-parents=0", "HEAD"])?;

    smol::block_on(apply_commit_file(
        &repo(repo_dir),
        &root,
        "root.txt",
        Direction::Revert,
    ))?;
    assert!(!repo_dir.join("root.txt").exists());
    Ok(())
}

#[test]
fn lists_the_files_a_commit_changed() -> Result<()> {
    let (temp, sha) = repo_with_two_commits()?;

    let files = smol::block_on(commit_files(&repo(temp.path()), &sha))?;
    let files: Vec<_> = files
        .iter()
        .map(|file| (file.status, file.path.as_str()))
        .collect();
    assert_eq!(files, [('M', "a.txt"), ('M', "b.txt")]);
    Ok(())
}

#[test]
fn rejects_file_untouched_by_commit() -> Result<()> {
    let (temp, sha) = repo_with_two_commits()?;

    let message = smol::block_on(apply_commit_file(
        &repo(temp.path()),
        &sha,
        "root.txt",
        Direction::Revert,
    ))
    .err()
    .map(|error| error.to_string())
    .unwrap_or_default();
    assert!(message.contains("does not change"), "{message}");
    Ok(())
}

#[test]
fn conflicting_revert_leaves_conflict_markers() -> Result<()> {
    let (temp, sha) = repo_with_two_commits()?;
    let repo_dir = temp.path();
    write(repo_dir, "a.txt", "1\nTWO\n3\n")?;
    run(repo_dir, &["commit", "-qam", "three"])?;

    let message = smol::block_on(apply_commit_file(
        &repo(repo_dir),
        &sha,
        "a.txt",
        Direction::Revert,
    ))
    .err()
    .map(|error| error.to_string())
    .unwrap_or_default();
    assert!(message.contains("with conflicts"), "{message}");
    assert!(read(repo_dir, "a.txt")?.contains("<<<<<<<"));
    Ok(())
}

fn commit_on_branch(repo_dir: &Path, branch: &str, contents: &str) -> Result<()> {
    run(repo_dir, &["checkout", "-q", branch])?;
    write(repo_dir, "a.txt", contents)?;
    run(repo_dir, &["commit", "-qam", contents])?;
    run(repo_dir, &["checkout", "-q", "-"])?;
    Ok(())
}

// Returns the remote and a clone of it with a local `feature` branch tracking origin.
fn remote_and_clone() -> Result<(tempfile::TempDir, std::path::PathBuf, std::path::PathBuf)> {
    let (remote_temp, _) = repo_with_two_commits()?;
    let remote = remote_temp.path().to_path_buf();
    run(&remote, &["branch", "feature"])?;
    let local = remote.join("clone");
    run(&remote, &["clone", "-q", ".", "clone"])?;
    configure_identity(&local)?;
    run(&local, &["branch", "--track", "feature", "origin/feature"])?;
    Ok((remote_temp, remote, local))
}

#[test]
fn fast_forwards_a_branch_that_is_not_checked_out() -> Result<()> {
    let (_temp, remote, local) = remote_and_clone()?;
    commit_on_branch(&remote, "feature", "1\nremote\n3\n")?;

    smol::block_on(update_branch(
        &repo(&local),
        "feature",
        "origin",
        "feature",
        false,
    ))?;
    assert_eq!(
        run(&local, &["rev-parse", "feature"])?,
        run(&remote, &["rev-parse", "feature"])?
    );
    Ok(())
}

#[test]
fn fetches_only_the_requested_branch() -> Result<()> {
    let (_temp, remote, local) = remote_and_clone()?;
    run(&remote, &["branch", "other"])?;
    run(&local, &["fetch", "-q", "origin"])?;
    let other_before = run(&local, &["rev-parse", "origin/other"])?;
    commit_on_branch(&remote, "feature", "1\nremote\n3\n")?;
    commit_on_branch(&remote, "other", "1\nother\n3\n")?;

    smol::block_on(fetch_branch(&repo(&local), "origin", "feature"))?;
    assert_eq!(
        run(&local, &["rev-parse", "origin/feature"])?,
        run(&remote, &["rev-parse", "feature"])?
    );
    assert_eq!(run(&local, &["rev-parse", "origin/other"])?, other_before);
    Ok(())
}

#[test]
fn refuses_to_update_a_diverged_branch() -> Result<()> {
    let (_temp, remote, local) = remote_and_clone()?;
    commit_on_branch(&remote, "feature", "1\nremote\n3\n")?;
    commit_on_branch(&local, "feature", "1\nlocal\n3\n")?;
    let before = run(&local, &["rev-parse", "feature"])?;

    let result = smol::block_on(update_branch(
        &repo(&local),
        "feature",
        "origin",
        "feature",
        false,
    ));
    assert!(result.is_err());
    assert_eq!(run(&local, &["rev-parse", "feature"])?, before);
    Ok(())
}

// Returns the checked out default branch after creating `topic` one commit behind it.
fn repo_with_topic_branch(topic_contents: &str) -> Result<(tempfile::TempDir, String)> {
    let (temp, _) = repo_with_two_commits()?;
    let repo_dir = temp.path();
    let default_branch = run(repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    run(repo_dir, &["checkout", "-q", "-b", "topic", "HEAD~1"])?;
    write(repo_dir, "a.txt", topic_contents)?;
    run(repo_dir, &["commit", "-qam", "topic"])?;
    Ok((temp, default_branch))
}

#[test]
fn rebases_the_checked_out_branch_onto_another() -> Result<()> {
    let (temp, default_branch) = repo_with_topic_branch("1\n2\n3\n4\n")?;
    let repo_dir = temp.path();

    smol::block_on(rebase_onto(&repo(repo_dir), &default_branch))?;
    run(
        repo_dir,
        &["merge-base", "--is-ancestor", &default_branch, "topic"],
    )?;
    Ok(())
}

#[test]
fn a_conflicting_rebase_is_rolled_back() -> Result<()> {
    let (temp, default_branch) = repo_with_topic_branch("1\nconflict\n3\n")?;
    let repo_dir = temp.path();

    let error = smol::block_on(rebase_onto(&repo(repo_dir), &default_branch))
        .expect_err("the topic branch must conflict");

    assert!(error.to_string().contains("rolled back"), "{error}");
    assert!(!smol::block_on(operation_in_progress(&repo(repo_dir)))?);
    Ok(())
}

#[test]
fn lists_commits_that_are_not_on_the_remote() -> Result<()> {
    let (_temp, _remote, local) = remote_and_clone()?;
    commit_on_branch(&local, "feature", "1\none\n3\n")?;
    commit_on_branch(&local, "feature", "1\ntwo\n3\n")?;

    let local_repo = repo(&local);
    let with_upstream = smol::block_on(outgoing_commits(
        &local_repo,
        "refs/heads/feature",
        Some("refs/remotes/origin/feature"),
    ))?;
    let without_upstream =
        smol::block_on(outgoing_commits(&local_repo, "refs/heads/feature", None))?;
    let subjects = |commits: &[OutgoingCommit]| {
        commits
            .iter()
            .map(|commit| commit.subject.clone())
            .collect::<Vec<_>>()
    };
    // `commit_on_branch` uses the file contents as the message; git joins its lines.
    assert_eq!(subjects(&with_upstream), ["1 two 3", "1 one 3"]);
    assert_eq!(subjects(&with_upstream), subjects(&without_upstream));
    Ok(())
}

#[test]
fn pushes_a_branch_and_refuses_a_non_fast_forward() -> Result<()> {
    let (_temp, remote, local) = remote_and_clone()?;
    let target = || PushTarget {
        remote: "origin",
        branch: "feature",
        set_upstream: false,
        force_with_lease: false,
    };
    commit_on_branch(&local, "feature", "1\nlocal\n3\n")?;

    smol::block_on(push_branch(&repo(&local), "feature", target()))?;
    assert_eq!(
        run(&local, &["rev-parse", "feature"])?,
        run(&remote, &["rev-parse", "feature"])?
    );

    commit_on_branch(&remote, "feature", "1\nremote\n3\n")?;
    commit_on_branch(&local, "feature", "1\nlocal again\n3\n")?;
    assert!(smol::block_on(push_branch(&repo(&local), "feature", target())).is_err());
    Ok(())
}

#[test]
fn single_file_apply_treats_the_name_literally() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo_dir = temp.path();
    run(repo_dir, &["init", "-q"])?;
    configure_identity(repo_dir)?;
    write(repo_dir, "file[1].txt", "old\n")?;
    write(repo_dir, "file1.txt", "old\n")?;
    run(repo_dir, &["add", "."])?;
    run(repo_dir, &["commit", "-qm", "one"])?;
    write(repo_dir, "file[1].txt", "new\n")?;
    write(repo_dir, "file1.txt", "new\n")?;
    run(repo_dir, &["commit", "-qam", "two"])?;
    let sha = run(repo_dir, &["rev-parse", "HEAD"])?;

    smol::block_on(apply_commit_file(
        &repo(repo_dir),
        &sha,
        "file[1].txt",
        Direction::Revert,
    ))?;
    assert_eq!(read(repo_dir, "file[1].txt")?, "old\n");
    assert_eq!(read(repo_dir, "file1.txt")?, "new\n");
    Ok(())
}

#[test]
fn lists_unusual_file_names_verbatim() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo_dir = temp.path();
    run(repo_dir, &["init", "-q"])?;
    configure_identity(repo_dir)?;
    write(repo_dir, "tab\there \"q\" é.txt", "x\n")?;
    run(repo_dir, &["add", "."])?;
    run(repo_dir, &["commit", "-qm", "one"])?;
    let sha = run(repo_dir, &["rev-parse", "HEAD"])?;

    let files = smol::block_on(commit_files(&repo(repo_dir), &sha))?;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "tab\there \"q\" é.txt");
    Ok(())
}

#[test]
fn updates_the_branch_even_when_a_tag_shares_its_name() -> Result<()> {
    let (_temp, remote, local) = remote_and_clone()?;
    commit_on_branch(&remote, "feature", "1\nremote\n3\n")?;
    run(&remote, &["tag", "feature", "HEAD~1"])?;

    smol::block_on(update_branch(
        &repo(&local),
        "feature",
        "origin",
        "feature",
        false,
    ))?;
    assert_eq!(
        run(&local, &["rev-parse", "refs/heads/feature"])?,
        run(&remote, &["rev-parse", "refs/heads/feature"])?
    );
    Ok(())
}

#[test]
fn default_remote_prefers_origin_then_any_configured_remote() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo_dir = temp.path();
    run(repo_dir, &["init", "-q"])?;
    assert_eq!(smol::block_on(default_remote(&repo(repo_dir)))?, "origin");

    run(repo_dir, &["remote", "add", "upstream", "."])?;
    assert_eq!(smol::block_on(default_remote(&repo(repo_dir)))?, "upstream");

    run(repo_dir, &["remote", "add", "origin", "."])?;
    assert_eq!(smol::block_on(default_remote(&repo(repo_dir)))?, "origin");
    Ok(())
}

#[test]
fn untracking_keeps_the_file_on_disk_and_stages_its_removal() -> Result<()> {
    let (temp, _) = repo_with_two_commits()?;
    let repo_dir = temp.path();

    smol::block_on(untrack(&repo(repo_dir), &["a.txt".to_string()]))?;

    assert_eq!(read(repo_dir, "a.txt")?, "1\ntwo\n3\n");
    let status = run(repo_dir, &["status", "--porcelain"])?;
    assert!(status.contains("D  a.txt"), "{status}");
    assert!(status.contains("?? a.txt"), "{status}");
    assert!(!status.contains("b.txt"), "{status}");
    Ok(())
}

#[test]
fn untracking_a_folder_skips_files_git_does_not_know() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo_dir = temp.path();
    run(repo_dir, &["init", "-q"])?;
    configure_identity(repo_dir)?;
    fs::create_dir(repo_dir.join("dir"))?;
    write(repo_dir, "dir/tracked.txt", "t")?;
    run(repo_dir, &["add", "."])?;
    run(repo_dir, &["commit", "-qm", "one"])?;
    write(repo_dir, "dir/new.txt", "n")?;

    smol::block_on(untrack(&repo(repo_dir), &["dir".to_string()]))?;
    smol::block_on(untrack(&repo(repo_dir), &["dir/new.txt".to_string()]))?;

    assert!(repo_dir.join("dir/tracked.txt").exists());
    assert_eq!(
        run(repo_dir, &["ls-files"])?,
        "",
        "nothing is tracked anymore"
    );
    Ok(())
}

#[test]
fn untracking_takes_glob_characters_literally() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo_dir = temp.path();
    run(repo_dir, &["init", "-q"])?;
    configure_identity(repo_dir)?;
    write(repo_dir, "a.txt", "a")?;
    write(repo_dir, "[ab].txt", "x")?;
    run(repo_dir, &["add", "."])?;
    run(repo_dir, &["commit", "-qm", "one"])?;

    smol::block_on(untrack(&repo(repo_dir), &["[ab].txt".to_string()]))?;

    assert_eq!(run(repo_dir, &["ls-files"])?, "a.txt");
    Ok(())
}
