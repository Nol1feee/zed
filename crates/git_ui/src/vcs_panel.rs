use std::{
    any::TypeId,
    collections::{BTreeMap, HashMap},
    ops::Range,
};

use anyhow::Result;
use editor::{Editor, EditorSettings, SplittableEditor};
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyBinding, PromptLevel, ScrollStrategy, Subscription, UniformListScrollHandle, WeakEntity,
    Window, actions, uniform_list,
};
use language::{Buffer, Capability};
use multi_buffer::{MultiBuffer, PathKey};
use project::{
    Project,
    vcs_store::{VcsDiff, VcsRepositoryId, VcsRepositorySnapshot, VcsStore},
};
use settings::Settings;
use ui::{Button, Color, IconButton, IconName, Label, ListItem, prelude::*};
use util::ResultExt as _;
use vcs::{
    Change, ChangeStatus, ContentKind, FileOperation, RepositoryOperation, RepositoryOperationKind,
};
use workspace::{
    Item, Panel, Workspace,
    dock::{DockPosition, PanelEvent},
    searchable::SearchableItemHandle,
};

actions!(
    vcs_panel,
    [
        /// Focuses changes provided by external version-control systems.
        ToggleFocus,
        /// Refreshes external source-control repositories.
        Refresh,
        /// Opens errors and bounded stderr output from external version-control providers.
        ShowDiagnostics,
        /// Stages the selected file through its external version-control provider.
        StageFile,
        /// Unstages the selected file through its external version-control provider.
        UnstageFile,
        /// Discards the selected file's unstaged changes while preserving staged changes.
        DiscardFile,
        /// Commits staged changes in the selected external repository scope.
        CommitStaged,
        /// Switches the selected external checkout to an existing branch.
        SwitchBranch,
    ]
);

pub fn init(cx: &mut App) {
    crate::vcs_repository_ui::init(cx);
    cx.bind_keys([
        KeyBinding::new("up", menu::SelectPrevious, Some("VcsPanel")),
        KeyBinding::new("down", menu::SelectNext, Some("VcsPanel")),
        KeyBinding::new("enter", menu::Confirm, Some("VcsPanel")),
    ]);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<VcsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &ShowDiagnostics, window, cx| {
            show_diagnostics(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &Refresh, _, cx| {
            if let Some(store) = workspace.project().read(cx).vcs_store().cloned() {
                store.update(cx, |store, cx| store.refresh_all(cx));
            }
        });
    })
    .detach();
}

fn show_diagnostics(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(store) = workspace.project().read(cx).vcs_store() else {
        return;
    };
    let store = store.read(cx);
    let mut sections = vec!["External VCS diagnostics".to_owned()];
    for repository in store.repositories() {
        sections.push(format!(
            "{}\nScope: {}\nBaseline: {}\nLoading: {}\nError: {}",
            repository.descriptor.display_name,
            repository.descriptor.scope_uri,
            repository.selected_baseline,
            repository.loading,
            repository.error.as_deref().unwrap_or("none"),
        ));
    }
    sections.extend(store.errors().into_iter().map(|error| error.to_string()));
    sections.extend(
        store
            .diagnostics()
            .into_iter()
            .map(|message| message.to_string()),
    );
    let buffer = cx.new(|cx| {
        let mut buffer = Buffer::local(sections.join("\n\n"), cx);
        buffer.set_capability(Capability::ReadOnly, cx);
        buffer
    });
    let multibuffer = cx
        .new(|cx| MultiBuffer::singleton(buffer, cx).with_title("External VCS diagnostics".into()));
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_multibuffer(multibuffer, None, window, cx);
        editor.set_should_serialize(false, cx);
        editor
    });
    let view = cx.new(|_| VcsDiagnosticsView { editor });
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

struct VcsDiagnosticsView {
    editor: Entity<Editor>,
}

impl EventEmitter<()> for VcsDiagnosticsView {}

impl Focusable for VcsDiagnosticsView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for VcsDiagnosticsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

impl Item for VcsDiagnosticsView {
    type Event = ();

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        self.editor.read(cx).tab_content_text(detail, cx)
    }

    fn capability(&self, cx: &App) -> Capability {
        self.editor.read(cx).capability(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx);
        });
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }
}

#[derive(Clone)]
struct Row {
    repository: VcsRepositoryId,
    path: String,
    label: String,
    depth: usize,
    change: Option<Change>,
}

