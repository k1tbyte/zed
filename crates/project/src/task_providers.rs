//! Task templates contributed by extensions.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use collections::HashMap;
use extension::{Extension, ExtensionHostProxy, ExtensionTaskProviderProxy, WorktreeDelegate};
use fs::Fs;
use futures::{
    FutureExt as _,
    future::{self, Either, Shared},
};
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Subscription, Task};
use settings::WorktreeId;
use task::TaskTemplate;
use util::{
    ResultExt as _,
    paths::{PathMatcher, PathStyle},
    rel_path::RelPath,
};
use worktree::Snapshot;

use crate::{
    ProjectEnvironment,
    task_inventory::TaskSourceKind,
    trusted_worktrees::TrustedWorktrees,
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};

const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);
const LIST_DIRECTORY_LIMIT: usize = 1000;

pub fn init(cx: &mut App) {
    let registry = TaskProviderRegistry::default_global(cx);
    ExtensionHostProxy::default_global(cx).register_task_provider_proxy(RegistryProxy { registry });
}

struct TaskProvider {
    extension: Arc<dyn Extension>,
    detect: PathMatcher,
}

#[derive(Default)]
struct TaskProviderRegistry {
    providers: BTreeMap<Arc<str>, Arc<TaskProvider>>,
}

struct GlobalTaskProviderRegistry(Entity<TaskProviderRegistry>);

impl Global for GlobalTaskProviderRegistry {}

impl TaskProviderRegistry {
    fn default_global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalTaskProviderRegistry>() {
            let registry = cx.new(|_| Self::default());
            cx.set_global(GlobalTaskProviderRegistry(registry));
        }
        cx.global::<GlobalTaskProviderRegistry>().0.clone()
    }
}

struct RegistryProxy {
    registry: Entity<TaskProviderRegistry>,
}

impl ExtensionTaskProviderProxy for RegistryProxy {
    fn register_task_provider(
        &self,
        extension: Arc<dyn Extension>,
        provider_name: Arc<str>,
        detect: Vec<String>,
        cx: &mut App,
    ) {
        let detect = PathMatcher::new_lenient(&detect, PathStyle::Unix, |error| {
            log::error!("invalid detect glob of task provider {provider_name}: {error}")
        });
        self.registry.update(cx, |registry, cx| {
            registry
                .providers
                .insert(provider_name, Arc::new(TaskProvider { extension, detect }));
            cx.notify();
        });
    }

    fn unregister_task_provider(&self, provider_name: Arc<str>, cx: &mut App) {
        self.registry.update(cx, |registry, cx| {
            registry.providers.remove(&provider_name);
            cx.notify();
        });
    }
}

/// Task templates of all registered providers for the worktrees of one project.
pub struct ExtensionTasks {
    registry: Entity<TaskProviderRegistry>,
    worktree_store: Entity<WorktreeStore>,
    environment: Entity<ProjectEnvironment>,
    // Dropping an entry is the only invalidation, so a stale run can never repopulate it.
    cache: HashMap<(Arc<str>, WorktreeId), Shared<Task<Arc<[TaskTemplate]>>>>,
    _subscriptions: [Subscription; 2],
}

impl ExtensionTasks {
    pub fn new(
        worktree_store: Entity<WorktreeStore>,
        environment: Entity<ProjectEnvironment>,
        cx: &mut Context<Self>,
    ) -> Self {
        let registry = TaskProviderRegistry::default_global(cx);
        let subscriptions = [
            cx.observe(&registry, |this, _, _| this.cache.clear()),
            cx.subscribe(&worktree_store, Self::on_worktree_store_event),
        ];
        Self {
            registry,
            worktree_store,
            environment,
            cache: HashMap::default(),
            _subscriptions: subscriptions,
        }
    }

