use super::*;
use extension::*;
use gpui::{EntityId, TestAppContext, UpdateGlobal};
use language::{LanguageName, LanguageRegistry};
use lsp::LanguageServerName;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
use task::{SpawnInTerminal, ZedDebugConfig};

struct FixtureExtension {
    manifest: Arc<ExtensionManifest>,
    directory: Arc<Path>,
    scenario: String,
    callbacks: AtomicUsize,
}

impl FixtureExtension {
    fn new(id: &str, directory: &Path, scenario: &str) -> Arc<Self> {
        Arc::new(Self {
            manifest: Arc::new(
                serde_json::from_value(json!({
                    "id": id, "name": id, "version": "0.1.0", "schema_version": 1,
                    "vcs_providers": {"fixture": {"root_markers": [".fixture-vcs"]}}
                }))
                .unwrap(),
            ),
            directory: Arc::from(directory),
            scenario: scenario.into(),
            callbacks: AtomicUsize::new(0),
        })
    }

    fn register(self: &Arc<Self>, cx: &mut TestAppContext) {
        cx.update(|cx| {
            ExtensionHostProxy::global(cx).register_vcs_provider(self.clone(), "fixture".into(), cx)
        });
    }
}

macro_rules! fixture_extension {
    ($(async fn $method:ident($($parameters:tt)*) -> $result:ty;)*) => {
        #[async_trait::async_trait]
        impl Extension for FixtureExtension {
            fn manifest(&self) -> Arc<ExtensionManifest> { self.manifest.clone() }
            fn work_dir(&self) -> Arc<Path> { self.directory.clone() }
            async fn vcs_provider_command(&self, _: Arc<str>, _: Arc<dyn WorktreeDelegate>) -> Result<Command> {
                self.callbacks.fetch_add(1, Ordering::SeqCst);
                let python = std::env::var_os("VCS_FIXTURE_PYTHON").map(PathBuf::from).unwrap_or_else(|| which::which("python3").expect("public fixture Python"));
                Ok(Command {
                    command: python,
                    args: vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../vcs/tests/fixture_provider.py").to_string_lossy().into_owned(),
                        "--scenario".into(), self.scenario.clone(), "--events".into(), self.directory.join(format!("{}.jsonl", self.manifest.id)).to_string_lossy().into_owned()],
                    env: Vec::new(),
                })
            }
            $(async fn $method($($parameters)*) -> $result { bail!("fixture implements only VCS") })*
        }
    };
}

fixture_extension! {
    async fn language_server_command(&self, _: LanguageServerName, _: LanguageName, _: Arc<dyn WorktreeDelegate>, _: EntityId) -> Result<Command>;
    async fn language_server_initialization_options(&self, _: LanguageServerName, _: LanguageName, _: Arc<dyn WorktreeDelegate>, _: EntityId) -> Result<Option<String>>;
    async fn language_server_workspace_configuration(&self, _: LanguageServerName, _: Arc<dyn WorktreeDelegate>, _: EntityId) -> Result<Option<String>>;
    async fn language_server_initialization_options_schema(&self, _: LanguageServerName, _: Arc<dyn WorktreeDelegate>, _: EntityId) -> Result<Option<String>>;
    async fn language_server_workspace_configuration_schema(&self, _: LanguageServerName, _: Arc<dyn WorktreeDelegate>, _: EntityId) -> Result<Option<String>>;
    async fn language_server_additional_initialization_options(&self, _: LanguageServerName, _: LanguageServerName, _: Arc<dyn WorktreeDelegate>, _: EntityId) -> Result<Option<String>>;
    async fn language_server_additional_workspace_configuration(&self, _: LanguageServerName, _: LanguageServerName, _: Arc<dyn WorktreeDelegate>, _: EntityId) -> Result<Option<String>>;
    async fn labels_for_completions(&self, _: LanguageServerName, _: Vec<Completion>) -> Result<Vec<Option<CodeLabel>>>;
    async fn labels_for_symbols(&self, _: LanguageServerName, _: Vec<Symbol>) -> Result<Vec<Option<CodeLabel>>>;
    async fn complete_slash_command_argument(&self, _: SlashCommand, _: Vec<String>) -> Result<Vec<SlashCommandArgumentCompletion>>;
    async fn run_slash_command(&self, _: SlashCommand, _: Vec<String>, _: Option<Arc<dyn WorktreeDelegate>>) -> Result<SlashCommandOutput>;
    async fn context_server_command(&self, _: Arc<str>, _: Arc<dyn ProjectDelegate>) -> Result<Command>;
    async fn context_server_configuration(&self, _: Arc<str>, _: Arc<dyn ProjectDelegate>) -> Result<Option<ContextServerConfiguration>>;
    async fn suggest_docs_packages(&self, _: Arc<str>) -> Result<Vec<String>>;
    async fn index_docs(&self, _: Arc<str>, _: Arc<str>, _: Arc<dyn KeyValueStoreDelegate>) -> Result<()>;
    async fn get_dap_binary(&self, _: Arc<str>, _: DebugTaskDefinition, _: Option<PathBuf>, _: Arc<dyn WorktreeDelegate>) -> Result<DebugAdapterBinary>;
    async fn dap_request_kind(&self, _: Arc<str>, _: serde_json::Value) -> Result<StartDebuggingRequestArgumentsRequest>;
    async fn dap_config_to_scenario(&self, _: ZedDebugConfig) -> Result<DebugScenario>;
    async fn dap_locator_create_scenario(&self, _: String, _: BuildTaskTemplate, _: String, _: String) -> Result<Option<DebugScenario>>;
    async fn run_dap_locator(&self, _: String, _: SpawnInTerminal) -> Result<DebugRequest>;
}

struct Fixture {
    directory: tempfile::TempDir,
    project: Entity<Project>,
    store: Entity<VcsStore>,
    fs: Arc<fs::FakeFs>,
    worktree_id: WorktreeId,
}

#[gpui::test]
async fn file_operation_keeps_its_comparison_after_panel_baseline_changes(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "comparison-operation",
        fixture.directory.path(),
        "file-workflow",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    fixture
        .store
        .update(cx, |store, cx| {
            store.load_comparison(repository.key.clone(), "staged".into(), cx)
        })
        .await
        .unwrap();
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation_for_baseline(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Unstage,
                "staged".into(),
                cx,
            )
        })
        .await
        .unwrap();
    fixture
        .store
        .update(cx, |store, cx| {
            store.set_baseline(&repository.key, "unstaged".into(), cx)
        })
        .unwrap();
    ready(&fixture, cx).await;
    fixture.store.update(cx, |store, _| {
        let current = store.repositories.get_mut(&repository.key).unwrap();
        Arc::make_mut(current.view.snapshot.as_mut().unwrap())
            .changes
            .clear();
        assert!(
            store
                .file_operations_for_path(&repository.key, "src/example.txt")
                .is_empty()
        );
        assert!(
            store
                .file_operations_for_baseline(&repository.key, "src/example.txt", "staged")
                .contains(&FileOperation::Unstage)
        );
    });
    fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_file_operation(prepared, cx)
        })
        .await
        .unwrap();
    assert_eq!(ready(&fixture, cx).await.selected_baseline, "unstaged");
    let events =
        std::fs::read_to_string(fixture.directory.path().join("comparison-operation.jsonl"))
            .unwrap();
    let applied = events
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["event"] == "file-operation-applied")
        .unwrap();
    assert_eq!(applied["operation"], "unstage");
    assert_eq!(applied["path"], "src/example.txt");
    assert_eq!(applied["staged"], false);
    assert_eq!(applied["disk"], "working");
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn comparison_sides_preserve_index_and_dirty_working_buffer(cx: &mut TestAppContext) {
    let fixture = fixture(cx, true).await;
    trust(&fixture, cx);
    FixtureExtension::new("workflow", fixture.directory.path(), "file-workflow").register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..buffer.len(), "C\n")], None, cx)
    });
    let staged = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff_for_purpose(
                repository.key.clone(),
                "src/example.txt".into(),
                vcs::ComparisonPurpose::Staged,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(staged.current_content, vcs::ContentSource::Snapshot);
    staged.buffer.read_with(cx, |buffer, _| {
        assert_eq!(buffer.text(), "B\n");
        assert_eq!(buffer.capability(), language::Capability::ReadOnly);
        assert!(
            buffer.file().is_none(),
            "snapshot must never acquire a save target"
        );
    });
    assert_eq!(
        staged
            .diff
            .read_with(cx, |diff, cx| diff.base_text(cx).text()),
        "A\n"
    );
    let unstaged = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff_for_purpose(
                repository.key.clone(),
                "src/example.txt".into(),
                vcs::ComparisonPurpose::Unstaged,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(unstaged.buffer, buffer);
    assert_eq!(
        unstaged
            .diff
            .read_with(cx, |diff, cx| diff.base_text(cx).text()),
        "B\n"
    );
    let working = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff_for_purpose(
                repository.key.clone(),
                "src/example.txt".into(),
                vcs::ComparisonPurpose::Working,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(working.buffer, buffer);
    assert_ne!(working.diff, unstaged.diff);
    assert_eq!(
        working
            .diff
            .read_with(cx, |diff, cx| diff.base_text(cx).text()),
        "A\n"
    );
    std::fs::write(
        fixture.directory.path().join("workflow.jsonl.index"),
        "B2\n",
    )
    .unwrap();
    fixture
        .store
        .update(cx, |store, cx| {
            store.load_comparison(repository.key.clone(), "staged".into(), cx)
        })
        .await
        .unwrap();
    let refreshed = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff_for_purpose(
                repository.key.clone(),
                "src/example.txt".into(),
                vcs::ComparisonPurpose::Staged,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        refreshed.buffer.read_with(cx, |buffer, _| buffer.text()),
        "B2\n"
    );
    assert_eq!(
        staged.buffer.read_with(cx, |buffer, _| buffer.text()),
        "B\n",
        "already opened immutable revision remains pinned"
    );
    buffer.read_with(cx, |buffer, _| {
        assert_eq!(buffer.text(), "C\n");
        assert!(buffer.is_dirty());
    });
    fixture
        .fs
        .remove_file(
            &fixture.directory.path().join("src/example.txt"),
            fs::RemoveOptions::default(),
        )
        .await
        .unwrap();
    let deleted = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff_for_purpose(
                repository.key.clone(),
                "src/example.txt".into(),
                vcs::ComparisonPurpose::Unstaged,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        deleted.buffer, buffer,
        "missing disk file must preserve the open dirty buffer"
    );
    assert_eq!(
        deleted.buffer.read_with(cx, |buffer, _| buffer.text()),
        "C\n"
    );
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.open_diff_for_purpose(
                repository.key.clone(),
                "nested/file.txt".into(),
                vcs::ComparisonPurpose::Staged,
                cx
            ))
            .await
            .is_err()
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn delayed_index_content_is_rejected_after_new_index_snapshot(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "stale-workflow",
        fixture.directory.path(),
        "file-workflow-stale",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let stale = fixture.store.update(cx, |store, cx| {
        store.open_diff_for_purpose(
            repository.key.clone(),
            "src/example.txt".into(),
            vcs::ComparisonPurpose::Staged,
            cx,
        )
    });
    let events = fixture.directory.path().join("stale-workflow.jsonl");
    provider_event(events.clone(), "content-held").await;
    std::fs::write(events.with_extension("jsonl.index"), "B2\n").unwrap();
    fixture
        .store
        .update(cx, |store, cx| {
            store.load_comparison(repository.key.clone(), "staged".into(), cx)
        })
        .await
        .unwrap();
    std::fs::write(events.with_extension("jsonl.release"), "release").unwrap();
    assert!(
        stale
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("comparison changed")
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn file_history_is_bounded_and_revision_diff_has_no_working_file(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("history", fixture.directory.path(), "file-workflow").register(cx);
    let repository = ready(&fixture, cx).await;
    let first = fixture
        .store
        .update(cx, |store, cx| {
            store.file_history(
                repository.key.clone(),
                "src/example.txt".into(),
                None,
                1,
                cx,
            )
        })
        .await
        .unwrap();
    let second = fixture
        .store
        .update(cx, |store, cx| {
            store.file_history(
                repository.key.clone(),
                "src/example.txt".into(),
                first.next_cursor.clone(),
                1,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(second.entries[0].commit.revision_id, "revision:A");
    assert!(second.next_cursor.is_none());
    let diff = fixture
        .store
        .update(cx, |store, cx| {
            store.open_revision_diff(
                repository.key.clone(),
                "src/example.txt".into(),
                first.entries[0].clone(),
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        diff.diff
            .read_with(cx, |diff, cx| diff.base_text(cx).text()),
        "A\n"
    );
    diff.buffer.read_with(cx, |buffer, _| {
        assert_eq!(buffer.text(), "B\n");
        assert!(buffer.file().is_none());
        assert_eq!(buffer.capability(), language::Capability::ReadOnly);
    });
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.file_history(
                repository.key.clone(),
                "src/example.txt".into(),
                None,
                101,
                cx
            ))
            .await
            .is_err()
    );
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.file_history(
                repository.key.clone(),
                "new.txt".into(),
                first.next_cursor,
                1,
                cx
            ))
            .await
            .is_err()
    );
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.file_history(
                repository.key.clone(),
                "../outside".into(),
                None,
                1,
                cx
            ))
            .await
            .is_err()
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn ignore_checks_declared_target_and_dirty_configuration(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    fixture
        .fs
        .insert_file(
            fixture.directory.path().join("new.txt"),
            b"untracked\n".to_vec(),
        )
        .await;
    fixture
        .fs
        .insert_file(
            fixture.directory.path().join(".fixtureignore"),
            b"existing\n".to_vec(),
        )
        .await;
    trust(&fixture, cx);
    FixtureExtension::new("ignore", fixture.directory.path(), "file-workflow").register(cx);
    let repository = ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation(
                repository.key.clone(),
                "new.txt".into(),
                FileOperation::Ignore,
                cx,
            )
        })
        .await
        .unwrap();
    assert_eq!(prepared.affected_paths, ["new.txt", ".fixtureignore"]);
    let config = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join(".fixtureignore"), cx)
        })
        .await
        .unwrap();
    config.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "unsaved\n")], None, cx)
    });
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store
                .apply_prepared_file_operation(prepared, cx))
            .await
            .is_err()
    );
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.prepare_file_operation(
                repository.key.clone(),
                "new.txt".into(),
                FileOperation::Ignore,
                cx
            ))
            .await
            .is_err()
    );
    fixture.store.read_with(cx, |store, _| {
        assert!(
            store
                .validate_file_targets(
                    &repository.key,
                    "new.txt",
                    FileOperation::Ignore,
                    &["new.txt".into(), "neighbor.txt".into()],
                    Some("wrong target"),
                    "working",
                )
                .is_err()
        );
        assert!(
            store
                .validate_file_targets(
                    &repository.key,
                    "src/example.txt",
                    FileOperation::Ignore,
                    &["src/example.txt".into(), ".fixtureignore".into()],
                    Some("tracked file"),
                    "working",
                )
                .is_err()
        );
    });
    let events = std::fs::read_to_string(fixture.directory.path().join("ignore.jsonl")).unwrap();
    assert!(!events.contains("vcs/applyFileOperation"));
    fixture
        .fs
        .insert_symlink(
            fixture.directory.path().join("config-link"),
            "src/example.txt".into(),
        )
        .await;
    let fs: Arc<dyn Fs> = fixture.fs.clone();
    assert!(
        validate_ignore_config(fixture.directory.path(), "config-link", &fs)
            .await
            .is_err()
    );
    stop(&fixture, cx).await;
}

