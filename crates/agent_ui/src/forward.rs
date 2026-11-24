//! Forwarding agent replies to other threads.
//!
//! A forward lands in the target thread's message box as a mention that
//! carries the forwarded text, so the user can add a note before sending. The
//! same picker also forks the conversation or hands the whole thread to another
//! agent, which used to be separate lists in the message menu.

use std::{rc::Rc, sync::Arc};

use acp_thread::MentionUri;
use agent_client_protocol::v2::schema as acp_v2;
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, Global, SharedString, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use project::AgentId;
use remote::RemoteConnectionOptions;
use serde::{Deserialize, Serialize};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{
    Agent, AgentInitialContent, AgentPanel, AgentThreadSource,
    agent_panel::CreateThreadOptions,
    conversation_view::ThreadView,
    thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore},
};

const FORWARD_LINKS_KEY: &str = "agent_forward_links";
const LOCAL_MACHINE_LABEL: &str = if cfg!(target_os = "macos") {
    "This Mac"
} else {
    "This Computer"
};

/// Opens a thread wherever it lives: its own project, machine and window.
/// The threads sidebar knows how to do that, and it depends on this crate, so
/// it registers itself here.
pub struct ThreadOpener(pub Rc<dyn Fn(ThreadMetadata, &mut Window, &mut App)>);

impl Global for ThreadOpener {}

pub fn set_thread_opener(
    opener: impl Fn(ThreadMetadata, &mut Window, &mut App) + 'static,
    cx: &mut App,
) {
    cx.set_global(ThreadOpener(Rc::new(opener)));
}

/// Opens a thread, falling back to this window's agent panel when no opener
/// is registered.
pub(crate) fn open_thread(
    metadata: ThreadMetadata,
    workspace: Option<Entity<Workspace>>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(opener) = cx
        .try_global::<ThreadOpener>()
        .map(|opener| opener.0.clone())
    {
        opener(metadata, window, cx);
        return;
    }
    let Some(panel) = workspace.and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
    else {
        return;
    };
    panel.update(cx, |panel, cx| {
        panel.load_agent_thread(
            Agent::from(metadata.agent_id.clone()),
            metadata.thread_id,
            None,
            metadata.title(),
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        )
    });
}

pub(crate) fn open_thread_by_session(
    session_id: &acp_v2::SessionId,
    workspace: Option<Entity<Workspace>>,
    window: &mut Window,
    cx: &mut App,
) {
    let metadata = ThreadMetadataStore::try_global(cx)
        .and_then(|store| store.read(cx).entry_by_session(session_id).cloned());
    if let Some(metadata) = metadata {
        open_thread(metadata, workspace, window, cx);
    }
}

/// Forwards waiting for their thread's view to open. Each view takes its own
/// when it appears, or right away if it's already open.
#[derive(Default)]
pub(crate) struct PendingForwards(HashMap<ThreadId, Vec<PendingForward>>);

impl Global for PendingForwards {}

pub(crate) struct PendingForward {
    pub blocks: Vec<acp_v2::ContentBlock>,
    pub send: bool,
}

pub(crate) fn push_pending_forward(thread_id: ThreadId, forward: PendingForward, cx: &mut App) {
    if !cx.has_global::<PendingForwards>() {
        cx.set_global(PendingForwards::default());
    }
    cx.update_global::<PendingForwards, _>(|pending, _| {
        pending.0.entry(thread_id).or_default().push(forward);
    });
}

pub(crate) fn take_pending_forwards(thread_id: ThreadId, cx: &mut App) -> Vec<PendingForward> {
    if !cx.has_global::<PendingForwards>() {
        return Vec::new();
    }
    if !cx.global::<PendingForwards>().0.contains_key(&thread_id) {
        return Vec::new();
    }
    cx.global_mut::<PendingForwards>()
        .0
        .remove(&thread_id)
        .unwrap_or_default()
}

/// Where a reply was forwarded, shown under it in the source thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ForwardLink {
    pub entry_ix: usize,
    pub thread_id: ThreadId,
    pub title: String,
    pub agent: String,
}

#[derive(Default)]
pub(crate) struct ForwardLinks {
    loaded: bool,
    by_session: HashMap<String, Vec<ForwardLink>>,
}

impl Global for ForwardLinks {}