pub struct VcsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Option<Entity<VcsStore>>,
    focus: FocusHandle,
    position: DockPosition,
    repositories: Vec<VcsRepositorySnapshot>,
    rows: Vec<Row>,
    selected: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    error: Option<SharedString>,
    preparing_operation: bool,
    opened: HashMap<(VcsRepositoryId, String), WeakEntity<SplittableEditor>>,
    _subscription: Option<Subscription>,
}

impl VcsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _, cx| {
            let project = workspace.project().clone();
            let workspace = cx.entity().downgrade();
            cx.new(|cx| Self::new(workspace, project, cx))
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = project.read(cx).vcs_store().cloned();
        let subscription = store
            .as_ref()
            .map(|store| cx.subscribe(store, |this, _, _, cx| this.update_entries(cx)));
        let mut panel = Self {
            workspace,
            project,
            store,
            focus: cx.focus_handle(),
            position: DockPosition::Left,
            repositories: Vec::new(),
            rows: Vec::new(),
            selected: None,
            scroll_handle: UniformListScrollHandle::new(),
            error: None,
            preparing_operation: false,
            opened: HashMap::new(),
            _subscription: subscription,
        };
        panel.update_entries(cx);
        panel
    }

    fn update_entries(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .selected
            .and_then(|index| self.rows.get(index))
            .map(|row| (row.repository.clone(), row.path.clone()));
        self.repositories = self
            .store
            .as_ref()
            .map(|store| store.read(cx).repositories())
            .unwrap_or_default();
        let changes = self
            .repositories
            .iter()
            .map(|repository| {
                let changes = self
                    .store
                    .as_ref()
                    .map(|store| store.read(cx).changes(&repository.key, cx))
                    .unwrap_or_default();
                (repository, changes)
            })
            .collect::<Vec<_>>();
        self.rows = rows_for_repositories(&changes);
        self.selected = selected
            .and_then(|selected| {
                self.rows.iter().position(|row| {
                    row.change.is_some() && (row.repository.clone(), row.path.clone()) == selected
                })
            })
            .or_else(|| self.rows.iter().position(|row| row.change.is_some()));
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let start = self.selected.map_or(0, |index| index + 1);
        self.selected = (start..self.rows.len())
            .find(|index| self.rows[*index].change.is_some())
            .or(self.selected);
        if let Some(index) = self.selected {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = self.selected.unwrap_or(self.rows.len());
        self.selected = (0..start)
            .rev()
            .find(|index| self.rows[*index].change.is_some())
            .or(self.selected);
        if let Some(index) = self.selected {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.open_diff(index, window, cx);
        }
    }

    fn metadata_notice(&self) -> Option<&'static str> {
        let change = self.rows.get(self.selected?)?.change.as_ref()?;
        match change.kind {
            ContentKind::File => None,
            ContentKind::Binary => Some("Binary file — text diff is unavailable."),
            ContentKind::Symlink => Some("Symbolic link — shown as metadata only in this preview."),
            ContentKind::Submodule => Some("Submodule — shown as metadata only in this preview."),
        }
    }

    fn stage_file(&mut self, _: &StageFile, _: &mut Window, cx: &mut Context<Self>) {
        self.run_file_operation(FileOperation::Stage, cx);
    }

    fn unstage_file(&mut self, _: &UnstageFile, _: &mut Window, cx: &mut Context<Self>) {
        self.run_file_operation(FileOperation::Unstage, cx);
    }

    fn discard_file(&mut self, _: &DiscardFile, window: &mut Window, cx: &mut Context<Self>) {
        if self.preparing_operation {
            return;
        }
        let Some(row) = self.selected_file().cloned() else {
            return;
        };
        let Some(store) = self.store.clone() else {
            return;
        };
        let Some(repository) = self
            .repositories
            .iter()
            .find(|repository| repository.key == row.repository)
        else {
            return;
        };
        let detail = format!(
            "Project: {}\nScope: {}",
            repository.descriptor.display_name, repository.descriptor.scope_uri,
        );
        self.error = None;
        self.preparing_operation = true;
        let preparation = store.update(cx, |store, cx| {
            store.prepare_file_operation(
                row.repository,
                row.path.clone(),
                FileOperation::Discard,
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let prepared = preparation.await?;
                let detail = format!(
                    "{}\n\n{}\n\nAffected paths:\n{}",
                    detail,
                    prepared.summary().unwrap_or("The working file will be restored to its staged version. Staged changes will be preserved."),
                    prepared.affected_paths().join("\n"),
                );
                let answer = this
                    .update_in(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Warning,
                            &format!("Discard unstaged changes to {}?", row.path),
                            Some(&detail),
                            &["Discard Unstaged Changes", "Cancel"],
                            cx,
                        )
                    })?
                    .await?;
                if answer == 0 {
                    store
                        .update(cx, |store, cx| {
                            store.apply_prepared_file_operation(prepared, cx)
                        })
                        .await?;
                }
                Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.preparing_operation = false;
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn selected_file(&self) -> Option<&Row> {
        self.rows
            .get(self.selected?)
            .filter(|row| row.change.is_some())
    }

    fn selected_repository(&self) -> Option<VcsRepositorySnapshot> {
        let selected = self
            .selected
            .and_then(|index| self.rows.get(index))
            .map(|row| &row.repository);
        self.repositories
            .iter()
            .find(|repository| {
                selected.is_some_and(|selected| selected == &repository.key)
                    || (selected.is_none() && self.repositories.len() == 1)
            })
            .cloned()
    }

    fn commit_staged(&mut self, _: &CommitStaged, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(repository) = self.selected_repository() {
            self.commit_repository(repository.key, window, cx);
        }
    }

    fn commit_repository(
        &mut self,
        key: VcsRepositoryId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preparing_operation {
            return;
        }
        let Some(repository) = self
            .repositories
            .iter()
            .find(|repository| repository.key == key)
            .cloned()
        else {
            return;
        };
        let Some(store) = self.store.clone() else {
            return;
        };
        crate::vcs_repository_ui::show_commit(
            self.workspace.clone(),
            store,
            repository,
            window,
            cx,
        );
    }

    fn switch_branch(&mut self, _: &SwitchBranch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(repository) = self.selected_repository() {
            self.switch_repository_branch(repository.key, window, cx);
        }
    }

    fn switch_repository_branch(
        &mut self,
        key: VcsRepositoryId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preparing_operation {
            return;
        }
        let Some(repository) = self
            .repositories
            .iter()
            .find(|repository| repository.key == key)
            .cloned()
        else {
            return;
        };
        let Some(state) = repository.repository_state.as_ref() else {
            return;
        };
        let branches = state
            .branches
            .iter()
            .filter(|branch| {
                state
                    .current_branch
                    .as_ref()
                    .is_none_or(|current| current.id != branch.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        if branches.is_empty() {
            self.error = Some("No other branches are available.".into());
            cx.notify();
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let selection = crate::picker_prompt::prompt(
            "Switch checkout branch",
            branches
                .iter()
                .map(|branch| branch.name.clone().into())
                .collect(),
            self.workspace.clone(),
            window,
            cx,
        );
        self.preparing_operation = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let Some(branch) = selection.await.and_then(|index| branches.get(index)) else {
                    return Ok(());
                };
                let prepared = store
                    .update(cx, |store, cx| {
                        store.prepare_repository_operation(
                            key,
                            RepositoryOperation::SwitchBranch {
                                branch_id: branch.id.clone(),
                            },
                            cx,
                        )
                    })
                    .await?;
                let detail = format!(
                    "Checkout: {}\nProject scope: {}\n{}",
                    repository.descriptor.root_uri,
                    repository.descriptor.scope_uri,
                    prepared.summary()
                );
                let answer = this
                    .update_in(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Warning,
                            &format!("Switch checkout to {}?", branch.name),
                            Some(&detail),
                            &["Switch Branch", "Cancel"],
                            cx,
                        )
                    })?
                    .await?;
                if answer == 0 {
                    store
                        .update(cx, |store, cx| {
                            store.apply_prepared_repository_operation(prepared, cx)
                        })
                        .await?;
                }
                Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.preparing_operation = false;
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn run_file_operation(&mut self, operation: FileOperation, cx: &mut Context<Self>) {
        if self.preparing_operation {
            return;
        }
        let Some(row) = self.selected_file().cloned() else {
            return;
        };
        let Some(store) = self.store.clone() else {
            return;
        };
        self.error = None;
        let task = store.update(cx, |store, cx| {
            store.run_file_operation(row.repository, row.path, operation, cx)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn open_diff(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index).cloned() else {
            return;
        };
        let Some(change) = &row.change else {
            return;
        };
        self.selected = Some(index);
        self.error = None;
        cx.notify();
        if change.kind != ContentKind::File {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let item_key = (row.repository.clone(), row.path.clone());
        if let Some(item) = self.opened.get(&item_key).and_then(WeakEntity::upgrade) {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.activate_item(&item, true, true, window, cx)
                })
                .log_err();
            return;
        }
        let task = store.update(cx, |store, cx| {
            store.open_diff(row.repository.clone(), row.path.clone(), cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let opened_diff = task.await?;
                this.update_in(cx, |this, window, cx| {
                    this.show_comparison(item_key, opened_diff, window, cx)
                })??;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{error:#}").into());
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
        cx.notify();
    }

    fn show_comparison(
        &mut self,
        key: (VcsRepositoryId, String),
        opened_diff: VcsDiff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Entity<SplittableEditor>> {
        let existing = self.opened.get(&key).and_then(WeakEntity::upgrade);
        let project = self.project.clone();
        let item = self.workspace.update(cx, |workspace, cx| {
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                existing
            } else {
                add_comparison(workspace, project, opened_diff, &key.1, window, cx)
            }
        })?;
        self.opened.insert(key, item.downgrade());
        Ok(item)
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|index| self.rows.get(index).cloned().map(|row| (index, row)))
            .map(|(index, row)| {
                let mut label = row.label;
                let (status, color) = match row.change.as_ref().map(|change| change.status) {
                    Some(ChangeStatus::Added) => ("A", Color::Success),
                    Some(ChangeStatus::Untracked) => ("?", Color::Success),
                    Some(ChangeStatus::Modified) => ("M", Color::Warning),
                    Some(ChangeStatus::Deleted) => ("D", Color::Error),
                    Some(ChangeStatus::Renamed) => ("R", Color::Muted),
                    None => ("", Color::Muted),
                };
                if let Some(change) = &row.change {
                    match change.kind {
                        ContentKind::File => {}
                        ContentKind::Binary => label.push_str(" (binary)"),
                        ContentKind::Symlink => label.push_str(" (symlink)"),
                        ContentKind::Submodule => label.push_str(" (submodule)"),
                    }
                    if let Some(old_path) = &change.old_path {
                        label = format!("{old_path} → {label}");
                    }
                    if change.old_mode.is_some()
                        && change.new_mode.is_some()
                        && change.old_mode != change.new_mode
                    {
                        label.push_str(" (mode changed)");
                    }
                }
                ListItem::new(index)
                    .inset(true)
                    .indent_level(row.depth)
                    .toggle_state(self.selected == Some(index))
                    .disabled(row.change.is_none())
                    .child(Label::new(label).color(color))
                    .end_slot(Label::new(status).color(color))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_diff(index, window, cx)),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

fn add_comparison(
    workspace: &mut Workspace,
    project: Entity<Project>,
    opened_diff: VcsDiff,
    path: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<SplittableEditor> {
    let multibuffer = cx.new(|cx| {
        // A comparison must not deduplicate against the file's ordinary singleton editor.
        let mut multibuffer =
            MultiBuffer::without_headers(opened_diff.buffer.read(cx).capability());
        let end = opened_diff.buffer.read(cx).max_point();
        multibuffer.set_excerpts_for_path(
            PathKey::sorted(0),
            opened_diff.buffer,
            [language::Point::zero()..end],
            0,
            cx,
        );
        multibuffer.add_diff(opened_diff.diff, cx);
        multibuffer.set_all_diff_hunks_expanded(cx);
        multibuffer.set_title(format!("{path} — Changes"), cx);
        multibuffer
    });
    let workspace_entity = cx.entity();
    let editor = cx.new(|cx| {
        let editor = SplittableEditor::new(
            EditorSettings::get_global(cx).diff_view_style,
            multibuffer,
            project,
            workspace_entity,
            window,
            cx,
        );
        editor.rhs_editor().update(cx, |editor, cx| {
            editor.set_should_serialize(false, cx);
        });
        editor
    });
    workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
    editor
}

fn rows_for_repositories(repositories: &[(&VcsRepositorySnapshot, Vec<Change>)]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (repository, changes) in repositories {
        rows.push(Row {
            repository: repository.key.clone(),
            path: String::new(),
            label: format!(
                "{} — {}",
                repository.descriptor.display_name, repository.descriptor.scope_uri
            ),
            depth: 0,
            change: None,
        });
        let mut entries = BTreeMap::new();
        for change in changes {
            let components: Vec<_> = change.path.split('/').collect();
            for depth in 1..components.len() {
                let path = components[..depth].join("/");
                entries
                    .entry(
                        components[..depth]
                            .iter()
                            .map(|part| part.to_string())
                            .collect::<Vec<_>>(),
                    )
                    .or_insert(Row {
                        repository: repository.key.clone(),
                        path,
                        label: components[depth - 1].to_owned(),
                        depth,
                        change: None,
                    });
            }
            entries.insert(
                components
                    .iter()
                    .map(|part| part.to_string())
                    .collect::<Vec<_>>(),
                Row {
                    repository: repository.key.clone(),
                    path: change.path.clone(),
                    label: components.last().unwrap_or(&"").to_string(),
                    depth: components.len(),
                    change: Some(change.clone()),
                },
            );
        }
        rows.extend(entries.into_values());
    }
    rows
}

impl EventEmitter<PanelEvent> for VcsPanel {}
impl Focusable for VcsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for VcsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let errors = self
            .store
            .as_ref()
            .map(|store| store.read(cx).errors())
            .unwrap_or_default();
        let operation_running = self.preparing_operation
            || self
                .store
                .as_ref()
                .is_some_and(|store| store.read(cx).file_operation_running());
        let file_actions = self.selected_file().map(|row| {
            let unresolved = self
                .store
                .as_ref()
                .is_some_and(|store| store.read(cx).file_operation_unresolved(&row.repository));
            let operations = self
                .store
                .as_ref()
                .map(|store| store.read(cx).file_operations_for_path(&row.repository, &row.path))
                .unwrap_or_default();
            v_flex()
                .px_2()
                .py_1()
                .gap_1()
                .child(Label::new(row.path.clone()).size(LabelSize::Small))
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .child(
                            Button::new("vcs-stage-file", "Stage File")
                                .disabled(
                                    operation_running
                                        || unresolved
                                        || !operations.contains(&FileOperation::Stage),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_file_operation(FileOperation::Stage, cx)
                                })),
                        )
                        .child(
                            Button::new("vcs-unstage-file", "Unstage File")
                                .disabled(
                                    operation_running
                                        || unresolved
                                        || !operations.contains(&FileOperation::Unstage),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_file_operation(FileOperation::Unstage, cx)
                                })),
                        )
                        .child(
                            Button::new("vcs-discard-file", "Discard Unstaged")
                                .disabled(
                                    operation_running
                                        || unresolved
                                        || !operations.contains(&FileOperation::Discard),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.discard_file(&DiscardFile, window, cx);
                                })),
                        ),
                )
                .when(operations.is_empty(), |element| {
                    element.child(
                        Label::new(row.change.as_ref().and_then(|change| change.unavailable_reason.clone()).unwrap_or_else(|| "This provider does not support file operations for this change.".into()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .when(unresolved && !operation_running, |element| {
                    element.child(
                        Label::new("Refresh to reconcile the previous operation.")
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                })
        });
        let has_errors = self.error.is_some()
            || !errors.is_empty()
            || self.repositories.iter().any(|repository| {
                repository.error.is_some()
                    || self.store.as_ref().is_some_and(|store| {
                        store
                            .read(cx)
                            .file_operation_error(&repository.key)
                            .is_some()
                    })
            });
        let repository_headers =
            self.repositories
                .iter()
                .enumerate()
                .map(|(repository_index, repository)| {
                    let key = repository.key.clone();
                    let repository_operations = self
                        .store
                        .as_ref()
                        .map(|store| store.read(cx).repository_operations(&key))
                        .unwrap_or_default();
                    let repository_blocked = operation_running
                        || repository.repository_state.is_none()
                        || repository.repository_state_error.is_some()
                        || self.store.as_ref().is_some_and(|store| {
                            store.read(cx).repository_operation_unresolved(&key)
                        });
                    let commit_key = key.clone();
                    let branch_key = key.clone();
                    let checkout_key = key.clone();
                    let has_checkout_catalog = self
                        .store
                        .as_ref()
                        .is_some_and(|store| store.read(cx).checkout_catalog_available(&key));
                    let operation_error = self
                        .store
                        .as_ref()
                        .and_then(|store| store.read(cx).file_operation_error(&key));
                    let baselines = repository
                        .descriptor
                        .baselines
                        .iter()
                        .enumerate()
                        .map(|(index, baseline)| {
                            let key = key.clone();
                            let baseline_id = baseline.id.clone();
                            Button::new(("baseline", index), baseline.label.clone())
                                .toggle_state(repository.selected_baseline == baseline.id)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.error = None;
                                    if let Some(store) = this.store.clone() {
                                        if let Err(error) = store.update(cx, |store, cx| {
                                            store.set_baseline(&key, baseline_id.clone(), cx)
                                        }) {
                                            this.error = Some(format!("{error:#}").into());
                                        }
                                    }
                                    cx.notify();
                                }))
                        })
                        .collect::<Vec<_>>();
                    v_flex()
                        .id(("vcs-repository", repository_index))
                        .px_2()
                        .py_1()
                        .gap_1()
                        .child(Label::new(repository.descriptor.display_name.clone()))
                        .child(
                            Label::new(repository.descriptor.scope_uri.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(h_flex().flex_wrap().gap_1().children(baselines))
                        .when_some(
                            repository
                                .repository_state
                                .as_ref()
                                .and_then(|state| state.current_branch.as_ref()),
                            |element, branch| {
                                element.child(
                                    Label::new(format!("Branch: {}", branch.name))
                                        .size(LabelSize::Small),
                                )
                            },
                        )
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_1()
                                .child(
                                    Button::new(
                                        ("vcs-commit-staged", repository_index),
                                        "Commit Staged",
                                    )
                                    .disabled(
                                        repository_blocked
                                            || !repository_operations
                                                .contains(&RepositoryOperationKind::Commit)
                                            || repository
                                                .repository_state
                                                .as_ref()
                                                .is_none_or(|state| state.staged_paths.is_empty()),
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.commit_repository(commit_key.clone(), window, cx)
                                        },
                                    )),
                                )
                                .child(
                                    Button::new(
                                        ("vcs-switch-branch", repository_index),
                                        "Switch Branch",
                                    )
                                    .disabled(
                                        repository_blocked
                                            || !repository_operations
                                                .contains(&RepositoryOperationKind::SwitchBranch),
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.switch_repository_branch(
                                                branch_key.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                ),
                        )
                        .when(has_checkout_catalog, |element| {
                            element.child(
                                Button::new(("vcs-checkouts", repository_index), "Checkouts")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(workspace) = this.workspace.upgrade() {
                                            workspace.update(cx, |workspace, cx| {
                                                git_ui_core::vcs_checkout_picker::show(
                                                    workspace,
                                                    Some(checkout_key.clone()),
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }
                                    })),
                            )
                        })
                        .when(repository.show_loading, |element| {
                            element.child(Label::new("Loading changes…").color(Color::Muted))
                        })
                        .when_some(repository.error.clone(), |element, error| {
                            element.child(Label::new(error).color(Color::Error))
                        })
                        .when_some(operation_error, |element, error| {
                            element.child(Label::new(error).color(Color::Error))
                        })
                        .when_some(
                            repository.repository_state_error.clone(),
                            |element, error| element.child(Label::new(error).color(Color::Error)),
                        )
                })
                .collect::<Vec<_>>();
        v_flex()
            .id("vcs-panel")
            .key_context("VcsPanel")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::stage_file))
            .on_action(cx.listener(Self::unstage_file))
            .on_action(cx.listener(Self::discard_file))
            .on_action(cx.listener(Self::commit_staged))
            .on_action(cx.listener(Self::switch_branch))
            .child(
                h_flex()
                    .p_2()
                    .justify_between()
                    .child(Label::new("Source Control"))
                    .child(
                        Button::new("vcs-diagnostics", "Diagnostics").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        show_diagnostics(workspace, window, cx)
                                    })
                                    .log_err();
                            },
                        )),
                    )
                    .child(
                        IconButton::new("refresh-vcs", IconName::ArrowCircle)
                            .disabled(operation_running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.error = None;
                                if let Some(store) = &this.store {
                                    store.update(cx, |store, cx| store.refresh_all(cx));
                                }
                            })),
                    ),
            )
            .children(repository_headers)
            .children(file_actions)
            .when(operation_running, |element| {
                element.child(
                    div()
                        .px_2()
                        .child(Label::new("Updating source control…").color(Color::Muted)),
                )
            })
            .children(
                errors
                    .into_iter()
                    .map(|error| Label::new(error).color(Color::Error)),
            )
            .when_some(self.error.clone(), |element, error| {
                element.child(Label::new(error).color(Color::Error))
            })
            .when_some(self.metadata_notice(), |element, notice| {
                element.child(
                    div()
                        .px_2()
                        .py_1()
                        .child(Label::new(notice).color(Color::Muted)),
                )
            })
            .when(
                self.rows.iter().all(|row| row.change.is_none())
                    && !has_errors
                    && !self
                        .repositories
                        .iter()
                        .any(|repository| repository.show_loading),
                |element| {
                    element.child(
                        Label::new(if self.repositories.is_empty() {
                            "No external source-control repository"
                        } else {
                            "No changes for this comparison"
                        })
                        .color(Color::Muted),
                    )
                },
            )
            .child(
                uniform_list(
                    "vcs-changes",
                    self.rows.len(),
                    cx.processor(Self::render_rows),
                )
                .track_scroll(&self.scroll_handle)
                .flex_1(),
            )
    }
}

