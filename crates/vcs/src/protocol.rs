use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u32 = 1;
pub const SUPPORTED_PROTOCOL_VERSIONS: &[u32] = &[1, 2];
pub const MAX_OPERATION_TOKEN_BYTES: usize = 4096;
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

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
    pub file_operations: Vec<FileOperation>,
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
                    && self.capabilities.repository_operations.is_empty()
                    && self.capabilities.checkout_operations.is_empty()),
            "VCS protocol 1 requires a read-only provider without write operations"
        );
        ensure!(
            !self.capabilities.read_only
                || (self.capabilities.file_operations.is_empty()
                    && self.capabilities.repository_operations.is_empty()
                    && self.capabilities.checkout_operations.is_empty()),
            "read-only VCS providers cannot advertise write operations"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Baseline {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryDescriptor {
    pub id: String,
    pub root_uri: String,
    pub scope_uri: String,
    pub display_name: String,
    pub baselines: Vec<Baseline>,
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
            !self.baselines.is_empty(),
            "VCS repository has no comparison baseline"
        );
        let mut ids = std::collections::HashSet::new();
        for baseline in &self.baselines {
            ensure!(
                !baseline.id.is_empty() && ids.insert(&baseline.id),
                "invalid or duplicate VCS baseline id"
            );
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
