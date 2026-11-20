use std::sync::Arc;

use gpui::{
    Action, Anchor, App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Global,
    SharedString, Subscription, Task, TaskExt, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{
    ContextMenu, ContextMenuEntry, ListItem, ListItemSpacing, PopoverMenu, prelude::*,
    utils::WithRemSize,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::{
    Agent, AgentPanel, AgentThreadSource, ConversationView,
    thread_metadata_store::{ThreadMetadata, ThreadMetadataStore},
};

struct ConversationTabs;
impl Global for ConversationTabs {}

pub fn init(cx: &mut App) {
    cx.set_global(ConversationTabs);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(open_recent_chats);
    })
    .detach();
}

pub(crate) fn enabled(cx: &App) -> bool {
    cx.has_global::<ConversationTabs>()
}

pub fn close_all(workspace: &Workspace, window: &mut Window, cx: &mut App) {
    for pane in workspace.panes() {
        let tabs = pane
            .read(cx)
            .items_of_type::<ConversationTab>()
            .map(|tab| tab.entity_id())
            .collect::<Vec<_>>();
        if !tabs.is_empty() {
            pane.update(cx, |pane, cx| {
                pane.close_items(window, cx, workspace::SaveIntent::Close, &|item_id| {
                    tabs.contains(&item_id)
                })
                .detach_and_log_err(cx);
            });
        }
    }
}

pub(crate) struct ConversationTab {
    conversation: Entity<ConversationView>,
    panel: WeakEntity<AgentPanel>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ConversationTab {
    pub(crate) fn thread_id(&self, cx: &App) -> crate::ThreadId {
        self.conversation.read(cx).thread_id
    }

    pub(crate) fn open(
        conversation: Entity<ConversationView>,
        workspace: WeakEntity<Workspace>,
        panel: WeakEntity<AgentPanel>,
        window: &Window,
        cx: &mut App,
    ) {
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            let Some(agent_panel) = panel.upgrade() else {
                return;
            };
            if !agent_settings::AgentSettings::get_global(cx).enabled(cx) {
                return;
            }
            let close_dock = AgentPanel::is_visible(&workspace, cx)
                && agent_panel.read(cx).active_conversation_view() == Some(&conversation);
            workspace.update(cx, |workspace, cx| {
                if close_dock {
                    workspace.close_panel::<AgentPanel>(window, cx);
                }
                if let Some(tab) = conversation.read(cx).editor_tab()
                    && workspace.activate_item(&tab, true, true, window, cx)
                {
                    return;
                }
                let tab = cx.new(|cx| Self::new(conversation.clone(), panel, window, cx));
                conversation.update(cx, |conversation, cx| {
                    conversation.set_editor_tab(tab.downgrade());
                    cx.notify();
                });
                workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
            });
        });
    }

    fn new(
        conversation: Entity<ConversationView>,
        panel: WeakEntity<AgentPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let panel_to_activate = panel.clone();
        let mut subscriptions = vec![
            cx.observe(&conversation, |_, _, cx| cx.emit(ItemEvent::UpdateTab)),
            cx.on_focus_in(&focus_handle, window, move |tab, window, cx| {
                let conversation = tab.conversation.clone();
                let panel = panel_to_activate.clone();
                window.defer(cx, move |window, cx| {
                    panel
                        .update(cx, |panel, cx| {
                            panel.activate_conversation_tab(conversation, window, cx)
                        })
                        .log_err();
                });
            }),
        ];
        if let Some(store) = ThreadMetadataStore::try_global(cx) {
            subscriptions.push(cx.observe(&store, |_, _, cx| cx.emit(ItemEvent::UpdateTab)));
        }
        Self {
            conversation,
            panel,
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    fn new_chat_menu(&self) -> impl IntoElement {
        let panel = self.panel.clone();
        PopoverMenu::new("editor-new-agent-chat")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("editor-new-agent-chat-button", "New chat")
                    .label_size(LabelSize::Small),
            )
            .menu(move |window, cx| {
                let panel = panel.upgrade()?;
                let agents = panel.read(cx).chat_agents(cx);
                let panel = panel.downgrade();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    agents.into_iter().fold(menu, |menu, (agent, label)| {
                        let panel = panel.clone();
                        menu.item(ContextMenuEntry::new(label).handler(move |window, cx| {
                            panel
                                .update(cx, |panel, cx| {
                                    panel.new_chat_for_agent(agent.clone(), window, cx);
                                })
                                .log_err();
                        }))
                    })
                }))
            })
    }
}

impl EventEmitter<ItemEvent> for ConversationTab {}

