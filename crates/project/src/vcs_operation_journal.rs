use super::{FileOperationScope, OperationRequest, PendingFileOperation, PendingOperation};
use anyhow::{Context as _, Result, anyhow, ensure};
use fs::Fs;
use gpui::{App, Global};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf, sync::Arc};

#[derive(Clone, Deserialize, Serialize)]
struct Entry {
    scope: FileOperationScope,
    pending: PendingOperation,
}

#[derive(Deserialize, Serialize)]
struct Document {
    version: u32,
    operations: Vec<Entry>,
}

#[derive(Deserialize)]
struct LegacyEntry {
    scope: FileOperationScope,
    pending: PendingFileOperation,
}

#[derive(Deserialize)]
struct LegacyDocument {
    operations: Vec<LegacyEntry>,
}

struct GlobalJournal(Arc<OperationJournal>);
impl Global for GlobalJournal {}

pub(super) struct OperationJournal {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    io: async_lock::Mutex<()>,
    entries: Mutex<Option<Result<Vec<Entry>, String>>>,
    active: Arc<Mutex<HashSet<(FileOperationScope, String)>>>,
}

#[must_use = "keep the claim alive until dispatch and reconciliation have finished"]
pub(super) struct OperationClaim {
    active: Arc<Mutex<HashSet<(FileOperationScope, String)>>>,
    key: (FileOperationScope, String),
}

impl Drop for OperationClaim {
    fn drop(&mut self) {
        self.active.lock().remove(&self.key);
    }
}

impl OperationJournal {
    pub(super) fn global(fs: Arc<dyn Fs>, cx: &mut App) -> Arc<Self> {
        if let Some(journal) = cx.try_global::<GlobalJournal>() {
            return journal.0.clone();
        }
        let journal = Arc::new(Self::new(
            fs,
            paths::data_dir()
                .join("vcs")
                .join("pending-file-operations.json"),
        ));
        cx.set_global(GlobalJournal(journal.clone()));
        journal
    }

    pub(super) fn new(fs: Arc<dyn Fs>, path: PathBuf) -> Self {
        Self {
            fs,
            path,
            io: async_lock::Mutex::new(()),
            entries: Mutex::new(None),
            active: Arc::default(),
        }
    }

    pub(super) async fn load(&self) -> Result<()> {
        let _guard = self.io.lock().await;
        if self.entries.lock().as_ref().is_some_and(Result::is_ok) {
            return Ok(());
        }
        let result = self.read().await;
        *self.entries.lock() = Some(result.map_err(|error| format!("{error:#}")));
        self.loaded().map(|_| ())
    }

    async fn read(&self) -> Result<Vec<Entry>> {
        if self.fs.metadata(&self.path).await?.is_none() {
            return Ok(Vec::new());
        }
        let document: serde_json::Value = serde_json::from_str(&self.fs.load(&self.path).await?)
            .context("VCS recovery journal could not be decoded; file operations are blocked")?;
        let mut entries = match document["version"].as_u64() {
            Some(1) => serde_json::from_value::<LegacyDocument>(document)?
                .operations
                .into_iter()
                .map(|entry| Entry {
                    scope: entry.scope,
                    pending: entry.pending.into(),
                })
                .collect(),
            Some(2) => serde_json::from_value::<Document>(document)?.operations,
            _ => anyhow::bail!("Unsupported VCS recovery journal version"),
        };
        let mut scopes = std::collections::HashSet::new();
        for entry in &mut entries {
            entry.scope.normalize()?;
            ensure!(
                scopes.insert(entry.scope.clone()),
                "Duplicate VCS recovery scope"
            );
            ensure!(
                !entry.scope.extension_id.is_empty()
                    && !entry.scope.provider_id.is_empty()
                    && !entry.scope.repository_id.is_empty(),
                "Invalid VCS recovery identity"
            );
            let root = vcs::local_path(&entry.scope.root_uri)?;
            let scope = vcs::local_path(&entry.scope.scope_uri)?;
            ensure!(scope.starts_with(root), "Invalid VCS recovery scope");
            entry.pending.request.normalize()?;
            vcs::validate_operation_token(&entry.pending.token)?;
        }
        Ok(entries)
    }

