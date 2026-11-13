use super::*;
use std::path::PathBuf;

fn configure_pull(dir: &Path, rebase: &str, fast_forward: &str) -> Result<()> {
    run(dir, &["config", "pull.rebase", rebase])?;
    run(dir, &["config", "pull.ff", fast_forward])?;
    run(dir, &["config", "merge.autoStash", "false"])?;
    run(dir, &["config", "rebase.autoStash", "false"])?;
    Ok(())
}

fn blocker_of(result: Result<()>) -> Option<Blocker> {
    result.err().and_then(|error| blocker(&error))
}

fn pull_options(autostash: bool, reconcile: Option<Reconcile>) -> UpdateOptions {
    UpdateOptions {
        autostash,
        reconcile,
    }
}

fn update_checked_out(local: &Path, options: UpdateOptions) -> Result<()> {
    smol::block_on(update_branch(
        &repo(local),
        "feature",
        "origin",
        "feature",
        true,
        options,
    ))
}

// The clone is on `feature`, one remote commit behind, with a dirty line 3 in a.txt.
fn dirty_clone_behind_remote(pull_rebase: &str) -> Result<(tempfile::TempDir, PathBuf)> {
    let (temp, remote, local) = remote_and_clone()?;
    configure_pull(&local, pull_rebase, "true")?;
    commit_on_branch(&remote, "feature", "ONE\ntwo\n3\n")?;
    run(&local, &["switch", "-q", "feature"])?;
    write(&local, "a.txt", "1\ntwo\nthree\n")?;
    Ok((temp, local))
}

fn assert_update_needs_autostash(pull_rebase: &str) -> Result<()> {
    let (_temp, local) = dirty_clone_behind_remote(pull_rebase)?;

    let result = update_checked_out(&local, UpdateOptions::default());
    assert_eq!(blocker_of(result), Some(Blocker::LocalChanges));
    assert_eq!(read(&local, "a.txt")?, "1\ntwo\nthree\n");

    update_checked_out(&local, pull_options(true, None))?;
    assert_eq!(read(&local, "a.txt")?, "ONE\ntwo\nthree\n");
    assert_eq!(
        run(&local, &["rev-parse", "feature"])?,
        run(&local, &["rev-parse", "origin/feature"])?
    );
    Ok(())
}

#[test]
fn merge_pull_blocked_by_local_changes_succeeds_with_autostash() -> Result<()> {
    assert_update_needs_autostash("false")
}

#[test]
fn rebase_pull_blocked_by_local_changes_succeeds_with_autostash() -> Result<()> {
    assert_update_needs_autostash("true")
}

// The clone is on `feature` with a local commit to b.txt and the remote has one to a.txt.
fn diverged_clone() -> Result<(tempfile::TempDir, PathBuf)> {
    let (temp, remote, local) = remote_and_clone()?;
    configure_pull(&local, "false", "only")?;
    commit_on_branch(&remote, "feature", "ONE\ntwo\n3\n")?;
    run(&local, &["switch", "-q", "feature"])?;
    write(&local, "b.txt", "local\n")?;
    run(&local, &["commit", "-qam", "local"])?;
    Ok((temp, local))
}

#[test]
fn diverged_update_is_blocked_and_merges_on_request() -> Result<()> {
    let (_temp, local) = diverged_clone()?;

    let result = update_checked_out(&local, UpdateOptions::default());
    assert_eq!(blocker_of(result), Some(Blocker::Diverged));

    update_checked_out(&local, pull_options(false, Some(Reconcile::Merge)))?;
    let parents = run(&local, &["rev-list", "--parents", "-n", "1", "HEAD"])?;
    assert_eq!(parents.split(' ').count(), 3, "{parents}");
    assert_eq!(read(&local, "a.txt")?, "ONE\ntwo\n3\n");
    assert_eq!(read(&local, "b.txt")?, "local\n");
    Ok(())
}

#[test]
fn diverged_update_rebases_on_request() -> Result<()> {
    let (_temp, local) = diverged_clone()?;

    update_checked_out(&local, pull_options(false, Some(Reconcile::Rebase)))?;
    assert_eq!(
        run(&local, &["rev-parse", "HEAD~1"])?,
        run(&local, &["rev-parse", "origin/feature"])?
    );
    assert_eq!(run(&local, &["log", "-1", "--format=%s"])?, "local");
    assert_eq!(read(&local, "a.txt")?, "ONE\ntwo\n3\n");
    Ok(())
}

// The clone is on the default branch with a dirty line 3 in a.txt, and `feature` rewrote line 3.
fn dirty_clone_with_other_branch() -> Result<(tempfile::TempDir, PathBuf, String)> {
    let (temp, _remote, local) = remote_and_clone()?;
    let default_branch = current_branch(&local)?;
    commit_on_branch(&local, "feature", "1\ntwo\nfeature\n")?;
    write(&local, "a.txt", "1\ntwo\nedit\n")?;
    Ok((temp, local, default_branch))
}

