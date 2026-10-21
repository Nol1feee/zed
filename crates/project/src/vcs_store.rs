//! External source-control repositories, scoped to opened worktrees.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use buffer_diff::{BufferDiff, DiffOperations};
use collections::{HashMap, HashSet};
use extension::{Extension, ExtensionHostProxy, ExtensionVcsProviderProxy, WorktreeDelegate};
use fs::Fs;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Global, ReadGlobal, SharedString, Subscription,
    Task, TaskExt as _, WeakEntity,
};
use language::{Buffer, BufferEvent};
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsLocation, WorktreeId};
use util::{ResultExt, rel_path::RelPath};
use vcs::{
    ChangeStatus, CheckoutCatalog, CheckoutDescriptor, CheckoutOperation, CheckoutOperationKind,
    CheckoutOperationResult, CheckoutState, Content, FileOperation, FileOperationOutcome,
    OperationProgress, ProviderCommand, ProviderConnection, ProviderNotification,
    RepositoryDescriptor, RepositoryOperation, RepositoryOperationKind, RepositoryState, Snapshot,
};

use crate::{
    Project, ProjectEnvironment, ProjectItem, ProjectPath,
    git_store::GitStore,
    trusted_worktrees::TrustedWorktrees,
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};

#[path = "vcs_operation_journal.rs"]
mod operation_journal;
use operation_journal::OperationJournal;
#[path = "vcs_checkout_operations.rs"]
mod checkout_operations;
#[path = "vcs_repository_operations.rs"]
mod repository_operations;

#[derive(Clone)]
struct Registration {
    extension: Arc<dyn Extension>,
    provider_id: Arc<str>,
}

type ProviderKey = (Arc<str>, Arc<str>);
type SessionKey = (WorktreeId, ProviderKey);

#[derive(Default)]
struct Registry {
    registrations: BTreeMap<ProviderKey, Registration>,
    stores: Vec<WeakEntity<VcsStore>>,
}
impl EventEmitter<()> for Registry {}
struct GlobalRegistry(Entity<Registry>);
impl Global for GlobalRegistry {}

struct RegistryProxy(Entity<Registry>);
impl ExtensionVcsProviderProxy for RegistryProxy {
    fn register_vcs_provider(
        &self,
        extension: Arc<dyn Extension>,
        provider_id: Arc<str>,
        cx: &mut App,
    ) {
        let key = (extension.manifest().id.clone(), provider_id.clone());
        log::info!(
            "Registered VCS provider: extension={}, provider={}",
            key.0,
            key.1
        );
        self.0.update(cx, |registry, cx| {
            registry.registrations.insert(
                key,
                Registration {
                    extension,
                    provider_id,
                },
            );
            cx.emit(());
        });
    }

    fn unregister_vcs_provider(&self, extension_id: Arc<str>, provider_id: Arc<str>, cx: &mut App) {
        log::info!("Unregistered VCS provider: extension={extension_id}, provider={provider_id}");
        self.0.update(cx, |registry, cx| {
            registry.registrations.remove(&(extension_id, provider_id));
            cx.emit(());
        });
    }
}

