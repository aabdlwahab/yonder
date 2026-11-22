//! Agents answering in another agent's thread, as guests.
//!
//! Tagging an agent (`@Codex`) in a thread sends that message to the tagged
//! agent only. The guest runs its own session on the same machine and in the
//! same folders, starts read-only, and sees the owner's conversation: all of it
//! the first time (capped, oldest dropped first) and only what's new after
//! that. Its answer shows up inline in the owner's thread, and the owner gets
//! it on its next turn through a mention the user can delete.

use std::rc::Rc;

use acp_thread::{
    AcpThread, AcpThreadEvent, AgentConnection, AgentThreadEntry, AssistantMessageChunk,
    MentionUri, SelectedPermissionOutcome, ThreadStatus,
};
use agent_client_protocol::v2::schema as acp_v2;
use collections::HashMap;
use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task, TaskExt as _,
};
use markdown::Markdown;
use project::{AgentId, Project};
use serde::{Deserialize, Serialize};
use util::{ResultExt as _, path_list::PathList};

use crate::{Agent, agent_connection_store::AgentConnectionStore};

const GUESTS_KEY_PREFIX: &str = "agent_guests:";
/// How much of the owner's conversation a guest sees at most, in characters.
const MAX_CONTEXT_CHARS: usize = 60_000;
/// How many exchanges each thread keeps on disk.
const MAX_SAVED_EXCHANGES: usize = 50;

/// An agent that can be tagged into a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestCandidate {
    pub agent_id: AgentId,
    pub name: SharedString,
}

/// Mode ids agents use for "look, don't touch".
fn is_read_only_mode(id: &str) -> bool {
    matches!(
        id.to_ascii_lowercase().as_str(),
        "plan" | "read-only" | "read_only" | "readonly" | "ask"
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyStatus {
    Connecting,
    Working,
    WaitingForApproval,
    Done,
    Stopped,
    Failed(SharedString),
}

impl ReplyStatus {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Working | Self::WaitingForApproval
        )
    }
}

pub struct GuestReply {
    pub agent_id: AgentId,
    pub agent_name: SharedString,
    pub status: ReplyStatus,
    pub markdown: Entity<Markdown>,
    text: String,
    /// The guest's entry count before this question, so the reply is
    /// everything after it.
    start_ix: usize,
}

pub struct GuestExchange {
    /// How many owner entries existed when the user asked. The exchange shows
    /// up under the last of them.
    pub anchor: usize,
    pub prompt: Entity<Markdown>,
    prompt_text: String,
    pub replies: Vec<GuestReply>,
}

/// A pending permission request from a guest, with the choices to answer it.
pub struct GuestApproval {
    pub agent_id: AgentId,
    pub title: String,
    tool_call_id: acp_v2::ToolCallId,
    allow: Option<acp_v2::PermissionOption>,
    deny: Option<acp_v2::PermissionOption>,
}

impl GuestApproval {
    pub fn can_answer(&self) -> bool {
        self.allow.is_some() || self.deny.is_some()
    }
}

struct GuestSession {
    session_id: Option<acp_v2::SessionId>,
    /// How many owner entries this guest has already seen.
    seen_entries: usize,
    thread: Option<Entity<AcpThread>>,
    connection: Option<Rc<dyn AgentConnection>>,
    /// The mode the session started in, to switch back to when the user lets
    /// the guest edit.
    editing_mode: Option<acp_v2::SessionConfigValueId>,
    read_only: bool,
    _subscription: Option<gpui::Subscription>,
    _task: Option<Task<()>>,
}

impl GuestSession {
    fn new(session_id: Option<acp_v2::SessionId>, seen_entries: usize) -> Self {
        Self {
            session_id,
            seen_entries,
            thread: None,
            connection: None,
            editing_mode: None,
            read_only: true,
            _subscription: None,
            _task: None,
        }
    }
}

