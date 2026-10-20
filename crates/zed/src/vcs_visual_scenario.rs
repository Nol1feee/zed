//! Opt-in acceptance scenarios against a real, prebuilt external VCS extension.
//! The caller supplies disposable checkouts and asserts provider/checkout state
//! independently. No real user settings, credentials, or extension installs are used.

use anyhow::{Context as _, Result, ensure};
use extension::{ExtensionHostProxy, ExtensionManifest, ExtensionVcsProviderProxy as _};
use extension_host::wasm_host::WasmHost;
use gpui::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Modifiers, VisualTestAppContext,
    WindowBounds, WindowHandle, WindowOptions, point, px, size,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use workspace::{AppState, MultiWorkspace, Workspace};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scenario {
    pub project_path: PathBuf,
    extension_directory: PathBuf,
    provider_id: String,
    output_directory: PathBuf,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Action { name: String, data: Option<Value> },
    Keys { keys: String },
    Input { text: String },
    Click { x: f32, y: f32 },
    OpenFile { path: PathBuf },
    SelectWorkspace { path: PathBuf },
    CloseWindow,
    Wait { pointer: String, equals: Value },
    WaitFile { path: PathBuf, content: String },
    WaitEditor { suffix: String, dirty: bool },
    Screenshot { name: String },
}

impl Step {
    fn label(&self) -> String {
        match self {
            Self::Action { name, .. } => format!("Action {name}"),
            Self::Keys { keys } => format!("Keys {keys}"),
            Self::Input { text } => format!("Input ({} characters)", text.chars().count()),
            Self::Click { x, y } => format!("Click {x}, {y}"),
            Self::OpenFile { path } => format!("Open file {}", path.display()),
            Self::SelectWorkspace { path } => format!("Select workspace {}", path.display()),
            Self::CloseWindow => "Close window".into(),
            Self::Wait { pointer, equals } => format!("Wait {pointer} = {equals}"),
            Self::WaitFile { path, .. } => format!("Wait for file content: {}", path.display()),
            Self::WaitEditor { dirty, .. } => format!("Wait for editor suffix and dirty={dirty}"),
            Self::Screenshot { name } => format!("Screenshot {name}"),
        }
    }
}

