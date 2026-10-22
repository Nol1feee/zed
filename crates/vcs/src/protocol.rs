use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u32 = 1;
pub const SUPPORTED_PROTOCOL_VERSIONS: &[u32] = &[1, 2];
pub const MAX_OPERATION_TOKEN_BYTES: usize = 4096;
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FILE_BATCH_PATHS: usize = 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub supported_protocol_versions: Vec<u32>,
    pub client_info: ProviderInfo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub read_only: bool,
    #[serde(default)]
    pub blame: bool,
    #[serde(default)]
    pub file_history: bool,
    #[serde(default)]
    pub file_operations: Vec<FileOperation>,
    #[serde(default)]
    pub batch_file_operations: Vec<FileOperation>,
    #[serde(default)]
    pub repository_operations: Vec<RepositoryOperationKind>,
    #[serde(default)]
    pub checkout_catalog: bool,
    #[serde(default)]
    pub checkout_operations: Vec<CheckoutOperationKind>,
}

impl ProviderCapabilities {
    pub fn supports_file_operation(&self, operation: FileOperation) -> bool {
        !self.read_only && self.file_operations.contains(&operation)
    }

    pub fn supports_batch_file_operation(&self, operation: FileOperation) -> bool {
        matches!(operation, FileOperation::Stage | FileOperation::Unstage)
            && self.supports_file_operation(operation)
            && self.batch_file_operations.contains(&operation)
    }

    pub fn supports_repository_operation(&self, operation: RepositoryOperationKind) -> bool {
        !self.read_only && self.repository_operations.contains(&operation)
    }