/// A guest answer the owner hasn't been given yet.
pub struct FinishedReply {
    pub agent_name: SharedString,
    pub session_id: acp_v2::SessionId,
    pub question: String,
    pub text: String,
}

/// What the owner thread looks like when the user asks.
pub struct OwnerContext {
    pub owner_name: SharedString,
    /// Each owner entry as markdown.
    pub entries: Vec<String>,
    pub project: Entity<Project>,
    pub work_dirs: PathList,
    pub connection_store: Entity<AgentConnectionStore>,
}

pub enum ThreadGuestsEvent {
    ReplyFinished,
}

/// The guests of one owner thread: their sessions and what they said.
pub struct ThreadGuests {
    owner_session_id: acp_v2::SessionId,
    exchanges: Vec<GuestExchange>,
    sessions: HashMap<AgentId, GuestSession>,
    undelivered: Vec<FinishedReply>,
}

impl EventEmitter<ThreadGuestsEvent> for ThreadGuests {}

#[derive(Default)]
struct GuestRegistry(HashMap<acp_v2::SessionId, Entity<ThreadGuests>>);

impl Global for GuestRegistry {}

#[derive(Default, Serialize, Deserialize)]
struct SavedGuests {
    #[serde(default)]
    sessions: Vec<SavedSession>,
    #[serde(default)]
    exchanges: Vec<SavedExchange>,
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    agent_id: String,
    session_id: String,
    seen_entries: usize,
}

#[derive(Serialize, Deserialize)]
struct SavedExchange {
    anchor: usize,
    prompt: String,
    replies: Vec<SavedReply>,
}

#[derive(Serialize, Deserialize)]
struct SavedReply {
    agent_id: String,
    agent_name: String,
    text: String,
    #[serde(default)]
    error: Option<String>,
}

/// The guests of the thread with `owner_session_id`, loaded from disk the
/// first time.
pub fn guests_for(
    owner_session_id: &acp_v2::SessionId,
    project: &Entity<Project>,
    cx: &mut App,
) -> Entity<ThreadGuests> {
    if let Some(guests) = cx
        .try_global::<GuestRegistry>()
        .and_then(|registry| registry.0.get(owner_session_id))
    {
        return guests.clone();
    }
    let saved: SavedGuests = KeyValueStore::global(cx)
        .read_kvp(&format!("{GUESTS_KEY_PREFIX}{owner_session_id}"))
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default();
    let languages = project.read(cx).languages().clone();
    let guests = cx.new(|cx| {
        let sessions = saved
            .sessions
            .into_iter()
            .map(|session| {
                (
                    AgentId(session.agent_id.into()),
                    GuestSession::new(
                        Some(acp_v2::SessionId::new(session.session_id)),
                        session.seen_entries,
                    ),
                )
            })
            .collect();
        let exchanges = saved
            .exchanges
            .into_iter()
            .map(|exchange| GuestExchange {
                anchor: exchange.anchor,
                prompt: cx.new(|cx| {
                    Markdown::new(
                        exchange.prompt.clone().into(),
                        Some(languages.clone()),
                        None,
                        cx,
                    )
                }),
                prompt_text: exchange.prompt,
                replies: exchange
                    .replies
                    .into_iter()
                    .map(|reply| GuestReply {
                        agent_id: AgentId(reply.agent_id.into()),
                        agent_name: reply.agent_name.into(),
                        status: match reply.error {
                            Some(error) => ReplyStatus::Failed(error.into()),
                            None => ReplyStatus::Done,
                        },
                        markdown: cx.new(|cx| {
                            Markdown::new(
                                reply.text.clone().into(),
                                Some(languages.clone()),
                                None,
                                cx,
                            )
                        }),
                        text: reply.text,
                        start_ix: 0,
                    })
                    .collect(),
            })
            .collect();
        ThreadGuests {
            owner_session_id: owner_session_id.clone(),
            exchanges,
            sessions,
            undelivered: Vec::new(),
        }
    });
    cx.default_global::<GuestRegistry>()
        .0
        .insert(owner_session_id.clone(), guests.clone());
    guests
}

