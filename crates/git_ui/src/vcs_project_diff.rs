use std::{
    any::TypeId,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use anyhow::Result;
use buffer_diff::BufferDiffEvent;
use editor::{
    Editor, EditorEvent, EditorSettings, SelectionEffects, SplittableEditor,
    multibuffer_context_lines, scroll::Autoscroll,
};
use futures::{StreamExt as _, stream};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task, WeakEntity,
    Window,
};
use language::BufferId;
use language::{Anchor, Capability, OffsetRangeExt as _, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::{
    Project, ProjectPath,
    vcs_store::{VcsDiff, VcsRepositoryId, VcsStore},
};
use settings::Settings as _;
use ui::{Button, ContextMenu, Label, Tooltip, prelude::*};
use util::{ResultExt as _, rel_path::RelPath};
use vcs::{Change, ChangeStatus, ContentKind};
use workspace::{
    Item, ItemNavHistory, Workspace,
    item::{ItemEvent, SaveOptions},
    searchable::SearchableItemHandle,
};

use crate::{
    git_panel::GitPanel,
    vcs_panel::{
        DiscardFile, discard_context_menu, discard_file_changes, discard_unavailable_reason,
    },
};

const MAX_CONCURRENT_BUFFER_LOADS: usize = 4;

pub(crate) fn try_deploy(
    workspace: &mut Workspace,
    path: Option<ProjectPath>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let project = workspace.project().read(cx);
    let Some(store) = project.vcs_store() else {
        return false;
    };
    let target = if let Some(path) = path {
        store
            .read(cx)
            .repository_for_path(&path, cx)
            .map(|key| (key, Some(path.path.as_unix_str().to_owned())))
    } else if let Some(panel) = workspace.panel::<GitPanel>(cx) {
        let selected = panel.read(cx).selected_vcs_repository(cx);
        if selected.is_none() && panel.read(cx).uses_vcs_panel(cx) {
            crate::reject_external_vcs_action(workspace, cx);
            return true;
        }
        selected.map(|key| (key, None))
    } else if let Some(path) = workspace
        .active_item(cx)
        .and_then(|item| item.project_path(cx))
    {
        store
            .read(cx)
            .repository_for_path(&path, cx)
            .map(|key| (key, None))
    } else {
        let repositories = store.read(cx).repositories();
        if project.repositories(cx).is_empty() && repositories.len() == 1 {
            repositories
                .first()
                .map(|repository| (repository.key.clone(), None))
        } else {
            None
        }
    };
    let Some((key, path)) = target else {
        return false;
    };
    deploy_at(workspace, key, path, window, cx);
    true
}

pub(crate) fn deploy_at(
    workspace: &mut Workspace,
    repository: VcsRepositoryId,
    path: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    deploy_at_with_focus(workspace, repository, path, true, window, cx);
}

pub(crate) fn deploy_at_with_focus(
    workspace: &mut Workspace,
    repository: VcsRepositoryId,
    path: Option<String>,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(store) = workspace.project().read(cx).vcs_store().cloned() else {
        return;
    };
    let item = open_item(workspace, repository, Some(store), focus, window, cx);
    if let Some(path) = path {
        item.update(cx, |item, cx| item.move_to_path(path, window, cx));
    }
}

fn open_item(
    workspace: &mut Workspace,
    repository: VcsRepositoryId,
    store: Option<Entity<VcsStore>>,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<VcsProjectDiff> {
    let existing = workspace
        .items_of_type::<VcsProjectDiff>(cx)
        .find(|item| item.read(cx).repository == repository);
    if let Some(item) = existing {
        item.update(cx, |item, cx| {
            if item.error.is_some() && !item.loading {
                item.snapshot = None;
                item.refresh(window, cx);
            }
        });
        workspace.activate_item(&item, focus, focus, window, cx);
        return item;
    }
    let project = workspace.project().clone();
    let workspace_entity = cx.entity();
    let item =
        cx.new(|cx| VcsProjectDiff::new(repository, store, project, workspace_entity, window, cx));
    workspace.add_item_to_active_pane(Box::new(item.clone()), None, focus, window, cx);
    item
}

struct LoadedDiff {
    opened: VcsDiff,
    status: ChangeStatus,
    _subscription: Subscription,
}

pub(crate) struct VcsProjectDiff {
    repository: VcsRepositoryId,
    store: Option<Entity<VcsStore>>,
    editor: Entity<SplittableEditor>,
    multibuffer: Entity<MultiBuffer>,
    loaded: BTreeMap<String, LoadedDiff>,
    snapshot: Option<(String, String, Vec<Change>)>,
    pending_path: Option<String>,
    generation: u64,
    loading: bool,
    preparing_discard: bool,
    metadata_count: usize,
    error: Option<SharedString>,
    repository_error: Option<SharedString>,
    task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl VcsProjectDiff {
    pub(crate) fn editor(&self) -> Entity<SplittableEditor> {
        self.editor.clone()
    }

    fn new(
        repository: VcsRepositoryId,
        store: Option<Entity<VcsStore>>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut buffer = MultiBuffer::new(Capability::ReadWrite);
            buffer.set_all_diff_hunks_expanded(cx);
            buffer.set_title("Uncommitted Changes".into(), cx);
            buffer
        });
        let diff_view = cx.entity().downgrade();
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project,
                workspace,
                window,
                cx,
            );
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_should_serialize(false, cx);
                editor.register_addon(VcsDiffAddon { view: diff_view });
            });
            editor
        });
        let mut subscriptions = vec![cx.subscribe_in(
            &editor,
            window,
            |this, _, event: &EditorEvent, window, cx| {
                if matches!(event, EditorEvent::Saved) {
                    this.refresh(window, cx);
                }
                cx.emit(event.clone());
                cx.notify();
            },
        )];
        if let Some(store) = &store {
            subscriptions.push(cx.subscribe_in(store, window, |this, _, _, window, cx| {
                this.refresh(window, cx);
            }));
        }
        let mut this = Self {
            repository,
            store,
            editor,
            multibuffer,
            loaded: BTreeMap::new(),
            snapshot: None,
            pending_path: None,
            generation: 0,
            loading: false,
            preparing_discard: false,
            metadata_count: 0,
            error: None,
            repository_error: None,
            task: None,
            _subscriptions: subscriptions,
        };
        this.refresh(window, cx);
        this
    }

    fn path_for_buffer(&self, buffer_id: BufferId, cx: &App) -> Option<String> {
        self.loaded.iter().find_map(|(path, loaded)| {
            (loaded.opened.buffer.read(cx).remote_id() == buffer_id
                || loaded
                    .opened
                    .diff
                    .read(cx)
                    .snapshot(cx)
                    .base_text()
                    .remote_id()
                    == buffer_id)
                .then(|| path.clone())
        })
    }

    fn selected_path(&self, cx: &App) -> Option<String> {
        let editor = self.editor.read(cx).focused_editor().read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let (buffer, _) = snapshot.point_to_buffer_offset(selection.head())?;
        self.path_for_buffer(buffer.remote_id(), cx)
    }

    fn discard_reason(&self, path: &str, cx: &App) -> Option<SharedString> {
        if self.preparing_discard {
            return Some("Another discard operation is being prepared.".into());
        }
        discard_unavailable_reason(self.store.as_ref(), &self.repository, path, cx)
    }

    fn discard_selected_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Header menus have their own target; a shortcut must not fall back to the editor cursor.
        if !self
            .editor
            .read(cx)
            .focused_editor()
            .focus_handle(cx)
            .is_focused(window)
        {
            return;
        }
        if let Some(path) = self.selected_path(cx) {
            self.discard_path(path, window, cx);
        }
    }

    fn discard_path(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self.discard_reason(&path, cx) {
            self.error = Some(reason);
            cx.notify();
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        self.error = None;
        self.preparing_discard = true;
        let operation = discard_file_changes(store, self.repository.clone(), path, window, cx);
        cx.spawn(async move |this, cx| {
            let result = operation.await;
            this.update(cx, |this, cx| {
                this.preparing_discard = false;
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

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let repository = store
            .read(cx)
            .repositories()
            .into_iter()
            .find(|repository| repository.key == self.repository);
        let Some(repository) = repository else {
            self.cancel_refresh();
            self.repository_error =
                Some("Source-control repository is no longer available.".into());
            cx.notify();
            return;
        };
        self.repository_error = repository.error.clone();
        let Some(snapshot) = repository.snapshot else {
            self.cancel_refresh();
            self.loading = repository.loading;
            cx.notify();
            return;
        };
        let mut changes = store.read(cx).changes(&self.repository, cx);
        changes.sort_by(|left, right| left.path.cmp(&right.path));
        let fingerprint = (
            snapshot.baseline_id.clone(),
            snapshot.revision_id.clone(),
            changes,
        );
        let Some(changes) = self.prepare_refresh(fingerprint, cx) else {
            cx.notify();
            return;
        };
        if changes.is_empty() {
            cx.notify();
            return;
        }
        let generation = self.generation;
        let repository = self.repository.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let async_cx = cx.clone();
            let loads = changes.into_iter().map(move |change| {
                let store = store.clone();
                let repository = repository.clone();
                let mut cx = async_cx.clone();
                async move {
                    let result = store
                        .update(&mut cx, |store, cx| {
                            store.open_diff(repository, change.path.clone(), cx)
                        })
                        .await;
                    (change, result)
                }
            });
            let mut loads = stream::iter(loads).buffered(MAX_CONCURRENT_BUFFER_LOADS);
            while let Some((change, result)) = loads.next().await {
                futures_lite::future::yield_now().await;
                if this
                    .update_in(cx, |this, window, cx| {
                        this.apply_loaded(generation, change, result, window, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
            this.update(cx, |this, cx| this.finish_refresh(generation, cx))
                .log_err();
        }));
        cx.notify();
    }

    fn cancel_refresh(&mut self) {
        self.task = None;
        self.snapshot = None;
        self.generation += 1;
        self.loading = false;
    }

    fn prepare_refresh(
        &mut self,
        fingerprint: (String, String, Vec<Change>),
        cx: &mut Context<Self>,
    ) -> Option<Vec<Change>> {
        let changes = fingerprint
            .2
            .iter()
            .filter(|change| change.kind == ContentKind::File)
            .cloned()
            .collect::<Vec<_>>();
        self.retain_paths(&changes, cx);
        let changes = changes
            .into_iter()
            .filter(|change| {
                self.loaded.get(&change.path).is_none_or(|loaded| {
                    loaded.status != change.status && !loaded.opened.buffer.read(cx).is_dirty()
                })
            })
            .collect::<Vec<_>>();
        if self.snapshot.as_ref() == Some(&fingerprint) && (self.loading || changes.is_empty()) {
            return None;
        }
        self.metadata_count = fingerprint
            .2
            .iter()
            .filter(|change| change.kind != ContentKind::File)
            .count();
        self.snapshot = Some(fingerprint);
        self.error = None;
        self.task = None;
        self.generation += 1;
        self.loading = !changes.is_empty();
        Some(changes)
    }

    fn finish_refresh(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.generation == generation {
            self.loading = false;
            cx.notify();
        }
    }

    fn retain_paths(&mut self, changes: &[Change], cx: &mut Context<Self>) {
        let paths = changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<BTreeSet<_>>();
        let removed = self
            .loaded
            .iter()
            .filter(|(path, loaded)| {
                !paths.contains(path.as_str()) && !loaded.opened.buffer.read(cx).is_dirty()
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in removed {
            self.loaded.remove(&path);
            if let Some(path) = path_key(&path).log_err() {
                self.editor
                    .update(cx, |editor, cx| editor.remove_excerpts_for_path(path, cx));
            }
        }
    }

    fn apply_loaded(
        &mut self,
        generation: u64,
        change: Change,
        result: Result<VcsDiff>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation {
            return;
        }
        let result = result.and_then(|opened| {
            if self.loaded.get(&change.path).is_some_and(|previous| {
                previous.opened.buffer != opened.buffer
                    && previous.opened.buffer.read(cx).is_dirty()
            }) {
                return Ok(());
            }
            self.register_diff(change.path.clone(), change.status, opened, window, cx)
        });
        if let Err(error) = result {
            self.error = Some(format!("{}: {error:#}", change.path).into());
        }
        cx.notify();
    }

    fn register_diff(
        &mut self,
        path: String,
        status: ChangeStatus,
        opened: VcsDiff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        path_key(&path)?;
        let subscription = cx.subscribe_in(&opened.diff, window, {
            let path = path.clone();
            move |this, _, event, window, cx| {
                if matches!(event, BufferDiffEvent::DiffChanged(_)) {
                    this.update_ranges(&path, window, cx);
                }
            }
        });
        self.loaded.insert(
            path.clone(),
            LoadedDiff {
                opened,
                status,
                _subscription: subscription,
            },
        );
        self.update_ranges(&path, window, cx);
        Ok(())
    }

    fn update_ranges(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded.get(path) else {
            return;
        };
        let Some(key) = path_key(path).log_err() else {
            return;
        };
        let buffer = loaded.opened.buffer.clone();
        let diff = loaded.opened.diff.clone();
        let snapshot = buffer.read(cx).snapshot();
        let mut ranges = diff
            .read(cx)
            .snapshot(cx)
            .hunks_intersecting_range(
                Anchor::min_max_range_for_buffer(snapshot.remote_id()),
                &snapshot,
            )
            .map(|hunk| hunk.buffer_range.to_point(&snapshot))
            .collect::<Vec<_>>();
        if ranges.is_empty() {
            // Status-only changes still need a header and a navigation target.
            // Dirty files retain a saveable excerpt across baseline changes.
            let end = if buffer.read(cx).is_dirty() {
                snapshot.max_point()
            } else {
                Point::zero()
            };
            ranges.push(Point::zero()..end);
        }
        self.editor.update(cx, |editor, cx| {
            editor.update_excerpts_for_path(
                key,
                buffer,
                ranges,
                multibuffer_context_lines(cx),
                diff,
                cx,
            );
        });
        if self.pending_path.as_deref() == Some(path) {
            self.move_to_path(path.to_owned(), window, cx);
        }
        cx.notify();
    }

    fn move_to_path(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = path_key(&path).log_err() else {
            return;
        };
        self.pending_path = Some(path);
        let Some(position) = self.multibuffer.read(cx).location_for_path(&key, cx) else {
            return;
        };
        self.pending_path = None;
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::focused()),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges([position..position]);
                    },
                );
            });
        });
    }
}

