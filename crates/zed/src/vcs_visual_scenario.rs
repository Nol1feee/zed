//! Opt-in acceptance scenarios against a real, prebuilt external VCS extension.
//! The caller supplies disposable checkouts and asserts provider/checkout state
//! independently. No real user settings, credentials, or extension installs are used.

use anyhow::{Context as _, Result, ensure};
use extension::{ExtensionHostProxy, ExtensionManifest, ExtensionVcsProviderProxy as _};
use extension_host::wasm_host::WasmHost;
use gpui::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Focusable as _, Modifiers, UpdateGlobal as _,
    VisualTestAppContext, WindowBounds, WindowHandle, WindowOptions, point, px, size,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use workspace::{AppState, MultiWorkspace, Panel as _, Workspace};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scenario {
    pub project_path: PathBuf,
    extension_directory: PathBuf,
    provider_id: String,
    output_directory: PathBuf,
    user_settings: Option<Value>,
    viewport: Option<[f32; 2]>,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Action {
        name: String,
        data: Option<Value>,
    },
    Keys {
        keys: String,
    },
    Input {
        text: String,
    },
    Click {
        x: f32,
        y: f32,
    },
    RightClick {
        x: f32,
        y: f32,
    },
    OpenFile {
        path: PathBuf,
        root: Option<PathBuf>,
    },
    AddRoot {
        path: PathBuf,
    },
    SelectWorkspace {
        path: PathBuf,
    },
    CloseWindow,
    Wait {
        pointer: String,
        equals: Value,
    },
    WaitFile {
        path: PathBuf,
        content: String,
    },
    WaitEditor {
        suffix: String,
        dirty: bool,
    },
    Screenshot {
        name: String,
    },
}