impl Item for ConversationTab {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        let conversation = self.conversation.read(cx);
        ThreadMetadataStore::try_global(cx)
            .and_then(|store| {
                store
                    .read(cx)
                    .entry(conversation.thread_id)
                    .and_then(|entry| entry.title())
            })
            .unwrap_or_else(|| conversation.title(cx))
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Sparkle).color(Color::Accent))
    }

    fn include_in_nav_history() -> bool {
        false
    }

    fn to_item_events(event: &ItemEvent, emit: &mut dyn FnMut(ItemEvent)) {
        if matches!(event, ItemEvent::UpdateTab) {
            emit(ItemEvent::UpdateTab);
        }
    }
}

impl Focusable for ConversationTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation.read(cx).activation_focus_handle(cx)
    }
}

impl Render for ConversationTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let agents_enabled = agent_settings::AgentSettings::get_global(cx).enabled(cx);
        let conversation = self.conversation.read(cx);
        let thread_title = conversation
            .active_thread()
            .map(|thread| thread.read(cx).title_editor.clone());
        let agent_icon = conversation
            .agent_key()
            .icon()
            .unwrap_or(IconName::ZedAgent);
        let agent_name = conversation.editor_agent_name(cx);
        let font_settings = ThemeSettings::get_global(cx);

        WithRemSize::new(font_settings.agent_ui_font_size(cx))
            .size_full()
            .font_family(font_settings.agent_ui_font_family().clone())
            .child(
                v_flex()
                    .size_full()
                    .track_focus(&self.focus_handle)
                    .bg(cx.theme().colors().editor_background)
                    .when(agents_enabled, |this| {
                        this.child(
                            h_flex()
                                .flex_none()
                                .px_4()
                                .py_2()
                                .gap_2()
                                .border_b_1()
                                .border_color(cx.theme().colors().border_variant)
                                .child(
                                    Icon::new(agent_icon)
                                        .size(IconSize::Small)
                                        .color(Color::Accent),
                                )
                                .child(
                                    Label::new(agent_name)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(div().flex_1().min_w_0().map(|this| match thread_title {
                                    Some(title) => this.child(title),
                                    None => this.child(Label::new(self.tab_content_text(0, cx))),
                                }))
                                .child(self.new_chat_menu())
                                .child(
                                    Button::new("split-chat-editor", "Split right")
                                        .label_size(LabelSize::Small)
                                        .on_click(|_, window, cx| {
                                            window.dispatch_action(
                                                workspace::pane::SplitRight {
                                                    mode: workspace::pane::SplitMode::EmptyPane,
                                                }
                                                .boxed_clone(),
                                                cx,
                                            );
                                        }),
                                ),
                        )
                        .child(div().min_h_0().flex_1().child(self.conversation.clone()))
                    })
                    .when(!agents_enabled, |this| {
                        this.items_center().justify_center().child(
                            Label::new("Agents are disabled in settings.").color(Color::Muted),
                        )
                    }),
            )
    }
}

struct RecentChats {
    workspace: WeakEntity<Workspace>,
    threads: Vec<ThreadMetadata>,
    matches: Vec<usize>,
    selected: usize,
}

fn open_recent_chats(
    workspace: &mut Workspace,
    _: &zed_actions::agent::RecentChats,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.panel::<AgentPanel>(cx).is_none() {
        return;
    }
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return;
    };
    let project = workspace.project().read(cx);
    let paths = project.default_path_list(cx);
    let connection = project.remote_connection_options(cx);
    let mut threads = store
        .read(cx)
        .entries_for_path(&paths, connection)
        .filter(|entry| !entry.is_draft() || entry.has_saved_draft_prompt(cx))
        .cloned()
        .collect::<Vec<_>>();
    threads.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
    let matches = (0..threads.len()).collect();
    let delegate = RecentChats {
        workspace: cx.weak_entity(),
        threads,
        matches,
        selected: 0,
    };
    workspace.toggle_modal(window, cx, |window, cx| {
        Picker::uniform_list(delegate, window, cx)
    });
}

impl PickerDelegate for RecentChats {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "recent-chats"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = index;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Find a recent chat in this project…".into()
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some("No matching recent chats in this project.".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, thread)| {
                thread.display_title().to_lowercase().contains(&query)
                    || thread.agent_id.0.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(thread) = self
            .matches
            .get(self.selected)
            .and_then(|index| self.threads.get(*index))
            .cloned()
        else {
            return;
        };
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| {
            let Some(panel) = workspace
                .read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))
                .ok()
                .flatten()
            else {
                return;
            };
            let title = thread.title();
            let paths = thread.folder_paths().clone();
            panel.update(cx, |panel, cx| {
                panel.load_agent_thread(
                    Agent::from(thread.agent_id),
                    thread.thread_id,
                    Some(paths),
                    title,
                    true,
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                );
            });
        });
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<ListItem> {
        let thread = self.threads.get(*self.matches.get(index)?)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::Sparkle).color(Color::Accent))
                .child(Label::new(thread.display_title()).single_line())
                .end_slot(
                    Label::new(Agent::from(thread.agent_id.clone()).label())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
    }
}
