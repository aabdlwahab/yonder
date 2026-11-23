use agent_servers::HubStatus;
use editor::{Editor, EditorEvent};
use gpui::{
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task, WeakEntity,
    actions,
};
use ui::{Button, ButtonStyle, IconButton, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::hub_sync::{DispatchReply, HubSync};

actions!(
    agent,
    [
        /// Asks Dispatch about agent threads on the Yonder hub.
        AskDispatch
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &AskDispatch, window, cx| {
            let Some(hub) = HubSync::try_global(cx) else {
                return;
            };
            let workspace_handle = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                AskDispatchModal::new(hub, workspace_handle, window, cx)
            });
        });
    })
    .detach();
}

pub(crate) fn toolbar_button() -> IconButton {
    IconButton::new("ask-dispatch", IconName::Sparkle)
        .icon_size(IconSize::Small)
        .tooltip(|_, cx| Tooltip::for_action("Ask Dispatch", &AskDispatch, cx))
        .on_click(|_, window, cx| window.dispatch_action(Box::new(AskDispatch), cx))
}

struct AskDispatchModal {
    hub: Entity<HubSync>,
    workspace: WeakEntity<Workspace>,
    composer: Entity<Editor>,
    response: Option<(String, DispatchReply)>,
    error: Option<SharedString>,
    pending: Option<(&'static str, Task<()>)>,
    _subscriptions: [Subscription; 2],
}

impl AskDispatchModal {
    fn new(
        hub: Entity<HubSync>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("What's running?", window, cx);
            editor
        });
        let subscriptions = [
            cx.observe(&hub, |_, _, cx| cx.notify()),
            cx.subscribe(&composer, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
        ];
        Self {
            hub,
            workspace,
            composer,
            response: None,
            error: None,
            pending: None,
            _subscriptions: subscriptions,
        }
    }

    fn ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hub = self.hub.read(cx);
        let draft = self.composer.read(cx).text(cx);
        let prompt = draft.trim();
        if self.pending.is_some()
            || prompt.is_empty()
            || !matches!(hub.status(), HubStatus::Connected { .. })
        {
            return;
        }
        let Some(address) = hub.address().map(str::to_owned) else {
            return;
        };
        let request = hub.ask_dispatch(prompt.to_owned(), cx);
        self.error = None;
        self.pending = Some((
            "Waiting for Dispatch…",
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                this.update_in(cx, |this, window, cx| {
                    this.pending = None;
                    if this.hub.read(cx).address() != Some(&address) {
                        this.error = Some("The Yonder hub changed. Ask Dispatch again.".into());
                    } else {
                        match result {
                            Ok(reply) => {
                                this.response = Some((address, reply));
                                // The question is saved as a conversation;
                                // show it in the Fleet panel now.
                                this.hub.update(cx, |hub, cx| hub.reload_conversations(cx));
                                if this.composer.read(cx).text(cx) == draft {
                                    this.composer
                                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                                }
                            }
                            Err(error) => this.error = Some(format!("{error:#}").into()),
                        }
                    }
                    cx.notify();
                })
                .log_err();
            }),
        ));
        cx.notify();
    }

    fn open_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((address, response)) = self.response.as_ref() else {
            return;
        };
        let hub = self.hub.read(cx);
        let Some(session_id) = response.open_session_id.clone() else {
            return;
        };
        if self.pending.is_some() || hub.address() != Some(address) {
            return;
        }
        let request = hub.sync_session(session_id, cx);
        let address = address.clone();
        self.error = None;
        self.pending = Some((
            "Opening thread…",
            cx.spawn_in(window, async move |this, cx| {
                let result = request.await;
                this.update_in(cx, |this, window, cx| {
                    this.pending = None;
                    if this.hub.read(cx).address() != Some(&address) {
                        this.error = Some("The Yonder hub changed. Ask Dispatch again.".into());
                    } else {
                        match result {
                            Ok(metadata) => {
                                let workspace = this.workspace.clone();
                                cx.emit(DismissEvent);
                                window.defer(cx, move |window, cx| {
                                    crate::forward::open_thread(
                                        metadata,
                                        workspace.upgrade(),
                                        window,
                                        cx,
                                    );
                                });
                            }
                            Err(error) => this.error = Some(format!("{error:#}").into()),
                        }
                    }
                    cx.notify();
                })
                .log_err();
            }),
        ));
        cx.notify();
    }
}

impl EventEmitter<DismissEvent> for AskDispatchModal {}
impl ModalView for AskDispatchModal {}

impl Focusable for AskDispatchModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.focus_handle(cx)
    }
}

impl Render for AskDispatchModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hub = self.hub.read(cx);
        let (status, status_color) = match hub.status() {
            HubStatus::Off => (
                "Enable the Yonder hub in Settings → Machines.",
                Color::Muted,
            ),
            HubStatus::Connecting => ("Connecting to the Yonder hub…", Color::Muted),
            HubStatus::NoToken => ("Set the hub token in Settings → Machines.", Color::Warning),
            HubStatus::Connected { .. } => ("Connected to the Yonder hub", Color::Success),
            HubStatus::Failed(error) => (error.as_ref(), Color::Error),
        };
        let can_ask = matches!(hub.status(), HubStatus::Connected { .. })
            && self.pending.is_none()
            && !self.composer.read(cx).text(cx).trim().is_empty();
        let can_retry_connection = matches!(hub.status(), HubStatus::Failed(_));
        let reply = self
            .response
            .as_ref()
            .filter(|(address, _)| hub.address() == Some(address));
        let colors = cx.theme().colors();

        v_flex()
            .id("ask-dispatch-modal")
            .w(rems(34.)
                .to_pixels(window.rem_size())
                .min(window.viewport_size().width - px(32.)))
            .elevation_3(cx)
            .p_4()
            .gap_3()
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.ask(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                h_flex()
                    .justify_between()
                    .child(Headline::new("Ask Dispatch"))
                    .child(
                        IconButton::new("dismiss-ask-dispatch", IconName::Close)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(status.to_owned())
                            .size(LabelSize::Small)
                            .color(status_color),
                    )
                    .when(can_retry_connection, |this| {
                        this.child(
                            Button::new("reconnect-hub", "Reconnect").on_click(cx.listener(
                                |this, _, _, cx| this.hub.update(cx, |hub, cx| hub.refresh(cx)),
                            )),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(if self.focus_handle(cx).is_focused(window) {
                                colors.border_focused
                            } else {
                                colors.border_variant
                            })
                            .bg(colors.editor_background)
                            .child(self.composer.clone()),
                    )
                    .child(
                        Button::new("send-to-dispatch", "Ask")
                            .style(ButtonStyle::Filled)
                            .disabled(!can_ask)
                            .on_click(cx.listener(|this, _, window, cx| this.ask(window, cx))),
                    ),
            )
            .when_some(self.pending.as_ref(), |this, (status, _)| {
                this.child(Label::new(*status).color(Color::Muted))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error))
            })
            .when_some(reply, |this, (_, reply)| {
                this.child(
                    div()
                        .id("dispatch-reply")
                        .max_h(rems(16.))
                        .overflow_y_scroll()
                        .child(Label::new(reply.text.clone())),
                )
                .when(reply.open_session_id.is_some(), |this| {
                    this.child(
                        h_flex().justify_end().child(
                            Button::new("open-dispatch-thread", "Open Thread")
                                .disabled(self.pending.is_some())
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.open_thread(window, cx)),
                                ),
                        ),
                    )
                })
            })
    }
}
