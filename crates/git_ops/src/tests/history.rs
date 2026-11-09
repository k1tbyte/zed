use super::*;

// Commit `n` adds `f{n}.txt` and is called `c{n}`.
fn linear_repo(count: usize) -> Result<(tempfile::TempDir, Vec<String>)> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path();
    run(dir, &["init", "-q"])?;
    configure_identity(dir)?;
    for number in 1..=count {
        write(dir, &format!("f{number}.txt"), &format!("{number}\n"))?;
        run(dir, &["add", "."])?;
        run(dir, &["commit", "-qm", &format!("c{number}")])?;
    }
    let shas = shas(dir)?;
    Ok((temp, shas))
}

fn shas(dir: &Path) -> Result<Vec<String>> {
    let output = run(dir, &["rev-list", "--reverse", "HEAD"])?;
    Ok(output.lines().map(str::to_string).collect())
}

fn subjects(dir: &Path) -> Result<Vec<String>> {
    let output = run(dir, &["log", "--reverse", "--format=%s"])?;
    Ok(output.lines().map(str::to_string).collect())
}

fn commit_changing(dir: &Path, file: &str, contents: &str, message: &str) -> Result<()> {
    write(dir, file, contents)?;
    run(dir, &["add", "."])?;
    run(dir, &["commit", "-qm", message])?;
    Ok(())
}

// Commits `a`, `b`, `c` all rewrite f1.txt, so dropping `b` makes `c` conflict.
fn conflict_prone_repo() -> Result<(tempfile::TempDir, Vec<String>)> {
    let temp = tempfile::tempdir()?;
    let dir = temp.path();
    run(dir, &["init", "-q"])?;
    configure_identity(dir)?;
    for (contents, message) in [("a\n", "a"), ("b\n", "b"), ("c\n", "c")] {
        commit_changing(dir, "f1.txt", contents, message)?;
    }
    let shas = shas(dir)?;
    Ok((temp, shas))
}

fn assert_clean_and_idle(dir: &Path) -> Result<()> {
    assert_eq!(run(dir, &["status", "--porcelain"])?, "");
    assert!(!smol::block_on(operation_in_progress(&repo(dir)))?);
    Ok(())
}

#[test]
fn rewords_one_commit_and_keeps_lines_starting_with_a_hash() -> Result<()> {
    let (temp, shas) = linear_repo(3)?;
    let dir = temp.path();
    let message = "#12 fix\n\n# not a comment".to_string();

    smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Reword {
            sha: shas[1].clone(),
            message: message.clone(),
        },
    ))?;

    assert_eq!(subjects(dir)?, ["c1", "#12 fix", "c3"]);
    assert_eq!(run(dir, &["log", "-1", "--format=%B", "HEAD~1"])?, message);
    assert_eq!(read(dir, "f3.txt")?, "3\n");
    Ok(())
}

#[test]
fn rewords_the_root_commit() -> Result<()> {
    let (temp, shas) = linear_repo(2)?;
    let dir = temp.path();

    smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Reword {
            sha: shas[0].clone(),
            message: "first".into(),
        },
    ))?;

    assert_eq!(subjects(dir)?, ["first", "c2"]);
    Ok(())
}

#[test]
fn drops_commits_that_are_not_adjacent() -> Result<()> {
    let (temp, shas) = linear_repo(4)?;
    let dir = temp.path();

    smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Drop {
            shas: vec![shas[1].clone(), shas[3].clone()],
        },
    ))?;

    assert_eq!(subjects(dir)?, ["c1", "c3"]);
    assert!(!dir.join("f2.txt").exists() && !dir.join("f4.txt").exists());
    assert!(dir.join("f1.txt").exists() && dir.join("f3.txt").exists());
    Ok(())
}

#[test]
fn drops_the_root_commit() -> Result<()> {
    let (temp, shas) = linear_repo(3)?;
    let dir = temp.path();

    smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Drop {
            shas: vec![shas[0].clone()],
        },
    ))?;

    assert_eq!(subjects(dir)?, ["c2", "c3"]);
    Ok(())
}

#[test]
fn squashes_adjacent_commits_with_the_given_message() -> Result<()> {
    let (temp, shas) = linear_repo(4)?;
    let dir = temp.path();

    smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Squash {
            shas: vec![shas[1].clone(), shas[2].clone()],
            message: Some("both".into()),
        },
    ))?;

    assert_eq!(subjects(dir)?, ["c1", "both", "c4"]);
    assert_eq!(run(dir, &["ls-files"])?.lines().count(), 4);
    Ok(())
}

