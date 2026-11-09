//! Edits the checked-out branch's history. Rewrites run `git rebase -i` with a todo we generate.

use super::{LocalRepo, git, git_env};
use anyhow::{Result, anyhow, bail};
use std::path::Path;

pub enum Rewrite {
    Reword {
        sha: String,
        message: String,
    },
    Drop {
        shas: Vec<String>,
    },
    /// Folds every commit into the oldest one; `None` keeps the oldest commit's message.
    Squash {
        shas: Vec<String>,
        message: Option<String>,
    },
}

impl Rewrite {
    fn shas(&self) -> &[String] {
        match self {
            Rewrite::Reword { sha, .. } => std::slice::from_ref(sha),
            Rewrite::Drop { shas } | Rewrite::Squash { shas, .. } => shas,
        }
    }

    fn message(&self) -> Option<&str> {
        match self {
            Rewrite::Reword { message, .. } => Some(message),
            Rewrite::Squash { message, .. } => message.as_deref(),
            Rewrite::Drop { .. } => None,
        }
    }
}

struct Commit {
    sha: String,
    parent_count: usize,
}

pub async fn rewrite(repo: &LocalRepo, request: Rewrite) -> Result<()> {
    ensure_idle(repo).await?;
    let selected = request.shas();
    let oldest = oldest_commit(repo, selected).await?;
    let (base, commits) = linear_range(repo, &oldest).await?;
    if let Some(missing) = selected
        .iter()
        .find(|sha| !commits.iter().any(|commit| &commit.sha == *sha))
    {
        bail!("{missing} is not on the current branch");
    }
    // Git would leave an empty root commit behind instead of an unborn branch.
    if base.is_none()
        && matches!(request, Rewrite::Drop { .. })
        && commits.iter().all(|commit| selected.contains(&commit.sha))
    {
        bail!("Dropping every commit would leave the branch empty");
    }

    let scratch = tempfile::tempdir()?;
    let message_file = scratch.path().join("message");
    std::fs::write(&message_file, request.message().unwrap_or_default())?;
    let todo_file = scratch.path().join("todo");
    std::fs::write(&todo_file, todo(&request, &commits, &message_file))?;

    let sequence_editor = format!("cp {}", shell_quote(&todo_file));
    let mut args = vec!["rebase", "-i", "--autostash"];
    args.push(base.as_deref().unwrap_or("--root"));
    let envs = [("GIT_SEQUENCE_EDITOR", sequence_editor.as_str())];
    let stash_before = stash_tip(repo).await;
    if let Err(error) = git_env(repo, &args, &[], &envs).await {
        return Err(undo_if_stopped(repo, "rebase", error).await);
    }
    // Git keeps the autostash when it cannot be reapplied and still exits successfully.
    if stash_tip(repo).await != stash_before {
        bail!(
            "The history was rewritten, but your uncommitted changes no longer apply to it. \
            They are kept in the stash and the files hold conflict markers."
        );
    }
    Ok(())
}

/// Applies `shas` (oldest first) on top of the current branch.
pub async fn cherry_pick(repo: &LocalRepo, shas: &[String]) -> Result<()> {
    sequence(repo, "cherry-pick", &[], shas).await
}

/// Reverts `shas` (newest first) with one new commit each.
pub async fn revert(repo: &LocalRepo, shas: &[String]) -> Result<()> {
    sequence(repo, "revert", &["--no-edit"], shas).await
}

#[derive(Clone, Copy)]
pub enum ResetMode {
    Hard,
    Keep,
}

pub async fn reset(repo: &LocalRepo, sha: &str, mode: ResetMode) -> Result<()> {
    let flag = match mode {
        ResetMode::Hard => "--hard",
        ResetMode::Keep => "--keep",
    };
    git(repo, &["reset", flag, sha], &[]).await?;
    Ok(())
}

/// Full messages, in the order of `shas`.
pub async fn commit_messages(repo: &LocalRepo, shas: &[String]) -> Result<Vec<String>> {
    let mut args = vec!["log", "-z", "--no-walk=unsorted", "--format=%B"];
    args.extend(shas.iter().map(String::as_str));
    let output = git(repo, &args, &[]).await?;
    Ok(String::from_utf8_lossy(&output)
        .split('\0')
        .take(shas.len())
        .map(|message| message.trim_end().to_string())
        .collect())
}

/// A remote branch that already has the commit, which a rewrite would force you to force-push.
pub async fn pushed_to(repo: &LocalRepo, sha: &str) -> Result<Option<String>> {
    let output = git(
        repo,
        &[
            "for-each-ref",
            "--contains",
            sha,
            "--format=%(refname)",
            "refs/remotes",
        ],
        &[],
    )
    .await?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|name| name.strip_prefix("refs/remotes/"))
        .find(|name| !name.ends_with("/HEAD"))
        .map(str::to_string))
}

