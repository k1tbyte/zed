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

/// Why git refused, when the user can get past it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocker {
    LocalChanges,
    Diverged,
}

pub fn blocker(error: &anyhow::Error) -> Option<Blocker> {
    let message = format!("{error:#}");
    if message.contains("Your local changes to the following files would be overwritten")
        || message.contains("cannot pull with rebase")
    {
        Some(Blocker::LocalChanges)
    } else if message.contains("Need to specify how to reconcile divergent branches")
        || message.contains("Not possible to fast-forward")
    {
        Some(Blocker::Diverged)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateOptions {
    pub autostash: bool,
    pub reconcile: Option<Reconcile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconcile {
    Merge,
    Rebase,
}

pub async fn update_branch(
    repo: &LocalRepo,
    branch: &str,
    remote: &str,
    remote_branch: &str,
    is_checked_out: bool,
    options: UpdateOptions,
) -> Result<()> {
    if is_checked_out {
        let mut args = vec!["pull"];
        if options.autostash {
            args.push("--autostash");
        }
        match options.reconcile {
            // `--ff` overrides a configured `pull.ff=only`.
            Some(Reconcile::Merge) => args.extend(["--no-rebase", "--ff"]),
            Some(Reconcile::Rebase) => args.push("--rebase"),
            None => {}
        }
        // Named explicitly so a branch without an upstream can be pulled too.
        let source = format!("refs/heads/{remote_branch}");
        args.extend([remote, &source]);
        git(repo, &args, &[]).await?;
    } else {
        // Full refs keep a same-named tag from winning. No leading `+`, so anything but a
        // fast-forward is refused and local commits stay safe.
        let refspec = format!("refs/heads/{remote_branch}:refs/heads/{branch}");
        git(repo, &["fetch", remote, &refspec], &[]).await?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalChanges {
    Refuse,
    Keep,
    Discard,
}

pub async fn checkout_branch(
    repo: &LocalRepo,
    branch: &str,
    local_changes: LocalChanges,
) -> Result<()> {
    match local_changes {
        LocalChanges::Refuse => git(repo, &["switch", branch], &[]).await.map(drop),
        LocalChanges::Discard => git(repo, &["switch", "--discard-changes", branch], &[])
            .await
            .map(drop),
        LocalChanges::Keep => switch_keeping_changes(repo, branch).await,
    }
}

async fn switch_keeping_changes(repo: &LocalRepo, branch: &str) -> Result<()> {
    let stash_before = stash_head(repo).await;
    let message = format!("Checkout {branch}");
    git(repo, &["stash", "push", "--message", &message], &[]).await?;
    // Nothing to save still exits 0, so compare to avoid popping an older stash.
    let stashed = stash_head(repo).await != stash_before;

    if let Err(switch_error) = git(repo, &["switch", branch], &[]).await {
        if stashed && let Err(pop_error) = git(repo, &["stash", "pop"], &[]).await {
            return Err(switch_error.context(format!("Restoring the stash failed: {pop_error}")));
        }
        return Err(switch_error);
    }
    if stashed {
        git(repo, &["stash", "pop"], &[]).await.context(format!(
            "Checked out {branch}, but the stashed changes conflict with it. They are kept in the stash."
        ))?;
    }
    Ok(())
}

async fn stash_head(repo: &LocalRepo) -> Option<String> {
    let output = git(repo, &["rev-parse", "-q", "--verify", "refs/stash"], &[])
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&output).trim().to_string())
}

pub async fn delete_remote_branch(repo: &LocalRepo, remote: &str, branch: &str) -> Result<()> {
    let full_ref = format!("refs/heads/{branch}");
    match git(repo, &["push", remote, "--delete", &full_ref], &[]).await {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("remote ref does not exist") => {
            // Already gone on the remote, so only the stale tracking ref is left.
            let tracking = format!("{remote}/{branch}");
            git(repo, &["branch", "-d", "-r", &tracking], &[]).await?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub async fn prune_remote_branches(repo: &LocalRepo) -> Result<()> {
    git(repo, &["fetch", "--all", "--prune"], &[]).await?;
    Ok(())
}

/// Returns full ref names such as `refs/heads/topic`.
pub async fn merged_branches(repo: &LocalRepo, target: &str) -> Result<Vec<String>> {
    let merged = format!("--merged={target}");
    let output = git(
        repo,
        &[
            "for-each-ref",
            &merged,
            "--format=%(refname)",
            "refs/heads/",
        ],
        &[],
    )
    .await?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .map(str::to_string)
        .collect())
}

// Refreshes only this branch's remote-tracking ref, not everything the remote has.
pub async fn fetch_branch(repo: &LocalRepo, remote: &str, branch: &str) -> Result<()> {
    // Full refs keep a same-named tag from winning. The leading `+` matches a regular fetch,
    // which also accepts a rewritten remote branch.
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    git(repo, &["fetch", remote, &refspec], &[]).await?;
    Ok(())
}

pub async fn rebase_onto(repo: &LocalRepo, onto: &str) -> Result<()> {
    history::ensure_idle(repo).await?;
    history::rebase(repo, &[onto], &[]).await
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

/// Removes paths from the index and keeps them on disk. Untracked paths are skipped.
pub async fn untrack(repo: &LocalRepo, paths: &[String]) -> Result<()> {
    let mut args = vec![
        "--literal-pathspecs",
        "rm",
        "--cached",
        "-r",
        "-q",
        "--ignore-unmatch",
        "--",
    ];
    args.extend(paths.iter().map(String::as_str));
    git(repo, &args, &[]).await?;
    Ok(())
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