impl Panel for VcsPanel {
    fn persistent_name() -> &'static str {
        "VcsPanel"
    }
    fn panel_key() -> &'static str {
        "vcs"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::GitBranch)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Source Control")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        3
    }
    fn enabled(&self, cx: &App) -> bool {
        !self.repositories.is_empty()
            || self
                .store
                .as_ref()
                .is_some_and(|store| !store.read(cx).errors().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::FakeFs;
    use settings::{DiffViewStyle, SettingsStore};
    use std::sync::Arc;
    use workspace::MultiWorkspace;

    #[gpui::test]
    async fn diagnostics_is_read_only_searchable_and_not_persisted(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            show_diagnostics(workspace, window, cx);
            let item = workspace
                .active_item(cx)
                .expect("Diagnostics tab must open");
            assert!(
                item.to_serializable_item_handle(cx).is_none(),
                "Temporary diagnostics must not enter workspace persistence"
            );
            assert_eq!(item.capability(cx), Capability::ReadOnly);
            assert!(item.to_searchable_item_handle(cx).is_some());
            let editor = item
                .act_as::<Editor>(cx)
                .expect("Diagnostics exposes its editor");
            assert_eq!(editor.read(cx).text(cx), "External VCS diagnostics");
            assert!(!workspace::SerializableItem::should_serialize(
                editor.read(cx),
                &editor::EditorEvent::Saved,
            ));
            assert!(
                editor
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            );
        });
    }

    #[gpui::test]
    async fn comparison_opens_beside_an_existing_editor_and_reuses_its_live_buffer(
        cx: &mut TestAppContext,
    ) {
        use buffer_diff::BufferDiff;
        use fs::Fs as _;
        use project::ProjectItem as _;
        use std::path::Path;
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project_root = Path::new(util::path!("/project"));
        let file_path = project_root.join("file.txt");
        fs.insert_tree(project_root, serde_json::json!({"file.txt": "saved\n"}))
            .await;
        let project = Project::test(fs.clone(), [project_root], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let normal = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    file_path.clone(),
                    workspace::OpenOptions {
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .expect("normal file editor");
        let normal_editor = cx
            .read(|cx| normal.act_as::<Editor>(cx))
            .expect("ordinary Editor");
        let buffer = normal_editor.read_with(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("file buffer")
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "unsaved\n")], None, cx);
        });
        let diff = cx.new(|cx| {
            let snapshot = buffer.read(cx).snapshot();
            BufferDiff::new(&snapshot.text, None, None, cx)
        });
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.set_base_text(Some("saved\n".into()), snapshot, cx)
        })
        .await;
        let panel = cx.new_window_entity(|_, cx| VcsPanel::new(workspace.downgrade(), project, cx));
        let worktree_id = buffer.read_with(cx, |buffer, cx| {
            buffer.project_path(cx).unwrap().worktree_id
        });
        let repository = VcsRepositoryId {
            worktree_id,
            extension_id: "fixture".into(),
            provider_id: "fixture".into(),
            repository_id: "repository".into(),
        };
        let key = (repository.clone(), "file.txt".to_owned());
        let comparison = panel
            .update_in(cx, |panel, window, cx| {
                panel.show_comparison(
                    key.clone(),
                    VcsDiff {
                        buffer: buffer.clone(),
                        diff: diff.clone(),
                    },
                    window,
                    cx,
                )
            })
            .expect("comparison");
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .active_item(cx)
                    .expect("active comparison")
                    .item_id(),
                comparison.entity_id()
            );
            assert_eq!(workspace.active_pane().read(cx).items_len(), 2);
            assert_eq!(
                comparison.read(cx).tab_content_text(0, cx).as_ref(),
                "file.txt — Changes"
            );
            assert!(
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .any(|item| item.item_id() == normal.item_id())
            );
        });
        panel.update(cx, |panel, _| {
            panel.rows = vec![Row {
                repository,
                path: "file.txt".into(),
                label: "file.txt".into(),
                depth: 1,
                change: Some(Change {
                    path: "file.txt".into(),
                    old_path: None,
                    status: ChangeStatus::Modified,
                    kind: ContentKind::File,
                    old_mode: None,
                    new_mode: None,
                    file_operations: None,
                    unavailable_reason: None,
                }),
            }];
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&normal_editor, true, true, window, cx);
        });
        let second_completion = panel
            .update_in(cx, |panel, window, cx| {
                panel.show_comparison(
                    key,
                    VcsDiff {
                        buffer: buffer.clone(),
                        diff,
                    },
                    window,
                    cx,
                )
            })
            .expect("second completed diff load");
        assert_eq!(
            second_completion.entity_id(),
            comparison.entity_id(),
            "concurrent loads must reuse the first inserted comparison"
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&normal_editor, true, true, window, cx);
        });
        panel.update_in(cx, |panel, window, cx| panel.open_diff(0, window, cx));
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_item(cx).unwrap().item_id(),
                comparison.entity_id(),
                "reopening activates the cached comparison"
            );
            assert_eq!(workspace.active_pane().read(cx).items_len(), 2);
        });
        normal_editor.update(cx, |editor, cx| {
            editor.edit(
                [(language::Point::zero()..language::Point::zero(), "live\n")],
                cx,
            );
        });
        comparison.read_with(cx, |comparison, cx| {
            assert_eq!(
                comparison.rhs_editor().read(cx).text(cx),
                "live\nunsaved\nsaved\n"
            );
            assert!(comparison.is_dirty(cx));
        });
        buffer.update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(end..end, "appended\n")], None, cx);
        });
        comparison.read_with(cx, |comparison, cx| {
            assert!(
                comparison
                    .rhs_editor()
                    .read(cx)
                    .text(cx)
                    .ends_with("appended\n")
            );
        });
        assert_eq!(
            fs.load(&file_path).await.unwrap(),
            "saved\n",
            "opening/reusing the comparison does not save or replace unsaved edits"
        );
    }

    #[gpui::test]
    async fn metadata_selection_is_not_an_error_and_cached_text_clears_prior_error(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let buffer = cx.new(|cx| Buffer::local("working content\n", cx));
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let editor = cx.new_window_entity(|window, cx| {
            SplittableEditor::new(
                DiffViewStyle::Unified,
                multibuffer,
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        });
        let panel = cx.new_window_entity(|_, cx| VcsPanel::new(workspace.downgrade(), project, cx));
        panel.update_in(cx, |panel, window, cx| {
            let repository = VcsRepositoryId {
                worktree_id: settings::WorktreeId::from_usize(1),
                extension_id: Arc::from("fixture"),
                provider_id: Arc::from("fixture"),
                repository_id: "opaque-repository".into(),
            };
            panel.rows = [
                ("binary.bin", ContentKind::Binary),
                ("working.txt", ContentKind::File),
            ]
            .into_iter()
            .map(|(path, kind)| Row {
                repository: repository.clone(),
                path: path.into(),
                label: path.into(),
                depth: 1,
                change: Some(Change {
                    path: path.into(),
                    old_path: None,
                    status: ChangeStatus::Modified,
                    kind,
                    old_mode: None,
                    new_mode: None,
                    file_operations: None,
                    unavailable_reason: None,
                }),
            })
            .collect();
            panel
                .opened
                .insert((repository, "working.txt".into()), editor.downgrade());
            panel.open_diff(0, window, cx);
            assert!(
                panel.error.is_none(),
                "Metadata-only content must not become a provider error"
            );
            assert!(panel.metadata_notice().is_some());
            assert_eq!(
                panel.selected_file().map(|row| row.path.as_str()),
                Some("binary.bin"),
                "metadata-only changes remain selectable for capability-gated file operations"
            );
            panel.error = Some("previous provider failure".into());
            panel.open_diff(1, window, cx);
            assert!(
                panel.error.is_none(),
                "Opening an already cached text diff must clear a previous error"
            );
            assert_eq!(panel.selected, Some(1));
            assert!(panel.metadata_notice().is_none());
            panel.open_diff(0, window, cx);
            panel.select_next(&menu::SelectNext, window, cx);
            assert!(panel.metadata_notice().is_none());
        });
    }
}