impl ThreadGuests {
    pub fn exchanges(&self) -> &[GuestExchange] {
        &self.exchanges
    }

    pub fn is_working(&self) -> bool {
        self.exchanges.iter().any(|exchange| {
            exchange
                .replies
                .iter()
                .any(|reply| reply.status.is_active())
        })
    }

    pub fn is_read_only(&self, agent_id: &AgentId) -> bool {
        self.sessions
            .get(agent_id)
            .is_none_or(|session| session.read_only)
    }

    pub fn take_undelivered(&mut self) -> Vec<FinishedReply> {
        std::mem::take(&mut self.undelivered)
    }

    /// Sends `blocks` to each of `guests`, with as much of the owner's
    /// conversation as they haven't seen yet.
    pub fn ask(
        &mut self,
        guests: Vec<GuestCandidate>,
        prompt_text: String,
        blocks: Vec<acp_v2::ContentBlock>,
        owner: OwnerContext,
        cx: &mut Context<Self>,
    ) {
        let languages = owner.project.read(cx).languages().clone();
        let anchor = owner.entries.len();
        let exchange_ix = self.exchanges.len();
        let replies = guests
            .iter()
            .map(|guest| GuestReply {
                agent_id: guest.agent_id.clone(),
                agent_name: guest.name.clone(),
                status: ReplyStatus::Connecting,
                markdown: cx.new(|cx| Markdown::new("".into(), Some(languages.clone()), None, cx)),
                text: String::new(),
                start_ix: 0,
            })
            .collect();
        self.exchanges.push(GuestExchange {
            anchor,
            prompt: cx
                .new(|cx| Markdown::new(prompt_text.clone().into(), Some(languages), None, cx)),
            prompt_text,
            replies,
        });

        for guest in guests {
            let session = self
                .sessions
                .entry(guest.agent_id.clone())
                .or_insert_with(|| GuestSession::new(None, 0));
            let seen = session.seen_entries.min(owner.entries.len());
            let mut prompt = vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                guest_preface(&owner.owner_name, &owner.entries[seen..], seen == 0),
            ))];
            prompt.extend(blocks.iter().cloned());
            session.seen_entries = owner.entries.len();

            if let Some(thread) = session.thread.clone() {
                if thread.read(cx).status() == ThreadStatus::Idle {
                    self.send_to_guest(exchange_ix, &guest.agent_id, thread, prompt, cx);
                } else {
                    self.set_status(
                        exchange_ix,
                        &guest.agent_id,
                        ReplyStatus::Failed(
                            format!("{} is still answering the last message.", guest.name).into(),
                        ),
                        cx,
                    );
                }
                continue;
            }

            let saved_session_id = session.session_id.clone();
            let agent_id = guest.agent_id.clone();
            let project = owner.project.clone();
            let work_dirs = owner.work_dirs.clone();
            let entry = owner.connection_store.update(cx, |store, cx| {
                store.request_connection(Agent::from(agent_id.clone()), guest_server(&agent_id), cx)
            });
            let connect = entry.read(cx).wait_for_connection();
            let task = cx.spawn(async move |this, cx| {
                let result = async {
                    let connection = connect
                        .await
                        .map_err(|error| anyhow::anyhow!("{error:?}"))?
                        .connection;
                    let load = match saved_session_id {
                        Some(session_id) if connection.supports_load_session() => {
                            let load = cx.update(|cx| {
                                connection.clone().load_session(
                                    session_id,
                                    project.clone(),
                                    work_dirs.clone(),
                                    None,
                                    cx,
                                )
                            });
                            load.await.log_err()
                        }
                        _ => None,
                    };
                    let thread = match load {
                        Some(thread) => thread,
                        None => {
                            cx.update(|cx| connection.clone().new_session(project, work_dirs, cx))
                                .await?
                        }
                    };
                    anyhow::Ok((connection, thread))
                }
                .await;
                this.update(cx, |this, cx| match result {
                    Ok((connection, thread)) => {
                        this.attach_session(&agent_id, connection, thread.clone(), cx);
                        this.send_to_guest(exchange_ix, &agent_id, thread, prompt, cx);
                    }
                    Err(error) => {
                        log::error!("guest {agent_id} failed to start: {error:#}");
                        this.set_status(
                            exchange_ix,
                            &agent_id,
                            ReplyStatus::Failed(start_failure_message(&error).into()),
                            cx,
                        );
                    }
                })
                .ok();
            });
            if let Some(session) = self.sessions.get_mut(&guest.agent_id) {
                session._task = Some(task);
            }
        }
        self.save(cx);
        cx.notify();
    }

    /// Stops every guest that is still answering.
    pub fn stop_all(&mut self, cx: &mut Context<Self>) {
        for session in self.sessions.values() {
            if let Some(thread) = &session.thread
                && thread.read(cx).status() == ThreadStatus::Generating
            {
                thread
                    .update(cx, |thread, cx| thread.cancel(cx))
                    .detach_and_log_err(cx);
            }
        }
        for exchange in &mut self.exchanges {
            for reply in &mut exchange.replies {
                if reply.status.is_active() {
                    reply.status = ReplyStatus::Stopped;
                }
            }
        }
        self.save(cx);
        cx.notify();
    }

    /// Lets the guest edit files, or puts it back in read-only mode.
    pub fn set_read_only(&mut self, agent_id: &AgentId, read_only: bool, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get_mut(agent_id) else {
            return;
        };
        session.read_only = read_only;
        if let (Some(connection), Some(session_id)) = (&session.connection, &session.session_id) {
            apply_mode(
                connection.as_ref(),
                session_id,
                read_only,
                session.editing_mode.clone(),
                cx,
            );
        }
        cx.notify();
    }

    /// Permission requests guests are waiting on.
    pub fn approvals(&self, cx: &App) -> Vec<GuestApproval> {
        let mut approvals = Vec::new();
        for (agent_id, session) in &self.sessions {
            let Some(thread) = &session.thread else {
                continue;
            };
            let thread = thread.read(cx);
            for request in thread.pending_permission_requests() {
                let Some(tool_call_id) = request.legacy_tool_call_id() else {
                    continue;
                };
                let options = request.legacy_options();
                let first = |kind| {
                    options
                        .and_then(|options| options.first_option_of_kind(kind))
                        .cloned()
                };
                let title = thread
                    .tool_call(tool_call_id)
                    .map(|(_, call)| call.label.read(cx).source().to_string())
                    .unwrap_or_default();
                approvals.push(GuestApproval {
                    agent_id: agent_id.clone(),
                    title,
                    tool_call_id: tool_call_id.clone(),
                    allow: first(acp_v2::PermissionOptionKind::AllowOnce),
                    deny: first(acp_v2::PermissionOptionKind::RejectOnce),
                });
            }
        }
        approvals
    }

    pub fn answer(&mut self, approval: &GuestApproval, allow: bool, cx: &mut Context<Self>) {
        let option = if allow {
            &approval.allow
        } else {
            &approval.deny
        };
        let Some(option) = option.clone() else {
            return;
        };
        let Some(thread) = self
            .sessions
            .get(&approval.agent_id)
            .and_then(|session| session.thread.clone())
        else {
            return;
        };
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                approval.tool_call_id.clone(),
                SelectedPermissionOutcome::new(option.option_id, option.kind),
                cx,
            )
        });
    }

    fn attach_session(
        &mut self,
        agent_id: &AgentId,
        connection: Rc<dyn AgentConnection>,
        thread: Entity<AcpThread>,
        cx: &mut Context<Self>,
    ) {
        let session_id = thread.read(cx).session_id().clone();
        let editing_mode = current_mode(connection.as_ref(), &session_id, cx)
            .filter(|mode| !is_read_only_mode(&mode.to_string()));
        let subscription = cx.subscribe(&thread, {
            let agent_id = agent_id.clone();
            move |this, thread, event, cx| this.handle_guest_event(&agent_id, thread, event, cx)
        });
        let session = self
            .sessions
            .entry(agent_id.clone())
            .or_insert_with(|| GuestSession::new(None, 0));
        if session.editing_mode.is_none() {
            session.editing_mode = editing_mode;
        }
        apply_mode(
            connection.as_ref(),
            &session_id,
            session.read_only,
            session.editing_mode.clone(),
            cx,
        );
        session.session_id = Some(session_id);
        session.connection = Some(connection);
        session.thread = Some(thread);
        session._subscription = Some(subscription);
    }

    fn send_to_guest(
        &mut self,
        exchange_ix: usize,
        agent_id: &AgentId,
        thread: Entity<AcpThread>,
        prompt: Vec<acp_v2::ContentBlock>,
        cx: &mut Context<Self>,
    ) {
        let start_ix = thread.read(cx).entries().len();
        if let Some(reply) = self.reply_mut(exchange_ix, agent_id) {
            reply.start_ix = start_ix;
            reply.status = ReplyStatus::Working;
        }
        let submission = thread.update(cx, |thread, cx| thread.send(prompt, cx));
        let agent_id = agent_id.clone();
        cx.spawn(async move |this, cx| {
            if let Err(error) = submission.await {
                this.update(cx, |this, cx| {
                    this.set_status(
                        exchange_ix,
                        &agent_id,
                        ReplyStatus::Failed(error.to_string().into()),
                        cx,
                    )
                })
                .ok();
            }
        })
        .detach();
        self.save(cx);
        cx.notify();
    }

    fn handle_guest_event(
        &mut self,
        agent_id: &AgentId,
        thread: Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        // Only the latest question to this guest can still be streaming.
        let Some(exchange_ix) = self.exchanges.iter().rposition(|exchange| {
            exchange
                .replies
                .iter()
                .any(|reply| &reply.agent_id == agent_id)
        }) else {
            return;
        };
        match event {
            AcpThreadEvent::NewEntry
            | AcpThreadEvent::EntryUpdated(_)
            | AcpThreadEvent::EntriesRemoved(_) => {
                let Some(reply) = self.reply_mut(exchange_ix, agent_id) else {
                    return;
                };
                if !reply.status.is_active() {
                    return;
                }
                let text = reply_markdown(thread.read(cx), reply.start_ix, cx);
                if text != reply.text {
                    reply.text = text.clone();
                    reply
                        .markdown
                        .update(cx, |markdown, cx| markdown.replace(text, cx));
                }
                cx.notify();
            }
            AcpThreadEvent::ToolAuthorizationRequested(_) => {
                self.set_status(exchange_ix, agent_id, ReplyStatus::WaitingForApproval, cx);
            }
            AcpThreadEvent::ToolAuthorizationReceived(_) => {
                self.set_status(exchange_ix, agent_id, ReplyStatus::Working, cx);
            }
            AcpThreadEvent::Stopped { .. } => {
                let Some(reply) = self.reply_mut(exchange_ix, agent_id) else {
                    return;
                };
                if !reply.status.is_active() {
                    return;
                }
                let text = reply_markdown(thread.read(cx), reply.start_ix, cx);
                reply.text = text.clone();
                reply
                    .markdown
                    .update(cx, |markdown, cx| markdown.replace(text.clone(), cx));
                reply.status = ReplyStatus::Done;
                let agent_name = reply.agent_name.clone();
                if !text.trim().is_empty() {
                    self.undelivered.push(FinishedReply {
                        agent_name,
                        session_id: thread.read(cx).session_id().clone(),
                        question: self.exchanges[exchange_ix].prompt_text.clone(),
                        text,
                    });
                    cx.emit(ThreadGuestsEvent::ReplyFinished);
                }
                self.save(cx);
                cx.notify();
            }
            AcpThreadEvent::Error | AcpThreadEvent::ConnectionLost => {
                let message = if matches!(event, AcpThreadEvent::ConnectionLost) {
                    "Lost the connection to the agent."
                } else {
                    "The agent stopped with an error."
                };
                self.set_status(
                    exchange_ix,
                    agent_id,
                    ReplyStatus::Failed(message.into()),
                    cx,
                );
            }
            _ => {}
        }
    }

    fn reply_mut(&mut self, exchange_ix: usize, agent_id: &AgentId) -> Option<&mut GuestReply> {
        self.exchanges
            .get_mut(exchange_ix)?
            .replies
            .iter_mut()
            .find(|reply| &reply.agent_id == agent_id)
    }

    fn set_status(
        &mut self,
        exchange_ix: usize,
        agent_id: &AgentId,
        status: ReplyStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some(reply) = self.reply_mut(exchange_ix, agent_id) {
            reply.status = status;
        }
        self.save(cx);
        cx.notify();
    }

    fn save(&self, cx: &mut Context<Self>) {
        let skip = self.exchanges.len().saturating_sub(MAX_SAVED_EXCHANGES);
        let saved = SavedGuests {
            sessions: self
                .sessions
                .iter()
                .filter_map(|(agent_id, session)| {
                    Some(SavedSession {
                        agent_id: agent_id.to_string(),
                        session_id: session.session_id.as_ref()?.to_string(),
                        seen_entries: session.seen_entries,
                    })
                })
                .collect(),
            exchanges: self
                .exchanges
                .iter()
                .skip(skip)
                .map(|exchange| SavedExchange {
                    anchor: exchange.anchor,
                    prompt: exchange.prompt_text.clone(),
                    replies: exchange
                        .replies
                        .iter()
                        .map(|reply| SavedReply {
                            agent_id: reply.agent_id.to_string(),
                            agent_name: reply.agent_name.to_string(),
                            text: reply.text.clone(),
                            error: match &reply.status {
                                ReplyStatus::Failed(error) => Some(error.to_string()),
                                ReplyStatus::Stopped => Some("Stopped.".to_string()),
                                // Not finished yet; after a restart it can't
                                // resume, so it reads as interrupted.
                                status if status.is_active() => {
                                    Some("Interrupted before it finished.".to_string())
                                }
                                _ => None,
                            },
                        })
                        .collect(),
                })
                .collect(),
        };
        let Some(json) = serde_json::to_string(&saved).log_err() else {
            return;
        };
        let key = format!("{GUESTS_KEY_PREFIX}{}", self.owner_session_id);
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move { kvp.write_kvp(key, json).await.log_err() })
            .detach();
    }
}

