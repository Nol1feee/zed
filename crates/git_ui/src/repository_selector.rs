use crate::{git_panel::GitPanel, git_status_icon};
use git::status::{FileStatus, StatusCode, TrackedStatus, UnmergedStatus, UnmergedStatusCode};
use gpui::{App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use project::{Project, git_store::Repository, vcs_store::VcsRepositorySnapshot};
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(open);
}

pub fn open(
    workspace: &mut Workspace,
    _: &zed_actions::git::SelectRepo,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let panel = workspace
        .panel::<GitPanel>(cx)
        .map(|panel| panel.downgrade());
    workspace.toggle_modal(window, cx, |window, cx| {
        RepositorySelector::new(project, panel, rems(34.), window, cx)
    })
}

#[derive(Clone)]
enum RepositoryEntry {
    Git(Entity<Repository>),
    External(VcsRepositorySnapshot),
}

impl RepositoryEntry {
    fn display_name(&self, cx: &App) -> String {
        match self {
            Self::Git(repository) => repository.read(cx).display_name().to_string(),
            Self::External(repository) => repository.descriptor.display_name.clone(),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Git(left), Self::Git(right)) => left == right,
            (Self::External(left), Self::External(right)) => left.key == right.key,
            _ => false,
        }
    }
}

pub struct RepositorySelector {
    picker: Entity<Picker<RepositorySelectorDelegate>>,
}

impl RepositorySelector {
    pub fn new(
        project: Entity<Project>,
        panel: Option<WeakEntity<GitPanel>>,
        width: Rems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = project.read(cx);
        let git_store = project.git_store().read(cx);
        let mut repository_entries = git_store
            .repositories()
            .values()
            .cloned()
            .map(RepositoryEntry::Git)
            .collect::<Vec<_>>();
        if panel.is_some() {
            repository_entries.extend(
                project
                    .vcs_store()
                    .into_iter()
                    .flat_map(|store| store.read(cx).repositories())
                    .map(RepositoryEntry::External),
            );
        }
        repository_entries.sort_by_key(|repository| repository.display_name(cx).to_lowercase());
        let panel_entity = panel.as_ref().and_then(WeakEntity::upgrade);
        let external_selected = panel_entity
            .as_ref()
            .is_some_and(|panel| panel.read(cx).uses_vcs_panel(cx));
        let selected_vcs = panel_entity
            .as_ref()
            .and_then(|panel| panel.read(cx).selected_vcs_repository(cx));
        let active_repository = if let Some(key) = selected_vcs {
            repository_entries.iter().find(|entry| matches!(entry, RepositoryEntry::External(repository) if repository.key == key)).cloned()
        } else if external_selected {
            None
        } else {
            git_store.active_repository().map(RepositoryEntry::Git)
        };
        let selected_index = active_repository
            .as_ref()
            .and_then(|active| {
                repository_entries
                    .iter()
                    .position(|entry| entry.matches(active))
            })
            .unwrap_or(0);
        let delegate = RepositorySelectorDelegate {
            repository_selector: cx.entity().downgrade(),
            panel,
            filtered_repositories: repository_entries.clone(),
            repository_entries,
            active_repository,
            selected_index,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(width)
                .show_scrollbar(true)
        });
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for RepositorySelector {}
impl Focusable for RepositorySelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}
impl Render for RepositorySelector {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("GitRepositorySelector")
            .child(self.picker.clone())
    }
}
impl ModalView for RepositorySelector {}

pub struct RepositorySelectorDelegate {
    repository_selector: WeakEntity<RepositorySelector>,
    panel: Option<WeakEntity<GitPanel>>,
    repository_entries: Vec<RepositoryEntry>,
    filtered_repositories: Vec<RepositoryEntry>,
    active_repository: Option<RepositoryEntry>,
    selected_index: usize,
}

impl PickerDelegate for RepositorySelectorDelegate {
    type ListItem = ListItem;
    fn name() -> &'static str {
        "repository selector"
    }
    fn match_count(&self) -> usize {
        self.filtered_repositories.len()
    }
    fn selected_index(&self) -> usize {
        self.selected_index
    }
    fn set_selected_index(&mut self, index: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = index.min(self.filtered_repositories.len().saturating_sub(1));
        cx.notify();
    }
    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Select a repository...".into()
    }
    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::End
    }
    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.filtered_repositories = self
            .repository_entries
            .iter()
            .filter(|entry| entry.display_name(cx).to_lowercase().contains(&query))
            .cloned()
            .collect();
        self.selected_index = self
            .active_repository
            .as_ref()
            .and_then(|active| {
                self.filtered_repositories
                    .iter()
                    .position(|entry| entry.matches(active))
            })
            .unwrap_or(0);
        cx.notify();
        Task::ready(())
    }
    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(selected) = self.filtered_repositories.get(self.selected_index).cloned() else {
            return;
        };
        match selected {
            RepositoryEntry::Git(repository) => {
                repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
                if let Some(panel) = self.panel.as_ref().and_then(WeakEntity::upgrade) {
                    panel.update(cx, |panel, cx| panel.select_git_repository(window, cx));
                }
            }
            RepositoryEntry::External(repository) => {
                if let Some(panel) = self.panel.as_ref().and_then(WeakEntity::upgrade) {
                    panel.update(cx, |panel, cx| {
                        panel.select_vcs_repository(repository.key, window, cx)
                    });
                }
            }
        }
        self.dismissed(window, cx);
    }
    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.repository_selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }
    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.filtered_repositories.get(index)?;
        let is_active = self
            .active_repository
            .as_ref()
            .is_some_and(|active| entry.matches(active));
        let mut item = ListItem::new(index)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new(entry.display_name(cx)))
                    .when(is_active, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                    }),
            );
        match entry {
            RepositoryEntry::External(repository) => {
                item = item.end_slot(
                    Label::new(repository.key.provider_id.to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            RepositoryEntry::Git(repository) => {
                let summary = repository.read(cx).status_summary();
                if summary.count > 0 {
                    let status = if summary.conflict > 0 {
                        FileStatus::Unmerged(UnmergedStatus {
                            first_head: UnmergedStatusCode::Updated,
                            second_head: UnmergedStatusCode::Updated,
                        })
                    } else if summary.worktree.deleted > 0 || summary.index.deleted > 0 {
                        FileStatus::Tracked(TrackedStatus {
                            index_status: StatusCode::Deleted,
                            worktree_status: StatusCode::Unmodified,
                        })
                    } else if summary.worktree.modified > 0 || summary.index.modified > 0 {
                        FileStatus::Tracked(TrackedStatus {
                            index_status: StatusCode::Modified,
                            worktree_status: StatusCode::Unmodified,
                        })
                    } else {
                        FileStatus::Tracked(TrackedStatus {
                            index_status: StatusCode::Added,
                            worktree_status: StatusCode::Unmodified,
                        })
                    };
                    item = item.end_slot(div().pr_2().child(git_status_icon(status)));
                }
            }
        }
        Some(item)
    }
}
