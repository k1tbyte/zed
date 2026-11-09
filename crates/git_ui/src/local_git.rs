//! Where `git_ops` work meets a `Repository`: whether it may run, and how it is queued.

use gpui::{App, AppContext as _, Entity, SharedString, Task};
use project::git_store::Repository;
use std::future::Future;

/// None for remote and collab projects, where the host's path is no checkout on this machine.
pub(crate) fn local_repo(
    repository: &Entity<Repository>,
    cx: &mut App,
) -> Option<git_ops::LocalRepo> {
    let (id, dir, git_store) = {
        let repository = repository.read(cx);
        (
            repository.id,
            repository.work_directory_abs_path.clone(),
            repository.git_store()?,
        )
    };
    git_store.update(cx, |git_store, cx| {
        git_store.is_local().then(|| git_ops::LocalRepo {
            dir,
            trusted: git_store.repository_is_trusted(id, cx),
        })
    })
}

/// Runs `work` behind the repository's own jobs, so it cannot overlap a Git panel operation.
pub(crate) fn queue_git_job(
    repository: &Entity<Repository>,
    job: &'static str,
    status: impl Into<SharedString>,
    work: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    cx: &mut App,
) -> Task<anyhow::Result<()>> {
    let queued = repository.update(cx, |repository, _| {
        repository.send_job(job, Some(status.into()), move |_, cx| async move {
            cx.background_spawn(work).await
        })
    });
    cx.spawn(async move |_| {
        queued
            .await
            .map_err(|_| anyhow::anyhow!("Operation was canceled"))?
    })
}
