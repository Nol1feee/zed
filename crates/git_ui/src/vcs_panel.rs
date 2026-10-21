use std::collections::{HashMap, HashSet};
use std::{any::TypeId, ops::Range, path::Path, rc::Rc, sync::Arc};

use crate::git_panel::{self, GitPanel, GitTreeDirEntry, Section, StatusTreeRow, TreeKey};
use crate::git_panel_settings::GitPanelSettings;
use anyhow::Result;
use editor::{Editor, EditorSettings, SplittableEditor};
use git::status::{FileStatus, StatusCode};
use gpui::{
    Anchor, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel,
    ScrollStrategy, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, actions,
    uniform_list,
};
use language::{Buffer, Capability};
use multi_buffer::{MultiBuffer, PathKey};
use project::vcs_store::VcsDiff;
use project::{
    Project, ProjectPath,
    vcs_store::{VcsRepositoryId, VcsRepositorySnapshot, VcsStore, VcsStoreEvent},
};
use settings::{GitPanelClickBehavior, GitPanelGroupBy, GitPanelSortBy, Settings, SettingsStore};
use ui::{
    ButtonLike, Checkbox, Color, ContextMenu, ContextMenuEntry, ElevationIndex, IconButton,
    IconName, Label, PopoverMenu, SplitButton, Tab, ToggleState, Tooltip, prelude::*,
    right_click_menu,
};
use util::{ResultExt as _, rel_path::RelPath};
use vcs::{
    Change, ChangeStatus, ComparisonPurpose, ContentKind, ContentSource, FileOperation,
    RepositoryOperation, RepositoryOperationKind,
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
    baseline: Option<&str>,
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
    let operations = baseline
        .map(|baseline| store.file_operations_for_baseline(repository, path, baseline))
        .unwrap_or_else(|| store.file_operations_for_path(repository, path));
    if operations.contains(&FileOperation::Discard) {
        let changes = baseline
            .map(|baseline| store.changes_for_baseline(repository, baseline, cx))
            .unwrap_or_else(|| store.changes(repository, cx));
        if changes.iter().any(|change| {
            change.path == path
                && change.staged_status.is_some()
                && change.unstaged_status.is_none()
        }) {
            return Some("This file has no unstaged changes to discard.".into());
        }
        return None;
    }
    Some(
        baseline
            .map(|baseline| store.changes_for_baseline(repository, baseline, cx))
            .unwrap_or_else(|| store.changes(repository, cx))
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

#[cfg(test)]
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
    baseline: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    if let Some(reason) =
        discard_unavailable_reason(Some(&store), &repository, &path, baseline.as_deref(), cx)
    {
        return Task::ready(Err(anyhow::anyhow!(reason.to_string())));
    }
    confirm_file_operation(
        store,
        repository,
        path,
        baseline,
        FileOperation::Discard,
        window,
        cx,
    )
}

fn confirm_file_operation(
    store: Entity<VcsStore>,
    repository: VcsRepositoryId,
    path: String,
    baseline: Option<String>,
    operation: FileOperation,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
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
    let (title, confirm, fallback) = match operation {
        FileOperation::Discard => (
            format!("Discard unstaged changes to {path}?"),
            "Discard Unstaged Changes",
            "The working file will be restored to its staged version. Staged changes will be preserved.",
        ),
        FileOperation::Ignore => {
            let Some(config) = &snapshot.descriptor.ignore_config_path else {
                return Task::ready(Err(anyhow::anyhow!(
                    "The provider did not declare an ignore configuration."
                )));
            };
            (
                format!("Add {path} to {config}?"),
                "Add Ignore Rule",
                "The selected file will be kept.",
            )
        }
        _ => {
            return Task::ready(Err(anyhow::anyhow!(
                "This operation does not use file-change confirmation."
            )));
        }
    };
    let preparation = store.update(cx, |store, cx| match baseline {
        Some(baseline) => {
            store.prepare_file_operation_for_baseline(repository, path, operation, baseline, cx)
        }
        None => store.prepare_file_operation(repository, path, operation, cx),
    });
    window.spawn(cx, async move |cx| {
        let prepared = preparation.await?;
        let detail = format!(
            "Project: {}\nScope: {}\n\n{}\n\nAffected paths:\n{}",
            snapshot.descriptor.display_name,
            snapshot.descriptor.scope_uri,
            prepared.summary().unwrap_or(fallback),
            prepared.affected_paths().join("\n"),
        );
        let answer = cx
            .update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    &title,
                    Some(&detail),
                    &[confirm, "Cancel"],
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
    presentation: RowPresentation,
}

#[derive(Clone, Default)]
struct RowPresentation {
    section: Option<Section>,
    comparison: Option<ComparisonPurpose>,
    directory: Option<GitTreeDirEntry>,
    depth: usize,
    hidden: bool,
    empty_section: bool,
}

impl Row {
    fn baseline(&self, repository: &VcsRepositorySnapshot) -> String {
        self.presentation
            .comparison
            .and_then(|purpose| {
                repository
                    .descriptor
                    .baselines
                    .iter()
                    .find(|baseline| baseline.purpose == Some(purpose))
            })
            .map(|baseline| baseline.id.clone())
            .unwrap_or_else(|| repository.selected_baseline.clone())
    }

    fn is_selectable(&self) -> bool {
        self.change.is_some() || self.presentation.directory.is_some()
    }

    fn staging_state(&self, staged_paths: &[String]) -> ToggleState {
        match self.presentation.section {
            Some(Section::Staged) => ToggleState::Selected,
            Some(Section::Unstaged) => ToggleState::Unselected,
            _ => self
                .change
                .as_ref()
                .map_or(ToggleState::Unselected, |change| {
                    staging_state(change, staged_paths)
                }),
        }
    }
}

#[derive(Clone)]
pub(crate) struct FileTarget {
    pub repository: VcsRepositoryId,
    pub baseline: String,
    pub change: Change,
}

#[derive(Clone, Copy)]
pub(crate) enum FileAction {
    Stage,
    Unstage,
    Discard,
    UnstagedChanges,
    StagedChanges,
    CopyPath,
    CopyRelativePath,
    Ignore,
    OpenDiff,
    OpenFileDiff,
    ViewFile,
    History,
}

fn presentation_status(change: &Change) -> FileStatus {
    match change.status {
        ChangeStatus::Untracked => FileStatus::Untracked,
        ChangeStatus::Added => FileStatus::worktree(StatusCode::Added),
        ChangeStatus::Modified => FileStatus::worktree(StatusCode::Modified),
        ChangeStatus::Deleted => FileStatus::worktree(StatusCode::Deleted),
        ChangeStatus::Renamed => FileStatus::worktree(StatusCode::Renamed),
    }
}

pub(crate) fn staging_state(change: &Change, staged_paths: &[String]) -> ToggleState {
    let staged = change.staged_status.is_some() || staged_paths.contains(&change.path);
    match (staged, change.unstaged_status.is_some()) {
        (true, true) => ToggleState::Indeterminate,
        (true, false) => ToggleState::Selected,
        (false, _) => ToggleState::Unselected,
    }
}

fn staging_operation(state: ToggleState) -> FileOperation {
    if state == ToggleState::Selected {
        FileOperation::Unstage
    } else {
        FileOperation::Stage
    }
}

pub(crate) fn staging_baseline_for_target(
    target: &FileTarget,
    repository: Option<&VcsRepositorySnapshot>,
) -> String {
    let working = repository.and_then(|repository| {
        let comparison = repository
            .descriptor
            .baselines
            .iter()
            .find(|baseline| baseline.id == target.baseline)?;
        if !matches!(
            comparison.purpose,
            Some(ComparisonPurpose::Staged | ComparisonPurpose::Unstaged)
        ) {
            return None;
        }
        repository
            .descriptor
            .baselines
            .iter()
            .find(|baseline| baseline.purpose == Some(ComparisonPurpose::Working))
    });
    working.map_or_else(|| target.baseline.clone(), |baseline| baseline.id.clone())
}

pub(crate) fn staging_unavailable_reason(
    target: &FileTarget,
    store: Option<&Entity<VcsStore>>,
    busy: bool,
    operation: FileOperation,
    cx: &App,
) -> Option<SharedString> {
    let Some(store) = store else {
        return Some("Source-control provider is unavailable.".into());
    };
    let store = store.read(cx);
    if busy || store.file_operation_running() {
        return Some("Another source-control operation is running.".into());
    }
    let repository = store
        .repositories()
        .into_iter()
        .find(|repository| repository.key == target.repository);
    let baseline = staging_baseline_for_target(target, repository.as_ref());
    let loaded = repository.as_ref().is_some_and(|repository| {
        if repository.selected_baseline == baseline {
            repository.snapshot.is_some()
        } else {
            store
                .comparison_snapshot(&target.repository, &baseline)
                .is_some()
        }
    });
    if !loaded {
        return Some("Refresh working changes before staging or unstaging this file.".into());
    }
    if store
        .file_operations_for_baseline(&target.repository, &target.change.path, &baseline)
        .contains(&operation)
    {
        None
    } else {
        Some(
            target
                .change
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| {
                    "The provider does not support this action for this file.".into()
                })
                .into(),
        )
    }
}

pub(crate) fn staging_state_for_target(
    target: &FileTarget,
    repository: Option<&VcsRepositorySnapshot>,
) -> ToggleState {
    let purpose = repository
        .and_then(|repository| {
            repository
                .descriptor
                .baselines
                .iter()
                .find(|baseline| baseline.id == target.baseline)
        })
        .and_then(|baseline| baseline.purpose);
    match purpose {
        Some(ComparisonPurpose::Staged) => ToggleState::Selected,
        Some(ComparisonPurpose::Unstaged) => ToggleState::Unselected,
        _ => staging_state(
            &target.change,
            repository
                .and_then(|repository| repository.repository_state.as_ref())
                .map_or(&[][..], |state| state.staged_paths.as_slice()),
        ),
    }
}

fn aggregate_staging_state(states: impl IntoIterator<Item = ToggleState>) -> ToggleState {
    let mut states = states.into_iter();
    let Some(first) = states.next() else {
        return ToggleState::Unselected;
    };
    if states.all(|state| state == first) {
        first
    } else {
        ToggleState::Indeterminate
    }
}

pub(crate) fn file_context_menu(
    mut menu: ContextMenu,
    target: &FileTarget,
    store: Option<&Entity<VcsStore>>,
    busy: bool,
    cx: &App,
    handler: impl Fn(FileAction, &mut Window, &mut App) + 'static,
) -> ContextMenu {
    let handler = Rc::new(handler);
    let repository = store.and_then(|store| {
        store
            .read(cx)
            .repositories()
            .into_iter()
            .find(|repository| repository.key == target.repository)
    });
    let operations = store
        .map(|store| {
            store.read(cx).file_operations_for_baseline(
                &target.repository,
                &target.change.path,
                &target.baseline,
            )
        })
        .unwrap_or_default();
    let state = staging_state_for_target(target, repository.as_ref());
    let operation = staging_operation(state);
    let entry = |label: SharedString, action, reason: Option<SharedString>| {
        let handler = handler.clone();
        let mut entry = ContextMenuEntry::new(label)
            .disabled(reason.is_some())
            .handler(move |window, cx| handler(action, window, cx));
        if let Some(reason) = reason {
            entry = entry.documentation_aside(ui::DocumentationSide::Right, move |_| {
                div()
                    .p_2()
                    .max_w_64()
                    .child(Label::new(reason.clone()).size(LabelSize::Small))
                    .into_any_element()
            });
        }
        entry
    };
    let unavailable = |operation: FileOperation| -> Option<SharedString> {
        if matches!(operation, FileOperation::Stage | FileOperation::Unstage) {
            return staging_unavailable_reason(target, store, busy, operation, cx);
        }
        if busy || store.is_some_and(|store| store.read(cx).file_operation_running()) {
            Some("Another source-control operation is running.".into())
        } else if !operations.contains(&operation) {
            Some(
                target
                    .change
                    .unavailable_reason
                    .clone()
                    .unwrap_or_else(|| {
                        "The provider does not support this action for this file.".into()
                    })
                    .into(),
            )
        } else {
            None
        }
    };
    let (stage_label, stage_action) = if operation == FileOperation::Unstage {
        ("Unstage File", FileAction::Unstage)
    } else {
        ("Stage File", FileAction::Stage)
    };
    menu.push_item(entry(
        stage_label.into(),
        stage_action,
        unavailable(operation),
    ));
    if state == ToggleState::Indeterminate {
        menu.push_item(entry(
            "Unstage File".into(),
            FileAction::Unstage,
            unavailable(FileOperation::Unstage),
        ));
    }
    let discard_reason = if busy {
        Some("Another source-control operation is running.".into())
    } else {
        discard_unavailable_reason(
            store,
            &target.repository,
            &target.change.path,
            Some(&target.baseline),
            cx,
        )
    };
    let discard_label = if repository.as_ref().is_some_and(|repository| {
        repository.descriptor.baselines.iter().any(|baseline| {
            baseline.id == target.baseline && baseline.current_content == ContentSource::Snapshot
        })
    }) {
        "Discard Unstaged Changes"
    } else {
        "Discard Changes"
    };
    menu.push_item(entry(
        discard_label.into(),
        FileAction::Discard,
        discard_reason,
    ));
    menu = menu.separator();
    for (label, purpose, action) in [
        (
            "Unstaged Changes",
            ComparisonPurpose::Unstaged,
            FileAction::UnstagedChanges,
        ),
        (
            "Staged Changes",
            ComparisonPurpose::Staged,
            FileAction::StagedChanges,
        ),
    ] {
        let reason = repository
            .as_ref()
            .is_none_or(|repository| {
                !repository
                    .descriptor
                    .baselines
                    .iter()
                    .any(|baseline| baseline.purpose == Some(purpose))
            })
            .then(|| "This comparison is unavailable from the provider.".into());
        menu.push_item(entry(label.into(), action, reason));
    }
    menu = menu.separator();
    menu.push_item(entry("Copy Path".into(), FileAction::CopyPath, None));
    menu.push_item(entry(
        "Copy Relative Path".into(),
        FileAction::CopyRelativePath,
        None,
    ));
    if let Some(config) = repository
        .as_ref()
        .and_then(|repository| repository.descriptor.ignore_config_path.as_ref())
    {
        menu = menu.separator();
        menu.push_item(entry(
            format!("Add to {config}").into(),
            FileAction::Ignore,
            unavailable(FileOperation::Ignore),
        ));
    }
    menu = menu.separator();
    let text_reason = (target.change.kind != ContentKind::File)
        .then(|| "Text comparison is unavailable for this file type.".into());
    menu.push_item(entry(
        "Open Diff".into(),
        FileAction::OpenDiff,
        text_reason.clone(),
    ));
    menu.push_item(entry(
        "Open File Diff".into(),
        FileAction::OpenFileDiff,
        text_reason,
    ));
    let deleted = (target.change.status == ChangeStatus::Deleted)
        .then(|| "This file no longer exists in the working tree.".into());
    menu.push_item(entry("View File".into(), FileAction::ViewFile, deleted));
    menu = menu.separator();
    let history_reason = if target.change.status == ChangeStatus::Untracked {
        Some("Untracked files have no revision history.".into())
    } else if store.is_none_or(|store| !store.read(cx).file_history_available(&target.repository)) {
        Some("File history is unavailable from the provider.".into())
    } else {
        None
    };
    menu.push_item(entry(
        "View File History".into(),
        FileAction::History,
        history_reason,
    ));
    menu
}

pub struct VcsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Option<Entity<VcsStore>>,
    focus: FocusHandle,
    repositories: Vec<VcsRepositorySnapshot>,
    selected_repository_id: Option<VcsRepositoryId>,
    rows: Vec<Row>,
    collapsed_sections: HashSet<String>,
    expanded_directories: collections::HashMap<TreeKey, bool>,
    selected: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    error: Option<SharedString>,
    preparing_operation: bool,
    refreshing_comparisons: bool,
    comparison_refresh_pending: bool,
    opened: HashMap<(VcsRepositoryId, String, String), WeakEntity<SplittableEditor>>,
    _subscription: Option<Subscription>,
}

impl VcsPanel {
    #[cfg(test)]
    pub(crate) fn set_test_repository(&mut self, snapshot: VcsRepositorySnapshot) {
        self.selected_repository_id = Some(snapshot.key.clone());
        self.repositories = vec![snapshot];
    }

    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = project.read(cx).vcs_store().cloned();
        let subscription = store.as_ref().map(|store| {
            cx.subscribe(store, |this, _, event, cx| {
                this.update_entries(cx);
                if matches!(event, VcsStoreEvent::DiffChanged { .. }) {
                    this.refresh_snapshot_comparisons(cx);
                }
            })
        });
        let view_options = |cx: &App| {
            let settings = GitPanelSettings::get_global(cx);
            (settings.group_by, settings.sort_by, settings.tree_view)
        };
        let mut previous_view_options = view_options(cx);
        cx.observe_global::<SettingsStore>(move |panel, cx| {
            let current = view_options(cx);
            if current != previous_view_options {
                previous_view_options = current;
                panel.update_entries(cx);
            } else {
                cx.notify();
            }
        })
        .detach();
        let mut panel = Self {
            workspace,
            project,
            store,
            focus: cx.focus_handle(),
            repositories: Vec::new(),
            selected_repository_id: None,
            rows: Vec::new(),
            collapsed_sections: HashSet::new(),
            expanded_directories: collections::HashMap::default(),
            selected: None,
            scroll_handle: UniformListScrollHandle::new(),
            error: None,
            preparing_operation: false,
            refreshing_comparisons: false,
            comparison_refresh_pending: false,
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
            .map(|row| {
                (
                    row.repository.clone(),
                    row.path.clone(),
                    row.presentation.section,
                )
            });
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
        let settings = GitPanelSettings::get_global(cx);
        self.rows = rows_for_repositories(
            &changes,
            settings.group_by,
            settings.sort_by,
            settings.tree_view,
            &mut self.expanded_directories,
        );
        self.selected = selected
            .and_then(|selected| {
                self.rows.iter().position(|row| {
                    row.is_selectable()
                        && (
                            row.repository.clone(),
                            row.path.clone(),
                            row.presentation.section,
                        ) == selected
                })
            })
            .filter(|index| self.visible_row_indices().contains(index))
            .or_else(|| {
                self.visible_row_indices()
                    .into_iter()
                    .find(|index| self.rows[*index].is_selectable())
            });
        cx.notify();
    }

    fn refresh_snapshot_comparisons(&mut self, cx: &mut Context<Self>) {
        if self.refreshing_comparisons {
            self.comparison_refresh_pending = true;
            return;
        }
        self.comparison_refresh_pending = false;
        let Some(store) = self.store.clone() else {
            return;
        };
        self.opened.retain(|_, editor| editor.upgrade().is_some());
        let comparisons = self
            .opened
            .iter()
            .filter_map(|((repository, path, baseline), editor)| {
                let descriptor = self
                    .repositories
                    .iter()
                    .find(|entry| &entry.key == repository)?
                    .descriptor
                    .baselines
                    .iter()
                    .find(|entry| &entry.id == baseline)?;
                (descriptor.current_content == ContentSource::Snapshot).then(|| {
                    (
                        repository.clone(),
                        path.clone(),
                        baseline.clone(),
                        editor.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        if comparisons.is_empty() {
            return;
        }
        self.refreshing_comparisons = true;
        cx.spawn(async move |this, cx| {
            for (repository, path, baseline, editor) in comparisons {
                let result = store
                    .update(cx, |store, cx| {
                        store.open_diff_for_baseline(repository, path, baseline, cx)
                    })
                    .await;
                this.update(cx, |this, cx| match result {
                    Ok(opened) => {
                        if let Some(editor) = editor.upgrade() {
                            let buffer = editor.read(cx).rhs_editor().read(cx).buffer().clone();
                            buffer.update(cx, |multibuffer, cx| {
                                let end = opened.buffer.read(cx).max_point();
                                multibuffer.set_excerpts_for_path(
                                    PathKey::sorted(0),
                                    opened.buffer,
                                    [language::Point::zero()..end],
                                    0,
                                    cx,
                                );
                                multibuffer.add_diff(opened.diff, cx);
                                multibuffer.set_all_diff_hunks_expanded(cx);
                            });
                        }
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                })
                .log_err();
            }
            this.update(cx, |this, cx| {
                this.refreshing_comparisons = false;
                if this.comparison_refresh_pending {
                    this.refresh_snapshot_comparisons(cx);
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let start = self.selected.map_or(0, |index| index + 1);
        self.selected = self
            .visible_row_indices()
            .into_iter()
            .find(|index| *index >= start && self.rows[*index].is_selectable())
            .or(self.selected);
        if let Some(index) = self.selected {
            self.scroll_to_row(index);
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
        self.selected = self
            .visible_row_indices()
            .into_iter()
            .rev()
            .find(|index| *index < start && self.rows[*index].is_selectable())
            .or(self.selected);
        if let Some(index) = self.selected {
            self.scroll_to_row(index);
        }
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            if let Some(directory) = self
                .rows
                .get(index)
                .and_then(|row| row.presentation.directory.clone())
            {
                self.expanded_directories
                    .insert(directory.key, !directory.expanded);
                self.update_entries(cx);
            } else {
                self.open_diff(index, window, cx);
            }
        }
    }

    fn expand_selected_entry(
        &mut self,
        _: &git_panel::ExpandSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(directory) = self
            .selected
            .and_then(|index| self.rows.get(index))
            .and_then(|row| row.presentation.directory.clone())
        {
            if !directory.expanded {
                self.expanded_directories.insert(directory.key, true);
                self.update_entries(cx);
                return;
            }
        }
        self.select_next(&menu::SelectNext, window, cx);
    }

    fn collapse_selected_entry(
        &mut self,
        _: &git_panel::CollapseSelectedEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !GitPanelSettings::get_global(cx).tree_view {
            self.select_previous(&menu::SelectPrevious, window, cx);
            return;
        }
        let Some(row) = self
            .selected
            .and_then(|index| self.rows.get(index))
            .cloned()
        else {
            return;
        };
        let target = row
            .presentation
            .directory
            .as_ref()
            .filter(|directory| directory.expanded)
            .map(|directory| (self.selected, directory.key.clone()))
            .or_else(|| {
                self.rows
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, parent)| {
                        let directory = parent.presentation.directory.as_ref()?;
                        (directory.expanded
                            && parent.presentation.section == row.presentation.section
                            && row.path.starts_with(&format!("{}/", parent.path)))
                        .then(|| (Some(index), directory.key.clone()))
                    })
            });
        let Some((selected, key)) = target else {
            self.select_previous(&menu::SelectPrevious, window, cx);
            return;
        };
        self.selected = selected;
        self.expanded_directories.insert(key, false);
        self.update_entries(cx);
        if let Some(index) = self.selected {
            self.scroll_to_row(index);
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

    fn stage_file(&mut self, _: &StageFile, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_action(FileAction::Stage, window, cx);
    }

    fn unstage_file(&mut self, _: &UnstageFile, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_action(FileAction::Unstage, window, cx);
    }

    fn discard_file(&mut self, _: &DiscardFile, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_file().cloned() else {
            return;
        };
        self.discard_path(row.repository, row.path, None, window, cx);
    }

    fn discard_path(
        &mut self,
        repository: VcsRepositoryId,
        path: String,
        baseline: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.confirm_path_operation(
            repository,
            path,
            baseline,
            FileOperation::Discard,
            window,
            cx,
        );
    }

    fn confirm_path_operation(
        &mut self,
        repository: VcsRepositoryId,
        path: String,
        baseline: Option<String>,
        operation: FileOperation,
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
        let operation = match operation {
            FileOperation::Discard => {
                discard_file_changes(store, repository, path, baseline, window, cx)
            }
            _ => confirm_file_operation(store, repository, path, baseline, operation, window, cx),
        };
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

    pub(crate) fn selected_repository(&self) -> Option<VcsRepositorySnapshot> {
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
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    if let Some(panel) = workspace.panel::<GitPanel>(cx) {
                        panel.update(cx, |panel, cx| {
                            panel.ensure_vcs_commit_editor(window, cx);
                            panel
                                .active_commit_editor(cx)
                                .focus_handle(cx)
                                .focus(window, cx);
                        });
                    }
                })
                .log_err();
        });
    }

    pub(crate) fn commit_unavailable_reason(&self, cx: &App) -> Option<&'static str> {
        let Some(repository) = self.selected_repository() else {
            return Some("No repository available");
        };
        let Some(store) = self.store.as_ref().map(|store| store.read(cx)) else {
            return Some("Source-control provider is unavailable");
        };
        if self.preparing_operation || store.file_operation_running() {
            return Some("Source-control operation in progress");
        }
        if store.file_operation_unresolved(&repository.key)
            || store.repository_operation_unresolved(&repository.key)
        {
            return Some("Refresh source control to reconcile the previous operation");
        }
        if !store
            .repository_operations(&repository.key)
            .contains(&RepositoryOperationKind::Commit)
        {
            return Some("The provider does not allow commits in this checkout");
        }
        if repository
            .repository_state
            .as_ref()
            .is_none_or(|state| state.staged_paths.is_empty())
        {
            return Some("No staged changes to commit");
        }
        if self.project.read(cx).is_read_only(cx) {
            return Some("You do not have write access to this project");
        }
        None
    }

    pub(crate) fn commit_message(
        &mut self,
        message: String,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if message.trim().is_empty() {
            return;
        }
        if let Some(reason) = self.commit_unavailable_reason(cx) {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        let Some(repository) = self.selected_repository() else {
            return;
        };
        let Some(store) = self.store.clone() else {
            return;
        };
        self.preparing_operation = true;
        self.error = None;
        let preparation = store.update(cx, |store, cx| {
            store.prepare_repository_operation(
                repository.key,
                RepositoryOperation::Commit {
                    message: message.clone(),
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let prepared = preparation.await?;
                store
                    .update(cx, |store, cx| {
                        store.apply_prepared_repository_operation(prepared, cx)
                    })
                    .await?;
                Ok(())
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                this.preparing_operation = false;
                match result {
                    Ok(()) => editor.update(cx, |editor, cx| {
                        if editor.text(cx) == message {
                            editor.set_text("", window, cx);
                        }
                    }),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn can_switch_branch(&self, cx: &App) -> bool {
        !self.preparing_operation
            && self.selected_repository().is_some_and(|repository| {
                self.store.as_ref().is_some_and(|store| {
                    !store.read(cx).file_operation_running()
                        && store
                            .read(cx)
                            .repository_operations(&repository.key)
                            .contains(&RepositoryOperationKind::SwitchBranch)
                })
            })
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
        let Some(target) = self.target_for_row(&row) else {
            return;
        };
        let baseline = if matches!(operation, FileOperation::Stage | FileOperation::Unstage) {
            staging_baseline_for_target(
                &target,
                self.repositories
                    .iter()
                    .find(|repository| repository.key == target.repository),
            )
        } else {
            target.baseline.clone()
        };
        self.run_path_operation(
            target.repository,
            target.change.path,
            operation,
            Some(baseline),
            cx,
        );
    }

    fn run_path_operation(
        &mut self,
        repository: VcsRepositoryId,
        path: String,
        operation: FileOperation,
        baseline: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.preparing_operation {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let operations = baseline
            .as_deref()
            .map(|baseline| {
                store
                    .read(cx)
                    .file_operations_for_baseline(&repository, &path, baseline)
            })
            .unwrap_or_else(|| store.read(cx).file_operations_for_path(&repository, &path));
        if !operations.contains(&operation) {
            self.error = Some("This operation is unavailable for this file or checkout.".into());
            cx.notify();
            return;
        }
        self.error = None;
        let task = store.update(cx, |store, cx| match baseline {
            Some(baseline) => {
                store.run_file_operation_for_baseline(repository, path, operation, baseline, cx)
            }
            None => store.run_file_operation(repository, path, operation, cx),
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
        let Some(target) = self.target_for_row(&row) else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                crate::vcs_project_diff::deploy_baseline(
                    workspace,
                    target.repository,
                    target.baseline,
                    Some(target.change.path),
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

    fn target_for_row(&self, row: &Row) -> Option<FileTarget> {
        let baseline = self
            .repositories
            .iter()
            .find(|repository| repository.key == row.repository)
            .map(|repository| row.baseline(repository))
            .unwrap_or_default();
        Some(FileTarget {
            repository: row.repository.clone(),
            baseline,
            change: row.change.clone()?,
        })
    }

    fn open_file_diff(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self
            .rows
            .get(index)
            .and_then(|row| self.target_for_row(row))
        else {
            return;
        };
        self.selected = Some(index);
        self.open_target_diff(target, window, cx);
    }

    fn open_target_diff(
        &mut self,
        target: FileTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        cx.notify();
        if target.change.kind != ContentKind::File {
            return;
        }
        let key = (
            target.repository.clone(),
            target.change.path.clone(),
            target.baseline.clone(),
        );
        if let Some(item) = self.opened.get(&key).and_then(WeakEntity::upgrade) {
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
        let task = store.update(cx, |store, cx| {
            store.open_diff_for_baseline(target.repository, target.change.path, target.baseline, cx)
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

    pub(crate) fn dispatch_file_action(
        &mut self,
        target: FileTarget,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let staging_baseline = staging_baseline_for_target(
            &target,
            self.repositories
                .iter()
                .find(|repository| repository.key == target.repository),
        );
        match action {
            FileAction::Stage => self.run_path_operation(
                target.repository,
                target.change.path,
                FileOperation::Stage,
                Some(staging_baseline),
                cx,
            ),
            FileAction::Unstage => self.run_path_operation(
                target.repository,
                target.change.path,
                FileOperation::Unstage,
                Some(staging_baseline),
                cx,
            ),
            FileAction::Ignore => self.confirm_path_operation(
                target.repository,
                target.change.path,
                Some(target.baseline),
                FileOperation::Ignore,
                window,
                cx,
            ),
            FileAction::Discard => self.discard_path(
                target.repository,
                target.change.path,
                Some(target.baseline),
                window,
                cx,
            ),
            FileAction::OpenFileDiff => self.open_target_diff(target, window, cx),
            FileAction::CopyRelativePath => {
                cx.write_to_clipboard(ClipboardItem::new_string(target.change.path))
            }
            FileAction::CopyPath => {
                if let Some(repository) = self
                    .repositories
                    .iter()
                    .find(|repository| repository.key == target.repository)
                {
                    match vcs::local_path(&repository.descriptor.scope_uri) {
                        Ok(scope) => cx.write_to_clipboard(ClipboardItem::new_string(
                            scope
                                .join(&target.change.path)
                                .to_string_lossy()
                                .into_owned(),
                        )),
                        Err(error) => self.error = Some(format!("{error:#}").into()),
                    }
                }
            }
            FileAction::ViewFile => {
                self.open_working_file(target.repository, target.change.path, window, cx)
            }
            FileAction::History => {
                if target.change.status == ChangeStatus::Untracked {
                    return;
                }
                self.workspace
                    .update(cx, |workspace, cx| {
                        crate::vcs_file_history::deploy(
                            workspace,
                            target.repository,
                            target.change.path,
                            window,
                            cx,
                        )
                    })
                    .log_err();
            }
            FileAction::OpenDiff | FileAction::StagedChanges | FileAction::UnstagedChanges => {
                let baseline = match action {
                    FileAction::StagedChanges | FileAction::UnstagedChanges => {
                        let purpose = if matches!(action, FileAction::StagedChanges) {
                            ComparisonPurpose::Staged
                        } else {
                            ComparisonPurpose::Unstaged
                        };
                        let baseline = self
                            .repositories
                            .iter()
                            .find(|repository| repository.key == target.repository)
                            .and_then(|repository| {
                                repository
                                    .descriptor
                                    .baselines
                                    .iter()
                                    .find(|baseline| baseline.purpose == Some(purpose))
                            });
                        let Some(baseline) = baseline else {
                            self.error =
                                Some("This comparison is unavailable from the provider.".into());
                            cx.notify();
                            return;
                        };
                        baseline.id.clone()
                    }
                    _ => target.baseline,
                };
                self.workspace
                    .update(cx, |workspace, cx| {
                        crate::vcs_project_diff::deploy_baseline(
                            workspace,
                            target.repository,
                            baseline,
                            Some(target.change.path),
                            true,
                            window,
                            cx,
                        )
                    })
                    .log_err();
            }
        }
        cx.notify();
    }

    fn selected_action(&mut self, action: FileAction, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            return;
        }
        if let Some(target) = self
            .selected_file()
            .and_then(|row| self.target_for_row(row))
        {
            self.dispatch_file_action(target, action, window, cx);
        }
    }

    fn view_file(&mut self, _: &git::ViewFile, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_file().cloned() else {
            return;
        };
        self.open_working_file(row.repository, row.path, window, cx);
    }

    fn open_working_file(
        &mut self,
        repository: VcsRepositoryId,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        let path = match RelPath::from_unix_str(&path) {
            Ok(path) => ProjectPath {
                worktree_id: repository.worktree_id,
                path: Arc::from(path),
            },
            Err(error) => {
                self.error = Some(format!("{error:#}").into());
                cx.notify();
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
        cx.notify();
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
        self.rows
            .iter()
            .filter(|row| row.change.is_some())
            .map(|row| (&row.repository, &row.path))
            .collect::<HashSet<_>>()
            .len()
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
            self.scroll_to_row(index);
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
            self.scroll_to_row(index);
            self.open_diff_with_focus(index, false, window, cx);
        }
    }

    fn first_entry(
        &mut self,
        _: &git_panel::FirstEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = self
            .visible_row_indices()
            .into_iter()
            .find(|index| self.rows[*index].is_selectable());
        if let Some(index) = self.selected {
            self.scroll_to_row(index);
            self.open_diff_with_focus(index, false, window, cx);
        }
    }

    fn last_entry(
        &mut self,
        _: &git_panel::LastEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = self
            .visible_row_indices()
            .into_iter()
            .rev()
            .find(|index| self.rows[*index].is_selectable());
        if let Some(index) = self.selected {
            self.scroll_to_row(index);
            self.open_diff_with_focus(index, false, window, cx);
        }
    }

    fn toggle_staged(&mut self, _: &git::ToggleStaged, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_file() else {
            return;
        };
        let staged_paths = self
            .selected_repository()
            .and_then(|repository| repository.repository_state)
            .map(|state| state.staged_paths.clone())
            .unwrap_or_default();
        self.run_file_operation(staging_operation(row.staging_state(&staged_paths)), cx);
    }

    pub(crate) fn unsupported_action(&mut self, cx: &mut Context<Self>) {
        self.error = Some("This action is unavailable for the selected external repository. Use its supported file or repository actions.".into());
        cx.notify();
    }

    fn show_comparison(
        &mut self,
        key: (VcsRepositoryId, String, String),
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
                add_comparison(workspace, project, opened_diff, &key.1, &key.2, window, cx)
            }
        })?;
        self.opened.insert(key, item.downgrade());
        Ok(item)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn visible_row_count(&self) -> usize {
        self.visible_row_indices().len()
    }

    fn visible_row_indices(&self) -> Vec<usize> {
        let mut collapsed = false;
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if row.change.is_none()
                    && row.presentation.directory.is_none()
                    && !row.presentation.empty_section
                {
                    let title = row
                        .label
                        .rsplit_once(" (")
                        .map_or(row.label.as_str(), |(title, _)| title);
                    collapsed = self.collapsed_sections.contains(title);
                    Some(index)
                } else {
                    (!collapsed && !row.presentation.hidden).then_some(index)
                }
            })
            .collect()
    }

    fn compute_visible_depths(&self, range: Range<usize>) -> smallvec::SmallVec<[usize; 64]> {
        let visible = self.visible_row_indices();
        range
            .map(|index| {
                visible
                    .get(index)
                    .and_then(|index| self.rows.get(*index))
                    .map_or(0, |row| row.presentation.depth)
            })
            .collect()
    }

    fn scroll_to_row(&self, index: usize) {
        if let Some(index) = self
            .visible_row_indices()
            .iter()
            .position(|row| *row == index)
        {
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    fn render_options_menu(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.clone();
        PopoverMenu::new("view_options_menu")
            .trigger_with_tooltip(
                IconButton::new("view-options-menu-trigger", IconName::Filter)
                    .icon_size(IconSize::Small),
                Tooltip::text("View Options"),
            )
            .menu(move |window, cx| {
                Some(git_panel::git_panel_view_options_menu(
                    focus.clone(),
                    window,
                    cx,
                ))
            })
            .anchor(Anchor::TopRight)
    }

    fn render_actions_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity();
        let staged_paths = self
            .selected_repository()
            .and_then(|repository| repository.repository_state)
            .map(|state| state.staged_paths.clone())
            .unwrap_or_default();
        let unstage = self.change_count() > 0
            && self
                .rows
                .iter()
                .filter_map(|row| row.change.as_ref())
                .all(|change| staging_state(change, &staged_paths) == ToggleState::Selected);
        let title = if unstage { "Unstage All" } else { "Stage All" };
        SplitButton::new(
            ButtonLike::new_rounded_left("git-changes-actions-split-button-left")
                .layer(ElevationIndex::ModalSurface)
                .size(ButtonSize::Compact)
                .child(Label::new(title).size(LabelSize::Small).mr_0p5())
                .disabled(true)
                .tooltip(Tooltip::text(
                    "The provider supports staging individual files",
                )),
            PopoverMenu::new("git-changes-actions-split-button-menu")
                .trigger(crate::render_split_button_chevron_trigger(
                    "changes-actions-split-button-right",
                    false,
                ))
                .menu(move |window, cx| {
                    let panel = panel.clone();
                    Some(ContextMenu::build(window, cx, |menu, _, cx| {
                        let refresh_panel = panel.clone();
                        let diagnostics_panel = panel.clone();
                        let repository = panel.read(cx).selected_repository();
                        let has_checkouts = repository.as_ref().is_some_and(|repository| {
                            panel.read(cx).store.as_ref().is_some_and(|store| {
                                store.read(cx).checkout_catalog_available(&repository.key)
                            })
                        });
                        let mut menu = menu;
                        if let Some(repository) = &repository {
                            menu = menu.header("Comparison");
                            for baseline in repository.descriptor.baselines.clone() {
                                let key = repository.key.clone();
                                let panel = panel.clone();
                                let selected = repository.selected_baseline == baseline.id;
                                menu = menu.item(
                                    ContextMenuEntry::new(baseline.label)
                                        .toggle(IconPosition::End, selected)
                                        .handler(move |_, cx| {
                                            panel.update(cx, |panel, cx| {
                                                panel.error = None;
                                                if let Some(store) = &panel.store {
                                                    if let Err(error) =
                                                        store.update(cx, |store, cx| {
                                                            store.set_baseline(
                                                                &key,
                                                                baseline.id.clone(),
                                                                cx,
                                                            )
                                                        })
                                                    {
                                                        panel.error =
                                                            Some(format!("{error:#}").into());
                                                    }
                                                }
                                                cx.notify();
                                            });
                                        }),
                                );
                            }
                            menu = menu.separator();
                        }
                        menu.entry("Refresh", None, move |_, cx| {
                            refresh_panel.update(cx, |panel, cx| {
                                panel.error = None;
                                if let Some(store) = &panel.store {
                                    store.update(cx, |store, cx| store.refresh_all(cx));
                                }
                            });
                        })
                        .entry("Provider Diagnostics", None, move |window, cx| {
                            let workspace = diagnostics_panel.read(cx).workspace.clone();
                            workspace
                                .update(cx, |workspace, cx| show_diagnostics(workspace, window, cx))
                                .log_err();
                        })
                        .when(has_checkouts, |menu| {
                            menu.entry("Checkouts", None, move |window, cx| {
                                let workspace = panel.read(cx).workspace.clone();
                                let key =
                                    repository.as_ref().map(|repository| repository.key.clone());
                                workspace
                                    .update(cx, |workspace, cx| {
                                        git_ui_core::vcs_checkout_picker::show(
                                            workspace, key, window, cx,
                                        )
                                    })
                                    .log_err();
                            })
                        })
                    }))
                })
                .anchor(Anchor::TopRight)
                .offset(gpui::Point {
                    x: px(0.),
                    y: px(2.),
                })
                .into_any_element(),
        )
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let visible = self.visible_row_indices();
        range
            .filter_map(|visible_index| visible.get(visible_index).copied())
            .filter_map(|index| self.rows.get(index).cloned().map(|row| (index, row)))
            .map(|(index, row)| {
                if row.presentation.empty_section {
                    return git_panel::status_empty_section(
                        row.presentation.section.unwrap_or(Section::Tracked),
                    );
                }
                if let Some(directory) = row.presentation.directory.clone() {
                    let key = directory.key.clone();
                    let staged_paths = self
                        .selected_repository()
                        .and_then(|repository| repository.repository_state)
                        .map(|state| state.staged_paths.clone())
                        .unwrap_or_default();
                    let prefix = format!("{}/", row.path);
                    let directory_state = aggregate_staging_state(
                        self.rows
                            .iter()
                            .filter(|child| {
                                child.change.is_some()
                                    && child.presentation.section == row.presentation.section
                                    && child.path.starts_with(&prefix)
                            })
                            .map(|child| child.staging_state(&staged_paths)),
                    );
                    return git_panel::status_row(
                        ("vcs-directory", index).into(),
                        self.selected == Some(index),
                        false,
                        self.focus.is_focused(window),
                        cx,
                    )
                    .min_w_0()
                    .justify_between()
                    .child(git_panel::status_directory_name(&directory, cx))
                    .child(
                        Checkbox::new(("vcs-directory-stage", index), directory_state)
                            .fill()
                            .elevation(ElevationIndex::Surface)
                            .disabled(true)
                            .tooltip(Tooltip::text(
                                "The provider supports staging individual files",
                            )),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = Some(index);
                        this.expanded_directories
                            .insert(key.clone(), !directory.expanded);
                        this.update_entries(cx);
                    }))
                    .into_any_element();
                }
                let Some(change) = row.change.as_ref() else {
                    let (title, count) = row
                        .label
                        .rsplit_once(" (")
                        .map(|(title, count)| {
                            (
                                title.to_owned(),
                                count.trim_end_matches(')').parse().unwrap_or(0),
                            )
                        })
                        .unwrap_or_else(|| (row.label.clone(), 0));
                    let collapsed = self.collapsed_sections.contains(&title);
                    let staged_paths = self
                        .selected_repository()
                        .and_then(|repository| repository.repository_state)
                        .map(|state| state.staged_paths.clone())
                        .unwrap_or_default();
                    let states = self
                        .rows
                        .iter()
                        .skip(index + 1)
                        .take_while(|row| {
                            row.change.is_some() || row.presentation.directory.is_some()
                        })
                        .filter_map(|row| row.change.as_ref())
                        .map(|change| staging_state(change, &staged_paths))
                        .collect::<Vec<_>>();
                    let section_state = if row.presentation.section == Some(Section::Staged) {
                        ToggleState::Selected
                    } else if row.presentation.section == Some(Section::Unstaged) {
                        ToggleState::Unselected
                    } else if states.iter().all(|state| *state == ToggleState::Selected) {
                        ToggleState::Selected
                    } else if states.iter().all(|state| *state == ToggleState::Unselected) {
                        ToggleState::Unselected
                    } else {
                        ToggleState::Indeterminate
                    };
                    return git_panel::status_list_header(
                        ("vcs-header", index).into(),
                        title.clone().into(),
                        count,
                        collapsed,
                        cx,
                    )
                    .when(count > 0, |this| {
                        this.child(
                            Checkbox::new(("vcs-stage-section", index), section_state)
                                .fill()
                                .elevation(ElevationIndex::Surface)
                                .disabled(true)
                                .tooltip(Tooltip::text(
                                    "The provider supports staging individual files",
                                )),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_sections.remove(&title) {
                            this.collapsed_sections.insert(title.clone());
                        }
                        let visible = this.visible_row_indices();
                        if this.selected.is_some_and(|index| !visible.contains(&index)) {
                            this.selected = visible
                                .into_iter()
                                .find(|index| this.rows[*index].change.is_some());
                        }
                        cx.notify();
                    }))
                    .into_any_element();
                };
                let path = Path::new(&row.path);
                let parent = path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|path| path.to_string_lossy().into_owned());
                let panel = cx.entity().downgrade();
                let Some(target) = self.target_for_row(&row) else {
                    return gpui::Empty.into_any_element();
                };
                let staged_paths = self
                    .selected_repository()
                    .and_then(|repository| repository.repository_state)
                    .map(|state| state.staged_paths.clone())
                    .unwrap_or_default();
                let state = row.staging_state(&staged_paths);
                let operation = staging_operation(state);
                let unavailable = staging_unavailable_reason(
                    &target,
                    self.store.as_ref(),
                    self.preparing_operation,
                    operation,
                    cx,
                );
                let disabled = unavailable.is_some();
                let checkbox_panel = panel.clone();
                let checkbox_target = target.clone();
                let checkbox = Checkbox::new(("vcs-stage", index), state)
                    .fill()
                    .elevation(ElevationIndex::Surface)
                    .disabled(disabled)
                    .tooltip(Tooltip::text(unavailable.unwrap_or_else(|| {
                        if operation == FileOperation::Unstage {
                            "Unstage File".into()
                        } else {
                            "Stage File".into()
                        }
                    })))
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        checkbox_panel
                            .update(cx, |panel, cx| {
                                panel.dispatch_file_action(
                                    checkbox_target.clone(),
                                    if operation == FileOperation::Unstage {
                                        FileAction::Unstage
                                    } else {
                                        FileAction::Stage
                                    },
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                    });
                let item = git_panel::status_row(
                    ("vcs-entry", index).into(),
                    self.selected == Some(index),
                    false,
                    self.focus.is_focused(window),
                    cx,
                )
                .child(git_panel::status_name_row(
                    path,
                    row.label.clone(),
                    parent,
                    presentation_status(change),
                    row.presentation.depth,
                    GitPanelSettings::get_global(cx).tree_view,
                    self.project.read(cx).path_style(cx),
                    cx,
                ))
                .child(
                    div()
                        .id(("vcs-stage-wrapper", index))
                        .flex_none()
                        .occlude()
                        .cursor_pointer()
                        .child(checkbox)
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                )
                .tooltip(Tooltip::text(match &change.old_path {
                    Some(old) => format!("{old} → {}", row.path),
                    None => row.path.clone(),
                }))
                .on_click(cx.listener(
                    move |this, event: &gpui::ClickEvent, window, cx| {
                        this.open_entry_on_click(index, event.click_count() > 1, window, cx)
                    },
                ));
                right_click_menu(("vcs-file-menu", index))
                    .trigger(move |_, _, _| item)
                    .menu(move |window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.selected = panel.rows.iter().position(|row| {
                                    row.repository == target.repository
                                        && row.path == target.change.path
                                        && panel.target_for_row(row).is_some_and(|row_target| {
                                            row_target.baseline == target.baseline
                                        })
                                });
                                cx.notify();
                            })
                            .log_err();
                        let target = target.clone();
                        let panel = panel.clone();
                        ContextMenu::build(window, cx, move |menu, _, cx| {
                            let Some(entity) = panel.upgrade() else {
                                return menu;
                            };
                            let view = entity.read(cx);
                            let target_for_handler = target.clone();
                            file_context_menu(
                                menu,
                                &target,
                                view.store.as_ref(),
                                view.preparing_operation,
                                cx,
                                move |action, window, cx| {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.dispatch_file_action(
                                                target_for_handler.clone(),
                                                action,
                                                window,
                                                cx,
                                            )
                                        })
                                        .log_err();
                                },
                            )
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
    comparison: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<SplittableEditor> {
    let multibuffer = cx.new(|cx| {
        // A comparison must not deduplicate against the file's ordinary singleton editor.
        let mut multibuffer = MultiBuffer::without_headers(
            if opened_diff.current_content == ContentSource::Snapshot {
                Capability::ReadOnly
            } else {
                opened_diff.buffer.read(cx).capability()
            },
        );
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
        multibuffer.set_title(
            format!(
                "{path} — {}",
                if comparison.is_empty() {
                    "Changes"
                } else {
                    comparison
                }
            ),
            cx,
        );
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

fn rows_for_repositories(
    repositories: &[(&VcsRepositorySnapshot, Vec<Change>)],
    group_by: GitPanelGroupBy,
    sort_by: GitPanelSortBy,
    tree_view: bool,
    expanded_directories: &mut collections::HashMap<TreeKey, bool>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (repository, changes) in repositories {
        let staged_paths = repository
            .repository_state
            .as_ref()
            .map_or(&[][..], |state| state.staged_paths.as_slice());
        let project_staging = repository
            .descriptor
            .baselines
            .iter()
            .find(|baseline| baseline.id == repository.selected_baseline)
            .is_some_and(|baseline| {
                matches!(
                    baseline.purpose,
                    Some(
                        ComparisonPurpose::Working
                            | ComparisonPurpose::Staged
                            | ComparisonPurpose::Unstaged
                    )
                )
            });
        let sections = match group_by {
            GitPanelGroupBy::None => vec![(Section::Tracked, "")],
            GitPanelGroupBy::Status => {
                vec![(Section::Tracked, "Tracked"), (Section::New, "Untracked")]
            }
            GitPanelGroupBy::Staging => {
                vec![(Section::Staged, "Staged"), (Section::Unstaged, "Unstaged")]
            }
        };
        for (section, label) in sections {
            let mut changes = changes
                .iter()
                .filter(|change| {
                    if group_by == GitPanelGroupBy::None {
                        return true;
                    }
                    match section {
                        Section::Tracked => !presentation_status(change).is_created(),
                        Section::New => presentation_status(change).is_created(),
                        Section::Staged => {
                            staging_state(change, staged_paths) != ToggleState::Unselected
                        }
                        Section::Unstaged => {
                            staging_state(change, staged_paths) != ToggleState::Selected
                        }
                        Section::Conflict => false,
                    }
                })
                .cloned()
                .collect::<Vec<_>>();
            let show_empty = group_by == GitPanelGroupBy::Staging
                && repositories.iter().any(|(_, changes)| !changes.is_empty());
            if changes.is_empty() && !show_empty {
                continue;
            }
            changes.sort_by(|left, right| {
                git_panel::compare_change_paths(&left.path, &right.path, sort_by)
            });
            let comparison = if project_staging {
                match section {
                    Section::Staged => Some(ComparisonPurpose::Staged),
                    Section::Unstaged => Some(ComparisonPurpose::Unstaged),
                    _ => None,
                }
            } else {
                None
            };
            if !label.is_empty() {
                rows.push(Row {
                    repository: repository.key.clone(),
                    path: String::new(),
                    label: format!("{label} ({})", changes.len()),
                    change: None,
                    presentation: RowPresentation {
                        section: Some(section),
                        comparison,
                        ..Default::default()
                    },
                });
            }
            if changes.is_empty() {
                rows.push(Row {
                    repository: repository.key.clone(),
                    path: String::new(),
                    label: String::new(),
                    change: None,
                    presentation: RowPresentation {
                        section: Some(section),
                        empty_section: true,
                        ..Default::default()
                    },
                });
                continue;
            }
            let files = changes
                .into_iter()
                .map(|mut change| {
                    if comparison == Some(ComparisonPurpose::Staged) {
                        change.status = change.staged_status.unwrap_or(change.status);
                    } else if comparison == Some(ComparisonPurpose::Unstaged) {
                        change.status = change.unstaged_status.unwrap_or(change.status);
                    }
                    Row {
                        repository: repository.key.clone(),
                        path: change.path.clone(),
                        label: change
                            .path
                            .rsplit('/')
                            .next()
                            .unwrap_or(&change.path)
                            .to_owned(),
                        change: Some(change),
                        presentation: RowPresentation {
                            section: Some(section),
                            comparison,
                            ..Default::default()
                        },
                    }
                })
                .collect::<Vec<_>>();
            if !tree_view {
                rows.extend(files);
                continue;
            }
            let indexed_paths = files
                .iter()
                .enumerate()
                .filter_map(
                    |(index, row)| match git::repository::RepoPath::new(&row.path) {
                        Ok(path) => Some((path, index)),
                        Err(error) => {
                            log::error!("Invalid provider file path: {error}");
                            None
                        }
                    },
                )
                .collect();
            let mut seen = collections::HashSet::default();
            let mut descendants = collections::HashMap::default();
            for (row, visible) in git_panel::project_status_tree(
                section,
                indexed_paths,
                expanded_directories,
                &mut seen,
                &mut descendants,
            ) {
                match row {
                    StatusTreeRow::File {
                        source_index,
                        depth,
                    } => {
                        if let Some(file) = files.get(source_index) {
                            let mut file = file.clone();
                            file.presentation.depth = depth;
                            file.presentation.hidden = !visible;
                            rows.push(file);
                        }
                    }
                    StatusTreeRow::Directory(directory) => rows.push(Row {
                        repository: repository.key.clone(),
                        path: directory.key.path.as_unix_str().to_owned(),
                        label: directory.name.to_string(),
                        change: None,
                        presentation: RowPresentation {
                            section: Some(section),
                            comparison,
                            depth: directory.depth,
                            hidden: !visible,
                            directory: Some(directory),
                            empty_section: false,
                        },
                    }),
                }
            }
        }
    }
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
        let operation_error = repository.as_ref().and_then(|repository| {
            self.store
                .as_ref()
                .and_then(|store| store.read(cx).file_operation_error(&repository.key))
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
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .on_action(cx.listener(Self::next_entry))
            .on_action(cx.listener(Self::previous_entry))
            .on_action(cx.listener(Self::first_entry))
            .on_action(cx.listener(Self::last_entry))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(|this, _: &git::ViewFile, window, cx| {
                this.selected_action(FileAction::ViewFile, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &zed_actions::workspace::CopyPath, window, cx| {
                    this.selected_action(FileAction::CopyPath, window, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &zed_actions::workspace::CopyRelativePath, window, cx| {
                    this.selected_action(FileAction::CopyRelativePath, window, cx)
                },
            ))
            .on_action(cx.listener(|this, action: &git::ToggleStaged, window, cx| {
                if this.focus.is_focused(window) {
                    this.toggle_staged(action, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &git::FileHistory, window, cx| {
                this.selected_action(FileAction::History, window, cx)
            }))
            .on_action(cx.listener(|this, _: &git::AddToGitignore, window, cx| {
                this.selected_action(FileAction::Ignore, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &git_panel::ViewStagedChanges, window, cx| {
                    this.selected_action(FileAction::StagedChanges, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_panel::ViewUnstagedChanges, window, cx| {
                    this.selected_action(FileAction::UnstagedChanges, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &git::OpenFileDiff, window, cx| {
                this.selected_action(FileAction::OpenFileDiff, window, cx)
            }))
            .on_action(cx.listener(Self::stage_file))
            .on_action(cx.listener(Self::unstage_file))
            .on_action(cx.listener(|this, action: &DiscardFile, window, cx| {
                if this.focus.is_focused(window) {
                    this.discard_file(action, window, cx);
                }
            }))
            .on_action(cx.listener(Self::commit_staged))
            .on_action(cx.listener(Self::switch_branch))
            .on_action(cx.listener(|this, _: &git::StageFile, window, cx| {
                this.selected_action(FileAction::Stage, window, cx)
            }))
            .on_action(cx.listener(|this, _: &git::UnstageFile, window, cx| {
                this.selected_action(FileAction::Unstage, window, cx)
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
                    .flex_none()
                    .flex_wrap()
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
                            .child(self.render_options_menu(cx))
                            .child(self.render_actions_menu(cx)),
                    ),
            )
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
                    v_flex().flex_1().items_center().justify_center().child(
                        Label::new(if repository.is_some() {
                            "No changes to commit"
                        } else {
                            "No repository available"
                        })
                        .color(Color::Muted),
                    ),
                )
            })
            .when(count > 0, |this| {
                this.child(
                    uniform_list(
                        "vcs-changes",
                        self.visible_row_indices().len(),
                        cx.processor(Self::render_rows),
                    )
                    .when(GitPanelSettings::get_global(cx).tree_view, |list| {
                        list.with_decoration(
                            git_panel::status_indent_guides(cx)
                                .with_compute_indents_fn(cx.entity(), |panel, range, _, _| {
                                    panel.compute_visible_depths(range)
                                }),
                        )
                    })
                    .track_scroll(&self.scroll_handle)
                    .size_full()
                    .flex_1(),
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

    #[test]
    fn stock_projection_preserves_mixed_staging_targets_and_tree_paths() {
        let repository = VcsRepositorySnapshot {
            key: VcsRepositoryId {
                worktree_id: settings::WorktreeId::from_usize(1),
                extension_id: "fixture".into(),
                provider_id: "native".into(),
                repository_id: "repo".into(),
            },
            descriptor: vcs::RepositoryDescriptor {
                id: "repo".into(),
                root_uri: "file:///repo".into(),
                scope_uri: "file:///repo".into(),
                display_name: "repo".into(),
                ignore_config_path: None,
                baselines: [
                    ("work", ComparisonPurpose::Working),
                    ("index-view", ComparisonPurpose::Staged),
                    ("disk-view", ComparisonPurpose::Unstaged),
                ]
                .into_iter()
                .map(|(id, purpose)| vcs::Baseline {
                    id: id.into(),
                    label: id.into(),
                    purpose: Some(purpose),
                    current_content: if purpose == ComparisonPurpose::Staged {
                        ContentSource::Snapshot
                    } else {
                        ContentSource::Working
                    },
                })
                .collect(),
            },
            selected_baseline: "work".into(),
            snapshot: None,
            loading: false,
            show_loading: false,
            error: None,
            repository_state: None,
            repository_state_error: None,
            checkouts: None,
            checkout_error: None,
            checkout_loading: false,
        };
        let mixed = Change {
            path: "a/nested/z.rs".into(),
            old_path: None,
            status: ChangeStatus::Modified,
            kind: ContentKind::File,
            old_mode: None,
            new_mode: None,
            file_operations: None,
            unavailable_reason: None,
            staged_status: Some(ChangeStatus::Added),
            unstaged_status: Some(ChangeStatus::Modified),
        };
        let mut unstaged = mixed.clone();
        unstaged.path = "z/a.rs".into();
        unstaged.staged_status = None;
        let repositories = [(&repository, vec![mixed, unstaged])];
        let mut expanded = collections::HashMap::default();
        let mut created_changes = repositories[0].1.clone();
        let mut added = created_changes[0].clone();
        added.path = "created.rs".into();
        added.status = ChangeStatus::Added;
        added.staged_status = Some(ChangeStatus::Added);
        added.unstaged_status = None;
        let mut untracked = added.clone();
        untracked.path = "scratch.rs".into();
        untracked.status = ChangeStatus::Untracked;
        untracked.staged_status = None;
        untracked.unstaged_status = Some(ChangeStatus::Untracked);
        created_changes.extend([added, untracked]);
        let status_rows = rows_for_repositories(
            &[(&repository, created_changes)],
            GitPanelGroupBy::Status,
            GitPanelSortBy::Path,
            false,
            &mut expanded,
        );
        for path in ["created.rs", "scratch.rs"] {
            assert_eq!(
                status_rows
                    .iter()
                    .find(|row| row.path == path)
                    .expect("created row")
                    .presentation
                    .section,
                Some(Section::New)
            );
        }
        assert_eq!(
            status_rows
                .iter()
                .find(|row| row.path == "a/nested/z.rs")
                .expect("modified row")
                .presentation
                .section,
            Some(Section::Tracked)
        );
        let rows = rows_for_repositories(
            &repositories,
            GitPanelGroupBy::Staging,
            GitPanelSortBy::Path,
            false,
            &mut expanded,
        );
        let mixed_rows = rows
            .iter()
            .filter(|row| row.path == "a/nested/z.rs")
            .collect::<Vec<_>>();
        assert_eq!(mixed_rows.len(), 2);
        assert_eq!(mixed_rows[0].baseline(&repository), "index-view");
        assert_eq!(mixed_rows[1].baseline(&repository), "disk-view");
        assert_eq!(mixed_rows[0].staging_state(&[]), ToggleState::Selected);
        assert_eq!(mixed_rows[1].staging_state(&[]), ToggleState::Unselected);
        for (row, expected) in [
            (mixed_rows[0], FileOperation::Unstage),
            (mixed_rows[1], FileOperation::Stage),
        ] {
            let target = FileTarget {
                repository: repository.key.clone(),
                baseline: row.baseline(&repository),
                change: row.change.clone().expect("change"),
            };
            assert_eq!(
                staging_operation(staging_state_for_target(&target, Some(&repository))),
                expected
            );
            assert_eq!(
                staging_baseline_for_target(&target, Some(&repository)),
                "work",
                "Grouped Stage/Unstage uses working metadata even before opening an index comparison"
            );
        }
        let historical = FileTarget {
            repository: repository.key.clone(),
            baseline: "revision-view".into(),
            change: repositories[0].1[0].clone(),
        };
        assert_eq!(
            staging_baseline_for_target(&historical, Some(&repository)),
            "revision-view",
            "Historical snapshots must not borrow working mutation capabilities"
        );
        let rows = rows_for_repositories(
            &repositories,
            GitPanelGroupBy::None,
            GitPanelSortBy::Name,
            false,
            &mut expanded,
        );
        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            ["z/a.rs", "a/nested/z.rs"]
        );
        let rows = rows_for_repositories(
            &repositories,
            GitPanelGroupBy::None,
            GitPanelSortBy::Path,
            true,
            &mut expanded,
        );
        assert_eq!(rows[0].label, "a/nested");
        assert_eq!(
            rows.iter()
                .find(|row| row.path == "a/nested/z.rs")
                .expect("file row")
                .presentation
                .depth,
            1
        );
        let key = rows[0]
            .presentation
            .directory
            .as_ref()
            .expect("native compact directory")
            .key
            .clone();
        expanded.insert(key, false);
        let rows = rows_for_repositories(
            &repositories,
            GitPanelGroupBy::None,
            GitPanelSortBy::Path,
            true,
            &mut expanded,
        );
        assert!(
            rows.iter()
                .find(|row| row.path == "a/nested/z.rs")
                .expect("file row")
                .presentation
                .hidden
        );
        assert!(
            !rows
                .iter()
                .find(|row| row.path == "z/a.rs")
                .expect("unrelated file row")
                .presentation
                .hidden
        );
    }

    #[test]
    fn staging_checkbox_tracks_partial_and_complete_changes() {
        let mut change = Change {
            path: "file.txt".into(),
            old_path: None,
            status: ChangeStatus::Modified,
            kind: ContentKind::File,
            old_mode: None,
            new_mode: None,
            file_operations: None,
            unavailable_reason: None,
            staged_status: None,
            unstaged_status: Some(ChangeStatus::Modified),
        };
        assert_eq!(staging_state(&change, &[]), ToggleState::Unselected);
        assert_eq!(
            staging_operation(staging_state(&change, &[])),
            FileOperation::Stage
        );
        change.staged_status = Some(ChangeStatus::Modified);
        assert_eq!(staging_state(&change, &[]), ToggleState::Indeterminate);
        assert_eq!(
            staging_operation(staging_state(&change, &[])),
            FileOperation::Stage
        );
        change.unstaged_status = None;
        assert_eq!(staging_state(&change, &[]), ToggleState::Selected);
        assert_eq!(
            staging_operation(staging_state(&change, &[])),
            FileOperation::Unstage
        );
        change.staged_status = None;
        assert_eq!(
            staging_state(&change, &["file.txt".into()]),
            ToggleState::Selected
        );
    }

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
        let key = (repository.clone(), "file.txt".to_owned(), String::new());
        let comparison = panel
            .update_in(cx, |panel, window, cx| {
                panel.show_comparison(
                    key.clone(),
                    VcsDiff {
                        buffer: buffer.clone(),
                        diff: diff.clone(),
                        current_content: ContentSource::Working,
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
                presentation: RowPresentation::default(),
                change: Some(Change {
                    path: "file.txt".into(),
                    old_path: None,
                    status: ChangeStatus::Modified,
                    kind: ContentKind::File,
                    old_mode: None,
                    new_mode: None,
                    file_operations: None,
                    unavailable_reason: None,
                    staged_status: None,
                    unstaged_status: None,
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
                        current_content: ContentSource::Working,
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
                presentation: RowPresentation::default(),
                change: Some(Change {
                    path: path.into(),
                    old_path: None,
                    status: ChangeStatus::Modified,
                    kind,
                    old_mode: None,
                    new_mode: None,
                    file_operations: None,
                    unavailable_reason: None,
                    staged_status: None,
                    unstaged_status: None,
                }),
            })
            .collect();
            panel.opened.insert(
                (repository, "working.txt".into(), String::new()),
                editor.downgrade(),
            );
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
        let target = panel.read_with(cx, |panel, _| panel.target_for_row(&panel.rows[0]).unwrap());
        let weak_panel = panel.downgrade();
        let menu = cx.new_window_entity(|window, cx| {
            ContextMenu::new(window, cx, |menu, _, cx| {
                file_context_menu(menu, &target, None, false, cx, {
                    let target = target.clone();
                    move |action, window, cx| {
                        weak_panel
                            .update(cx, |panel, cx| {
                                panel.dispatch_file_action(target.clone(), action, window, cx)
                            })
                            .unwrap();
                    }
                })
            })
        });
        panel.update(cx, |panel, _| panel.selected = Some(1));
        menu.update_in(cx, |menu, window, cx| {
            menu.select_first(&menu::SelectFirst, window, cx);
            menu.select_next(&menu::SelectNext, window, cx);
            menu.confirm(&menu::Confirm, window, cx);
        });
        cx.read(|cx| {
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("binary.bin".into()),
                "menu actions retain their path after selection changes"
            )
        });
    }
}