#[test]
fn squashing_apart_commits_moves_them_together_and_keeps_the_oldest_message() -> Result<()> {
    let (temp, shas) = linear_repo(4)?;
    let dir = temp.path();

    smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Squash {
            shas: vec![shas[0].clone(), shas[2].clone()],
            message: None,
        },
    ))?;

    assert_eq!(subjects(dir)?, ["c1", "c2", "c4"]);
    let first_commit_files = run(dir, &["show", "--name-only", "--format=", "HEAD~2"])?;
    assert_eq!(first_commit_files, "f1.txt\nf3.txt");
    Ok(())
}

#[test]
fn a_rewrite_keeps_uncommitted_changes() -> Result<()> {
    let (temp, shas) = linear_repo(3)?;
    let dir = temp.path();
    write(dir, "f1.txt", "dirty\n")?;
    write(dir, "untracked.txt", "new\n")?;

    smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Reword {
            sha: shas[0].clone(),
            message: "renamed".into(),
        },
    ))?;

    assert_eq!(read(dir, "f1.txt")?, "dirty\n");
    assert_eq!(read(dir, "untracked.txt")?, "new\n");
    assert_eq!(subjects(dir)?[0], "renamed");
    Ok(())
}

#[test]
fn a_conflicting_rewrite_is_rolled_back() -> Result<()> {
    let (temp, shas) = conflict_prone_repo()?;
    let dir = temp.path();

    let error = smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Drop {
            shas: vec![shas[1].clone()],
        },
    ))
    .expect_err("dropping `b` must conflict");

    assert!(error.to_string().contains("rolled back"), "{error}");
    assert_eq!(run(dir, &["rev-parse", "HEAD"])?, shas[2]);
    assert_clean_and_idle(dir)
}

#[test]
fn refuses_a_range_with_a_merge_commit() -> Result<()> {
    let (temp, shas) = linear_repo(2)?;
    let dir = temp.path();
    run(dir, &["checkout", "-q", "-b", "side", &shas[0]])?;
    commit_changing(dir, "side.txt", "s\n", "side")?;
    run(dir, &["checkout", "-q", "-"])?;
    run(dir, &["merge", "-q", "--no-ff", "-m", "merge", "side"])?;
    let head = run(dir, &["rev-parse", "HEAD"])?;

    let error = smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Reword {
            sha: shas[0].clone(),
            message: "x".into(),
        },
    ))
    .expect_err("a merge in the range must be refused");

    assert!(error.to_string().contains("merge commit"), "{error}");
    assert_eq!(run(dir, &["rev-parse", "HEAD"])?, head);
    Ok(())
}

#[test]
fn refuses_commits_that_are_not_on_the_current_branch() -> Result<()> {
    let (temp, shas) = linear_repo(2)?;
    let dir = temp.path();
    run(dir, &["checkout", "-q", "-b", "side", &shas[0]])?;
    commit_changing(dir, "side.txt", "s\n", "side")?;
    let side = run(dir, &["rev-parse", "HEAD"])?;
    run(dir, &["checkout", "-q", "-"])?;

    let error = smol::block_on(rewrite(&repo(dir), Rewrite::Drop { shas: vec![side] }))
        .expect_err("a commit of another branch must be refused");

    assert!(
        error.to_string().contains("not on the current branch"),
        "{error}"
    );
    assert_eq!(subjects(dir)?, ["c1", "c2"]);
    Ok(())
}

#[test]
fn leaves_an_operation_that_is_not_ours_alone() -> Result<()> {
    let (temp, shas) = linear_repo(1)?;
    let dir = temp.path();
    run(dir, &["checkout", "-q", "-b", "side"])?;
    commit_changing(dir, "f1.txt", "side\n", "side")?;
    run(dir, &["checkout", "-q", "-"])?;
    commit_changing(dir, "f1.txt", "main\n", "main")?;
    assert!(run(dir, &["rebase", "side"]).is_err());
    assert!(smol::block_on(operation_in_progress(&repo(dir)))?);

    let error = smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Reword {
            sha: shas[0].clone(),
            message: "x".into(),
        },
    ))
    .expect_err("must not start on top of a stopped rebase");

    assert!(error.to_string().contains("in progress"), "{error}");
    assert!(smol::block_on(operation_in_progress(&repo(dir)))?);
    Ok(())
}

#[test]
fn reverts_a_commit_with_a_new_one() -> Result<()> {
    let (temp, shas) = linear_repo(3)?;
    let dir = temp.path();

    smol::block_on(revert(&repo(dir), &[shas[2].clone()]))?;

    assert_eq!(subjects(dir)?, ["c1", "c2", "c3", "Revert \"c3\""]);
    assert!(!dir.join("f3.txt").exists());
    Ok(())
}