/// The server for a guest agent. Guests are always external agents; Yonder's
/// own agent needs a thread store and isn't offered.
fn guest_server(agent_id: &AgentId) -> Rc<dyn agent_servers::AgentServer> {
    #[cfg(any(test, feature = "test-support"))]
    if matches!(Agent::from(agent_id.clone()), Agent::Stub) {
        return Rc::new(crate::test_support::StubAgentServer::default_response());
    }
    Rc::new(agent_servers::CustomAgentServer::new(agent_id.clone()))
}

fn apply_mode(
    connection: &dyn AgentConnection,
    session_id: &acp_v2::SessionId,
    read_only: bool,
    editing_mode: Option<acp_v2::SessionConfigValueId>,
    cx: &mut App,
) {
    let Some(modes) = connection.session_modes(session_id, cx) else {
        apply_mode_option(connection, session_id, read_only, editing_mode, cx);
        return;
    };
    let target = if read_only {
        modes
            .all_modes()
            .into_iter()
            .map(|mode| mode.value)
            .find(|mode| is_read_only_mode(&mode.to_string()))
    } else {
        editing_mode
    };
    if let Some(target) = target
        && target != modes.current_mode()
    {
        modes.set_mode(target, cx).detach_and_log_err(cx);
    }
}

fn current_mode(
    connection: &dyn AgentConnection,
    session_id: &acp_v2::SessionId,
    cx: &App,
) -> Option<acp_v2::SessionConfigValueId> {
    if let Some(modes) = connection.session_modes(session_id, cx) {
        return Some(modes.current_mode());
    }
    let options = connection.session_config_options(session_id, cx)?;
    options.config_options().into_iter().find_map(|option| {
        let is_mode = option.category == Some(acp_v2::SessionConfigOptionCategory::Mode)
            || option.config_id.to_string() == "mode";
        match option.kind {
            acp_v2::SessionConfigKind::Select(select) if is_mode => Some(select.current_value),
            _ => None,
        }
    })
}

