#![cfg(unix)]

use crate::{
    Content, FileOperation, FileOperationOutcome, MAX_OPERATION_TOKEN_BYTES, ProviderCommand,
    ProviderConnection, ProviderError, ProviderNotification, RepositoryOperation,
};
use futures::future::{Either, select};
use gpui::TestAppContext;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

struct Fixture {
    connection: Arc<ProviderConnection>,
    directory: tempfile::TempDir,
    events: PathBuf,
}

#[gpui::test]
async fn blame_requires_capability_and_rejects_malformed_results(cx: &mut TestAppContext) {
    let unsupported = fixture("normal", cx);
    bounded(unsupported.connection.initialize(), cx)
        .await
        .unwrap();
    assert!(
        unsupported
            .connection
            .blame("fixture-repository".into(), "src/example.txt".into())
            .await
            .is_err()
    );
    assert!(
        !events(&unsupported)
            .iter()
            .any(|event| event["method"] == "vcs/blame")
    );
    bounded(unsupported.connection.shutdown(), cx)
        .await
        .unwrap();

    let supported = fixture("blame", cx);
    let initialized = bounded(supported.connection.initialize(), cx)
        .await
        .unwrap();
    assert!(initialized.capabilities.read_only && initialized.capabilities.blame);
    let blame = bounded(
        supported
            .connection
            .blame("fixture-repository".into(), "src/example.txt".into()),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(blame.revision_id, "revision:42");
    assert_eq!(blame.entries[0].commit_id, "revision:original");
    assert_eq!(
        blame.content,
        "first committed line\nsecond committed line\n"
    );
    bounded(supported.connection.shutdown(), cx).await.unwrap();

    let malformed = fixture("blame-malformed", cx);
    bounded(malformed.connection.initialize(), cx)
        .await
        .unwrap();
    assert!(
        bounded(
            malformed
                .connection
                .blame("fixture-repository".into(), "src/example.txt".into()),
            cx
        )
        .await
        .is_err()
    );
    bounded(malformed.connection.shutdown(), cx).await.unwrap();

    let missing_head = fixture("blame-missing-head", cx);
    bounded(missing_head.connection.initialize(), cx)
        .await
        .unwrap();
    assert!(
        bounded(
            missing_head
                .connection
                .snapshot("fixture-repository".into(), "submitted".into()),
            cx
        )
        .await
        .is_err()
    );
    bounded(missing_head.connection.shutdown(), cx)
        .await
        .unwrap();
}

#[gpui::test]
async fn repository_operations_require_capability_and_bind_the_prepared_request(
    cx: &mut TestAppContext,
) {
    let read_only = fixture("normal", cx);
    bounded(read_only.connection.initialize(), cx)
        .await
        .unwrap();
    let operation = RepositoryOperation::Commit {
        message: "Commit staged changes".into(),
    };
    assert!(
        read_only
            .connection
            .prepare_repository_operation("fixture-repository".into(), operation.clone())
            .await
            .is_err()
    );
    assert!(
        !events(&read_only)
            .iter()
            .any(|event| event["method"] == "vcs/prepareRepositoryOperation")
    );
    bounded(read_only.connection.shutdown(), cx).await.unwrap();

    let fixture = fixture("repository-operations", cx);
    bounded(fixture.connection.initialize(), cx).await.unwrap();
    let state = fixture
        .connection
        .repository_state("fixture-repository".into())
        .await
        .unwrap();
    assert_eq!(state.staged_paths, ["src/example.txt"]);
    let prepared = fixture
        .connection
        .prepare_repository_operation("fixture-repository".into(), operation.clone())
        .await
        .unwrap();
    let changed = RepositoryOperation::Commit {
        message: "A different request".into(),
    };
    assert!(
        fixture
            .connection
            .apply_repository_operation(
                "fixture-repository".into(),
                changed,
                prepared.operation_token.clone(),
            )
            .await
            .is_err()
    );
    assert!(
        !events(&fixture)
            .iter()
            .any(|event| event["event"] == "repository-operation-applied")
    );
    assert_eq!(
        fixture
            .connection
            .repository_state("fixture-repository".into())
            .await
            .unwrap()
            .staged_paths,
        ["src/example.txt"]
    );
    let result = fixture
        .connection
        .apply_repository_operation(
            "fixture-repository".into(),
            operation.clone(),
            prepared.operation_token.clone(),
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, FileOperationOutcome::Applied);
    assert_token_consumed(
        fixture
            .connection
            .apply_repository_operation(
                "fixture-repository".into(),
                operation.clone(),
                prepared.operation_token.clone(),
            )
            .await
            .unwrap_err(),
    );
    let changed_after_completion = RepositoryOperation::Commit {
        message: "A different request after completion".into(),
    };
    assert!(
        fixture
            .connection
            .apply_repository_operation(
                "fixture-repository".into(),
                changed_after_completion.clone(),
                prepared.operation_token.clone(),
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .connection
            .reconcile_repository_operation(
                "fixture-repository".into(),
                changed_after_completion,
                prepared.operation_token.clone(),
            )
            .await
            .is_err()
    );
    let reconciled = fixture
        .connection
        .reconcile_repository_operation(
            "fixture-repository".into(),
            operation,
            prepared.operation_token,
        )
        .await
        .unwrap();
    assert_eq!(reconciled.outcome, FileOperationOutcome::Applied);
    assert!(
        fixture
            .connection
            .repository_state("fixture-repository".into())
            .await
            .unwrap()
            .staged_paths
            .is_empty()
    );
    assert_eq!(
        events(&fixture)
            .iter()
            .filter(|event| event["event"] == "repository-operation-applied")
            .count(),
        1
    );
    bounded(fixture.connection.shutdown(), cx).await.unwrap();
}

#[gpui::test]
async fn branch_operation_rejects_a_changed_checkout(cx: &mut TestAppContext) {
    let fixture = fixture("repository-clean", cx);
    bounded(fixture.connection.initialize(), cx).await.unwrap();
    let operation = RepositoryOperation::SwitchBranch {
        branch_id: "topic".into(),
    };
    let prepared = fixture
        .connection
        .prepare_repository_operation("fixture-repository".into(), operation.clone())
        .await
        .unwrap();
    std::fs::write(
        fixture.events.with_extension("jsonl.state"),
        "concurrent state",
    )
    .unwrap();
    let result = fixture
        .connection
        .apply_repository_operation(
            "fixture-repository".into(),
            operation.clone(),
            prepared.operation_token,
        )
        .await
        .unwrap();
    assert_eq!(result.outcome, FileOperationOutcome::Conflict);
    let prepared = fixture
        .connection
        .prepare_repository_operation("fixture-repository".into(), operation.clone())
        .await
        .unwrap();
    fixture
        .connection
        .apply_repository_operation(
            "fixture-repository".into(),
            operation,
            prepared.operation_token,
        )
        .await
        .unwrap();
    let state = fixture
        .connection
        .repository_state("fixture-repository".into())
        .await
        .unwrap();
    assert_eq!(state.current_branch.unwrap().id, "topic");
    bounded(fixture.connection.shutdown(), cx).await.unwrap();
}

fn fixture(scenario: &str, cx: &TestAppContext) -> Fixture {
    cx.executor().allow_parking();
    let directory = tempfile::tempdir().expect("fixture directory");
    let events = directory.path().join("events.jsonl");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixture_provider.py");
    let command = ProviderCommand {
        path: std::env::var_os("VCS_FIXTURE_PYTHON")
            .unwrap_or_else(|| OsString::from("python3"))
            .into(),
        arguments: vec![
            script.into_os_string(),
            "--scenario".into(),
            scenario.into(),
            "--events".into(),
            events.clone().into_os_string(),
        ],
        env: Default::default(),
    };
    let connection =
        ProviderConnection::start(command, directory.path(), cx.background_executor.clone())
            .expect("start fixture provider");
    Fixture {
        connection,
        directory,
        events,
    }
}

async fn bounded<T>(future: impl Future<Output = T>, cx: &TestAppContext) -> T {
    let timeout = cx.background_executor.timer(Duration::from_secs(5));
    futures::pin_mut!(future, timeout);
    match select(future, timeout).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => panic!("timed out during VCS fixture exchange"),
    }
}

fn events(fixture: &Fixture) -> Vec<Value> {
    read_events(&fixture.events)
}

fn read_events(path: &Path) -> Vec<Value> {
    let contents = std::fs::read_to_string(path).expect("fixture event log");
    let complete = contents.rfind('\n').map_or("", |end| &contents[..end]);
    complete
        .lines()
        .map(|line| serde_json::from_str(line).expect("fixture event"))
        .collect()
}

async fn provider_event(path: PathBuf, event: &'static str) {
    // The fixture writes this receipt outside GPUI's simulated clock.
    smol::unblock(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !read_events(&path)
            .iter()
            .any(|record| record["event"] == event)
        {
            assert!(Instant::now() < deadline, "fixture event {event} timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    })
    .await;
}

async fn shutdown_child_taken(connection: Arc<ProviderConnection>) {
    smol::unblock(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !connection.has_taken_shutdown_child() {
            assert!(Instant::now() < deadline, "shutdown did not take its child");
            std::thread::sleep(Duration::from_millis(5));
        }
    })
    .await;
}

#[gpui::test]
async fn stdio_roundtrip_uses_opaque_revisions_and_explicit_content_kinds(cx: &mut TestAppContext) {
    let fixture = fixture("normal", cx);
    let initialized = bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize");
    assert!(initialized.capabilities.read_only);
    let scope = url::Url::from_directory_path(fixture.directory.path())
        .expect("scope URI")
        .to_string();
    let repositories = bounded(fixture.connection.discover(scope), cx)
        .await
        .expect("discover");
    let repository = repositories.first().expect("fixture repository");
    let snapshot = bounded(
        fixture
            .connection
            .snapshot(repository.id.clone(), "submitted".into()),
        cx,
    )
    .await
    .expect("snapshot");
    assert_eq!(snapshot.revision_id, "r42");
    assert_eq!(
        snapshot.changes.first().expect("change").new_mode,
        Some(0o100644)
    );
    for (path, expected) in [
        (
            "src/example.txt",
            Content::Text {
                text: "baseline — r42\n".into(),
            },
        ),
        ("absent.txt", Content::Absent),
        ("binary.bin", Content::Binary),
        (
            "link",
            Content::Symlink {
                target: "../target".into(),
            },
        ),
    ] {
        let content = bounded(
            fixture.connection.read_content(
                repository.id.clone(),
                snapshot.id.clone(),
                "submitted".into(),
                path.into(),
            ),
            cx,
        )
        .await
        .expect("baseline content");
        assert_eq!(content, expected);
    }
    bounded(fixture.connection.shutdown(), cx)
        .await
        .expect("shutdown");
    assert!(
        events(&fixture)
            .iter()
            .any(|event| event["event"] == "exit-received")
    );
    assert!(!fixture.directory.path().join(".git").exists());
}

#[gpui::test]
async fn completed_reply_survives_immediate_provider_eof(cx: &mut TestAppContext) {
    let fixture = fixture("reply-eof", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize");
    let notifications = fixture.connection.notifications();
    let content = bounded(
        fixture.connection.read_content(
            "fixture-repository".into(),
            "snapshot:7".into(),
            "submitted".into(),
            "src/example.txt".into(),
        ),
        cx,
    )
    .await
    .expect("complete reply before EOF");
    assert_eq!(
        content,
        Content::Text {
            text: "baseline — r42\n".repeat(8192)
        }
    );
    assert!(matches!(
        bounded(notifications.recv(), cx)
            .await
            .expect("exit notification"),
        ProviderNotification::Exited { .. }
    ));
    assert!(!fixture.connection.is_alive());
}

#[gpui::test]
async fn provider_crash_rejects_all_pending_requests(cx: &mut TestAppContext) {
    let fixture = fixture("crash", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize");
    let requests = async {
        futures::join!(
            fixture
                .connection
                .snapshot("fixture-repository".into(), "submitted".into()),
            fixture
                .connection
                .snapshot("fixture-repository".into(), "submitted".into()),
        )
    };
    let (first, second) = bounded(requests, cx).await;
    assert!(first.is_err());
    assert!(second.is_err());
    assert!(!fixture.connection.is_alive());
}

#[gpui::test]
async fn typed_provider_errors_retain_their_data(cx: &mut TestAppContext) {
    let fixture = fixture("error", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize");
    let error = bounded(
        fixture
            .connection
            .snapshot("fixture-repository".into(), "submitted".into()),
        cx,
    )
    .await
    .expect_err("typed provider rejection");
    let error = error
        .downcast_ref::<ProviderError>()
        .expect("preserved protocol error");
    assert_eq!(error.code, -32044);
    assert_eq!(error.message, "Snapshot needs refresh");
    assert_eq!(
        error.data,
        Some(json!({"kind":"stale","revision":"r42","retryable":true}))
    );
    assert!(fixture.connection.is_alive());
    bounded(fixture.connection.shutdown(), cx)
        .await
        .expect("shutdown");
}

#[gpui::test]
async fn dropping_a_request_emits_cancellation_without_closing_the_connection(
    cx: &mut TestAppContext,
) {
    let fixture = fixture("pending", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize");
    let notifications = fixture.connection.notifications();
    let request = cx.background_executor.spawn({
        let connection = fixture.connection.clone();
        async move {
            connection
                .snapshot("fixture-repository".into(), "submitted".into())
                .await
        }
    });
    assert!(matches!(
        bounded(notifications.recv(), cx)
            .await
            .expect("request received"),
        ProviderNotification::DidChange { .. }
    ));
    drop(request);
    assert!(matches!(
        bounded(notifications.recv(), cx)
            .await
            .expect("cancel received"),
        ProviderNotification::DidChange { .. }
    ));
    let recorded = events(&fixture);
    let pending = recorded
        .iter()
        .find(|event| event["event"] == "request-pending")
        .expect("pending request");
    let cancelled = recorded
        .iter()
        .find(|event| event["event"] == "cancel-received")
        .expect("cancel notification");
    assert_eq!(pending["id"], cancelled["id"]);
    assert!(fixture.connection.is_alive());
    bounded(fixture.connection.shutdown(), cx)
        .await
        .expect("shutdown");
}

#[gpui::test]
async fn invalid_or_oversized_frames_fail_the_connection(cx: &mut TestAppContext) {
    for scenario in ["malformed", "oversized"] {
        let fixture = fixture(scenario, cx);
        bounded(fixture.connection.initialize(), cx)
            .await
            .expect("initialize");
        assert!(
            bounded(
                fixture
                    .connection
                    .snapshot("fixture-repository".into(), "submitted".into()),
                cx
            )
            .await
            .is_err()
        );
        assert!(!fixture.connection.is_alive());
    }
}

#[gpui::test]
async fn stderr_flood_is_drained_and_retains_only_a_bounded_utf8_tail(cx: &mut TestAppContext) {
    let fixture = fixture("stderr-flood", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize despite stderr flood");
    bounded(
        async {
            while !fixture
                .connection
                .diagnostics()
                .ends_with("fixture-stderr-tail\n")
            {
                cx.background_executor
                    .timer(Duration::from_millis(10))
                    .await;
            }
        },
        cx,
    )
    .await;
    let diagnostics = fixture.connection.diagnostics();
    assert!(diagnostics.len() <= 64 * 1024);
    assert!(!diagnostics.contains("discarded-prefix"));
    bounded(fixture.connection.shutdown(), cx)
        .await
        .expect("shutdown");
}

async fn process_is_running(pid: u64) -> bool {
    let output = util::command::new_command("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .await
        .expect("inspect fixture process");
    assert!(output.stderr.is_empty(), "process status query failed");
    let state = String::from_utf8_lossy(&output.stdout);
    !state.trim().is_empty() && !state.trim().starts_with('Z')
}

fn fixture_process_ids(fixture: &Fixture) -> Vec<u64> {
    let recorded = events(fixture);
    ["provider-started", "descendant-started"]
        .into_iter()
        .map(|name| {
            recorded
                .iter()
                .find(|event| event["event"] == name)
                .and_then(|event| event["pid"].as_u64())
                .expect("fixture process id")
        })
        .collect()
}

async fn processes_exit(ids: Vec<u64>) {
    smol::unblock(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while smol::block_on(futures::future::join_all(
            ids.iter().map(|pid| process_is_running(*pid)),
        ))
        .into_iter()
        .any(|running| running)
        {
            assert!(Instant::now() < deadline, "fixture processes did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
    })
    .await;
}

#[gpui::test]
async fn dropping_connection_stops_provider_and_its_descendant(cx: &mut TestAppContext) {
    let fixture = fixture("descendant", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize descendant fixture");
    let ids = fixture_process_ids(&fixture);
    for pid in &ids {
        assert!(process_is_running(*pid).await);
    }
    drop(fixture.connection);
    processes_exit(ids).await;
}

#[gpui::test]
async fn cancelled_shutdown_waiter_cleans_the_taken_child_and_its_group(cx: &mut TestAppContext) {
    let fixture = fixture("hang-after-exit", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize shutdown fixture");
    let ids = fixture_process_ids(&fixture);
    let shutdown = cx.background_executor.spawn({
        let connection = fixture.connection.clone();
        async move { connection.shutdown().await }
    });
    provider_event(fixture.events.clone(), "exit-received").await;
    shutdown_child_taken(fixture.connection.clone()).await;
    assert!(
        events(&fixture)
            .iter()
            .any(|event| event["event"] == "shutdown-received")
    );
    for pid in &ids {
        assert!(process_is_running(*pid).await);
    }
    drop(shutdown);
    drop(fixture.connection);
    processes_exit(ids).await;
}

#[gpui::test]
async fn natural_parent_exit_still_cleans_descendants_holding_stdout(cx: &mut TestAppContext) {
    let fixture = fixture("exit-leaves-descendant", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize surviving-descendant fixture");
    let ids = fixture_process_ids(&fixture);
    for pid in &ids {
        assert!(process_is_running(*pid).await);
    }
    bounded(fixture.connection.shutdown(), cx)
        .await
        .expect("shutdown cleans the group after its leader exits");
    assert!(
        events(&fixture)
            .iter()
            .any(|event| event["event"] == "exit-received")
    );
    processes_exit(ids).await;
}

#[gpui::test]
async fn exit_before_request_registration_never_leaves_initialization_pending(
    cx: &mut TestAppContext,
) {
    for _ in 0..4 {
        let fixture = fixture("early-exit", cx);
        assert!(bounded(fixture.connection.initialize(), cx).await.is_err());
        assert!(!fixture.connection.is_alive());
    }
}

#[gpui::test]
async fn file_operations_are_denied_before_dispatch_without_the_capability(
    cx: &mut TestAppContext,
) {
    for scenario in [
        "normal",
        "v2-read-only",
        "v2-no-file-operations",
        "v2-stage-only",
    ] {
        let fixture = fixture(scenario, cx);
        assert!(fixture.connection.capabilities().is_none());
        assert!(
            bounded(
                fixture.connection.prepare_file_operation(
                    "fixture-repository".into(),
                    "src/example.txt".into(),
                    FileOperation::Stage,
                ),
                cx,
            )
            .await
            .is_err()
        );
        let initialized = bounded(fixture.connection.initialize(), cx)
            .await
            .expect("initialize");
        let capability = fixture
            .connection
            .capabilities()
            .expect("cached capabilities");
        assert_eq!(capability.read_only, initialized.capabilities.read_only);
        assert_eq!(
            capability.file_operations,
            initialized.capabilities.file_operations
        );
        assert_eq!(
            initialized.protocol_version,
            if scenario == "normal" { 1 } else { 2 }
        );
        for operation in [
            FileOperation::Stage,
            FileOperation::Unstage,
            FileOperation::Discard,
        ] {
            if scenario == "v2-stage-only" && operation == FileOperation::Stage {
                continue;
            }
            assert!(
                bounded(
                    fixture.connection.prepare_file_operation(
                        "fixture-repository".into(),
                        "src/example.txt".into(),
                        operation,
                    ),
                    cx,
                )
                .await
                .is_err()
            );
            assert!(
                bounded(
                    fixture.connection.apply_file_operation(
                        "fixture-repository".into(),
                        "src/example.txt".into(),
                        operation,
                        "unsupported-token".into(),
                    ),
                    cx,
                )
                .await
                .is_err()
            );
            assert!(
                bounded(
                    fixture.connection.reconcile_file_operation(
                        "fixture-repository".into(),
                        "src/example.txt".into(),
                        operation,
                        "unsupported-token".into(),
                    ),
                    cx,
                )
                .await
                .is_err()
            );
        }
        bounded(
            fixture
                .connection
                .snapshot("fixture-repository".into(), "submitted".into()),
            cx,
        )
        .await
        .expect("read-only operations remain usable");
        bounded(fixture.connection.shutdown(), cx)
            .await
            .expect("shutdown");
        let recorded = events(&fixture);
        let initialize = recorded
            .iter()
            .find(|event| event["event"] == "initialize-received")
            .expect("initialization receipt");
        assert_eq!(initialize["protocol_version"], 1);
        assert_eq!(initialize["supported_protocol_versions"], json!([1, 2]));
        assert!(
            !recorded.iter().any(|event| {
                event["method"]
                    .as_str()
                    .is_some_and(|method| method.ends_with("FileOperation"))
            }),
            "denied operation reached fixture in {scenario}"
        );
    }
}

#[gpui::test]
async fn contradictory_file_capabilities_do_not_initialize(cx: &mut TestAppContext) {
    for scenario in ["v1-writable", "v2-contradictory"] {
        let fixture = fixture(scenario, cx);
        assert!(bounded(fixture.connection.initialize(), cx).await.is_err());
        assert!(fixture.connection.capabilities().is_none());
        assert!(
            bounded(
                fixture.connection.prepare_file_operation(
                    "fixture-repository".into(),
                    "src/example.txt".into(),
                    FileOperation::Stage,
                ),
                cx,
            )
            .await
            .is_err()
        );
        bounded(fixture.connection.shutdown(), cx)
            .await
            .expect("shutdown");
        assert!(!events(&fixture).iter().any(|event| {
            event["method"]
                .as_str()
                .is_some_and(|method| method.ends_with("FileOperation"))
        }));
    }
}

#[gpui::test]
async fn explicit_file_operations_are_scoped_idempotent_and_reconcilable(cx: &mut TestAppContext) {
    let fixture = fixture("writable", cx);
    let initialized = bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize");
    assert_eq!(initialized.protocol_version, 2);
    assert!(!initialized.capabilities.read_only);
    let prepared = bounded(
        fixture.connection.prepare_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Stage,
        ),
        cx,
    )
    .await
    .expect("prepare stage");
    let reconciled = bounded(
        fixture.connection.reconcile_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Stage,
            prepared.operation_token.clone(),
        ),
        cx,
    )
    .await
    .expect("reconcile before apply");
    assert_eq!(reconciled.outcome, FileOperationOutcome::NotApplied);
    let consumed_token = prepared.operation_token;
    assert_token_consumed(
        bounded(
            fixture.connection.apply_file_operation(
                "fixture-repository".into(),
                "src/example.txt".into(),
                FileOperation::Stage,
                consumed_token.clone(),
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );
    let prepared = bounded(
        fixture.connection.prepare_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Stage,
        ),
        cx,
    )
    .await
    .expect("fresh preparation after terminal reconciliation");
    assert_ne!(prepared.operation_token, consumed_token);
    let sibling = bounded(
        fixture.connection.prepare_file_operation(
            "fixture-repository".into(),
            "src/sibling.txt".into(),
            FileOperation::Stage,
        ),
        cx,
    )
    .await
    .expect("prepare sibling stage");
    let sibling_result = bounded(
        fixture.connection.apply_file_operation(
            "fixture-repository".into(),
            "src/sibling.txt".into(),
            FileOperation::Stage,
            sibling.operation_token,
        ),
        cx,
    )
    .await
    .expect("stage sibling");
    assert_eq!(sibling_result.outcome, FileOperationOutcome::Applied);
    let applied = bounded(
        fixture.connection.apply_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Stage,
            prepared.operation_token.clone(),
        ),
        cx,
    )
    .await
    .expect("apply fresh token");
    assert_eq!(applied.outcome, FileOperationOutcome::Applied);
    assert_token_consumed(
        bounded(
            fixture.connection.apply_file_operation(
                "fixture-repository".into(),
                "src/example.txt".into(),
                FileOperation::Stage,
                prepared.operation_token.clone(),
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );
    let unstage = bounded(
        fixture.connection.prepare_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Unstage,
        ),
        cx,
    )
    .await
    .expect("prepare unstage");
    let applied = bounded(
        fixture.connection.apply_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Unstage,
            unstage.operation_token,
        ),
        cx,
    )
    .await
    .expect("unstage selected file");
    assert_eq!(applied.outcome, FileOperationOutcome::Applied);
    assert_token_consumed(
        bounded(
            fixture.connection.apply_file_operation(
                "fixture-repository".into(),
                "src/example.txt".into(),
                FileOperation::Stage,
                prepared.operation_token,
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );
    bounded(fixture.connection.shutdown(), cx)
        .await
        .expect("shutdown");
    let recorded = events(&fixture);
    let applied: Vec<_> = recorded
        .iter()
        .filter(|event| event["event"] == "file-operation-applied")
        .map(|event| {
            (
                event["path"].clone(),
                event["operation"].clone(),
                event["staged"].clone(),
            )
        })
        .collect();
    assert_eq!(
        applied,
        vec![
            (json!("src/sibling.txt"), json!("stage"), json!(true)),
            (json!("src/example.txt"), json!("stage"), json!(true)),
            (json!("src/example.txt"), json!("unstage"), json!(false)),
        ]
    );
}

#[gpui::test]
async fn stale_file_operation_token_is_a_conflict_without_a_mutation(cx: &mut TestAppContext) {
    let fixture = fixture("writable", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize");
    let prepared = bounded(
        fixture.connection.prepare_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Stage,
        ),
        cx,
    )
    .await
    .expect("prepare stage");
    std::fs::write(
        format!("{}.state", fixture.events.display()),
        "concurrent edit",
    )
    .expect("change fixture state");
    let applied = bounded(
        fixture.connection.apply_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Stage,
            prepared.operation_token.clone(),
        ),
        cx,
    )
    .await
    .expect("report conflict");
    assert_eq!(applied.outcome, FileOperationOutcome::Conflict);
    let reconciled = bounded(
        fixture.connection.reconcile_file_operation(
            "fixture-repository".into(),
            "src/example.txt".into(),
            FileOperation::Stage,
            prepared.operation_token.clone(),
        ),
        cx,
    )
    .await
    .expect("reconcile conflict");
    assert_eq!(reconciled.outcome, FileOperationOutcome::Conflict);
    std::fs::write(format!("{}.state", fixture.events.display()), "").unwrap();
    assert_token_consumed(
        bounded(
            fixture.connection.apply_file_operation(
                "fixture-repository".into(),
                "src/example.txt".into(),
                FileOperation::Stage,
                prepared.operation_token,
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );
    bounded(fixture.connection.shutdown(), cx)
        .await
        .expect("shutdown");
    assert!(
        !events(&fixture)
            .iter()
            .any(|event| event["event"] == "file-operation-applied")
    );
}

#[gpui::test]
async fn ambiguous_apply_is_reconciled_without_an_automatic_retry(cx: &mut TestAppContext) {
    for scenario in ["write-error", "write-invalid-result"] {
        let fixture = fixture(scenario, cx);
        bounded(fixture.connection.initialize(), cx)
            .await
            .expect("initialize");
        let prepared = bounded(
            fixture.connection.prepare_file_operation(
                "fixture-repository".into(),
                "src/example.txt".into(),
                FileOperation::Stage,
            ),
            cx,
        )
        .await
        .expect("prepare stage");
        bounded(
            fixture.connection.apply_file_operation(
                "fixture-repository".into(),
                "src/example.txt".into(),
                FileOperation::Stage,
                prepared.operation_token.clone(),
            ),
            cx,
        )
        .await
        .expect_err("unknown apply outcome");
        let reconciled = bounded(
            fixture.connection.reconcile_file_operation(
                "fixture-repository".into(),
                "src/example.txt".into(),
                FileOperation::Stage,
                prepared.operation_token,
            ),
            cx,
        )
        .await
        .expect("reconcile the original token");
        assert_eq!(reconciled.outcome, FileOperationOutcome::Applied);
        bounded(fixture.connection.shutdown(), cx)
            .await
            .expect("shutdown");
        let recorded = events(&fixture);
        assert_eq!(
            recorded
                .iter()
                .filter(|event| event["method"] == "vcs/applyFileOperation")
                .count(),
            1
        );
        assert_eq!(
            recorded
                .iter()
                .filter(|event| event["event"] == "file-operation-applied")
                .count(),
            1
        );
    }
}

#[gpui::test]
async fn file_operation_inputs_and_prepared_tokens_are_validated(cx: &mut TestAppContext) {
    let fixture = fixture("writable", cx);
    bounded(fixture.connection.initialize(), cx)
        .await
        .expect("initialize");
    for (repository, path) in [
        ("", "src/example.txt"),
        ("fixture-repository", "../outside"),
    ] {
        assert!(
            bounded(
                fixture.connection.prepare_file_operation(
                    repository.into(),
                    path.into(),
                    FileOperation::Stage,
                ),
                cx,
            )
            .await
            .is_err()
        );
    }
    for token in [String::new(), "x".repeat(MAX_OPERATION_TOKEN_BYTES + 1)] {
        assert!(
            bounded(
                fixture.connection.apply_file_operation(
                    "fixture-repository".into(),
                    "src/example.txt".into(),
                    FileOperation::Stage,
                    token.clone(),
                ),
                cx,
            )
            .await
            .is_err()
        );
        assert!(
            bounded(
                fixture.connection.reconcile_file_operation(
                    "fixture-repository".into(),
                    "src/example.txt".into(),
                    FileOperation::Stage,
                    token,
                ),
                cx,
            )
            .await
            .is_err()
        );
    }
    bounded(fixture.connection.shutdown(), cx)
        .await
        .expect("shutdown");
    assert!(!events(&fixture).iter().any(|event| {
        event["method"]
            .as_str()
            .is_some_and(|method| method.ends_with("FileOperation"))
    }));

    let invalid = self::fixture("invalid-token", cx);
    bounded(invalid.connection.initialize(), cx)
        .await
        .expect("initialize");
    assert!(
        bounded(
            invalid.connection.prepare_file_operation(
                "fixture-repository".into(),
                "src/example.txt".into(),
                FileOperation::Stage,
            ),
            cx,
        )
        .await
        .is_err()
    );
    bounded(invalid.connection.shutdown(), cx)
        .await
        .expect("shutdown");
}

#[gpui::test]
async fn file_batches_validate_capability_paths_and_prepared_targets(cx: &mut TestAppContext) {
    let legacy = fixture("writable", cx);
    bounded(legacy.connection.initialize(), cx).await.unwrap();
    assert!(
        bounded(
            legacy.connection.prepare_file_batch(
                "fixture-repository".into(),
                vec!["a-mixed.txt".into()],
                FileOperation::Stage,
            ),
            cx
        )
        .await
        .is_err()
    );
    assert!(
        !events(&legacy)
            .iter()
            .any(|event| event["method"] == "vcs/prepareFileBatch")
    );
    bounded(legacy.connection.shutdown(), cx).await.unwrap();

    let bulk = fixture("bulk-write", cx);
    bounded(bulk.connection.initialize(), cx).await.unwrap();
    for paths in [
        vec![],
        vec!["../outside".into()],
        vec!["a.txt".into(), "a.txt".into()],
        (0..=crate::MAX_FILE_BATCH_PATHS)
            .map(|index| format!("file-{index}"))
            .collect(),
    ] {
        assert!(
            bounded(
                bulk.connection.prepare_file_batch(
                    "fixture-repository".into(),
                    paths,
                    FileOperation::Stage,
                ),
                cx
            )
            .await
            .is_err()
        );
    }
    for (repository, operation) in [
        ("", FileOperation::Stage),
        ("fixture-repository", FileOperation::Discard),
    ] {
        assert!(
            bounded(
                bulk.connection.prepare_file_batch(
                    repository.into(),
                    vec!["a.txt".into()],
                    operation,
                ),
                cx
            )
            .await
            .is_err()
        );
    }
    assert!(
        !events(&bulk)
            .iter()
            .any(|event| event["method"] == "vcs/prepareFileBatch")
    );
    bounded(bulk.connection.shutdown(), cx).await.unwrap();

    for scenario in ["bulk-write-missing-paths", "bulk-write-invalid-paths"] {
        let invalid = fixture(scenario, cx);
        bounded(invalid.connection.initialize(), cx).await.unwrap();
        assert!(
            bounded(
                invalid.connection.prepare_file_batch(
                    "fixture-repository".into(),
                    vec!["a.txt".into(), "b.txt".into()],
                    FileOperation::Stage,
                ),
                cx
            )
            .await
            .is_err()
        );
        bounded(invalid.connection.shutdown(), cx).await.unwrap();
    }
}

#[gpui::test]
async fn file_batches_bind_all_paths_and_reconcile_without_reapplying(cx: &mut TestAppContext) {
    for scenario in ["bulk-write", "bulk-write-ambiguous", "bulk-write-error"] {
        let fixture = fixture(scenario, cx);
        bounded(fixture.connection.initialize(), cx).await.unwrap();
        let paths = vec!["a-mixed.txt".into(), "d-renamed.txt".into()];
        let prepared = bounded(
            fixture.connection.prepare_file_batch(
                "fixture-repository".into(),
                paths.clone(),
                FileOperation::Stage,
            ),
            cx,
        )
        .await
        .unwrap();
        assert_eq!(
            prepared.affected_paths.unwrap(),
            ["a-mixed.txt", "d-renamed.txt", "old.txt"]
        );
        assert!(
            bounded(
                fixture.connection.apply_file_batch(
                    "fixture-repository".into(),
                    vec!["a-mixed.txt".into()],
                    FileOperation::Stage,
                    prepared.operation_token.clone(),
                ),
                cx
            )
            .await
            .is_err()
        );
        let applied = bounded(
            fixture.connection.apply_file_batch(
                "fixture-repository".into(),
                paths.clone(),
                FileOperation::Stage,
                prepared.operation_token.clone(),
            ),
            cx,
        )
        .await;
        if scenario == "bulk-write-ambiguous" {
            assert!(applied.is_err());
        } else {
            assert_eq!(
                applied.unwrap().outcome,
                if scenario == "bulk-write-error" {
                    FileOperationOutcome::NotApplied
                } else {
                    FileOperationOutcome::Applied
                }
            );
        }
        let reconciled = bounded(
            fixture.connection.reconcile_file_batch(
                "fixture-repository".into(),
                paths.clone(),
                FileOperation::Stage,
                prepared.operation_token.clone(),
            ),
            cx,
        )
        .await
        .unwrap();
        assert_eq!(
            reconciled.outcome,
            if scenario == "bulk-write-error" {
                FileOperationOutcome::NotApplied
            } else {
                FileOperationOutcome::Applied
            }
        );
        assert!(
            bounded(
                fixture.connection.apply_file_batch(
                    "fixture-repository".into(),
                    paths,
                    FileOperation::Stage,
                    prepared.operation_token,
                ),
                cx
            )
            .await
            .is_err()
        );
        bounded(fixture.connection.shutdown(), cx).await.unwrap();
        assert_eq!(
            events(&fixture)
                .iter()
                .filter(|event| event["event"] == "file-batch-applied")
                .count(),
            usize::from(scenario != "bulk-write-error")
        );
    }
}

#[gpui::test]
async fn checkout_catalog_and_write_capabilities_are_independent(cx: &mut TestAppContext) {
    for scenario in ["normal", "checkout-read-only"] {
        let fixture = fixture(scenario, cx);
        bounded(fixture.connection.initialize(), cx).await.unwrap();
        let catalog = bounded(
            fixture
                .connection
                .list_checkouts("fixture-repository".into()),
            cx,
        )
        .await;
        if scenario == "normal" {
            assert!(catalog.is_err());
        } else {
            assert_eq!(catalog.unwrap().checkouts.len(), 5);
        }
        let operation = crate::CheckoutOperation::Open {
            checkout_id: "fixture-checkout".into(),
        };
        assert!(
            bounded(
                fixture
                    .connection
                    .prepare_checkout_operation("fixture-repository".into(), operation.clone()),
                cx
            )
            .await
            .is_err()
        );
        assert!(
            bounded(
                fixture.connection.apply_checkout_operation(
                    "fixture-repository".into(),
                    operation.clone(),
                    "opaque-token".into()
                ),
                cx
            )
            .await
            .is_err()
        );
        assert!(
            bounded(
                fixture.connection.reconcile_checkout_operation(
                    "fixture-repository".into(),
                    operation,
                    "opaque-token".into()
                ),
                cx
            )
            .await
            .is_err()
        );
        bounded(fixture.connection.shutdown(), cx).await.unwrap();
        assert!(!events(&fixture).iter().any(|event| {
            event["method"]
                .as_str()
                .is_some_and(|method| method.ends_with("CheckoutOperation"))
        }));
    }
}

#[gpui::test]
async fn checkout_transport_returns_prepared_target_and_verified_identity(cx: &mut TestAppContext) {
    let fixture = fixture("checkouts", cx);
    bounded(fixture.connection.initialize(), cx).await.unwrap();
    let operation = crate::CheckoutOperation::Open {
        checkout_id: "unmounted-checkout".into(),
    };
    let prepared = bounded(
        fixture
            .connection
            .prepare_checkout_operation("fixture-repository".into(), operation.clone()),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(prepared.checkout.as_ref().unwrap().id, "unmounted-checkout");
    let result = bounded(
        fixture.connection.apply_checkout_operation(
            "fixture-repository".into(),
            operation.clone(),
            prepared.operation_token,
        ),
        cx,
    )
    .await
    .unwrap();
    result
        .validate(
            &operation,
            &prepared.target_root_uri,
            prepared.checkout.as_ref(),
        )
        .unwrap();
    assert_eq!(
        result.checkout.unwrap().state,
        crate::CheckoutState::Mounted
    );
    bounded(fixture.connection.shutdown(), cx).await.unwrap();
}

#[test]
fn checkout_result_must_preserve_prepared_root_and_existing_identity() {
    let operation = crate::CheckoutOperation::Open {
        checkout_id: "existing".into(),
    };
    let checkout: crate::CheckoutDescriptor = serde_json::from_value(json!({
        "id": "existing", "name": "Existing", "rootUri": "file:///checkouts/existing",
        "scopeUri": "file:///checkouts/existing/project", "state": "mounted",
        "owned": true, "operations": ["open"], "preparation": "ready"
    }))
    .unwrap();
    let mut result = crate::CheckoutOperationResult {
        outcome: FileOperationOutcome::Applied,
        checkout: Some(checkout.clone()),
    };
    result
        .validate(&operation, &checkout.root_uri, Some(&checkout))
        .unwrap();
    result.checkout.as_mut().unwrap().id = "replacement".into();
    assert!(
        result
            .validate(&operation, &checkout.root_uri, Some(&checkout))
            .is_err()
    );
    result.checkout = None;
    assert!(
        result
            .validate(&operation, &checkout.root_uri, Some(&checkout))
            .is_err()
    );
    let mut invalid = checkout;
    invalid.scope_uri = "file:///another/project".into();
    assert!(invalid.validate().is_err());
}

#[test]
fn applied_removal_cannot_report_a_live_checkout() {
    let operation = crate::CheckoutOperation::Remove {
        checkout_id: "existing".into(),
    };
    let mut checkout: crate::CheckoutDescriptor = serde_json::from_value(json!({
        "id": "existing", "name": "Existing", "rootUri": "file:///checkouts/existing",
        "scopeUri": "file:///checkouts/existing/project", "state": "mounted",
        "owned": true, "operations": ["open"], "preparation": "ready"
    }))
    .unwrap();
    let expected = checkout.clone();
    for state in [
        crate::CheckoutState::Mounted,
        crate::CheckoutState::Unmounted,
        crate::CheckoutState::Busy,
    ] {
        checkout.state = state;
        let result = crate::CheckoutOperationResult {
            outcome: FileOperationOutcome::Applied,
            checkout: Some(checkout.clone()),
        };
        assert!(
            result
                .validate(&operation, &expected.root_uri, Some(&expected))
                .is_err()
        );
    }
    checkout.state = crate::CheckoutState::Missing;
    checkout.operations.clear();
    crate::CheckoutOperationResult {
        outcome: FileOperationOutcome::Applied,
        checkout: Some(checkout),
    }
    .validate(&operation, &expected.root_uri, Some(&expected))
    .unwrap();
    crate::CheckoutOperationResult {
        outcome: FileOperationOutcome::Applied,
        checkout: None,
    }
    .validate(&operation, &expected.root_uri, Some(&expected))
    .unwrap();
}

#[test]
fn optional_file_operation_metadata_keeps_legacy_responses_valid() {
    let legacy: crate::PreparedFileOperation =
        serde_json::from_value(json!({"operationToken": "legacy"})).unwrap();
    legacy.validate().unwrap();
    assert!(legacy.affected_paths.is_none());
    assert!(legacy.summary.is_none());
    let prepared: crate::PreparedFileOperation = serde_json::from_value(json!({
        "operationToken": "typed", "affectedPaths": ["new.txt", "old.txt"], "summary": "Restore both paths"
    })).unwrap();
    prepared.validate().unwrap();
    assert_eq!(prepared.affected_paths.unwrap(), ["new.txt", "old.txt"]);
}

fn assert_token_consumed(error: anyhow::Error) {
    let error = error
        .downcast_ref::<ProviderError>()
        .expect("typed consumed-token error");
    assert_eq!(error.data.as_ref().unwrap()["kind"], "tokenConsumed");
}

#[gpui::test]
async fn repository_terminal_tokens_cannot_rewrite_state_after_aba(cx: &mut TestAppContext) {
    let fixture = fixture("repository-clean", cx);
    bounded(fixture.connection.initialize(), cx).await.unwrap();
    let topic = crate::RepositoryOperation::SwitchBranch {
        branch_id: "topic".into(),
    };
    let main = crate::RepositoryOperation::SwitchBranch {
        branch_id: "main".into(),
    };
    let unused = bounded(
        fixture
            .connection
            .prepare_repository_operation("fixture-repository".into(), topic.clone()),
        cx,
    )
    .await
    .unwrap();
    let result = bounded(
        fixture.connection.reconcile_repository_operation(
            "fixture-repository".into(),
            topic.clone(),
            unused.operation_token.clone(),
        ),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, FileOperationOutcome::NotApplied);
    assert_token_consumed(
        bounded(
            fixture.connection.apply_repository_operation(
                "fixture-repository".into(),
                topic.clone(),
                unused.operation_token.clone(),
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );

    let first = bounded(
        fixture
            .connection
            .prepare_repository_operation("fixture-repository".into(), topic.clone()),
        cx,
    )
    .await
    .unwrap();
    assert_ne!(first.operation_token, unused.operation_token);
    let applied = bounded(
        fixture.connection.apply_repository_operation(
            "fixture-repository".into(),
            topic.clone(),
            first.operation_token.clone(),
        ),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(applied.outcome, FileOperationOutcome::Applied);
    let second = bounded(
        fixture
            .connection
            .prepare_repository_operation("fixture-repository".into(), main.clone()),
        cx,
    )
    .await
    .unwrap();
    bounded(
        fixture.connection.apply_repository_operation(
            "fixture-repository".into(),
            main,
            second.operation_token,
        ),
        cx,
    )
    .await
    .unwrap();
    assert_token_consumed(
        bounded(
            fixture.connection.apply_repository_operation(
                "fixture-repository".into(),
                topic.clone(),
                first.operation_token,
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );

    let stale = bounded(
        fixture
            .connection
            .prepare_repository_operation("fixture-repository".into(), topic.clone()),
        cx,
    )
    .await
    .unwrap();
    std::fs::write(format!("{}.state", fixture.events.display()), "changed").unwrap();
    let conflicted = bounded(
        fixture.connection.reconcile_repository_operation(
            "fixture-repository".into(),
            topic.clone(),
            stale.operation_token.clone(),
        ),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(conflicted.outcome, FileOperationOutcome::Conflict);
    std::fs::write(format!("{}.state", fixture.events.display()), "").unwrap();
    assert_token_consumed(
        bounded(
            fixture.connection.apply_repository_operation(
                "fixture-repository".into(),
                topic,
                stale.operation_token,
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );
    let state = bounded(
        fixture
            .connection
            .repository_state("fixture-repository".into()),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(state.current_branch.unwrap().id, "main");
    bounded(fixture.connection.shutdown(), cx).await.unwrap();
    assert_eq!(
        events(&fixture)
            .iter()
            .filter(|event| event["event"] == "repository-operation-applied")
            .count(),
        2
    );
}

#[gpui::test]
async fn checkout_terminal_tokens_cannot_remount_after_aba(cx: &mut TestAppContext) {
    let fixture = fixture("checkouts", cx);
    bounded(fixture.connection.initialize(), cx).await.unwrap();
    let open = crate::CheckoutOperation::Open {
        checkout_id: "unmounted-checkout".into(),
    };
    let suspend = crate::CheckoutOperation::Suspend {
        checkout_id: "unmounted-checkout".into(),
    };
    let unused = bounded(
        fixture
            .connection
            .prepare_checkout_operation("fixture-repository".into(), open.clone()),
        cx,
    )
    .await
    .unwrap();
    let result = bounded(
        fixture.connection.reconcile_checkout_operation(
            "fixture-repository".into(),
            open.clone(),
            unused.operation_token.clone(),
        ),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, FileOperationOutcome::NotApplied);
    assert_token_consumed(
        bounded(
            fixture.connection.apply_checkout_operation(
                "fixture-repository".into(),
                open.clone(),
                unused.operation_token.clone(),
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );

    let first = bounded(
        fixture
            .connection
            .prepare_checkout_operation("fixture-repository".into(), open.clone()),
        cx,
    )
    .await
    .unwrap();
    assert_ne!(first.operation_token, unused.operation_token);
    bounded(
        fixture.connection.apply_checkout_operation(
            "fixture-repository".into(),
            open.clone(),
            first.operation_token.clone(),
        ),
        cx,
    )
    .await
    .unwrap();
    let second = bounded(
        fixture
            .connection
            .prepare_checkout_operation("fixture-repository".into(), suspend.clone()),
        cx,
    )
    .await
    .unwrap();
    bounded(
        fixture.connection.apply_checkout_operation(
            "fixture-repository".into(),
            suspend,
            second.operation_token,
        ),
        cx,
    )
    .await
    .unwrap();
    assert_token_consumed(
        bounded(
            fixture.connection.apply_checkout_operation(
                "fixture-repository".into(),
                open.clone(),
                first.operation_token,
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );

    let stale = bounded(
        fixture
            .connection
            .prepare_checkout_operation("fixture-repository".into(), open.clone()),
        cx,
    )
    .await
    .unwrap();
    std::fs::write(
        format!("{}.checkout-state", fixture.events.display()),
        "changed",
    )
    .unwrap();
    let conflicted = bounded(
        fixture.connection.reconcile_checkout_operation(
            "fixture-repository".into(),
            open.clone(),
            stale.operation_token.clone(),
        ),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(conflicted.outcome, FileOperationOutcome::Conflict);
    std::fs::write(format!("{}.checkout-state", fixture.events.display()), "").unwrap();
    assert_token_consumed(
        bounded(
            fixture.connection.apply_checkout_operation(
                "fixture-repository".into(),
                open,
                stale.operation_token,
            ),
            cx,
        )
        .await
        .unwrap_err(),
    );
    let catalog = bounded(
        fixture
            .connection
            .list_checkouts("fixture-repository".into()),
        cx,
    )
    .await
    .unwrap();
    assert_eq!(
        catalog
            .checkouts
            .iter()
            .find(|checkout| checkout.id == "unmounted-checkout")
            .unwrap()
            .state,
        crate::CheckoutState::Unmounted
    );
    bounded(fixture.connection.shutdown(), cx).await.unwrap();
    assert_eq!(
        events(&fixture)
            .iter()
            .filter(|event| event["event"] == "checkout-operation-applied")
            .count(),
        2
    );
}
