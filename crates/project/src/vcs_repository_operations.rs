use super::*;
use std::path::PathBuf;

impl VcsStore {
    pub fn repository_operations(&self, key: &VcsRepositoryId) -> Vec<RepositoryOperationKind> {
        self.repositories
            .get(key)
            .and_then(|repository| repository.connection.capabilities())
            .filter(|capabilities| !capabilities.read_only)
            .map(|capabilities| capabilities.repository_operations)
            .unwrap_or_default()
    }

    pub fn repository_operation_unresolved(&self, key: &VcsRepositoryId) -> bool {
        self.file_operation_scope(key).is_some_and(|scope| {
            self.operation_journal
                .blocking(&scope, true)
                .map_or(true, |pending| pending.is_some())
        })
    }

    pub fn checkout_in_use(root: &Path, cx: &App) -> bool {
        let stores = GlobalRegistry::global(cx).0.read(cx).stores.clone();
        stores
            .iter()
            .filter_map(WeakEntity::upgrade)
            .any(|store| store.read(cx).worktrees_overlap(root, cx))
    }

    pub(super) fn worktrees_overlap(&self, root: &Path, cx: &App) -> bool {
        self.worktrees.read(cx).worktrees().any(|worktree| {
            let path = worktree.read(cx).abs_path();
            path.starts_with(root) || root.starts_with(path.as_ref())
        })
    }

    pub(super) fn check_buffers(
        &self,
        root: &Path,
        affected_paths: Option<&[PathBuf]>,
        cx: &App,
    ) -> Result<()> {
        let Some(project) = self.project.upgrade() else {
            return Ok(());
        };
        for buffer in project.read(cx).buffer_store().read(cx).buffers() {
            let buffer = buffer.read(cx);
            let Some(path) = buffer.project_path(cx) else {
                continue;
            };
            let Some(worktree) = self
                .worktrees
                .read(cx)
                .worktree_for_id(path.worktree_id, cx)
            else {
                continue;
            };
            let absolute = worktree.read(cx).abs_path().join(path.path.as_unix_str());
            if absolute.starts_with(root)
                && affected_paths.is_none_or(|paths| paths.contains(&absolute))
            {
                ensure!(
                    !buffer.is_dirty() && !buffer.has_conflict(),
                    "Save or revert unsaved edits and resolve disk conflicts before this repository operation: {}",
                    absolute.display()
                );
            }
        }
        Ok(())
    }

