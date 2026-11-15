use anyhow::Result;
use gpui::{App, AppContext as _, Entity, SharedString, Task};
use project::{Project, ProjectEntryId, ProjectPath};
use settings::Settings as _;

use super::{EnvironmentFile, SecretsGroup, read_state};
use crate::{SopsSettings, environments::FilePatterns};

impl project::ProjectItem for SecretsGroup {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if !project.read(cx).is_local() {
            return None;
        }
        let patterns = FilePatterns::new(&SopsSettings::get_global(cx).file_patterns);
        let opened = patterns.classify(path.path.as_unix_str())?;
        let worktree = project.read(cx).worktree_for_id(path.worktree_id, cx)?;
        let snapshot = worktree.read(cx).snapshot();

        let title = SharedString::from(opened.group.title().to_string());
        let entry_id = snapshot.entry_for_path(&path.path).map(|entry| entry.id);
        let opened_path = path.clone();
        let fs = project.read(cx).fs().clone();
        let files = cx.background_spawn(async move {
            let mut candidates = vec![(opened_path.path.clone(), opened.clone())];
            // Siblings are found by scanning, so keep it off the main thread for large worktrees.
            if opened.environment.is_some() {
                for entry in snapshot.files(true, 0) {
                    if entry.path == opened_path.path {
                        continue;
                    }
                    if let Some(file) = patterns.classify(entry.path.as_unix_str())
                        && file.group == opened.group
                    {
                        candidates.push((entry.path.clone(), file));
                    }
                }
            }
            candidates.sort_by(|(_, left), (_, right)| left.environment.cmp(&right.environment));
            let files: Vec<_> = candidates
                .into_iter()
                .map(|(relative_path, file)| {
                    let name = file.environment.unwrap_or_else(|| {
                        relative_path.file_name().unwrap_or_default().to_string()
                    });
                    (
                        SharedString::from(name),
                        snapshot.absolutize(&relative_path),
                        file.format,
                    )
                })
                .collect();
            (opened_path, files)
        });

        Some(cx.spawn(async move |cx| {
            let (opened_path, files) = files.await;
            let mut environments = Vec::new();
            for (name, abs_path, format) in files {
                let (state, recipients) = read_state(fs.as_ref(), &abs_path, format).await;
                environments.push(EnvironmentFile {
                    name,
                    abs_path,
                    format,
                    state,
                    recipients,
                });
            }
            Ok(cx.new(|_| SecretsGroup::new(title, opened_path, entry_id, environments)))
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.opened.clone())
    }

    fn is_dirty(&self) -> bool {
        SecretsGroup::is_dirty(self)
    }
}