async fn fixture(cx: &mut TestAppContext, nested_git: bool) -> Fixture {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
        settings::SettingsStore::update_global(cx, |settings, cx| {
            settings.update_user_settings(cx, |content| {
                content.session.get_or_insert_default().trust_all_worktrees = Some(false);
            })
        });
        crate::trusted_worktrees::init(Default::default(), cx);
    });
    let directory = tempfile::tempdir().unwrap();
    let fs = fs::FakeFs::new(cx.background_executor.clone());
    let mut tree = json!({".fixture-vcs": "", "src": {"example.txt": "working text\n"}});
    if nested_git {
        tree["nested"] = json!({".git": {}, "file.txt": "Git content"});
    }
    fs.insert_tree(directory.path(), tree).await;
    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    let client = cx.update(|cx| {
        client::Client::new(
            Arc::new(clock::FakeSystemClock::new()),
            http_client::FakeHttpClient::with_404_response(),
            cx,
        )
    });
    let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
    let project = cx.update(|cx| {
        Project::local(
            client,
            node_runtime::NodeRuntime::unavailable(),
            user_store,
            languages,
            fs.clone(),
            Some(HashMap::default()),
            crate::LocalProjectFlags {
                init_worktree_trust: true,
                ..Default::default()
            },
            cx,
        )
    });
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(directory.path(), true, cx)
        })
        .await
        .unwrap();
    worktree
        .read_with(cx, |worktree, _| {
            worktree.as_local().unwrap().scan_complete()
        })
        .await;
    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let store = project.read_with(cx, |project, _| project.vcs_store().unwrap().clone());
    Fixture {
        directory,
        project,
        store,
        fs,
        worktree_id,
    }
}

fn trust(fixture: &Fixture, cx: &mut TestAppContext) {
    trust_worktree(fixture, fixture.worktree_id, cx);
}

fn trust_worktree(fixture: &Fixture, worktree_id: WorktreeId, cx: &mut TestAppContext) {
    let worktree_store = fixture
        .project
        .read_with(cx, |project, _| project.worktree_store());
    cx.update(|cx| {
        TrustedWorktrees::try_get_global(cx)
            .unwrap()
            .update(cx, |trust, cx| {
                trust.trust(
                    &worktree_store,
                    collections::HashSet::from_iter([
                        crate::trusted_worktrees::PathTrust::Worktree(worktree_id),
                    ]),
                    cx,
                )
            });
    });
}

async fn open_worktree(fixture: &Fixture, path: &Path, cx: &mut TestAppContext) -> WorktreeId {
    let (worktree, _) = fixture
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path, true, cx)
        })
        .await
        .expect("open additional worktree");
    worktree
        .read_with(cx, |worktree, _| {
            worktree.as_local().unwrap().scan_complete()
        })
        .await;
    worktree.read_with(cx, |worktree, _| worktree.id())
}

async fn ready(fixture: &Fixture, cx: &mut TestAppContext) -> VcsRepositorySnapshot {
    cx.condition(&fixture.store, |store, _| {
        store
            .repositories()
            .first()
            .is_some_and(|repository| repository.snapshot.is_some() && !repository.loading)
    })
    .await;
    fixture
        .store
        .read_with(cx, |store, _| store.repositories().remove(0))
}

async fn stop(fixture: &Fixture, cx: &mut TestAppContext) {
    let connections: Vec<_> = fixture.store.read_with(cx, |store, _| {
        store
            .sessions
            .values()
            .filter_map(|session| session.connection.clone())
            .collect()
    });
    for connection in connections {
        connection.shutdown().await.unwrap();
    }
    fixture.store.update(cx, |store, cx| store.stop_all(cx));
}

async fn provider_event(path: PathBuf, event: &'static str) {
    // The child writes this handshake outside GPUI's simulated clock.
    smol::unblock(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|value| value["event"] == event)
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture handshake {event} timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    })
    .await;
}

async fn wait_diff_base(diff: &Entity<BufferDiff>, expected: &str, cx: &TestAppContext) {
    let expected = expected.to_owned();
    let changed = diff.condition::<buffer_diff::BufferDiffEvent>(cx, move |diff, cx| {
        diff.base_text(cx).text() == expected
    });
    let timeout = cx.background_executor.timer(Duration::from_secs(3));
    futures::pin_mut!(changed, timeout);
    match futures::future::select(changed, timeout).await {
        futures::future::Either::Left(_) => {}
        futures::future::Either::Right(_) => panic!(
            "diff baseline did not update: {:?}",
            diff.read_with(cx, |diff, cx| diff.base_text(cx).text())
        ),
    }
}

#[gpui::test]
async fn trust_precedes_callback_and_spawn_and_read_only_rejects_git_init(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    let extension = FixtureExtension::new("trust-fixture", fixture.directory.path(), "normal");
    extension.register(cx);
    let immediate_initialization = fixture.project.read_with(cx, |project, cx| {
        project.git_init(Arc::from(fixture.directory.path()), "main".into(), cx)
    });
    assert!(immediate_initialization.await.is_err());
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 0);
    assert!(
        !fixture
            .directory
            .path()
            .join("trust-fixture.jsonl")
            .exists()
    );
    trust(&fixture, cx);
    let repository = ready(&fixture, cx).await;
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 1);
    assert_eq!(repository.snapshot.unwrap().revision_id, "r42");
    let mutation = fixture.store.update(cx, |store, cx| {
        store.run_file_operation(
            repository.key.clone(),
            "src/example.txt".into(),
            FileOperation::Stage,
            cx,
        )
    });
    assert!(mutation.await.is_err());
    let initialize_repository = fixture.project.read_with(cx, |project, cx| {
        project.git_init(Arc::from(fixture.directory.path()), "main".into(), cx)
    });
    assert!(initialize_repository.await.is_err());
    fixture.store.update(cx, |store, cx| store.refresh_all(cx));
    let during_refresh = fixture.project.read_with(cx, |project, cx| {
        project.git_init(Arc::from(fixture.directory.path()), "main".into(), cx)
    });
    assert!(during_refresh.await.is_err());
    assert!(
        fixture
            .fs
            .metadata(&fixture.directory.path().join(".git"))
            .await
            .unwrap()
            .is_none()
    );
    fixture.fs.insert_tree("/ordinary", json!({})).await;
    let ordinary = fixture.project.read_with(cx, |project, cx| {
        project.git_init(Arc::from(Path::new("/ordinary")), "main".into(), cx)
    });
    ordinary.await.unwrap();
    assert!(
        fixture
            .fs
            .metadata(Path::new("/ordinary/.git"))
            .await
            .unwrap()
            .is_some()
    );
    stop(&fixture, cx).await;
    let events =
        std::fs::read_to_string(fixture.directory.path().join("trust-fixture.jsonl")).unwrap();
    assert!(events.contains("shutdown-received"));
    assert!(!events.contains("vcs/commit"));
    assert!(!events.contains("vcs/prepareFileOperation"));
}

#[gpui::test]
async fn excluded_git_metadata_never_falls_back_when_provider_is_unavailable(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, true).await;
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |settings, cx| {
            settings.update_user_settings(cx, |content| {
                content
                    .git
                    .get_or_insert_default()
                    .excluded_repository_markers = Some(vec![".compatibility-vcs".into()]);
            });
        });
    });
    fixture
        .fs
        .insert_tree(
            fixture.directory.path().join(".git"),
            json!({".compatibility-vcs": ""}),
        )
        .await;
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let external_path = ProjectPath {
        worktree_id: fixture.worktree_id,
        path: RelPath::from_unix_str("src/example.txt").unwrap().into(),
    };
    fixture.store.read_with(cx, |store, cx| {
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&external_path, cx)
                .is_none()
        );
        let nested_path = ProjectPath {
            worktree_id: fixture.worktree_id,
            path: RelPath::from_unix_str("nested/file.txt").unwrap().into(),
        };
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&nested_path, cx)
                .is_some()
        );
    });
    let initialization = fixture.project.read_with(cx, |project, cx| {
        project.git_init(fixture.directory.path().into(), "main".into(), cx)
    });
    assert!(
        initialization
            .await
            .unwrap_err()
            .to_string()
            .contains("excluded_repository_markers")
    );

    let extension = FixtureExtension::new("excluded-git", fixture.directory.path(), "normal");
    extension.register(cx);
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 0);
    trust(&fixture, cx);
    let repository = ready(&fixture, cx).await;
    fixture.store.read_with(cx, |store, cx| {
        assert_eq!(
            store.repository_for_path(&external_path, cx),
            Some(repository.key)
        );
    });
    stop(&fixture, cx).await;
    cx.update(|cx| {
        ExtensionHostProxy::global(cx).unregister_vcs_provider(
            "excluded-git".into(),
            "fixture".into(),
            cx,
        );
    });
    let initialization = fixture.project.read_with(cx, |project, cx| {
        project.git_init(fixture.directory.path().into(), "main".into(), cx)
    });
    assert!(initialization.await.is_err());

    FixtureExtension::new("failed-provider", fixture.directory.path(), "early-exit").register(cx);
    cx.condition(&fixture.store, |store, _| !store.errors().is_empty())
        .await;
    fixture.store.read_with(cx, |store, cx| {
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&external_path, cx)
                .is_none()
        );
        assert!(store.repository_for_path(&external_path, cx).is_none());
    });
    let initialization = fixture.project.read_with(cx, |project, cx| {
        project.git_init(fixture.directory.path().into(), "main".into(), cx)
    });
    assert!(initialization.await.is_err());
}