impl Scenario {
    pub fn from_environment() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("ZED_VCS_VISUAL_SCENARIO") else {
            return Ok(None);
        };
        let mut scenario: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        scenario.project_path = scenario.project_path.canonicalize()?;
        scenario.extension_directory = scenario.extension_directory.canonicalize()?;
        ensure!(
            scenario
                .steps
                .iter()
                .any(|step| matches!(step, Step::Wait { .. } | Step::WaitFile { .. })),
            "scenario must contain state or file assertions"
        );
        std::fs::create_dir_all(&scenario.output_directory)?;
        scenario.output_directory = scenario.output_directory.canonicalize()?;
        Ok(Some(scenario))
    }

    pub fn register_provider(
        &self,
        app: &Arc<AppState>,
        cx: &mut VisualTestAppContext,
    ) -> Result<()> {
        let proxy = cx.update(|cx| {
            cx.set_prompt_builder(gpui::fallback_prompt_renderer);
            project::trusted_worktrees::init(Default::default(), cx);
            project::vcs_store::init(cx);
            // Newly opened checkout Projects also need this fixture's explicit CLI environment.
            cx.observe_new(|project: &mut project::Project, _, cx| {
                project.environment().update(cx, |environment, _| {
                    environment.set_cli_environment_for_test(std::env::vars().collect());
                });
            })
            .detach();
            ExtensionHostProxy::default_global(cx)
        });
        cx.background_executor.allow_parking();
        let manifest = cx.foreground_executor.block_test(ExtensionManifest::load(
            app.fs.clone(),
            &self.extension_directory,
        ));
        cx.background_executor.forbid_parking();
        let manifest = Arc::new(manifest?);
        ensure!(
            manifest
                .vcs_providers
                .contains_key(self.provider_id.as_str()),
            "provider missing from manifest"
        );
        let host = cx.update(|cx| {
            WasmHost::new(
                app.fs.clone(),
                app.client.http_client(),
                app.node_runtime.clone(),
                proxy.clone(),
                paths::data_dir().join("extension-work"),
                cx,
            )
        });
        let wasm = std::fs::read(self.extension_directory.join("extension.wasm"))?;
        let task = cx.update(|cx| host.load_extension(wasm, &manifest, &cx.to_async()));
        cx.background_executor.allow_parking();
        let extension = cx.foreground_executor.block_test(task);
        cx.background_executor.forbid_parking();
        let extension = Arc::new(extension?);
        cx.update(|cx| proxy.register_vcs_provider(extension, self.provider_id.clone().into(), cx));
        Ok(())
    }

    pub fn run(self, app: &Arc<AppState>, cx: &mut VisualTestAppContext) -> Result<()> {
        let project = cx.update(|cx| {
            project::Project::local(
                app.client.clone(),
                app.node_runtime.clone(),
                app.user_store.clone(),
                app.languages.clone(),
                app.fs.clone(),
                Some(std::env::vars().collect()),
                project::LocalProjectFlags {
                    init_worktree_trust: true,
                    ..Default::default()
                },
                cx,
            )
        });
        // Workspace actions live on MultiWorkspace, matching the application's real root.
        let initial: WindowHandle<MultiWorkspace> = cx.update(|cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.), px(0.)),
                        size: size(px(1280.), px(800.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                |window, cx| {
                    let workspace =
                        cx.new(|cx| Workspace::new(None, project.clone(), app.clone(), window, cx));
                    cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                },
            )
        })?;
        let mut window: AnyWindowHandle = initial.into();
        let workspace = initial.update(cx, |multi_workspace, _, _| {
            multi_workspace.workspace().clone()
        })?;
        let task = cx.update(|cx| {
            project.update(cx, |project, cx| {
                project.find_or_create_worktree(&self.project_path, true, cx)
            })
        });
        cx.background_executor.allow_parking();
        let worktree = cx.foreground_executor.block_test(task);
        cx.background_executor.forbid_parking();
        worktree?;
        cx.update(|cx| {
            let project = project.read(cx);
            let worktrees = project.worktree_store();
            let paths = project
                .visible_worktrees(cx)
                .map(|worktree| {
                    project::trusted_worktrees::PathTrust::Worktree(worktree.read(cx).id())
                })
                .collect();
            project::trusted_worktrees::TrustedWorktrees::try_get_global(cx)
                .context("test trust store missing")?
                .update(cx, |trust, cx| trust.trust(&worktrees, paths, cx));
            anyhow::Ok(())
        })?;
        let async_cx = cx.update_window(window, |_, window, cx| window.to_async(cx))?;
        cx.background_executor.allow_parking();
        let panel = cx
            .foreground_executor
            .block_test(git_ui::vcs_panel::VcsPanel::load(
                workspace.downgrade(),
                async_cx.clone(),
            ));
        let project_panel = cx
            .foreground_executor
            .block_test(project_panel::ProjectPanel::load(
                workspace.downgrade(),
                async_cx,
            ));
        cx.background_executor.forbid_parking();
        let panel = panel?;
        let project_panel = project_panel?;
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.add_panel(project_panel, window, cx);
                workspace.add_panel(panel, window, cx);
                workspace.open_panel::<project_panel::ProjectPanel>(window, cx);
                workspace.open_panel::<git_ui::vcs_panel::VcsPanel>(window, cx);
            })
        })?;
        cx.run_until_parked();
        for (index, step) in self.steps.iter().enumerate() {
            println!("VCS visual step {}: {}", index + 1, step.label());
            let result = self.run_step(step, app, &mut window, cx);
            let observation = observe(app, window, cx)?;
            std::fs::write(
                self.output_directory
                    .join(format!("step-{:02}.json", index + 1)),
                serde_json::to_vec_pretty(&observation)?,
            )?;
            if let Err(error) = result {
                cx.capture_screenshot(window)?
                    .save(self.output_directory.join("failure.png"))?;
                return Err(error)
                    .with_context(|| format!("VCS visual step {}: {}", index + 1, step.label()));
            }
        }
        println!(
            "External VCS visual scenario passed ({} steps)",
            self.steps.len()
        );
        Ok(())
    }

    fn run_step(
        &self,
        step: &Step,
        app: &Arc<AppState>,
        window: &mut AnyWindowHandle,
        cx: &mut VisualTestAppContext,
    ) -> Result<()> {
        match step {
            Step::Action { name, data } => {
                cx.update_window(*window, |_, window, cx| {
                    let action = cx.build_action(name, data.clone())?;
                    ensure!(
                        window
                            .available_actions(cx)
                            .iter()
                            .any(|a| a.name() == action.name()),
                        "action is not available in the focused UI: {name}"
                    );
                    window.dispatch_action(action, cx);
                    anyhow::Ok(())
                })??;
            }
            Step::Keys { keys } => cx.simulate_keystrokes(*window, keys),
            Step::Input { text } => cx.simulate_input(*window, text),
            Step::Click { x, y } => {
                cx.simulate_click(*window, point(px(*x), px(*y)), Modifiers::default())
            }
            Step::OpenFile { path } => {
                ensure!(
                    !path.is_absolute()
                        && !path
                            .components()
                            .any(|c| matches!(c, std::path::Component::ParentDir)),
                    "file must be relative to this workspace"
                );
                let workspace = current_workspace(*window, cx)?;
                let task = cx.update_window(*window, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        let root = workspace
                            .root_paths(cx)
                            .into_iter()
                            .next()
                            .context("workspace has no root")?;
                        Ok::<_, anyhow::Error>(workspace.open_abs_path(
                            root.join(path),
                            workspace::OpenOptions {
                                focus: Some(true),
                                ..Default::default()
                            },
                            window,
                            cx,
                        ))
                    })
                })??;
                cx.background_executor.allow_parking();
                let result = cx.foreground_executor.block_test(task);
                cx.background_executor.forbid_parking();
                result?;
            }
            Step::SelectWorkspace { path } => {
                let path = path.canonicalize()?;
                *window = cx
                    .read(|cx| {
                        app.workspace_store
                            .read(cx)
                            .workspaces_with_windows()
                            .find_map(|(handle, weak)| {
                                let workspace = weak.upgrade()?;
                                let active = handle
                                    .read::<MultiWorkspace, _, _>(cx, |multi, cx| {
                                        multi.read(cx).workspace().clone()
                                    })
                                    .ok()?;
                                (workspace == active
                                    && workspace
                                        .read(cx)
                                        .root_paths(cx)
                                        .iter()
                                        .any(|root| root.as_ref() == path))
                                .then_some(handle)
                            })
                    })
                    .context("requested workspace is not open")?;
                let workspace = current_workspace(*window, cx)?;
                if cx.read(|cx| {
                    workspace
                        .read(cx)
                        .panel::<git_ui::vcs_panel::VcsPanel>(cx)
                        .is_none()
                }) {
                    let async_cx =
                        cx.update_window(*window, |_, window, cx| window.to_async(cx))?;
                    cx.background_executor.allow_parking();
                    let panel =
                        cx.foreground_executor
                            .block_test(git_ui::vcs_panel::VcsPanel::load(
                                workspace.downgrade(),
                                async_cx,
                            ));
                    cx.background_executor.forbid_parking();
                    let panel = panel?;
                    cx.update_window(*window, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.add_panel(panel, window, cx);
                        })
                    })?;
                }
            }
            Step::CloseWindow => {
                let workspace = current_workspace(*window, cx)?;
                ensure!(
                    cx.read(|cx| workspace
                        .read(cx)
                        .project()
                        .read(cx)
                        .opened_buffers(cx)
                        .iter()
                        .all(|buffer| !buffer.read(cx).is_dirty())),
                    "refusing to close a window with unsaved buffers"
                );
                cx.update_window(*window, |_, window, _| window.remove_window())?;
            }
            Step::Wait { pointer, equals } => {
                let start = Instant::now();
                loop {
                    cx.run_until_parked();
                    let state = observe(app, *window, cx)?;
                    if state.pointer(pointer) == Some(equals) {
                        break;
                    }
                    ensure!(
                        start.elapsed() < Duration::from_secs(120),
                        "timed out waiting for {pointer} = {equals}; observed: {:?}",
                        state.pointer(pointer)
                    );
                    std::thread::sleep(Duration::from_millis(20));
                    cx.advance_clock(Duration::from_millis(20));
                }
            }
            Step::WaitFile { path, content } => {
                let start = Instant::now();
                loop {
                    cx.run_until_parked();
                    if std::fs::read_to_string(path).as_deref().ok() == Some(content) {
                        break;
                    }
                    ensure!(
                        start.elapsed() < Duration::from_secs(120),
                        "file content did not reach expected state: {}",
                        path.display()
                    );
                    std::thread::sleep(Duration::from_millis(20));
                    cx.advance_clock(Duration::from_millis(20));
                }
            }
            Step::WaitEditor { suffix, dirty } => {
                let start = Instant::now();
                loop {
                    cx.run_until_parked();
                    let workspace = current_workspace(*window, cx)?;
                    let ready = cx.read(|cx| {
                        let workspace = workspace.read(cx);
                        workspace
                            .active_item_as::<editor::Editor>(cx)
                            .is_some_and(|editor| editor.read(cx).text(cx).ends_with(suffix))
                            && workspace
                                .active_item(cx)
                                .is_some_and(|item| item.is_dirty(cx) == *dirty)
                    });
                    if ready {
                        break;
                    }
                    ensure!(
                        start.elapsed() < Duration::from_secs(120),
                        "active editor did not reach expected suffix/dirty state"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                    cx.advance_clock(Duration::from_millis(20));
                }
            }
            Step::Screenshot { name } => {
                ensure!(
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                    "invalid screenshot name"
                );
                cx.capture_screenshot(*window)?
                    .save(self.output_directory.join(format!("{name}.png")))?;
            }
        }
        cx.run_until_parked();
        Ok(())
    }
}

