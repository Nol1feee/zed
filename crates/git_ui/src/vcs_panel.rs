use std::collections::HashMap;
use std::{any::TypeId, ops::Range, path::Path, sync::Arc};

use crate::git_panel::{self, GitPanel};
use crate::git_panel_settings::GitPanelSettings;
use anyhow::Result;
use editor::{Editor, EditorSettings, SplittableEditor};
use file_icons::FileIcons;
use gpui::{
    Action, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel,
    ScrollStrategy, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, actions,
    uniform_list,
};
use language::{Buffer, Capability};
use multi_buffer::{MultiBuffer, PathKey};
use project::vcs_store::VcsDiff;
use project::{
    Project, ProjectPath,
    vcs_store::{VcsRepositoryId, VcsRepositorySnapshot, VcsStore},
};
use settings::{GitPanelClickBehavior, Settings};
use ui::{
    Button, ButtonLike, Color, ContextMenu, ContextMenuEntry, IconButton, IconName, Label,
    ListItem, ListItemSpacing, Tab, Tooltip, prelude::*, right_click_menu,
};
use util::{ResultExt as _, rel_path::RelPath};
use vcs::{
    Change, ChangeStatus, ContentKind, FileOperation, RepositoryOperation, RepositoryOperationKind,
};
use workspace::{Item, Workspace, notifications::NotifyTaskExt, searchable::SearchableItemHandle};

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
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<GitPanel>(window, cx);
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

pub(crate) fn discard_unavailable_reason(
    store: Option<&Entity<VcsStore>>,
    repository: &VcsRepositoryId,
    path: &str,
    cx: &App,
) -> Option<SharedString> {
    let Some(store) = store else {
        return Some("Source-control provider is unavailable.".into());
    };
    let store = store.read(cx);
    if store.file_operation_running() {
        return Some("Another source-control operation is running.".into());
    }
    if store.file_operation_unresolved(repository)
        || store.repository_operation_unresolved(repository)
    {
        return Some("Refresh source control to reconcile the previous operation.".into());
    }
    if store
        .file_operations_for_path(repository, path)
        .contains(&FileOperation::Discard)
    {
        return None;
    }
    Some(
        store
            .changes(repository, cx)
            .into_iter()
            .find(|change| change.path == path)
            .and_then(|change| change.unavailable_reason)
            .unwrap_or_else(|| {
                "The provider does not allow discarding changes to this file in this checkout."
                    .into()
            })
            .into(),
    )
}

pub(crate) fn discard_context_menu(
    mut menu: ContextMenu,
    unavailable_reason: Option<SharedString>,
    handler: impl Fn(&mut Window, &mut App) + 'static,
) -> ContextMenu {
    menu.push_item(
        ContextMenuEntry::new("Discard Changes")
            .disabled(unavailable_reason.is_some())
            .handler(handler),
    );
    if let Some(reason) = unavailable_reason {
        menu = menu.label(reason);
    }
    menu
}

pub(crate) fn discard_file_changes(
    store: Entity<VcsStore>,
    repository: VcsRepositoryId,
    path: String,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    if let Some(reason) = discard_unavailable_reason(Some(&store), &repository, &path, cx) {
        return Task::ready(Err(anyhow::anyhow!(reason.to_string())));
    }
    let Some(snapshot) = store
        .read(cx)
        .repositories()
        .into_iter()
        .find(|snapshot| snapshot.key == repository)
    else {
        return Task::ready(Err(anyhow::anyhow!(
            "Source-control repository is no longer available."
        )));
    };
    let preparation = store.update(cx, |store, cx| {
        store.prepare_file_operation(repository, path.clone(), FileOperation::Discard, cx)
    });
    window.spawn(cx, async move |cx| {
        let prepared = preparation.await?;
        let detail = format!(
            "Project: {}\nScope: {}\n\n{}\n\nAffected paths:\n{}",
            snapshot.descriptor.display_name,
            snapshot.descriptor.scope_uri,
            prepared.summary().unwrap_or("The working file will be restored to its staged version. Staged changes will be preserved."),
            prepared.affected_paths().join("\n"),
        );
        let answer = cx.update(|window, cx| window.prompt(
            PromptLevel::Warning,
            &format!("Discard unstaged changes to {path}?"),
            Some(&detail),
            &["Discard Unstaged Changes", "Cancel"],
            cx,
        ))?.await?;
        if answer == 0 {
            store.update(cx, |store, cx| store.apply_prepared_file_operation(prepared, cx)).await?;
        }
        Ok(())
    })
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
    change: Option<Change>,
}

