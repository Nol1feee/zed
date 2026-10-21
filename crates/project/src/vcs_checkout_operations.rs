use super::*;

impl VcsStore {
    pub fn checkout_catalog_available(&self, key: &VcsRepositoryId) -> bool {
        self.repositories
            .get(key)
            .and_then(|repository| repository.connection.capabilities())
            .is_some_and(|capabilities| capabilities.checkout_catalog)
    }

    pub fn checkout_operations(&self, key: &VcsRepositoryId) -> Vec<CheckoutOperationKind> {
        self.repositories
            .get(key)
            .and_then(|repository| repository.connection.capabilities())
            .filter(|capabilities| !capabilities.read_only)
            .map(|capabilities| capabilities.checkout_operations)
            .unwrap_or_default()
    }

    pub fn operation_progress(&self, key: &VcsRepositoryId) -> Option<OperationProgress> {
        self.operation_progress.get(key).cloned()
    }

    pub(super) fn receive_operation_progress(
        &mut self,
        key: &VcsRepositoryId,
        progress: OperationProgress,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self.file_operation_scope(key) else {
            return;
        };
        if self
            .operation_journal
            .pending(&scope)
            .ok()
            .flatten()
            .is_some_and(|pending| pending.token == progress.operation_token)
        {
            self.operation_progress.insert(key.clone(), progress);
            self.notify(cx);
        }
    }

    pub fn refresh_checkouts(&mut self, key: &VcsRepositoryId, cx: &mut Context<Self>) {
        self.reconcile_file_operation(key, cx);
        self.refresh_checkout_catalog(key, cx);
    }

    pub(super) fn refresh_checkout_catalog(
        &mut self,
        key: &VcsRepositoryId,
        cx: &mut Context<Self>,
    ) {
        if !self.checkout_catalog_available(key) {
            return;
        }
        let Some(repository) = self.repositories.get_mut(key) else {
            return;
        };
        if !repository.connection.is_alive() || repository.view.checkout_loading {
            return;
        }
        repository.checkout_generation += 1;
        let generation = repository.checkout_generation;
        let connection = repository.connection.clone();
        let key = key.clone();
        repository.view.checkout_loading = true;
        repository.checkout_task = Some(cx.spawn(async move |this, cx| {
            let result = connection.list_checkouts(key.repository_id.clone()).await;
            this.update(cx, |this, cx| {
                let Some(repository) = this.repositories.get_mut(&key) else {
                    return;
                };
                if repository.checkout_generation != generation
                    || !Arc::ptr_eq(&repository.connection, &connection)
                {
                    return;
                }
                repository.view.checkout_loading = false;
                match result {
                    Ok(catalog) => {
                        repository.view.checkouts = Some(Arc::new(catalog));
                        repository.view.checkout_error = None;
                    }
                    Err(error) => {
                        repository.view.checkout_error = Some(format!("{error:#}").into());
                    }
                }
                this.notify(cx);
            })
            .log_err();
        }));
        self.notify(cx);
    }

    pub(super) fn invalidate_checkout_catalog(&mut self, key: &VcsRepositoryId) {
        if let Some(repository) = self.repositories.get_mut(key) {
            repository.checkout_generation += 1;
            repository.checkout_task = None;
            repository.view.checkout_loading = false;
        }
    }

    fn check_checkout_target_idle(&self, root: &Path, cx: &Context<Self>) -> Result<()> {
        ensure!(
            !self.worktrees_overlap(root, cx),
            "Close the project using this checkout before continuing"
        );
        let stores = GlobalRegistry::global(cx).0.read(cx).stores.clone();
        for other in stores.iter().filter_map(WeakEntity::upgrade) {
            if other.entity_id() == cx.entity().entity_id() {
                continue;
            }
            ensure!(
                !other.read(cx).worktrees_overlap(root, cx),
                "Close other projects using this checkout before continuing"
            );
        }
        Ok(())
    }