/// Agents that report their mode as a session option rather than a mode.
fn apply_mode_option(
    connection: &dyn AgentConnection,
    session_id: &acp_v2::SessionId,
    read_only: bool,
    editing_mode: Option<acp_v2::SessionConfigValueId>,
    cx: &mut App,
) {
    let Some(options) = connection.session_config_options(session_id, cx) else {
        return;
    };
    if options.is_read_only() {
        return;
    }
    let Some((option_id, current, values)) =
        options.config_options().into_iter().find_map(|option| {
            let is_mode = option.category == Some(acp_v2::SessionConfigOptionCategory::Mode)
                || option.config_id.to_string() == "mode";
            let acp_v2::SessionConfigKind::Select(select) = option.kind else {
                return None;
            };
            if !is_mode {
                return None;
            }
            let values: Vec<acp_v2::SessionConfigValueId> = match select.options {
                acp_v2::SessionConfigSelectOptions::Ungrouped(options) => {
                    options.into_iter().map(|option| option.value).collect()
                }
                acp_v2::SessionConfigSelectOptions::Grouped(groups) => groups
                    .into_iter()
                    .flat_map(|group| group.options)
                    .map(|option| option.value)
                    .collect(),
                _ => Vec::new(),
            };
            Some((option.config_id, select.current_value, values))
        })
    else {
        return;
    };
    let target = if read_only {
        values
            .into_iter()
            .find(|value| is_read_only_mode(&value.to_string()))
    } else {
        editing_mode
    };
    if let Some(target) = target
        && target != current
    {
        options
            .set_config_option(option_id, acp_v2::SessionConfigOptionValue::id(target), cx)
            .detach_and_log_err(cx);
    }
}

