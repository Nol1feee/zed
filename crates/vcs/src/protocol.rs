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
}

impl ProviderCapabilities {
    pub fn supports_file_operation(&self, operation: FileOperation) -> bool {
        !self.read_only && self.file_operations.contains(&operation)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileOperation {
    Stage,
    Unstage,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreparedFileOperation {
    pub operation_token: String,
}

impl PreparedFileOperation {
    pub fn validate(&self) -> Result<()> {
        validate_operation_token(&self.operation_token)
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
                || (self.capabilities.read_only && self.capabilities.file_operations.is_empty()),
            "VCS protocol 1 requires a read-only provider without file operations"
        );
        ensure!(
            !self.capabilities.read_only || self.capabilities.file_operations.is_empty(),
            "read-only VCS providers cannot advertise file operations"
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