pub fn init(cx: &mut App) {
    if cx.try_global::<GlobalRegistry>().is_none() {
        let registry = cx.new(|_| Registry::default());
        ExtensionHostProxy::default_global(cx)
            .register_vcs_provider_proxy(RegistryProxy(registry.clone()));
        cx.set_global(GlobalRegistry(registry));
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VcsRepositoryId {
    pub worktree_id: WorktreeId,
    pub extension_id: Arc<str>,
    pub provider_id: Arc<str>,
    pub repository_id: String,
}

#[derive(Clone, Debug)]
pub struct VcsRepositorySnapshot {
    pub key: VcsRepositoryId,
    pub descriptor: RepositoryDescriptor,
    pub selected_baseline: String,
    pub snapshot: Option<Arc<Snapshot>>,
    pub loading: bool,
    pub show_loading: bool,
    pub error: Option<SharedString>,
    pub repository_state: Option<Arc<RepositoryState>>,
    pub repository_state_error: Option<SharedString>,
    pub checkouts: Option<Arc<CheckoutCatalog>>,
    pub checkout_error: Option<SharedString>,
    pub checkout_loading: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefreshReason {
    Explicit,
    Background,
}

#[derive(Clone, Debug)]
pub enum VcsStoreEvent {
    Updated,
    DiffChanged { worktree_id: WorktreeId },
    BlameChanged { worktree_id: WorktreeId },
}

pub struct VcsDiff {
    pub buffer: Entity<Buffer>,
    pub diff: Entity<BufferDiff>,
    pub current_content: vcs::ContentSource,
}

struct ComparisonDiff {
    buffer: WeakEntity<Buffer>,
    diff: WeakEntity<BufferDiff>,
    base: Option<Arc<str>>,
    generation: u64,
    update_task: Option<Task<()>>,
    waiters: Vec<futures::channel::oneshot::Sender<()>>,
    _subscription: Subscription,
}

struct Repository {
    view: VcsRepositorySnapshot,
    connection: Arc<ProviderConnection>,
    generation: u64,
    refresh_task: Option<Task<()>>,
    blame_head: Option<String>,
    blame_generation: u64,
    blames: HashMap<String, Arc<vcs::Blame>>,
    checkout_generation: u64,
    checkout_task: Option<Task<()>>,
    comparisons: HashMap<String, Arc<Snapshot>>,
    comparison_generations: HashMap<String, u64>,
    comparison_diffs: HashMap<(String, String), ComparisonDiff>,
}

struct Session {
    generation: u64,
    claims_scope: bool,
    connection: Option<Arc<ProviderConnection>>,
    _start_task: Task<()>,
    notification_task: Option<Task<()>>,
}

#[derive(Clone, Deserialize, Serialize)]
struct PendingFileOperation {
    path: String,
    operation: FileOperation,
    token: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum OperationRequest {
    File {
        path: String,
        operation: FileOperation,
        #[serde(default)]
        affected_paths: Vec<String>,
    },
    Repository {
        operation: RepositoryOperation,
        affected_paths: Vec<String>,
    },
    Checkout {
        operation: CheckoutOperation,
        target_root_uri: String,
        checkout: Option<CheckoutDescriptor>,
    },
}

impl OperationRequest {
    fn is_root_wide(&self) -> bool {
        matches!(self, Self::Repository { .. } | Self::Checkout { .. })
    }

    fn roots<'a>(&'a self, scope: &'a FileOperationScope) -> Vec<&'a str> {
        let mut roots = vec![scope.root_uri.as_str()];
        if let Self::Checkout {
            target_root_uri, ..
        } = self
        {
            roots.push(target_root_uri);
        }
        roots
    }

    fn normalize(&mut self) -> Result<()> {
        if let Self::File {
            path,
            affected_paths,
            ..
        } = self
        {
            if affected_paths.is_empty() {
                affected_paths.push(path.clone());
            }
        }
        if let Self::Checkout {
            target_root_uri, ..
        } = self
        {
            *target_root_uri = url::Url::from_directory_path(vcs::local_path(target_root_uri)?)
                .map_err(|()| anyhow!("invalid checkout target root"))?
                .to_string();
        }
        self.validate()
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::File {
                path,
                affected_paths,
                ..
            } => {
                vcs::validate_path(path)?;
                vcs::validate_paths(affected_paths)?;
                ensure!(
                    affected_paths.contains(path),
                    "Invalid file recovery affected paths"
                );
                Ok(())
            }
            Self::Repository {
                operation,
                affected_paths,
            } => {
                operation.validate()?;
                ensure!(
                    !matches!(operation, RepositoryOperation::Commit { .. })
                        || !affected_paths.is_empty(),
                    "Invalid staged commit recovery paths"
                );
                vcs::validate_paths(affected_paths)
            }
            Self::Checkout {
                operation,
                target_root_uri,
                checkout,
            } => {
                operation.validate()?;
                let root = vcs::local_path(target_root_uri)?;
                if let Some(checkout) = checkout {
                    checkout.validate()?;
                    ensure!(
                        vcs::local_path(&checkout.root_uri)? == root,
                        "Checkout recovery target changed"
                    );
                }
                if let Some(identifier) = operation.checkout_id() {
                    ensure!(
                        checkout
                            .as_ref()
                            .is_some_and(|checkout| checkout.id == identifier),
                        "Checkout recovery identity changed"
                    );
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct PendingOperation {
    token: String,
    request: OperationRequest,
}

impl From<PendingFileOperation> for PendingOperation {
    fn from(pending: PendingFileOperation) -> Self {
        Self {
            token: pending.token,
            request: OperationRequest::File {
                path: pending.path.clone(),
                operation: pending.operation,
                affected_paths: vec![pending.path],
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
struct FileOperationScope {
    extension_id: Arc<str>,
    provider_id: Arc<str>,
    repository_id: String,
    root_uri: String,
    scope_uri: String,
}

impl FileOperationScope {
    fn normalize(&mut self) -> Result<()> {
        self.root_uri = url::Url::from_directory_path(vcs::local_path(&self.root_uri)?)
            .map_err(|()| anyhow!("invalid VCS recovery root"))?
            .to_string();
        self.scope_uri = url::Url::from_directory_path(vcs::local_path(&self.scope_uri)?)
            .map_err(|()| anyhow!("invalid VCS recovery scope"))?
            .to_string();
        Ok(())
    }

    fn same_provider(&self, other: &Self) -> bool {
        self.extension_id == other.extension_id && self.provider_id == other.provider_id
    }
}

pub struct VcsPreparedFileOperation {
    key: VcsRepositoryId,
    baseline: String,
    scope: FileOperationScope,
    path: String,
    operation: FileOperation,
    token: String,
    affected_paths: Vec<String>,
    summary: Option<String>,
    change: vcs::Change,
    connection: Arc<ProviderConnection>,
}

struct StagingBatch {
    scope: FileOperationScope,
    owner: Arc<()>,
}

impl VcsPreparedFileOperation {
    pub fn affected_paths(&self) -> &[String] {
        &self.affected_paths
    }
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }
}

pub struct VcsPreparedRepositoryOperation {
    key: VcsRepositoryId,
    scope: FileOperationScope,
    operation: RepositoryOperation,
    prepared: vcs::PreparedRepositoryOperation,
    connection: Arc<ProviderConnection>,
}

impl VcsPreparedRepositoryOperation {
    pub fn summary(&self) -> &str {
        &self.prepared.summary
    }

    pub fn affected_paths(&self) -> &[String] {
        &self.prepared.affected_paths
    }
}

pub struct VcsPreparedCheckoutOperation {
    key: VcsRepositoryId,
    scope: FileOperationScope,
    operation: CheckoutOperation,
    prepared: vcs::PreparedCheckoutOperation,
    connection: Arc<ProviderConnection>,
}

impl VcsPreparedCheckoutOperation {
    pub fn summary(&self) -> &str {
        &self.prepared.summary
    }
    pub fn checkout(&self) -> Option<&CheckoutDescriptor> {
        self.prepared.checkout.as_ref()
    }
    pub fn target_root_uri(&self) -> &str {
        &self.prepared.target_root_uri
    }

    fn request(&self) -> OperationRequest {
        OperationRequest::Checkout {
            operation: self.operation.clone(),
            target_root_uri: self.prepared.target_root_uri.clone(),
            checkout: self.prepared.checkout.clone(),
        }
    }
}

struct DiffState {
    repository: VcsRepositoryId,
    path: String,
    buffer: WeakEntity<Buffer>,
    diff: Entity<BufferDiff>,
    base: Option<Arc<str>>,
    base_loaded: bool,
    loading: bool,
    generation: u64,
    update_task: Option<Task<()>>,
    waiters: Vec<futures::channel::oneshot::Sender<Result<Entity<BufferDiff>, Arc<anyhow::Error>>>>,
    _subscription: Subscription,
}

impl DiffState {
    fn finish_waiters(&mut self, result: Result<Entity<BufferDiff>>) {
        let result = result.map_err(Arc::new);
        for waiter in self.waiters.drain(..) {
            if waiter.send(result.clone()).is_err() {
                log::debug!("VCS diff consumer closed");
            }
        }
    }
}

struct ReadOnlyDiffOperations;
impl DiffOperations for ReadOnlyDiffOperations {
    fn supports_staging(&self) -> bool {
        false
    }
    fn supports_unstaging(&self) -> bool {
        false
    }
    fn supports_restore(&self) -> bool {
        false
    }
}

pub struct VcsStore {
    project: WeakEntity<Project>,
    worktrees: Entity<WorktreeStore>,
    git: Entity<GitStore>,
    environment: Entity<ProjectEnvironment>,
    fs: Arc<dyn Fs>,
    sessions: HashMap<SessionKey, Session>,
    repositories: HashMap<VcsRepositoryId, Repository>,
    selected_baselines: HashMap<VcsRepositoryId, String>,
    diffs: HashMap<text::BufferId, DiffState>,
    generation: u64,
    discovery_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    errors: HashMap<SessionKey, SharedString>,
    file_operation_running: bool,
    staging_batch: Option<StagingBatch>,
    operation_journal: Arc<OperationJournal>,
    pending_file_operations: HashMap<FileOperationScope, PendingOperation>,
    file_operation_errors: HashMap<VcsRepositoryId, SharedString>,
    operation_progress: HashMap<VcsRepositoryId, OperationProgress>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<VcsStoreEvent> for VcsStore {}

impl VcsStore {
    pub fn new(
        project: WeakEntity<Project>,
        worktrees: Entity<WorktreeStore>,
        git: Entity<GitStore>,
        environment: Entity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        init(cx);
        let registry = GlobalRegistry::global(cx).0.clone();
        let this = cx.entity().downgrade();
        registry.update(cx, |registry, _| {
            registry.stores.retain(|store| store.upgrade().is_some());
            registry.stores.push(this);
        });
        let mut subscriptions = vec![
            cx.subscribe(&registry, |this, _, _, cx| {
                this.stop_all(cx);
                this.schedule_discovery(cx);
            }),
            cx.subscribe(&worktrees, |this, _, event, cx| match event {
                WorktreeStoreEvent::WorktreeAdded(_)
                | WorktreeStoreEvent::WorktreeUpdatedGitRepositories(_, _) => {
                    this.schedule_discovery(cx)
                }
                WorktreeStoreEvent::WorktreeRemoved(_, worktree_id)
                | WorktreeStoreEvent::WorktreeReleased(_, worktree_id) => {
                    this.selected_baselines
                        .retain(|key, _| key.worktree_id != *worktree_id);
                    this.schedule_discovery(cx);
                }
                WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, _) => {
                    let keys: Vec<_> = this
                        .repositories
                        .keys()
                        .filter(|key| key.worktree_id == *worktree_id)
                        .cloned()
                        .collect();
                    for key in keys {
                        this.refresh_with_reason(&key, RefreshReason::Background, cx);
                    }
                    if !this
                        .repositories
                        .keys()
                        .any(|key| key.worktree_id == *worktree_id)
                    {
                        this.schedule_discovery(cx);
                    }
                }
                _ => {}
            }),
        ];
        if let Some(trust) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.subscribe(&trust, |this, _, _, cx| this.schedule_discovery(cx)));
        }
        let operation_journal = OperationJournal::global(fs.clone(), cx);
        let mut store = Self {
            project,
            worktrees,
            git,
            environment,
            fs,
            sessions: HashMap::default(),
            repositories: HashMap::default(),
            selected_baselines: HashMap::default(),
            diffs: HashMap::default(),
            generation: 0,
            discovery_task: None,
            poll_task: None,
            errors: HashMap::default(),
            file_operation_running: false,
            staging_batch: None,
            operation_journal,
            pending_file_operations: HashMap::default(),
            file_operation_errors: HashMap::default(),
            operation_progress: HashMap::default(),
            _subscriptions: subscriptions,
        };
        store.schedule_discovery(cx);
        store
    }

    pub fn repositories(&self) -> Vec<VcsRepositorySnapshot> {
        let mut repositories: Vec<_> = self
            .repositories
            .values()
            .map(|repository| repository.view.clone())
            .collect();
        repositories.sort_by(|left, right| {
            left.descriptor
                .scope_uri
                .cmp(&right.descriptor.scope_uri)
                .then_with(|| {
                    left.descriptor
                        .display_name
                        .cmp(&right.descriptor.display_name)
                })
        });
        repositories
    }

    pub fn errors(&self) -> Vec<SharedString> {
        self.errors.values().cloned().collect()
    }

    pub fn diagnostics(&self) -> Vec<SharedString> {
        self.sessions
            .values()
            .filter_map(|session| session.connection.as_ref())
            .map(|connection| connection.diagnostics())
            .filter(|message| !message.is_empty())
            .map(Into::into)
            .collect()
    }

    fn notify(&self, cx: &mut Context<Self>) {
        cx.emit(VcsStoreEvent::Updated);
        cx.notify();
    }

    fn schedule_discovery(&mut self, cx: &mut Context<Self>) {
        self.discovery_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            this.update(cx, |this, cx| this.discover(cx)).log_err();
        }));
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        let registrations = GlobalRegistry::global(cx).0.read(cx).registrations.clone();
        let roots: Vec<_> = self
            .worktrees
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| {
                let worktree = worktree.read(cx);
                (worktree.id(), worktree.abs_path())
            })
            .collect();
        let mut desired = std::collections::HashSet::new();
        for (worktree_id, root) in roots {
            let root_path = ProjectPath {
                worktree_id,
                path: Arc::from(RelPath::empty()),
            };
            if self
                .git
                .read(cx)
                .repository_and_path_for_project_path(&root_path, cx)
                .is_some()
                || !self.trusted(worktree_id, cx)
            {
                continue;
            }
            for (provider, registration) in &registrations {
                let key = (worktree_id, provider.clone());
                desired.insert(key.clone());
                if !self.sessions.contains_key(&key) {
                    self.start_session(key, root.clone(), registration.clone(), cx);
                }
            }
        }
        let obsolete: Vec<_> = self
            .sessions
            .keys()
            .filter(|key| !desired.contains(*key))
            .cloned()
            .collect();
        for key in obsolete {
            self.stop_session(&key, cx);
        }
    }

    fn trusted(&self, worktree_id: WorktreeId, cx: &mut App) -> bool {
        TrustedWorktrees::try_get_global(cx).is_some_and(|trust| {
            trust.update(cx, |trust, cx| {
                trust.can_trust(&self.worktrees, worktree_id, cx)
            })
        })
    }

    fn start_session(
        &mut self,
        key: SessionKey,
        root: Arc<Path>,
        registration: Registration,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        let fs = self.fs.clone();
        let environment = self.environment.clone();
        let task_key = key.clone();
        let operation_journal = self.operation_journal.clone();
        let task = cx.spawn(async move |this, cx| {
            if let Err(error) = operation_journal.load().await {
                log::warn!("VCS recovery journal is unavailable: {error:#}");
            }
            let result: Result<Option<(Arc<ProviderConnection>, Vec<RepositoryDescriptor>)>> =
                async {
                    let markers = registration
                        .extension
                        .manifest()
                        .vcs_providers
                        .get(&registration.provider_id)
                        .context("VCS provider was unloaded")?
                        .root_markers
                        .clone();
                    if !matches_markers(&root, &markers, &fs).await? {
                        return Ok(None);
                    }
                    this.update(cx, |this, _| {
                        if let Some(session) = this.sessions.get_mut(&task_key) {
                            session.claims_scope = true;
                        }
                    })?;
                    let shell_env = environment
                        .update(cx, |environment, cx| {
                            environment.directory_environment(root.clone(), cx)
                        })
                        .await
                        .unwrap_or_default();
                    ensure!(
                        this.update(cx, |this, cx| this.trusted(task_key.0, cx))?,
                        "VCS worktree is not trusted"
                    );
                    let delegate = Arc::new(ProviderWorktree {
                        id: task_key.0,
                        root: root.clone(),
                        fs,
                        environment: shell_env.clone(),
                    });
                    let command = registration
                        .extension
                        .vcs_provider_command(registration.provider_id, delegate)
                        .await?;
                    ensure!(
                        this.update(cx, |this, cx| this.trusted(task_key.0, cx))?,
                        "VCS worktree trust was revoked"
                    );
                    let mut environment: HashMap<_, _> = shell_env.into_iter().collect();
                    environment.extend(command.env);
                    let command = ProviderCommand {
                        path: registration.extension.path_from_extension(&command.command),
                        arguments: command.args.into_iter().map(Into::into).collect(),
                        env: environment,
                    };
                    let connection = ProviderConnection::start(
                        command,
                        &root,
                        cx.background_executor().clone(),
                    )?;
                    connection.initialize().await?.validate()?;
                    let scope = url::Url::from_directory_path(&root)
                        .map_err(|()| anyhow!("invalid VCS scope"))?
                        .to_string();
                    let repositories = connection.discover(scope.clone()).await?;
                    for repository in &repositories {
                        repository.validate(&scope)?;
                    }
                    ensure!(
                        repositories.len() <= 1,
                        "multiple VCS repositories claim the same opened scope"
                    );
                    Ok(Some((connection, repositories)))
                }
                .await;
            this.update(cx, |this, cx| {
                if this
                    .sessions
                    .get(&task_key)
                    .is_none_or(|session| session.generation != generation)
                {
                    return;
                }
                match result {
                    Ok(Some((connection, repositories))) => {
                        this.install_session(task_key.clone(), connection, repositories, cx)
                    }
                    Ok(None) => {
                        this.sessions.remove(&task_key);
                    }
                    Err(error) => {
                        this.errors
                            .insert(task_key.clone(), format!("VCS provider: {error:#}").into());
                        this.notify(cx);
                    }
                }
            })
            .log_err();
        });
        self.sessions.insert(
            key,
            Session {
                generation,
                claims_scope: false,
                connection: None,
                _start_task: task,
                notification_task: None,
            },
        );
    }

    fn install_session(
        &mut self,
        key: SessionKey,
        connection: Arc<ProviderConnection>,
        descriptors: Vec<RepositoryDescriptor>,
        cx: &mut Context<Self>,
    ) {
        log::info!(
            "Connected VCS provider: extension={}, provider={}, repositories={}",
            key.1.0,
            key.1.1,
            descriptors.len()
        );
        let notifications = connection.notifications();
        let notification_key = key.clone();
        let generation = self
            .sessions
            .get(&key)
            .map_or(0, |session| session.generation);
        let notification_task = cx.spawn(async move |this, cx| {
            let mut reported_exit = false;
            while let Ok(notification) = notifications.recv().await {
                let exited = matches!(&notification, ProviderNotification::Exited { .. });
                if this
                    .update(cx, |this, cx| match notification {
                        ProviderNotification::DidChange { repository_id } => {
                            let keys: Vec<_> = this
                                .repositories
                                .keys()
                                .filter(|repository| {
                                    repository.worktree_id == notification_key.0
                                        && repository.extension_id == notification_key.1.0
                                        && repository.provider_id == notification_key.1.1
                                        && repository.repository_id == repository_id
                                })
                                .cloned()
                                .collect();
                            for key in keys {
                                this.set_blame_head(&key, None, cx);
                                this.refresh_with_reason(&key, RefreshReason::Background, cx);
                            }
                        }
                        ProviderNotification::OperationProgress { progress } => {
                            if this
                                .sessions
                                .get(&notification_key)
                                .is_some_and(|session| session.generation == generation)
                            {
                                let keys = this
                                    .repositories
                                    .keys()
                                    .filter(|key| {
                                        key.worktree_id == notification_key.0
                                            && key.extension_id == notification_key.1.0
                                            && key.provider_id == notification_key.1.1
                                            && key.repository_id == progress.repository_id
                                    })
                                    .cloned()
                                    .collect::<Vec<_>>();
                                for key in keys {
                                    this.receive_operation_progress(&key, progress.clone(), cx);
                                }
                            }
                        }
                        ProviderNotification::Exited { message } => {
                            this.disconnected(&notification_key, generation, message, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
                if exited {
                    reported_exit = true;
                    break;
                }
            }
            if !reported_exit {
                this.update(cx, |this, cx| {
                    this.disconnected(
                        &notification_key,
                        generation,
                        "VCS provider notification stream closed; refresh to reconnect".into(),
                        cx,
                    )
                })
                .log_err();
            }
        });
        if let Some(session) = self.sessions.get_mut(&key) {
            session.connection = Some(connection.clone());
            session.notification_task = Some(notification_task);
        }
        self.errors.remove(&key);
        for descriptor in descriptors {
            let Some(baseline) = descriptor.baselines.first() else {
                continue;
            };
            let id = VcsRepositoryId {
                worktree_id: key.0,
                extension_id: key.1.0.clone(),
                provider_id: key.1.1.clone(),
                repository_id: descriptor.id.clone(),
            };
            let selected_baseline = self
                .selected_baselines
                .get(&id)
                .filter(|selected| {
                    descriptor
                        .baselines
                        .iter()
                        .any(|baseline| &baseline.id == *selected)
                })
                .cloned()
                .unwrap_or_else(|| baseline.id.clone());
            self.repositories.insert(
                id.clone(),
                Repository {
                    view: VcsRepositorySnapshot {
                        key: id.clone(),
                        selected_baseline,
                        descriptor,
                        snapshot: None,
                        loading: false,
                        show_loading: false,
                        error: None,
                        repository_state: None,
                        repository_state_error: None,
                        checkouts: None,
                        checkout_error: None,
                        checkout_loading: false,
                    },
                    connection: connection.clone(),
                    generation: 0,
                    refresh_task: None,
                    blame_head: None,
                    blame_generation: 0,
                    blames: HashMap::default(),
                    checkout_generation: 0,
                    checkout_task: None,
                    comparisons: HashMap::default(),
                    comparison_generations: HashMap::default(),
                    comparison_diffs: HashMap::default(),
                },
            );
            self.refresh(&id, cx);
        }
        let owners: Vec<_> = self
            .repositories
            .keys()
            .filter(|repository| repository.worktree_id == key.0)
            .cloned()
            .collect();
        if owners.len() > 1 {
            for owner in owners {
                if let Some(repository) = self.repositories.get_mut(&owner) {
                    repository.generation += 1;
                    repository.refresh_task = None;
                    repository.view.snapshot = None;
                    repository.view.loading = false;
                    repository.view.show_loading = false;
                    repository.view.error = Some(
                        "Multiple VCS providers claim this folder; disable one provider".into(),
                    );
                }
                self.set_blame_head(&owner, None, cx);
                self.invalidate_diffs(&owner, Some("VCS ownership is ambiguous"), cx);
            }
        }
        self.ensure_polling(cx);
        self.notify(cx);
        cx.emit(VcsStoreEvent::DiffChanged { worktree_id: key.0 });
    }

    fn disconnected(
        &mut self,
        key: &SessionKey,
        generation: u64,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self
            .sessions
            .get(key)
            .is_none_or(|session| session.generation != generation)
        {
            return;
        }
        log::warn!(
            "VCS provider disconnected: extension={}, provider={}; see Source Control diagnostics",
            key.1.0,
            key.1.1
        );
        self.errors.insert(key.clone(), message.clone().into());
        let repositories: Vec<_> = self
            .repositories
            .keys()
            .filter(|repository| {
                repository.worktree_id == key.0
                    && repository.extension_id == key.1.0
                    && repository.provider_id == key.1.1
            })
            .cloned()
            .collect();
        for repository in repositories {
            if let Some(state) = self.repositories.get_mut(&repository) {
                state.generation += 1;
                state.view.loading = false;
                state.view.show_loading = false;
                state.view.error = Some(message.clone().into());
                state.view.snapshot = None;
                state.refresh_task = None;
                state.checkout_generation += 1;
                state.checkout_task = None;
                state.view.checkout_loading = false;
                state.view.checkout_error = Some(message.clone().into());
            }
            self.set_blame_head(&repository, None, cx);
            self.invalidate_diffs(&repository, Some(&message), cx);
        }
        self.notify(cx);
    }

    fn stop_session(&mut self, key: &SessionKey, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions.remove(key) {
            if let Some(connection) = session.connection {
                cx.background_spawn(async move {
                    connection.shutdown().await.log_err();
                })
                .detach();
            }
        }
        self.errors.remove(key);
        let repositories: Vec<_> = self
            .repositories
            .keys()
            .filter(|repository| {
                repository.worktree_id == key.0
                    && repository.extension_id == key.1.0
                    && repository.provider_id == key.1.1
            })
            .cloned()
            .collect();
        for repository in repositories {
            self.set_blame_head(&repository, None, cx);
            self.invalidate_diffs(&repository, Some("VCS repository closed"), cx);
            self.repositories.remove(&repository);
        }
        if self.repositories.is_empty() {
            self.poll_task = None;
        }
        self.notify(cx);
        cx.emit(VcsStoreEvent::DiffChanged { worktree_id: key.0 });
    }

    fn stop_all(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<_> = self.sessions.keys().cloned().collect();
        for key in keys {
            self.stop_session(&key, cx);
        }
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        if self.file_operation_running() {
            return;
        }
        let failed: Vec<_> = self
            .sessions
            .iter()
            .filter(|(key, session)| {
                session
                    .connection
                    .as_ref()
                    .is_some_and(|connection| !connection.is_alive())
                    || self.errors.contains_key(*key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &failed {
            self.stop_session(key, cx);
        }
        let healthy: Vec<_> = self.repositories.keys().cloned().collect();
        for key in healthy {
            self.refresh(&key, cx);
        }
        if !failed.is_empty() || self.repositories.is_empty() {
            self.schedule_discovery(cx);
        }
    }

    pub fn set_baseline(
        &mut self,
        key: &VcsRepositoryId,
        baseline: String,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        ensure!(
            !self.staging_batch.as_ref().is_some_and(|batch| {
                self.file_operation_scope(key).is_some_and(|scope| {
                    batch.scope.same_provider(&scope) && batch.scope.root_uri == scope.root_uri
                })
            }),
            "Wait for Stage All or Unstage All to finish before changing the comparison"
        );
        ensure!(
            self.repositories
                .keys()
                .filter(|candidate| candidate.worktree_id == key.worktree_id)
                .count()
                == 1,
            "VCS ownership is ambiguous"
        );
        let repository = self
            .repositories
            .get_mut(key)
            .context("VCS repository is unavailable")?;
        ensure!(
            repository
                .view
                .descriptor
                .baselines
                .iter()
                .any(|entry| entry.id == baseline),
            "unknown VCS baseline"
        );
        repository.view.selected_baseline = baseline.clone();
        self.selected_baselines.insert(key.clone(), baseline);
        repository.view.snapshot = None;
        self.invalidate_diffs(key, None, cx);
        self.refresh(key, cx);
        Ok(())
    }

    pub fn refresh(&mut self, key: &VcsRepositoryId, cx: &mut Context<Self>) {
        self.reconcile_file_operation(key, cx);
        self.refresh_with_reason(key, RefreshReason::Explicit, cx);
        self.refresh_checkouts(key, cx);
    }

    fn refresh_with_reason(
        &mut self,
        key: &VcsRepositoryId,
        reason: RefreshReason,
        cx: &mut Context<Self>,
    ) {
        if self.staging_batch.as_ref().is_some_and(|batch| {
            self.file_operation_scope(key).is_some_and(|scope| {
                batch.scope.same_provider(&scope) && batch.scope.root_uri == scope.root_uri
            })
        }) {
            return;
        }
        if self
            .repositories
            .keys()
            .filter(|candidate| candidate.worktree_id == key.worktree_id)
            .count()
            != 1
        {
            return;
        }
        let Some(repository) = self.repositories.get_mut(key) else {
            return;
        };
        if !repository.connection.is_alive() {
            self.refresh_all(cx);
            return;
        }
        repository.generation += 1;
        let generation = repository.generation;
        let baseline = repository.view.selected_baseline.clone();
        let comparison_generation = repository
            .comparison_generations
            .entry(baseline.clone())
            .or_default();
        *comparison_generation += 1;
        let comparison_generation = *comparison_generation;
        let connection = repository.connection.clone();
        let key = key.clone();
        repository.view.show_loading = (repository.view.loading && repository.view.show_loading)
            || reason == RefreshReason::Explicit
            || repository.view.snapshot.is_none();
        repository.view.loading = true;
        let show_loading = repository.view.show_loading;
        repository.refresh_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let request = async {
                let state = async {
                    if connection
                        .capabilities()
                        .is_some_and(|capabilities| !capabilities.repository_operations.is_empty())
                    {
                        Some(connection.repository_state(key.repository_id.clone()).await)
                    } else {
                        None
                    }
                };
                futures::join!(
                    connection.snapshot(key.repository_id.clone(), baseline.clone()),
                    state,
                )
            };
            let (result, repository_state) = if show_loading {
                request.await
            } else {
                let slow_refresh = cx.background_executor().timer(Duration::from_secs(2));
                match futures::future::select(Box::pin(request), Box::pin(slow_refresh)).await {
                    futures::future::Either::Left((result, _)) => result,
                    futures::future::Either::Right((_, request)) => {
                        this.update(cx, |this, cx| {
                            if let Some(repository) = this.repositories.get_mut(&key)
                                && repository.generation == generation
                                && repository.view.loading
                            {
                                repository.view.show_loading = true;
                                this.notify(cx);
                            }
                        })
                        .log_err();
                        request.await
                    }
                }
            };
            this.update(cx, |this, cx| {
                let Some(repository) = this.repositories.get_mut(&key) else {
                    return;
                };
                if repository.generation != generation
                    || repository.view.selected_baseline != baseline
                    || !Arc::ptr_eq(&repository.connection, &connection)
                {
                    return;
                }
                repository.view.loading = false;
                repository.view.show_loading = false;
                if let Some(state) = repository_state {
                    match state {
                        Ok(state) => {
                            repository.view.repository_state = Some(Arc::new(state));
                            repository.view.repository_state_error = None;
                        }
                        Err(error) => {
                            repository.view.repository_state_error =
                                Some(format!("{error:#}").into());
                        }
                    }
                }
                if repository.comparison_generations.get(&baseline) != Some(&comparison_generation)
                {
                    this.notify(cx);
                    return;
                }
                let diff_changed = match result.and_then(|snapshot| {
                    snapshot.validate(&key.repository_id, &baseline)?;
                    let source = repository
                        .view
                        .descriptor
                        .baselines
                        .iter()
                        .find(|candidate| candidate.id == baseline)
                        .context("Unknown VCS comparison")?
                        .current_content;
                    ensure!(
                        source != vcs::ContentSource::Snapshot
                            || snapshot.current_revision_id.is_some(),
                        "Snapshot comparison omitted its current content identity"
                    );
                    Ok(snapshot)
                }) {
                    Ok(snapshot) => {
                        // A new snapshot token need not change immutable baseline content.
                        let changed = repository.view.error.is_some()
                            || repository.view.snapshot.as_ref().is_none_or(|previous| {
                                previous.baseline_id != snapshot.baseline_id
                                    || previous.revision_id != snapshot.revision_id
                                    || previous.current_revision_id != snapshot.current_revision_id
                                    || previous.changes != snapshot.changes
                            });
                        let head = snapshot.head_revision_id.clone();
                        let snapshot = Arc::new(snapshot);
                        repository
                            .comparisons
                            .insert(baseline.clone(), snapshot.clone());
                        repository.view.snapshot = Some(snapshot);
                        repository.view.error = None;
                        this.set_blame_head(&key, head, cx);
                        if changed {
                            let closed_buffers: Vec<_> = this
                                .diffs
                                .iter()
                                .filter(|(_, state)| {
                                    state.repository == key && state.buffer.upgrade().is_none()
                                })
                                .map(|(buffer_id, _)| *buffer_id)
                                .collect();
                            for buffer_id in closed_buffers {
                                if let Some(mut state) = this.diffs.remove(&buffer_id) {
                                    state.finish_waiters(Err(anyhow!("VCS diff buffer closed")));
                                }
                            }
                            let buffers: Vec<_> = this
                                .diffs
                                .iter()
                                .filter(|(_, state)| state.repository == key)
                                .filter_map(|(_, state)| state.buffer.upgrade())
                                .collect();
                            for buffer in buffers {
                                this.start_diff_load(buffer, cx);
                            }
                        }
                        changed
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        repository.view.error = Some(message.clone().into());
                        this.set_blame_head(&key, None, cx);
                        this.invalidate_diffs(&key, Some(&message), cx);
                        true
                    }
                };
                this.notify(cx);
                if diff_changed {
                    this.refresh_open_comparison_diffs(&key, &baseline, cx);
                    cx.emit(VcsStoreEvent::DiffChanged {
                        worktree_id: key.worktree_id,
                    });
                }
                let other_baselines: Vec<_> = this
                    .repositories
                    .get(&key)
                    .into_iter()
                    .flat_map(|repository| repository.comparisons.keys())
                    .filter(|candidate| **candidate != baseline)
                    .cloned()
                    .collect();
                for baseline in other_baselines {
                    this.load_comparison(key.clone(), baseline, cx)
                        .detach_and_log_err(cx);
                }
            })
            .log_err();
        }));
        self.notify(cx);
    }

    fn ensure_polling(&mut self, cx: &mut Context<Self>) {
        if self.poll_task.is_some() || self.repositories.is_empty() {
            return;
        }
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<_> = self
            .repositories
            .iter()
            .filter(|(_, repository)| !repository.view.loading && repository.connection.is_alive())
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.refresh_with_reason(&key, RefreshReason::Background, cx);
        }
    }

    pub fn repository_for_path(&self, path: &ProjectPath, cx: &App) -> Option<VcsRepositoryId> {
        if self
            .git
            .read(cx)
            .repository_and_path_for_project_path(path, cx)
            .is_some()
        {
            return None;
        }
        let mut matches = self
            .repositories
            .keys()
            .filter(|key| key.worktree_id == path.worktree_id);
        let key = matches.next()?.clone();
        if matches.next().is_some() {
            return None;
        }
        Some(key)
    }

    pub fn owns_project_path(&self, path: &ProjectPath, cx: &App) -> bool {
        self.repository_for_path(path, cx).is_some()
    }

    fn set_blame_head(
        &mut self,
        key: &VcsRepositoryId,
        head: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repositories.get_mut(key) else {
            return;
        };
        if repository.blame_head != head {
            repository.blame_head = head;
            repository.blame_generation += 1;
            repository.blames.clear();
            cx.emit(VcsStoreEvent::BlameChanged {
                worktree_id: key.worktree_id,
            });
        }
    }

    pub fn has_provider_blame(&self, buffer: &Entity<Buffer>, cx: &App) -> bool {
        buffer
            .read(cx)
            .project_path(cx)
            .and_then(|path| self.repository_for_path(&path, cx))
            .and_then(|key| self.repositories.get(&key))
            .is_some_and(|repository| {
                repository.connection.is_alive()
                    && repository.view.error.is_none()
                    && repository.blame_head.is_some()
                    && repository
                        .connection
                        .capabilities()
                        .is_some_and(|capabilities| capabilities.blame)
            })
    }

    pub fn blame_buffer(
        &mut self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Arc<vcs::Blame>>>> {
        if !self.has_provider_blame(buffer, cx) {
            return Task::ready(Ok(None));
        }
        let request = (|| {
            let project_path = buffer
                .read(cx)
                .project_path(cx)
                .context("buffer has no VCS path")?;
            let key = self
                .repository_for_path(&project_path, cx)
                .context("buffer has no unambiguous VCS provider")?;
            ensure!(
                self.trusted(key.worktree_id, cx),
                "VCS worktree is not trusted"
            );
            let repository = self
                .repositories
                .get(&key)
                .context("VCS repository closed")?;
            let head = repository
                .blame_head
                .clone()
                .context("VCS HEAD is unavailable")?;
            let path = project_path.path.as_unix_str().to_owned();
            vcs::validate_path(&path)?;
            let root = vcs::local_path(&repository.view.descriptor.scope_uri)?;
            let cached = repository.blames.get(&path).cloned();
            Ok((
                key,
                project_path,
                path,
                root,
                head,
                repository.blame_generation,
                repository.connection.clone(),
                cached,
            ))
        })();
        let (key, project_path, path, root, head, generation, connection, cached) = match request {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        let fs = self.fs.clone();
        let buffer = buffer.downgrade();
        cx.spawn(async move |this, cx| {
            let metadata = scoped_metadata(&root, &path, &fs).await?;
            ensure!(
                metadata.is_none_or(|metadata| !metadata.is_dir && !metadata.is_symlink),
                "VCS blame requires a regular file"
            );
            let blame = match cached {
                Some(blame) => blame,
                None => Arc::new(
                    connection
                        .blame(key.repository_id.clone(), path.clone())
                        .await?,
                ),
            };
            ensure!(
                blame.revision_id == head,
                "VCS blame HEAD changed during the request"
            );
            this.update(cx, |this, cx| {
                ensure!(
                    this.trusted(key.worktree_id, cx),
                    "VCS worktree is no longer trusted"
                );
                ensure!(
                    this.repository_for_path(&project_path, cx).as_ref() == Some(&key),
                    "VCS blame owner changed"
                );
                ensure!(
                    buffer.upgrade().is_some_and(|buffer| buffer
                        .read(cx)
                        .project_path(cx)
                        .as_ref()
                        == Some(&project_path)),
                    "VCS blame buffer path changed"
                );
                let repository = this
                    .repositories
                    .get(&key)
                    .context("VCS repository closed")?;
                ensure!(
                    repository.connection.is_alive()
                        && Arc::ptr_eq(&repository.connection, &connection)
                        && repository.blame_generation == generation
                        && repository.blame_head.as_ref() == Some(&head)
                        && repository.view.error.is_none(),
                    "VCS blame response is stale"
                );
                if let Some(cached) = repository.blames.get(&path) {
                    return Ok(Some(cached.clone()));
                }
                let (count, size) = this
                    .repositories
                    .values()
                    .flat_map(|repository| repository.blames.values())
                    .fold((0, 0), |(count, size), blame| {
                        (count + 1, size + blame.estimated_size())
                    });
                if count >= 16 || size + blame.estimated_size() > 32 * 1024 * 1024 {
                    for repository in this.repositories.values_mut() {
                        repository.blames.clear();
                    }
                }
                this.repositories
                    .get_mut(&key)
                    .context("VCS repository closed")?
                    .blames
                    .insert(path, blame.clone());
                Ok(Some(blame))
            })?
        })
    }

    pub fn file_operations(&self, key: &VcsRepositoryId) -> Vec<FileOperation> {
        self.repositories
            .get(key)
            .and_then(|repository| repository.connection.capabilities())
            .filter(|capabilities| !capabilities.read_only)
            .map(|capabilities| capabilities.file_operations)
            .unwrap_or_default()
    }

    pub fn file_operation_running(&self) -> bool {
        self.file_operation_running || self.staging_batch.is_some()
    }

    fn check_mutation_available(
        &self,
        key: &VcsRepositoryId,
        owner: Option<&Arc<()>>,
        cx: &Context<Self>,
    ) -> Result<()> {
        ensure!(
            !self.file_operation_running
                && self.staging_batch.as_ref().is_none_or(|batch| {
                    owner.is_some_and(|owner| Arc::ptr_eq(owner, &batch.owner))
                }),
            "Another VCS operation is running"
        );
        let scope = self
            .file_operation_scope(key)
            .context("VCS repository closed")?;
        let stores = GlobalRegistry::global(cx).0.read(cx).stores.clone();
        for other in stores.iter().filter_map(WeakEntity::upgrade) {
            if other.entity_id() != cx.entity().entity_id() {
                ensure!(
                    other.read(cx).staging_batch.as_ref().is_none_or(|batch| {
                        !batch.scope.same_provider(&scope) || batch.scope.root_uri != scope.root_uri
                    }),
                    "Another project is staging files in this checkout"
                );
            }
        }
        Ok(())
    }

    pub fn file_operation_unresolved(&self, key: &VcsRepositoryId) -> bool {
        self.file_operation_scope(key).is_some_and(|scope| {
            self.operation_journal
                .blocking(&scope, false)
                .map_or(true, |pending| pending.is_some())
        })
    }

    pub fn file_operation_error(&self, key: &VcsRepositoryId) -> Option<SharedString> {
        self.file_operation_errors.get(key).cloned()
    }

    fn file_operation_scope(&self, key: &VcsRepositoryId) -> Option<FileOperationScope> {
        let repository = self.repositories.get(key)?;
        let mut scope = FileOperationScope {
            extension_id: key.extension_id.clone(),
            provider_id: key.provider_id.clone(),
            repository_id: key.repository_id.clone(),
            root_uri: repository.view.descriptor.root_uri.clone(),
            scope_uri: repository.view.descriptor.scope_uri.clone(),
        };
        scope.normalize().ok()?;
        Some(scope)
    }

    fn file_change(
        &self,
        key: &VcsRepositoryId,
        path: &str,
        baseline: &str,
    ) -> Result<&vcs::Change> {
        let repository = self
            .repositories
            .get(key)
            .context("VCS repository closed")?;
        let snapshot = if repository.view.selected_baseline == baseline {
            repository.view.snapshot.as_ref()
        } else {
            repository.comparisons.get(baseline)
        };
        snapshot
            .context("Refresh changes before modifying a file")?
            .changes
            .iter()
            .find(|change| change.path == path)
            .context("File is no longer in the selected comparison; refresh changes")
    }

    pub fn file_operations_for_path(
        &self,
        key: &VcsRepositoryId,
        path: &str,
    ) -> Vec<FileOperation> {
        let Some(repository) = self.repositories.get(key) else {
            return Vec::new();
        };
        self.file_operations_for_baseline(key, path, &repository.view.selected_baseline)
    }

    pub fn file_operations_for_baseline(
        &self,
        key: &VcsRepositoryId,
        path: &str,
        baseline: &str,
    ) -> Vec<FileOperation> {
        let Ok(change) = self.file_change(key, path, baseline) else {
            return Vec::new();
        };
        if change.kind == vcs::ContentKind::Submodule {
            return Vec::new();
        }
        let provider = self.file_operations(key);
        let operations = match &change.file_operations {
            Some(operations) => operations
                .iter()
                .copied()
                .filter(|operation| provider.contains(operation))
                .collect(),
            None if legacy_file_change(change) => provider,
            None => Vec::new(),
        };
        operations
            .into_iter()
            .filter(|operation| {
                *operation != FileOperation::Ignore
                    || (change.status == ChangeStatus::Untracked
                        && self
                            .repositories
                            .get(key)
                            .and_then(|repository| {
                                repository.view.descriptor.ignore_config_path.as_ref()
                            })
                            .is_some_and(|config| config != path))
            })
            .collect()
    }

    fn check_file_buffers(
        &self,
        scope: &FileOperationScope,
        paths: &[String],
        cx: &Context<Self>,
    ) -> Result<()> {
        let root = vcs::local_path(&scope.root_uri)?;
        let directory = vcs::local_path(&scope.scope_uri)?;
        let paths = paths
            .iter()
            .map(|path| directory.join(path))
            .collect::<Vec<_>>();
        self.check_buffers(&root, Some(&paths), cx)?;
        let stores = GlobalRegistry::global(cx).0.read(cx).stores.clone();
        for other in stores.iter().filter_map(WeakEntity::upgrade) {
            if other.entity_id() == cx.entity().entity_id() {
                continue;
            }
            other.read(cx).check_buffers(&root, Some(&paths), cx)?;
        }
        Ok(())
    }

    fn validate_file_targets(
        &self,
        key: &VcsRepositoryId,
        path: &str,
        operation: FileOperation,
        affected_paths: &[String],
        summary: Option<&str>,
        baseline: &str,
    ) -> Result<()> {
        vcs::validate_paths(affected_paths)?;
        ensure!(
            affected_paths.iter().any(|affected| affected == path),
            "Prepared operation omitted the selected file"
        );
        let change = self.file_change(key, path, baseline)?;
        if operation == FileOperation::Ignore {
            let config = self
                .repositories
                .get(key)
                .and_then(|repository| repository.view.descriptor.ignore_config_path.as_ref())
                .context("Provider has not declared an ignore configuration file")?;
            ensure!(
                change.status == ChangeStatus::Untracked && config != path,
                "Only untracked files can be ignored"
            );
            ensure!(
                affected_paths.len() == 2 && affected_paths.contains(config),
                "Ignore must affect only the selected file and the declared ignore configuration"
            );
            ensure!(
                summary.is_some_and(|summary| !summary.trim().is_empty()),
                "Ignore requires an explicit summary"
            );
            return Ok(());
        }
        ensure!(
            affected_paths
                .iter()
                .all(|affected| affected == path || change.old_path.as_ref() == Some(affected)),
            "Prepared file operation affects another file"
        );
        if let Some(old_path) = &change.old_path {
            ensure!(
                affected_paths.contains(old_path),
                "Rename operation must include both paths"
            );
        }
        if operation == FileOperation::Discard && !legacy_file_change(change) {
            ensure!(
                summary.is_some_and(|summary| !summary.trim().is_empty()),
                "This discard operation requires an explicit summary of its effect"
            );
        }
        Ok(())
    }

    fn check_file_operation(
        &mut self,
        key: &VcsRepositoryId,
        path: &str,
        operation: FileOperation,
        baseline: &str,
        cx: &mut Context<Self>,
    ) -> Result<(Arc<ProviderConnection>, Arc<Path>)> {
        vcs::validate_path(path)?;
        ensure!(
            self.trusted(key.worktree_id, cx),
            "VCS worktree is not trusted"
        );
        ensure!(self.project.upgrade().is_some(), "VCS project closed");
        ensure!(
            self.file_operations_for_baseline(key, path, baseline)
                .contains(&operation),
            "File operation is unavailable for the selected change"
        );
        let repository = self
            .repositories
            .get(key)
            .context("VCS repository closed")?;
        ensure!(
            repository.connection.is_alive(),
            "VCS provider is no longer running"
        );
        let change = self.file_change(key, path, baseline)?;
        let mut paths = vec![path.to_owned()];
        if let Some(old_path) = &change.old_path {
            if old_path != path {
                paths.push(old_path.clone());
            }
        }
        if operation == FileOperation::Ignore {
            paths.push(
                repository
                    .view
                    .descriptor
                    .ignore_config_path
                    .clone()
                    .context("Provider has not declared an ignore configuration file")?,
            );
        }
        for path in &paths {
            ensure!(
                self.repository_for_path(
                    &ProjectPath {
                        worktree_id: key.worktree_id,
                        path: Arc::from(RelPath::from_unix_str(path)?),
                    },
                    cx
                )
                .as_ref()
                    == Some(key),
                "An affected file belongs to another source-control provider"
            );
        }
        let scope = self
            .file_operation_scope(key)
            .context("VCS repository closed")?;
        self.check_file_buffers(&scope, &paths, cx)?;
        let root = self
            .worktrees
            .read(cx)
            .worktree_for_id(key.worktree_id, cx)
            .context("VCS worktree closed")?
            .read(cx)
            .abs_path();
        Ok((repository.connection.clone(), root))
    }

    pub fn prepare_file_operation(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        operation: FileOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsPreparedFileOperation>> {
        let Some(repository) = self.repositories.get(&key) else {
            return Task::ready(Err(anyhow!("VCS repository closed")));
        };
        let baseline = repository.view.selected_baseline.clone();
        self.prepare_file_operation_for_baseline(key, path, operation, baseline, cx)
    }

    pub fn prepare_file_operation_for_baseline(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        operation: FileOperation,
        baseline: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsPreparedFileOperation>> {
        self.prepare_file_operation_for_owner(key, path, operation, baseline, None, cx)
    }

    fn prepare_file_operation_for_owner(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        operation: FileOperation,
        baseline: String,
        owner: Option<&Arc<()>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsPreparedFileOperation>> {
        let result = (|| {
            self.check_mutation_available(&key, owner, cx)?;
            let (connection, root) =
                self.check_file_operation(&key, &path, operation, &baseline, cx)?;
            let scope = self
                .file_operation_scope(&key)
                .context("VCS repository closed")?;
            let change = self.file_change(&key, &path, &baseline)?.clone();
            let ignore_config = if operation == FileOperation::Ignore {
                self.repositories
                    .get(&key)
                    .and_then(|repository| repository.view.descriptor.ignore_config_path.clone())
            } else {
                None
            };
            Ok((connection, root, scope, change, ignore_config))
        })();
        let (connection, root, scope, change, ignore_config) = match result {
            Ok(result) => result,
            Err(error) => return Task::ready(Err(error)),
        };
        let fs = self.fs.clone();
        let journal = self.operation_journal.clone();
        cx.spawn(async move |this, cx| {
            journal.load().await?;
            ensure!(
                journal.blocking(&scope, false)?.is_none(),
                "The previous operation has an unknown outcome; refresh to reconcile it"
            );
            validate_file_metadata(&root, &change, &fs).await?;
            if let Some(config) = &ignore_config {
                validate_ignore_config(&root, config, &fs).await?;
            }
            let prepared = connection
                .prepare_file_operation(key.repository_id.clone(), path.clone(), operation)
                .await?;
            let affected_paths = prepared
                .affected_paths
                .unwrap_or_else(|| vec![path.clone()]);
            this.update(cx, |this, cx| {
                let (current, _) =
                    this.check_file_operation(&key, &path, operation, &baseline, cx)?;
                ensure!(
                    Arc::ptr_eq(&current, &connection),
                    "VCS provider changed while preparing the operation"
                );
                this.validate_file_targets(
                    &key,
                    &path,
                    operation,
                    &affected_paths,
                    prepared.summary.as_deref(),
                    &baseline,
                )?;
                this.check_file_buffers(&scope, &affected_paths, cx)
            })??;
            Ok(VcsPreparedFileOperation {
                key,
                baseline,
                scope,
                path,
                operation,
                token: prepared.operation_token,
                affected_paths,
                summary: prepared.summary,
                change,
                connection,
            })
        })
    }

    pub fn apply_prepared_file_operation(
        &mut self,
        prepared: VcsPreparedFileOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.apply_prepared_file_operation_for_owner(prepared, None, cx)
    }

    fn apply_prepared_file_operation_for_owner(
        &mut self,
        prepared: VcsPreparedFileOperation,
        owner: Option<&Arc<()>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let result = self
            .check_prepared_file_operation(&prepared, cx)
            .and_then(|()| {
                self.check_mutation_available(&prepared.key, owner, cx)?;
                ensure!(
                    !self.file_operation_unresolved(&prepared.key),
                    "The previous operation has an unknown outcome; refresh to reconcile it"
                );
                Ok(())
            });
        if let Err(error) = result {
            return Task::ready(Err(error));
        }
        self.file_operation_running = true;
        self.file_operation_errors.remove(&prepared.key);
        self.notify(cx);
        let journal = self.operation_journal.clone();
        let fs = self.fs.clone();
        let (sender, receiver) = futures::channel::oneshot::channel();
        // Persist before dispatch; the detached operation retains its recovery owner
        // when the confirming panel or its waiter closes.
        cx.spawn(async move |this, cx| {
            let pending = PendingOperation {
                token: prepared.token.clone(),
                request: OperationRequest::File {
                    path: prepared.path.clone(),
                    operation: prepared.operation,
                    affected_paths: prepared.affected_paths.clone(),
                },
            };
            let result: Result<()> = async {
                let _claim = journal.record(prepared.scope.clone(), pending.clone()).await?;
                let checked: Result<()> = async {
                    validate_file_metadata(&vcs::local_path(&prepared.scope.scope_uri)?, &prepared.change, &fs).await?;
                    if prepared.operation == FileOperation::Ignore {
                        for path in &prepared.affected_paths {
                            if path != &prepared.path { validate_ignore_config(&vcs::local_path(&prepared.scope.scope_uri)?, path, &fs).await?; }
                        }
                    }
                    this.update(cx, |this, cx| {
                        this.pending_file_operations.insert(prepared.scope.clone(), pending.clone());
                        this.check_prepared_file_operation(&prepared, cx)
                    })?
                }.await;
                if let Err(error) = checked {
                    journal.clear(&prepared.scope, &prepared.token).await?;
                    return Err(error);
                }
                let outcome = match prepared.connection.apply_file_operation(
                    prepared.key.repository_id.clone(), prepared.path.clone(),
                    prepared.operation, prepared.token.clone(),
                ).await {
                    Ok(result) => result.outcome,
                    Err(error) => prepared.connection.reconcile_file_operation(
                        prepared.key.repository_id.clone(), prepared.path.clone(),
                        prepared.operation, prepared.token.clone(),
                    ).await.with_context(|| format!(
                        "File operation outcome is unknown ({error:#}); refresh to reconcile before retrying"
                    ))?.outcome,
                };
                journal.clear(&prepared.scope, &prepared.token).await.context(
                    "File operation completed, but its recovery record could not be cleared; refresh to reconcile",
                )?;
                if outcome == FileOperationOutcome::Applied && matches!(prepared.operation, FileOperation::Discard | FileOperation::Ignore) {
                    this.update(cx, |this, cx| {
                        this.check_discarded_buffers(&prepared.scope, &prepared.affected_paths, cx)
                    })??;
                }
                match outcome {
                    FileOperationOutcome::Applied => Ok(()),
                    FileOperationOutcome::NotApplied => bail!("The file operation was not applied"),
                    FileOperationOutcome::Conflict => bail!("The file changed while the operation was running; review its current state"),
                }
            }.await;
            this.update(cx, |this, cx| {
                this.file_operation_running = false;
                if journal.pending(&prepared.scope).is_ok_and(|pending| pending.is_none()) {
                    this.pending_file_operations.remove(&prepared.scope);
                }
                if let Err(error) = &result {
                    this.file_operation_errors.insert(prepared.key.clone(), format!("{error:#}").into());
                }
                if prepared.connection.is_alive() {
                    this.refresh_with_reason(&prepared.key, RefreshReason::Explicit, cx);
                }
                this.notify(cx);
            }).log_err();
            if sender.send(result).is_err() {
                log::debug!("VCS file operation waiter closed after reconciliation");
            }
        }).detach();
        cx.spawn(async move |_, _| {
            receiver
                .await
                .context("VCS file operation was interrupted")?
        })
    }

    fn check_prepared_file_operation(
        &mut self,
        prepared: &VcsPreparedFileOperation,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let (connection, _) = self.check_file_operation(
            &prepared.key,
            &prepared.path,
            prepared.operation,
            &prepared.baseline,
            cx,
        )?;
        ensure!(
            Arc::ptr_eq(&connection, &prepared.connection)
                && self.file_operation_scope(&prepared.key).as_ref() == Some(&prepared.scope),
            "VCS provider or scope changed after preparing the operation"
        );
        self.validate_file_targets(
            &prepared.key,
            &prepared.path,
            prepared.operation,
            &prepared.affected_paths,
            prepared.summary.as_deref(),
            &prepared.baseline,
        )?;
        self.check_file_buffers(&prepared.scope, &prepared.affected_paths, cx)
    }

    fn check_discarded_buffers(
        &self,
        scope: &FileOperationScope,
        paths: &[String],
        cx: &Context<Self>,
    ) -> Result<()> {
        self.check_file_buffers(scope, paths, cx).context(
            "The operation was applied on disk; edits made in buffers during the operation were preserved. Review them before saving.",
        )
    }

    pub fn run_staging_batch(
        &mut self,
        key: VcsRepositoryId,
        operation: FileOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let checked = (|| {
            ensure!(
                matches!(operation, FileOperation::Stage | FileOperation::Unstage),
                "Only staging and unstaging support batches"
            );
            self.check_mutation_available(&key, None, cx)?;
            ensure!(
                self.file_operations(&key).contains(&operation),
                "The provider does not support this staging operation"
            );
            let repository = self
                .repositories
                .get(&key)
                .context("VCS repository closed")?;
            let baseline = repository
                .view
                .descriptor
                .baselines
                .iter()
                .find(|baseline| baseline.purpose == Some(vcs::ComparisonPurpose::Working))
                .context("The provider has no working comparison for staging")?
                .id
                .clone();
            let scope = self
                .file_operation_scope(&key)
                .context("VCS repository closed")?;
            let stores = GlobalRegistry::global(cx).0.read(cx).stores.clone();
            for other in stores.iter().filter_map(WeakEntity::upgrade) {
                if other.entity_id() != cx.entity().entity_id() {
                    let other = other.read(cx);
                    ensure!(
                        !other.file_operation_running
                            || !other.repositories.keys().any(|key| {
                                other.file_operation_scope(key).is_some_and(|other_scope| {
                                    other_scope.same_provider(&scope)
                                        && other_scope.root_uri == scope.root_uri
                                })
                            }),
                        "Another project is modifying this checkout"
                    );
                }
            }
            Ok((baseline, scope, repository.connection.clone()))
        })();
        let (baseline, scope, connection) = match checked {
            Ok(checked) => checked,
            Err(error) => return Task::ready(Err(error)),
        };
        let owner = Arc::new(());
        self.staging_batch = Some(StagingBatch {
            scope: scope.clone(),
            owner: owner.clone(),
        });
        self.file_operation_errors.remove(&key);
        self.notify(cx);
        let fs = self.fs.clone();
        let journal = self.operation_journal.clone();
        let (sender, receiver) = futures::channel::oneshot::channel();
        // The owner outlives the UI waiter, just like an individual journaled write.
        cx.spawn(async move |this, cx| {
            let mut completed = 0;
            let mut total = 0;
            let mut current_path = None;
            let result: Result<()> = async {
                journal.load().await?;
                ensure!(journal.blocking(&scope, true)?.is_none(), "The previous operation has an unknown outcome; refresh to reconcile it");
                let snapshot = this.update(cx, |this, cx| this.load_comparison(key.clone(), baseline.clone(), cx))?;
                let state = async {
                    if connection.capabilities().is_some_and(|capabilities| !capabilities.repository_operations.is_empty()) {
                        Ok(Some(connection.repository_state(key.repository_id.clone()).await?))
                    } else {
                        Ok::<_, anyhow::Error>(None)
                    }
                };
                let (snapshot, state) = futures::try_join!(snapshot, state)?;
                let targets = this.update(cx, |this, cx| {
                    this.check_staging_batch_identity(&key, &scope, &connection, &owner)?;
                    let changes = snapshot.changes.iter()
                        .filter(|change| this.check_read_path(&key, &change.path, cx).is_ok())
                        .cloned().collect();
                    let targets = staging_batch_targets(changes, state.as_ref().map_or(&[][..], |state| state.staged_paths.as_slice()), operation)?;
                    for change in &targets {
                        this.check_file_operation(&key, &change.path, operation, &baseline, cx)?;
                    }
                    if let Some(state) = state {
                        let repository = this.repositories.get_mut(&key).context("VCS repository closed")?;
                        repository.view.repository_state = Some(Arc::new(state));
                        repository.view.repository_state_error = None;
                    }
                    Ok::<_, anyhow::Error>(targets)
                })??;
                total = targets.len();
                let root = vcs::local_path(&scope.scope_uri)?;
                for change in &targets {
                    validate_file_metadata(&root, change, &fs).await?;
                }
                for change in targets {
                    current_path = Some(change.path.clone());
                    let prepared = this.update(cx, |this, cx| {
                        this.check_staging_batch_identity(&key, &scope, &connection, &owner)?;
                        Ok::<_, anyhow::Error>(this.prepare_file_operation_for_owner(
                            key.clone(), change.path.clone(), operation, baseline.clone(), Some(&owner), cx,
                        ))
                    })??.await?;
                    ensure!(prepared.change.old_path == change.old_path
                        && prepared.affected_paths.iter().all(|path| path == &change.path || Some(path) == change.old_path.as_ref()),
                        "The file's rename paths changed after the batch started");
                    this.update(cx, |this, cx| {
                        this.check_staging_batch_identity(&key, &scope, &connection, &owner)?;
                        Ok::<_, anyhow::Error>(this.apply_prepared_file_operation_for_owner(prepared, Some(&owner), cx))
                    })??.await?;
                    completed += 1;
                }
                Ok(())
            }.await;
            let action = if operation == FileOperation::Stage { "Stage All" } else { "Unstage All" };
            let result = result.with_context(|| match current_path {
                Some(path) => format!("{action} stopped at {path}: {completed} of {total} file operations confirmed; {} not started", total.saturating_sub(completed + 1)),
                None => format!("{action} stopped before any files were modified"),
            });
            this.update(cx, |this, cx| {
                if this.staging_batch.as_ref().is_some_and(|batch| Arc::ptr_eq(&batch.owner, &owner)) {
                    this.staging_batch = None;
                }
                if let Err(error) = &result {
                    this.file_operation_errors.insert(key.clone(), format!("{error:#}").into());
                }
                if connection.is_alive() {
                    this.refresh_with_reason(&key, RefreshReason::Explicit, cx);
                }
                this.notify(cx);
            }).log_err();
            if sender.send(result).is_err() {
                log::debug!("VCS staging batch waiter closed after completion");
            }
        }).detach();
        cx.spawn(async move |_, _| {
            receiver
                .await
                .context("VCS staging batch was interrupted")?
        })
    }

    fn check_staging_batch_identity(
        &self,
        key: &VcsRepositoryId,
        scope: &FileOperationScope,
        connection: &Arc<ProviderConnection>,
        owner: &Arc<()>,
    ) -> Result<()> {
        ensure!(
            self.staging_batch
                .as_ref()
                .is_some_and(|batch| Arc::ptr_eq(&batch.owner, owner))
                && self.file_operation_scope(key).as_ref() == Some(scope)
                && self
                    .repositories
                    .get(key)
                    .is_some_and(|repository| Arc::ptr_eq(&repository.connection, connection))
                && connection.is_alive(),
            "VCS provider or scope changed during the staging batch"
        );
        Ok(())
    }

    pub fn run_file_operation(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        operation: FileOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(repository) = self.repositories.get(&key) else {
            return Task::ready(Err(anyhow!("VCS repository closed")));
        };
        let baseline = repository.view.selected_baseline.clone();
        self.run_file_operation_for_baseline(key, path, operation, baseline, cx)
    }

    pub fn run_file_operation_for_baseline(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        operation: FileOperation,
        baseline: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Err(error) = self.check_mutation_available(&key, None, cx) {
            return Task::ready(Err(error));
        }
        let prepare =
            self.prepare_file_operation_for_baseline(key.clone(), path, operation, baseline, cx);
        self.file_operation_running = true;
        self.notify(cx);
        let (sender, receiver) = futures::channel::oneshot::channel();
        cx.spawn(async move |this, cx| {
            let prepared = prepare.await;
            let result = match this.update(cx, |this, cx| {
                this.file_operation_running = false;
                match prepared {
                    Ok(prepared) => this.apply_prepared_file_operation(prepared, cx),
                    Err(error) => {
                        this.file_operation_errors
                            .insert(key, format!("{error:#}").into());
                        this.notify(cx);
                        Task::ready(Err(error))
                    }
                }
            }) {
                Ok(task) => task.await,
                Err(error) => Err(error),
            };
            if sender.send(result).is_err() {
                log::debug!("VCS file operation waiter closed");
            }
        })
        .detach();
        cx.spawn(async move |_, _| {
            receiver
                .await
                .context("VCS file operation was interrupted")?
        })
    }

    fn reconcile_file_operation(&mut self, key: &VcsRepositoryId, cx: &mut Context<Self>) {
        if self.file_operation_running() {
            return;
        }
        let Some(current_scope) = self.file_operation_scope(key) else {
            return;
        };
        let (scope, pending) = match self.operation_journal.blocking(&current_scope, true) {
            Ok(Some(pending)) => pending,
            Ok(None) => return,
            Err(error) => {
                self.file_operation_errors
                    .insert(key.clone(), format!("{error:#}").into());
                return;
            }
        };
        let claim = match self
            .operation_journal
            .claim_recovery(&scope, &pending.token)
        {
            Ok(Some(claim)) => claim,
            Ok(None) => return,
            Err(error) => {
                self.file_operation_errors
                    .insert(key.clone(), format!("{error:#}").into());
                return;
            }
        };
        self.pending_file_operations
            .insert(scope.clone(), pending.clone());
        let Some(repository) = self.repositories.get(key) else {
            return;
        };
        let connection = repository.connection.clone();
        if !connection.is_alive() {
            return;
        }
        let key = key.clone();
        let journal = self.operation_journal.clone();
        self.file_operation_running = true;
        self.notify(cx);
        cx.spawn(async move |this, cx| {
            let _claim = claim;
            let result: Result<FileOperationOutcome> = async {
                if scope != current_scope {
                    let discovered = connection.discover(scope.scope_uri.clone()).await?;
                    ensure!(discovered.into_iter().any(|descriptor| {
                        let mut discovered_scope = FileOperationScope {
                            extension_id: scope.extension_id.clone(),
                            provider_id: scope.provider_id.clone(),
                            repository_id: descriptor.id,
                            root_uri: descriptor.root_uri,
                            scope_uri: descriptor.scope_uri,
                        };
                        discovered_scope.normalize().is_ok() && discovered_scope == scope
                    }), "The original recovery scope changed; its operation remains unresolved");
                }
                let result = match &pending.request {
                    OperationRequest::File { path, operation, .. } => connection.reconcile_file_operation(
                        scope.repository_id.clone(), path.clone(), *operation, pending.token.clone(),
                    ).await?,
                    OperationRequest::Repository { operation, .. } => connection.reconcile_repository_operation(
                        scope.repository_id.clone(), operation.clone(), pending.token.clone(),
                    ).await?,
                    OperationRequest::Checkout { operation, target_root_uri, checkout } => {
                        let result = connection.reconcile_checkout_operation(
                            scope.repository_id.clone(), operation.clone(), pending.token.clone(),
                        ).await?;
                        result.validate(operation, target_root_uri, checkout.as_ref())?;
                        vcs::FileOperationResult { outcome: result.outcome }
                    }
                };
                journal.clear(&scope, &pending.token).await?;
                Ok(result.outcome)
            }.await;
            this.update(cx, |this, cx| {
                this.file_operation_running = false;
                let completed = result.is_ok();
                match result {
                    Ok(outcome) => {
                        this.pending_file_operations.remove(&scope);
                        match outcome {
                            FileOperationOutcome::Applied => {
                                this.file_operation_errors.remove(&key);
                                if let OperationRequest::File { affected_paths, operation: FileOperation::Discard | FileOperation::Ignore, .. } = &pending.request
                                    && let Err(error) = this.check_discarded_buffers(&scope, affected_paths, cx)
                                {
                                    this.file_operation_errors.insert(key.clone(), format!("{error:#}").into());
                                }
                                if let OperationRequest::Repository { operation, .. } = &pending.request
                                    && let Err(error) = this.check_repository_result_buffers(&key, operation, cx)
                                {
                                    this.file_operation_errors.insert(key.clone(), format!("{error:#}").into());
                                }
                            }
                            FileOperationOutcome::NotApplied => {
                                this.file_operation_errors.insert(key.clone(),
                                    "The previous operation was not applied. You can try again.".into());
                            }
                            FileOperationOutcome::Conflict => {
                                this.file_operation_errors.insert(key.clone(),
                                    "The file changed after the previous operation. Review its current state before continuing.".into());
                            }
                        }
                    }
                    Err(error) => {
                        this.file_operation_errors.insert(key.clone(),
                            format!("File operation outcome remains unknown: {error:#}").into());
                    }
                }
                if connection.is_alive() {
                    if completed {
                        this.reconcile_file_operation(&key, cx);
                    }
                    this.refresh_with_reason(&key, RefreshReason::Explicit, cx);
                    this.invalidate_checkout_catalog(&key);
                    this.refresh_checkout_catalog(&key, cx);
                }
                this.notify(cx);
            }).log_err();
        }).detach();
    }

    pub fn changes(&self, key: &VcsRepositoryId, cx: &App) -> Vec<vcs::Change> {
        self.repositories
            .get(key)
            .and_then(|repository| repository.view.snapshot.as_ref())
            .into_iter()
            .flat_map(|snapshot| snapshot.changes.iter())
            .filter(|change| {
                RelPath::from_unix_str(&change.path)
                    .ok()
                    .is_some_and(|path| {
                        self.repository_for_path(
                            &ProjectPath {
                                worktree_id: key.worktree_id,
                                path: Arc::from(path),
                            },
                            cx,
                        )
                        .as_ref()
                            == Some(key)
                    })
            })
            .cloned()
            .collect()
    }

    pub fn comparison_snapshot(
        &self,
        key: &VcsRepositoryId,
        baseline_id: &str,
    ) -> Option<Arc<Snapshot>> {
        self.repositories
            .get(key)
            .and_then(|repository| repository.comparisons.get(baseline_id))
            .cloned()
    }

    pub fn changes_for_baseline(
        &self,
        key: &VcsRepositoryId,
        baseline_id: &str,
        cx: &App,
    ) -> Vec<vcs::Change> {
        self.repositories
            .get(key)
            .and_then(|repository| repository.comparisons.get(baseline_id))
            .into_iter()
            .flat_map(|snapshot| &snapshot.changes)
            .filter(|change| self.check_read_path(key, &change.path, cx).is_ok())
            .cloned()
            .collect()
    }

    fn check_read_path(
        &self,
        key: &VcsRepositoryId,
        path: &str,
        cx: &App,
    ) -> Result<Arc<ProviderConnection>> {
        vcs::validate_path(path)?;
        let project_path = ProjectPath {
            worktree_id: key.worktree_id,
            path: Arc::from(RelPath::from_unix_str(path)?),
        };
        ensure!(
            self.repository_for_path(&project_path, cx).as_ref() == Some(key),
            "file belongs to another source-control provider"
        );
        let repository = self
            .repositories
            .get(key)
            .context("VCS repository closed")?;
        ensure!(
            repository.connection.is_alive(),
            "VCS provider is no longer running"
        );
        Ok(repository.connection.clone())
    }

    pub fn load_comparison(
        &mut self,
        key: VcsRepositoryId,
        baseline_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<Snapshot>>> {
        let request = (|| {
            ensure!(
                self.trusted(key.worktree_id, cx),
                "VCS worktree is not trusted"
            );
            let repository = self
                .repositories
                .get_mut(&key)
                .context("VCS repository closed")?;
            let baseline = repository
                .view
                .descriptor
                .baselines
                .iter()
                .find(|baseline| baseline.id == baseline_id)
                .context("Unknown VCS comparison")?;
            let source = baseline.current_content;
            let generation = repository
                .comparison_generations
                .entry(baseline_id.clone())
                .or_default();
            *generation += 1;
            Ok((repository.connection.clone(), *generation, source))
        })();
        let (connection, generation, source) = match request {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let snapshot = Arc::new(
                connection
                    .snapshot(key.repository_id.clone(), baseline_id.clone())
                    .await?,
            );
            ensure!(
                source != vcs::ContentSource::Snapshot || snapshot.current_revision_id.is_some(),
                "Snapshot comparison omitted its current content identity"
            );
            this.update(cx, |this, cx| {
                ensure!(
                    this.trusted(key.worktree_id, cx),
                    "VCS worktree is not trusted"
                );
                let repository = this
                    .repositories
                    .get_mut(&key)
                    .context("VCS repository closed")?;
                ensure!(
                    Arc::ptr_eq(&repository.connection, &connection),
                    "VCS provider changed"
                );
                if repository.comparison_generations.get(&baseline_id) != Some(&generation) {
                    if let Some(current) = repository.comparisons.get(&baseline_id) {
                        ensure!(
                            same_comparison(current, &snapshot)
                                && current.changes == snapshot.changes,
                            "VCS comparison request was superseded"
                        );
                        return Ok(current.clone());
                    }
                }
                let changed = repository
                    .comparisons
                    .get(&baseline_id)
                    .is_none_or(|previous| {
                        !same_comparison(previous, &snapshot)
                            || previous.changes != snapshot.changes
                    });
                repository
                    .comparisons
                    .insert(baseline_id.clone(), snapshot.clone());
                if repository.view.selected_baseline == baseline_id {
                    repository.view.snapshot = Some(snapshot.clone());
                    repository.view.error = None;
                }
                if changed {
                    this.refresh_open_comparison_diffs(&key, &baseline_id, cx);
                    cx.emit(VcsStoreEvent::DiffChanged {
                        worktree_id: key.worktree_id,
                    });
                }
                Ok(snapshot)
            })?
        })
    }

    fn refresh_open_comparison_diffs(
        &mut self,
        key: &VcsRepositoryId,
        baseline_id: &str,
        cx: &mut Context<Self>,
    ) {
        let paths: Vec<_> = self
            .repositories
            .get(key)
            .into_iter()
            .flat_map(|repository| repository.comparison_diffs.iter())
            .filter(|((_, baseline), state)| {
                baseline == baseline_id && state.diff.upgrade().is_some()
            })
            .map(|((path, _), _)| path.clone())
            .collect();
        for path in paths {
            self.open_diff_for_baseline(key.clone(), path, baseline_id.to_owned(), cx)
                .detach_and_log_err(cx);
        }
    }

    pub fn file_history_available(&self, key: &VcsRepositoryId) -> bool {
        self.repositories.get(key).is_some_and(|repository| {
            repository
                .connection
                .capabilities()
                .is_some_and(|capabilities| capabilities.file_history)
        })
    }

    pub fn file_history(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        cursor: Option<String>,
        limit: u32,
        cx: &mut Context<Self>,
    ) -> Task<Result<vcs::FileHistory>> {
        if !self.trusted(key.worktree_id, cx) {
            return Task::ready(Err(anyhow!("VCS worktree is not trusted")));
        }
        let connection = match self.check_read_path(&key, &path, cx) {
            Ok(connection) => connection,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let history = connection
                .file_history(key.repository_id.clone(), path.clone(), cursor, limit)
                .await?;
            this.update(cx, |this, cx| {
                ensure!(
                    this.trusted(key.worktree_id, cx),
                    "VCS worktree is not trusted"
                );
                let current = this.check_read_path(&key, &path, cx)?;
                ensure!(Arc::ptr_eq(&current, &connection), "VCS provider changed");
                for entry in &history.entries {
                    for path in [&entry.path, &entry.parent_path].into_iter().flatten() {
                        this.check_read_path(&key, path, cx)?;
                    }
                }
                Ok(history)
            })?
        })
    }

    pub fn open_revision_diff(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        entry: vcs::FileHistoryEntry,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsDiff>> {
        let request = (|| {
            ensure!(
                self.trusted(key.worktree_id, cx),
                "VCS worktree is not trusted"
            );
            entry.validate()?;
            let connection = self.check_read_path(&key, &path, cx)?;
            let current_path = entry.path.clone().unwrap_or(path.clone());
            let parent_path = entry.parent_path.clone().unwrap_or(current_path.clone());
            self.check_read_path(&key, &current_path, cx)?;
            self.check_read_path(&key, &parent_path, cx)?;
            Ok((connection, current_path, parent_path))
        })();
        let (connection, current_path, parent_path) = match request {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let base = match entry.parent_revision_id {
                Some(revision) => {
                    connection
                        .read_revision(key.repository_id.clone(), parent_path, revision)
                        .await?
                }
                None => Content::Absent,
            };
            let current = connection
                .read_revision(
                    key.repository_id.clone(),
                    current_path,
                    entry.commit.revision_id,
                )
                .await?;
            this.update(cx, |this, cx| {
                ensure!(
                    this.trusted(key.worktree_id, cx),
                    "VCS worktree is not trusted"
                );
                let active = this.check_read_path(&key, &path, cx)?;
                ensure!(Arc::ptr_eq(&active, &connection), "VCS provider changed");
                anyhow::Ok(this.create_snapshot_diff(base, current, cx))
            })??
            .await
        })
    }

    fn create_snapshot_diff(
        &mut self,
        base: Content,
        current: Content,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsDiff>> {
        let contents = (|| {
            Ok((
                content_text(base)?,
                content_text(current)?.unwrap_or_default(),
            ))
        })();
        let (base, current) = match contents {
            Ok(contents) => contents,
            Err(error) => return Task::ready(Err(error)),
        };
        let Some(project) = self.project.upgrade() else {
            return Task::ready(Err(anyhow!("VCS project closed")));
        };
        let buffer = project.update(cx, |project, cx| {
            project.create_local_buffer(&current, None, false, cx)
        });
        buffer.update(cx, |buffer, cx| {
            buffer.set_capability(language::Capability::ReadOnly, cx)
        });
        let snapshot = buffer.read(cx).snapshot();
        let language_registry = buffer.read(cx).language_registry();
        let diff = cx.new(|cx| {
            let mut diff = BufferDiff::new(
                &snapshot.text,
                snapshot.language().cloned(),
                language_registry,
                cx,
            );
            diff.set_operations(Arc::new(ReadOnlyDiffOperations));
            diff
        });
        let update = diff.update(cx, |diff, cx| diff.set_base_text(base, snapshot.text, cx));
        cx.spawn(async move |_, _| {
            update.await;
            Ok(VcsDiff {
                buffer,
                diff,
                current_content: vcs::ContentSource::Snapshot,
            })
        })
    }

    pub fn open_diff_for_purpose(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        purpose: vcs::ComparisonPurpose,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsDiff>> {
        let baseline = self
            .repositories
            .get(&key)
            .and_then(|repository| {
                repository
                    .view
                    .descriptor
                    .baselines
                    .iter()
                    .find(|baseline| baseline.purpose == Some(purpose))
            })
            .map(|baseline| baseline.id.clone());
        match baseline {
            Some(baseline) => self.open_diff_for_baseline(key, path, baseline, cx),
            None => Task::ready(Err(anyhow!("VCS comparison is unavailable"))),
        }
    }

    pub fn open_diff_for_baseline(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        baseline_id: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsDiff>> {
        let request = (|| {
            ensure!(
                self.trusted(key.worktree_id, cx),
                "VCS worktree is not trusted"
            );
            let connection = self.check_read_path(&key, &path, cx)?;
            let repository = self
                .repositories
                .get(&key)
                .context("VCS repository closed")?;
            let source = repository
                .view
                .descriptor
                .baselines
                .iter()
                .find(|baseline| baseline.id == baseline_id)
                .context("Unknown VCS comparison")?
                .current_content;
            let root = vcs::local_path(&repository.view.descriptor.scope_uri)?;
            Ok((connection, source, root))
        })();
        let (connection, source, root) = match request {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        let snapshot = match self
            .repositories
            .get(&key)
            .and_then(|repository| repository.comparisons.get(&baseline_id))
            .cloned()
        {
            Some(snapshot) => Task::ready(Ok(snapshot)),
            None => self.load_comparison(key.clone(), baseline_id.clone(), cx),
        };
        let fs = self.fs.clone();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let snapshot = snapshot.await?;
            let base = connection
                .read_content(
                    snapshot.repository_id.clone(),
                    snapshot.id.clone(),
                    baseline_id.clone(),
                    path.clone(),
                )
                .await?;
            if source == vcs::ContentSource::Snapshot {
                let current = connection
                    .read_content_side(
                        snapshot.repository_id.clone(),
                        snapshot.id.clone(),
                        baseline_id.clone(),
                        path.clone(),
                        vcs::ContentSide::Current,
                    )
                    .await?;
                return this
                    .update(cx, |this, cx| {
                        this.check_comparison(&key, &path, &connection, &snapshot, cx)?;
                        anyhow::Ok(this.create_snapshot_diff(base, current, cx))
                    })??
                    .await;
            }
            let base = content_text(base)?;
            let project_path = ProjectPath {
                worktree_id: key.worktree_id,
                path: Arc::from(RelPath::from_unix_str(&path)?),
            };
            let metadata = scoped_metadata(&root, &path, &fs).await?;
            let existing =
                project.read_with(cx, |project, cx| project.get_open_buffer(&project_path, cx))?;
            let buffer = if let Some(buffer) = existing {
                buffer
            } else {
                match metadata {
                    Some(metadata) => {
                        ensure!(
                            !metadata.is_dir && !metadata.is_symlink,
                            "This change has no text diff"
                        );
                        project
                            .update(cx, |project, cx| project.open_buffer(project_path, cx))?
                            .await?
                    }
                    None => {
                        let buffer = project.update(cx, |project, cx| {
                            project.create_local_buffer("", None, false, cx)
                        })?;
                        buffer.update(cx, |buffer, cx| {
                            buffer.set_capability(language::Capability::ReadOnly, cx)
                        });
                        buffer
                    }
                }
            };
            let (diff, updated) = this.update(cx, |this, cx| {
                this.check_comparison(&key, &path, &connection, &snapshot, cx)?;
                let repository = this
                    .repositories
                    .get_mut(&key)
                    .context("VCS repository closed")?;
                repository.comparison_diffs.retain(|_, state| {
                    state.buffer.upgrade().is_some() && state.diff.upgrade().is_some()
                });
                let cache_key = (path.clone(), baseline_id);
                let existing = repository
                    .comparison_diffs
                    .get(&cache_key)
                    .filter(|state| state.buffer.upgrade().as_ref() == Some(&buffer))
                    .and_then(|state| state.diff.upgrade());
                let diff = if let Some(diff) = existing {
                    diff
                } else {
                    ensure!(
                        repository.comparison_diffs.len() < 128,
                        "Too many open VCS comparisons; close a comparison before opening another"
                    );
                    let snapshot = buffer.read(cx).snapshot();
                    let language_registry = buffer.read(cx).language_registry();
                    let diff = cx.new(|cx| {
                        let mut diff = BufferDiff::new(
                            &snapshot.text,
                            snapshot.language().cloned(),
                            language_registry,
                            cx,
                        );
                        diff.set_operations(Arc::new(ReadOnlyDiffOperations));
                        diff
                    });
                    let repository_key = key.clone();
                    let comparison_key = cache_key.clone();
                    let subscription = cx.subscribe(&buffer, move |this, _, event, cx| {
                        if matches!(event, BufferEvent::Edited { .. } | BufferEvent::Reloaded) {
                            this.update_comparison_diff(&repository_key, &comparison_key, cx);
                        }
                    });
                    repository.comparison_diffs.insert(
                        cache_key.clone(),
                        ComparisonDiff {
                            buffer: buffer.downgrade(),
                            diff: diff.downgrade(),
                            base: None,
                            generation: 0,
                            update_task: None,
                            waiters: Vec::new(),
                            _subscription: subscription,
                        },
                    );
                    diff
                };
                let state = repository
                    .comparison_diffs
                    .get_mut(&cache_key)
                    .context("VCS comparison closed")?;
                state.base = base;
                let (sender, receiver) = futures::channel::oneshot::channel();
                state.waiters.retain(|waiter| !waiter.is_canceled());
                state.waiters.push(sender);
                this.update_comparison_diff(&key, &cache_key, cx);
                Ok((diff, receiver))
            })??;
            updated
                .await
                .context("VCS comparison update was interrupted")?;
            Ok(VcsDiff {
                buffer,
                diff,
                current_content: vcs::ContentSource::Working,
            })
        })
    }

    fn update_comparison_diff(
        &mut self,
        repository_key: &VcsRepositoryId,
        comparison_key: &(String, String),
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self
            .repositories
            .get_mut(repository_key)
            .and_then(|repository| repository.comparison_diffs.get_mut(comparison_key))
        else {
            return;
        };
        state.update_task = None;
        let Some((buffer, diff)) = state.buffer.upgrade().zip(state.diff.upgrade()) else {
            state.waiters.clear();
            return;
        };
        state.generation += 1;
        let generation = state.generation;
        let snapshot = buffer.read(cx).text_snapshot();
        let update = diff.update(cx, |diff, cx| {
            diff.set_base_text(state.base.clone(), snapshot, cx)
        });
        let repository_key = repository_key.clone();
        let comparison_key = comparison_key.clone();
        state.update_task = Some(cx.spawn(async move |this, cx| {
            update.await;
            this.update(cx, |this, _| {
                let Some(state) = this
                    .repositories
                    .get_mut(&repository_key)
                    .and_then(|repository| repository.comparison_diffs.get_mut(&comparison_key))
                else {
                    return;
                };
                if state.generation != generation {
                    return;
                }
                state.update_task = None;
                for waiter in state.waiters.drain(..) {
                    if waiter.send(()).is_err() {
                        log::debug!("VCS comparison consumer closed");
                    }
                }
            })
            .log_err();
        }));
    }

    fn check_comparison(
        &self,
        key: &VcsRepositoryId,
        path: &str,
        connection: &Arc<ProviderConnection>,
        snapshot: &Snapshot,
        cx: &App,
    ) -> Result<()> {
        let current = self.check_read_path(key, path, cx)?;
        ensure!(Arc::ptr_eq(&current, connection), "VCS provider changed");
        ensure!(
            self.repositories
                .get(key)
                .and_then(|repository| repository.comparisons.get(&snapshot.baseline_id))
                .is_some_and(|current| same_comparison(current, snapshot)),
            "VCS comparison changed while reading its content"
        );
        Ok(())
    }

    pub fn owns_absolute_path(&self, path: &Path, cx: &App) -> bool {
        self.sessions
            .iter()
            .filter(|(_, session)| session.claims_scope)
            .any(|((worktree_id, _), _)| {
                self.worktrees
                    .read(cx)
                    .worktree_for_id(*worktree_id, cx)
                    .is_some_and(|worktree| {
                        let root = worktree.read(cx).abs_path();
                        let Ok(relative) = path.strip_prefix(&root) else {
                            return false;
                        };
                        let Ok(relative) = RelPath::new(relative, worktree.read(cx).path_style())
                        else {
                            return false;
                        };
                        self.git
                            .read(cx)
                            .repository_and_path_for_project_path(
                                &ProjectPath {
                                    worktree_id: *worktree_id,
                                    path: relative.into(),
                                },
                                cx,
                            )
                            .is_none()
                    })
            })
    }

    pub fn check_git_initialization(&self, path: Arc<Path>, cx: &App) -> Task<Result<()>> {
        self.check_git_operation(path, false, cx)
    }

    pub fn check_git_destructive_operation(&self, path: Arc<Path>, cx: &App) -> Task<Result<()>> {
        self.check_git_operation(path, true, cx)
    }

    fn check_git_operation(
        &self,
        path: Arc<Path>,
        external_markers_first: bool,
        cx: &App,
    ) -> Task<Result<()>> {
        let markers: Vec<Vec<String>> = GlobalRegistry::global(cx)
            .0
            .read(cx)
            .registrations
            .values()
            .filter_map(|registration| {
                registration
                    .extension
                    .manifest()
                    .vcs_providers
                    .get(&registration.provider_id)
                    .map(|provider| provider.root_markers.clone())
            })
            .collect();
        let settings_location =
            self.worktrees
                .read(cx)
                .find_worktree(&path, cx)
                .map(|(worktree, _)| SettingsLocation {
                    worktree_id: worktree.read(cx).id(),
                    path: RelPath::empty(),
                });
        let excluded_markers = worktree::WorktreeSettings::get(settings_location, cx)
            .git_repository_exclusion_markers
            .clone();
        if markers.is_empty() && excluded_markers.is_empty() {
            return Task::ready(Ok(()));
        }
        let fs = self.fs.clone();
        cx.background_executor().spawn(async move {
            for ancestor in path.ancestors() {
                ensure!(
                    !worktree::is_git_repository_excluded(
                        &ancestor.join(".git"), &excluded_markers, fs.as_ref()
                    ).await?,
                    "Git is unavailable because this repository matches git.excluded_repository_markers"
                );
                if !external_markers_first && fs.metadata(&ancestor.join(".git")).await?.is_some() { return Ok(()); }
                for provider_markers in &markers {
                    for marker in provider_markers {
                        ensure!(!marker.is_empty() && marker != "." && marker != ".." && !marker.contains(['/', '\\', '\0']), "invalid VCS repository marker");
                        if fs.metadata(&ancestor.join(marker)).await?.is_some() {
                            let operation = if external_markers_first { "mutation" } else { "initialization" };
                            bail!("The folder has an external VCS repository marker; Git {operation} is unavailable");
                        }
                    }
                }
                if external_markers_first && fs.metadata(&ancestor.join(".git")).await?.is_some() {
                    return Ok(());
                }
            }
            Ok(())
        })
    }

    /// The Git-shaped summary is only a paint adapter for the existing tree.
    /// It does not create an index, Git repository, or mutation capability.
    pub fn summary_for_path(
        &self,
        path: &ProjectPath,
        cx: &App,
    ) -> Option<git::status::GitSummary> {
        let key = self.repository_for_path(path, cx)?;
        let repository = self.repositories.get(&key)?;
        let snapshot = repository.view.snapshot.as_ref()?;
        if repository.view.error.is_some() {
            return None;
        }
        let prefix = path.path.as_unix_str();
        let mut summary = git::status::GitSummary::default();
        for change in &snapshot.changes {
            if !prefix.is_empty()
                && change.path != prefix
                && !change
                    .path
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            {
                continue;
            }
            let Ok(relative) = RelPath::from_unix_str(&change.path) else {
                continue;
            };
            let change_path = ProjectPath {
                worktree_id: key.worktree_id,
                path: Arc::from(relative),
            };
            if self
                .git
                .read(cx)
                .repository_and_path_for_project_path(&change_path, cx)
                .is_some()
            {
                continue;
            }
            summary.count += 1;
            match change.status {
                ChangeStatus::Added | ChangeStatus::Untracked => summary.untracked += 1,
                ChangeStatus::Deleted => summary.worktree.deleted += 1,
                ChangeStatus::Modified | ChangeStatus::Renamed => summary.worktree.modified += 1,
            }
        }
        Some(summary)
    }

    pub fn open_buffer_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let Some(path) = buffer.read(cx).project_path(cx) else {
            return Task::ready(Err(anyhow!("buffer has no VCS path")));
        };
        let Some(key) = self.repository_for_path(&path, cx) else {
            return Task::ready(Err(anyhow!("buffer has no unambiguous VCS provider")));
        };
        let relative = path.path.as_unix_str().to_owned();
        let working = self
            .repositories
            .get(&key)
            .and_then(|repository| {
                repository
                    .view
                    .descriptor
                    .baselines
                    .iter()
                    .find(|baseline| baseline.purpose == Some(vcs::ComparisonPurpose::Working))
            })
            .map(|baseline| baseline.id.clone());
        if let Some(baseline) = working {
            let task = self.open_diff_for_baseline(key, relative, baseline, cx);
            return cx.spawn(async move |_, _| Ok(task.await?.diff));
        }
        self.ensure_diff(&key, &relative, &buffer, cx);
        self.load_diff_if_needed(buffer, cx)
    }

    pub fn open_diff(
        &mut self,
        key: VcsRepositoryId,
        path: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsDiff>> {
        let selected = self
            .repositories
            .get(&key)
            .and_then(|repository| {
                repository
                    .view
                    .descriptor
                    .baselines
                    .iter()
                    .find(|baseline| baseline.id == repository.view.selected_baseline)
            })
            .filter(|baseline| {
                baseline.purpose.is_some()
                    || baseline.current_content == vcs::ContentSource::Snapshot
            })
            .map(|baseline| baseline.id.clone());
        if let Some(baseline) = selected {
            return self.open_diff_for_baseline(key, path, baseline, cx);
        }
        let result = (|| {
            vcs::validate_path(&path)?;
            let project_path = ProjectPath {
                worktree_id: key.worktree_id,
                path: Arc::from(RelPath::from_unix_str(&path)?),
            };
            ensure!(
                self.repository_for_path(&project_path, cx).as_ref() == Some(&key),
                "file belongs to another source-control provider"
            );
            let root = self
                .worktrees
                .read(cx)
                .worktree_for_id(key.worktree_id, cx)
                .context("VCS worktree closed")?
                .read(cx)
                .abs_path();
            Ok((project_path, root))
        })();
        let (project_path, root) = match result {
            Ok(result) => result,
            Err(error) => return Task::ready(Err(error)),
        };
        let project = self.project.clone();
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            let buffer = match scoped_metadata(&root, &path, &fs).await? {
                Some(metadata) => {
                    ensure!(!metadata.is_dir && !metadata.is_symlink, "binary, directory and symlink changes are displayed in the changes list without a text diff");
                    project.update(cx, |project, cx| project.open_buffer(project_path, cx))?.await?
                }
                None => project.update(cx, |project, cx| {
                    let buffer = project.create_local_buffer("", None, false, cx);
                    buffer.update(cx, |buffer, cx| buffer.set_capability(language::Capability::ReadOnly, cx));
                    buffer
                })?,
            };
            let diff_task = this.update(cx, |this, cx| {
                this.ensure_diff(&key, &path, &buffer, cx);
                this.load_diff_if_needed(buffer.clone(), cx)
            })?;
            let diff = diff_task.await?;
            Ok(VcsDiff { buffer, diff, current_content: vcs::ContentSource::Working })
        })
    }

    fn ensure_diff(
        &mut self,
        key: &VcsRepositoryId,
        path: &str,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) {
        let buffer_id = buffer.read(cx).remote_id();
        if self
            .diffs
            .get(&buffer_id)
            .is_some_and(|state| state.repository == *key && state.path == path)
        {
            return;
        }
        let snapshot = buffer.read(cx).snapshot();
        let language_registry = buffer.read(cx).language_registry();
        let diff = cx.new(|cx| {
            let mut diff = BufferDiff::new(
                &snapshot.text,
                snapshot.language().cloned(),
                language_registry,
                cx,
            );
            diff.set_operations(Arc::new(ReadOnlyDiffOperations));
            diff
        });
        let subscription = cx.subscribe(buffer, |this, buffer, event, cx| {
            if matches!(
                event,
                BufferEvent::Edited { .. } | BufferEvent::Reloaded | BufferEvent::FileHandleChanged
            ) {
                let buffer_id = buffer.read(cx).remote_id();
                if this
                    .diffs
                    .get(&buffer_id)
                    .is_some_and(|state| !state.base_loaded)
                {
                    if this
                        .diffs
                        .get(&buffer_id)
                        .is_some_and(|state| !state.loading)
                    {
                        this.start_diff_load(buffer, cx);
                    }
                    return;
                }
                this.update_cached_diff(buffer, cx);
            }
        });
        self.diffs.insert(
            buffer_id,
            DiffState {
                repository: key.clone(),
                path: path.to_owned(),
                buffer: buffer.downgrade(),
                diff,
                base: None,
                base_loaded: false,
                loading: false,
                generation: 0,
                update_task: None,
                waiters: Vec::new(),
                _subscription: subscription,
            },
        );
    }

    fn load_diff_if_needed(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let buffer_id = buffer.read(cx).remote_id();
        let Some(state) = self.diffs.get(&buffer_id) else {
            return Task::ready(Err(anyhow!("VCS diff is unavailable")));
        };
        if state.base_loaded && !state.loading {
            return Task::ready(Ok(state.diff.clone()));
        }
        let loading = state.loading;
        let task = self.wait_for_diff(buffer_id, cx);
        if !loading {
            self.start_diff_load(buffer, cx);
        }
        task
    }

    fn wait_for_diff(
        &mut self,
        buffer_id: text::BufferId,
        cx: &Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let Some(state) = self.diffs.get_mut(&buffer_id) else {
            return Task::ready(Err(anyhow!("VCS diff is unavailable")));
        };
        let (sender, receiver) = futures::channel::oneshot::channel();
        state.waiters.retain(|waiter| !waiter.is_canceled());
        state.waiters.push(sender);
        cx.background_spawn(async move {
            receiver
                .await
                .context("VCS diff owner closed")?
                .map_err(|error| anyhow!("{error:#}"))
        })
    }

    fn start_diff_load(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let buffer_id = buffer.read(cx).remote_id();
        let Some(state) = self.diffs.get_mut(&buffer_id) else {
            return;
        };
        let request = (|| {
            let repository = self
                .repositories
                .get(&state.repository)
                .context("VCS repository closed")?;
            if let Some(error) = &repository.view.error {
                bail!("{error}");
            }
            let Some(snapshot) = repository.view.snapshot.clone() else {
                ensure!(repository.view.loading, "VCS snapshot is unavailable");
                return Ok(None);
            };
            let root = vcs::local_path(&repository.view.descriptor.scope_uri)?;
            Ok(Some((repository.connection.clone(), snapshot, root)))
        })();
        let (connection, snapshot, root) = match request {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(error) => {
                state.finish_waiters(Err(error));
                return;
            }
        };
        let fs = self.fs.clone();
        let path = state.path.clone();
        state.base_loaded = false;
        state.loading = true;
        state.generation += 1;
        let generation = state.generation;
        state.update_task = None;
        state.update_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<Option<Arc<str>>> = async {
                scoped_metadata(&root, &path, &fs).await?;
                let content = connection
                    .read_content(
                        snapshot.repository_id.clone(),
                        snapshot.id.clone(),
                        snapshot.baseline_id.clone(),
                        path,
                    )
                    .await?;
                match content {
                    Content::Absent => Ok(None),
                    Content::Text { mut text } => {
                        text::LineEnding::normalize(&mut text);
                        Ok(Some(Arc::<str>::from(text)))
                    }
                    Content::Binary => {
                        bail!("Binary file: content is available only through its VCS provider")
                    }
                    Content::Symlink { .. } => {
                        bail!("Symlink: target changes are displayed in the changes list")
                    }
                }
            }
            .await;
            this.update(cx, |this, cx| {
                let Some(state) = this.diffs.get_mut(&buffer_id) else {
                    return;
                };
                if state.generation != generation {
                    return;
                }
                let result = result.and_then(|base| {
                    ensure!(
                        this.repositories
                            .get(&state.repository)
                            .and_then(|repository| repository.view.snapshot.as_ref())
                            .is_some_and(|current| current.revision_id == snapshot.revision_id
                                && current.baseline_id == snapshot.baseline_id
                                && current.current_revision_id == snapshot.current_revision_id),
                        "VCS snapshot changed"
                    );
                    Ok(base)
                });
                state.update_task = None;
                match result {
                    Ok(base) => {
                        state.base = base;
                        state.base_loaded = true;
                        this.update_cached_diff(buffer, cx);
                    }
                    Err(error) => {
                        state.loading = false;
                        log::error!("Failed to load VCS diff content: {error:#}");
                        state.finish_waiters(Err(error));
                        let current = buffer.read(cx).text_snapshot();
                        state.update_task = Some(state.diff.update(cx, |diff, cx| {
                            diff.set_base_text(Some(current.text().into()), current, cx)
                        }));
                    }
                }
            })
            .log_err();
        }));
    }

    fn update_cached_diff(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let buffer_id = buffer.read(cx).remote_id();
        let Some(state) = self.diffs.get_mut(&buffer_id) else {
            return;
        };
        state.generation += 1;
        let generation = state.generation;
        state.loading = true;
        state.update_task = None;
        let snapshot = buffer.read(cx).text_snapshot();
        let update = state.diff.update(cx, |diff, cx| {
            diff.set_base_text(state.base.clone(), snapshot, cx)
        });
        state.update_task = Some(cx.spawn(async move |this, cx| {
            update.await;
            this.update(cx, |this, _| {
                if let Some(state) = this.diffs.get_mut(&buffer_id)
                    && state.generation == generation
                {
                    state.update_task = None;
                    state.loading = false;
                    state.finish_waiters(Ok(state.diff.clone()));
                }
            })
            .log_err();
        }));
    }

    fn invalidate_diffs(
        &mut self,
        repository: &VcsRepositoryId,
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        for state in self
            .diffs
            .values_mut()
            .filter(|state| state.repository == *repository)
        {
            state.generation += 1;
            state.update_task = None;
            state.base_loaded = false;
            state.loading = false;
            if let Some(error) = error {
                state.finish_waiters(Err(anyhow!("{error}")));
            }
            if let Some(buffer) = state.buffer.upgrade() {
                let snapshot = buffer.read(cx).text_snapshot();
                let current: Arc<str> = snapshot.text().into();
                state.base = Some(current.clone());
                state.update_task = Some(state.diff.update(cx, |diff, cx| {
                    diff.set_base_text(Some(current), snapshot, cx)
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn provider_which_inherits_process_path_only_when_directory_path_is_missing(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let mut worktree = ProviderWorktree {
            id: WorktreeId::from_proto(0),
            root: directory.path().into(),
            fs: fs::FakeFs::new(cx.background_executor.clone()),
            environment: HashMap::default(),
        };
        let expected = which::which("git").expect("Git is available to project tests");
        assert_eq!(
            worktree.which("git".into()).await,
            Some(expected.to_string_lossy().into_owned())
        );
        for path in [
            String::new(),
            directory.path().to_string_lossy().into_owned(),
        ] {
            worktree.environment.insert("PATH".into(), path);
            assert_eq!(worktree.which("git".into()).await, None);
        }
    }

    #[gpui::test]
    async fn marker_discovery_is_ancestor_scoped_and_rejects_paths(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/repository",
            serde_json::json!({
                ".example-vcs": "",
                "project": {"source": {}},
                "unrelated": {".nested-provider": ""}
            }),
        )
        .await;
        let fs: Arc<dyn Fs> = fs;
        assert!(
            matches_markers(
                Path::new("/repository/project/source"),
                &[".example-vcs".into()],
                &fs
            )
            .await
            .unwrap()
        );
        assert!(
            !matches_markers(
                Path::new("/repository/project"),
                &[".nested-provider".into()],
                &fs
            )
            .await
            .unwrap()
        );
        assert!(
            matches_markers(
                Path::new("/repository/project"),
                &["../.example-vcs".into()],
                &fs
            )
            .await
            .is_err()
        );
    }

    #[gpui::test]
    async fn external_diff_uses_live_buffer_and_denies_restore(cx: &mut gpui::TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("saved\n", cx));
        let diff = cx.new(|cx| {
            let snapshot = buffer.read(cx).snapshot();
            let mut diff = BufferDiff::new(&snapshot.text, None, None, cx);
            diff.set_operations(Arc::new(ReadOnlyDiffOperations));
            diff
        });
        let initial = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.set_base_text(Some("saved\n".into()), initial, cx)
        })
        .await;
        assert!(diff.read_with(cx, |diff, cx| diff.snapshot(cx).is_empty()));
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..5, "unsaved")], None, cx);
        });
        let edited = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.set_base_text(Some("saved\n".into()), edited, cx)
        })
        .await;
        diff.read_with(cx, |diff, cx| {
            assert!(!diff.snapshot(cx).is_empty());
            let operations = diff.operations().unwrap();
            assert!(!operations.supports_staging());
            assert!(!operations.supports_unstaging());
            assert!(!operations.supports_restore());
        });
    }

    #[gpui::test]
    async fn text_diff_rejects_symlink_parents_and_accepts_deleted_files(
        cx: &mut gpui::TestAppContext,
    ) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/scope",
            serde_json::json!({"ordinary": {"file.txt": "text"}}),
        )
        .await;
        fs.insert_tree("/outside", serde_json::json!({"private.txt": "outside"}))
            .await;
        fs.insert_symlink("/scope/link", Path::new("/outside").to_path_buf())
            .await;
        let fs: Arc<dyn Fs> = fs;
        assert!(
            scoped_metadata(Path::new("/scope"), "link/private.txt", &fs)
                .await
                .is_err()
        );
        assert!(
            scoped_metadata(Path::new("/scope"), "ordinary/deleted.txt", &fs)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            scoped_metadata(Path::new("/scope"), "ordinary/file.txt", &fs)
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[cfg(all(test, unix, feature = "test-support"))]
#[path = "vcs_store_tests.rs"]
mod integration_tests;

fn same_comparison(left: &Snapshot, right: &Snapshot) -> bool {
    left.repository_id == right.repository_id
        && left.baseline_id == right.baseline_id
        && left.revision_id == right.revision_id
        && left.current_revision_id == right.current_revision_id
}

fn content_text(content: Content) -> Result<Option<Arc<str>>> {
    content.validate()?;
    match content {
        Content::Absent => Ok(None),
        Content::Text { mut text } => {
            text::LineEnding::normalize(&mut text);
            Ok(Some(text.into()))
        }
        Content::Binary => bail!("Binary file: no text diff is available"),
        Content::Symlink { .. } => {
            bail!("Symlink: target changes are displayed in the changes list")
        }
    }
}

async fn matches_markers(root: &Path, markers: &[String], fs: &Arc<dyn Fs>) -> Result<bool> {
    ensure!(
        !markers.is_empty(),
        "VCS provider must declare a repository marker"
    );
    for marker in markers {
        ensure!(
            !marker.is_empty()
                && marker != "."
                && marker != ".."
                && !marker.contains(['/', '\\', '\0']),
            "invalid VCS repository marker"
        );
    }
    for ancestor in root.ancestors() {
        for marker in markers {
            if fs.metadata(&ancestor.join(marker)).await?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn validate_file_metadata(root: &Path, change: &vcs::Change, fs: &Arc<dyn Fs>) -> Result<()> {
    let explicit = change.file_operations.is_some();
    let mut paths = vec![(change.path.as_str(), false)];
    if let Some(old_path) = &change.old_path {
        if old_path != &change.path {
            paths.push((old_path, true));
        }
    }
    for (path, old_path) in paths {
        let symlink = explicit
            && if old_path {
                change
                    .old_mode
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
            } else {
                change.kind == vcs::ContentKind::Symlink
            };
        match scoped_entry_metadata(root, path, fs, symlink).await? {
            Some(metadata) => ensure!(
                !metadata.is_dir || (symlink && metadata.is_symlink),
                "File operations cannot target a directory"
            ),
            None => ensure!(explicit, "An affected file no longer exists"),
        }
    }
    Ok(())
}

async fn validate_ignore_config(root: &Path, path: &str, fs: &Arc<dyn Fs>) -> Result<()> {
    let metadata = scoped_metadata(root, path, fs).await?;
    ensure!(
        metadata.is_none_or(|metadata| !metadata.is_dir && !metadata.is_symlink),
        "Ignore configuration must be a regular file"
    );
    Ok(())
}

fn staging_batch_targets(
    mut changes: Vec<vcs::Change>,
    staged_paths: &[String],
    operation: FileOperation,
) -> Result<Vec<vcs::Change>> {
    changes.retain(|change| {
        let staged = change.staged_status.is_some() || staged_paths.contains(&change.path);
        match operation {
            FileOperation::Stage => !staged || change.unstaged_status.is_some(),
            FileOperation::Unstage => staged,
            _ => false,
        }
    });
    changes.sort_by(|left, right| {
        left.old_path
            .is_none()
            .cmp(&right.old_path.is_none())
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut covered = HashSet::default();
    let mut targets = Vec::new();
    for change in changes {
        if let Some(old_path) = &change.old_path {
            ensure!(
                !covered.contains(&change.path) && !covered.contains(old_path),
                "Overlapping rename paths prevent staging this batch"
            );
            covered.insert(old_path.clone());
        } else if covered.contains(&change.path) {
            continue;
        }
        covered.insert(change.path.clone());
        targets.push(change);
    }
    targets.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(targets)
}

fn legacy_file_change(change: &vcs::Change) -> bool {
    change.status == ChangeStatus::Modified
        && change.kind == vcs::ContentKind::File
        && change.old_path.is_none()
        && change
            .old_mode
            .zip(change.new_mode)
            .is_none_or(|(old, new)| old == new)
}

async fn scoped_metadata(
    root: &Path,
    path: &str,
    fs: &Arc<dyn Fs>,
) -> Result<Option<fs::Metadata>> {
    scoped_entry_metadata(root, path, fs, false).await
}

async fn scoped_entry_metadata(
    root: &Path,
    path: &str,
    fs: &Arc<dyn Fs>,
    allow_leaf_symlink: bool,
) -> Result<Option<fs::Metadata>> {
    vcs::validate_path(path)?;
    let mut cursor = root.to_path_buf();
    let mut metadata = fs.metadata(&cursor).await?;
    ensure!(
        metadata.as_ref().is_some_and(|metadata| metadata.is_dir),
        "VCS scope no longer exists"
    );
    ensure!(
        metadata
            .as_ref()
            .is_none_or(|metadata| !metadata.is_symlink),
        "VCS text diffs do not follow symlink scopes"
    );
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        if metadata.is_none() {
            return Ok(None);
        }
        cursor.push(component);
        metadata = fs.metadata(&cursor).await?;
        ensure!(
            metadata.as_ref().is_none_or(|metadata| !metadata.is_symlink
                || (allow_leaf_symlink && components.peek().is_none())),
            "VCS operations do not follow symlink parents"
        );
    }
    Ok(metadata)
}

struct ProviderWorktree {
    id: WorktreeId,
    root: Arc<Path>,
    fs: Arc<dyn Fs>,
    environment: HashMap<String, String>,
}

#[async_trait::async_trait]
impl WorktreeDelegate for ProviderWorktree {
    fn id(&self) -> u64 {
        self.id.to_proto()
    }
    fn root_path(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
    async fn read_text_file(&self, path: &RelPath) -> Result<String> {
        scoped_metadata(&self.root, path.as_unix_str(), &self.fs).await?;
        self.fs.load(&self.root.join(path.as_std_path())).await
    }
    async fn which(&self, binary_name: String) -> Option<String> {
        let path = self
            .environment
            .get("PATH")
            .map(std::ffi::OsString::from)
            .or_else(|| std::env::var_os("PATH"));
        which::which_in(binary_name, path, &self.root)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    }
    async fn shell_env(&self) -> Vec<(String, String)> {
        self.environment.clone().into_iter().collect()
    }
}