pub(crate) fn forward_links(
    session_id: &acp_v2::SessionId,
    entry_ix: usize,
    cx: &App,
) -> Vec<ForwardLink> {
    let Some(links) = cx.try_global::<ForwardLinks>() else {
        return Vec::new();
    };
    links
        .by_session
        .get(&session_id.to_string())
        .map(|links| {
            links
                .iter()
                .filter(|link| link.entry_ix == entry_ix)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn ensure_links_loaded(cx: &mut App) {
    if cx
        .try_global::<ForwardLinks>()
        .is_some_and(|links| links.loaded)
    {
        return;
    }
    let by_session = KeyValueStore::global(cx)
        .read_kvp(FORWARD_LINKS_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default();
    cx.set_global(ForwardLinks {
        loaded: true,
        by_session,
    });
}

fn record_forward_link(session_id: &acp_v2::SessionId, link: ForwardLink, cx: &mut App) {
    ensure_links_loaded(cx);
    let json = cx.update_global::<ForwardLinks, _>(|links, _| {
        let entries = links.by_session.entry(session_id.to_string()).or_default();
        if !entries.contains(&link) {
            entries.push(link);
        }
        serde_json::to_string(&links.by_session).log_err()
    });
    let Some(json) = json else {
        return;
    };
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.write_kvp(FORWARD_LINKS_KEY.to_string(), json)
            .await
            .log_err();
    })
    .detach();
}

/// The reply being forwarded and the thread it comes from.
#[derive(Clone)]
pub(crate) struct ForwardSource {
    pub thread_view: WeakEntity<ThreadView>,
    pub workspace: WeakEntity<Workspace>,
    pub thread_id: ThreadId,
    pub session_id: acp_v2::SessionId,
    pub title: SharedString,
    pub agent_id: AgentId,
    pub agent_name: SharedString,
    pub entry_ix: usize,
    pub text: String,
    pub is_selection: bool,
    /// Agents this project can start, with their display names.
    pub agents: Vec<(AgentId, SharedString)>,
    /// Other Claude accounts, which resume the exact session instead of
    /// receiving a transcript when the whole thread is handed off.
    pub other_claude_accounts: HashSet<AgentId>,
}

impl ForwardSource {
    fn agent_name(&self, agent_id: &AgentId) -> SharedString {
        self.agents
            .iter()
            .find(|(id, _)| id == agent_id)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| agent_id.0.clone())
    }
}

/// The forwarded text as a mention, followed by a line for the user's note.
pub(crate) fn forward_blocks(source: &ForwardSource) -> Vec<acp_v2::ContentBlock> {
    let uri = MentionUri::Forward {
        session_id: source.session_id.clone(),
        title: source.title.to_string(),
        agent: source.agent_name.to_string(),
    };
    let what = if source.is_selection {
        "Part of a reply"
    } else {
        "A reply"
    };
    let text = format!(
        "{what} forwarded from {agent} in the thread \u{201c}{title}\u{201d}:\n\n{body}",
        agent = source.agent_name,
        title = source.title,
        body = source.text,
    );
    vec![
        acp_v2::ContentBlock::Resource(acp_v2::EmbeddedResource::new(
            acp_v2::EmbeddedResourceResource::TextResourceContents(
                acp_v2::TextResourceContents::new(text, uri.to_uri().to_string()),
            ),
        )),
        acp_v2::ContentBlock::Text(acp_v2::TextContent::new(" ")),
    ]
}

pub(crate) fn open_forward_picker(
    source: ForwardSource,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let threads = forward_threads(&source, cx);
    workspace.toggle_modal(window, cx, |window, cx| {
        Picker::uniform_list(ForwardPickerDelegate::new(source, threads), window, cx)
    });
}

#[derive(Clone)]
struct ForwardThread {
    metadata: ThreadMetadata,
    location: SharedString,
}

fn forward_threads(source: &ForwardSource, cx: &App) -> Vec<ForwardThread> {
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return Vec::new();
    };
    let mut threads = store
        .read(cx)
        .entries()
        .filter(|thread| {
            !thread.archived && !thread.is_draft() && thread.thread_id != source.thread_id
        })
        .map(|thread| ForwardThread {
            metadata: thread.clone(),
            location: thread_location(thread),
        })
        .collect::<Vec<_>>();
    threads.sort_by_key(|thread| std::cmp::Reverse(thread.metadata.updated_at));
    threads
}

fn thread_location(thread: &ThreadMetadata) -> SharedString {
    let machine: SharedString = match &thread.remote_connection {
        Some(RemoteConnectionOptions::Ssh(options)) => options
            .nickname
            .clone()
            .unwrap_or_else(|| options.host.to_string())
            .into(),
        Some(options) => options.display_name().into(),
        None => LOCAL_MACHINE_LABEL.into(),
    };
    match thread
        .folder_paths()
        .ordered_paths()
        .next()
        .and_then(|path| path.file_name())
    {
        Some(project) => format!("{} · {machine}", project.to_string_lossy()).into(),
        None => machine,
    }
}