    fn check_checkout_operation(
        &mut self,
        key: &VcsRepositoryId,
        operation: &CheckoutOperation,
        prepared: Option<&vcs::PreparedCheckoutOperation>,
        cx: &mut Context<Self>,
    ) -> Result<Arc<ProviderConnection>> {
        operation.validate()?;
        ensure!(self.project.upgrade().is_some(), "VCS project closed");
        ensure!(
            self.trusted(key.worktree_id, cx),
            "VCS worktree is not trusted"
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
            self.checkout_operations(key).contains(&operation.kind()),
            "VCS provider does not support this checkout operation"
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
            repository.view.checkout_error.is_none(),
            "Checkout catalog is unavailable; refresh before continuing"
        );
        let catalog = repository
            .view
            .checkouts
            .as_ref()
            .context("Checkout catalog is loading; refresh before continuing")?;
        if let Some(identifier) = operation.checkout_id() {
            let checkout = catalog
                .checkouts
                .iter()
                .find(|checkout| checkout.id == identifier)
                .context("Checkout no longer exists; it will not be recreated")?;
            ensure!(
                checkout.owned,
                "{}",
                checkout
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or("Checkout is not owned by this provider")
            );
            ensure!(
                checkout.state != CheckoutState::Missing,
                "Checkout is missing; it will not be recreated"
            );
            ensure!(
                checkout.operations.contains(&operation.kind()),
                "{}",
                checkout
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or("This checkout does not support the selected operation")
            );
            if matches!(
                operation,
                CheckoutOperation::Suspend { .. } | CheckoutOperation::Remove { .. }
            ) {
                ensure!(
                    checkout.state == CheckoutState::Mounted,
                    "Checkout must be mounted and idle"
                );
                self.check_checkout_target_idle(&vcs::local_path(&checkout.root_uri)?, cx)?;
            }
            if let Some(prepared) = prepared {
                prepared.validate(operation)?;
                let expected = prepared
                    .checkout
                    .as_ref()
                    .context("Prepared checkout is missing")?;
                ensure!(
                    checkout.id == expected.id
                        && vcs::local_path(&checkout.root_uri)?
                            == vcs::local_path(&expected.root_uri)?
                        && vcs::local_path(&checkout.scope_uri)?
                            == vcs::local_path(&expected.scope_uri)?,
                    "Checkout identity changed after preparation"
                );
            }
        } else if let Some(prepared) = prepared {
            prepared.validate(operation)?;
            let target = vcs::local_path(&prepared.target_root_uri)?;
            ensure!(
                !catalog
                    .checkouts
                    .iter()
                    .any(|checkout| vcs::local_path(&checkout.root_uri).ok().as_ref()
                        == Some(&target)),
                "Checkout target is already registered; creation will not replace it"
            );
            self.check_checkout_target_idle(&target, cx)?;
        }
        Ok(repository.connection.clone())
    }