    fn on_worktree_store_event(
        &mut self,
        _: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, changes) => {
                let providers = &self.registry.read(cx).providers;
                self.cache.retain(|(name, id), _| {
                    id != worktree_id
                        || !providers.get(name).is_some_and(|provider| {
                            changes
                                .iter()
                                .any(|(path, ..)| provider.detect.is_match(path.as_ref()))
                        })
                });
            }
            WorktreeStoreEvent::WorktreeRemoved(_, worktree_id) => {
                self.cache.retain(|(_, id), _| id != worktree_id);
            }
            _ => {}
        }
    }

    /// Providers run only for local, trusted worktrees. A failing or slow provider is skipped.
    pub fn templates(
        &mut self,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) -> Task<Vec<(TaskSourceKind, TaskTemplate)>> {
        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)
        else {
            return Task::ready(Vec::new());
        };
        let Some(fs) = worktree
            .read(cx)
            .as_local()
            .map(|local_worktree| local_worktree.fs().clone())
        else {
            return Task::ready(Vec::new());
        };
        let can_trust = TrustedWorktrees::try_get_global(cx).is_none_or(|trusted_worktrees| {
            trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                trusted_worktrees.can_trust(&self.worktree_store, worktree_id, cx)
            })
        });
        if !can_trust {
            return Task::ready(Vec::new());
        }

        let snapshot = worktree.read(cx).snapshot();
        let environment = self.environment.update(cx, |environment, cx| {
            environment.worktree_environment(worktree, cx)
        });
        let providers = self.registry.read(cx).providers.clone();
        let mut pending = Vec::with_capacity(providers.len());
        for (name, provider) in providers {
            let task = self
                .cache
                .entry((name.clone(), worktree_id))
                .or_insert_with(|| {
                    let delegate = Arc::new(ProjectWorktreeDelegate {
                        snapshot: snapshot.clone(),
                        fs: fs.clone(),
                        environment: environment.clone(),
                    });
                    let name = name.clone();
                    cx.background_spawn(async move { provider.templates(&name, delegate).await })
                        .shared()
                })
                .clone();
            pending.push((SharedString::from(name), task));
        }

        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let mut templates = Vec::new();
            for (provider, task) in pending {
                match future::select(task, executor.timer(PROVIDER_TIMEOUT)).await {
                    Either::Left((found, _)) => {
                        let kind = TaskSourceKind::Extension {
                            provider,
                            worktree: worktree_id,
                        };
                        templates.extend(
                            found
                                .iter()
                                .map(|template| (kind.clone(), template.clone())),
                        );
                    }
                    Either::Right(_) => log::warn!("task provider {provider} timed out"),
                }
            }
            templates
        })
    }
}

impl TaskProvider {
    async fn templates(
        &self,
        name: &Arc<str>,
        delegate: Arc<ProjectWorktreeDelegate>,
    ) -> Arc<[TaskTemplate]> {
        let has_input = delegate
            .snapshot
            .files(false, 0)
            .any(|entry| self.detect.is_match(entry.path.as_ref()));
        if !has_input {
            return Arc::default();
        }
        self.extension
            .provide_tasks(name.clone(), delegate)
            .await
            .with_context(|| format!("task provider {name} failed"))
            .log_err()
            .map_or_else(Arc::default, Into::into)
    }
}

/// Built per call from a worktree snapshot, so it never outlives the call's view of the files.
struct ProjectWorktreeDelegate {
    snapshot: Snapshot,
    fs: Arc<dyn Fs>,
    environment: Shared<Task<Option<HashMap<String, String>>>>,
}

#[async_trait]
impl WorktreeDelegate for ProjectWorktreeDelegate {
    fn id(&self) -> u64 {
        self.snapshot.id().to_proto()
    }

    fn root_path(&self) -> String {
        self.snapshot.abs_path().to_string_lossy().into_owned()
    }

    async fn read_text_file(&self, path: &RelPath) -> Result<String> {
        let entry = self
            .snapshot
            .entry_for_path(path)
            .with_context(|| format!("no worktree entry for path {path:?}"))?;
        self.fs.load(&self.snapshot.absolutize(&entry.path)).await
    }

    async fn which(&self, binary_name: String) -> Option<String> {
        let environment = self.environment.clone().await.unwrap_or_default();
        which::which_in(
            binary_name,
            environment.get("PATH"),
            self.snapshot.abs_path(),
        )
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
    }

    async fn shell_env(&self) -> Vec<(String, String)> {
        self.environment
            .clone()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    async fn list_directory(
        &self,
        path: &RelPath,
        max_entries: Option<u32>,
    ) -> Result<Vec<String>> {
        ensure!(
            self.snapshot
                .entry_for_path(path)
                .is_some_and(|entry| entry.is_dir()),
            "{path:?} is not a directory in the worktree"
        );
        let limit = max_entries.map_or(LIST_DIRECTORY_LIMIT, |max| {
            (max as usize).min(LIST_DIRECTORY_LIMIT)
        });
        Ok(self
            .snapshot
            .child_entries(path)
            // The iterator's own `include_ignored` filter skips its first entry.
            .filter(|entry| !entry.is_ignored || entry.is_always_included)
            .take(limit)
            .map(|entry| {
                let path = entry.path.as_unix_str();
                if entry.is_dir() {
                    format!("{path}/")
                } else {
                    path.to_string()
                }
            })
            .collect())
    }
}