#[derive(Clone)]
enum ForwardTarget {
    NewThread(AgentId),
    Thread(ForwardThread),
    Fork(AgentId),
    HandOff(AgentId),
}

impl ForwardTarget {
    fn group(&self) -> usize {
        match self {
            ForwardTarget::NewThread(_) => 0,
            ForwardTarget::HandOff(_) => 1,
            ForwardTarget::Fork(_) => 2,
            ForwardTarget::Thread(_) => 3,
        }
    }
}

struct ForwardPickerDelegate {
    source: ForwardSource,
    targets: Vec<ForwardTarget>,
    matches: Vec<usize>,
    selected_index: usize,
}

impl ForwardPickerDelegate {
    fn new(source: ForwardSource, threads: Vec<ForwardThread>) -> Self {
        let agents = source
            .agents
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut targets = Vec::new();
        // The short lists come first; recent threads can run long, so they go
        // last and the hand-off and fork entries never scroll out of sight.
        targets.extend(agents.iter().cloned().map(ForwardTarget::NewThread));
        targets.extend(
            agents
                .iter()
                .filter(|agent| **agent != source.agent_id)
                .cloned()
                .map(ForwardTarget::HandOff),
        );
        targets.extend(agents.iter().cloned().map(ForwardTarget::Fork));
        targets.extend(threads.into_iter().map(ForwardTarget::Thread));
        let matches = (0..targets.len()).collect();
        Self {
            source,
            targets,
            matches,
            selected_index: 0,
        }
    }

    fn label(&self, target: &ForwardTarget) -> (SharedString, Option<SharedString>) {
        match target {
            ForwardTarget::NewThread(agent) => (
                format!("New thread with {}", self.source.agent_name(agent)).into(),
                None,
            ),
            ForwardTarget::Thread(thread) => (
                thread.metadata.display_title(),
                Some(
                    format!(
                        "{} · {}",
                        self.source.agent_name(&thread.metadata.agent_id),
                        thread.location
                    )
                    .into(),
                ),
            ),
            ForwardTarget::Fork(agent) => (
                format!("Fork from here to {}", self.source.agent_name(agent)).into(),
                None,
            ),
            ForwardTarget::HandOff(agent) => (
                format!("Hand off whole thread to {}", self.source.agent_name(agent)).into(),
                self.source
                    .other_claude_accounts
                    .contains(agent)
                    .then(|| "resumes the same session".into()),
            ),
        }
    }

    fn selected_target(&self) -> Option<&ForwardTarget> {
        self.targets.get(*self.matches.get(self.selected_index)?)
    }

    fn forward_to_thread(
        &self,
        thread: &ForwardThread,
        send: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let thread_id = thread.metadata.thread_id;
        let blocks = forward_blocks(&self.source);
        push_pending_forward(thread_id, PendingForward { blocks, send }, cx);
        record_forward_link(
            &self.source.session_id,
            ForwardLink {
                entry_ix: self.source.entry_ix,
                thread_id,
                title: thread.metadata.display_title().to_string(),
                agent: self
                    .source
                    .agent_name(&thread.metadata.agent_id)
                    .to_string(),
            },
            cx,
        );
        let metadata = thread.metadata.clone();
        let workspace = self.source.workspace.upgrade();
        window.defer(cx, move |window, cx| {
            open_thread(metadata, workspace, window, cx)
        });
    }