    pub fn supports_checkout_operation(&self, operation: CheckoutOperationKind) -> bool {
        !self.read_only && self.checkout_operations.contains(&operation)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileOperation {
    Stage,
    Unstage,
    Discard,
    Ignore,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CheckoutOperationKind {
    Create,
    Open,
    Prepare,
    Suspend,
    Remove,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "operation",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CheckoutOperation {
    Create {
        name: String,
        branch: String,
        base: String,
    },
    Open {
        checkout_id: String,
    },
    Prepare {
        checkout_id: String,
    },
    Suspend {
        checkout_id: String,
    },
    Remove {
        checkout_id: String,
    },
}

impl CheckoutOperation {
    pub fn kind(&self) -> CheckoutOperationKind {
        match self {
            Self::Create { .. } => CheckoutOperationKind::Create,
            Self::Open { .. } => CheckoutOperationKind::Open,
            Self::Prepare { .. } => CheckoutOperationKind::Prepare,
            Self::Suspend { .. } => CheckoutOperationKind::Suspend,
            Self::Remove { .. } => CheckoutOperationKind::Remove,
        }
    }

    pub fn checkout_id(&self) -> Option<&str> {
        match self {
            Self::Create { .. } => None,
            Self::Open { checkout_id }
            | Self::Prepare { checkout_id }
            | Self::Suspend { checkout_id }
            | Self::Remove { checkout_id } => Some(checkout_id),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let values = match self {
            Self::Create { name, branch, base } => {
                vec![name.as_str(), branch.as_str(), base.as_str()]
            }
            _ => vec![self.checkout_id().context("missing checkout identity")?],
        };
        for value in values {
            ensure!(
                !value.trim().is_empty()
                    && value.len() <= 1024
                    && !value.contains(['\0', '\r', '\n']),
                "invalid VCS checkout operation value"
            );
        }
        if let Self::Create { name, .. } = self {
            ensure!(
                name != "." && name != ".." && !name.contains(['/', '\\']),
                "invalid checkout name"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CheckoutState {
    Mounted,
    Unmounted,
    Missing,
    Busy,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CheckoutPreparation {
    Ready,
    Preparing,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDescriptor {
    pub id: String,
    pub name: String,
    pub root_uri: String,
    pub scope_uri: String,
    pub branch: Option<String>,
    pub state: CheckoutState,
    pub owned: bool,
    pub operations: Vec<CheckoutOperationKind>,
    pub unavailable_reason: Option<String>,
    pub preparation: CheckoutPreparation,
    pub preparation_error: Option<String>,
}

impl CheckoutDescriptor {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty() && !self.name.is_empty(),
            "invalid VCS checkout identity"
        );
        let root = local_path(&self.root_uri)?;
        let scope = local_path(&self.scope_uri)?;
        ensure!(
            scope.starts_with(root),
            "checkout scope is outside its root"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutCatalog {
    pub checkouts: Vec<CheckoutDescriptor>,
}

impl CheckoutCatalog {
    pub fn validate(&self) -> Result<()> {
        let mut identifiers = std::collections::HashSet::new();
        for checkout in &self.checkouts {
            checkout.validate()?;
            ensure!(
                identifiers.insert(&checkout.id),
                "duplicate checkout identity"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedCheckoutOperation {
    pub operation_token: String,
    pub summary: String,
    pub target_root_uri: String,
    pub checkout: Option<CheckoutDescriptor>,
}

impl PreparedCheckoutOperation {
    pub fn validate(&self, operation: &CheckoutOperation) -> Result<()> {
        validate_operation_token(&self.operation_token)?;
        ensure!(
            !self.summary.is_empty(),
            "checkout operation summary is empty"
        );
        let root = local_path(&self.target_root_uri)?;
        if let Some(checkout) = &self.checkout {
            checkout.validate()?;
            ensure!(
                local_path(&checkout.root_uri)? == root,
                "prepared checkout root changed"
            );
        }
        if let Some(identifier) = operation.checkout_id() {
            ensure!(
                self.checkout
                    .as_ref()
                    .is_some_and(|checkout| checkout.id == identifier),
                "prepared checkout identity does not match the request"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutOperationResult {
    pub outcome: FileOperationOutcome,
    pub checkout: Option<CheckoutDescriptor>,
}

impl CheckoutOperationResult {
    pub fn validate(
        &self,
        operation: &CheckoutOperation,
        target_root_uri: &str,
        expected: Option<&CheckoutDescriptor>,
    ) -> Result<()> {
        let root = local_path(target_root_uri)?;
        if let Some(checkout) = &self.checkout {
            checkout.validate()?;
            ensure!(
                local_path(&checkout.root_uri)? == root,
                "checkout result belongs to another root"
            );
            if let Some(identifier) = operation.checkout_id() {
                ensure!(
                    checkout.id == identifier,
                    "checkout result belongs to another checkout"
                );
            }
            if let Some(expected) = expected {
                ensure!(
                    checkout.id == expected.id
                        && local_path(&checkout.scope_uri)? == local_path(&expected.scope_uri)?,
                    "checkout identity or project scope changed"
                );
            }
        }
        if self.outcome == FileOperationOutcome::Applied {
            match operation {
                CheckoutOperation::Create { .. }
                | CheckoutOperation::Open { .. }
                | CheckoutOperation::Prepare { .. } => {
                    ensure!(
                        self.checkout.as_ref().is_some_and(|checkout| checkout.owned
                            && matches!(
                                checkout.state,
                                CheckoutState::Mounted | CheckoutState::Busy
                            )),
                        "checkout opening result is not a verified mounted checkout"
                    );
                }
                CheckoutOperation::Suspend { .. } => {
                    ensure!(
                        self.checkout
                            .as_ref()
                            .is_some_and(|checkout| checkout.owned
                                && checkout.state == CheckoutState::Unmounted),
                        "checkout suspension result is not verified"
                    );
                }
                CheckoutOperation::Remove { .. } => {
                    ensure!(
                        self.checkout
                            .as_ref()
                            .is_none_or(|checkout| checkout.state == CheckoutState::Missing
                                && checkout.operations.is_empty()),
                        "checkout removal result still describes an existing checkout"
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OperationProgress {
    pub repository_id: String,
    pub operation_token: String,
    pub phase: String,
    pub message: String,
}

impl OperationProgress {
    pub fn validate(&self) -> Result<()> {
        validate_operation_token(&self.operation_token)?;
        ensure!(
            !self.repository_id.is_empty()
                && !self.phase.is_empty()
                && self.phase.len() <= 128
                && self.message.len() <= 4096,
            "invalid VCS operation progress"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RepositoryOperationKind {
    Commit,
    SwitchBranch,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "operation",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RepositoryOperation {
    Commit { message: String },
    SwitchBranch { branch_id: String },
}

impl RepositoryOperation {
    pub fn kind(&self) -> RepositoryOperationKind {
        match self {
            Self::Commit { .. } => RepositoryOperationKind::Commit,
            Self::SwitchBranch { .. } => RepositoryOperationKind::SwitchBranch,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Commit { message } => ensure!(
                !message.trim().is_empty() && message.len() <= 64 * 1024 && !message.contains('\0'),
                "invalid VCS commit message"
            ),
            Self::SwitchBranch { branch_id } => ensure!(
                !branch_id.is_empty() && branch_id.len() <= 4096 && !branch_id.contains('\0'),
                "invalid VCS branch identity"
            ),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryState {
    pub current_branch: Option<Branch>,
    pub branches: Vec<Branch>,
    pub staged_paths: Vec<String>,
}

impl RepositoryState {
    pub fn validate(&self) -> Result<()> {
        let mut branches = std::collections::HashSet::new();
        for branch in self.branches.iter().chain(self.current_branch.iter()) {
            ensure!(
                !branch.id.is_empty() && !branch.name.is_empty(),
                "invalid VCS branch"
            );
        }
        for branch in &self.branches {
            ensure!(branches.insert(&branch.id), "duplicate VCS branch identity");
        }
        validate_paths(&self.staged_paths)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedRepositoryOperation {
    pub operation_token: String,
    pub summary: String,
    pub affected_paths: Vec<String>,
}

impl PreparedRepositoryOperation {
    pub fn validate(&self, operation: &RepositoryOperation) -> Result<()> {
        validate_operation_token(&self.operation_token)?;
        ensure!(!self.summary.is_empty(), "VCS operation summary is empty");
        validate_paths(&self.affected_paths)?;
        if matches!(operation, RepositoryOperation::Commit { .. }) {
            ensure!(
                !self.affected_paths.is_empty(),
                "There are no staged files to commit"
            );
        }
        Ok(())
    }
}

pub fn validate_paths(paths: &[String]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        validate_path(path)?;
        ensure!(seen.insert(path), "duplicate VCS operation path");
    }
    Ok(())
}

pub fn validate_file_batch_paths(paths: &[String]) -> Result<()> {
    ensure!(
        !paths.is_empty() && paths.len() <= MAX_FILE_BATCH_PATHS,
        "VCS file batch must contain between 1 and {MAX_FILE_BATCH_PATHS} paths"
    );
    validate_paths(paths)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreparedFileOperation {
    pub operation_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl PreparedFileOperation {
    pub fn validate(&self) -> Result<()> {
        validate_operation_token(&self.operation_token)?;
        if let Some(paths) = &self.affected_paths {
            validate_paths(paths)?;
        }
        ensure!(
            self.summary
                .as_ref()
                .is_none_or(|summary| !summary.trim().is_empty() && summary.len() <= 64 * 1024),
            "invalid file operation summary"
        );
        Ok(())
    }
}

pub fn validate_operation_token(token: &str) -> Result<()> {
    ensure!(
        !token.is_empty() && token.len() <= MAX_OPERATION_TOKEN_BYTES,
        "invalid VCS operation token length"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileOperationOutcome {
    Applied,
    NotApplied,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileOperationResult {
    pub outcome: FileOperationOutcome,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub server_info: ProviderInfo,
    pub capabilities: ProviderCapabilities,
}

impl InitializeResult {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            SUPPORTED_PROTOCOL_VERSIONS.contains(&self.protocol_version),
            "unsupported VCS protocol version {}",
            self.protocol_version
        );
        ensure!(
            self.protocol_version != 1
                || (self.capabilities.read_only
                    && self.capabilities.file_operations.is_empty()
                    && self.capabilities.batch_file_operations.is_empty()
                    && self.capabilities.repository_operations.is_empty()
                    && self.capabilities.checkout_operations.is_empty()),
            "VCS protocol 1 requires a read-only provider without write operations"
        );
        ensure!(
            !self.capabilities.read_only
                || (self.capabilities.file_operations.is_empty()
                    && self.capabilities.batch_file_operations.is_empty()
                    && self.capabilities.repository_operations.is_empty()
                    && self.capabilities.checkout_operations.is_empty()),
            "read-only VCS providers cannot advertise write operations"
        );
        ensure!(
            self.capabilities
                .batch_file_operations
                .iter()
                .all(|operation| self.capabilities.supports_batch_file_operation(*operation)),
            "VCS file batches require the corresponding stage or unstage file capability"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Baseline {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<ComparisonPurpose>,
    #[serde(default)]
    pub current_content: ContentSource,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum ComparisonPurpose {
    Working,
    Staged,
    Unstaged,
    Branch,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ContentSource {
    #[default]
    Working,
    Snapshot,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ContentSide {
    #[default]
    Base,
    Current,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryDescriptor {
    pub id: String,
    pub root_uri: String,
    pub scope_uri: String,
    pub display_name: String,
    pub baselines: Vec<Baseline>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_config_path: Option<String>,
}

impl RepositoryDescriptor {
    pub fn validate(&self, requested_scope: &str) -> Result<()> {
        ensure!(!self.id.is_empty(), "empty VCS repository id");
        let requested_scope = local_path(requested_scope)?;
        let scope = local_path(&self.scope_uri)?;
        let root = local_path(&self.root_uri)?;
        ensure!(
            scope == requested_scope,
            "VCS provider changed the requested scope"
        );
        ensure!(
            scope.starts_with(root),
            "VCS scope is outside its repository"
        );
        ensure!(
            !self.baselines.is_empty() && self.baselines.len() <= 32,
            "VCS repository has no comparison baseline"
        );
        let mut ids = std::collections::HashSet::new();
        for baseline in &self.baselines {
            ensure!(
                !baseline.id.is_empty() && ids.insert(&baseline.id),
                "invalid or duplicate VCS baseline id"
            );
            ensure!(
                baseline.purpose != Some(ComparisonPurpose::Staged)
                    || baseline.current_content == ContentSource::Snapshot,
                "staged VCS comparison requires immutable current content"
            );
        }
        for purpose in [
            ComparisonPurpose::Working,
            ComparisonPurpose::Staged,
            ComparisonPurpose::Unstaged,
        ] {
            ensure!(
                self.baselines
                    .iter()
                    .filter(|baseline| baseline.purpose == Some(purpose))
                    .count()
                    <= 1,
                "duplicate VCS comparison purpose"
            );
        }
        if let Some(path) = &self.ignore_config_path {
            validate_path(path)?;
        }
        Ok(())
    }
}

pub fn local_path(uri: &str) -> Result<PathBuf> {
    let uri = url::Url::parse(uri).context("invalid VCS file URI")?;
    ensure!(
        uri.query().is_none() && uri.fragment().is_none(),
        "VCS file URI cannot contain a query or fragment"
    );
    let path = uri
        .to_file_path()
        .map_err(|()| anyhow::anyhow!("VCS URI must identify a local file"))?;
    ensure!(path.is_absolute(), "VCS file URI must be absolute");
    Ok(path)
}

/// Protocol paths are relative to scope_uri, with slash-separated components.
pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && !path.contains(['\\', '\0']) && !path.starts_with('/'),
        "invalid VCS relative path"
    );
    ensure!(
        !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.contains(':')),
        "VCS path escapes its scope or uses a non-portable component"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ContentKind {
    File,
    Binary,
    Symlink,
    Submodule,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    pub kind: ContentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_operations: Option<Vec<FileOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_status: Option<ChangeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstaged_status: Option<ChangeStatus>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub id: String,
    pub repository_id: String,
    pub baseline_id: String,
    /// Immutable identity of the selected baseline contents, not necessarily the current revision.
    /// Reusing this identity allows the host to retain already loaded baseline contents.
    pub revision_id: String,
    /// Current committed revision, independent of the selected comparison baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_revision_id: Option<String>,
    /// Immutable identity of snapshot-backed current content, independent of HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_revision_id: Option<String>,
    pub changes: Vec<Change>,
}

impl Snapshot {
    pub fn validate(&self, repository_id: &str, baseline_id: &str) -> Result<()> {
        ensure!(
            !self.id.is_empty() && !self.revision_id.is_empty(),
            "empty VCS snapshot or revision id"
        );
        ensure!(
            self.repository_id == repository_id && self.baseline_id == baseline_id,
            "VCS snapshot belongs to another repository or baseline"
        );
        if let Some(head) = &self.head_revision_id {
            validate_revision_id(head)?;
        }
        validate_revision_id(&self.revision_id)?;
        if let Some(current) = &self.current_revision_id {
            validate_revision_id(current)?;
        }
        let mut paths = std::collections::HashSet::new();
        for change in &self.changes {
            validate_path(&change.path)?;
            ensure!(paths.insert(&change.path), "duplicate path in VCS snapshot");
            if let Some(old_path) = &change.old_path {
                validate_path(old_path)?;
            }
            ensure!(
                change.status != ChangeStatus::Renamed || change.old_path.is_some(),
                "VCS rename has no original path"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Content {
    Absent,
    Text { text: String },
    Binary,
    Symlink { target: String },
}

impl Content {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Text { text } => ensure!(
                text.len() <= MAX_BLAME_CONTENT_BYTES && !text.contains('\0'),
                "VCS text content is too large or binary"
            ),
            Self::Symlink { target } => ensure!(
                target.len() <= 16 * 1024 && !target.contains('\0'),
                "invalid VCS symlink target"
            ),
            _ => {}
        }
        Ok(())
    }
}

pub const MAX_FILE_HISTORY_ENTRIES: u32 = 100;
pub type Commit = BlameCommit;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryEntry {
    pub commit: Commit,
    pub parent_revision_id: Option<String>,
    pub path: Option<String>,
    pub parent_path: Option<String>,
}

impl FileHistoryEntry {
    pub fn validate(&self) -> Result<()> {
        self.commit.validate()?;
        if let Some(revision) = &self.parent_revision_id {
            validate_revision_id(revision)?;
        }
        for path in [&self.path, &self.parent_path].into_iter().flatten() {
            validate_path(path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileHistory {
    pub entries: Vec<FileHistoryEntry>,
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub truncated: bool,
}

pub fn validate_history_request(cursor: Option<&str>, limit: u32) -> Result<()> {
    ensure!(
        (1..=MAX_FILE_HISTORY_ENTRIES).contains(&limit),
        "invalid VCS file history page size"
    );
    if let Some(cursor) = cursor {
        validate_revision_id(cursor)?;
    }
    Ok(())
}

impl FileHistory {
    pub fn validate(&self, cursor: Option<&str>, limit: u32) -> Result<()> {
        validate_history_request(cursor, limit)?;
        ensure!(
            self.entries.len() <= limit as usize,
            "VCS file history exceeds its page limit"
        );
        if let Some(next) = &self.next_cursor {
            validate_revision_id(next)?;
            ensure!(
                Some(next.as_str()) != cursor && !self.entries.is_empty(),
                "VCS file history pagination did not advance"
            );
        }
        let mut revisions = std::collections::HashSet::new();
        for entry in &self.entries {
            entry.validate()?;
            ensure!(
                revisions.insert(&entry.commit.revision_id),
                "duplicate VCS file history revision"
            );
        }
        Ok(())
    }
}

pub const MAX_BLAME_CONTENT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_BLAME_LINES: usize = 100_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Blame {
    pub revision_id: String,
    pub content: String,
    pub entries: Vec<BlameEntry>,
    pub commits: Vec<BlameCommit>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BlameEntry {
    /// Zero-based, half-open rows in the committed content, excluding a trailing empty row.
    pub start_line: u32,
    pub end_line: u32,
    pub commit_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BlameCommit {
    pub revision_id: String,
    pub author: Option<String>,
    pub author_time: Option<i64>,
    pub summary: Option<String>,
    pub message: Option<String>,
    pub commit_url: Option<String>,
}

pub fn validate_revision_id(revision: &str) -> Result<()> {
    ensure!(
        !revision.is_empty() && revision.len() <= 4096 && !revision.chars().any(char::is_control),
        "invalid VCS revision identity"
    );
    Ok(())
}

impl BlameCommit {
    pub fn validate(&self) -> Result<()> {
        validate_revision_id(&self.revision_id)?;
        for (value, limit) in [
            (self.author.as_deref(), 4096),
            (self.summary.as_deref(), 16 * 1024),
            (self.message.as_deref(), 64 * 1024),
        ] {
            ensure!(
                value.is_none_or(|value| value.len() <= limit && !value.contains('\0')),
                "invalid VCS commit metadata"
            );
        }
        if let Some(timestamp) = self.author_time {
            ensure!(
                (-62_135_596_800..=253_402_300_799).contains(&timestamp),
                "invalid VCS commit author timestamp"
            );
        }
        if let Some(commit_url) = &self.commit_url {
            ensure!(commit_url.len() <= 8192, "VCS commit URL is too long");
            let url = url::Url::parse(commit_url).context("invalid VCS commit URL")?;
            ensure!(
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none(),
                "VCS commit URL must use HTTPS without credentials"
            );
        }
        Ok(())
    }
}

impl Blame {
    pub fn validate(&self) -> Result<()> {
        validate_revision_id(&self.revision_id)?;
        ensure!(
            self.content.len() <= MAX_BLAME_CONTENT_BYTES && !self.content.contains('\0'),
            "VCS blame content is too large or binary"
        );
        let line_count = self.content.lines().count();
        ensure!(
            line_count <= MAX_BLAME_LINES
                && self.entries.len() <= line_count
                && self.commits.len() <= self.entries.len(),
            "VCS blame exceeds its line or record limit"
        );
        let mut commits = std::collections::HashSet::new();
        for commit in &self.commits {
            validate_revision_id(&commit.revision_id)?;
            ensure!(
                commits.insert(commit.revision_id.as_str()),
                "duplicate VCS blame commit"
            );
            for (value, limit) in [
                (commit.author.as_deref(), 4096),
                (commit.summary.as_deref(), 16 * 1024),
                (commit.message.as_deref(), 64 * 1024),
            ] {
                ensure!(
                    value.is_none_or(|value| value.len() <= limit && !value.contains('\0')),
                    "invalid VCS blame metadata"
                );
            }
            if let Some(timestamp) = commit.author_time {
                ensure!(
                    (-62_135_596_800..=253_402_300_799).contains(&timestamp),
                    "invalid VCS blame author timestamp"
                );
            }
            if let Some(commit_url) = &commit.commit_url {
                ensure!(commit_url.len() <= 8192, "VCS blame commit URL is too long");
                let url = url::Url::parse(commit_url).context("invalid VCS blame commit URL")?;
                ensure!(
                    url.scheme() == "https"
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none(),
                    "VCS blame commit URL must use HTTPS without credentials"
                );
            }
        }
        let mut next_line = 0;
        let mut referenced = std::collections::HashSet::new();
        for entry in &self.entries {
            ensure!(
                entry.start_line == next_line
                    && entry.end_line > entry.start_line
                    && entry.end_line as usize <= line_count,
                "invalid or incomplete VCS blame line ranges"
            );
            ensure!(
                commits.contains(entry.commit_id.as_str()),
                "unknown VCS blame commit"
            );
            referenced.insert(entry.commit_id.as_str());
            next_line = entry.end_line;
        }
        ensure!(
            next_line as usize == line_count && referenced.len() == commits.len(),
            "incomplete VCS blame attribution"
        );
        Ok(())
    }

    pub fn estimated_size(&self) -> usize {
        self.content.len()
            + self.revision_id.len()
            + self
                .entries
                .iter()
                .map(|entry| std::mem::size_of::<BlameEntry>() + entry.commit_id.len())
                .sum::<usize>()
            + self
                .commits
                .iter()
                .map(|commit| {
                    std::mem::size_of::<BlameCommit>()
                        + commit.revision_id.len()
                        + [
                            &commit.author,
                            &commit.summary,
                            &commit.message,
                            &commit.commit_url,
                        ]
                        .iter()
                        .filter_map(|value| value.as_ref())
                        .map(|value| value.len())
                        .sum::<usize>()
                })
                .sum::<usize>()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, thiserror::Error)]
#[error("{message} (VCS error {code})")]
pub struct ProviderError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_metadata_is_optional_and_ignore_target_is_scoped() {
        let legacy: Baseline =
            serde_json::from_value(serde_json::json!({"id":"submitted","label":"Submitted"}))
                .unwrap();
        assert_eq!(legacy.current_content, ContentSource::Working);
        assert_eq!(legacy.purpose, None);
        let mut descriptor: RepositoryDescriptor = serde_json::from_value(serde_json::json!({
            "id":"repository", "rootUri":"file:///repository", "scopeUri":"file:///repository/project", "displayName":"Fixture",
            "baselines":[{"id":"staged", "label":"Staged", "purpose":"staged", "currentContent":"snapshot"}], "ignoreConfigPath":".fixtureignore"
        })).unwrap();
        descriptor.validate("file:///repository/project").unwrap();
        descriptor.ignore_config_path = Some("../outside".into());
        assert!(descriptor.validate("file:///repository/project").is_err());
        descriptor.ignore_config_path = None;
        descriptor.baselines[0].current_content = ContentSource::Working;
        assert!(descriptor.validate("file:///repository/project").is_err());
        descriptor.baselines[0].current_content = ContentSource::Snapshot;
        let mut duplicate = descriptor.baselines[0].clone();
        duplicate.id = "other-staged".into();
        descriptor.baselines.push(duplicate);
        assert!(descriptor.validate("file:///repository/project").is_err());
    }

    #[test]
    fn history_validates_opaque_revisions_pagination_and_content_limits() {
        let page: FileHistory = serde_json::from_value(serde_json::json!({
            "entries":[{"commit":{"revisionId":"revision:topic/17","summary":"Commit"}, "parentRevisionId":"revision:topic/16", "path":"file.txt", "parentPath":"old.txt"}],
            "nextCursor":"cursor:page/2"
        })).unwrap();
        page.validate(None, 1).unwrap();
        assert!(!page.truncated);
        assert!(page.validate(Some("cursor:page/2"), 1).is_err());
        assert!(page.validate(None, 0).is_err());
        assert!(page.validate(None, 101).is_err());
        let mut invalid = page.clone();
        invalid.entries[0].parent_path = Some("../outside".into());
        assert!(invalid.validate(None, 1).is_err());
        let mut invalid = page.clone();
        invalid.entries[0].commit.revision_id = "bad\nrevision".into();
        assert!(invalid.validate(None, 1).is_err());
        let mut invalid = page.clone();
        invalid.entries.push(page.entries[0].clone());
        assert!(invalid.validate(None, 2).is_err());
        assert!(
            Content::Text {
                text: "x".repeat(MAX_BLAME_CONTENT_BYTES + 1)
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn blame_is_optional_and_independent_of_writes() {
        let mut result: InitializeResult = serde_json::from_value(serde_json::json!({
            "protocolVersion": 1, "serverInfo": {"name": "fixture", "version": "1"},
            "capabilities": {"readOnly": true}
        }))
        .expect("legacy initialization");
        assert!(!result.capabilities.blame);
        result.capabilities.blame = true;
        result
            .validate()
            .expect("read-only protocol 1 can support blame");
        result.protocol_version = 2;
        result
            .validate()
            .expect("read-only protocol 2 can support blame");
        assert!(
            !result
                .capabilities
                .supports_file_operation(FileOperation::Stage)
        );
    }

    fn sample_blame() -> Blame {
        serde_json::from_value(serde_json::json!({
            "revisionId": "revision:42", "content": "one\ntwo\n",
            "entries": [{"startLine": 0, "endLine": 2, "commitId": "change:original"}],
            "commits": [{"revisionId": "change:original", "author": "Author", "authorTime": 1700000000,
                "message": "Commit message", "commitUrl": "https://example.test/revision/original"}]
        })).expect("blame fixture")
    }

    #[test]
    fn blame_validates_bounded_complete_attribution_and_opaque_revisions() {
        let blame = sample_blame();
        blame.validate().expect("opaque revision IDs");
        for invalid in [
            {
                let mut value = blame.clone();
                value.entries[0].start_line = 1;
                value
            },
            {
                let mut value = blame.clone();
                value.entries[0].end_line = 3;
                value
            },
            {
                let mut value = blame.clone();
                value.entries[0].commit_id = "missing".into();
                value
            },
            {
                let mut value = blame.clone();
                value.commits.push(value.commits[0].clone());
                value
            },
            {
                let mut value = blame.clone();
                value.content = "x".repeat(MAX_BLAME_CONTENT_BYTES + 1);
                value
            },
            {
                let mut value = blame.clone();
                value.commits[0].commit_url = Some("javascript:alert(1)".into());
                value
            },
            {
                let mut value = blame.clone();
                value.commits[0].commit_url = Some("https://user:password@example.test".into());
                value
            },
            {
                let mut value = blame.clone();
                value.commits[0].author_time = Some(i64::MAX);
                value
            },
            {
                let mut value = blame;
                value.revision_id = "bad\nrevision".into();
                value
            },
        ] {
            assert!(invalid.validate().is_err());
        }
        let empty = Blame {
            content: String::new(),
            entries: Vec::new(),
            commits: Vec::new(),
            revision_id: "empty:revision".into(),
        };
        empty.validate().expect("empty committed file");
    }

    #[test]
    fn neutral_snapshot_uses_opaque_ids_and_requires_no_index() {
        let snapshot: Snapshot = serde_json::from_str(r#"{"id":"snapshot:9","repositoryId":"example","baselineId":"submitted","revisionId":"r42","changes":[{"path":"src/main.rs","status":"modified","kind":"file"}]}"#).unwrap();
        snapshot.validate("example", "submitted").unwrap();
        assert_eq!(snapshot.revision_id, "r42");
        assert!(snapshot.validate("example", "other").is_err());
    }

    #[test]
    fn changes_cannot_escape_the_requested_scope() {
        for invalid in [
            "",
            "../secret",
            "/tmp/secret",
            "a/../../b",
            "a//b",
            "C:/secret",
            "..\\secret",
            "a\0b",
        ] {
            assert!(validate_path(invalid).is_err(), "{invalid:?}");
        }
        validate_path("source/Пример с пробелами.rs").unwrap();
    }

    #[test]
    fn writes_require_explicit_version_and_capabilities() {
        let missing = r#"{"protocolVersion":1,"serverInfo":{"name":"fixture","version":"1"},"capabilities":{}}"#;
        assert!(serde_json::from_str::<InitializeResult>(missing).is_err());
        let mut result: InitializeResult = serde_json::from_str(
            r#"{"protocolVersion":1,"serverInfo":{"name":"fixture","version":"1"},"capabilities":{"readOnly":true}}"#,
        )
        .expect("legacy read-only initialization");
        result.validate().expect("protocol 1 remains supported");
        assert!(result.capabilities.file_operations.is_empty());
        assert!(
            !result
                .capabilities
                .supports_file_operation(FileOperation::Stage)
        );

        result.capabilities.read_only = false;
        assert!(result.validate().is_err());
        result.protocol_version = 2;
        result
            .validate()
            .expect("protocol 2 without write capabilities");
        assert!(
            !result
                .capabilities
                .supports_file_operation(FileOperation::Stage)
        );
        result.capabilities.file_operations = vec![FileOperation::Stage];
        result.validate().expect("explicit stage capability");
        assert!(
            result
                .capabilities
                .supports_file_operation(FileOperation::Stage)
        );
        assert!(
            !result
                .capabilities
                .supports_file_operation(FileOperation::Unstage)
        );

        result.capabilities.read_only = true;
        assert!(result.validate().is_err());
        assert!(
            !result
                .capabilities
                .supports_file_operation(FileOperation::Stage)
        );
        result.protocol_version = 1;
        assert!(result.validate().is_err());
        result.protocol_version = 3;
        assert!(result.validate().is_err());
    }

    #[test]
    fn file_batches_require_matching_file_capabilities_and_protocol_two() {
        let mut result: InitializeResult = serde_json::from_value(serde_json::json!({
            "protocolVersion": 2, "serverInfo": {"name": "fixture", "version": "1"},
            "capabilities": {"readOnly": false, "fileOperations": ["stage", "unstage"]}
        }))
        .unwrap();
        result.validate().unwrap();
        assert!(
            !result
                .capabilities
                .supports_batch_file_operation(FileOperation::Stage)
        );
        result.capabilities.batch_file_operations = vec![FileOperation::Stage];
        result.validate().unwrap();
        assert!(
            result
                .capabilities
                .supports_batch_file_operation(FileOperation::Stage)
        );
        assert!(
            !result
                .capabilities
                .supports_batch_file_operation(FileOperation::Unstage)
        );
        result.capabilities.batch_file_operations = vec![FileOperation::Discard];
        result
            .capabilities
            .file_operations
            .push(FileOperation::Discard);
        assert!(result.validate().is_err());
        result.capabilities.batch_file_operations = vec![FileOperation::Stage];
        result.capabilities.file_operations.clear();
        assert!(result.validate().is_err());
        result.capabilities.file_operations = vec![FileOperation::Stage];
        result.capabilities.read_only = true;
        assert!(result.validate().is_err());
        result.capabilities.read_only = false;
        result.protocol_version = 1;
        assert!(result.validate().is_err());
    }

    #[test]
    fn operation_tokens_are_opaque_nonempty_and_bounded() {
        validate_operation_token("token:opaque/Пример+=").expect("opaque token");
        validate_operation_token(&"x".repeat(MAX_OPERATION_TOKEN_BYTES)).expect("bounded token");
        assert!(validate_operation_token("").is_err());
        assert!(validate_operation_token(&"x".repeat(MAX_OPERATION_TOKEN_BYTES + 1)).is_err());
        assert!(
            serde_json::from_value::<FileOperationResult>(
                serde_json::json!({"outcome": "unknown"})
            )
            .is_err()
        );
    }
}