fn current_workspace(
    window: AnyWindowHandle,
    cx: &VisualTestAppContext,
) -> Result<Entity<Workspace>> {
    cx.read(|cx| {
        window.read::<MultiWorkspace, _, _>(cx, |multi_workspace, cx| {
            multi_workspace.read(cx).workspace().clone()
        })
    })
}

fn observe(
    app: &Arc<AppState>,
    window: AnyWindowHandle,
    cx: &mut VisualTestAppContext,
) -> Result<Value> {
    let active = current_workspace(window, cx)
        .ok()
        .map(|workspace| workspace.entity_id());
    let mut state = cx.read(|cx| {
        let workspaces = app.workspace_store.read(cx).workspaces_with_windows().filter_map(|(handle, weak)| {
            let workspace = weak.upgrade()?;
            let entity_id = workspace.entity_id();
            let workspace = workspace.read(cx);
            let project = workspace.project().read(cx);
            let repositories = project.vcs_store().map(|store| {
                let store = store.read(cx);
                store.repositories().into_iter().map(|repo| json!({
                    "descriptor": repo.descriptor,
                    "loading": repo.loading,
                    "error": repo.error.as_deref(),
                    "snapshot": repo.snapshot,
                    "changes": store.changes(&repo.key, cx),
                    "repository_state": repo.repository_state,
                    "checkouts": repo.checkouts,
                    "checkout_error": repo.checkout_error.as_deref(),
                    "checkout_loading": repo.checkout_loading,
                    "progress": store.operation_progress(&repo.key),
                    "operation_unresolved": store.file_operation_unresolved(&repo.key),
                    "operation_error": store.file_operation_error(&repo.key).as_deref(),
                })).collect::<Vec<_>>()
            }).unwrap_or_default();
            Some(json!({
                "current": handle == window && Some(entity_id) == active,
                "roots": workspace.root_paths(cx),
                "active_item_dirty": workspace.active_item(cx).map(|item| item.is_dirty(cx)),
                "active_item_kind": if workspace.active_item_as::<editor::SplittableEditor>(cx).is_some() { "diff" } else if workspace.active_item_as::<editor::Editor>(cx).is_some() { "editor" } else { "other" },
                "repositories": repositories,
                "errors": project.vcs_store().map(|store| store.read(cx).errors().into_iter().map(|s| s.to_string()).collect::<Vec<_>>()),
                "operation_running": project.vcs_store().is_some_and(|store| store.read(cx).file_operation_running()),
            }))
        }).collect::<Vec<_>>();
        let current = workspaces.iter().find(|item| item["current"] == true).cloned();
        json!({"workspaces": workspaces, "current": current})
    });
    state["prompt"] = json!(
        cx.update_window(window, |_, window, _| window.has_active_prompt())
            .unwrap_or(false)
    );
    Ok(state)
}