#[gpui::test]
async fn outer_git_cannot_claim_an_excluded_repository_opened_in_multiple_scopes(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    let outer_directory = tempfile::tempdir().unwrap();
    let project_directory = outer_directory.path().join("external/project");
    std::fs::create_dir_all(&project_directory).unwrap();
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |settings, cx| {
            settings.update_user_settings(cx, |content| {
                content
                    .git
                    .get_or_insert_default()
                    .excluded_repository_markers = Some(vec![".compatibility-vcs".into()]);
            });
        })
    });
    fixture.fs.insert_tree(outer_directory.path(), json!({
        ".git": {},
        "external": {
            ".fixture-vcs": "", ".git": {".compatibility-vcs": ""},
            "project": {"src": {"example.txt": "external"}, "nested": {".git": {}, "file.txt": "Git"}}
        }
    })).await;
    let outer_id = open_worktree(&fixture, outer_directory.path(), cx).await;
    let external_worktree = fixture
        .project
        .update(cx, |project, cx| {
            project.create_worktree(&project_directory, true, cx)
        })
        .await
        .unwrap();
    external_worktree
        .read_with(cx, |worktree, _| {
            worktree.as_local().unwrap().scan_complete()
        })
        .await;
    let external_id = external_worktree.read_with(cx, |worktree, _| worktree.id());
    assert_ne!(outer_id, external_id);
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let external_path = ProjectPath {
        worktree_id: external_id,
        path: RelPath::from_unix_str("src/example.txt").unwrap().into(),
    };
    let outer_path = ProjectPath {
        worktree_id: outer_id,
        path: RelPath::from_unix_str("external/project/src/example.txt")
            .unwrap()
            .into(),
    };
    let nested_path = ProjectPath {
        worktree_id: external_id,
        path: RelPath::from_unix_str("nested/file.txt").unwrap().into(),
    };
    fixture.store.read_with(cx, |store, cx| {
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&outer_path, cx)
                .is_none()
        );
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&external_path, cx)
                .is_none()
        );
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&nested_path, cx)
                .is_some()
        );
        let root = ProjectPath {
            worktree_id: outer_id,
            path: RelPath::empty_arc(),
        };
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&root, cx)
                .is_some()
        );
    });
    trust_worktree(&fixture, external_id, cx);
    FixtureExtension::new("outer-boundary", fixture.directory.path(), "normal").register(cx);
    let repository = ready(&fixture, cx).await;
    fixture.store.read_with(cx, |store, cx| {
        assert_eq!(
            store.repository_for_path(&external_path, cx),
            Some(repository.key)
        );
        assert!(store.repository_for_path(&nested_path, cx).is_none());
    });
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn file_operations_preserve_scope_and_refuse_dirty_conflicted_and_git_files(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, true).await;
    trust(&fixture, cx);
    FixtureExtension::new("file-operations", fixture.directory.path(), "writable").register(cx);
    let repository = ready(&fixture, cx).await;
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    for path in ["nested/file.txt", "../src/example.txt", "src/missing.txt"] {
        let task = fixture.store.update(cx, |store, cx| {
            store.run_file_operation(
                repository.key.clone(),
                path.into(),
                FileOperation::Stage,
                cx,
            )
        });
        assert!(task.await.is_err(), "{path}");
    }
    let mut foreign_key = repository.key.clone();
    foreign_key.worktree_id = WorktreeId::from_usize(999_999);
    assert!(
        fixture
            .store
            .update(cx, |store, cx| {
                store.run_file_operation(
                    foreign_key,
                    "src/example.txt".into(),
                    FileOperation::Stage,
                    cx,
                )
            })
            .await
            .is_err()
    );
    for operation in [FileOperation::Stage, FileOperation::Unstage] {
        fixture
            .store
            .update(cx, |store, cx| {
                store.run_file_operation(
                    repository.key.clone(),
                    "src/example.txt".into(),
                    operation,
                    cx,
                )
            })
            .await
            .unwrap();
        ready(&fixture, cx).await;
    }
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "unsaved\n")], None, cx)
    });
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.run_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Stage,
                cx,
            )
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unsaved"));
    buffer.update(cx, |buffer, _| buffer.set_conflict());
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.run_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Unstage,
                cx,
            )
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("conflict"));
    let events =
        std::fs::read_to_string(fixture.directory.path().join("file-operations.jsonl")).unwrap();
    let applied: Vec<serde_json::Value> = events
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["event"] == "file-operation-applied")
        .collect();
    assert_eq!(applied.len(), 2);
    assert!(
        applied
            .iter()
            .all(|event| event["path"] == "src/example.txt")
    );
    assert_eq!(
        fixture
            .fs
            .load(&fixture.directory.path().join("src/example.txt"))
            .await
            .unwrap(),
        "working text\n"
    );
    assert_eq!(
        fixture
            .fs
            .load(&fixture.directory.path().join("nested/file.txt"))
            .await
            .unwrap(),
        "Git content"
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn file_operation_rechecks_buffer_after_prepare_and_rejects_double_dispatch(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "prepare-buffer",
        fixture.directory.path(),
        "prepare-pending",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let task = fixture.store.update(cx, |store, cx| {
        store.run_file_operation(
            repository.key.clone(),
            "src/example.txt".into(),
            FileOperation::Stage,
            cx,
        )
    });
    let events_path = fixture.directory.path().join("prepare-buffer.jsonl");
    provider_event(events_path.clone(), "prepare-held").await;
    let duplicate = fixture.store.update(cx, |store, cx| {
        store.run_file_operation(
            repository.key.clone(),
            "src/example.txt".into(),
            FileOperation::Stage,
            cx,
        )
    });
    assert!(duplicate.await.unwrap_err().to_string().contains("running"));
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "edited during prepare\n")], None, cx)
    });
    std::fs::write(events_path.with_extension("jsonl.release"), "release").unwrap();
    assert!(task.await.unwrap_err().to_string().contains("unsaved"));
    fixture.store.read_with(cx, |store, _| {
        assert!(!store.file_operation_running());
        assert!(store.pending_file_operations.is_empty());
    });
    let events = std::fs::read_to_string(events_path).unwrap();
    assert!(!events.contains("vcs/applyFileOperation"));
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn file_operation_unknown_outcome_requires_reconciliation_before_retry(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "unknown-operation",
        fixture.directory.path(),
        "write-unknown",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let first = fixture.store.update(cx, |store, cx| {
        store.run_file_operation(
            repository.key.clone(),
            "src/example.txt".into(),
            FileOperation::Stage,
            cx,
        )
    });
    assert!(first.await.unwrap_err().to_string().contains("unknown"));
    let duplicate = fixture.store.update(cx, |store, cx| {
        store.run_file_operation(
            repository.key.clone(),
            "src/example.txt".into(),
            FileOperation::Stage,
            cx,
        )
    });
    assert!(duplicate.await.unwrap_err().to_string().contains("unknown"));
    let events_path = fixture.directory.path().join("unknown-operation.jsonl");
    std::fs::write(events_path.with_extension("jsonl.reconcile"), "ready").unwrap();
    fixture.store.update(cx, |store, cx| store.refresh_all(cx));
    cx.condition(&fixture.store, |store, _| {
        !store.file_operation_running() && store.pending_file_operations.is_empty()
    })
    .await;
    fixture.store.read_with(cx, |store, _| {
        assert!(store.file_operation_error(&repository.key).is_none());
    });
    let events = std::fs::read_to_string(events_path).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"method\": \"vcs/applyFileOperation\""))
            .count(),
        1
    );
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"event\": \"file-operation-applied\""))
            .count(),
        1
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn repository_commit_uses_cached_state_and_rejects_a_changed_index(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "repository-commit",
        fixture.directory.path(),
        "repository-operations",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    assert_eq!(
        repository.repository_state.as_ref().unwrap().staged_paths,
        ["src/example.txt"]
    );
    let operation = RepositoryOperation::Commit {
        message: "Commit staged content".into(),
    };
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_repository_operation(repository.key.clone(), operation.clone(), cx)
        })
        .await
        .unwrap();
    assert_eq!(prepared.affected_paths(), ["src/example.txt"]);
    fixture
        .store
        .update(cx, |store, cx| {
            store.run_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Stage,
                cx,
            )
        })
        .await
        .unwrap();
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_repository_operation(prepared, cx)
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("checkout changed"));
    ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_repository_operation(repository.key.clone(), operation, cx)
        })
        .await
        .unwrap();
    fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_repository_operation(prepared, cx)
        })
        .await
        .unwrap();
    let updated = ready(&fixture, cx).await;
    assert!(updated.repository_state.unwrap().staged_paths.is_empty());
    assert_eq!(
        fixture
            .fs
            .load(&fixture.directory.path().join("src/example.txt"))
            .await
            .unwrap(),
        "working text\n"
    );
    let events =
        std::fs::read_to_string(fixture.directory.path().join("repository-commit.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"event\": \"repository-operation-applied\""))
            .count(),
        1
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn branch_switch_refuses_dirty_buffers_and_another_open_project(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "repository-switch",
        fixture.directory.path(),
        "repository-clean",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let operation = RepositoryOperation::SwitchBranch {
        branch_id: "topic".into(),
    };
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "unsaved\n")], None, cx)
    });
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_repository_operation(repository.key.clone(), operation.clone(), cx)
        })
        .await
        .err()
        .expect("dirty buffer blocks branch switch");
    assert!(error.to_string().contains("unsaved"));
    drop(buffer);
    cx.run_until_parked();
    let other = Project::test(fixture.fs.clone(), [fixture.directory.path()], cx).await;
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_repository_operation(repository.key.clone(), operation, cx)
        })
        .await
        .err()
        .expect("another open project blocks branch switch");
    assert!(error.to_string().contains("other projects"));
    let events =
        std::fs::read_to_string(fixture.directory.path().join("repository-switch.jsonl")).unwrap();
    assert!(!events.contains("vcs/prepareRepositoryOperation"));
    drop(other);
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn unknown_repository_operation_blocks_file_writes_until_reconciled(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "repository-unknown",
        fixture.directory.path(),
        "repository-unknown",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_repository_operation(
                repository.key.clone(),
                RepositoryOperation::Commit {
                    message: "Saved intent".into(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    assert!(
        fixture
            .store
            .update(cx, |store, cx| {
                store.apply_prepared_repository_operation(prepared, cx)
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown")
    );
    assert!(
        fixture
            .store
            .update(cx, |store, cx| {
                store.run_file_operation(
                    repository.key.clone(),
                    "src/example.txt".into(),
                    FileOperation::Stage,
                    cx,
                )
            })
            .await
            .is_err()
    );
    let events = fixture.directory.path().join("repository-unknown.jsonl");
    std::fs::write(events.with_extension("jsonl.reconcile"), "ready").unwrap();
    fixture.store.update(cx, |store, cx| store.refresh_all(cx));
    cx.condition(&fixture.store, |store, _| {
        !store.file_operation_running() && !store.repository_operation_unresolved(&repository.key)
    })
    .await;
    let events = std::fs::read_to_string(events).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"method\": \"vcs/applyRepositoryOperation\""))
            .count(),
        1
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn another_store_cannot_recover_an_intent_before_its_owner_dispatches(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("active-owner", fixture.directory.path(), "write-unknown").register(cx);
    let repository = ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Stage,
                cx,
            )
        })
        .await
        .unwrap();
    let journal = fixture
        .store
        .read_with(cx, |store, _| store.operation_journal.clone());
    let owner = journal
        .record(
            prepared.scope.clone(),
            PendingFileOperation {
                path: prepared.path.clone(),
                operation: prepared.operation,
                token: prepared.token.clone(),
            },
        )
        .await
        .unwrap();
    let parts = fixture.store.read_with(cx, |store, _| {
        (
            store.project.clone(),
            store.worktrees.clone(),
            store.git.clone(),
            store.environment.clone(),
        )
    });
    let refresher =
        cx.new(|cx| VcsStore::new(parts.0, parts.1, parts.2, parts.3, fixture.fs.clone(), cx));
    cx.condition(&refresher, |store, _| {
        store
            .repositories()
            .iter()
            .any(|repository| repository.snapshot.is_some() && !repository.loading)
    })
    .await;
    refresher.update(cx, |store, cx| store.refresh_all(cx));
    cx.condition(&refresher, |store, _| {
        !store
            .repositories()
            .iter()
            .any(|repository| repository.loading)
    })
    .await;
    let events_path = fixture.directory.path().join("active-owner.jsonl");
    assert!(
        !std::fs::read_to_string(&events_path)
            .unwrap()
            .contains("vcs/reconcileFileOperation")
    );
    assert!(journal.pending(&prepared.scope).unwrap().is_some());
    assert!(
        prepared
            .connection
            .apply_file_operation(
                prepared.key.repository_id,
                prepared.path,
                prepared.operation,
                prepared.token,
            )
            .await
            .is_err()
    );
    drop(owner);
    assert!(journal.pending(&prepared.scope).unwrap().is_some());
    let connections = refresher.read_with(cx, |store, _| {
        store
            .sessions
            .values()
            .filter_map(|session| session.connection.clone())
            .collect::<Vec<_>>()
    });
    for connection in connections {
        connection.shutdown().await.unwrap();
    }
    refresher.update(cx, |store, cx| store.stop_all(cx));
    std::fs::write(events_path.with_extension("jsonl.reconcile"), "ready").unwrap();
    fixture.store.update(cx, |store, cx| store.refresh_all(cx));
    cx.condition(&fixture.store, |store, _| {
        !store.file_operation_running() && !store.file_operation_unresolved(&repository.key)
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(events_path)
            .unwrap()
            .lines()
            .filter(|line| line.contains("\"method\": \"vcs/applyFileOperation\""))
            .count(),
        1
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn prepared_branch_switch_is_rejected_after_git_takes_scope_ownership(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "branch-takeover",
        fixture.directory.path(),
        "repository-clean",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_repository_operation(
                repository.key.clone(),
                RepositoryOperation::SwitchBranch {
                    branch_id: "topic".into(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    // Hold the provider view stale so the apply guard, not eventual rediscovery, must reject.
    fixture
        .store
        .update(cx, |store, _| store._subscriptions.clear());
    fixture
        .fs
        .insert_tree(fixture.directory.path().join(".git"), json!({}))
        .await;
    let _buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    fixture.store.read_with(cx, |store, cx| {
        assert!(store.repositories.contains_key(&repository.key));
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(
                    &ProjectPath {
                        worktree_id: fixture.worktree_id,
                        path: Arc::from(RelPath::empty()),
                    },
                    cx
                )
                .is_some(),
            "Git ownership must be discovered before apply"
        );
    });
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_repository_operation(prepared, cx)
        })
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("provider")
            || error.to_string().contains("closed")
            || error.to_string().contains("ownership"),
        "{error:#}"
    );
    assert!(
        !std::fs::read_to_string(fixture.directory.path().join("branch-takeover.jsonl"))
            .unwrap()
            .contains("vcs/applyRepositoryOperation")
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn unresolved_file_operation_survives_worktree_identity_change(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "reopened-operation",
        fixture.directory.path(),
        "write-unknown",
    )
    .register(cx);
    let original = ready(&fixture, cx).await;
    assert!(
        fixture
            .store
            .update(cx, |store, cx| {
                store.run_file_operation(
                    original.key.clone(),
                    "src/example.txt".into(),
                    FileOperation::Stage,
                    cx,
                )
            })
            .await
            .is_err()
    );
    fixture.project.update(cx, |project, cx| {
        project.remove_worktree(fixture.worktree_id, cx)
    });
    cx.condition(&fixture.store, |store, _| store.repositories().is_empty())
        .await;
    let reopened_id = open_worktree(&fixture, fixture.directory.path(), cx).await;
    assert_ne!(fixture.worktree_id, reopened_id);
    trust_worktree(&fixture, reopened_id, cx);
    let reopened = ready(&fixture, cx).await;
    cx.condition(&fixture.store, |store, _| !store.file_operation_running())
        .await;
    fixture.store.read_with(cx, |store, _| {
        assert!(store.file_operation_unresolved(&reopened.key));
    });
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.run_file_operation(
                reopened.key.clone(),
                "src/example.txt".into(),
                FileOperation::Stage,
                cx,
            )
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown"));
    let events =
        std::fs::read_to_string(fixture.directory.path().join("reopened-operation.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"method\": \"vcs/applyFileOperation\""))
            .count(),
        1
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn crashing_reconciliation_waits_for_explicit_reconnect(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    let extension = FixtureExtension::new(
        "crashing-reconcile",
        fixture.directory.path(),
        "write-reconcile-crash",
    );
    extension.register(cx);
    let repository = ready(&fixture, cx).await;
    assert!(
        fixture
            .store
            .update(cx, |store, cx| {
                store.run_file_operation(
                    repository.key.clone(),
                    "src/example.txt".into(),
                    FileOperation::Stage,
                    cx,
                )
            })
            .await
            .is_err()
    );
    cx.executor().advance_clock(Duration::from_secs(20));
    cx.run_until_parked();
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 1);
    fixture.store.update(cx, |store, cx| store.refresh_all(cx));
    cx.condition(&fixture.store, |store, _| {
        !store.file_operation_running()
            && store
                .repositories
                .values()
                .any(|repository| !repository.connection.is_alive())
            && extension.callbacks.load(Ordering::SeqCst) == 2
    })
    .await;
    cx.executor().advance_clock(Duration::from_secs(20));
    cx.run_until_parked();
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 2);
    fixture.store.read_with(cx, |store, _| {
        assert!(store.file_operation_unresolved(&repository.key));
    });
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn external_repository_preserves_nested_git_ownership_and_refuses_ambiguity(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, true).await;
    trust(&fixture, cx);
    FixtureExtension::new("first", fixture.directory.path(), "normal").register(cx);
    let repository = ready(&fixture, cx).await;
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    fixture.store.read_with(cx, |store, cx| {
        let external = ProjectPath {
            worktree_id: fixture.worktree_id,
            path: Arc::from(RelPath::from_unix_str("src/example.txt").unwrap()),
        };
        let nested = ProjectPath {
            worktree_id: fixture.worktree_id,
            path: Arc::from(RelPath::from_unix_str("nested/file.txt").unwrap()),
        };
        assert_eq!(
            store.repository_for_path(&external, cx),
            Some(repository.key.clone())
        );
        assert!(store.repository_for_path(&nested, cx).is_none());
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&nested, cx)
                .is_some()
        );
    });
    FixtureExtension::new("second", fixture.directory.path(), "normal").register(cx);
    cx.condition(&fixture.store, |store, _| {
        store.repositories().len() == 2
            && store
                .repositories()
                .iter()
                .all(|repository| repository.error.is_some())
    })
    .await;
    fixture.store.read_with(cx, |store, cx| {
        let path = ProjectPath {
            worktree_id: fixture.worktree_id,
            path: Arc::from(RelPath::from_unix_str("src/example.txt").unwrap()),
        };
        assert!(store.repository_for_path(&path, cx).is_none());
        assert!(store.changes(&repository.key, cx).is_empty());
    });
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn linked_git_worktree_keeps_git_ownership_with_external_provider_registered(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    let main = tempfile::tempdir().expect("main Git repository directory");
    let linked = tempfile::tempdir().expect("linked Git checkout directory");
    let git_directory = main.path().join(".git/worktrees/feature");
    fixture
        .fs
        .insert_tree(
            main.path(),
            json!({
                ".git": {
                    "HEAD": "ref: refs/heads/main\n",
                    "worktrees": {"feature": {
                        "HEAD": "ref: refs/heads/feature\n",
                        "commondir": "../..\n",
                        "gitdir": format!("{}\n", linked.path().join(".git").display())
                    }}
                }
            }),
        )
        .await;
    fixture
        .fs
        .insert_tree(
            linked.path(),
            json!({
                ".fixture-vcs": "",
                ".git": format!("gitdir: {}\n", git_directory.display()),
                "src": {"example.txt": "linked Git working content\n"}
            }),
        )
        .await;
    assert_eq!(
        crate::git_store::resolve_git_worktree_to_main_repo(
            fixture.fs.as_ref(),
            linked.path(),
            &[]
        )
        .await,
        Some(main.path().to_path_buf())
    );
    let linked_id = open_worktree(&fixture, linked.path(), cx).await;
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    trust(&fixture, cx);
    trust_worktree(&fixture, linked_id, cx);
    let extension = FixtureExtension::new("linked-arbitration", fixture.directory.path(), "normal");
    extension.register(cx);
    let external = ready(&fixture, cx).await;
    fixture.store.read_with(cx, |store, cx| {
        let linked_path = ProjectPath {
            worktree_id: linked_id,
            path: Arc::from(RelPath::from_unix_str("src/example.txt").unwrap()),
        };
        assert!(
            store
                .git
                .read(cx)
                .repository_and_path_for_project_path(&linked_path, cx)
                .is_some()
        );
        assert!(store.repository_for_path(&linked_path, cx).is_none());
        assert_eq!(store.repositories().len(), 1);
        assert_eq!(external.key.worktree_id, fixture.worktree_id);
        assert!(!store.sessions.keys().any(|key| key.0 == linked_id));
    });
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 1);
    assert!(
        !fixture
            .fs
            .metadata(&linked.path().join(".git"))
            .await
            .unwrap()
            .unwrap()
            .is_dir
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn multiple_external_scopes_keep_baselines_and_lifetimes_independent(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    let second = tempfile::tempdir().expect("second external scope");
    fixture
        .fs
        .insert_tree(
            second.path(),
            json!({
                ".fixture-vcs": "", "src": {"example.txt": "second working content\n"}
            }),
        )
        .await;
    let second_id = open_worktree(&fixture, second.path(), cx).await;
    trust(&fixture, cx);
    trust_worktree(&fixture, second_id, cx);
    let extension = FixtureExtension::new("multiple-scopes", fixture.directory.path(), "normal");
    extension.register(cx);
    cx.condition(&fixture.store, |store, _| {
        let repositories = store.repositories();
        repositories.len() == 2
            && repositories
                .iter()
                .all(|repository| repository.snapshot.is_some() && !repository.loading)
    })
    .await;
    let repositories = fixture.store.read_with(cx, |store, _| store.repositories());
    let first = repositories
        .iter()
        .find(|repository| repository.key.worktree_id == fixture.worktree_id)
        .expect("first scope")
        .clone();
    let second_repository = repositories
        .iter()
        .find(|repository| repository.key.worktree_id == second_id)
        .expect("second scope")
        .clone();
    assert_eq!(first.descriptor.id, second_repository.descriptor.id);
    assert_ne!(first.key, second_repository.key);
    assert_ne!(
        first.descriptor.scope_uri,
        second_repository.descriptor.scope_uri
    );
    let (first_connection, second_connection) = fixture.store.read_with(cx, |store, cx| {
        for repository in [&first, &second_repository] {
            let path = ProjectPath {
                worktree_id: repository.key.worktree_id,
                path: Arc::from(RelPath::from_unix_str("src/example.txt").unwrap()),
            };
            assert_eq!(
                store.repository_for_path(&path, cx),
                Some(repository.key.clone())
            );
        }
        (
            store.repositories[&first.key].connection.clone(),
            store.repositories[&second_repository.key]
                .connection
                .clone(),
        )
    });
    assert!(!Arc::ptr_eq(&first_connection, &second_connection));
    let first_diff = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff(first.key.clone(), "src/example.txt".into(), cx)
        })
        .await
        .unwrap();
    let second_diff = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff(second_repository.key.clone(), "src/example.txt".into(), cx)
        })
        .await
        .unwrap();
    fixture
        .store
        .update(cx, |store, cx| {
            store.set_baseline(&first.key, "previous".into(), cx)
        })
        .unwrap();
    wait_diff_base(&first_diff.diff, "previous — r41\n", cx).await;
    assert_eq!(
        second_diff
            .diff
            .read_with(cx, |diff, cx| diff.base_text(cx).text()),
        "baseline — r42\n"
    );
    assert_eq!(
        second_diff.buffer.read_with(cx, |buffer, _| buffer.text()),
        "second working content\n"
    );
    fixture.store.read_with(cx, |store, _| {
        assert_eq!(
            store.repositories[&first.key].view.selected_baseline,
            "previous"
        );
        assert_eq!(
            store.repositories[&second_repository.key]
                .view
                .selected_baseline,
            "submitted"
        );
    });
    fixture.project.update(cx, |project, cx| {
        project.remove_worktree(fixture.worktree_id, cx)
    });
    cx.condition(&fixture.store, |store, _| {
        let repositories = store.repositories();
        repositories.len() == 1
            && repositories
                .first()
                .is_some_and(|repository| repository.key == second_repository.key)
    })
    .await;
    provider_event(
        fixture.directory.path().join("multiple-scopes.jsonl"),
        "exit-received",
    )
    .await;
    assert!(!first_connection.is_alive());
    fixture.store.read_with(cx, |store, _| {
        let remaining = &store.repositories[&second_repository.key];
        assert!(Arc::ptr_eq(&remaining.connection, &second_connection));
        assert!(remaining.connection.is_alive());
        assert_eq!(remaining.view.selected_baseline, "submitted");
    });
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 2);
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn baseline_change_updates_existing_live_diff_and_close_reopen_uses_new_session(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    let extension = FixtureExtension::new("reopen", fixture.directory.path(), "normal");
    extension.register(cx);
    let repository = ready(&fixture, cx).await;
    let opened = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff(repository.key.clone(), "src/example.txt".into(), cx)
        })
        .await
        .unwrap();
    opened.buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "unsaved ")], None, cx);
    });
    let before = opened.buffer.read_with(cx, |buffer, _| buffer.text());
    fixture
        .store
        .update(cx, |store, cx| {
            store.set_baseline(&repository.key, "previous".into(), cx)
        })
        .unwrap();
    ready(&fixture, cx).await;
    wait_diff_base(&opened.diff, "previous — r41\n", cx).await;
    assert_eq!(
        opened.buffer.read_with(cx, |buffer, _| buffer.text()),
        before
    );
    let old_connection = fixture.store.read_with(cx, |store, _| {
        store
            .repositories
            .get(&repository.key)
            .unwrap()
            .connection
            .clone()
    });
    fixture.project.update(cx, |project, cx| {
        project.remove_worktree(fixture.worktree_id, cx)
    });
    cx.condition(&fixture.store, |store, _| store.repositories().is_empty())
        .await;
    provider_event(
        fixture.directory.path().join("reopen.jsonl"),
        "exit-received",
    )
    .await;
    assert!(!old_connection.is_alive());
    let (worktree, _) = fixture
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(fixture.directory.path(), true, cx)
        })
        .await
        .unwrap();
    worktree
        .read_with(cx, |worktree, _| {
            worktree.as_local().unwrap().scan_complete()
        })
        .await;
    let worktree_store = fixture
        .project
        .read_with(cx, |project, _| project.worktree_store());
    cx.update(|cx| {
        TrustedWorktrees::try_get_global(cx)
            .unwrap()
            .update(cx, |trust, cx| {
                trust.trust(
                    &worktree_store,
                    collections::HashSet::from_iter([
                        crate::trusted_worktrees::PathTrust::Worktree(worktree.read(cx).id()),
                    ]),
                    cx,
                )
            })
    });
    let reopened = ready(&fixture, cx).await;
    assert_ne!(reopened.key.worktree_id, repository.key.worktree_id);
    assert_eq!(reopened.selected_baseline, "submitted");
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 2);
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn refresh_preserves_baseline_for_healthy_and_reconnected_provider(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    let extension = FixtureExtension::new("refresh", fixture.directory.path(), "normal");
    extension.register(cx);
    let repository = ready(&fixture, cx).await;
    fixture
        .store
        .update(cx, |store, cx| {
            store.set_baseline(&repository.key, "previous".into(), cx)
        })
        .unwrap();
    ready(&fixture, cx).await;
    let opened = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff(repository.key.clone(), "src/example.txt".into(), cx)
        })
        .await
        .unwrap();
    fixture.store.update(cx, |store, cx| store.refresh_all(cx));
    let refreshed = ready(&fixture, cx).await;
    assert_eq!(refreshed.selected_baseline, "previous");
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 1);
    wait_diff_base(&opened.diff, "previous — r41\n", cx).await;
    let connection = fixture.store.read_with(cx, |store, _| {
        store
            .repositories
            .get(&repository.key)
            .unwrap()
            .connection
            .clone()
    });
    connection.shutdown().await.unwrap();
    cx.condition(&fixture.store, |store, _| {
        store
            .repositories()
            .first()
            .is_some_and(|repository| repository.error.is_some())
    })
    .await;
    fixture.store.update(cx, |store, cx| store.refresh_all(cx));
    let reconnected = ready(&fixture, cx).await;
    assert_eq!(reconnected.selected_baseline, "previous");
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 2);
    wait_diff_base(&opened.diff, "previous — r41\n", cx).await;
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn late_snapshot_cannot_replace_new_baseline(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    let extension = FixtureExtension::new("reordered", fixture.directory.path(), "reordered");
    extension.register(cx);
    provider_event(
        fixture.directory.path().join("reordered.jsonl"),
        "snapshot-held",
    )
    .await;
    let repository = fixture
        .store
        .read_with(cx, |store, _| store.repositories().remove(0));
    assert!(repository.loading);
    fixture
        .store
        .update(cx, |store, cx| {
            store.set_baseline(&repository.key, "previous".into(), cx)
        })
        .unwrap();
    let current = ready(&fixture, cx).await;
    assert_eq!(current.snapshot.as_ref().unwrap().revision_id, "r41");
    let opened = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff(repository.key.clone(), "src/example.txt".into(), cx)
        })
        .await
        .unwrap();
    assert_eq!(
        opened
            .diff
            .read_with(cx, |diff, cx| diff.base_text(cx).text()),
        "previous — r41\n"
    );
    fixture.store.read_with(cx, |store, _| {
        let current = &store.repositories.get(&repository.key).unwrap().view;
        assert_eq!(current.selected_baseline, "previous");
        assert_eq!(current.snapshot.as_ref().unwrap().id, "snapshot:previous");
    });
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn comparison_load_does_not_supersede_repository_state_refresh(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "comparison-state",
        fixture.directory.path(),
        "repository-operations",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    assert!(repository.repository_state.is_some());
    let comparison = fixture.store.update(cx, |store, cx| {
        store
            .repositories
            .get_mut(&repository.key)
            .unwrap()
            .view
            .repository_state = None;
        store.refresh(&repository.key, cx);
        store.load_comparison(
            repository.key.clone(),
            repository.selected_baseline.clone(),
            cx,
        )
    });
    comparison.await.unwrap();
    let refreshed = ready(&fixture, cx).await;
    assert_eq!(
        refreshed.repository_state.unwrap().staged_paths,
        ["src/example.txt"]
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn periodic_refresh_reuses_session_and_skips_in_flight_snapshot(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    let extension = FixtureExtension::new("polling", fixture.directory.path(), "normal");
    extension.register(cx);
    let repository = ready(&fixture, cx).await;
    let initial_generation = fixture.store.read_with(cx, |store, _| {
        store.repositories.get(&repository.key).unwrap().generation
    });
    cx.executor().advance_clock(Duration::from_secs(5));
    cx.condition(&fixture.store, |store, _| {
        store
            .repositories
            .get(&repository.key)
            .is_some_and(|repository| {
                repository.generation > initial_generation && !repository.view.loading
            })
    })
    .await;
    assert_eq!(extension.callbacks.load(Ordering::SeqCst), 1);
    let generation = fixture.store.update(cx, |store, _| {
        let repository = store.repositories.get_mut(&repository.key).unwrap();
        repository.view.loading = true;
        repository.generation
    });
    cx.executor().advance_clock(Duration::from_secs(5));
    cx.run_until_parked();
    assert_eq!(
        fixture.store.read_with(cx, |store, _| store
            .repositories
            .get(&repository.key)
            .unwrap()
            .generation),
        generation
    );
    stop(&fixture, cx).await;
    fixture
        .store
        .read_with(cx, |store, _| assert!(store.poll_task.is_none()));
}

#[gpui::test]
async fn unchanged_poll_reuses_baseline_but_new_revision_reloads_it(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    let extension =
        FixtureExtension::new("content-cache", fixture.directory.path(), "polling-content");
    extension.register(cx);
    let repository = ready(&fixture, cx).await;
    let opened = fixture
        .store
        .update(cx, |store, cx| {
            store.open_diff(repository.key.clone(), "src/example.txt".into(), cx)
        })
        .await
        .unwrap();
    let events = fixture.directory.path().join("content-cache.jsonl");
    let content_reads = || {
        std::fs::read_to_string(&events)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|event| event["method"] == "vcs/readContent")
            .count()
    };
    assert_eq!(content_reads(), 1);
    let mut snapshot_id = repository.snapshot.unwrap().id.clone();
    for _ in 0..2 {
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.condition(&fixture.store, |store, _| {
            store
                .repositories
                .get(&repository.key)
                .is_some_and(|repository| {
                    !repository.view.loading
                        && repository
                            .view
                            .snapshot
                            .as_ref()
                            .is_some_and(|snapshot| snapshot.id != snapshot_id)
                })
        })
        .await;
        snapshot_id = fixture.store.read_with(cx, |store, _| {
            store
                .repositories
                .get(&repository.key)
                .unwrap()
                .view
                .snapshot
                .as_ref()
                .unwrap()
                .id
                .clone()
        });
        let reopened = fixture
            .store
            .update(cx, |store, cx| {
                store.open_buffer_diff(opened.buffer.clone(), cx)
            })
            .await
            .unwrap();
        assert_eq!(reopened, opened.diff);
        assert_eq!(
            content_reads(),
            1,
            "unchanged polls and reopening a buffer must reuse its baseline"
        );
    }
    std::fs::write(events.with_extension("jsonl.revision"), "r43").unwrap();
    cx.executor().advance_clock(Duration::from_secs(5));
    cx.condition(&fixture.store, |store, _| {
        store
            .repositories
            .get(&repository.key)
            .is_some_and(|repository| {
                !repository.view.loading
                    && repository
                        .view
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.revision_id == "r43")
            })
    })
    .await;
    wait_diff_base(&opened.diff, "baseline — r43\n", cx).await;
    assert_eq!(
        content_reads(),
        2,
        "changed baseline revision must be loaded"
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn concurrent_diff_readers_share_content_and_keep_pending_edits(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("coalesced", fixture.directory.path(), "normal").register(cx);
    ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let (first, second) = fixture.store.update(cx, |store, cx| {
        (
            store.open_buffer_diff(buffer.clone(), cx),
            store.open_buffer_diff(buffer.clone(), cx),
        )
    });
    buffer.update(cx, |buffer, cx| {
        buffer.edit(
            [(0..buffer.len(), "baseline — r42\nunsaved second line\n")],
            None,
            cx,
        );
    });
    let (first, second) = futures::future::join(first, second).await;
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    first.read_with(cx, |diff, cx| {
        assert_eq!(diff.base_text(cx).text(), "baseline — r42\n");
        assert_eq!(
            diff.snapshot(cx).buffer_snapshot().text(),
            "baseline — r42\nunsaved second line\n"
        );
        assert_eq!(diff.changed_row_counts(), (1, 0));
    });
    let events = std::fs::read_to_string(fixture.directory.path().join("coalesced.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|event| event["method"] == "vcs/readContent")
            .count(),
        1
    );
    assert_eq!(
        fixture
            .fs
            .load(&fixture.directory.path().join("src/example.txt"))
            .await
            .unwrap(),
        "working text\n"
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn superseded_diff_readers_wait_for_the_new_baseline(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("superseded-content", fixture.directory.path(), "normal").register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let (first, second, latest) = fixture.store.update(cx, |store, cx| {
        let first = store.open_buffer_diff(buffer.clone(), cx);
        let second = store.open_buffer_diff(buffer.clone(), cx);
        store
            .set_baseline(&repository.key, "previous".into(), cx)
            .unwrap();
        let latest = store.open_buffer_diff(buffer.clone(), cx);
        (first, second, latest)
    });
    let (first, second, latest) = futures::join!(first, second, latest);
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    assert_eq!(first, latest.unwrap());
    assert_eq!(
        first.read_with(cx, |diff, cx| diff.base_text(cx).text()),
        "previous — r41\n"
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn coalesced_diff_readers_receive_real_provider_errors(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("content-error", fixture.directory.path(), "polling-content")
        .register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let (first, second) = fixture.store.update(cx, |store, cx| {
        let repository = store.repositories.get_mut(&repository.key).unwrap();
        Arc::make_mut(repository.view.snapshot.as_mut().unwrap()).id = "expired".into();
        (
            store.open_buffer_diff(buffer.clone(), cx),
            store.open_buffer_diff(buffer.clone(), cx),
        )
    });
    let (first, second) = futures::future::join(first, second).await;
    for result in [first, second] {
        assert!(format!("{:#}", result.unwrap_err()).contains("Unknown fixture snapshot"));
    }
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn changing_snapshots_do_not_accumulate_internal_diff_waiters(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "slow-content",
        fixture.directory.path(),
        "changing-slow-content",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());
    let first = fixture
        .store
        .update(cx, |store, cx| store.open_buffer_diff(buffer.clone(), cx));
    provider_event(
        fixture.directory.path().join("slow-content.jsonl"),
        "content-held",
    )
    .await;
    for _ in 0..4 {
        fixture
            .store
            .update(cx, |store, cx| store.refresh(&repository.key, cx));
        ready(&fixture, cx).await;
        fixture.store.read_with(cx, |store, _| {
            assert_eq!(store.diffs.get(&buffer_id).unwrap().waiters.len(), 1);
        });
    }
    drop(first);
    cx.run_until_parked();
    let second = fixture
        .store
        .update(cx, |store, cx| store.open_buffer_diff(buffer, cx));
    fixture.store.read_with(cx, |store, _| {
        assert_eq!(
            store.diffs.get(&buffer_id).unwrap().waiters.len(),
            1,
            "dropped readers must be pruned"
        );
    });
    drop(second);
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn closing_buffer_before_snapshot_completes_resolves_diff_waiter(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("closed-buffer", fixture.directory.path(), "normal").register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = cx.new(|cx| Buffer::local("working text\n", cx));
    let weak_buffer = buffer.downgrade();
    let waiting = fixture.store.update(cx, |store, cx| {
        let current = store.repositories.get_mut(&repository.key).unwrap();
        current.view.snapshot = None;
        current.view.loading = true;
        store.ensure_diff(&repository.key, "src/example.txt", &buffer, cx);
        store.load_diff_if_needed(buffer.clone(), cx)
    });
    drop(buffer);
    cx.run_until_parked();
    assert!(weak_buffer.upgrade().is_none());
    fixture
        .store
        .update(cx, |store, cx| store.refresh(&repository.key, cx));
    assert!(format!("{:#}", waiting.await.unwrap_err()).contains("VCS diff buffer closed"));
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn refresh_progress_distinguishes_explicit_actions_from_background_work(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    let initial_progress = Arc::new(AtomicUsize::new(0));
    let observer = initial_progress.clone();
    let _subscription = cx.update(|cx| {
        cx.observe(&fixture.store, move |store, cx| {
            for repository in store.read(cx).repositories() {
                if repository.loading && repository.snapshot.is_none() {
                    assert!(repository.show_loading);
                    observer.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
    });
    FixtureExtension::new("progress", fixture.directory.path(), "normal").register(cx);
    let repository = ready(&fixture, cx).await;
    assert!(initial_progress.load(Ordering::SeqCst) > 0);
    fixture.store.update(cx, |store, cx| {
        store.poll(cx);
        let view = &store.repositories.get(&repository.key).unwrap().view;
        assert!(view.loading);
        assert!(
            !view.show_loading,
            "ordinary background refresh should remain quiet"
        );
    });
    ready(&fixture, cx).await;
    fixture.store.update(cx, |store, cx| {
        store.refresh_all(cx);
        assert!(
            store
                .repositories
                .get(&repository.key)
                .unwrap()
                .view
                .show_loading
        );
        store.refresh_with_reason(&repository.key, RefreshReason::Background, cx);
        assert!(
            store
                .repositories
                .get(&repository.key)
                .unwrap()
                .view
                .show_loading,
            "background supersession must preserve explicit progress"
        );
    });
    ready(&fixture, cx).await;
    fixture.store.update(cx, |store, cx| {
        store
            .set_baseline(&repository.key, "previous".into(), cx)
            .unwrap();
        let view = &store.repositories.get(&repository.key).unwrap().view;
        assert!(view.loading && view.show_loading && view.snapshot.is_none());
    });
    let updated = ready(&fixture, cx).await;
    assert!(!updated.show_loading);
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn slow_background_refresh_reveals_progress_and_preserves_failures(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("slow-progress", fixture.directory.path(), "pending-refresh")
        .register(cx);
    let repository = ready(&fixture, cx).await;
    fixture.store.update(cx, |store, cx| store.poll(cx));
    provider_event(
        fixture.directory.path().join("slow-progress.jsonl"),
        "refresh-held",
    )
    .await;
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    fixture.store.read_with(cx, |store, _| {
        let view = &store.repositories.get(&repository.key).unwrap().view;
        assert!(view.loading && !view.show_loading);
    });
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.condition(&fixture.store, |store, _| {
        store
            .repositories
            .get(&repository.key)
            .unwrap()
            .view
            .show_loading
    })
    .await;
    let connection = fixture.store.read_with(cx, |store, _| {
        let repository = store.repositories.get(&repository.key).unwrap();
        assert!(repository.view.loading);
        repository.connection.clone()
    });
    connection.shutdown().await.unwrap();
    cx.condition(&fixture.store, |store, _| {
        store
            .repositories
            .get(&repository.key)
            .unwrap()
            .view
            .error
            .is_some()
    })
    .await;
    fixture.store.read_with(cx, |store, _| {
        let view = &store.repositories.get(&repository.key).unwrap().view;
        assert!(!view.loading && !view.show_loading);
        assert!(view.error.is_some());
    });
    fixture.store.update(cx, |store, cx| store.stop_all(cx));
}

#[gpui::test]
async fn new_store_restores_durable_unknown_operation_before_accepting_writes(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "durable-operation",
        fixture.directory.path(),
        "persistent-write-unknown",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    assert!(
        fixture
            .store
            .update(cx, |store, cx| {
                store.run_file_operation(
                    repository.key.clone(),
                    "src/example.txt".into(),
                    FileOperation::Stage,
                    cx,
                )
            })
            .await
            .is_err()
    );
    let journal_path = paths::data_dir().join("vcs/pending-file-operations.json");
    let saved: serde_json::Value =
        serde_json::from_str(&fixture.fs.load(&journal_path).await.unwrap()).unwrap();
    assert_eq!(saved["operations"].as_array().unwrap().len(), 1);
    stop(&fixture, cx).await;

    let restored = fixture.store.update(cx, |store, cx| {
        let (project, worktrees, git, environment, fs) = (
            store.project.clone(),
            store.worktrees.clone(),
            store.git.clone(),
            store.environment.clone(),
            store.fs.clone(),
        );
        cx.new(|cx| {
            let mut store = VcsStore::new(project, worktrees, git, environment, fs.clone(), cx);
            store.operation_journal = Arc::new(OperationJournal::new(fs, journal_path.clone()));
            store
        })
    });
    cx.condition(&restored, |store, _| {
        store
            .repositories()
            .first()
            .is_some_and(|repository| repository.snapshot.is_some())
            && !store.file_operation_running()
    })
    .await;
    let restored_key = restored.read_with(cx, |store, _| store.repositories()[0].key.clone());
    assert!(restored.read_with(cx, |store, _| {
        store.file_operation_unresolved(&restored_key)
    }));
    let error = restored
        .update(cx, |store, cx| {
            store.run_file_operation(
                restored_key.clone(),
                "src/example.txt".into(),
                FileOperation::Unstage,
                cx,
            )
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown"));
    let events_path = fixture.directory.path().join("durable-operation.jsonl");
    std::fs::write(events_path.with_extension("jsonl.reconcile"), "ready").unwrap();
    restored.update(cx, |store, cx| store.refresh_all(cx));
    cx.condition(&restored, |store, _| {
        !store.file_operation_running() && !store.file_operation_unresolved(&restored_key)
    })
    .await;
    let saved: serde_json::Value =
        serde_json::from_str(&fixture.fs.load(&journal_path).await.unwrap()).unwrap();
    assert_eq!(saved["operations"], json!([]));
    let events = std::fs::read_to_string(events_path).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"method\": \"vcs/applyFileOperation\""))
            .count(),
        1
    );
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"event\": \"file-operation-applied\""))
            .count(),
        1
    );
    let connections: Vec<_> = restored.read_with(cx, |store, _| {
        store
            .sessions
            .values()
            .filter_map(|session| session.connection.clone())
            .collect()
    });
    for connection in connections {
        connection.shutdown().await.unwrap();
    }
    restored.update(cx, |store, cx| store.stop_all(cx));
}

#[gpui::test]
async fn prepared_discard_rejects_changes_during_confirmation(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("confirmed-discard", fixture.directory.path(), "writable").register(cx);
    let repository = ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Discard,
                cx,
            )
        })
        .await
        .unwrap();
    assert!(
        !fixture
            .store
            .read_with(cx, |store, _| store.file_operation_running())
    );
    let events_path = fixture.directory.path().join("confirmed-discard.jsonl");
    std::fs::write(
        events_path.with_extension("jsonl.state"),
        "changed during confirmation",
    )
    .unwrap();
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_file_operation(prepared, cx)
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("changed"));

    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Discard,
                cx,
            )
        })
        .await
        .unwrap();
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "new unsaved edit\n")], None, cx)
    });
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_file_operation(prepared, cx)
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unsaved"));
    let events = std::fs::read_to_string(events_path).unwrap();
    assert!(!events.contains("file-operation-applied"));
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"method\": \"vcs/applyFileOperation\""))
            .count(),
        1
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn discard_preserves_edits_made_after_dispatch(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("dirty-discard", fixture.directory.path(), "discard-pending")
        .register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let task = fixture.store.update(cx, |store, cx| {
        store.run_file_operation(
            repository.key.clone(),
            "src/example.txt".into(),
            FileOperation::Discard,
            cx,
        )
    });
    let events_path = fixture.directory.path().join("dirty-discard.jsonl");
    provider_event(events_path.clone(), "apply-held").await;
    let journal_path = paths::data_dir().join("vcs/pending-file-operations.json");
    let saved: serde_json::Value =
        serde_json::from_str(&fixture.fs.load(&journal_path).await.unwrap()).unwrap();
    assert_eq!(saved["version"], 2);
    assert_eq!(
        saved["operations"][0]["pending"]["request"]["operation"],
        "discard"
    );
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "edit after dispatch\n")], None, cx)
    });
    let edited = buffer.read_with(cx, |buffer, _| buffer.text());
    fixture
        .fs
        .insert_file(
            fixture.directory.path().join("src/example.txt"),
            b"restored index text\n".to_vec(),
        )
        .await;
    std::fs::write(events_path.with_extension("jsonl.release"), "release").unwrap();
    let error = task.await.unwrap_err();
    assert!(error.to_string().contains("preserved"), "{error:#}");
    assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), edited);
    assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    assert!(!fixture.store.read_with(cx, |store, _| {
        store.file_operation_unresolved(&repository.key)
    }));
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn discarded_clean_buffer_reloads_through_the_existing_file_watcher(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("clean-discard", fixture.directory.path(), "discard-pending")
        .register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let task = fixture.store.update(cx, |store, cx| {
        store.run_file_operation(
            repository.key.clone(),
            "src/example.txt".into(),
            FileOperation::Discard,
            cx,
        )
    });
    let events_path = fixture.directory.path().join("clean-discard.jsonl");
    provider_event(events_path.clone(), "apply-held").await;
    fixture
        .fs
        .insert_file(
            fixture.directory.path().join("src/example.txt"),
            b"restored index text\n".to_vec(),
        )
        .await;
    cx.condition(&buffer, |buffer, _| {
        buffer.text() == "restored index text\n"
    })
    .await;
    assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    std::fs::write(events_path.with_extension("jsonl.release"), "release").unwrap();
    task.await.unwrap();
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn destructive_git_guard_checks_external_markers_before_same_directory_git(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    let git = fixture.store.read_with(cx, |store, _| store.git.clone());
    let empty_checkpoint = git.update(cx, |git, cx| git.checkpoint(cx)).await.unwrap();
    fixture
        .fs
        .insert_tree(
            fixture.directory.path().join("nested"),
            json!({".git": {}, "file.txt": "Git content"}),
        )
        .await;
    FixtureExtension::new("destructive-guard", fixture.directory.path(), "normal").register(cx);
    assert!(git.update(cx, |git, cx| git.checkpoint(cx)).await.is_err());
    assert!(
        git.update(cx, |git, cx| git.restore_checkpoint(empty_checkpoint, cx))
            .await
            .is_err()
    );
    fixture
        .fs
        .create_dir(&fixture.directory.path().join(".git"))
        .await
        .unwrap();
    let same_directory = fixture.store.read_with(cx, |store, cx| {
        store.check_git_destructive_operation(Arc::from(fixture.directory.path()), cx)
    });
    assert!(
        same_directory
            .await
            .unwrap_err()
            .to_string()
            .contains("external VCS")
    );
    let nested = fixture.store.read_with(cx, |store, cx| {
        store
            .check_git_destructive_operation(Arc::from(fixture.directory.path().join("nested")), cx)
    });
    nested.await.expect("genuine nested Git remains allowed");
    let initialization = fixture.store.read_with(cx, |store, cx| {
        store.check_git_initialization(Arc::from(fixture.directory.path()), cx)
    });
    initialization
        .await
        .expect("initialization keeps its existing Git-first behavior");
}

#[gpui::test]
async fn recovery_write_failure_prevents_apply_dispatch(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "journal-write-failure",
        fixture.directory.path(),
        "writable",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let journal_path = paths::data_dir().join("vcs/pending-file-operations.json");
    fixture.fs.create_dir(&journal_path).await.unwrap();
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.run_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Stage,
                cx,
            )
        })
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("journal"));
    let events =
        std::fs::read_to_string(fixture.directory.path().join("journal-write-failure.jsonl"))
            .unwrap();
    assert!(events.contains("vcs/prepareFileOperation"));
    assert!(!events.contains("vcs/applyFileOperation"));
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn nested_external_marker_blocks_git_checkpoint_and_worktree_operations(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    fixture
        .fs
        .remove_file(
            &fixture.directory.path().join(".fixture-vcs"),
            fs::RemoveOptions::default(),
        )
        .await
        .unwrap();
    let git = fixture.store.read_with(cx, |store, _| store.git.clone());
    let empty_checkpoint = git.update(cx, |git, cx| git.checkpoint(cx)).await.unwrap();
    let nested_path = fixture.directory.path().join("nested");
    let clean_path = fixture.directory.path().join("clean-git");
    for path in [&nested_path, &clean_path] {
        fixture
            .fs
            .insert_tree(
                path,
                json!({".git": {}, "file.txt": "original Git content\n"}),
            )
            .await;
    }
    let mut opened_buffers = Vec::new();
    for path in [&nested_path, &clean_path] {
        opened_buffers.push(
            fixture
                .project
                .update(cx, |project, cx| {
                    project.open_local_buffer(path.join("file.txt"), cx)
                })
                .await
                .unwrap(),
        );
    }
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    assert_eq!(
        fixture
            .project
            .read_with(cx, |project, cx| project.worktrees(cx).count()),
        1
    );
    cx.condition(&git, |git, _| git.repositories().len() == 2)
        .await;
    FixtureExtension::new(
        "nested-destructive-guard",
        fixture.directory.path(),
        "normal",
    )
    .register(cx);
    let (nested_repository, clean_repository) = git.read_with(cx, |git, cx| {
        let repository_for = |path: &Path| {
            git.repositories()
                .values()
                .find(|repository| {
                    repository
                        .read(cx)
                        .snapshot()
                        .work_directory_abs_path
                        .as_ref()
                        == path
                })
                .unwrap()
                .clone()
        };
        (repository_for(&nested_path), repository_for(&clean_path))
    });
    let genuine_checkpoint = git
        .update(cx, |git, cx| git.checkpoint(cx))
        .await
        .expect("genuine nested Git supports checkpoints with a provider registered");
    git.update(cx, |git, cx| {
        git.restore_checkpoint(genuine_checkpoint.clone(), cx)
    })
    .await
    .expect("genuine nested Git supports checkpoint restoration");
    clean_repository
        .read_with(cx, |repository, cx| {
            repository.check_worktree_operation(clean_path.clone(), cx)
        })
        .await
        .expect("clean source and target remain allowed");

    fixture
        .fs
        .insert_file(nested_path.join(".fixture-vcs"), Vec::new())
        .await;
    fixture
        .fs
        .insert_file(
            nested_path.join("file.txt"),
            b"keep current nested content\n".to_vec(),
        )
        .await;
    assert!(git.update(cx, |git, cx| git.checkpoint(cx)).await.is_err());
    assert!(
        git.update(cx, |git, cx| git.restore_checkpoint(empty_checkpoint, cx))
            .await
            .is_err()
    );
    assert!(
        git.update(cx, |git, cx| git.restore_checkpoint(genuine_checkpoint, cx))
            .await
            .is_err()
    );
    let target_guard = clean_repository.read_with(cx, |repository, cx| {
        repository.check_worktree_operation(nested_path.clone(), cx)
    });
    assert!(
        target_guard
            .await
            .unwrap_err()
            .to_string()
            .contains("external VCS")
    );
    let source_guard = nested_repository.read_with(cx, |repository, cx| {
        repository.check_worktree_operation(clean_path.clone(), cx)
    });
    assert!(
        source_guard
            .await
            .unwrap_err()
            .to_string()
            .contains("external VCS")
    );
    clean_repository
        .read_with(cx, |repository, cx| {
            repository.check_worktree_operation(clean_path.clone(), cx)
        })
        .await
        .expect("unrelated genuine Git stays allowed");
    assert!(
        fixture
            .fs
            .metadata(&fixture.directory.path().join(".fixture-vcs"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .fs
            .metadata(&nested_path.join(".fixture-vcs"))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        fixture
            .fs
            .load(&nested_path.join("file.txt"))
            .await
            .unwrap(),
        "keep current nested content\n"
    );
    stop(&fixture, cx).await;
}

async fn checkout_catalog_ready(
    fixture: &Fixture,
    cx: &mut TestAppContext,
) -> Arc<CheckoutCatalog> {
    cx.condition(&fixture.store, |store, _| {
        store.repositories().first().is_some_and(|repository| {
            repository.checkouts.is_some() && !repository.checkout_loading
        })
    })
    .await;
    fixture.store.read_with(cx, |store, _| {
        store.repositories()[0].checkouts.clone().unwrap()
    })
}

#[gpui::test]
async fn readonly_checkout_catalog_is_cached_and_unavailable_entries_are_not_mutated(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "readonly-checkouts",
        fixture.directory.path(),
        "checkout-read-only",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    let catalog = checkout_catalog_ready(&fixture, cx).await;
    assert_eq!(catalog.checkouts.len(), 5);
    assert!(
        catalog
            .checkouts
            .iter()
            .any(|checkout| !checkout.owned && checkout.state == CheckoutState::Missing)
    );
    assert!(fixture.store.read_with(cx, |store, _| {
        store.checkout_catalog_available(&repository.key)
    }));
    assert!(fixture.store.read_with(cx, |store, _| {
        store.checkout_operations(&repository.key).is_empty()
    }));
    for _ in 0..8 {
        fixture.store.update(cx, |store, cx| store.notify(cx));
    }
    cx.run_until_parked();
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.prepare_checkout_operation(
                repository.key.clone(),
                CheckoutOperation::Open {
                    checkout_id: "fixture-checkout".into()
                },
                cx,
            ))
            .await
            .is_err()
    );
    let events =
        std::fs::read_to_string(fixture.directory.path().join("readonly-checkouts.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"method\": \"vcs/listCheckouts\""))
            .count(),
        1
    );
    assert!(!events.contains("vcs/prepareCheckoutOperation"));
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn checkout_lifecycle_retains_identity_and_busy_target_blocks_apply(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("checkout-lifecycle", fixture.directory.path(), "checkouts").register(cx);
    let repository = ready(&fixture, cx).await;
    let catalog = checkout_catalog_ready(&fixture, cx).await;
    for checkout_id in ["foreign-checkout", "missing-checkout", "busy-checkout"] {
        assert!(
            fixture
                .store
                .update(cx, |store, cx| store.prepare_checkout_operation(
                    repository.key.clone(),
                    CheckoutOperation::Remove {
                        checkout_id: checkout_id.into()
                    },
                    cx,
                ))
                .await
                .is_err()
        );
    }
    for operation in [
        CheckoutOperation::Open {
            checkout_id: "unmounted-checkout".into(),
        },
        CheckoutOperation::Suspend {
            checkout_id: "unmounted-checkout".into(),
        },
        CheckoutOperation::Open {
            checkout_id: "unmounted-checkout".into(),
        },
        CheckoutOperation::Remove {
            checkout_id: "unmounted-checkout".into(),
        },
    ] {
        let expected = operation.checkout_id().unwrap().to_owned();
        let prepared = fixture
            .store
            .update(cx, |store, cx| {
                store.prepare_checkout_operation(repository.key.clone(), operation, cx)
            })
            .await
            .unwrap();
        let result = fixture
            .store
            .update(cx, |store, cx| {
                store.apply_prepared_checkout_operation(prepared, cx)
            })
            .await
            .unwrap();
        if let Some(checkout) = result.checkout {
            assert_eq!(checkout.id, expected);
        }
        checkout_catalog_ready(&fixture, cx).await;
    }
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_checkout_operation(
                repository.key.clone(),
                CheckoutOperation::Suspend {
                    checkout_id: "fixture-checkout".into(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    let target = catalog
        .checkouts
        .iter()
        .find(|checkout| checkout.id == "fixture-checkout")
        .unwrap();
    let scope = vcs::local_path(&target.scope_uri).unwrap();
    fixture
        .fs
        .insert_tree(&scope, json!({"keep.txt": "Target data"}))
        .await;
    open_worktree(&fixture, &scope, cx).await;
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_checkout_operation(prepared, cx)
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Close"));
    assert_eq!(
        fixture.fs.load(&scope.join("keep.txt")).await.unwrap(),
        "Target data"
    );
    let events =
        std::fs::read_to_string(fixture.directory.path().join("checkout-lifecycle.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"event\": \"checkout-operation-applied\""))
            .count(),
        4
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn checkout_unknown_recovers_from_picker_refresh_without_duplicate_creation(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "unknown-checkout",
        fixture.directory.path(),
        "checkout-unknown",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    checkout_catalog_ready(&fixture, cx).await;
    let operation = CheckoutOperation::Create {
        name: "new-checkout".into(),
        branch: "topic".into(),
        base: "main".into(),
    };
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_checkout_operation(repository.key.clone(), operation.clone(), cx)
        })
        .await
        .unwrap();
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store
                .apply_prepared_checkout_operation(prepared, cx))
            .await
            .is_err()
    );
    assert!(fixture.store.read_with(cx, |store, _| {
        store.repository_operation_unresolved(&repository.key)
    }));
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.prepare_checkout_operation(
                repository.key.clone(),
                operation,
                cx,
            ))
            .await
            .is_err()
    );
    let events_path = fixture.directory.path().join("unknown-checkout.jsonl");
    std::fs::write(events_path.with_extension("jsonl.reconcile"), "ready").unwrap();
    fixture
        .store
        .update(cx, |store, cx| store.refresh_checkouts(&repository.key, cx));
    cx.condition(&fixture.store, |store, _| {
        !store.file_operation_running() && !store.repository_operation_unresolved(&repository.key)
    })
    .await;
    let catalog = checkout_catalog_ready(&fixture, cx).await;
    assert_eq!(
        catalog
            .checkouts
            .iter()
            .filter(|checkout| checkout.name == "new-checkout")
            .count(),
        1
    );
    let events = std::fs::read_to_string(events_path).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"method\": \"vcs/applyCheckoutOperation\""))
            .count(),
        1
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn checkout_progress_is_token_bound_and_refresh_skips_active_owner(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "checkout-progress",
        fixture.directory.path(),
        "checkout-pending",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    checkout_catalog_ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_checkout_operation(
                repository.key.clone(),
                CheckoutOperation::Create {
                    name: "progress-checkout".into(),
                    branch: "topic".into(),
                    base: "main".into(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    let token = prepared.prepared.operation_token.clone();
    let apply = fixture.store.update(cx, |store, cx| {
        store.apply_prepared_checkout_operation(prepared, cx)
    });
    let events_path = fixture.directory.path().join("checkout-progress.jsonl");
    provider_event(events_path.clone(), "checkout-apply-held").await;
    cx.condition(&fixture.store, |store, _| {
        store
            .operation_progress(&repository.key)
            .is_some_and(|progress| progress.phase == "preparing")
    })
    .await;
    fixture
        .store
        .update(cx, |store, cx| store.refresh_checkouts(&repository.key, cx));
    checkout_catalog_ready(&fixture, cx).await;
    let progress = fixture.store.read_with(cx, |store, _| {
        store.operation_progress(&repository.key).unwrap()
    });
    assert_eq!(progress.operation_token, token);
    assert_eq!(progress.phase, "preparing");
    let events = std::fs::read_to_string(&events_path).unwrap();
    assert!(!events.contains("vcs/reconcileCheckoutOperation"));
    std::fs::write(events_path.with_extension("jsonl.release"), "release").unwrap();
    let result = apply.await.unwrap();
    assert_eq!(
        result.checkout.unwrap().id,
        "fixture-created:progress-checkout"
    );
    assert!(fixture.store.read_with(cx, |store, _| {
        store.operation_progress(&repository.key).is_none()
    }));
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn failed_checkout_preparation_retries_the_same_checkout(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new(
        "failed-checkout",
        fixture.directory.path(),
        "checkout-preparation-failed",
    )
    .register(cx);
    let repository = ready(&fixture, cx).await;
    checkout_catalog_ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_checkout_operation(
                repository.key.clone(),
                CheckoutOperation::Create {
                    name: "retained".into(),
                    branch: "topic".into(),
                    base: "main".into(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    let created = fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_checkout_operation(prepared, cx)
        })
        .await
        .unwrap()
        .checkout
        .unwrap();
    assert_eq!(created.preparation, vcs::CheckoutPreparation::Failed);
    checkout_catalog_ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_checkout_operation(
                repository.key.clone(),
                CheckoutOperation::Prepare {
                    checkout_id: created.id.clone(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    let retried = fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_checkout_operation(prepared, cx)
        })
        .await
        .unwrap()
        .checkout
        .unwrap();
    assert_eq!(retried.id, created.id);
    assert_eq!(retried.root_uri, created.root_uri);
    assert_eq!(retried.preparation, vcs::CheckoutPreparation::Ready);
    let events =
        std::fs::read_to_string(fixture.directory.path().join("failed-checkout.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(
                |line| line.contains("\"event\": \"checkout-operation-applied\"")
                    && line.contains("\"operation\": \"create\"")
            )
            .count(),
        1
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn checkout_intent_blocks_both_source_and_target_roots(cx: &mut TestAppContext) {
    let fs = fs::FakeFs::new(cx.background_executor.clone());
    let journal = OperationJournal::new(fs, "/journal/checkout.json".into());
    journal.load().await.unwrap();
    let source = FileOperationScope {
        extension_id: "fixture".into(),
        provider_id: "fixture".into(),
        repository_id: "source".into(),
        root_uri: "file:///source".into(),
        scope_uri: "file:///source/project".into(),
    };
    let target = FileOperationScope {
        repository_id: "target".into(),
        root_uri: "file:///target".into(),
        scope_uri: "file:///target/project".into(),
        ..source.clone()
    };
    let checkout = PendingOperation {
        token: "checkout-token".into(),
        request: OperationRequest::Checkout {
            operation: CheckoutOperation::Create {
                name: "target".into(),
                branch: "topic".into(),
                base: "main".into(),
            },
            target_root_uri: "file://localhost/target/".into(),
            checkout: None,
        },
    };
    let claim = journal
        .record(source.clone(), checkout.clone())
        .await
        .unwrap();
    assert!(journal.blocking(&source, false).unwrap().is_some());
    assert!(journal.blocking(&target, false).unwrap().is_some());
    assert!(
        journal
            .claim_recovery(&source, "checkout-token")
            .unwrap()
            .is_none()
    );
    let file = PendingFileOperation {
        path: "file.txt".into(),
        operation: FileOperation::Stage,
        token: "file-token".into(),
    };
    assert!(journal.record(target.clone(), file.clone()).await.is_err());
    drop(claim);
    journal.clear(&source, "checkout-token").await.unwrap();
    let claim = journal.record(target, file).await.unwrap();
    assert!(journal.record(source, checkout).await.is_err());
    drop(claim);
}

async fn populate_file_kind_fixture(fixture: &Fixture) {
    for path in [
        "new.txt",
        "renamed.txt",
        "old.txt",
        "type-change.txt",
        "executable.sh",
        "disabled.txt",
        "neighbor.txt",
    ] {
        fixture
            .fs
            .insert_file(fixture.directory.path().join(path), b"working\n".to_vec())
            .await;
    }
    for path in ["binary.bin", "binary-no-capabilities.bin"] {
        fixture
            .fs
            .insert_file(fixture.directory.path().join(path), vec![0, 255, 1])
            .await;
    }
    fixture
        .fs
        .create_dir(&fixture.directory.path().join("directory"))
        .await
        .unwrap();
    fixture
        .fs
        .insert_tree(
            fixture.directory.path().join("real-dir"),
            json!({"child.txt": "child", "old.txt": "old"}),
        )
        .await;
    fixture
        .fs
        .insert_symlink(
            fixture.directory.path().join("link"),
            "src/example.txt".into(),
        )
        .await;
    fixture
        .fs
        .insert_symlink(
            fixture.directory.path().join("linked-parent"),
            "real-dir".into(),
        )
        .await;
}

#[gpui::test]
async fn explicit_file_kinds_enable_only_the_advertised_rows_and_keep_parents_safe(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    populate_file_kind_fixture(&fixture).await;
    trust(&fixture, cx);
    FixtureExtension::new("file-kinds", fixture.directory.path(), "file-kinds").register(cx);
    let repository = ready(&fixture, cx).await;
    for path in ["disabled.txt", "binary-no-capabilities.bin"] {
        assert!(fixture.store.read_with(cx, |store, _| {
            store
                .file_operations_for_path(&repository.key, path)
                .is_empty()
        }));
        assert!(
            fixture
                .store
                .update(cx, |store, cx| store.prepare_file_operation(
                    repository.key.clone(),
                    path.into(),
                    FileOperation::Stage,
                    cx,
                ))
                .await
                .is_err()
        );
    }
    for path in [
        "new.txt",
        "deleted.txt",
        "binary.bin",
        "link",
        "renamed.txt",
        "executable.sh",
        "type-change.txt",
    ] {
        let prepared = fixture
            .store
            .update(cx, |store, cx| {
                store.prepare_file_operation(
                    repository.key.clone(),
                    path.into(),
                    FileOperation::Stage,
                    cx,
                )
            })
            .await
            .unwrap();
        if path == "renamed.txt" {
            assert_eq!(prepared.affected_paths(), ["renamed.txt", "old.txt"]);
        }
        assert!(prepared.summary().is_some());
        fixture
            .store
            .update(cx, |store, cx| {
                store.apply_prepared_file_operation(prepared, cx)
            })
            .await
            .unwrap();
    }
    for path in ["directory", "linked-parent/child.txt"] {
        assert!(
            fixture
                .store
                .update(cx, |store, cx| store.prepare_file_operation(
                    repository.key.clone(),
                    path.into(),
                    FileOperation::Stage,
                    cx,
                ))
                .await
                .is_err()
        );
    }
    assert_eq!(
        fixture
            .fs
            .load_bytes(&fixture.directory.path().join("binary.bin"))
            .await
            .unwrap(),
        [0, 255, 1]
    );
    assert_eq!(
        fixture
            .fs
            .load(&fixture.directory.path().join("src/example.txt"))
            .await
            .unwrap(),
        "working text\n"
    );
    let events =
        std::fs::read_to_string(fixture.directory.path().join("file-kinds.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"event\": \"file-operation-applied\""))
            .count(),
        7
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn file_preparation_refuses_widened_paths_incomplete_rename_and_ambiguous_discard(
    cx: &mut TestAppContext,
) {
    for (scenario, path, operation, reason) in [
        (
            "file-kinds-missing-summary",
            "new.txt",
            FileOperation::Discard,
            "summary",
        ),
        (
            "file-kinds-bad-targets",
            "binary.bin",
            FileOperation::Stage,
            "another file",
        ),
        (
            "file-kinds-missing-old-path",
            "renamed.txt",
            FileOperation::Stage,
            "both paths",
        ),
        (
            "file-kinds-old-parent",
            "renamed.txt",
            FileOperation::Stage,
            "symlink",
        ),
        (
            "file-kinds-old-directory",
            "renamed.txt",
            FileOperation::Stage,
            "directory",
        ),
    ] {
        let fixture = fixture(cx, false).await;
        populate_file_kind_fixture(&fixture).await;
        trust(&fixture, cx);
        FixtureExtension::new(scenario, fixture.directory.path(), scenario).register(cx);
        let repository = ready(&fixture, cx).await;
        let error = fixture
            .store
            .update(cx, |store, cx| {
                store.prepare_file_operation(repository.key.clone(), path.into(), operation, cx)
            })
            .await
            .err()
            .expect("unsafe preparation is refused");
        assert!(error.to_string().contains(reason), "{scenario}: {error:#}");
        let events =
            std::fs::read_to_string(fixture.directory.path().join(format!("{scenario}.jsonl")))
                .unwrap();
        assert!(!events.contains("vcs/applyFileOperation"));
        if scenario == "file-kinds-old-parent" || scenario == "file-kinds-old-directory" {
            assert!(!events.contains("vcs/prepareFileOperation"));
        }
        stop(&fixture, cx).await;
        cx.update(|cx| {
            ExtensionHostProxy::global(cx).unregister_vcs_provider(
                scenario.into(),
                "fixture".into(),
                cx,
            )
        });
    }
}

#[gpui::test]
async fn same_file_and_rename_source_buffers_in_another_project_block_mutation(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    populate_file_kind_fixture(&fixture).await;
    trust(&fixture, cx);
    FixtureExtension::new("shared-buffers", fixture.directory.path(), "file-kinds").register(cx);
    let repository = ready(&fixture, cx).await;
    let other = Project::test(fixture.fs.clone(), [fixture.directory.path()], cx).await;
    let buffer = other
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "unsaved in other project\n")], None, cx)
    });
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Discard,
                cx,
            )
        })
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("unsaved"));
    let unrelated = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation(
                repository.key.clone(),
                "new.txt".into(),
                FileOperation::Stage,
                cx,
            )
        })
        .await
        .unwrap();
    drop(unrelated);
    buffer.update(cx, |buffer, _| buffer.set_conflict());
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.prepare_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Stage,
                cx,
            ))
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("conflict")
    );
    let old = other
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("old.txt"), cx)
        })
        .await
        .unwrap();
    old.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "unsaved rename source\n")], None, cx)
    });
    assert!(
        fixture
            .store
            .update(cx, |store, cx| store.prepare_file_operation(
                repository.key.clone(),
                "renamed.txt".into(),
                FileOperation::Stage,
                cx,
            ))
            .await
            .is_err()
    );
    let events =
        std::fs::read_to_string(fixture.directory.path().join("shared-buffers.jsonl")).unwrap();
    assert!(!events.contains("vcs/applyFileOperation"));
    drop(other);
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn discard_preserves_a_buffer_edited_in_another_project_after_dispatch(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("other-discard", fixture.directory.path(), "discard-pending")
        .register(cx);
    let repository = ready(&fixture, cx).await;
    let other = Project::test(fixture.fs.clone(), [fixture.directory.path()], cx).await;
    let buffer = other
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let task = fixture.store.update(cx, |store, cx| {
        store.run_file_operation(
            repository.key.clone(),
            "src/example.txt".into(),
            FileOperation::Discard,
            cx,
        )
    });
    let events_path = fixture.directory.path().join("other-discard.jsonl");
    provider_event(events_path.clone(), "apply-held").await;
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "keep this new edit\n")], None, cx)
    });
    let edited = buffer.read_with(cx, |buffer, _| buffer.text());
    fixture
        .fs
        .insert_file(
            fixture.directory.path().join("src/example.txt"),
            b"restored\n".to_vec(),
        )
        .await;
    std::fs::write(events_path.with_extension("jsonl.release"), "release").unwrap();
    assert!(task.await.unwrap_err().to_string().contains("preserved"));
    assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), edited);
    assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    drop(other);
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn rename_old_path_metadata_is_rechecked_after_confirmation(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    populate_file_kind_fixture(&fixture).await;
    trust(&fixture, cx);
    FixtureExtension::new("rename-recheck", fixture.directory.path(), "file-kinds").register(cx);
    let repository = ready(&fixture, cx).await;
    let prepared = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation(
                repository.key.clone(),
                "renamed.txt".into(),
                FileOperation::Stage,
                cx,
            )
        })
        .await
        .unwrap();
    fixture
        .fs
        .remove_file(
            &fixture.directory.path().join("old.txt"),
            fs::RemoveOptions::default(),
        )
        .await
        .unwrap();
    fixture
        .fs
        .create_dir(&fixture.directory.path().join("old.txt"))
        .await
        .unwrap();
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.apply_prepared_file_operation(prepared, cx)
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("directory"));
    let events =
        std::fs::read_to_string(fixture.directory.path().join("rename-recheck.jsonl")).unwrap();
    assert!(!events.contains("vcs/applyFileOperation"));
    assert!(!fixture.store.read_with(cx, |store, _| {
        store.file_operation_unresolved(&repository.key)
    }));
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn overlapping_file_intents_in_parent_and_nested_scopes_are_blocked(cx: &mut TestAppContext) {
    let fs = fs::FakeFs::new(cx.background_executor.clone());
    let journal = OperationJournal::new(fs, "/journal/overlapping.json".into());
    journal.load().await.unwrap();
    let parent = FileOperationScope {
        extension_id: "fixture".into(),
        provider_id: "fixture".into(),
        repository_id: "parent".into(),
        root_uri: "file:///checkout".into(),
        scope_uri: "file:///checkout/project".into(),
    };
    let nested = FileOperationScope {
        repository_id: "nested".into(),
        scope_uri: "file:///checkout/project/nested".into(),
        ..parent.clone()
    };
    let rename = PendingOperation {
        token: "rename-token".into(),
        request: OperationRequest::File {
            path: "nested/new.txt".into(),
            operation: FileOperation::Stage,
            affected_paths: vec!["nested/new.txt".into(), "nested/old.txt".into()],
        },
    };
    let nested_file = PendingFileOperation {
        path: "old.txt".into(),
        operation: FileOperation::Unstage,
        token: "nested-token".into(),
    };
    let claim = journal
        .record(parent.clone(), rename.clone())
        .await
        .unwrap();
    assert!(
        journal
            .record(nested.clone(), nested_file.clone())
            .await
            .is_err()
    );
    drop(claim);
    assert!(
        journal
            .record(nested.clone(), nested_file.clone())
            .await
            .is_err()
    );
    let neighbor = PendingFileOperation {
        path: "neighbor.txt".into(),
        operation: FileOperation::Stage,
        token: "neighbor-token".into(),
    };
    drop(journal.record(nested.clone(), neighbor).await.unwrap());
    journal.clear(&parent, "rename-token").await.unwrap();
    journal.clear(&nested, "neighbor-token").await.unwrap();
    drop(journal.record(nested, nested_file).await.unwrap());
    assert!(journal.record(parent, rename).await.is_err());
}

