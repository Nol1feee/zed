use std::{any::TypeId, ops::Range, sync::Arc};

use editor::{Editor, EditorSettings, SplittableEditor};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollStrategy, Task,
    UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use language::Capability;
use multi_buffer::{MultiBuffer, PathKey};
use project::{
    Project, ProjectPath,
    vcs_store::{VcsDiff, VcsRepositoryId, VcsStore},
};
use settings::Settings;
use ui::{Button, Color, Label, ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, rel_path::RelPath};
use vcs::FileHistoryEntry;
use workspace::{Item, Workspace, notifications::NotifyTaskExt, searchable::SearchableItemHandle};

pub(crate) fn deploy(
    workspace: &mut Workspace,
    repository: VcsRepositoryId,
    path: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<VcsFileHistory>(cx)
        .find(|item| item.read(cx).repository == repository && item.read(cx).path == path);
    if let Some(item) = existing {
        workspace.activate_item(&item, true, true, window, cx);
        return;
    }
    let project = workspace.project().clone();
    let Some(store) = project.read(cx).vcs_store().cloned() else {
        return;
    };
    let workspace_entity = cx.entity();
    let item = cx.new(|cx| {
        let mut item = VcsFileHistory {
            workspace: workspace_entity.downgrade(),
            project,
            store,
            repository,
            path,
            focus: cx.focus_handle(),
            entries: Vec::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            next_cursor: None,
            truncated: false,
            loading: false,
            loading_diff: false,
            error: None,
            editor: None,
            generation: 0,
            history_task: None,
            diff_task: None,
        };
        item.load_history(false, window, cx);
        item
    });
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

pub struct VcsFileHistory {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<VcsStore>,
    repository: VcsRepositoryId,
    path: String,
    focus: FocusHandle,
    entries: Vec<FileHistoryEntry>,
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    next_cursor: Option<String>,
    truncated: bool,
    loading: bool,
    loading_diff: bool,
    error: Option<SharedString>,
    editor: Option<Entity<SplittableEditor>>,
    generation: u64,
    history_task: Option<Task<()>>,
    diff_task: Option<Task<()>>,
}

impl VcsFileHistory {
    #[cfg(any(test, feature = "test-support"))]
    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({
            "path": self.path,
            "entry_count": self.entries.len(),
            "selected_revision": self.selected.and_then(|index| self.entries.get(index)).map(|entry| &entry.commit.revision_id),
            "loading": self.loading,
            "loading_diff": self.loading_diff,
            "truncated": self.truncated,
            "error": self.error.as_ref().map(|error| error.as_ref()),
        })
    }

    fn load_history(&mut self, more: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let cursor = if more { self.next_cursor.clone() } else { None };
        if !more {
            self.generation += 1;
            self.entries.clear();
            self.selected = None;
            self.editor = None;
            self.diff_task = None;
            self.next_cursor = None;
            self.truncated = false;
        }
        self.loading = true;
        self.error = None;
        let task = self.store.update(cx, |store, cx| {
            store.file_history(self.repository.clone(), self.path.clone(), cursor, 50, cx)
        });
        self.history_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok(history) => {
                        this.next_cursor = history.next_cursor;
                        this.truncated = history.truncated;
                        for entry in history.entries {
                            if !this.entries.iter().any(|existing| {
                                existing.commit.revision_id == entry.commit.revision_id
                            }) {
                                this.entries.push(entry);
                            }
                        }
                        if this.selected.is_none() && !this.entries.is_empty() {
                            this.select(0, window, cx);
                        }
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        self.selected = Some(index);
        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        self.generation += 1;
        let generation = self.generation;
        self.loading_diff = true;
        self.editor = None;
        self.error = None;
        let task = self.store.update(cx, |store, cx| {
            store.open_revision_diff(self.repository.clone(), self.path.clone(), entry, cx)
        });
        self.diff_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading_diff = false;
                match result {
                    Ok(diff) => this.install_diff(diff, window, cx),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn install_diff(&mut self, opened: VcsDiff, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::without_headers(Capability::ReadOnly);
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
            multibuffer
        });
        self.editor = Some(cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                self.project.clone(),
                workspace,
                window,
                cx,
            );
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.set_should_serialize(false, cx));
            editor
        }));
    }

    fn open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = RelPath::from_unix_str(&self.path).log_err() else {
            return;
        };
        let path = ProjectPath {
            worktree_id: self.repository.worktree_id,
            path: Arc::from(path),
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(path, None, false, false, true, window, cx)
                    .detach_and_notify_err(self.workspace.clone(), window, cx);
            })
            .log_err();
    }

    fn render_entries(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|index| {
                let entry = self.entries.get(index)?;
                let commit = &entry.commit;
                let revision = commit.revision_id.chars().take(12).collect::<String>();
                let date = commit
                    .author_time
                    .and_then(|time| time::OffsetDateTime::from_unix_timestamp(time).ok())
                    .map(|time| time.date().to_string());
                let metadata = h_flex()
                    .gap_1()
                    .child(
                        Label::new(revision.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when_some(commit.author.clone(), |element, author| {
                        element.child(
                            Label::new(author)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    });
                let content = v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(Label::new(commit.summary.clone().unwrap_or(revision)).truncate())
                    .child(metadata)
                    .when_some(date, |element, date| {
                        element.child(Label::new(date).size(LabelSize::Small).color(Color::Muted))
                    });
                Some(
                    ListItem::new(index)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(self.selected == Some(index))
                        .child(content)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.focus.focus(window, cx);
                            this.select(index, window, cx);
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl EventEmitter<()> for VcsFileHistory {}
impl Focusable for VcsFileHistory {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for VcsFileHistory {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(format!("File History · {}", self.path)).truncate())
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Button::new("vcs-history-open-file", "View File").on_click(
                                cx.listener(|this, _, window, cx| this.open_file(window, cx)),
                            ))
                            .child(
                                Button::new("vcs-history-refresh", "Refresh")
                                    .disabled(self.loading)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.load_history(false, window, cx)
                                    })),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |element, error| {
                element.child(
                    div()
                        .px_3()
                        .py_2()
                        .child(Label::new(error).color(Color::Error)),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w_80()
                            .flex_none()
                            .h_full()
                            .border_r_1()
                            .border_color(cx.theme().colors().border_variant)
                            .key_context("VcsFileHistory menu")
                            .track_focus(&self.focus)
                            .on_action(cx.listener(|this, _: &menu::SelectNext, window, cx| {
                                let index = this.selected.map_or(0, |index| index + 1);
                                this.select(index, window, cx);
                            }))
                            .on_action(cx.listener(|this, _: &menu::SelectPrevious, window, cx| {
                                this.select(
                                    this.selected.unwrap_or(0).saturating_sub(1),
                                    window,
                                    cx,
                                );
                            }))
                            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                if let Some(editor) = &this.editor {
                                    editor.focus_handle(cx).focus(window, cx);
                                }
                            }))
                            .when(self.entries.is_empty(), |element| {
                                element.child(
                                    div().p_3().child(
                                        Label::new(if self.loading {
                                            "Loading file history…"
                                        } else {
                                            "No revisions available"
                                        })
                                        .color(Color::Muted),
                                    ),
                                )
                            })
                            .child(
                                uniform_list(
                                    "vcs-file-history-entries",
                                    self.entries.len(),
                                    cx.processor(Self::render_entries),
                                )
                                .track_scroll(&self.scroll)
                                .flex_1(),
                            )
                            .when(self.next_cursor.is_some(), |element| {
                                element.child(
                                    Button::new(
                                        "vcs-history-more",
                                        if self.loading {
                                            "Loading…"
                                        } else {
                                            "Load More"
                                        },
                                    )
                                    .disabled(self.loading)
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.load_history(true, window, cx),
                                    )),
                                )
                            })
                            .when(self.truncated, |element| {
                                element.child(
                                    div().p_2().child(
                                        Label::new(
                                            "Latest revisions shown; older revisions may exist",
                                        )
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .when_some(self.editor.clone(), |element, editor| element.child(editor))
                            .when(self.editor.is_none(), |element| {
                                element.child(
                                    div().p_4().child(
                                        Label::new(if self.loading_diff {
                                            "Loading revision…"
                                        } else {
                                            "Select a revision to view its changes"
                                        })
                                        .color(Color::Muted),
                                    ),
                                )
                            }),
                    ),
            )
    }
}

impl Item for VcsFileHistory {
    type Event = ();
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!(
            "{} — File History",
            self.path.rsplit('/').next().unwrap_or(&self.path)
        )
        .into()
    }
    fn capability(&self, _: &App) -> Capability {
        Capability::ReadOnly
    }
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        self.editor
            .clone()
            .map(|editor| Box::new(editor) as Box<dyn SearchableItemHandle>)
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
            self.editor.clone().map(Into::into)
        } else if type_id == TypeId::of::<Editor>() {
            self.editor
                .as_ref()
                .map(|editor| editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }
}