fn current_branch(dir: &Path) -> Result<String> {
    run(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
}

fn stash_count(dir: &Path) -> Result<usize> {
    Ok(run(dir, &["stash", "list"])?.lines().count())
}

fn checkout(local: &Path, local_changes: LocalChanges) -> Result<()> {
    smol::block_on(checkout_branch(&repo(local), "feature", local_changes))
}

#[test]
fn refused_checkout_reports_local_changes_and_stays_put() -> Result<()> {
    let (_temp, local, default_branch) = dirty_clone_with_other_branch()?;

    let result = checkout(&local, LocalChanges::Refuse);
    assert_eq!(blocker_of(result), Some(Blocker::LocalChanges));
    assert_eq!(current_branch(&local)?, default_branch);
    assert_eq!(read(&local, "a.txt")?, "1\ntwo\nedit\n");
    Ok(())
}

// Keep applies the stash with a 3-way merge, so the edit lands on top of the branch's change
// when they touch different lines.
#[test]
fn checkout_can_carry_local_changes_along() -> Result<()> {
    let (_temp, local, _) = dirty_clone_with_other_branch()?;
    write(&local, "a.txt", "edit\ntwo\n3\n")?;

    checkout(&local, LocalChanges::Keep)?;
    assert_eq!(current_branch(&local)?, "feature");
    assert_eq!(read(&local, "a.txt")?, "edit\ntwo\nfeature\n");
    assert_eq!(stash_count(&local)?, 0);
    Ok(())
}

#[test]
fn checkout_can_discard_local_changes() -> Result<()> {
    let (_temp, local, _) = dirty_clone_with_other_branch()?;

    checkout(&local, LocalChanges::Discard)?;
    assert_eq!(current_branch(&local)?, "feature");
    assert_eq!(read(&local, "a.txt")?, "1\ntwo\nfeature\n");
    Ok(())
}

#[test]
fn keeping_nothing_leaves_an_older_stash_alone() -> Result<()> {
    let (_temp, local, _) = dirty_clone_with_other_branch()?;
    run(&local, &["stash", "push", "-q"])?;
    assert_eq!(stash_count(&local)?, 1);

    checkout(&local, LocalChanges::Keep)?;
    assert_eq!(current_branch(&local)?, "feature");
    assert_eq!(stash_count(&local)?, 1);
    assert_eq!(read(&local, "a.txt")?, "1\ntwo\nfeature\n");
    Ok(())
}

#[test]
fn checkout_keeps_the_stash_when_the_changes_conflict() -> Result<()> {
    let (_temp, local, _) = dirty_clone_with_other_branch()?;

    let error = checkout(&local, LocalChanges::Keep)
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
    assert!(error.contains("kept in the stash"), "{error}");
    assert_eq!(current_branch(&local)?, "feature");
    assert_eq!(stash_count(&local)?, 1);
    Ok(())
}

fn tracking_refs(dir: &Path) -> Result<String> {
    run(dir, &["branch", "-r", "--list", "origin/feature"])
}

#[test]
fn deletes_a_remote_branch_and_its_tracking_ref() -> Result<()> {
    let (_temp, remote, local) = remote_and_clone()?;
    assert_ne!(tracking_refs(&local)?, "");

    smol::block_on(delete_remote_branch(&repo(&local), "origin", "feature"))?;
    assert_eq!(run(&remote, &["branch", "--list", "feature"])?, "");
    assert_eq!(tracking_refs(&local)?, "");
    Ok(())
}

#[test]
fn deleting_a_branch_already_gone_from_the_remote_drops_the_tracking_ref() -> Result<()> {
    let (_temp, remote, local) = remote_and_clone()?;
    run(&remote, &["branch", "-D", "feature"])?;
    assert_ne!(tracking_refs(&local)?, "");

    smol::block_on(delete_remote_branch(&repo(&local), "origin", "feature"))?;
    assert_eq!(tracking_refs(&local)?, "");
    Ok(())
}

#[test]
fn prunes_tracking_refs_of_deleted_remote_branches() -> Result<()> {
    let (_temp, remote, local) = remote_and_clone()?;
    run(&remote, &["branch", "-D", "feature"])?;

    smol::block_on(prune_remote_branches(&repo(&local)))?;
    assert_eq!(tracking_refs(&local)?, "");
    Ok(())
}

#[test]
fn lists_only_branches_merged_into_the_target() -> Result<()> {
    let (temp, _) = repo_with_two_commits()?;
    let dir = temp.path();
    let default_branch = current_branch(dir)?;
    run(dir, &["branch", "merged"])?;
    run(dir, &["switch", "-q", "-c", "unmerged"])?;
    write(dir, "a.txt", "1\nunmerged\n3\n")?;
    run(dir, &["commit", "-qam", "unmerged"])?;

    let merged = smol::block_on(merged_branches(&repo(dir), &default_branch))?;
    assert!(
        merged.contains(&"refs/heads/merged".to_string()),
        "{merged:?}"
    );
    assert!(
        !merged.contains(&"refs/heads/unmerged".to_string()),
        "{merged:?}"
    );
    Ok(())
}

#[test]
fn classifies_git_refusals() {
    let blocker_for = |message: &str| blocker(&anyhow!("{message}"));

    assert_eq!(
        blocker_for(
            "error: Your local changes to the following files would be overwritten by merge:\n\ta.txt"
        ),
        Some(Blocker::LocalChanges)
    );
    assert_eq!(
        blocker_for("fatal: cannot pull with rebase: You have unstaged changes."),
        Some(Blocker::LocalChanges)
    );
    assert_eq!(
        blocker_for("fatal: Need to specify how to reconcile divergent branches."),
        Some(Blocker::Diverged)
    );
    assert_eq!(
        blocker_for("fatal: Not possible to fast-forward, aborting."),
        Some(Blocker::Diverged)
    );
    assert_eq!(
        blocker_for(
            "error: The following untracked working tree files would be overwritten by checkout:\n\tnew.txt"
        ),
        None
    );
    let wrapped = anyhow!("fatal: Not possible to fast-forward, aborting.").context("pull failed");
    assert_eq!(blocker(&wrapped), Some(Blocker::Diverged));
}
