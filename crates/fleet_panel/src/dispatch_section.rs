//! The Fleet panel's Dispatch section: Dispatch's saved conversations (calls
//! from the phone, typed questions) from the Yonder hub, newest first. Each
//! opens in the agent panel like a thread, with every step Dispatch took.

use std::time::Duration;

use agent_servers::HubStatus;
use agent_ui::{
    AgentPanel, AskDispatch,
    hub_sync::{DispatchConversation, HubSync},
};
use chrono::{DateTime, Local, TimeZone as _, Utc};
use gpui::{AnyElement, Context, Entity, Focusable as _, SharedString, Subscription, Window, px};
use ui::{ButtonStyle, Indicator, Tooltip, prelude::*};

use crate::{FleetPanel, short_age};

/// How many conversations show before "Show all".
const RECENT_CONVERSATIONS: usize = 3;

impl FleetPanel {
    /// Re-renders when the hub's conversations change, and every second
    /// while one is going so its timer counts up.
    pub(crate) fn observe_hub(cx: &mut Context<Self>) -> Option<Subscription> {
        let hub = HubSync::try_global(cx)?;
        Some(cx.observe(&hub, |this, hub, cx| {
            this.update_live_tick(&hub, cx);
            cx.notify();
        }))
    }

    fn update_live_tick(&mut self, hub: &Entity<HubSync>, cx: &mut Context<Self>) {
        let live = hub
            .read(cx)
            .conversations()
            .iter()
            .any(DispatchConversation::is_live);
        if !live {
            self.live_tick = None;
        } else if self.live_tick.is_none() {
            self.live_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                }
            }));
        }
    }

    fn open_dispatch_conversation(
        &mut self,
        conversation: &DispatchConversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let session_id = conversation.session_id();
        let title = conversation.title();
        workspace.update(cx, |workspace, cx| {
            let Some(panel) = workspace.focus_panel::<AgentPanel>(window, cx) else {
                return;
            };
            panel.update(cx, |panel, cx| {
                panel.open_dispatch_conversation(session_id, Some(title), window, cx)
            });
        });
        // Highlight the row now; the agent panel doesn't tell this panel.
        cx.notify();
    }

    fn toggle_all_conversations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_all_conversations = !self.show_all_conversations;
        if self.show_all_conversations {
            window.focus(&self.conversation_filter.focus_handle(cx), cx);
        } else {
            self.conversation_filter
                .update(cx, |editor, cx| editor.set_text("", window, cx));
        }
        cx.notify();
    }

    /// The section, or nothing when no hub is set up.
    pub(crate) fn render_dispatch_section(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let hub = HubSync::try_global(cx)?;
        let hub = hub.read(cx);
        let status = hub.status().clone();
        if status == HubStatus::Off {
            return None;
        }
        let conversations = hub.conversations().to_vec();
        let total = hub.conversation_total();
        let open = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
            .and_then(|panel| panel.read(cx).active_dispatch_conversation(cx))
            .map(|session_id| session_id.to_string());
        let border = cx.theme().colors().border_variant;

        let header = h_flex()
            .pl_3()
            .pr_1()
            .py_0p5()
            .gap_1()
            .child(
                Label::new("Dispatch")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(total > 0, |this| {
                this.child(
                    Label::new(format!("· {total}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(div().flex_1())
            .child(
                IconButton::new("fleet-ask-dispatch", IconName::Sparkle)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(|_, cx| Tooltip::for_action("Ask Dispatch", &AskDispatch, cx))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(AskDispatch), cx)),
            );

        let body = match &status {
            HubStatus::Failed(_) => v_flex()
                .px_3()
                .gap_1()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new("Can’t reach the hub.")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        ),
                )
                .child(
                    Label::new(
                        "Conversations are kept on the hub, so they come back when it does.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    h_flex().pt_1().child(
                        Button::new("fleet-dispatch-reconnect", "Reconnect")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(|_, _, cx| {
                                if let Some(hub) = HubSync::try_global(cx) {
                                    hub.update(cx, |hub, cx| hub.refresh(cx));
                                }
                            }),
                    ),
                )
                .into_any_element(),
            HubStatus::NoToken => div()
                .px_3()
                .child(
                    Label::new("Set the hub token in Settings → Machines.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            HubStatus::Connecting if conversations.is_empty() => div()
                .px_3()
                .child(
                    Label::new("Connecting to the hub…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            _ if conversations.is_empty() => v_flex()
                .px_3()
                .gap_1p5()
                .child(
                    Label::new(
                        "Calls from your phone and questions you type show up here, \
                         with every thread Dispatch read.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    h_flex().child(
                        Button::new("fleet-dispatch-ask", "Ask Dispatch")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .start_icon(
                                Icon::new(IconName::Sparkle)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(AskDispatch), cx)
                            }),
                    ),
                )
                .into_any_element(),
            _ if self.show_all_conversations => {
                self.render_all_conversations(conversations, total, open, window, cx)
            }
            _ => {
                let now = Utc::now();
                let rows: Vec<AnyElement> = conversations
                    .iter()
                    .take(RECENT_CONVERSATIONS)
                    .map(|conversation| {
                        let selected = open.as_deref() == Some(conversation.id.as_str());
                        self.render_conversation(conversation, selected, false, now, cx)
                    })
                    .collect();
                v_flex()
                    .px_1()
                    .children(rows)
                    .when(total > RECENT_CONVERSATIONS, |this| {
                        this.child(self.render_show_more(format!("Show all {total}"), cx))
                    })
                    .into_any_element()
            }
        };

        Some(
            v_flex()
                .pb_2()
                .mb_1()
                .gap_0p5()
                .border_b_1()
                .border_color(border)
                .child(header)
                .child(body)
                .into_any_element(),
        )
    }

    fn render_all_conversations(
        &self,
        conversations: Vec<DispatchConversation>,
        total: usize,
        open: Option<String>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let query = self.conversation_filter.read(cx).text(cx).to_lowercase();
        let query = query.trim();
        let now = Utc::now();
        let today = Local::now().date_naive();
        let mut groups: Vec<(&'static str, Vec<AnyElement>)> = Vec::new();
        for conversation in conversations.iter().filter(|conversation| {
            query.is_empty()
                || conversation.title().to_lowercase().contains(query)
                || conversation.summary.to_lowercase().contains(query)
        }) {
            let day = Local
                .timestamp_opt(conversation.created_at, 0)
                .single()
                .map(|created| created.date_naive());
            let group = match day.map(|day| (today - day).num_days()) {
                Some(0) => "Today",
                Some(1) => "Yesterday",
                _ => "Earlier",
            };
            let selected = open.as_deref() == Some(conversation.id.as_str());
            let row = self.render_conversation(conversation, selected, true, now, cx);
            match groups.last_mut() {
                Some((name, rows)) if *name == group => rows.push(row),
                _ => groups.push((group, vec![row])),
            }
        }
        let colors = cx.theme().colors();
        let filter_border = if self.conversation_filter.focus_handle(cx).is_focused(window) {
            colors.border_focused
        } else {
            colors.border_variant
        };
        let filter_background = colors.editor_background;
        v_flex()
            .px_1()
            .gap_0p5()
            .child(
                div()
                    .mx_1()
                    .mb_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(filter_border)
                    .bg(filter_background)
                    .child(self.conversation_filter.clone()),
            )
            .when(groups.is_empty(), |this| {
                this.child(
                    div().px_2().child(
                        Label::new("No conversations match.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .children(groups.into_iter().map(|(name, rows)| {
                v_flex()
                    .child(
                        div().px_2().pt_1().child(
                            Label::new(name)
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                        ),
                    )
                    .children(rows)
            }))
            .when(total > conversations.len(), |this| {
                this.child(
                    div().px_2().child(
                        Label::new(format!("The newest {} are listed.", conversations.len()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(self.render_show_more("Show fewer".to_owned(), cx))
            .into_any_element()
    }

    fn render_show_more(&self, label: String, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .px_1()
            .child(
                Button::new("fleet-dispatch-show-all", label)
                    .style(ButtonStyle::Transparent)
                    .label_size(LabelSize::Small)
                    .color(Color::Accent)
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.toggle_all_conversations(window, cx)
                        }),
                    ),
            )
            .into_any_element()
    }

    /// One conversation. `compact` rows (in the full list) leave out the line
    /// saying what it touched.
    fn render_conversation(
        &self,
        conversation: &DispatchConversation,
        selected: bool,
        compact: bool,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let (selected_background, hover_background) =
            (colors.element_selected, colors.element_hover);
        let live = conversation.is_live();
        let title = conversation.title();
        let icon = if live {
            div()
                .w(px(14.))
                .h(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .child(Indicator::dot().color(Color::Warning))
                .into_any_element()
        } else {
            Icon::new(if conversation.is_voice() {
                IconName::Mic
            } else {
                IconName::Chat
            })
            .size(IconSize::Small)
            .color(if selected {
                Color::Accent
            } else {
                Color::Muted
            })
            .into_any_element()
        };
        let (age, age_color) = if live {
            (elapsed(conversation.created_at, now), Color::Warning)
        } else {
            let updated_at = Utc
                .timestamp_opt(conversation.updated_at, 0)
                .single()
                .unwrap_or(now);
            (short_age(updated_at, now), Color::Muted)
        };
        let kind = match (live, conversation.is_voice()) {
            (true, true) => "On a call",
            (true, false) => "Answering",
            (false, true) => "Voice call",
            (false, false) => "Typed",
        };
        let detail: SharedString = if conversation.summary.is_empty() {
            kind.into()
        } else {
            format!("{kind} · {}", conversation.summary).into()
        };
        let row_id = SharedString::from(format!("fleet-dispatch-{}", conversation.id));
        let conversation = conversation.clone();
        v_flex()
            .id(row_id)
            .w_full()
            .px_2()
            .py_1()
            .gap_0p5()
            .rounded_md()
            .cursor_pointer()
            .map(|this| {
                if selected {
                    this.bg(selected_background)
                } else {
                    this.hover(move |style| style.bg(hover_background))
                }
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(icon)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(title.clone()).truncate()),
                    )
                    .child(Label::new(age).size(LabelSize::XSmall).color(age_color)),
            )
            .when(!compact, |this| {
                this.child(
                    // Lines up with the title, past the 14px icon and its gap.
                    div().pl(px(22.)).min_w_0().child(
                        Label::new(detail.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
            })
            .tooltip(Tooltip::text(format!("{title} · {detail}")))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_dispatch_conversation(&conversation, window, cx)
            }))
            .into_any_element()
    }
}

/// How long a live conversation has been going, e.g. "2:14".
fn elapsed(created_at: i64, now: DateTime<Utc>) -> String {
    let seconds = (now.timestamp() - created_at).max(0);
    if seconds >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3_600,
            seconds % 3_600 / 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_conversations_count_up_in_minutes_and_seconds() {
        let now = Utc.timestamp_opt(10_000, 0).unwrap();
        assert_eq!(elapsed(10_000 - 134, now), "2:14");
        assert_eq!(elapsed(10_000 - 3_725, now), "1:02:05");
        assert_eq!(elapsed(10_100, now), "0:00");
    }
}