#[gpui::test]
async fn explicitly_supported_staged_added_absence_allows_all_file_operations(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, false).await;
    populate_file_kind_fixture(&fixture).await;
    trust(&fixture, cx);
    FixtureExtension::new("staged-missing", fixture.directory.path(), "file-kinds").register(cx);
    let repository = ready(&fixture, cx).await;
    assert!(
        fixture
            .fs
            .metadata(&fixture.directory.path().join("staged-missing.txt"))
            .await
            .unwrap()
            .is_none()
    );
    for operation in [
        FileOperation::Stage,
        FileOperation::Unstage,
        FileOperation::Discard,
    ] {
        fixture
            .store
            .update(cx, |store, cx| {
                store.run_file_operation(
                    repository.key.clone(),
                    "staged-missing.txt".into(),
                    operation,
                    cx,
                )
            })
            .await
            .unwrap();
    }
    fixture
        .fs
        .remove_file(
            &fixture.directory.path().join("src/example.txt"),
            fs::RemoveOptions::default(),
        )
        .await
        .unwrap();
    let error = fixture
        .store
        .update(cx, |store, cx| {
            store.prepare_file_operation(
                repository.key.clone(),
                "src/example.txt".into(),
                FileOperation::Stage,
                cx,
            )
        })
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("no longer exists"));
    let events =
        std::fs::read_to_string(fixture.directory.path().join("staged-missing.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("\"event\": \"file-operation-applied\""))
            .count(),
        3
    );
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn explicit_symlink_directory_leaf_is_allowed_without_following_parents(
    cx: &mut TestAppContext,
) {
    let fs = fs::FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/scope",
        json!({"target-directory": {"kept.txt": "untouched"}, "ordinary-directory": {}}),
    )
    .await;
    fs.insert_symlink("/scope/link", PathBuf::from("/scope/target-directory"))
        .await;
    let metadata = fs
        .metadata(Path::new("/scope/link"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        metadata.is_symlink && metadata.is_dir,
        "must reproduce RealFs symlink-to-directory metadata"
    );
    let mut change: vcs::Change = serde_json::from_value(json!({
        "path": "link", "status": "modified", "kind": "symlink",
        "oldMode": 40960, "newMode": 40960, "fileOperations": ["stage", "unstage", "discard"]
    }))
    .unwrap();
    let fs: Arc<dyn Fs> = fs;
    validate_file_metadata(Path::new("/scope"), &change, &fs)
        .await
        .unwrap();
    change.path = "link/kept.txt".into();
    change.kind = vcs::ContentKind::File;
    assert!(
        validate_file_metadata(Path::new("/scope"), &change, &fs)
            .await
            .is_err()
    );
    change.path = "ordinary-directory".into();
    assert!(
        validate_file_metadata(Path::new("/scope"), &change, &fs)
            .await
            .is_err()
    );
    assert_eq!(
        fs.load(Path::new("/scope/target-directory/kept.txt"))
            .await
            .unwrap(),
        "untouched"
    );
}