#[test]
fn cherry_picks_commits_from_another_branch() -> Result<()> {
    let (temp, shas) = linear_repo(2)?;
    let dir = temp.path();
    run(dir, &["checkout", "-q", "-b", "side", &shas[0]])?;
    commit_changing(dir, "one.txt", "1\n", "side one")?;
    commit_changing(dir, "two.txt", "2\n", "side two")?;
    let picked = shas_of(dir, "main..side")?;
    run(dir, &["checkout", "-q", "-"])?;

    smol::block_on(cherry_pick(&repo(dir), &picked))?;

    assert_eq!(subjects(dir)?, ["c1", "c2", "side one", "side two"]);
    Ok(())
}

fn shas_of(dir: &Path, range: &str) -> Result<Vec<String>> {
    let output = run(dir, &["rev-list", "--reverse", range])?;
    Ok(output.lines().map(str::to_string).collect())
}

#[test]
fn a_conflicting_cherry_pick_is_rolled_back() -> Result<()> {
    let (temp, shas) = conflict_prone_repo()?;
    let dir = temp.path();

    let error = smol::block_on(cherry_pick(&repo(dir), &[shas[1].clone()]))
        .expect_err("`b` rewrites a line that `c` already changed");

    assert!(error.to_string().contains("rolled back"), "{error}");
    assert_eq!(run(dir, &["rev-parse", "HEAD"])?, shas[2]);
    assert_clean_and_idle(dir)
}

#[test]
fn hard_reset_discards_and_keep_reset_preserves_local_changes() -> Result<()> {
    let (temp, shas) = linear_repo(3)?;
    let dir = temp.path();
    write(dir, "f1.txt", "dirty\n")?;

    smol::block_on(reset(&repo(dir), &shas[1], ResetMode::Keep))?;
    assert_eq!(run(dir, &["rev-parse", "HEAD"])?, shas[1]);
    assert_eq!(read(dir, "f1.txt")?, "dirty\n");

    smol::block_on(reset(&repo(dir), &shas[0], ResetMode::Hard))?;
    assert_eq!(run(dir, &["rev-parse", "HEAD"])?, shas[0]);
    assert_eq!(read(dir, "f1.txt")?, "1\n");
    Ok(())
}

#[test]
fn reads_full_messages_in_the_requested_order() -> Result<()> {
    let (temp, shas) = linear_repo(3)?;
    let dir = temp.path();
    write(dir, "f1.txt", "x\n")?;
    run(dir, &["commit", "-qam", "subject", "-m", "body"])?;
    let newest = run(dir, &["rev-parse", "HEAD"])?;

    let messages = smol::block_on(commit_messages(&repo(dir), &[newest, shas[0].clone()]))?;

    assert_eq!(messages, ["subject\n\nbody", "c1"]);
    Ok(())
}

#[test]
fn names_a_remote_branch_that_already_has_the_commit() -> Result<()> {
    let (_temp, _remote, local) = remote_and_clone()?;
    let pushed = run(&local, &["rev-parse", "HEAD"])?;
    commit_changing(&local, "local.txt", "l\n", "local only")?;
    let unpushed = run(&local, &["rev-parse", "HEAD"])?;

    let pushed_to_remote = smol::block_on(pushed_to(&repo(&local), &pushed))?;
    assert!(pushed_to_remote.is_some_and(|name| name.starts_with("origin/")));
    assert_eq!(smol::block_on(pushed_to(&repo(&local), &unpushed))?, None);
    Ok(())
}

#[test]
fn reports_uncommitted_changes_that_no_longer_apply_after_a_rewrite() -> Result<()> {
    let (temp, shas) = conflict_prone_repo()?;
    let dir = temp.path();
    write(dir, "f1.txt", "c-dirty\n")?;

    let error = smol::block_on(rewrite(
        &repo(dir),
        Rewrite::Drop {
            shas: vec![shas[2].clone()],
        },
    ))
    .expect_err("the stashed edit cannot apply on top of `b`");

    assert!(error.to_string().contains("kept in the stash"), "{error}");
    assert_eq!(subjects(dir)?, ["a", "b"]);
    assert_eq!(run(dir, &["stash", "list"])?.lines().count(), 1);
    Ok(())
}

#[test]
fn refuses_to_drop_every_commit() -> Result<()> {
    let (temp, shas) = linear_repo(2)?;
    let dir = temp.path();

    let error = smol::block_on(rewrite(&repo(dir), Rewrite::Drop { shas: shas.clone() }))
        .expect_err("an empty branch cannot be produced");

    assert!(error.to_string().contains("empty"), "{error}");
    assert_eq!(shas_of(dir, "HEAD")?, shas);
    Ok(())
}