    fn loaded(&self) -> Result<Vec<Entry>> {
        self.entries
            .lock()
            .as_ref()
            .context("VCS recovery journal has not loaded")?
            .clone()
            .map_err(|error| anyhow!(error))
    }

    pub(super) fn pending(&self, scope: &FileOperationScope) -> Result<Option<PendingOperation>> {
        let mut scope = scope.clone();
        scope.normalize()?;
        Ok(self
            .loaded()?
            .into_iter()
            .find(|entry| entry.scope == scope)
            .map(|entry| entry.pending))
    }

    pub(super) fn blocking(
        &self,
        scope: &FileOperationScope,
        root_wide: bool,
    ) -> Result<Option<(FileOperationScope, PendingOperation)>> {
        let mut scope = scope.clone();
        scope.normalize()?;
        Ok(self
            .loaded()?
            .into_iter()
            .find(|entry| {
                entry.scope == scope
                    || (entry.scope.same_provider(&scope)
                        && entry
                            .pending
                            .request
                            .roots(&entry.scope)
                            .contains(&scope.root_uri.as_str())
                        && (root_wide || entry.pending.request.is_root_wide()))
            })
            .map(|entry| (entry.scope, entry.pending)))
    }

    pub(super) fn blocking_request(
        &self,
        scope: &FileOperationScope,
        request: &OperationRequest,
    ) -> Result<Option<(FileOperationScope, PendingOperation)>> {
        let mut scope = scope.clone();
        scope.normalize()?;
        let mut request = request.clone();
        request.normalize()?;
        Ok(self
            .loaded()?
            .into_iter()
            .find(|entry| Self::conflicts(&scope, &request, &entry.scope, &entry.pending.request))
            .map(|entry| (entry.scope, entry.pending)))
    }

    fn conflicts(
        left_scope: &FileOperationScope,
        left: &OperationRequest,
        right_scope: &FileOperationScope,
        right: &OperationRequest,
    ) -> bool {
        if left_scope == right_scope {
            return true;
        }
        if !left_scope.same_provider(right_scope) {
            return false;
        }
        if left.is_root_wide() || right.is_root_wide() {
            return left
                .roots(left_scope)
                .iter()
                .any(|root| right.roots(right_scope).contains(root));
        }
        if left_scope.root_uri != right_scope.root_uri {
            return false;
        }
        if let (
            OperationRequest::File {
                affected_paths: left_paths,
                ..
            },
            OperationRequest::File {
                affected_paths: right_paths,
                ..
            },
        ) = (left, right)
        {
            let (Ok(left_directory), Ok(right_directory)) = (
                vcs::local_path(&left_scope.scope_uri),
                vcs::local_path(&right_scope.scope_uri),
            ) else {
                return true;
            };
            return left_paths.iter().any(|left_path| {
                right_paths.iter().any(|right_path| {
                    left_directory.join(left_path) == right_directory.join(right_path)
                })
            });
        }
        false
    }

    pub(super) async fn record(
        &self,
        mut scope: FileOperationScope,
        pending: impl Into<PendingOperation>,
    ) -> Result<OperationClaim> {
        let _guard = self.io.lock().await;
        scope.normalize()?;
        let mut entries = self.loaded()?;
        let mut pending = pending.into();
        pending.request.normalize()?;
        ensure!(
            !entries.iter().any(|entry| Self::conflicts(
                &scope,
                &pending.request,
                &entry.scope,
                &entry.pending.request
            )),
            "The previous operation has an unknown outcome; refresh to reconcile it"
        );
        let claim = self.claim(&scope, &pending.token)?;
        entries.push(Entry { scope, pending });
        self.save(entries).await?;
        Ok(claim)
    }

    fn claim(&self, scope: &FileOperationScope, token: &str) -> Result<OperationClaim> {
        let key = (scope.clone(), token.to_owned());
        ensure!(
            self.active.lock().insert(key.clone()),
            "This VCS operation still has an active owner"
        );
        Ok(OperationClaim {
            active: self.active.clone(),
            key,
        })
    }