#[gpui::test]
async fn provider_blame_cache_tracks_head_independently_of_baseline_and_preserves_git(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, true).await;
    trust(&fixture, cx);
    FixtureExtension::new("blame-cache", fixture.directory.path(), "blame").register(cx);
    let repository = ready(&fixture, cx).await;
    fixture
        .project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let nested = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("nested/file.txt"), cx)
        })
        .await
        .unwrap();
    assert!(
        fixture
            .project
            .read_with(cx, |project, cx| project.has_provider_blame(&buffer, cx))
    );
    assert!(
        !fixture
            .project
            .read_with(cx, |project, cx| project.has_provider_blame(&nested, cx))
    );
    assert!(
        fixture
            .project
            .update(cx, |project, cx| project.blame_provider_buffer(&nested, cx))
            .await
            .unwrap()
            .is_none()
    );
    let first = fixture
        .project
        .update(cx, |project, cx| project.blame_provider_buffer(&buffer, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.revision_id, "revision:42");
    fixture.store.update(cx, |store, _| {
        let cached = &mut store.repositories.get_mut(&repository.key).unwrap().blames;
        for index in 0..15 {
            cached.insert(format!("other-{index}.txt"), first.clone());
        }
    });
    fixture
        .store
        .update(cx, |store, cx| {
            store.set_baseline(&repository.key, "previous".into(), cx)
        })
        .unwrap();
    let current = ready(&fixture, cx).await;
    assert_eq!(current.snapshot.unwrap().revision_id, "r41");
    let cached = fixture
        .project
        .update(cx, |project, cx| project.blame_provider_buffer(&buffer, cx))
        .await
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&first, &cached));
    fixture.store.read_with(cx, |store, _| {
        assert_eq!(
            store
                .repositories
                .get(&repository.key)
                .unwrap()
                .blames
                .len(),
            16
        );
    });
    let events = fixture.directory.path().join("blame-cache.jsonl");
    let calls = || {
        std::fs::read_to_string(&events)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|event| event["method"] == "vcs/blame")
            .count()
    };
    assert_eq!(calls(), 1);
    std::fs::write(events.with_extension("jsonl.revision"), "revision:43").unwrap();
    fixture
        .store
        .update(cx, |store, cx| store.refresh(&repository.key, cx));
    ready(&fixture, cx).await;
    let refreshed = fixture
        .project
        .update(cx, |project, cx| project.blame_provider_buffer(&buffer, cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.revision_id, "revision:43");
    assert!(!Arc::ptr_eq(&first, &refreshed));
    assert_eq!(calls(), 2);
    stop(&fixture, cx).await;
    assert!(
        !fixture
            .project
            .read_with(cx, |project, cx| project.has_provider_blame(&buffer, cx))
    );
}