    fn forward_to_new_thread(
        &self,
        agent: &AgentId,
        send: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(workspace) = self.source.workspace.upgrade() else {
            return;
        };
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        let blocks = forward_blocks(&self.source);
        let thread_id = panel.update(cx, |panel, cx| {
            let thread_id = panel.create_thread_with_options(
                CreateThreadOptions {
                    agent: Some(Agent::from(agent.clone())),
                    initial_content: Some(AgentInitialContent::ContentBlock {
                        blocks,
                        auto_submit: send,
                    }),
                    ..Default::default()
                },
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
            panel.activate_retained_thread(thread_id, true, window, cx);
            thread_id
        });
        workspace.update(cx, |workspace, cx| {
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
        record_forward_link(
            &self.source.session_id,
            ForwardLink {
                entry_ix: self.source.entry_ix,
                thread_id,
                title: "New thread".to_string(),
                agent: self.source.agent_name(agent).to_string(),
            },
            cx,
        );
    }
}

impl PickerDelegate for ForwardPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "forward-picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
        cx.notify();
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        let groups = self
            .matches
            .iter()
            .filter_map(|&index| self.targets.get(index).map(ForwardTarget::group))
            .collect::<Vec<_>>();
        groups
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| pair[0] != pair[1])
            .map(|(index, _)| index)
            .collect()
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        if self.source.is_selection {
            format!(
                "Forward the selected text from {} to…",
                self.source.agent_name
            )
            .into()
        } else {
            format!("Forward {}'s reply to…", self.source.agent_name).into()
        }
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No matching threads or agents.".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.matches = self
            .targets
            .iter()
            .enumerate()
            .filter(|(_, target)| {
                let (title, detail) = self.label(target);
                let text = match detail {
                    Some(detail) => format!("{title} {detail}"),
                    None => title.to_string(),
                };
                matches_in_order(&query, &text)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(target) = self.selected_target().cloned() else {
            return;
        };
        cx.emit(DismissEvent);
        match target {
            ForwardTarget::NewThread(agent) => {
                self.forward_to_new_thread(&agent, secondary, window, cx)
            }
            ForwardTarget::Thread(thread) => self.forward_to_thread(&thread, secondary, window, cx),
            ForwardTarget::Fork(agent) => {
                let entry_ix = self.source.entry_ix;
                self.source
                    .thread_view
                    .update(cx, |view, cx| {
                        view.fork_conversation(agent, Some(entry_ix), secondary, window, cx)
                    })
                    .log_err();
            }
            ForwardTarget::HandOff(agent) => {
                let exact = self.source.other_claude_accounts.contains(&agent);
                self.source
                    .thread_view
                    .update(cx, |view, cx| {
                        if exact {
                            view.resume_claude_account(agent, window, cx)
                        } else {
                            view.fork_conversation(agent, None, false, window, cx)
                        }
                    })
                    .log_err();
            }
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let target = self.targets.get(*self.matches.get(index)?)?;
        let (title, detail) = self.label(target);
        let icon = match target {
            ForwardTarget::NewThread(_) => IconName::Plus,
            ForwardTarget::Thread(_) => IconName::Thread,
            ForwardTarget::Fork(_) => IconName::GitBranch,
            ForwardTarget::HandOff(_) => IconName::ArrowRight,
        };
        let tooltip = match &detail {
            Some(detail) => format!("{title} · {detail}"),
            None => title.to_string(),
        };
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(icon).color(Color::Muted).size(IconSize::Small))
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_2()
                        .child(Label::new(title).single_line().truncate())
                        .when_some(detail, |this, detail| {
                            this.child(
                                Label::new(detail)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .single_line()
                                    .truncate(),
                            )
                        }),
                )
                .tooltip(Tooltip::text(tooltip)),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let (enter, secondary) = match self.selected_target() {
            Some(ForwardTarget::NewThread(_) | ForwardTarget::Thread(_)) => {
                ("Put in message box", Some("Send now"))
            }
            Some(ForwardTarget::Fork(_)) => ("Fork", Some("Fork in a new worktree")),
            Some(ForwardTarget::HandOff(_)) | None => ("Hand off", None),
        };
        let hint = |keys: &'static str, label: &'static str| {
            h_flex()
                .gap_1()
                .child(
                    Label::new(keys)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        };
        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_3()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(hint("⏎", enter))
                .when_some(secondary, |this, label| this.child(hint("⌘⏎", label)))
                .into_any_element(),
        )
    }
}

/// Whether every character of `query` appears in `text`, in order, ignoring case.
pub(crate) fn matches_in_order(query: &str, text: &str) -> bool {
    let mut text_characters = text.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| !character.is_whitespace())
        .all(|wanted| text_characters.any(|character| character == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_mention_round_trips() {
        let uri = MentionUri::Forward {
            session_id: acp_v2::SessionId::new("abc-123"),
            title: "Fix login race".to_string(),
            agent: "Claude Agent".to_string(),
        };
        let parsed = MentionUri::parse(uri.to_uri().as_ref(), util::paths::PathStyle::Unix)
            .expect("forward mention parses");
        assert_eq!(parsed, uri);
        assert_eq!(parsed.name(), "From Claude Agent");
        assert_eq!(
            parsed.tooltip_text().as_deref(),
            Some("Forwarded from Claude Agent in \u{201c}Fix login race\u{201d}")
        );
    }

    #[test]
    fn search_matches_characters_in_order() {
        assert!(matches_in_order("", "New thread with Codex"));
        assert!(matches_in_order("fork cod", "Fork from here to Codex"));
        assert!(matches_in_order(
            "LOGIN",
            "Fix login race Claude · api · gpu-06"
        ));
        assert!(!matches_in_order("codex fork", "Fork from here to Codex"));
    }
}