/// Tells the guest where it is and what it missed.
fn guest_preface(owner_name: &str, entries: &[String], first_time: bool) -> String {
    let conversation = capped_conversation(entries);
    let mut preface = if first_time {
        format!(
            "The user is working with {owner_name} in this conversation and tagged you in \
             as a guest. Only you answer this message. Give your view; don't edit files or \
             change anything unless the user asks you to."
        )
    } else {
        String::new()
    };
    if !conversation.is_empty() {
        let lead = if first_time {
            "The conversation so far:"
        } else {
            "Since your last answer, the user's conversation with the owner of this thread continued:"
        };
        if !preface.is_empty() {
            preface.push_str("\n\n");
        }
        preface.push_str(&format!(
            "{lead}\n\n<conversation>\n{conversation}\n</conversation>"
        ));
    }
    if preface.is_empty() {
        preface.push_str("The user has a follow-up for you.");
    }
    preface.push_str("\n\nThe user's message to you:");
    preface
}

/// The newest entries that fit in [`MAX_CONTEXT_CHARS`], oldest first.
fn capped_conversation(entries: &[String]) -> String {
    let mut total = 0;
    let mut kept = Vec::new();
    for entry in entries.iter().rev() {
        if total + entry.len() > MAX_CONTEXT_CHARS && !kept.is_empty() {
            break;
        }
        total += entry.len();
        kept.push(entry.as_str());
    }
    let dropped = entries.len() - kept.len();
    kept.reverse();
    let mut conversation = kept.join("\n");
    if conversation.len() > MAX_CONTEXT_CHARS {
        let mut start = conversation.len() - MAX_CONTEXT_CHARS;
        while !conversation.is_char_boundary(start) {
            start += 1;
        }
        conversation = conversation[start..].to_string();
    }
    if dropped > 0 {
        conversation = format!("(Earlier messages left out.)\n\n{conversation}");
    }
    conversation
}

