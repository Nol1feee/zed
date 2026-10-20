use anyhow::{Result, anyhow};
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    PromptLevel, Subscription, WeakEntity, Window, actions,
};
use project::vcs_store::{VcsRepositoryId, VcsRepositorySnapshot, VcsStore};
use ui::{Button, Label, prelude::*};
use util::ResultExt as _;
use vcs::{
    CheckoutDescriptor, CheckoutOperation, CheckoutOperationKind, CheckoutPreparation,
    CheckoutState,
};
use workspace::{ModalView, Workspace};

use crate::worktree_service::open_provider_checkout_workspace;

actions!(
    vcs,
    [
        /// Opens checkouts managed by an external version-control provider.
        OpenCheckouts,
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "escape",
        menu::Cancel,
        Some("VcsCheckouts"),
    )]);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenCheckouts, window, cx| {
            show(workspace, None, window, cx)
        });
    })
    .detach();
}

pub fn show(
    workspace: &mut Workspace,
    selected: Option<VcsRepositoryId>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(store) = workspace.project().read(cx).vcs_store().cloned() else {
        return;
    };
    let workspace_handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, |window, cx| {
        CheckoutPicker::new(workspace_handle, store, selected, window, cx)
    });
}

pub struct CheckoutPicker {
    workspace: WeakEntity<Workspace>,
    store: Entity<VcsStore>,
    selected: Option<VcsRepositoryId>,
    filter: Entity<Editor>,
    name: Entity<Editor>,
    branch: Entity<Editor>,
    base: Entity<Editor>,
    creating: bool,
    busy: bool,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl CheckoutPicker {
    fn new(
        workspace: WeakEntity<Workspace>,
        store: Entity<VcsStore>,
        selected: Option<VcsRepositoryId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repositories = store
            .read(cx)
            .repositories()
            .into_iter()
            .filter(|repository| store.read(cx).checkout_catalog_available(&repository.key))
            .collect::<Vec<_>>();
        let selected =
            selected.or_else(|| (repositories.len() == 1).then(|| repositories[0].key.clone()));
        let filter = input("Filter checkouts", "", window, cx);
        let name = input("Checkout name", "", window, cx);
        let branch = input("New branch", "", window, cx);
        let base = input("Base branch or revision", "", window, cx);
        let subscriptions = vec![
            cx.observe(&store, |this, _, cx| {
                let repositories = this
                    .store
                    .read(cx)
                    .repositories()
                    .into_iter()
                    .filter(|repository| {
                        this.store
                            .read(cx)
                            .checkout_catalog_available(&repository.key)
                    })
                    .collect::<Vec<_>>();
                if !repositories
                    .iter()
                    .any(|repository| Some(&repository.key) == this.selected.as_ref())
                {
                    this.selected = if repositories.len() == 1 {
                        repositories
                            .first()
                            .map(|repository| repository.key.clone())
                    } else {
                        None
                    };
                    this.refresh(cx);
                }
                cx.notify();
            }),
            cx.observe(&filter, |_, _, cx| cx.notify()),
            cx.observe(&name, |_, _, cx| cx.notify()),
            cx.observe(&branch, |_, _, cx| cx.notify()),
            cx.observe(&base, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            workspace,
            store,
            selected,
            filter,
            name,
            branch,
            base,
            creating: false,
            busy: false,
            error: None,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    fn repository(&self, cx: &App) -> Option<VcsRepositorySnapshot> {
        self.store
            .read(cx)
            .repositories()
            .into_iter()
            .find(|repository| Some(&repository.key) == self.selected.as_ref())
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = &self.selected {
            self.store
                .update(cx, |store, cx| store.refresh_checkouts(key, cx));
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if !self.busy {
            cx.emit(DismissEvent);
        }
    }

    fn begin_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository(cx) else {
            return;
        };
        let current_branch = repository
            .repository_state
            .as_ref()
            .and_then(|state| state.current_branch.as_ref())
            .map(|branch| branch.name.clone())
            .unwrap_or_default();
        self.base
            .update(cx, |editor, cx| editor.set_text(current_branch, window, cx));
        self.creating = true;
        self.error = None;
        self.name.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let operation = CheckoutOperation::Create {
            name: self.name.read(cx).text(cx),
            branch: self.branch.read(cx).text(cx),
            base: self.base.read(cx).text(cx),
        };
        self.perform(operation, window, cx);
    }

    fn perform(
        &mut self,
        operation: CheckoutOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let Some(repository) = self.repository(cx) else {
            return;
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        let is_create = matches!(operation, CheckoutOperation::Create { .. });
        let should_open = is_create || matches!(operation, CheckoutOperation::Open { .. });
        let is_remove = matches!(operation, CheckoutOperation::Remove { .. });
        let preparation = store.update(cx, |store, cx| {
            store.prepare_checkout_operation(repository.key.clone(), operation, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<bool> = async {
                let prepared = preparation.await?;
                let detail = format!("Project scope: {}\nTarget checkout: {}\n\n{}", repository.descriptor.scope_uri, prepared.target_root_uri(), prepared.summary());
                let accepted = this.update_in(cx, |_, window, cx| window.prompt(
                    if is_remove { PromptLevel::Warning } else { PromptLevel::Info },
                    if is_remove { "Permanently remove this checkout?" } else { "Apply checkout operation?" },
                    Some(&detail), &[if is_remove { "Remove Checkout" } else { "Continue" }, "Cancel"], cx,
                ))?.await? == 0;
                if !accepted { return Ok(false); }
                let result = store.update(cx, |store, cx| store.apply_prepared_checkout_operation(prepared, cx)).await?;
                if is_create {
                    this.update(cx, |this, cx| { this.creating = false; cx.notify(); })?;
                }
                if should_open {
                    let checkout = result.checkout.ok_or_else(|| anyhow!("Provider did not return the resulting checkout"))?;
                    if checkout.preparation != CheckoutPreparation::Ready {
                        anyhow::bail!("Checkout {} was retained. Preparation failed: {}. Refresh and retry Prepare for this same checkout.", checkout.name, checkout.preparation_error.as_deref().unwrap_or("readiness was not confirmed"));
                    }
                    workspace.update_in(cx, |workspace, window, cx| open_provider_checkout_workspace(workspace, &repository, &checkout, is_create, true, window, cx))?.await?;
                    return Ok(true);
                }
                Ok(false)
            }.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(true) => cx.emit(DismissEvent),
                    Ok(false) => {},
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.refresh(cx);
                cx.notify();
            }).log_err();
        }).detach();
    }

    fn row(
        &self,
        checkout: CheckoutDescriptor,
        index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let active = vcs::local_path(&checkout.root_uri)
            .ok()
            .is_some_and(|root| VcsStore::checkout_in_use(&root, cx));
        let reason = if active {
            Some("Open in a workspace. Close it before suspending or removing.".to_string())
        } else {
            checkout.unavailable_reason.clone()
        };
        let mut buttons = Vec::new();
        for (kind, label) in [
            (CheckoutOperationKind::Open, "Open"),
            (CheckoutOperationKind::Prepare, "Prepare"),
            (CheckoutOperationKind::Suspend, "Suspend"),
            (CheckoutOperationKind::Remove, "Remove"),
        ] {
            let allowed = checkout.operations.contains(&kind)
                && !(active
                    && matches!(
                        kind,
                        CheckoutOperationKind::Suspend | CheckoutOperationKind::Remove
                    ));
            let operation = match kind {
                CheckoutOperationKind::Open => CheckoutOperation::Open {
                    checkout_id: checkout.id.clone(),
                },
                CheckoutOperationKind::Prepare => CheckoutOperation::Prepare {
                    checkout_id: checkout.id.clone(),
                },
                CheckoutOperationKind::Suspend => CheckoutOperation::Suspend {
                    checkout_id: checkout.id.clone(),
                },
                CheckoutOperationKind::Remove => CheckoutOperation::Remove {
                    checkout_id: checkout.id.clone(),
                },
                CheckoutOperationKind::Create => continue,
            };
            let id = match kind {
                CheckoutOperationKind::Open => "vcs-checkout-open",
                CheckoutOperationKind::Prepare => "vcs-checkout-prepare",
                CheckoutOperationKind::Suspend => "vcs-checkout-suspend",
                _ => "vcs-checkout-remove",
            };
            buttons.push(
                Button::new((id, index), label)
                    .disabled(self.busy || !allowed)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.perform(operation.clone(), window, cx)
                    })),
            );
        }
        let state = match checkout.state {
            CheckoutState::Mounted => "Mounted",
            CheckoutState::Unmounted => "Unmounted",
            CheckoutState::Missing => "Missing",
            CheckoutState::Busy => "Busy",
        };
        v_flex()
            .id(("vcs-checkout", index))
            .p_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(checkout.name.clone()))
            .child(
                Label::new(format!(
                    "{} · {} · {}",
                    state,
                    if checkout.owned { "Owned" } else { "Read only" },
                    checkout.branch.as_deref().unwrap_or("Unknown branch")
                ))
                .size(LabelSize::Small),
            )
            .child(
                Label::new(checkout.root_uri.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(reason, |element, reason| {
                element.child(
                    Label::new(reason)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(checkout.preparation_error, |element, error| {
                element
                    .child(Label::new(format!("Preparation failed: {error}")).color(Color::Error))
            })
            .child(h_flex().gap_1().children(buttons))
            .into_any_element()
    }
}

fn input(placeholder: &str, text: &str, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text(text, window, cx);
        editor
    })
}

impl ModalView for CheckoutPicker {
    fn on_before_dismiss(
        &mut self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        workspace::DismissDecision::Dismiss(!self.busy)
    }
}
impl EventEmitter<DismissEvent> for CheckoutPicker {}
impl Focusable for CheckoutPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }
}
impl Render for CheckoutPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let repository = self.repository(cx);
        let filter = self.filter.read(cx).text(cx).to_lowercase();
        let rows = repository
            .as_ref()
            .and_then(|repository| repository.checkouts.as_ref())
            .map(|catalog| {
                catalog
                    .checkouts
                    .iter()
                    .cloned()
                    .enumerate()
                    .filter(|(_, checkout)| {
                        format!(
                            "{} {} {}",
                            checkout.name,
                            checkout.branch.as_deref().unwrap_or(""),
                            checkout.root_uri
                        )
                        .to_lowercase()
                        .contains(&filter)
                    })
                    .map(|(index, checkout)| self.row(checkout, index, cx).into_any_element())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let can_create = self.selected.as_ref().is_some_and(|key| {
            self.store
                .read(cx)
                .checkout_operations(key)
                .contains(&CheckoutOperationKind::Create)
        });
        let progress = self
            .selected
            .as_ref()
            .and_then(|key| self.store.read(cx).operation_progress(key));
        let repositories = self
            .store
            .read(cx)
            .repositories()
            .into_iter()
            .filter(|repository| {
                self.store
                    .read(cx)
                    .checkout_catalog_available(&repository.key)
            })
            .collect::<Vec<_>>();
        v_flex()
            .id("vcs-checkout-picker")
            .key_context("VcsCheckouts")
            .on_action(cx.listener(Self::cancel))
            .elevation_2(cx)
            .w(rems(44.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new("Checkouts"))
                    .child(
                        Button::new("vcs-checkout-close", "Close")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cancel(&menu::Cancel, window, cx)
                            })),
                    ),
            )
            .when(repositories.len() != 1, |element| {
                element.child(
                    h_flex().gap_1().flex_wrap().children(
                        repositories
                            .into_iter()
                            .enumerate()
                            .map(|(index, repository)| {
                                let selected = self.selected.as_ref() == Some(&repository.key);
                                Button::new(
                                    ("vcs-checkout-source", index),
                                    repository.descriptor.display_name,
                                )
                                .toggle_state(selected)
                                .disabled(self.busy)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.selected = Some(repository.key.clone());
                                        this.creating = false;
                                        this.error = None;
                                        this.refresh(cx);
                                        cx.notify();
                                    },
                                ))
                            }),
                    ),
                )
            })
            .when_some(repository.as_ref(), |element, repository| {
                element.child(
                    Label::new(repository.descriptor.scope_uri.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(repository.is_none(), |element| element.child(Label::new("Select a project with checkout support. Keep it open while managing its checkouts.").color(Color::Muted)))
            .child(div().id("vcs-checkout-filter").child(self.filter.clone()))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("vcs-checkout-new", "Create Checkout")
                            .disabled(self.busy || !can_create)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.begin_create(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("vcs-checkout-refresh", "Refresh")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .when(self.creating, |element| {
                element.child(
                    v_flex()
                        .gap_2()
                        .child(Label::new("Create a checkout and a new branch"))
                        .child(div().id("vcs-checkout-name").child(self.name.clone()))
                        .child(div().id("vcs-checkout-branch").child(self.branch.clone()))
                        .child(div().id("vcs-checkout-base").child(self.base.clone()))
                        .child(
                            Button::new("vcs-checkout-create", "Create")
                                .disabled(
                                    self.busy
                                        || [&self.name, &self.branch, &self.base]
                                            .iter()
                                            .any(|input| input.read(cx).text(cx).trim().is_empty()),
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.create(window, cx)),
                                ),
                        ),
                )
            })
            .when_some(self.error.clone(), |element, error| {
                element.child(Label::new(error).color(Color::Error))
            })
            .when_some(
                repository
                    .as_ref()
                    .and_then(|repository| repository.checkout_error.clone()),
                |element, error| element.child(Label::new(error).color(Color::Error)),
            )
            .when_some(progress, |element, progress| {
                element.child(
                    Label::new(format!("{}: {}", progress.phase, progress.message))
                        .color(Color::Muted),
                )
            })
            .when(self.busy, |element| {
                element.child(
                    Label::new("Preparing or applying checkout operation…").color(Color::Muted),
                )
            })
            .when(
                repository
                    .as_ref()
                    .is_some_and(|repository| repository.checkout_loading),
                |element| {
                    element.child(Label::new("Refreshing checkout catalog…").color(Color::Muted))
                },
            )
            .child(
                v_flex()
                    .id("vcs-checkout-list")
                    .max_h(rems(24.))
                    .overflow_y_scroll()
                    .children(rows),
            )
    }
}
