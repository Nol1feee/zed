use anyhow::Result;
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    PromptLevel, Subscription, WeakEntity, Window,
};
use project::vcs_store::{VcsRepositorySnapshot, VcsStore};
use ui::{Button, Label, prelude::*};
use util::ResultExt as _;
use vcs::RepositoryOperation;
use workspace::{ModalView, Workspace};

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", menu::Confirm, Some("VcsCommit")),
        KeyBinding::new("escape", menu::Cancel, Some("VcsCommit")),
    ]);
}

pub fn show_commit(
    workspace: WeakEntity<Workspace>,
    store: Entity<VcsStore>,
    repository: VcsRepositorySnapshot,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                CommitModal::new(store, repository, window, cx)
            });
        })
        .log_err();
}

struct CommitModal {
    store: Entity<VcsStore>,
    repository: VcsRepositorySnapshot,
    message: Entity<Editor>,
    busy: bool,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl CommitModal {
    fn new(
        store: Entity<VcsStore>,
        repository: VcsRepositorySnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let message = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Commit message", window, cx);
            editor
        });
        let subscription = cx.observe(&message, |_, _, cx| cx.notify());
        Self {
            store,
            repository,
            message,
            busy: false,
            error: None,
            _subscription: subscription,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if !self.busy {
            cx.emit(DismissEvent);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let message = self.message.read(cx).text(cx);
        if message.trim().is_empty() {
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let store = self.store.clone();
        let repository = self.repository.clone();
        let preparation = store.update(cx, |store, cx| {
            store.prepare_repository_operation(
                repository.key.clone(),
                RepositoryOperation::Commit {
                    message: message.clone(),
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<bool> = async {
                let prepared = preparation.await?;
                let detail = format!(
                    "Project: {}\nScope: {}\n\n{}\n\nMessage: {}\n\nFiles:\n{}",
                    repository.descriptor.display_name,
                    repository.descriptor.scope_uri,
                    prepared.summary(),
                    message,
                    prepared.affected_paths().join("\n"),
                );
                let answer = this
                    .update_in(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Info,
                            "Commit these staged changes?",
                            Some(&detail),
                            &["Commit", "Cancel"],
                            cx,
                        )
                    })?
                    .await?;
                if answer != 0 {
                    return Ok(false);
                }
                store
                    .update(cx, |store, cx| {
                        store.apply_prepared_repository_operation(prepared, cx)
                    })
                    .await?;
                Ok(true)
            }
            .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(true) => cx.emit(DismissEvent),
                    Ok(false) => {}
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }
}

impl ModalView for CommitModal {
    fn on_before_dismiss(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        workspace::DismissDecision::Dismiss(!self.busy)
    }
}
impl EventEmitter<DismissEvent> for CommitModal {}
impl Focusable for CommitModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.message.focus_handle(cx)
    }
}

impl Render for CommitModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("vcs-commit-modal")
            .key_context("VcsCommit")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_2(cx)
            .w(rems(36.))
            .p_3()
            .gap_2()
            .child(Label::new("Commit staged changes"))
            .child(
                Label::new(self.repository.descriptor.scope_uri.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().id("vcs-commit-message").child(self.message.clone()))
            .when_some(self.error.clone(), |element, error| {
                element.child(Label::new(error).color(Color::Error))
            })
            .when(self.busy, |element| {
                element.child(Label::new("Preparing or applying commit…").color(Color::Muted))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("vcs-confirm-commit", "Commit Staged")
                            .disabled(self.busy || self.message.read(cx).text(cx).trim().is_empty())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    )
                    .child(
                        Button::new("vcs-cancel-commit", "Cancel")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel(&menu::Cancel, window, cx)
                            })),
                    ),
            )
    }
}