pub struct VcsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Option<Entity<VcsStore>>,
    focus: FocusHandle,
    repositories: Vec<VcsRepositorySnapshot>,
    selected_repository_id: Option<VcsRepositoryId>,
    rows: Vec<Row>,
    selected: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    error: Option<SharedString>,
    preparing_operation: bool,
    opened: HashMap<(VcsRepositoryId, String), WeakEntity<SplittableEditor>>,
    _subscription: Option<Subscription>,
}

impl VcsPanel {
    pub(crate) fn new(
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
            repositories: Vec::new(),
            selected_repository_id: None,
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
        if !self
            .repositories
            .iter()
            .any(|repository| Some(&repository.key) == self.selected_repository_id.as_ref())
        {
            self.selected_repository_id = self
                .repositories
                .first()
                .map(|repository| repository.key.clone());
        }
        let changes = self
            .repositories
            .iter()
            .filter(|repository| Some(&repository.key) == self.selected_repository_id.as_ref())
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
        let Some(row) = self.selected_file().cloned() else {
            return;
        };
        self.discard_path(row.repository, row.path, window, cx);
    }

    fn discard_path(
        &mut self,
        repository: VcsRepositoryId,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preparing_operation {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        self.error = None;
        self.preparing_operation = true;
        let operation = discard_file_changes(store, repository, path, window, cx);
        cx.spawn(async move |this, cx| {
            let result = operation.await;
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
        self.repositories
            .iter()
            .find(|repository| Some(&repository.key) == self.selected_repository_id.as_ref())
            .cloned()
    }

    pub(crate) fn commit_staged(
        &mut self,
        _: &CommitStaged,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if !store
            .read(cx)
            .repository_operations(&key)
            .contains(&RepositoryOperationKind::Commit)
        {
            self.unsupported_action(cx);
            return;
        }
        crate::vcs_repository_ui::show_commit(
            self.workspace.clone(),
            store,
            repository,
            window,
            cx,
        );
    }

    pub(crate) fn switch_branch(
        &mut self,
        _: &SwitchBranch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if !store
            .read(cx)
            .file_operations_for_path(&row.repository, &row.path)
            .contains(&operation)
        {
            self.error = Some(
                row.change
                    .as_ref()
                    .and_then(|change| change.unavailable_reason.clone())
                    .unwrap_or_else(|| {
                        "This operation is unavailable for this file or checkout.".into()
                    })
                    .into(),
            );
            cx.notify();
            return;
        }
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
        self.open_diff_with_focus(index, true, window, cx);
    }

    fn open_diff_with_focus(
        &mut self,
        index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self.workspace
            .update(cx, |workspace, cx| {
                crate::vcs_project_diff::deploy_at_with_focus(
                    workspace,
                    row.repository,
                    Some(row.path),
                    focus,
                    window,
                    cx,
                );
            })
            .log_err();
    }

    fn open_entry_on_click(
        &mut self,
        index: usize,
        secondary: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(index);
        let primary = GitPanelSettings::get_global(cx).entry_primary_click_action;
        match git_panel::entry_click_action(primary, secondary) {
            GitPanelClickBehavior::ProjectDiff => {
                self.open_diff_with_focus(index, false, window, cx);
                self.focus.focus(window, cx);
            }
            GitPanelClickBehavior::FileDiff => self.open_file_diff(index, window, cx),
            GitPanelClickBehavior::ViewFile => self.view_file(&git::ViewFile, window, cx),
        }
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.selected {
            self.open_file_diff(index, window, cx);
        }
    }

    fn open_file_diff(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
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
        if let Some(item) = self
            .opened
            .get(&(row.repository.clone(), row.path.clone()))
            .and_then(WeakEntity::upgrade)
        {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.activate_item(&item, true, true, window, cx)
                })
                .log_err();
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let key = (row.repository.clone(), row.path.clone());
        let task = store.update(cx, |store, cx| {
            store.open_diff(row.repository, row.path, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let opened_diff = task.await?;
                this.update_in(cx, |this, window, cx| {
                    this.show_comparison(key, opened_diff, window, cx)
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
    }

    fn view_file(&mut self, _: &git::ViewFile, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_file().cloned() else {
            return;
        };
        self.error = None;
        cx.notify();
        if row
            .change
            .as_ref()
            .is_none_or(|change| change.kind != ContentKind::File)
        {
            return;
        }
        let path = match RelPath::from_unix_str(&row.path) {
            Ok(path) => ProjectPath {
                worktree_id: row.repository.worktree_id,
                path: Arc::from(path),
            },
            Err(error) => {
                self.error = Some(format!("{error:#}").into());
                return;
            }
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(path, None, false, false, true, window, cx)
                    .detach_and_notify_err(self.workspace.clone(), window, cx);
            })
            .log_err();
    }

    pub(crate) fn select_repository(&mut self, key: VcsRepositoryId, cx: &mut Context<Self>) {
        if self.selected_repository_id.as_ref() != Some(&key) {
            self.selected_repository_id = Some(key);
            self.selected = None;
            self.error = None;
            self.update_entries(cx);
        }
    }

    pub(crate) fn selected_repository_id(&self) -> Option<VcsRepositoryId> {
        self.selected_repository_id.clone()
    }

    pub(crate) fn has_repositories_or_errors(&self, cx: &App) -> bool {
        !self.repositories.is_empty()
            || self
                .store
                .as_ref()
                .is_some_and(|store| !store.read(cx).errors().is_empty())
    }

    pub(crate) fn change_count(&self) -> usize {
        self.rows.iter().filter(|row| row.change.is_some()).count()
    }

    fn show_project_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.selected_repository_id() else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                crate::vcs_project_diff::deploy_at(workspace, key, None, window, cx);
            })
            .log_err();
    }

    fn next_entry(
        &mut self,
        _: &git_panel::NextEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_next(&menu::SelectNext, window, cx);
        if let Some(index) = self.selected {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
            self.open_diff_with_focus(index, false, window, cx);
        }
    }

    fn previous_entry(
        &mut self,
        _: &git_panel::PreviousEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_previous(&menu::SelectPrevious, window, cx);
        if let Some(index) = self.selected {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
            self.open_diff_with_focus(index, false, window, cx);
        }
    }

    fn first_entry(
        &mut self,
        _: &git_panel::FirstEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = self.rows.iter().position(|row| row.change.is_some());
        if let Some(index) = self.selected {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
            self.open_diff_with_focus(index, false, window, cx);
        }
    }

    fn last_entry(
        &mut self,
        _: &git_panel::LastEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = self.rows.iter().rposition(|row| row.change.is_some());
        if let Some(index) = self.selected {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
            self.open_diff_with_focus(index, false, window, cx);
        }
    }

    fn copy_path(
        &mut self,
        _: &zed_actions::workspace::CopyPath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.selected_file()
            && let Some(repository) = self.selected_repository()
        {
            match vcs::local_path(&repository.descriptor.scope_uri) {
                Ok(scope) => cx.write_to_clipboard(ClipboardItem::new_string(
                    scope.join(&row.path).to_string_lossy().into_owned(),
                )),
                Err(error) => self.error = Some(format!("{error:#}").into()),
            }
        }
    }

    fn copy_relative_path(
        &mut self,
        _: &zed_actions::workspace::CopyRelativePath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.selected_file() {
            cx.write_to_clipboard(ClipboardItem::new_string(row.path.clone()));
        }
    }

    fn toggle_staged(&mut self, _: &git::ToggleStaged, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_file() else {
            return;
        };
        let staged = self
            .selected_repository()
            .and_then(|repository| repository.repository_state)
            .is_some_and(|state| state.staged_paths.contains(&row.path));
        self.run_file_operation(
            if staged {
                FileOperation::Unstage
            } else {
                FileOperation::Stage
            },
            cx,
        );
    }

    pub(crate) fn unsupported_action(&mut self, cx: &mut Context<Self>) {
        self.error = Some("This action is unavailable for the selected external repository. Use its supported file or repository actions.".into());
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
                let Some(change) = row.change.as_ref() else {
                    return ListItem::new(index)
                        .inset(true)
                        .disabled(true)
                        .child(Label::new(row.label).color(Color::Muted))
                        .into_any_element();
                };
                let (status, color) = match change.status {
                    ChangeStatus::Added => ("A", Color::VersionControlAdded),
                    ChangeStatus::Untracked => ("?", Color::VersionControlAdded),
                    ChangeStatus::Modified => ("M", Color::VersionControlModified),
                    ChangeStatus::Deleted => ("D", Color::Disabled),
                    ChangeStatus::Renamed => ("R", Color::VersionControlModified),
                };
                let path = Path::new(&row.path);
                let parent = path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|path| path.to_string_lossy().into_owned());
                let icon = GitPanelSettings::get_global(cx)
                    .file_icons
                    .then(|| FileIcons::get_icon(path, cx))
                    .flatten();
                let panel = cx.entity().downgrade();
                let repository = row.repository.clone();
                let file_path = row.path.clone();
                let item = ListItem::new(index)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(self.selected == Some(index))
                    .child(
                        h_flex()
                            .gap_1()
                            .overflow_hidden()
                            .when_some(icon, |this, icon| {
                                this.child(Icon::from_path(icon).size(IconSize::Small))
                            })
                            .child(Label::new(row.label).color(color))
                            .when_some(parent, |this, parent| {
                                this.child(
                                    Label::new(parent)
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                )
                            }),
                    )
                    .end_slot(Label::new(status).color(color).size(LabelSize::Small))
                    .tooltip(Tooltip::text(match &change.old_path {
                        Some(old) => format!("{old} → {}", row.path),
                        None => row.path.clone(),
                    }))
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            this.open_entry_on_click(index, event.click_count() > 1, window, cx)
                        }),
                    );
                right_click_menu(("vcs-file-menu", index))
                    .trigger(move |_, _, _| item)
                    .menu(move |window, cx| {
                        let reason = panel
                            .update(cx, |panel, cx| {
                                panel.selected = panel.rows.iter().position(|row| {
                                    row.repository == repository && row.path == file_path
                                });
                                cx.notify();
                                if panel.preparing_operation {
                                    Some(
                                        "Another source-control operation is being prepared."
                                            .into(),
                                    )
                                } else {
                                    discard_unavailable_reason(
                                        panel.store.as_ref(),
                                        &repository,
                                        &file_path,
                                        cx,
                                    )
                                }
                            })
                            .unwrap_or_else(|_| {
                                Some("Source-control panel is no longer available.".into())
                            });
                        let panel = panel.clone();
                        let repository = repository.clone();
                        let file_path = file_path.clone();
                        ContextMenu::build(window, cx, move |menu, _, _| {
                            discard_context_menu(menu, reason, move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.discard_path(
                                            repository.clone(),
                                            file_path.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                    .log_err();
                            })
                        })
                    })
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
    let mut rows = repositories
        .iter()
        .flat_map(|(repository, changes)| {
            changes.iter().map(|change| Row {
                repository: repository.key.clone(),
                path: change.path.clone(),
                label: change
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&change.path)
                    .to_owned(),
                change: Some(change.clone()),
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.path.cmp(&right.path));
    rows
}

impl Focusable for VcsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for VcsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let repository = self.selected_repository();
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
        let repository_operations = repository
            .as_ref()
            .and_then(|repository| {
                self.store
                    .as_ref()
                    .map(|store| store.read(cx).repository_operations(&repository.key))
            })
            .unwrap_or_default();
        let file_operations = self
            .selected_file()
            .and_then(|row| {
                self.store.as_ref().map(|store| {
                    store
                        .read(cx)
                        .file_operations_for_path(&row.repository, &row.path)
                })
            })
            .unwrap_or_default();
        let discard_reason = self.selected_file().and_then(|row| {
            discard_unavailable_reason(self.store.as_ref(), &row.repository, &row.path, cx)
        });
        let unresolved = repository.as_ref().is_some_and(|repository| {
            self.store.as_ref().is_some_and(|store| {
                let store = store.read(cx);
                store.file_operation_unresolved(&repository.key)
                    || store.repository_operation_unresolved(&repository.key)
            })
        });
        let blocked = operation_running || unresolved;
        let operation_error = repository.as_ref().and_then(|repository| {
            self.store
                .as_ref()
                .and_then(|store| store.read(cx).file_operation_error(&repository.key))
        });
        let read_only_reason = repository.as_ref().and_then(|repository| {
            let store = self.store.as_ref()?.read(cx);
            if !store.file_operations(&repository.key).is_empty() || !repository_operations.is_empty() { return None; }
            Some(self.selected_file().and_then(|row| row.change.as_ref()).and_then(|change| change.unavailable_reason.clone())
                .unwrap_or_else(|| "Read-only repository: the provider does not allow changes in this checkout.".into()))
        });
        let count = self.change_count();
        let loading = repository
            .as_ref()
            .is_some_and(|repository| repository.show_loading);
        let repository_error = repository
            .as_ref()
            .and_then(|repository| repository.error.clone());
        let repository_state_error = repository
            .as_ref()
            .and_then(|repository| repository.repository_state_error.clone());
        let mut context = gpui::KeyContext::new_with_defaults();
        context.add("GitPanel");
        context.add("menu");
        context.add("ChangesList");
        v_flex()
            .id("vcs-changes-body")
            .key_context(context)
            .track_focus(&self.focus)
            .flex_1()
            .size_full()
            .overflow_hidden()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::next_entry))
            .on_action(cx.listener(Self::previous_entry))
            .on_action(cx.listener(Self::first_entry))
            .on_action(cx.listener(Self::last_entry))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(Self::view_file))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::copy_relative_path))
            .on_action(cx.listener(Self::toggle_staged))
            .on_action(cx.listener(Self::stage_file))
            .on_action(cx.listener(Self::unstage_file))
            .on_action(cx.listener(|this, action: &DiscardFile, window, cx| {
                if this.focus.is_focused(window) {
                    this.discard_file(action, window, cx);
                }
            }))
            .on_action(cx.listener(Self::commit_staged))
            .on_action(cx.listener(Self::switch_branch))
            .on_action(cx.listener(|this, _: &git::StageFile, _, cx| {
                this.run_file_operation(FileOperation::Stage, cx)
            }))
            .on_action(cx.listener(|this, _: &git::UnstageFile, _, cx| {
                this.run_file_operation(FileOperation::Unstage, cx)
            }))
            .on_action(cx.listener(|this, _: &git::RestoreFile, window, cx| {
                if this.focus.is_focused(window) {
                    this.discard_file(&DiscardFile, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &git::Commit, window, cx| {
                this.commit_staged(&CommitStaged, window, cx)
            }))
            .child(
                h_flex()
                    .min_h(Tab::container_height(cx))
                    .w_full()
                    .pl_1()
                    .pr_2()
                    .gap_1()
                    .justify_between()
                    .child(
                        ButtonLike::new("diff-button")
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Icon::new(IconName::Diff)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new("View Diff")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .tooltip(Tooltip::for_action_title(
                                "View Diff",
                                &crate::project_diff::Diff,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_project_diff(window, cx)
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(count.to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new("vcs-diagnostics", IconName::Ellipsis)
                                    .tooltip(Tooltip::text("Provider diagnostics"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.workspace
                                            .update(cx, |workspace, cx| {
                                                show_diagnostics(workspace, window, cx)
                                            })
                                            .log_err();
                                    })),
                            )
                            .child(
                                IconButton::new("refresh-vcs", IconName::ArrowCircle)
                                    .disabled(operation_running)
                                    .tooltip(Tooltip::text("Refresh changes"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.error = None;
                                        if let Some(store) = &this.store {
                                            store.update(cx, |store, cx| store.refresh_all(cx));
                                        }
                                    })),
                            ),
                    ),
            )
            .when_some(repository.clone(), |this, repository| {
                this.child(
                    h_flex().px_2().py_1().gap_1().flex_wrap().children(
                        repository
                            .descriptor
                            .baselines
                            .iter()
                            .enumerate()
                            .map(|(index, baseline)| {
                                let key = repository.key.clone();
                                let baseline_id = baseline.id.clone();
                                Button::new(("vcs-baseline", index), baseline.label.clone())
                                    .label_size(LabelSize::Small)
                                    .toggle_state(repository.selected_baseline == baseline.id)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.error = None;
                                        if let Some(store) = &this.store
                                            && let Err(error) = store.update(cx, |store, cx| {
                                                store.set_baseline(&key, baseline_id.clone(), cx)
                                            })
                                        {
                                            this.error = Some(format!("{error:#}").into());
                                        }
                                        cx.notify();
                                    }))
                            })
                            .collect::<Vec<_>>(),
                    ),
                )
            })
            .when(loading, |this| {
                this.child(
                    div()
                        .px_2()
                        .child(Label::new("Loading changes…").color(Color::Muted)),
                )
            })
            .children(
                errors
                    .into_iter()
                    .chain(repository_error)
                    .chain(repository_state_error)
                    .chain(operation_error)
                    .chain(self.error.clone())
                    .map(|error| {
                        div()
                            .px_2()
                            .py_1()
                            .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    }),
            )
            .when_some(self.metadata_notice(), |this, notice| {
                this.child(
                    div().px_2().child(
                        Label::new(notice)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .when(count == 0 && !loading, |this| {
                this.child(
                    div().p_4().child(
                        Label::new(if repository.is_some() {
                            "No changes for this comparison"
                        } else {
                            "No repository available"
                        })
                        .color(Color::Muted),
                    ),
                )
            })
            .child(
                uniform_list(
                    "vcs-changes",
                    self.rows.len(),
                    cx.processor(Self::render_rows),
                )
                .track_scroll(&self.scroll_handle)
                .flex_1(),
            )
            .when_some(
                read_only_reason.filter(|_| discard_reason.is_none()),
                |this, reason| {
                    this.child(
                        div().px_2().py_1().child(
                            Label::new(reason)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                },
            )
            .when(unresolved, |this| {
                this.child(
                    div().px_2().py_1().child(
                        Label::new("Refresh to reconcile the previous operation.")
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    ),
                )
            })
            .when(operation_running, |this| {
                this.child(
                    div().px_2().py_1().child(
                        Label::new("Updating source control…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .when(self.selected_file().is_some(), |this| {
                this.child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .child(
                            Button::new("vcs-stage-file", "Stage File")
                                .label_size(LabelSize::Small)
                                .disabled(
                                    blocked || !file_operations.contains(&FileOperation::Stage),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_file_operation(FileOperation::Stage, cx)
                                })),
                        )
                        .child(
                            Button::new("vcs-unstage-file", "Unstage File")
                                .label_size(LabelSize::Small)
                                .disabled(
                                    blocked || !file_operations.contains(&FileOperation::Unstage),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.run_file_operation(FileOperation::Unstage, cx)
                                })),
                        )
                        .child(
                            Button::new("vcs-discard-file", "Discard Changes")
                                .label_size(LabelSize::Small)
                                .disabled(blocked || discard_reason.is_some())
                                .tooltip(Tooltip::text(discard_reason.clone().unwrap_or_else(
                                    || "Discard unstaged changes to this file".into(),
                                )))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.discard_file(&DiscardFile, window, cx)
                                })),
                        ),
                )
            })
            .when_some(discard_reason, |this, reason| {
                this.child(
                    div().px_2().py_1().child(
                        Label::new(reason)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .when(
                repository_operations.contains(&RepositoryOperationKind::Commit),
                |this| {
                    this.child(
                        div().px_2().py_1().child(
                            Button::new("vcs-commit-staged", "Commit Staged")
                                .full_width()
                                .disabled(
                                    blocked
                                        || repository
                                            .as_ref()
                                            .and_then(|repository| {
                                                repository.repository_state.as_ref()
                                            })
                                            .is_none_or(|state| state.staged_paths.is_empty()),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.commit_staged(&CommitStaged, window, cx)
                                })),
                        ),
                    )
                },
            )
            .when_some(repository, |this, repository| {
                let branch = repository
                    .repository_state
                    .as_ref()
                    .and_then(|state| state.current_branch.as_ref())
                    .map(|branch| branch.name.clone());
                let has_checkouts = self.store.as_ref().is_some_and(|store| {
                    store.read(cx).checkout_catalog_available(&repository.key)
                });
                let key = repository.key.clone();
                this.child(
                    h_flex()
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_1()
                        .justify_between()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::GitBranch)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Button::new(
                                        "vcs-repository-selector",
                                        repository.descriptor.display_name,
                                    )
                                    .size(ButtonSize::None)
                                    .label_size(LabelSize::Small)
                                    .truncate(true)
                                    .tooltip(Tooltip::text("Switch Active Repository"))
                                    .on_click(
                                        |_, window, cx| {
                                            window.dispatch_action(
                                                zed_actions::git::SelectRepo.boxed_clone(),
                                                cx,
                                            )
                                        },
                                    ),
                                )
                                .when_some(branch, |this, branch| {
                                    this.child(Label::new("/").color(Color::Muted)).child(
                                        Button::new("vcs-branch-selector", branch)
                                            .size(ButtonSize::None)
                                            .label_size(LabelSize::Small)
                                            .truncate(true)
                                            .disabled(
                                                blocked
                                                    || !repository_operations.contains(
                                                        &RepositoryOperationKind::SwitchBranch,
                                                    ),
                                            )
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.switch_branch(&SwitchBranch, window, cx)
                                            })),
                                    )
                                }),
                        )
                        .when(has_checkouts, |this| {
                            this.child(
                                Button::new("vcs-checkouts", "Checkouts")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.workspace
                                            .update(cx, |workspace, cx| {
                                                git_ui_core::vcs_checkout_picker::show(
                                                    workspace,
                                                    Some(key.clone()),
                                                    window,
                                                    cx,
                                                )
                                            })
                                            .log_err();
                                    })),
                            )
                        }),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal};
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
        panel.update_in(cx, |panel, window, cx| {
            panel.open_entry_on_click(0, true, window, cx)
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_item(cx).unwrap().item_id(),
                comparison.entity_id(),
                "reopening activates the cached comparison"
            );
            assert_eq!(workspace.active_pane().read(cx).items_len(), 2);
        });
        for (setting, expected_item) in [
            (GitPanelClickBehavior::FileDiff, comparison.entity_id()),
            (GitPanelClickBehavior::ViewFile, normal.item_id()),
        ] {
            cx.update(|_, cx| {
                SettingsStore::update_global(cx, |store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings
                            .git_panel
                            .get_or_insert_default()
                            .entry_primary_click_action = Some(setting);
                    });
                })
            });
            panel.update_in(cx, |panel, window, cx| {
                panel.open_entry_on_click(0, false, window, cx)
            });
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, cx| {
                assert_eq!(
                    workspace.active_item(cx).expect("clicked item").item_id(),
                    expected_item
                );
                assert_eq!(workspace.active_pane().read(cx).items_len(), 2);
            });
        }
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
            panel.open_file_diff(0, window, cx);
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
            panel.open_file_diff(1, window, cx);
            assert!(
                panel.error.is_none(),
                "Opening an already cached text diff must clear a previous error"
            );
            assert_eq!(panel.selected, Some(1));
            assert!(panel.metadata_notice().is_none());
            panel.open_file_diff(0, window, cx);
            panel.select_next(&menu::SelectNext, window, cx);
            assert!(panel.metadata_notice().is_none());
        });
    }
}