#[gpui::test]
async fn provider_blame_rejects_response_from_another_head(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("blame-stale", fixture.directory.path(), "blame-stale").register(cx);
    ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let error = fixture
        .project
        .update(cx, |project, cx| project.blame_provider_buffer(&buffer, cx))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("HEAD changed"));
    fixture.store.read_with(cx, |store, _| {
        assert!(
            store
                .repositories
                .values()
                .all(|repository| repository.blames.is_empty())
        )
    });
    stop(&fixture, cx).await;
}

#[gpui::test]
async fn provider_blame_fences_in_flight_results_after_head_invalidation(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false).await;
    trust(&fixture, cx);
    FixtureExtension::new("blame-pending", fixture.directory.path(), "blame-pending").register(cx);
    let repository = ready(&fixture, cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(fixture.directory.path().join("src/example.txt"), cx)
        })
        .await
        .unwrap();
    let pending = fixture
        .project
        .update(cx, |project, cx| project.blame_provider_buffer(&buffer, cx));
    let events = fixture.directory.path().join("blame-pending.jsonl");
    provider_event(events.clone(), "blame-held").await;
    fixture.store.update(cx, |store, cx| {
        store.set_blame_head(&repository.key, None, cx);
        store.set_blame_head(&repository.key, Some("revision:42".into()), cx);
    });
    std::fs::write(events.with_extension("jsonl.release"), "ready").unwrap();
    let error = pending.await.unwrap_err();
    assert!(error.to_string().contains("stale"));
    fixture.store.read_with(cx, |store, _| {
        assert!(
            store
                .repositories
                .values()
                .all(|repository| repository.blames.is_empty())
        )
    });
    stop(&fixture, cx).await;
}