struct VcsDiffAddon {
    view: WeakEntity<VcsProjectDiff>,
}

impl editor::Addon for VcsDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn render_buffer_header_controls(
        &self,
        _: &multi_buffer::ExcerptBoundaryInfo,
        buffer: &language::BufferSnapshot,
        _: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let view = self.view.upgrade()?;
        let path = view.read(cx).path_for_buffer(buffer.remote_id(), cx)?;
        let reason = view.read(cx).discard_reason(&path, cx);
        let weak_view = self.view.clone();
        Some(
            Button::new("vcs-discard-file", "Discard Changes")
                .label_size(LabelSize::Small)
                .disabled(reason.is_some())
                .tooltip(Tooltip::text(reason.unwrap_or_else(|| {
                    "Discard unstaged changes to this file".into()
                })))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    weak_view
                        .update(cx, |view, cx| view.discard_path(path.clone(), window, cx))
                        .log_err();
                })
                .into_any_element(),
        )
    }

    fn extend_buffer_header_context_menu(
        &self,
        menu: ContextMenu,
        buffer: &language::BufferSnapshot,
        _: &mut Window,
        cx: &mut App,
    ) -> ContextMenu {
        let Some(view) = self.view.upgrade() else {
            return menu;
        };
        let Some(path) = view.read(cx).path_for_buffer(buffer.remote_id(), cx) else {
            return menu;
        };
        let reason = view.read(cx).discard_reason(&path, cx);
        let weak_view = self.view.clone();
        discard_context_menu(menu.separator(), reason, move |window, cx| {
            weak_view
                .update(cx, |view, cx| view.discard_path(path.clone(), window, cx))
                .log_err();
        })
    }
}