async fn sequence(repo: &LocalRepo, command: &str, flags: &[&str], shas: &[String]) -> Result<()> {
    ensure_idle(repo).await?;
    let mut args = vec![command];
    args.extend(flags);
    args.extend(shas.iter().map(String::as_str));
    match git(repo, &args, &[]).await {
        Ok(_) => Ok(()),
        Err(error) => Err(undo_if_stopped(repo, command, error).await),
    }
}

// Git finds a common ancestor of commits on one chain in the oldest of them.
async fn oldest_commit(repo: &LocalRepo, shas: &[String]) -> Result<String> {
    let mut args = vec!["merge-base", "--octopus"];
    args.extend(shas.iter().map(String::as_str));
    let output = git(repo, &args, &[])
        .await
        .map_err(|_| anyhow!("The selected commits do not share a history"))?;
    let oldest = String::from_utf8_lossy(&output).trim().to_string();
    if !shas.contains(&oldest) {
        bail!("The selected commits are not on one line of history");
    }
    Ok(oldest)
}

/// Commits from `oldest` to HEAD, oldest first, plus the rebase base (None when `oldest` is a root).
async fn linear_range(repo: &LocalRepo, oldest: &str) -> Result<(Option<String>, Vec<Commit>)> {
    let output = git(repo, &["rev-list", "--parents", "-n1", oldest], &[]).await?;
    let base = String::from_utf8_lossy(&output)
        .split_whitespace()
        .nth(1)
        .map(str::to_string);
    let range = base
        .as_ref()
        .map_or("HEAD".to_string(), |base| format!("{base}..HEAD"));
    let output = git(repo, &["rev-list", "--reverse", "--parents", &range], &[]).await?;
    let commits: Vec<Commit> = String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Commit {
                sha: fields.next()?.to_string(),
                parent_count: fields.count(),
            })
        })
        .collect();
    if commits.first().map(|commit| commit.sha.as_str()) != Some(oldest) {
        bail!("{oldest} is not on the current branch");
    }
    if commits.iter().any(|commit| commit.parent_count > 1) {
        bail!("A merge commit lies in this range and a rebase would flatten it");
    }
    Ok((base, commits))
}

fn todo(rewrite: &Rewrite, commits: &[Commit], message_file: &Path) -> String {
    let selected = rewrite.shas();
    let is_selected = |commit: &&Commit| selected.contains(&commit.sha);
    let set_message = format!(
        "exec git commit --amend --allow-empty --no-verify -F {}\n",
        shell_quote(message_file)
    );
    let mut lines = String::new();
    match rewrite {
        Rewrite::Reword { .. } => {
            for commit in commits {
                lines.push_str(&format!("pick {}\n", commit.sha));
                if is_selected(&commit) {
                    lines.push_str(&set_message);
                }
            }
        }
        Rewrite::Drop { .. } => {
            for commit in commits {
                let action = if is_selected(&commit) { "drop" } else { "pick" };
                lines.push_str(&format!("{action} {}\n", commit.sha));
            }
        }
        Rewrite::Squash { .. } => {
            // Unselected commits in between keep their order but move behind the folded one.
            for (index, commit) in commits.iter().filter(is_selected).enumerate() {
                let action = if index == 0 { "pick" } else { "fixup" };
                lines.push_str(&format!("{action} {}\n", commit.sha));
            }
            if rewrite.message().is_some() {
                lines.push_str(&set_message);
            }
            for commit in commits.iter().filter(|commit| !is_selected(commit)) {
                lines.push_str(&format!("pick {}\n", commit.sha));
            }
        }
    }
    lines
}

async fn stash_tip(repo: &LocalRepo) -> Option<String> {
    let output = git(repo, &["rev-parse", "-q", "--verify", "refs/stash"], &[])
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&output).trim().to_string())
}

pub(crate) async fn operation_in_progress(repo: &LocalRepo) -> Result<bool> {
    let output = git(repo, &["rev-parse", "--git-dir"], &[]).await?;
    let git_dir = repo.dir.join(String::from_utf8_lossy(&output).trim());
    Ok([
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "sequencer",
    ]
    .iter()
    .any(|name| git_dir.join(name).exists()))
}

// Also what keeps `undo_if_stopped` from aborting an operation that was not ours.
async fn ensure_idle(repo: &LocalRepo) -> Result<()> {
    if operation_in_progress(repo).await? {
        bail!("Finish or abort the rebase, merge, cherry-pick or revert in progress first");
    }
    Ok(())
}

// Zed has no way to continue a stopped operation, so a conflict is rolled back instead.
async fn undo_if_stopped(repo: &LocalRepo, command: &str, error: anyhow::Error) -> anyhow::Error {
    if !operation_in_progress(repo).await.unwrap_or(false) {
        return error;
    }
    let message = error.to_string();
    let reason = message
        .lines()
        .find(|line| line.starts_with("error:"))
        .unwrap_or_else(|| message.lines().next().unwrap_or_default());
    match git(repo, &[command, "--abort"], &[]).await {
        Ok(_) => anyhow!("Stopped on a conflict and rolled back, nothing changed. {reason}"),
        Err(abort_error) => anyhow!("{message}\nRolling back failed: {abort_error}"),
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}
