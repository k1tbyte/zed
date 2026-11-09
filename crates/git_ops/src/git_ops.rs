//! Local-only git operations that `Repository` does not offer. Shells out to `git`.

mod history;

use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncWriteExt as _;
pub use history::*;
use std::{path::Path, sync::Arc};
use util::command::{Stdio, new_command};

/// A checkout on this machine. `trusted` mirrors the project's worktree trust.
#[derive(Clone)]
pub struct LocalRepo {
    pub dir: Arc<Path>,
    pub trusted: bool,
}

#[derive(Clone, Copy)]
pub enum Direction {
    Apply,
    Revert,
}

pub async fn apply_commit_file(
    repo: &LocalRepo,
    sha: &str,
    path: &str,
    direction: Direction,
) -> Result<()> {
    // `-m --first-parent` makes root and merge commits diff like regular ones. Literal
    // pathspecs and no textconv keep the patch to exactly this file's committed bytes.
    let patch = git(
        repo,
        &[
            "--literal-pathspecs",
            "show",
            "--format=",
            "--binary",
            "--no-textconv",
            "--no-ext-diff",
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
    let Err(plain_error) = git_apply(repo, &flags, &patch).await else {
        return Ok(());
    };
    // Plain apply is atomic and also works on a dirty tree. The 3-way retry needs a clean
    // file but leaves conflict markers for the conflict UI.
    flags.push("--3way");
    git_apply(repo, &flags, &patch)
        .await
        .map_err(|three_way_error| anyhow!("{plain_error}\n{three_way_error}"))
}

pub async fn update_branch(
    repo: &LocalRepo,
    branch: &str,
    remote: &str,
    remote_branch: &str,
    is_checked_out: bool,
) -> Result<()> {
    if is_checked_out {
        git(repo, &["pull"], &[]).await?;
    } else {
        // Full refs keep a same-named tag from winning. No leading `+`, so anything but a
        // fast-forward is refused and local commits stay safe.
        let refspec = format!("refs/heads/{remote_branch}:refs/heads/{branch}");
        git(repo, &["fetch", remote, &refspec], &[]).await?;
    }
    Ok(())
}

// Autostash so a dirty tree does not block the rebase; git restores the changes afterwards.
pub async fn rebase_onto(repo: &LocalRepo, onto: &str) -> Result<()> {
    git(repo, &["rebase", "--autostash", onto], &[]).await?;
    Ok(())
}

// Remotes without fetched branches are missing from the branch list, so ask git.
pub async fn default_remote(repo: &LocalRepo) -> Result<String> {
    let output = String::from_utf8(git(repo, &["remote"], &[]).await?)?;
    let remotes: Vec<&str> = output.lines().collect();
    Ok(remotes
        .iter()
        .find(|remote| **remote == "origin")
        .or(remotes.first())
        .copied()
        .unwrap_or("origin")
        .to_string())
}

pub struct PushTarget<'a> {
    pub remote: &'a str,
    pub branch: &'a str,
    pub set_upstream: bool,
    pub force_with_lease: bool,
}

pub async fn push_branch(repo: &LocalRepo, branch: &str, target: PushTarget<'_>) -> Result<()> {
    // Full refs keep a tag or remote that shares the branch's name from being picked instead.
    let refspec = format!("refs/heads/{branch}:refs/heads/{}", target.branch);
    let mut args = vec!["push"];
    if target.set_upstream {
        args.push("--set-upstream");
    }
    if target.force_with_lease {
        args.push("--force-with-lease");
    }
    args.extend([target.remote, &refspec]);
    git(repo, &args, &[]).await?;
    Ok(())
}

pub struct OutgoingCommit {
    pub sha: String,
    pub author: String,
    pub subject: String,
}

// Without an upstream, "outgoing" means not reachable from any remote branch.
pub async fn outgoing_commits(
    repo: &LocalRepo,
    branch_ref: &str,
    upstream_ref: Option<&str>,
) -> Result<Vec<OutgoingCommit>> {
    let range = upstream_ref.map(|upstream| format!("{upstream}..{branch_ref}"));
    let mut args = vec!["log", "--max-count=200", "--format=%H%x1f%an%x1f%s"];
    match &range {
        Some(range) => args.push(range),
        None => args.extend([branch_ref, "--not", "--remotes"]),
    }
    let output = git(repo, &args, &[]).await?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\u{1f}');
            Some(OutgoingCommit {
                sha: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                subject: fields.next()?.to_string(),
            })
        })
        .collect())
}

pub struct ChangedFile {
    pub status: char,
    pub path: String,
}

pub async fn commit_files(repo: &LocalRepo, sha: &str) -> Result<Vec<ChangedFile>> {
    // NUL-separated output is the only form that never quotes or escapes path names.
    let args = [
        "show",
        "--name-status",
        "-z",
        "--no-renames",
        "--format=",
        "-m",
        "--first-parent",
        sha,
    ];
    let output = git(repo, &args, &[]).await?;
    let output = String::from_utf8_lossy(&output);
    let mut fields = output.split('\0');
    let mut files = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        if let Some(status) = status.trim().chars().next() {
            files.push(ChangedFile {
                status,
                path: path.to_string(),
            });
        }
    }
    Ok(files)
}

async fn git_apply(repo: &LocalRepo, flags: &[&str], patch: &[u8]) -> Result<()> {
    let args = [&["apply"], flags, &["-"]].concat();
    git(repo, &args, patch).await?;
    Ok(())
}

async fn git(repo: &LocalRepo, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
    git_env(repo, args, stdin, &[]).await
}

async fn git_env(
    repo: &LocalRepo,
    args: &[&str],
    stdin: &[u8],
    envs: &[(&str, &str)],
) -> Result<Vec<u8>> {
    let mut command = new_command("git");
    // Mirrors the project's git backend: an untrusted repo must not run its own hooks or helpers,
    // and signature lines must not leak into parsed output.
    command.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "log.showSignature=false",
    ]);
    if !repo.trusted {
        for config in [
            "core.hooksPath=/dev/null",
            "core.sshCommand=ssh",
            "credential.helper=",
            "protocol.ext.allow=never",
            "diff.external=",
        ] {
            command.args(["-c", config]);
        }
    }
    let mut child = command
        .args(args)
        .current_dir(&repo.dir)
        // Nobody can answer a credentials prompt or an editor here, so fail instead of hanging.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .envs(envs.iter().copied())
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
mod tests;