    pub(super) fn claim_recovery(
        &self,
        scope: &FileOperationScope,
        token: &str,
    ) -> Result<Option<OperationClaim>> {
        let mut scope = scope.clone();
        scope.normalize()?;
        let key = (scope.clone(), token.to_owned());
        if !self.active.lock().insert(key.clone()) {
            return Ok(None);
        }
        let claim = OperationClaim {
            active: self.active.clone(),
            key,
        };
        if self
            .pending(&scope)?
            .is_none_or(|pending| pending.token != token)
        {
            return Ok(None);
        }
        Ok(Some(claim))
    }

    pub(super) async fn clear(&self, scope: &FileOperationScope, token: &str) -> Result<()> {
        let _guard = self.io.lock().await;
        let mut scope = scope.clone();
        scope.normalize()?;
        let mut entries = self.loaded()?;
        let Some(index) = entries.iter().position(|entry| entry.scope == scope) else {
            return Ok(());
        };
        ensure!(
            entries[index].pending.token == token,
            "VCS recovery operation changed"
        );
        entries.remove(index);
        self.save(entries).await
    }

    async fn save(&self, entries: Vec<Entry>) -> Result<()> {
        let text = serde_json::to_string(&Document {
            version: 2,
            operations: entries.clone(),
        })?;
        let parent = self
            .path
            .parent()
            .context("VCS journal directory is unavailable")?;
        self.fs.create_dir(parent).await?;
        self.fs
            .atomic_write(self.path.clone(), text)
            .await
            .context("VCS recovery journal could not be saved; file operations are blocked")?;
        *self.entries.lock() = Some(Ok(entries));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs_store::OperationRequest;
    use gpui::TestAppContext;
    use vcs::FileOperation;

    fn scope() -> FileOperationScope {
        FileOperationScope {
            extension_id: "fixture".into(),
            provider_id: "fixture".into(),
            repository_id: "repository".into(),
            root_uri: "file:///checkout".into(),
            scope_uri: "file:///checkout/project".into(),
        }
    }

    fn pending() -> PendingFileOperation {
        PendingFileOperation {
            path: "src/example.txt".into(),
            operation: FileOperation::Discard,
            token: "opaque-operation".into(),
        }
    }

    #[gpui::test]
    async fn active_owner_blocks_recovery_and_drop_preserves_durable_intent(
        cx: &mut TestAppContext,
    ) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/state/active.json");
        let journal = OperationJournal::new(fs.clone(), path.clone());
        journal.load().await.unwrap();
        let owner = journal.record(scope(), pending()).await.unwrap();
        assert!(
            journal
                .claim_recovery(&scope(), "opaque-operation")
                .unwrap()
                .is_none()
        );
        drop(owner);
        assert!(journal.pending(&scope()).unwrap().is_some());
        let recovery = journal
            .claim_recovery(&scope(), "opaque-operation")
            .unwrap()
            .unwrap();
        assert!(
            journal
                .claim_recovery(&scope(), "opaque-operation")
                .unwrap()
                .is_none()
        );
        drop(recovery);
        let restarted = OperationJournal::new(fs, path);
        restarted.load().await.unwrap();
        assert!(
            restarted
                .claim_recovery(&scope(), "opaque-operation")
                .unwrap()
                .is_some()
        );
        assert!(restarted.pending(&scope()).unwrap().is_some());
    }

