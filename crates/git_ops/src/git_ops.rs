//! Local-only git operations that `Repository` does not offer. Shells out to `git`.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncWriteExt as _;
use std::path::Path;
use util::command::{Stdio, new_command};

#[derive(Clone, Copy)]
pub enum Direction {
    Apply,
    Revert,
}

pub async fn apply_commit_file(
    repo_dir: &Path,
    sha: &str,
    path: &str,
    direction: Direction,
) -> Result<()> {
    // `-m --first-parent` makes root and merge commits diff like regular ones.
    let patch = git(
        repo_dir,
        &[
            "show",
            "--format=",
            "--binary",
            "-m",
            "--first-parent",
            sha,
            "--",
            path,
        ],
        &[],
    )
    .await?;
    if patch.is_empty() {
        bail!("{sha} does not change {path}");
    }

    let mut flags = Vec::new();
    if matches!(direction, Direction::Revert) {
        flags.push("-R");
    }
    let Err(plain_error) = git_apply(repo_dir, &flags, &patch).await else {
        return Ok(());
    };
    // Plain apply is atomic and also works on a dirty tree. The 3-way retry needs a clean
    // file but leaves conflict markers for the conflict UI.
    flags.push("--3way");
    git_apply(repo_dir, &flags, &patch)
        .await
        .map_err(|three_way_error| anyhow!("{plain_error}\n{three_way_error}"))
}

async fn git_apply(repo_dir: &Path, flags: &[&str], patch: &[u8]) -> Result<()> {
    let args = [&["apply"], flags, &["-"]].concat();
    git(repo_dir, &args, patch).await?;
    Ok(())
}

async fn git(repo_dir: &Path, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
    let mut child = new_command("git")
        .args(args)
        .current_dir(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run git")?;
    let mut child_stdin = child.stdin.take().context("no stdin for git")?;
    let written = child_stdin.write_all(stdin).await;
    drop(child_stdin);

    // A failed write usually means git exited early, so its stderr is the better error.
    let output = child.output().await?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    written?;
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run(repo_dir: &Path, args: &[&str]) -> Result<String> {
        let output = smol::block_on(git(repo_dir, args, &[]))?;
        Ok(String::from_utf8(output)?.trim().to_string())
    }

    fn write(repo_dir: &Path, name: &str, contents: &str) -> Result<()> {
        Ok(fs::write(repo_dir.join(name), contents)?)
    }

    fn read(repo_dir: &Path, name: &str) -> Result<String> {
        Ok(fs::read_to_string(repo_dir.join(name))?)
    }

    // Commit one adds a.txt, b.txt and root.txt. Commit two edits a.txt and b.txt.
    fn repo_with_two_commits() -> Result<(tempfile::TempDir, String)> {
        let temp = tempfile::tempdir()?;
        let repo_dir = temp.path();
        run(repo_dir, &["init", "-q"])?;
        run(repo_dir, &["config", "user.email", "test@example.com"])?;
        run(repo_dir, &["config", "user.name", "test"])?;
        run(repo_dir, &["config", "commit.gpgsign", "false"])?;
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
            repo_dir,
            &sha,
            "a.txt",
            Direction::Revert,
        ))?;
        assert_eq!(read(repo_dir, "a.txt")?, "1\n2\n3\n");
        assert_eq!(read(repo_dir, "b.txt")?, "1\ntwo\n3\n");

        smol::block_on(apply_commit_file(repo_dir, &sha, "a.txt", Direction::Apply))?;
        assert_eq!(read(repo_dir, "a.txt")?, "1\ntwo\n3\n");
        Ok(())
    }

    #[test]
    fn reverts_file_added_by_root_commit() -> Result<()> {
        let (temp, _) = repo_with_two_commits()?;
        let repo_dir = temp.path();
        let root = run(repo_dir, &["rev-list", "--max-parents=0", "HEAD"])?;

        smol::block_on(apply_commit_file(
            repo_dir,
            &root,
            "root.txt",
            Direction::Revert,
        ))?;
        assert!(!repo_dir.join("root.txt").exists());
        Ok(())
    }

    #[test]
    fn rejects_file_untouched_by_commit() -> Result<()> {
        let (temp, sha) = repo_with_two_commits()?;

        let message = smol::block_on(apply_commit_file(
            temp.path(),
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
            repo_dir,
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
}