fn path_key(path: &str) -> Result<PathKey> {
    Ok(PathKey::with_sort_prefix(
        0,
        RelPath::from_unix_str(path)?.into_arc(),
    ))
}

impl EventEmitter<EditorEvent> for VcsProjectDiff {}

impl Focusable for VcsProjectDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for VcsProjectDiff {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.multibuffer.read(cx).is_empty();
        v_flex()
            .size_full()
            .key_context("GitDiff")
            .capture_action(cx.listener(|this, _: &git::RestoreFile, window, cx| {
                cx.stop_propagation();
                this.discard_selected_file(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &DiscardFile, window, cx| {
                cx.stop_propagation();
                this.discard_selected_file(window, cx);
            }))
            .when_some(self.repository_error.clone(), |element, error| {
                element.child(Label::new(error).color(Color::Error))
            })
            .when_some(self.error.clone(), |element, error| {
                element.child(Label::new(error).color(Color::Error))
            })
            .when(empty, |element| {
                element.child(Label::new(if self.loading {
                    "Loading changes…"
                } else if self.metadata_count > 0 {
                    "No text changes. Non-text changes are listed in Source Control."
                } else {
                    "No uncommitted text changes"
                }))
            })
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

impl Item for VcsProjectDiff {
    type Event = EditorEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Uncommitted Changes".into()
    }

    fn to_item_events(event: &EditorEvent, emit: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, emit);
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.editor.read(cx).has_conflict(cx)
    }

    fn capability(&self, cx: &App) -> Capability {
        self.editor.read(cx).capability(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.reload(project, window, cx))
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        visit: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, visit);
    }