    pub fn prepare_checkout_operation(
        &mut self,
        key: VcsRepositoryId,
        operation: CheckoutOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<VcsPreparedCheckoutOperation>> {
        let checked = (|| {
            self.check_mutation_available(&key, None, cx)?;
            let connection = self.check_checkout_operation(&key, &operation, None, cx)?;
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
            ensure!(
                journal.blocking(&scope, true)?.is_none(),
                "A previous operation has an unknown outcome; refresh to reconcile it"
            );
            let prepared = connection
                .prepare_checkout_operation(key.repository_id.clone(), operation.clone())
                .await?;
            this.update(cx, |this, cx| {
                let current =
                    this.check_checkout_operation(&key, &operation, Some(&prepared), cx)?;
                ensure!(
                    Arc::ptr_eq(&current, &connection),
                    "VCS provider changed while preparing checkout"
                );
                Ok::<_, anyhow::Error>(())
            })??;
            let prepared = VcsPreparedCheckoutOperation {
                key,
                scope,
                operation,
                prepared,
                connection,
            };
            ensure!(
                journal
                    .blocking_request(&prepared.scope, &prepared.request())?
                    .is_none(),
                "An operation on the source or target checkout requires reconciliation"
            );
            Ok(prepared)
        })
    }

    fn check_prepared_checkout_operation(
        &mut self,
        prepared: &VcsPreparedCheckoutOperation,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let current = self.check_checkout_operation(
            &prepared.key,
            &prepared.operation,
            Some(&prepared.prepared),
            cx,
        )?;
        ensure!(
            Arc::ptr_eq(&current, &prepared.connection)
                && self.file_operation_scope(&prepared.key).as_ref() == Some(&prepared.scope),
            "VCS provider or source scope changed after preparing checkout"
        );
        Ok(())
    }

    pub fn apply_prepared_checkout_operation(
        &mut self,
        prepared: VcsPreparedCheckoutOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<CheckoutOperationResult>> {
        let checked = (|| {
            self.check_mutation_available(&prepared.key, None, cx)?;
            ensure!(
                self.operation_journal
                    .blocking_request(&prepared.scope, &prepared.request())?
                    .is_none(),
                "A previous operation on the source or target checkout requires reconciliation"
            );
            self.check_prepared_checkout_operation(&prepared, cx)
        })();
        if let Err(error) = checked {
            return Task::ready(Err(error));
        }
        self.file_operation_running = true;
        self.file_operation_errors.remove(&prepared.key);
        self.operation_progress.remove(&prepared.key);
        self.notify(cx);
        let journal = self.operation_journal.clone();
        let (sender, receiver) = futures::channel::oneshot::channel();
        cx.spawn(async move |this, cx| {
            let pending = PendingOperation {
                token: prepared.prepared.operation_token.clone(),
                request: prepared.request(),
            };
            let result: Result<CheckoutOperationResult> = async {
                let _claim = journal.record(prepared.scope.clone(), pending.clone()).await?;
                let checked = this.update(cx, |this, cx| {
                    this.pending_file_operations.insert(prepared.scope.clone(), pending.clone());
                    this.check_prepared_checkout_operation(&prepared, cx)
                }).and_then(|result| result);
                if let Err(error) = checked {
                    journal.clear(&prepared.scope, &pending.token).await?;
                    return Err(error);
                }
                let validate = |result: CheckoutOperationResult| -> Result<CheckoutOperationResult> {
                    result.validate(&prepared.operation, &prepared.prepared.target_root_uri,
                        prepared.prepared.checkout.as_ref())?;
                    Ok(result)
                };
                let result = match prepared.connection.apply_checkout_operation(
                    prepared.key.repository_id.clone(), prepared.operation.clone(), pending.token.clone(),
                ).await.and_then(&validate) {
                    Ok(result) => result,
                    Err(error) => prepared.connection.reconcile_checkout_operation(
                        prepared.key.repository_id.clone(), prepared.operation.clone(), pending.token.clone(),
                    ).await.and_then(&validate).with_context(|| format!(
                        "Checkout operation outcome is unknown ({error:#}); refresh to reconcile before retrying"
                    ))?,
                };
                journal.clear(&prepared.scope, &pending.token).await.context(
                    "Checkout operation completed, but its recovery record could not be cleared; refresh to reconcile",
                )?;
                match result.outcome {
                    FileOperationOutcome::Applied => Ok(result),
                    FileOperationOutcome::NotApplied => bail!("The checkout operation was not applied"),
                    FileOperationOutcome::Conflict => bail!("The checkout changed; review its current state before continuing"),
                }
            }.await;
            this.update(cx, |this, cx| {
                this.file_operation_running = false;
                this.operation_progress.remove(&prepared.key);
                if journal.pending(&prepared.scope).is_ok_and(|pending| pending.is_none()) {
                    this.pending_file_operations.remove(&prepared.scope);
                }
                if let Err(error) = &result {
                    this.file_operation_errors.insert(prepared.key.clone(), format!("{error:#}").into());
                }
                if prepared.connection.is_alive() {
                    this.invalidate_checkout_catalog(&prepared.key);
                    this.refresh_checkout_catalog(&prepared.key, cx);
                    this.refresh_with_reason(&prepared.key, RefreshReason::Explicit, cx);
                }
                this.notify(cx);
            }).log_err();
            if sender.send(result).is_err() {
                log::debug!("VCS checkout operation waiter closed after reconciliation");
            }
        }).detach();
        cx.spawn(async move |_, _| {
            receiver
                .await
                .context("VCS checkout operation was interrupted")?
        })
    }
}