    #[gpui::test]
    async fn repository_intents_block_other_scopes_and_legacy_uris_are_normalized(
        cx: &mut TestAppContext,
    ) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/state/recovery.json");
        fs.create_dir(PathBuf::from("/state").as_path())
            .await
            .unwrap();
        let mut legacy_scope = scope();
        legacy_scope.root_uri = "file://localhost/checkout/".into();
        legacy_scope.scope_uri = "file://localhost/checkout/project/".into();
        fs.insert_file(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "operations": [{"scope": legacy_scope, "pending": pending()}]
            }))
            .unwrap(),
        )
        .await;
        let journal = OperationJournal::new(fs.clone(), path.clone());
        journal.load().await.unwrap();
        assert!(journal.pending(&scope()).unwrap().is_some());
        let mut other = scope();
        other.scope_uri = "file:///checkout/other-project".into();
        let commit = PendingOperation {
            token: "repository-token".into(),
            request: OperationRequest::Repository {
                operation: vcs::RepositoryOperation::Commit {
                    message: "Staged changes".into(),
                },
                affected_paths: vec!["src/example.txt".into()],
            },
        };
        assert!(journal.blocking(&other, true).unwrap().is_some());
        assert!(journal.record(other.clone(), commit.clone()).await.is_err());
        journal.clear(&scope(), "opaque-operation").await.unwrap();
        drop(journal.record(other.clone(), commit).await.unwrap());
        assert!(journal.blocking(&scope(), false).unwrap().is_some());
        assert!(journal.record(scope(), pending()).await.is_err());
        let recovered = OperationJournal::new(fs, path);
        recovered.load().await.unwrap();
        assert!(matches!(
            recovered.pending(&other).unwrap().unwrap().request,
            OperationRequest::Repository { .. }
        ));
    }

    #[gpui::test]
    async fn recovery_survives_new_instance_and_clear_failure_keeps_pending(
        cx: &mut TestAppContext,
    ) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/state/pending.json");
        let journal = OperationJournal::new(fs.clone(), path.clone());
        journal.load().await.expect("load empty journal");
        drop(
            journal
                .record(scope(), pending())
                .await
                .expect("persist before dispatch"),
        );
        let recovered = OperationJournal::new(fs.clone(), path.clone());
        recovered
            .load()
            .await
            .expect("load another journal instance");
        assert!(recovered.pending(&scope()).expect("read").is_some());
        assert!(recovered.record(scope(), pending()).await.is_err());

        fs.remove_file(&path, fs::RemoveOptions::default())
            .await
            .expect("replace journal target");
        fs.create_dir(&path).await.expect("simulate write failure");
        assert!(recovered.clear(&scope(), "opaque-operation").await.is_err());
        assert!(
            recovered
                .pending(&scope())
                .expect("retain pending")
                .is_some()
        );
        fs.remove_dir(&path, fs::RemoveOptions::default())
            .await
            .expect("restore target");
        recovered
            .clear(&scope(), "opaque-operation")
            .await
            .expect("durable clear");
        let empty = OperationJournal::new(fs, path);
        empty.load().await.expect("reload settled journal");
        assert!(empty.pending(&scope()).expect("read").is_none());
    }

    #[gpui::test]
    async fn invalid_journal_never_becomes_an_empty_journal(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/state/pending.json");
        fs.create_dir(path.parent().unwrap())
            .await
            .expect("journal directory");
        fs.insert_file(&path, b"{broken".to_vec()).await;
        let journal = OperationJournal::new(fs.clone(), path.clone());
        assert!(journal.load().await.is_err());
        assert!(journal.record(scope(), pending()).await.is_err());
        assert_eq!(fs.load(&path).await.expect("unchanged file"), "{broken");
    }

    #[gpui::test]
    async fn shared_journal_serializes_updates_from_multiple_windows(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/state/pending.json");
        let journal = OperationJournal::new(fs.clone(), path.clone());
        journal.load().await.expect("load");
        let first = scope();
        let mut second = scope();
        second.scope_uri = "file:///checkout/another-project".into();
        let (first_result, second_result) = futures::join!(
            journal.record(first.clone(), pending()),
            journal.record(second.clone(), pending()),
        );
        drop(first_result.expect("first window save"));
        drop(second_result.expect("second window save"));
        let recovered = OperationJournal::new(fs, path);
        recovered.load().await.expect("reload both writes");
        assert!(recovered.pending(&first).unwrap().is_some());
        assert!(recovered.pending(&second).unwrap().is_some());
        assert!(journal.record(first, pending()).await.is_err());
    }
}
