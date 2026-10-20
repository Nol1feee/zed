use super::{FileOperationScope, PendingFileOperation};
use anyhow::{Context as _, Result, anyhow, ensure};
use fs::Fs;
use gpui::{App, Global};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Deserialize, Serialize)]
struct Entry {
    scope: FileOperationScope,
    pending: PendingFileOperation,
}

#[derive(Deserialize, Serialize)]
struct Document {
    version: u32,
    operations: Vec<Entry>,
}

struct GlobalJournal(Arc<OperationJournal>);
impl Global for GlobalJournal {}

pub(super) struct OperationJournal {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    io: async_lock::Mutex<()>,
    entries: Mutex<Option<Result<Vec<Entry>, String>>>,
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
        let document: Document = serde_json::from_str(&self.fs.load(&self.path).await?)
            .context("VCS recovery journal could not be decoded; file operations are blocked")?;
        ensure!(
            document.version == 1,
            "Unsupported VCS recovery journal version"
        );
        let mut scopes = std::collections::HashSet::new();
        for entry in &document.operations {
            ensure!(scopes.insert(&entry.scope), "Duplicate VCS recovery scope");
            ensure!(
                !entry.scope.extension_id.is_empty()
                    && !entry.scope.provider_id.is_empty()
                    && !entry.scope.repository_id.is_empty(),
                "Invalid VCS recovery identity"
            );
            let root = vcs::local_path(&entry.scope.root_uri)?;
            let scope = vcs::local_path(&entry.scope.scope_uri)?;
            ensure!(scope.starts_with(root), "Invalid VCS recovery scope");
            vcs::validate_path(&entry.pending.path)?;
            vcs::validate_operation_token(&entry.pending.token)?;
        }
        Ok(document.operations)
    }

    fn loaded(&self) -> Result<Vec<Entry>> {
        self.entries
            .lock()
            .as_ref()
            .context("VCS recovery journal has not loaded")?
            .clone()
            .map_err(|error| anyhow!(error))
    }

    pub(super) fn pending(
        &self,
        scope: &FileOperationScope,
    ) -> Result<Option<PendingFileOperation>> {
        Ok(self
            .loaded()?
            .into_iter()
            .find(|entry| &entry.scope == scope)
            .map(|entry| entry.pending))
    }

    pub(super) async fn record(
        &self,
        scope: FileOperationScope,
        pending: PendingFileOperation,
    ) -> Result<()> {
        let _guard = self.io.lock().await;
        let mut entries = self.loaded()?;
        ensure!(
            !entries.iter().any(|entry| entry.scope == scope),
            "The previous operation has an unknown outcome; refresh to reconcile it"
        );
        entries.push(Entry { scope, pending });
        self.save(entries).await
    }

    pub(super) async fn clear(&self, scope: &FileOperationScope, token: &str) -> Result<()> {
        let _guard = self.io.lock().await;
        let mut entries = self.loaded()?;
        let Some(index) = entries.iter().position(|entry| &entry.scope == scope) else {
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
            version: 1,
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
    async fn recovery_survives_new_instance_and_clear_failure_keeps_pending(
        cx: &mut TestAppContext,
    ) {
        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/state/pending.json");
        let journal = OperationJournal::new(fs.clone(), path.clone());
        journal.load().await.expect("load empty journal");
        journal
            .record(scope(), pending())
            .await
            .expect("persist before dispatch");
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
        first_result.expect("first window save");
        second_result.expect("second window save");
        let recovered = OperationJournal::new(fs, path);
        recovered.load().await.expect("reload both writes");
        assert!(recovered.pending(&first).unwrap().is_some());
        assert!(recovered.pending(&second).unwrap().is_some());
        assert!(journal.record(first, pending()).await.is_err());
    }
}