    fn navigate(
        &mut self,
        data: Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn set_nav_history(&mut self, history: ItemNavHistory, _: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, _| editor.set_nav_history(Some(history)));
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer_diff::{BufferDiff, DiffOperations};
    use gpui::{TestAppContext, VisualTestContext};
    use language::Buffer;
    use project::FakeFs;
    use settings::{SettingsStore, WorktreeId};
    use workspace::{MultiWorkspace, ToolbarItemLocation, ToolbarItemView};

    struct ReadOnlyOperations;

    impl DiffOperations for ReadOnlyOperations {
        fn supports_staging(&self) -> bool {
            false
        }
        fn supports_unstaging(&self) -> bool {
            false
        }
        fn supports_restore(&self) -> bool {
            false
        }
    }

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    fn repository(worktree_id: WorktreeId) -> VcsRepositoryId {
        VcsRepositoryId {
            worktree_id,
            extension_id: "fixture".into(),
            provider_id: "fixture".into(),
            repository_id: "repository".into(),
        }
    }

    fn change(path: &str) -> Change {
        Change {
            path: path.into(),
            old_path: None,
            status: ChangeStatus::Modified,
            kind: ContentKind::File,
            old_mode: None,
            new_mode: None,
            file_operations: None,
            unavailable_reason: None,
        }
    }

    async fn comparison(buffer: Entity<Buffer>, base: &str, cx: &mut VisualTestContext) -> VcsDiff {
        let diff = cx.new(|cx| {
            let snapshot = buffer.read(cx).snapshot();
            let mut diff = BufferDiff::new(&snapshot.text, None, None, cx);
            diff.set_operations(Arc::new(ReadOnlyOperations));
            diff
        });
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.set_base_text(Some(base.into()), snapshot, cx)
        })
        .await;
        VcsDiff { buffer, diff }
    }

    #[gpui::test]
    async fn aggregate_reuses_tab_navigates_without_focus_and_preserves_dirty_files(
        cx: &mut TestAppContext,
    ) {
        use fs::Fs as _;
        use project::ProjectItem as _;
        use std::path::Path;

        init(cx);
        let fs = FakeFs::new(cx.executor());
        let root = Path::new(util::path!("/project"));
        fs.insert_tree(
            root,
            serde_json::json!({"a.txt": "working a\n", "b.txt": "working b\n", "mode-only.sh": "unchanged\n", "renamed.txt": "unchanged\n"}),
        )
        .await;
        let project = Project::test(fs.clone(), [root], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let normal = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    root.join("a.txt"),
                    workspace::OpenOptions {
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .expect("ordinary file editor");
        let normal_editor = cx
            .read(|cx| normal.act_as::<Editor>(cx))
            .expect("ordinary editor");
        let buffer_a = normal_editor.read_with(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("file buffer")
        });
        let project_path = buffer_a.read_with(cx, |buffer, cx| {
            buffer.project_path(cx).expect("project path")
        });
        let buffer_b = project
            .update(cx, |project, cx| {
                project.open_buffer(
                    ProjectPath {
                        worktree_id: project_path.worktree_id,
                        path: RelPath::from_unix_str("b.txt").unwrap().into_arc(),
                    },
                    cx,
                )
            })
            .await
            .expect("second buffer");
        buffer_a.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "unsaved\n")], None, cx);
        });
        let opened_a = comparison(buffer_a.clone(), "old a\n", cx).await;
        let opened_b = comparison(buffer_b, "old b\n", cx).await;
        let mut status_only = Vec::new();
        for (path, status) in [
            ("mode-only.sh", ChangeStatus::Modified),
            ("renamed.txt", ChangeStatus::Renamed),
        ] {
            let buffer = project
                .update(cx, |project, cx| {
                    project.open_buffer(
                        ProjectPath {
                            worktree_id: project_path.worktree_id,
                            path: RelPath::from_unix_str(path).unwrap().into_arc(),
                        },
                        cx,
                    )
                })
                .await
                .expect("status-only file");
            let opened = comparison(buffer, "unchanged\n", cx).await;
            status_only.push((path, status, opened));
        }
        let key = repository(project_path.worktree_id);
        let item = workspace.update_in(cx, |workspace, window, cx| {
            open_item(workspace, key.clone(), None, false, window, cx)
        });
        item.update_in(cx, |item, window, cx| {
            item.move_to_path("b.txt".into(), window, cx);
            item.register_diff("a.txt".into(), ChangeStatus::Modified, opened_a, window, cx)
                .unwrap();
            item.register_diff("b.txt".into(), ChangeStatus::Modified, opened_b, window, cx)
                .unwrap();
            assert_eq!(
                item.active_project_path(cx).unwrap().path.as_unix_str(),
                "b.txt"
            );
            assert!(
                normal_editor.focus_handle(cx).contains_focused(window, cx),
                "deferred navigation must keep panel/previous focus"
            );
            assert_eq!(item.selected_path(cx).as_deref(), Some("b.txt"));
            assert_eq!(
                item.path_for_buffer(buffer_a.read(cx).remote_id(), cx)
                    .as_deref(),
                Some("a.txt"),
                "a header targets its own file independently of the selected excerpt"
            );
            let before = buffer_a.read(cx).text();
            let menu = ContextMenu::build(window, cx, |menu, _, _| {
                discard_context_menu(menu, None, |_, _| {})
            });
            menu.focus_handle(cx).focus(window, cx);
            item.discard_selected_file(window, cx);
            assert!(
                item.error.is_none(),
                "menu shortcuts must not target the editor cursor"
            );
            assert!(!item.preparing_discard);
            item.editor
                .read(cx)
                .focused_editor()
                .focus_handle(cx)
                .focus(window, cx);
            item.discard_selected_file(window, cx);
            assert!(
                !item.preparing_discard,
                "unavailable operations must not prepare or dispatch"
            );
            assert_eq!(
                item.error.as_deref(),
                Some("Source-control provider is unavailable.")
            );
            assert_eq!(buffer_a.read(cx).text(), before);
            item.error = None;
            normal_editor.focus_handle(cx).focus(window, cx);
            assert!(item.is_dirty(cx));
            let snapshot = item.multibuffer.read(cx).snapshot(cx);
            let paths = snapshot
                .excerpts()
                .map(|excerpt| {
                    snapshot
                        .path_for_buffer(excerpt.context.start.buffer_id)
                        .unwrap()
                        .path
                        .as_unix_str()
                        .to_owned()
                })
                .collect::<Vec<_>>();
            assert_eq!(paths, ["a.txt", "b.txt"]);
            let changed_lines = snapshot.total_changed_lines();
            for (path, status, opened) in status_only {
                assert!(!opened.buffer.read(cx).is_dirty());
                assert!(opened.diff.read(cx).snapshot(cx).is_empty());
                item.move_to_path(path.into(), window, cx);
                item.register_diff(path.into(), status, opened, window, cx)
                    .unwrap();
                assert!(
                    item.pending_path.is_none(),
                    "status-only paths must resolve their pending navigation"
                );
                assert_eq!(
                    item.active_project_path(cx).unwrap().path.as_unix_str(),
                    path
                );
            }
            assert_eq!(
                item.multibuffer.read(cx).snapshot(cx).total_changed_lines(),
                changed_lines,
                "headers for status-only files must not invent changed lines"
            );
            item.retain_paths(&[], cx);
            assert_eq!(
                item.loaded.len(),
                1,
                "dirty file remains saveable after snapshot removal"
            );
            assert!(item.is_dirty(cx));
        });
        let reopened = workspace.update_in(cx, |workspace, window, cx| {
            open_item(workspace, key, None, true, window, cx)
        });
        assert_eq!(item, reopened);
        workspace.update_in(cx, |workspace, window, cx| {
            let toolbar = cx.new(|cx| crate::project_diff::ProjectDiffToolbar::new(workspace, cx));
            toolbar.update(cx, |toolbar, cx| {
                assert!(matches!(
                    toolbar.set_active_pane_item(Some(&item), window, cx),
                    ToolbarItemLocation::PrimaryRight
                ));
            });
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.active_pane().read(cx).items_len(), 2);
            let active = workspace.active_item(cx).expect("aggregate active");
            assert!(active.to_serializable_item_handle(cx).is_none());
            assert!(active.to_searchable_item_handle(cx).is_some());
        });
        assert_eq!(fs.load(&root.join("a.txt")).await.unwrap(), "working a\n");
    }

    #[gpui::test]
    async fn stale_load_cannot_replace_live_edits_and_baseline_updates_keep_the_same_item(
        cx: &mut TestAppContext,
    ) {
        init(cx);
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let item = workspace.update_in(cx, |workspace, window, cx| {
            open_item(
                workspace,
                repository(WorktreeId::from_usize(1)),
                None,
                true,
                window,
                cx,
            )
        });
        let buffer = cx.new(|cx| Buffer::local("working\n", cx));
        let opened = comparison(buffer.clone(), "old\n", cx).await;
        item.update_in(cx, |item, window, cx| {
            item.generation = 2;
            item.editor
                .update(cx, |editor, cx| editor.split(window, cx));
            item.apply_loaded(1, change("stale.txt"), Ok(opened), window, cx);
            assert!(item.loaded.is_empty());
        });
        let opened = comparison(buffer.clone(), "old\n", cx).await;
        let diff = opened.diff.clone();
        item.update_in(cx, |item, window, cx| {
            item.apply_loaded(2, change("a.txt"), Ok(opened), window, cx)
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "unsaved\n")], None, cx);
        });
        let before = buffer.read_with(cx, |buffer, _| buffer.text());
        let replacement_buffer = cx.new(|cx| Buffer::local("", cx));
        let replacement = comparison(replacement_buffer, "old\n", cx).await;
        item.update_in(cx, |item, window, cx| {
            item.apply_loaded(2, change("a.txt"), Ok(replacement), window, cx);
            assert_eq!(item.loaded["a.txt"].opened.buffer, buffer);
        });
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
        diff.update(cx, |diff, cx| {
            diff.set_base_text(Some("new baseline\n".into()), snapshot, cx)
        })
        .await;
        cx.run_until_parked();
        item.read_with(cx, |item, cx| {
            assert_eq!(item.loaded["a.txt"].opened.buffer.read(cx).text(), before);
            let split = item.editor.read(cx);
            assert!(
                split
                    .lhs_editor()
                    .expect("split left side")
                    .read(cx)
                    .text(cx)
                    .contains("new baseline")
            );
            let operations = diff.read(cx).operations().expect("provider operations");
            assert!(!operations.supports_staging());
            assert!(!operations.supports_unstaging());
            assert!(!operations.supports_restore());
            assert!(item.is_dirty(cx));
        });
    }

    #[gpui::test]
    async fn unchanged_snapshot_retries_failed_and_interrupted_loads_without_restarting_progress(
        cx: &mut TestAppContext,
    ) {
        init(cx);
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let item = workspace.update_in(cx, |workspace, window, cx| {
            open_item(
                workspace,
                repository(WorktreeId::from_usize(1)),
                None,
                true,
                window,
                cx,
            )
        });
        let buffer_a = cx.new(|cx| Buffer::local("unsaved a\n", cx));
        let buffer_b = cx.new(|cx| Buffer::local("working b\n", cx));
        let buffer_c = cx.new(|cx| Buffer::local("working c\n", cx));
        let opened_a = comparison(buffer_a.clone(), "old a\n", cx).await;
        let opened_b = comparison(buffer_b, "old b\n", cx).await;
        let opened_c = comparison(buffer_c, "old c\n", cx).await;
        let fingerprint = (
            "working".to_owned(),
            "r1".to_owned(),
            vec![change("a.txt"), change("b.txt")],
        );
        item.update_in(cx, |item, window, cx| {
            let pending = item
                .prepare_refresh(fingerprint.clone(), cx)
                .expect("initial load");
            assert_eq!(pending.len(), 2);
            let generation = item.generation;
            item.apply_loaded(generation, change("a.txt"), Ok(opened_a), window, cx);
            assert!(
                item.prepare_refresh(fingerprint.clone(), cx).is_none(),
                "polling must not restart an ongoing load"
            );
            assert_eq!(item.generation, generation);
            item.apply_loaded(
                generation,
                change("b.txt"),
                Err(anyhow::anyhow!("temporary provider failure")),
                window,
                cx,
            );
            item.finish_refresh(generation, cx);
            assert!(item.error.is_some());

            let pending = item
                .prepare_refresh(fingerprint.clone(), cx)
                .expect("unchanged snapshot retries a missing file");
            assert_eq!(
                pending
                    .iter()
                    .map(|change| change.path.as_str())
                    .collect::<Vec<_>>(),
                ["b.txt"]
            );
            assert!(item.error.is_none());
            assert_eq!(item.loaded["a.txt"].opened.buffer, buffer_a);
            let generation = item.generation;
            item.apply_loaded(generation, change("b.txt"), Ok(opened_b), window, cx);
            item.finish_refresh(generation, cx);
            assert!(
                item.prepare_refresh(fingerprint.clone(), cx).is_none(),
                "a completed unchanged snapshot needs no new loads"
            );

            let mut fingerprint = fingerprint.clone();
            fingerprint.2.push(change("c.txt"));
            let pending = item
                .prepare_refresh(fingerprint.clone(), cx)
                .expect("additional file");
            assert_eq!(pending.len(), 1);
            let interrupted_generation = item.generation;
            item.cancel_refresh();
            let pending = item
                .prepare_refresh(fingerprint, cx)
                .expect("same repository snapshot after reconnect");
            assert_eq!(
                pending
                    .iter()
                    .map(|change| change.path.as_str())
                    .collect::<Vec<_>>(),
                ["c.txt"]
            );
            item.apply_loaded(
                interrupted_generation,
                change("c.txt"),
                Ok(VcsDiff {
                    buffer: opened_c.buffer.clone(),
                    diff: opened_c.diff.clone(),
                }),
                window,
                cx,
            );
            assert!(!item.loaded.contains_key("c.txt"));
            let generation = item.generation;
            item.apply_loaded(generation, change("c.txt"), Ok(opened_c), window, cx);
            item.finish_refresh(generation, cx);
            assert_eq!(item.loaded.len(), 3);
            assert_eq!(item.loaded["a.txt"].opened.buffer, buffer_a);
            assert_eq!(buffer_a.read(cx).text(), "unsaved a\n");
            assert!(!item.loading);
        });
    }
}