impl Step {
    fn label(&self) -> String {
        match self {
            Self::Action { name, .. } => format!("Action {name}"),
            Self::Keys { keys } => format!("Keys {keys}"),
            Self::Input { text } => format!("Input ({} characters)", text.chars().count()),
            Self::Click { x, y } => format!("Click {x}, {y}"),
            Self::RightClick { x, y } => format!("Right click {x}, {y}"),
            Self::OpenFile { path, .. } => format!("Open file {}", path.display()),
            Self::AddRoot { path } => format!("Add root {}", path.display()),
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
        if let Some([width, height]) = scenario.viewport {
            ensure!(
                (640.0..=3840.0).contains(&width) && (480.0..=2160.0).contains(&height),
                "invalid visual scenario viewport"
            );
        }
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
        if let Some(user_settings) = &self.user_settings {
            ensure!(
                user_settings.is_object(),
                "scenario user_settings must be an object"
            );
            cx.update(|cx| {
                settings::SettingsStore::update_global(cx, |store, cx| {
                    store
                        .set_user_settings(&user_settings.to_string(), cx)
                        .result()
                })
            })?;
        }
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
        cx.update(|cx| {
            // The runner omits some application subsystems, but uses their bundled keymap.
            let mut bindings = settings::KeymapFile::load_asset_allow_partial_failure(
                settings::DEFAULT_KEYMAP_PATH,
                cx,
            )?;
            for action in [
                "git_panel::ToggleFocus",
                "git::Diff",
                "git_panel::NextEntry",
            ] {
                ensure!(
                    bindings
                        .iter()
                        .any(|binding| binding.action().name() == action),
                    "required source-control action missing from bundled keymap: {action}"
                );
            }
            for binding in &mut bindings {
                binding.set_meta(settings::KeybindSource::Default.meta());
            }
            cx.bind_keys(bindings);
            anyhow::Ok(())
        })?;
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
                        size: {
                            let [width, height] = self.viewport.unwrap_or([1280.0, 800.0]);
                            size(px(width), px(height))
                        },
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
            .block_test(git_ui::git_panel::GitPanel::load(
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
                workspace.open_panel::<git_ui::git_panel::GitPanel>(window, cx);
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
            Step::RightClick { x, y } => {
                let position = point(px(*x), px(*y));
                cx.simulate_mouse_down(
                    *window,
                    position,
                    gpui::MouseButton::Right,
                    Modifiers::default(),
                );
                cx.simulate_mouse_up(
                    *window,
                    position,
                    gpui::MouseButton::Right,
                    Modifiers::default(),
                );
            }
            Step::OpenFile { path, root } => {
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
                        let roots = workspace.root_paths(cx);
                        let root = if let Some(root) = root {
                            let root = root.canonicalize()?;
                            ensure!(
                                roots.iter().any(|path| path.as_ref() == root.as_path()),
                                "file root is not in this workspace"
                            );
                            root
                        } else {
                            roots
                                .into_iter()
                                .next()
                                .context("workspace has no root")?
                                .to_path_buf()
                        };
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
            Step::AddRoot { path } => {
                let path = path.canonicalize()?;
                ensure!(path.is_dir(), "additional root must be a directory");
                let workspace = current_workspace(*window, cx)?;
                let project = cx.read(|cx| workspace.read(cx).project().clone());
                let task = cx.update(|cx| {
                    project.update(cx, |project, cx| {
                        project.find_or_create_worktree(&path, true, cx)
                    })
                });
                cx.background_executor.allow_parking();
                let worktree = cx.foreground_executor.block_test(task);
                cx.background_executor.forbid_parking();
                let (worktree, _) = worktree?;
                cx.update(|cx| {
                    let worktrees = project.read(cx).worktree_store();
                    let trust_path =
                        project::trusted_worktrees::PathTrust::Worktree(worktree.read(cx).id());
                    project::trusted_worktrees::TrustedWorktrees::try_get_global(cx)
                        .context("test trust store missing")?
                        .update(cx, |trust, cx| {
                            trust.trust(&worktrees, [trust_path].into_iter().collect(), cx)
                        });
                    anyhow::Ok(())
                })?;
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
                        .panel::<git_ui::git_panel::GitPanel>(cx)
                        .is_none()
                }) {
                    let async_cx =
                        cx.update_window(*window, |_, window, cx| window.to_async(cx))?;
                    cx.background_executor.allow_parking();
                    let panel =
                        cx.foreground_executor
                            .block_test(git_ui::git_panel::GitPanel::load(
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
                if cx.read(|cx| {
                    workspace
                        .read(cx)
                        .panel::<project_panel::ProjectPanel>(cx)
                        .is_none()
                }) {
                    let async_cx =
                        cx.update_window(*window, |_, window, cx| window.to_async(cx))?;
                    cx.background_executor.allow_parking();
                    let panel =
                        cx.foreground_executor
                            .block_test(project_panel::ProjectPanel::load(
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
                            .active_item(cx)
                            .and_then(|item| item.act_as::<editor::Editor>(cx))
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
    let workspaces = cx.read(|cx| {
        app.workspace_store
            .read(cx)
            .workspaces_with_windows()
            .filter_map(|(handle, weak)| weak.upgrade().map(|workspace| (handle, workspace)))
            .collect::<Vec<_>>()
    });
    let workspaces = workspaces.into_iter().map(|(handle, workspace)| {
        let current = handle == window && Some(workspace.entity_id()) == active;
        cx.update_window(handle, |_, window, cx| {
            let active_editor = workspace.read(cx).active_item(cx)
                .and_then(|item| item.act_as::<editor::Editor>(cx));
            let blame_state = active_editor.as_ref().map(|editor| observe_blame(editor, cx));
            let workspace = workspace.read(cx);
            let vcs_panel_registered = workspace.all_docks().into_iter().any(|dock| {
                dock.read(cx).panel_index_for_persistent_name("VcsPanel", cx).is_some()
            });
            ensure!(!vcs_panel_registered, "external VCS panel must not be registered separately");
            let git_panel = workspace.panel::<git_ui::git_panel::GitPanel>(cx);
            let visible_panels = workspace.all_docks().into_iter().filter_map(|dock| {
                dock.read(cx).visible_panel().map(|panel| panel.persistent_name())
            }).collect::<Vec<_>>();
            let active_item = workspace.active_item(cx);
            let history = workspace.active_item_as::<git_ui::vcs_file_history::VcsFileHistory>(cx)
                .map(|history| history.read(cx).diagnostics());
            let editor = active_item.as_ref().and_then(|item| item.act_as::<editor::Editor>(cx));
            let split_editor = active_item.as_ref().and_then(|item| item.act_as::<editor::SplittableEditor>(cx));
            let mut buffers = editor.as_ref().map(|editor| {
                editor.read(cx).buffer().read(cx).all_buffers_iter().map(|buffer| {
                    let id = buffer.entity_id().as_u64();
                    let buffer = buffer.read(cx);
                    json!({
                        "id": id,
                        "path": buffer.file().map(|file| file.full_path(cx)),
                        "dirty": buffer.is_dirty(),
                        "read_only": buffer.read_only(),
                        "text": buffer.snapshot().text().chars().take(8192).collect::<String>(),
                    })
                }).collect::<Vec<_>>()
            }).unwrap_or_default();
            buffers.sort_by_key(|buffer| buffer["id"].as_u64());
            let active_item_kind = editor.as_ref().map_or("other", |editor| {
                if editor.read(cx).buffer().read(cx).is_singleton() { "editor" } else { "diff" }
            });
            let project = workspace.project().read(cx);
            let repositories = project.vcs_store().map(|store| {
                let store = store.read(cx);
                store.repositories().into_iter().map(|repo| json!({
                    "descriptor": repo.descriptor,
                    "selected_baseline": repo.selected_baseline,
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
                    "file_operations": store.file_operations(&repo.key),
                    "repository_operations": store.repository_operations(&repo.key),
                    "checkout_operations": store.checkout_operations(&repo.key),
                })).collect::<Vec<_>>()
            }).unwrap_or_default();
            Ok(json!({
                "current": current,
                "roots": workspace.root_paths(cx),
                "git_repository_count": project.repositories(cx).len(),
                "worktree_entry_count": project.visible_worktrees(cx).map(|worktree| worktree.read(cx).snapshot().entry_count()).sum::<usize>(),
                "active_item_dirty": active_item.as_ref().map(|item| item.is_dirty(cx)),
                "active_item_kind": active_item_kind,
                "active_editor_read_only": editor.as_ref().map(|editor| editor.read(cx).read_only(cx)),
                "active_item_title": active_item.as_ref().map(|item| item.tab_content_text(0, cx)),
                "active_item_buffers": buffers,
                "file_history": history,
                "blame": blame_state,
                "active_diff_split": split_editor.as_ref().map(|editor| editor.read(cx).is_split()),
                "panels": {
                    "git": {
                        "loaded": git_panel.is_some(),
                        "commit": git_panel.as_ref().map(|panel| panel.read(cx).commit_workflow_diagnostics(window, cx)),
                        "external_selected": git_panel.as_ref().is_some_and(|panel| panel.read(cx).uses_vcs_panel(cx)),
                        "enabled": git_panel.as_ref().is_some_and(|panel| panel.read(cx).enabled(cx)),
                        "focused": git_panel.as_ref().is_some_and(|panel| panel.read(cx).focus_handle(cx).contains_focused(window, cx)),
                    },
                    "vcs": {"loaded": vcs_panel_registered},
                },
                "visible_panels": visible_panels,
                "repositories": repositories,
                "errors": project.vcs_store().map(|store| store.read(cx).errors().into_iter().map(|s| s.to_string()).collect::<Vec<_>>()),
                "operation_running": project.vcs_store().is_some_and(|store| store.read(cx).file_operation_running()),
            }))
        })?
    }).collect::<Result<Vec<_>>>()?;
    let current = workspaces
        .iter()
        .find(|item| item["current"] == true)
        .cloned();
    let mut state = json!({"workspaces": workspaces, "current": current});
    state["prompt"] = json!(
        cx.update_window(window, |_, window, _| window.has_active_prompt())
            .unwrap_or(false)
    );
    Ok(state)
}

fn observe_blame(editor: &Entity<editor::Editor>, cx: &mut gpui::App) -> Value {
    let (inline, gutter, blame, rows) = {
        let editor = editor.read(cx);
        let rows = editor
            .buffer()
            .read(cx)
            .all_buffers_iter()
            .flat_map(|buffer| {
                let buffer = buffer.read(cx);
                let buffer_id = buffer.remote_id();
                (0..=buffer.snapshot().max_point().row.min(11)).map(move |row| editor::RowInfo {
                    buffer_id: Some(buffer_id),
                    buffer_row: Some(row),
                    ..Default::default()
                })
            })
            .collect::<Vec<_>>();
        (
            editor.git_blame_inline_enabled(),
            editor.show_git_blame_gutter(),
            editor.blame().cloned(),
            rows,
        )
    };
    let Some(blame) = blame else {
        return json!({"inline": inline, "gutter": gutter, "generated": false, "rows": []});
    };
    blame.update(cx, |blame, cx| {
        let generated = blame.has_generated_entries();
        let entries = blame
            .blame_for_rows(&rows, cx)
            .enumerate()
            .map(|(index, entry)| {
                json!({
                    "row": rows.get(index).and_then(|row| row.buffer_row),
                    "entry": entry.map(|(_, entry)| json!({
                        "revisionId": entry.revision_id(),
                        "author": entry.author(),
                        "provider": matches!(entry, editor::DisplayBlameEntry::Provider(_)),
                    }))
                })
            })
            .collect::<Vec<_>>();
        json!({"inline": inline, "gutter": gutter, "generated": generated, "rows": entries})
    })
}