/// The guest's answer: its messages, with each tool it used as a line.
fn reply_markdown(thread: &AcpThread, start_ix: usize, cx: &App) -> String {
    let mut parts = Vec::new();
    for entry in thread.entries().iter().skip(start_ix) {
        match entry {
            AgentThreadEntry::AssistantMessage(message) => {
                for chunk in &message.chunks {
                    if let AssistantMessageChunk::Message { block, .. } = chunk {
                        let text = block.to_markdown(cx);
                        if !text.trim().is_empty() {
                            parts.push(text);
                        }
                    }
                }
            }
            AgentThreadEntry::ToolCall(call) => {
                let label = call.label.read(cx).source().trim().replace('\n', " ");
                if !label.is_empty() {
                    parts.push(format!("_{label}_"));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

fn start_failure_message(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    if text.contains("AuthRequired") || text.to_ascii_lowercase().contains("auth") {
        "This agent isn't signed in on this machine. Sign in from Agent Accounts in Settings."
            .to_string()
    } else {
        format!("Couldn't start the agent: {text}")
    }
}

/// The mention that carries a guest's answer into the owner's next turn.
pub fn reply_blocks(reply: &FinishedReply) -> Vec<acp_v2::ContentBlock> {
    let uri = MentionUri::GuestReply {
        session_id: reply.session_id.clone(),
        agent: reply.agent_name.to_string(),
    };
    let question = reply.question.trim();
    let text = format!(
        "{agent} answered as a guest in this thread. The user asked {agent}:\n\n{question}\n\n\
         {agent}'s answer:\n\n{answer}",
        agent = reply.agent_name,
        answer = reply.text.trim(),
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

/// The agent a block tags, if it's a guest tag.
pub fn guest_tag(block: &acp_v2::ContentBlock) -> Option<MentionUri> {
    let uri = match block {
        acp_v2::ContentBlock::ResourceLink(link) => link.uri.as_str(),
        acp_v2::ContentBlock::Resource(resource) => match &resource.resource {
            acp_v2::EmbeddedResourceResource::TextResourceContents(text) => text.uri.as_str(),
            _ => return None,
        },
        _ => return None,
    };
    let uri = MentionUri::parse(uri, util::paths::PathStyle::local()).ok()?;
    matches!(
        uri,
        MentionUri::Guest { .. } | MentionUri::GuestReply { .. }
    )
    .then_some(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preface_includes_the_conversation_the_first_time() {
        let entries = vec!["## User\n\nfix the race".to_string()];
        let preface = guest_preface("Claude", &entries, true);
        assert!(preface.contains("working with Claude"));
        assert!(preface.contains("<conversation>\n## User\n\nfix the race\n</conversation>"));
        assert!(preface.ends_with("The user's message to you:"));
    }

    #[test]
    fn preface_without_anything_new() {
        let preface = guest_preface("Claude", &[], false);
        assert_eq!(
            preface,
            "The user has a follow-up for you.\n\nThe user's message to you:"
        );
    }

    #[test]
    fn conversation_drops_oldest_entries_first() {
        let old = "a".repeat(MAX_CONTEXT_CHARS);
        let new = "b".repeat(10);
        let conversation = capped_conversation(&[old, new.clone()]);
        assert_eq!(
            conversation,
            format!("(Earlier messages left out.)\n\n{new}")
        );
    }

    #[test]
    fn read_only_modes() {
        assert!(is_read_only_mode("plan"));
        assert!(is_read_only_mode("read-only"));
        assert!(!is_read_only_mode("default"));
        assert!(!is_read_only_mode("auto"));
    }
}