    fn check_repository_operation(
        &mut self,
        key: &VcsRepositoryId,
        operation: &RepositoryOperation,
        affected_paths: Option<&[String]>,
        cx: &mut Context<Self>,
    ) -> Result<Arc<ProviderConnection>> {
        operation.validate()?;
        ensure!(
            self.trusted(key.worktree_id, cx),
            "VCS worktree is not trusted"
        );
        ensure!(
            self.repositories
                .keys()
                .filter(|candidate| candidate.worktree_id == key.worktree_id)
                .count()
                == 1,
            "VCS repository ownership is ambiguous"
        );
        ensure!(
            self.repository_for_path(
                &ProjectPath {
                    worktree_id: key.worktree_id,
                    path: Arc::from(RelPath::empty()),
                },
                cx
            )
            .as_ref()
                == Some(key),
            "The opened scope belongs to another source-control provider"
        );
        ensure!(
            self.repository_operations(key).contains(&operation.kind()),
            "VCS provider does not support this repository operation"
        );
        let repository = self
            .repositories
            .get(key)
            .context("VCS repository closed")?;
        ensure!(
            repository.connection.is_alive(),
            "VCS provider is no longer running"
        );
        ensure!(
            repository.view.repository_state_error.is_none(),
            "Repository state is unavailable; refresh before continuing"
        );
        let state = repository
            .view
            .repository_state
            .as_ref()
            .context("Repository state is loading; refresh before continuing")?;
        let root = vcs::local_path(&repository.view.descriptor.root_uri)?;
        let scope = vcs::local_path(&repository.view.descriptor.scope_uri)?;
        let selected = match operation {
            RepositoryOperation::Commit { .. } => {
                let paths = affected_paths.unwrap_or(&state.staged_paths);
                ensure!(!paths.is_empty(), "There are no staged files to commit");
                vcs::validate_paths(paths)?;
                for path in paths {
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
                        "A staged file belongs to another source-control provider"
                    );
                }
                Some(
                    paths
                        .iter()
                        .map(|path| scope.join(path))
                        .collect::<Vec<_>>(),
                )
            }
            RepositoryOperation::SwitchBranch { branch_id } => {
                ensure!(
                    state.branches.iter().any(|branch| &branch.id == branch_id),
                    "The selected branch is no longer available; refresh"
                );
                None
            }
        };
        self.check_buffers(&root, selected.as_deref(), cx)?;
        let stores = GlobalRegistry::global(cx).0.read(cx).stores.clone();
        for other in stores.iter().filter_map(WeakEntity::upgrade) {
            if other.entity_id() == cx.entity().entity_id() {
                continue;
            }
            let other = other.read(cx);
            if matches!(operation, RepositoryOperation::SwitchBranch { .. }) {
                ensure!(
                    !other.worktrees_overlap(&root, cx),
                    "Close other projects using this checkout before switching branches"
                );
            }
            other.check_buffers(&root, selected.as_deref(), cx)?;
        }
        Ok(repository.connection.clone())
    }

    pub fn prepare_repository_operation(
        &mut self,
        key: VcsRepositoryId,
        operation: RepositoryOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsPreparedRepositoryOperation>> {
        let checked = (|| {
            self.check_mutation_available(&key, None, cx)?;
            let connection = self.check_repository_operation(&key, &operation, None, cx)?;
            let scope = self
                .file_operation_scope(&key)
                .context("VCS repository closed")?;
            Ok((connection, scope))
        })();
        let (connection, scope) = match checked {
            Ok(checked) => checked,
            Err(error) => return Task::ready(Err(error)),
        };
        let journal = self.operation_journal.clone();
        cx.spawn(async move |this, cx| {
            journal.load().await?;
            if let Some((blocked, _)) = journal.blocking(&scope, true)? {
                bail!("An operation in {} has an unknown outcome; reopen that scope and refresh to reconcile", blocked.scope_uri);
            }
            let prepared = connection.prepare_repository_operation(
                key.repository_id.clone(), operation.clone(),
            ).await?;
            this.update(cx, |this, cx| {
                let current = this.check_repository_operation(&key, &operation, Some(&prepared.affected_paths), cx)?;
                ensure!(Arc::ptr_eq(&current, &connection), "VCS provider changed while preparing the operation");
                Ok::<_, anyhow::Error>(())
            })??;
            Ok(VcsPreparedRepositoryOperation { key, scope, operation, prepared, connection })
        })
    }

    fn check_prepared_repository_operation(
        &mut self,
        prepared: &VcsPreparedRepositoryOperation,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let current = self.check_repository_operation(
            &prepared.key,
            &prepared.operation,
            Some(&prepared.prepared.affected_paths),
            cx,
        )?;
        ensure!(
            Arc::ptr_eq(&current, &prepared.connection)
                && self.file_operation_scope(&prepared.key).as_ref() == Some(&prepared.scope),
            "VCS provider or scope changed after preparing the operation"
        );
        Ok(())
    }

    pub(super) fn check_repository_result_buffers(
        &self,
        key: &VcsRepositoryId,
        operation: &RepositoryOperation,
        cx: &App,
    ) -> Result<()> {
        if matches!(operation, RepositoryOperation::SwitchBranch { .. })
            && let Some(repository) = self.repositories.get(key)
        {
            let root = vcs::local_path(&repository.view.descriptor.root_uri)?;
            self.check_buffers(&root, None, cx).context(
                "The branch changed on disk; edits made in buffers during the operation were preserved",
            )?;
        }
        Ok(())
    }

    pub fn apply_prepared_repository_operation(
        &mut self,
        prepared: VcsPreparedRepositoryOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let checked = (|| {
            self.check_mutation_available(&prepared.key, None, cx)?;
            ensure!(
                !self.repository_operation_unresolved(&prepared.key),
                "A previous checkout operation has an unknown outcome; refresh to reconcile it"
            );
            self.check_prepared_repository_operation(&prepared, cx)
        })();
        if let Err(error) = checked {
            return Task::ready(Err(error));
        }
        self.file_operation_running = true;
        self.file_operation_errors.remove(&prepared.key);
        self.notify(cx);
        let journal = self.operation_journal.clone();
        let (sender, receiver) = futures::channel::oneshot::channel();
        cx.spawn(async move |this, cx| {
            let pending = PendingOperation {
                token: prepared.prepared.operation_token.clone(),
                request: OperationRequest::Repository {
                    operation: prepared.operation.clone(),
                    affected_paths: prepared.prepared.affected_paths.clone(),
                },
            };
            let result: Result<()> = async {
                let _claim = journal.record(prepared.scope.clone(), pending.clone()).await?;
                let checked = this.update(cx, |this, cx| {
                    this.pending_file_operations.insert(prepared.scope.clone(), pending.clone());
                    this.check_prepared_repository_operation(&prepared, cx)
                }).and_then(|result| result);
                if let Err(error) = checked {
                    journal.clear(&prepared.scope, &pending.token).await?;
                    return Err(error);
                }
                let outcome = match prepared.connection.apply_repository_operation(
                    prepared.key.repository_id.clone(), prepared.operation.clone(), pending.token.clone(),
                ).await {
                    Ok(result) => result.outcome,
                    Err(error) => prepared.connection.reconcile_repository_operation(
                        prepared.key.repository_id.clone(), prepared.operation.clone(), pending.token.clone(),
                    ).await.with_context(|| format!(
                        "Repository operation outcome is unknown ({error:#}); refresh to reconcile before retrying"
                    ))?.outcome,
                };
                journal.clear(&prepared.scope, &pending.token).await.context(
                    "Repository operation completed, but its recovery record could not be cleared; refresh to reconcile",
                )?;
                if outcome == FileOperationOutcome::Applied {
                    this.update(cx, |this, cx| {
                        this.check_repository_result_buffers(&prepared.key, &prepared.operation, cx)
                    })??;
                }
                match outcome {
                    FileOperationOutcome::Applied => Ok(()),
                    FileOperationOutcome::NotApplied => bail!("The repository operation was not applied"),
                    FileOperationOutcome::Conflict => bail!("The checkout changed; review its current state before continuing"),
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
                log::debug!("VCS repository operation waiter closed after reconciliation");
            }
        }).detach();
        cx.spawn(async move |_, _| {
            receiver
                .await
                .context("VCS repository operation was interrupted")?
        })
    }
}
