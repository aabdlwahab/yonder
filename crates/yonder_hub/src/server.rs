//! The running hub: a websocket listener for clients, agent processes, and
//! the relay between them.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_servers::{AgentProcess, AgentProcessGuard, CustomAgentServer, spawn_agent_process};
use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
};
use collections::HashMap;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::future::{self, Either, select};
use futures::{AsyncReadExt as _, FutureExt as _, SinkExt as _, StreamExt as _};
use gpui::{App, AppContext as _, AsyncApp, BackgroundExecutor, Entity, Task};
use project::agent_server_store::{AgentServerCommand, AllAgentServersSettings};
use project::{AgentId, Project};
use serde_json::{Value, json};
use settings::Settings as _;

use crate::dispatch::{
    self, Action, CallEnd, Consent, Dispatch, Record, ToolResult, ToolStep, VoiceCall,
};
use crate::headless::AgentCliAppState;
use crate::machines::Machines;
use crate::push::{Push, Subscription};
use crate::registry::Registry;
use crate::relay::{
    AgentKey, ClientId, Endpoint, Notice, NoticeKind, Output, Relay, error_response,
    result_response,
};
use gpui::http_client::{AsyncBody, Method, Request as HttpRequest};

pub const DEFAULT_PORT: u16 = 7766;

pub(crate) enum Event {
    ClientConnected {
        client: ClientId,
        endpoint: Endpoint,
        tx: mpsc::UnboundedSender<String>,
    },
    ClientLine {
        client: ClientId,
        line: String,
    },
    ClientGone(ClientId),
    AgentLine {
        key: AgentKey,
        generation: u64,
        line: String,
    },
    AgentGone {
        key: AgentKey,
        generation: u64,
    },
    Voice(VoiceCall),
    DispatchSessions {
        id: Option<String>,
        reply: oneshot::Sender<Value>,
    },
    /// Something the phone asked for: notifications, or acting on a thread.
    Web {
        request: WebRequest,
        reply: oneshot::Sender<std::result::Result<Value, String>>,
    },
}

pub(crate) enum WebRequest {
    PushKey,
    Subscribe(Subscription),
    Unsubscribe {
        endpoint: String,
    },
    TestPush {
        endpoint: String,
    },
    Answer {
        session: String,
        approval: String,
        approve: bool,
    },
    Reply {
        session: String,
        text: String,
    },
}

struct RunningAgent {
    tx: mpsc::UnboundedSender<String>,
    generation: u64,
    _guard: AgentProcessGuard,
}

struct Hub {
    relay: Relay,
    registry: Registry,
    machines: Machines,
    clients: HashMap<ClientId, mpsc::UnboundedSender<String>>,
    agents: HashMap<AgentKey, RunningAgent>,
    dispatch: Option<Dispatch>,
    next_generation: u64,
    events: mpsc::UnboundedSender<Event>,
    /// Messages from Dispatch per thread, sent one turn at a time. A thread
    /// is in here while its messages are being delivered.
    outbox: HashMap<String, VecDeque<ThreadMessage>>,
    /// Ids for Dispatch's own connections, counting down so they never meet
    /// the websocket ids counting up.
    next_internal_client: ClientId,
    /// Notifications for phones, when phone Dispatch is set up.
    push: Option<Rc<Push>>,
}

/// Starts the hub and runs until the process exits.
pub fn serve(
    app_state: Arc<AgentCliAppState>,
    listen: String,
    local_name: String,
    dispatch_codex: Option<PathBuf>,
    dispatch_web: crate::dispatch_web::Options,
    cx: &mut App,
) -> Task<Result<()>> {
    cx.spawn(async move |cx| {
        let registry = Registry::open(Registry::default_dir())?;
        let token = registry.token()?;
        let dispatch = dispatch_codex.map(|codex| Dispatch::new(codex, registry.dispatch_dir()));
        let push = match dispatch_web.load_push() {
            Some(Ok(push)) => Some(Rc::new(push)),
            Some(Err(error)) => {
                log::error!("phone notifications are off: {error:#}");
                None
            }
            None => None,
        };
        let mut relay = Relay::new(now);
        let saved = registry.load()?;
        log::info!("restored {} sessions", saved.len());
        relay.restore(saved);
        let machines = Machines::new(app_state, local_name.clone());
        relay.set_machines(cx.update(|cx| machines.names(cx)));
        relay.set_agents(cx.update(|cx| {
            let mut agents: Vec<String> = AllAgentServersSettings::get_global(cx)
                .keys()
                .cloned()
                .collect();
            agents.sort();
            agents
        }));

        let (events, mut event_rx) = mpsc::unbounded::<Event>();
        let hub = Rc::new(RefCell::new(Hub {
            relay,
            registry,
            machines,
            clients: HashMap::default(),
            agents: HashMap::default(),
            dispatch,
            next_generation: 0,
            events: events.clone(),
            outbox: HashMap::default(),
            next_internal_client: ClientId::MAX,
            push,
        }));

        let listener = smol::net::TcpListener::bind(&listen)
            .await
            .with_context(|| format!("listening on {listen}"))?;
        eprintln!("yonder hub on {local_name}: listening on ws://{listen}");
        eprintln!("token: {}", Registry::default_dir().join("token").display());
        cx.background_spawn(accept_loop(listener, token, events.clone()))
            .detach();

        if dispatch_web.enabled() {
            let server = gpui_tokio::Tokio::spawn_result(
                cx,
                crate::dispatch_web::serve(dispatch_web, events),
            );
            cx.spawn(async move |_| {
                if let Err(error) = server.await {
                    log::error!("Dispatch web server: {error:#}");
                }
            })
            .detach();
        }

        while let Some(event) = event_rx.next().await {
            let outputs = {
                let mut state = hub.borrow_mut();
                match event {
                    Event::ClientConnected {
                        client,
                        endpoint,
                        tx,
                    } => {
                        log::info!("client {client} connected to {endpoint:?}");
                        state.clients.insert(client, tx);
                        state.relay.client_connected(client, endpoint);
                        Vec::new()
                    }
                    Event::ClientLine { client, line } => state.relay.client_message(client, &line),
                    Event::ClientGone(client) => {
                        log::info!("client {client} left");
                        state.clients.remove(&client);
                        state.relay.client_gone(client);
                        Vec::new()
                    }
                    Event::AgentLine {
                        key,
                        generation,
                        line,
                    } => {
                        if state.agents.get(&key).map(|agent| agent.generation) == Some(generation)
                        {
                            state.relay.agent_line(&key, &line)
                        } else {
                            Vec::new()
                        }
                    }
                    Event::AgentGone { key, generation } => {
                        if state.agents.get(&key).map(|agent| agent.generation) == Some(generation)
                        {
                            log::warn!("{} on {} exited", key.agent, key.machine);
                            state.agents.remove(&key);
                            state.relay.agent_gone(&key, "the agent exited")
                        } else {
                            Vec::new()
                        }
                    }
                    Event::DispatchSessions { id, reply } => {
                        let sessions = match id {
                            Some(id) => state
                                .relay
                                .dispatch_timeline(&id, 120)
                                .unwrap_or(Value::Null),
                            None => {
                                let mut catalog = state
                                    .relay
                                    .dispatch_sessions(dispatch::SessionFilter::All, 50);
                                let conversations = state.relay.dispatch_conversations(30);
                                if let Some(catalog) = catalog.as_object_mut() {
                                    catalog.insert(
                                        "conversations".into(),
                                        conversations["conversations"].clone(),
                                    );
                                    catalog.insert(
                                        "conversationTotal".into(),
                                        conversations["total"].clone(),
                                    );
                                }
                                catalog
                            }
                        };
                        reply.send(sessions).ok();
                        Vec::new()
                    }
                    Event::Voice(call) => {
                        start_dispatch_voice(hub.clone(), state.dispatch.clone(), call, cx);
                        Vec::new()
                    }
                    Event::Web { request, reply } => {
                        drop(state);
                        web_request(&hub, request, reply, cx)
                    }
                }
            };
            apply(&hub, outputs, cx);
        }
        Ok(())
    })
}

fn start_dispatch_voice(
    hub: Rc<RefCell<Hub>>,
    dispatch: Option<Dispatch>,
    mut call: VoiceCall,
    cx: &mut AsyncApp,
) {
    cx.spawn(async move |cx| {
        let result = match dispatch {
            Some(dispatch) => {
                let conversation = Conversation::new(hub.clone(), "call", cx.clone());
                let tools =
                    dispatch_tools(hub, dispatch.consent(), conversation.handle(), cx.clone());
                let record = |record: Record<'_>| conversation.record(record);
                let result = dispatch
                    .voice(&mut call, &tools, &record, cx.background_executor())
                    .await;
                match &result {
                    Ok(CallEnd::HungUp) => conversation.finish(Some(("Call ended", false))),
                    Ok(CallEnd::Closed(reason)) => {
                        let note = match reason {
                            Some(reason) => format!("The voice session closed: {reason}"),
                            None => "The voice session closed".to_owned(),
                        };
                        conversation.finish(Some((&note, true)));
                    }
                    Err(error) => {
                        conversation.finish(Some((&format!("The call failed: {error:#}"), true)));
                    }
                }
                result.map(|_| ())
            }
            None => Err(anyhow!("Dispatch is disabled on this hub")),
        };
        if let Err(error) = result {
            log::warn!("Dispatch voice call failed: {error:#}");
            call.updates
                .send(json!({ "type": "error", "message": format!("{error:#}") }))
                .await
                .ok();
        }
    })
    .detach();
}

fn apply(hub: &Rc<RefCell<Hub>>, outputs: Vec<Output>, cx: &mut AsyncApp) {
    let mut save = false;
    for output in outputs {
        match output {
            Output::ToClient(client, line) => {
                if let Some(tx) = hub.borrow().clients.get(&client) {
                    tx.unbounded_send(line).ok();
                }
            }
            Output::ToAgent(key, line) => {
                if let Some(agent) = hub.borrow().agents.get(&key) {
                    agent.tx.unbounded_send(line).ok();
                }
            }
            Output::StartAgent(key) => start_agent(hub.clone(), key, cx),
            Output::AskDispatch { client, id, prompt } => {
                start_dispatch(hub.clone(), client, id, prompt, cx);
            }
            Output::ContinueDispatch {
                client,
                id,
                session,
                prompt,
            } => continue_dispatch(hub.clone(), client, id, session, prompt, cx),
            Output::Notify(notice) => send_notice(hub, &notice, cx),
            Output::SaveSessions => save = true,
            Output::FinishSwitch {
                client,
                request,
                session,
                agent,
                setup,
                replay,
            } => {
                let outputs = {
                    let mut state = hub.borrow_mut();
                    let Hub {
                        relay, registry, ..
                    } = &mut *state;
                    match relay.finish_switch(&session, agent, setup, |sessions| {
                        registry.save_sessions(sessions)
                    }) {
                        Ok(()) => relay.attach_live(client, session, &request, replay),
                        Err(error) => vec![Output::ToClient(
                            client,
                            error_response(&request, &format!("{error:#}")),
                        )],
                    }
                };
                apply(hub, outputs, cx);
            }
            Output::AppendLog { session, line } => {
                if let Err(error) = hub.borrow().registry.append_log(&session, &line) {
                    log::error!("saving the log of {session}: {error:#}");
                }
            }
        }
    }
    if save {
        let hub = hub.borrow();
        if let Err(error) = hub.registry.save_sessions(&hub.relay.sessions()) {
            log::error!("saving sessions: {error:#}");
        }
    }
}

/// Answers the phone: notification settings, and approving or replying in a
/// thread. An approval tapped on the phone is the user's own answer, so it
/// skips Dispatch's read-back.
fn web_request(
    hub: &Rc<RefCell<Hub>>,
    request: WebRequest,
    reply: oneshot::Sender<std::result::Result<Value, String>>,
    cx: &mut AsyncApp,
) -> Vec<Output> {
    let push = hub
        .borrow()
        .push
        .clone()
        .ok_or_else(|| "Notifications aren't set up on this hub.".to_owned());
    let (result, outputs) = match request {
        WebRequest::PushKey => (
            push.map(|push| json!({ "key": push.public_key() })),
            Vec::new(),
        ),
        WebRequest::Subscribe(subscription) => (
            push.and_then(|push| {
                push.subscribe(subscription)
                    .map(|()| json!({ "subscribed": true }))
                    .map_err(|error| format!("{error:#}"))
            }),
            Vec::new(),
        ),
        WebRequest::Unsubscribe { endpoint } => (
            push.and_then(|push| {
                push.unsubscribe(&endpoint)
                    .map(|removed| json!({ "removed": removed }))
                    .map_err(|error| format!("{error:#}"))
            }),
            Vec::new(),
        ),
        WebRequest::TestPush { endpoint } => {
            let waiting = hub.borrow().relay.waiting_count();
            let result = push.and_then(|push| {
                let subscription = push
                    .subscription(&endpoint)
                    .ok_or_else(|| "Turn notifications on first.".to_owned())?;
                let payload = json!({
                    "title": "Dispatch notifications are on",
                    "body": "Threads that need you or finish will show up here.",
                    "tag": "test",
                    "url": "/dispatch/",
                    "waiting": waiting,
                });
                send_push(push, subscription, payload, "normal", cx);
                Ok(json!({ "sent": true }))
            });
            (result, Vec::new())
        }
        WebRequest::Answer {
            session,
            approval,
            approve,
        } => {
            let result = hub
                .borrow_mut()
                .relay
                .dispatch_answer_approval(&session, &approval, approve);
            match result {
                Ok(outputs) => {
                    let answered = if approve { "approved" } else { "rejected" };
                    (Ok(json!({ "answered": answered })), outputs)
                }
                Err(error) => (Err(format!("{error:#}")), Vec::new()),
            }
        }
        WebRequest::Reply { session, text } => {
            let text = text.trim().to_owned();
            let target = hub.borrow().relay.dispatch_target(&session);
            let problem = match target {
                _ if text.is_empty() || text.len() > dispatch::MAX_PROMPT_BYTES => {
                    Some("Write a message of at most 8192 bytes.")
                }
                Some((key, _)) if key.agent != crate::relay::DISPATCH_AGENT => None,
                _ => Some("That thread isn't on this hub."),
            };
            if let Some(problem) = problem {
                (Err(problem.to_owned()), Vec::new())
            } else {
                // Goes through Dispatch's queue, so it waits for a busy
                // thread's turn to end.
                let hub = hub.clone();
                cx.spawn(async move |cx| {
                    let result =
                        send_to_thread(hub, session, text, false, Rc::default(), cx.clone())
                            .await
                            .map(|result| result.data().clone())
                            .map_err(|error| format!("{error:#}"));
                    reply.send(result).ok();
                })
                .detach();
                return Vec::new();
            }
        }
    };
    reply.send(result).ok();
    outputs
}

/// Tells the phones that asked for it about a thread.
fn send_notice(hub: &Rc<RefCell<Hub>>, notice: &Notice, cx: &mut AsyncApp) {
    let (push, waiting) = {
        let hub = hub.borrow();
        (hub.push.clone(), hub.relay.waiting_count())
    };
    let Some(push) = push else {
        return;
    };
    let kind = match notice.kind {
        NoticeKind::Approval => "approval",
        NoticeKind::Done => "done",
        NoticeKind::Stopped => "stopped",
    };
    let payload = json!({
        "title": notice.title,
        "body": notice.body,
        // A newer notice about the same thing replaces the older one.
        "tag": format!("{kind}-{}", notice.session),
        "url": format!("/dispatch/?thread={}", url_component(&notice.session)),
        "waiting": waiting,
    });
    let urgency = if notice.kind == NoticeKind::Done {
        "normal"
    } else {
        "high"
    };
    for subscription in push.subscribers(notice.kind) {
        send_push(push.clone(), subscription, payload.clone(), urgency, cx);
    }
}

fn send_push(
    push: Rc<Push>,
    subscription: Subscription,
    payload: Value,
    urgency: &'static str,
    cx: &mut AsyncApp,
) {
    let message = match push.message(&subscription, payload.to_string().as_bytes()) {
        Ok(message) => message,
        Err(error) => {
            log::warn!("preparing a phone notification: {error:#}");
            return;
        }
    };
    let http = cx.update(|cx| cx.http_client());
    cx.spawn(async move |_| {
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri(&message.endpoint)
            // An approval still matters a day later; Apple drops it after that.
            .header("TTL", "86400")
            .header("Urgency", urgency)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("Authorization", &message.authorization)
            .body(AsyncBody::from(message.body));
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                log::warn!("building a phone notification: {error}");
                return;
            }
        };
        match http.send(request).await {
            Ok(response) if response.status().is_success() => {}
            // The phone unsubscribed or deleted the app.
            Ok(response) if matches!(response.status().as_u16(), 404 | 410) => {
                log::info!("a phone stopped taking notifications; forgetting it");
                if let Err(error) = push.unsubscribe(&message.endpoint) {
                    log::warn!("forgetting a phone: {error:#}");
                }
            }
            Ok(mut response) => {
                let mut text = String::new();
                response.body_mut().read_to_string(&mut text).await.ok();
                log::warn!(
                    "the push service refused a notification ({}): {}",
                    response.status(),
                    crate::relay::tail(&text, 300)
                );
            }
            Err(error) => log::warn!("sending a phone notification: {error:#}"),
        }
    })
    .detach();
}

/// `text` as a URL query value.
fn url_component(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            byte => format!("%{byte:02X}"),
        })
        .collect()
}

fn start_dispatch(
    hub: Rc<RefCell<Hub>>,
    client: ClientId,
    id: Value,
    prompt: String,
    cx: &mut AsyncApp,
) {
    let (tx, dispatch) = {
        let hub = hub.borrow();
        let Some(tx) = hub.clients.get(&client) else {
            return;
        };
        (tx.clone(), hub.dispatch.clone())
    };
    let Some(dispatch) = dispatch else {
        tx.unbounded_send(error_response(&id, "Dispatch is disabled on this hub"))
            .ok();
        return;
    };
    let shortcut = dispatch::open_latest_project_thread(&hub.borrow().relay, &prompt);
    if let Some(reply) = shortcut {
        let conversation = Conversation::new(hub, "typed", cx.clone());
        conversation.record(Record::Said {
            role: "user",
            text: &prompt,
        });
        conversation.record(Record::Said {
            role: "assistant",
            text: &reply.text,
        });
        conversation.finish(None);
        tx.unbounded_send(result_response(&id, json!(reply))).ok();
        return;
    }
    cx.spawn(async move |cx| {
        let conversation = Conversation::new(hub.clone(), "typed", cx.clone());
        conversation.record(Record::Said {
            role: "user",
            text: &prompt,
        });
        let tools = dispatch_tools(hub, dispatch.consent(), conversation.handle(), cx.clone());
        let record = |record: Record<'_>| conversation.record(record);
        let ask = dispatch.ask(&prompt, None, &tools, &record);
        let timeout = cx.background_executor().timer(dispatch::ASK_TIMEOUT);
        let result = smol::future::race(ask, async move {
            timeout.await;
            Err(anyhow!("Dispatch timed out"))
        })
        .await;
        let reply = match result {
            Ok(reply) => {
                conversation.record(Record::Said {
                    role: "assistant",
                    text: &reply.text,
                });
                conversation.finish(None);
                result_response(&id, json!(reply))
            }
            Err(error) => {
                conversation.finish(Some((
                    &format!("Dispatch couldn't answer: {error:#}"),
                    true,
                )));
                error_response(&id, &format!("{error:#}"))
            }
        };
        tx.unbounded_send(reply).ok();
    })
    .detach();
}

/// Answers a follow-up asked in a saved conversation. The app gets the id of
/// its message at once, then follows the answer as session updates.
fn continue_dispatch(
    hub: Rc<RefCell<Hub>>,
    client: ClientId,
    id: Value,
    session: String,
    prompt: String,
    cx: &mut AsyncApp,
) {
    let (tx, dispatch) = {
        let hub = hub.borrow();
        let Some(tx) = hub.clients.get(&client) else {
            return;
        };
        (tx.clone(), hub.dispatch.clone())
    };
    let Some(dispatch) = dispatch else {
        tx.unbounded_send(error_response(&id, "Dispatch is disabled on this hub"))
            .ok();
        return;
    };
    let context = conversation_context(&hub.borrow().relay, &session);
    let conversation = Conversation::continuing(hub.clone(), session, cx.clone());
    conversation.running();
    let message_id = conversation.message("user_message_chunk", &prompt);
    tx.unbounded_send(result_response(&id, json!({ "messageId": message_id })))
        .ok();
    cx.spawn(async move |cx| {
        let tools = dispatch_tools(hub, dispatch.consent(), conversation.handle(), cx.clone());
        let record = |record: Record<'_>| conversation.record(record);
        let ask = dispatch.ask(&prompt, context.as_deref(), &tools, &record);
        let timeout = cx.background_executor().timer(dispatch::ASK_TIMEOUT);
        let result = smol::future::race(ask, async move {
            timeout.await;
            Err(anyhow!("Dispatch timed out"))
        })
        .await;
        match result {
            Ok(reply) => {
                conversation.record(Record::Said {
                    role: "assistant",
                    text: &reply.text,
                });
                conversation.finish(None);
            }
            Err(error) => conversation.finish(Some((
                &format!("Dispatch couldn't answer: {error:#}"),
                true,
            ))),
        }
    })
    .detach();
}

/// What was said and done so far in a saved conversation, for Dispatch to
/// read before a follow-up. Its own thread may have moved on since, and a
/// call ran in a separate voice session.
fn conversation_context(relay: &Relay, session: &str) -> Option<String> {
    let timeline = relay.dispatch_timeline(session, 60)?;
    let lines: Vec<String> = timeline["entries"]
        .as_array()?
        .iter()
        .filter_map(
            |entry| match (entry["type"].as_str(), entry["role"].as_str()) {
                (Some("message"), Some("user")) => {
                    Some(format!("User: {}", entry["text"].as_str()?))
                }
                (Some("message"), Some("assistant")) => {
                    Some(format!("Dispatch: {}", entry["text"].as_str()?))
                }
                (Some("tool"), _) => Some(format!("Step: {}", entry["title"].as_str()?)),
                _ => None,
            },
        )
        .collect();
    if lines.is_empty() {
        return None;
    }
    let title = timeline["title"]
        .as_str()
        .unwrap_or("Dispatch conversation");
    Some(format!(
        "This continues an earlier conversation with the user, \"{title}\". What was said and done:\n{}",
        crate::relay::tail(&lines.join("\n"), 6_000)
    ))
}

/// The id of the conversation Dispatch is recording, once it has one. Steps
/// that finish after their tool answered (a queued message going out) are
/// added through it.
type ConversationId = Rc<RefCell<Option<String>>>;

/// Saves one Dispatch conversation, a typed question or a call, as a hub
/// session while it happens. Nothing is saved until something happens, so a
/// call that never connects leaves no empty conversation behind.
struct Conversation {
    hub: Rc<RefCell<Hub>>,
    kind: &'static str,
    id: ConversationId,
    titled: Cell<bool>,
    messages: Cell<u64>,
    cx: AsyncApp,
}

impl Conversation {
    fn new(hub: Rc<RefCell<Hub>>, kind: &'static str, cx: AsyncApp) -> Self {
        Self {
            hub,
            kind,
            id: Rc::new(RefCell::new(None)),
            titled: Cell::new(false),
            messages: Cell::new(0),
            cx,
        }
    }

    /// Adds to a saved conversation, after the messages it already has.
    fn continuing(hub: Rc<RefCell<Hub>>, id: String, cx: AsyncApp) -> Self {
        let messages = hub.borrow().relay.dispatch_message_count(&id);
        let kind = if id.starts_with("dispatch-call-") {
            "call"
        } else {
            "typed"
        };
        Self {
            hub,
            kind,
            id: Rc::new(RefCell::new(Some(id))),
            titled: Cell::new(true),
            messages: Cell::new(messages),
            cx,
        }
    }

    fn handle(&self) -> ConversationId {
        self.id.clone()
    }

    fn running(&self) {
        self.update(json!({ "sessionUpdate": "state_update", "state": "running" }));
    }

    fn record(&self, record: Record<'_>) {
        match record {
            Record::Said { role, text } => {
                if role == "user" && !self.titled.replace(true) {
                    self.update(json!({ "sessionUpdate": "session_info_update", "title": conversation_title(text) }));
                }
                let kind = if role == "user" {
                    "user_message_chunk"
                } else {
                    "agent_message_chunk"
                };
                self.message(kind, text);
            }
            Record::Notes(text) => {
                self.message("agent_thought_chunk", text);
            }
            Record::Tool {
                call_id,
                name,
                arguments,
                result,
            } => {
                let update = dispatch::tool_call_update(
                    &self.hub.borrow().relay,
                    call_id,
                    name,
                    arguments,
                    result,
                );
                self.update(update);
            }
            Record::Command(item) => self.update(dispatch::command_update(item)),
        }
    }

    /// Ends the conversation, with a closing note on how it ended.
    fn finish(&self, note: Option<(&str, bool)>) {
        if self.id.borrow().is_none() {
            return;
        }
        if let Some((text, failed)) = note {
            self.update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "end",
                "title": text,
                "kind": "other",
                "status": if failed { "failed" } else { "completed" },
            }));
        }
        self.update(json!({ "sessionUpdate": "state_update", "state": "idle" }));
    }

    fn message(&self, kind: &str, text: &str) -> String {
        let number = self.messages.get() + 1;
        self.messages.set(number);
        let message_id = format!("message-{number}");
        self.update(json!({
            "sessionUpdate": kind,
            "messageId": message_id,
            "content": { "type": "text", "text": text },
        }));
        message_id
    }

    fn update(&self, update: Value) {
        let existing = self.id.borrow().clone();
        let id = match existing {
            Some(id) => id,
            None => {
                let id = format!(
                    "dispatch-{}-{}-{:08x}",
                    self.kind,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_millis())
                        .unwrap_or_default(),
                    rand::random::<u32>(),
                );
                let title = if self.kind == "call" {
                    "Voice call"
                } else {
                    "Typed question"
                };
                let outputs = {
                    let relay = &mut self.hub.borrow_mut().relay;
                    let mut outputs = relay.dispatch_conversation_start(&id, title);
                    // Apps following the conversation show it as working.
                    outputs.extend(relay.dispatch_conversation_record(
                        &id,
                        json!({ "sessionUpdate": "state_update", "state": "running" }),
                    ));
                    outputs
                };
                self.apply(outputs);
                *self.id.borrow_mut() = Some(id.clone());
                id
            }
        };
        let outputs = self
            .hub
            .borrow_mut()
            .relay
            .dispatch_conversation_record(&id, update);
        self.apply(outputs);
    }

    fn apply(&self, outputs: Vec<Output>) {
        let mut cx = self.cx.clone();
        apply(&self.hub, outputs, &mut cx);
    }
}

fn conversation_title(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= 80 {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(79).collect::<String>())
    }
}

const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(300);
const THREAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const SEND_WAIT: Duration = Duration::from_secs(20);
const REPLY_WAIT: Duration = Duration::from_secs(45);
const TURN_START_WAIT: Duration = Duration::from_secs(60);
const REPLY_CHARS: usize = 2_000;
const FOLDER_CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// How long starting a thread holds up Dispatch's answer. Starting goes on
/// after that, and its outcome joins the conversation when it comes.
/// Dispatch has to answer within ASK_TIMEOUT, thinking time included.
const START_WAIT: Duration = Duration::from_secs(25);

/// Dispatch's tools: reads answer at once, and sends and approvals act
/// through the hub.
fn dispatch_tools(
    hub: Rc<RefCell<Hub>>,
    consent: Rc<RefCell<Consent>>,
    conversation: ConversationId,
    cx: AsyncApp,
) -> impl Fn(&str, Value) -> future::LocalBoxFuture<'static, Result<ToolResult>> {
    move |name: &str, arguments: Value| {
        let step = dispatch::call_tool(
            &hub.borrow().relay,
            &mut consent.borrow_mut(),
            name,
            arguments,
        );
        match step {
            Ok(ToolStep::Done(result)) => future::ready(Ok(result)).boxed_local(),
            Ok(ToolStep::Act(action)) => {
                run_action(hub.clone(), action, conversation.clone(), cx.clone()).boxed_local()
            }
            Err(error) => future::ready(Err(error)).boxed_local(),
        }
    }
}

async fn run_action(
    hub: Rc<RefCell<Hub>>,
    action: Action,
    conversation: ConversationId,
    mut cx: AsyncApp,
) -> Result<ToolResult> {
    match action {
        Action::AnswerApproval {
            session,
            approval,
            approve,
        } => {
            let outputs = hub
                .borrow_mut()
                .relay
                .dispatch_answer_approval(&session, &approval, approve)?;
            apply(&hub, outputs, &mut cx);
            let answer = if approve { "approved once" } else { "rejected" };
            Ok(ToolResult::new(json!({ "answer": answer })))
        }
        Action::Send {
            session,
            text,
            wait_for_reply,
        } => send_to_thread(hub, session, text, wait_for_reply, conversation, cx).await,
        Action::Start {
            agent,
            folder,
            text,
            title,
        } => start_thread(hub, agent, folder, text, title, conversation, cx).await,
    }
}

/// Starts a thread and sends it its first message. The work runs on its own,
/// so a slow machine or agent doesn't lose the thread when Dispatch stops
/// waiting.
async fn start_thread(
    hub: Rc<RefCell<Hub>>,
    agent: AgentKey,
    folder: String,
    text: String,
    title: String,
    conversation: ConversationId,
    cx: AsyncApp,
) -> Result<ToolResult> {
    let (done, outcome) = oneshot::channel::<Result<Value>>();
    cx.spawn({
        let hub = hub.clone();
        let agent = agent.clone();
        async move |cx| {
            let started = async {
                let cwd = resolve_folder(&hub, &agent.machine, &folder, cx).await?;
                let session = open_thread(&hub, &agent, &cwd, cx.background_executor()).await?;
                let outputs = hub
                    .borrow_mut()
                    .relay
                    .dispatch_name_session(&session, &title);
                apply(&hub, outputs, cx);
                anyhow::Ok((session, cwd))
            }
            .await;
            let outcome = match started {
                Ok((session, cwd)) => {
                    let sent = send_to_thread(
                        hub.clone(),
                        session.clone(),
                        text,
                        false,
                        conversation.clone(),
                        cx.clone(),
                    )
                    .await;
                    let note = match sent.as_ref().map(|sent| sent.data()["status"].as_str()) {
                        Ok(Some("sent")) => "Started. The agent is working on the message.",
                        Ok(_) => "Started. The message goes out once the agent is ready.",
                        Err(_) => "Started, but the message couldn't be sent. Try tell_session.",
                    };
                    Ok(json!({
                        "status": "started",
                        "sessionId": session,
                        "machine": agent.machine,
                        "agent": agent.agent,
                        "project": cwd,
                        "note": note,
                    }))
                }
                Err(error) => Err(error),
            };
            if let Err(outcome) = done.send(outcome) {
                record_late_start(&hub, &agent, outcome, &conversation, cx);
            }
        }
    })
    .detach();
    let executor = cx.background_executor().clone();
    match within(&executor, START_WAIT, outcome).await {
        Some(Ok(Ok(data))) => {
            let session = data["sessionId"].as_str().unwrap_or_default().to_owned();
            Ok(ToolResult::opening(data, session))
        }
        Some(Ok(Err(error))) => Err(error),
        Some(Err(_)) => bail!("The hub stopped starting the thread"),
        None => Ok(ToolResult::new(json!({
            "status": "starting",
            "machine": agent.machine,
            "agent": agent.agent,
            "note": "Still starting the agent. Tell the user it's starting and don't wait for it: the thread shows up in list_sessions once it's up, and the message goes out then.",
        }))),
    }
}

/// A start that finished after Dispatch stopped waiting, as a step in its
/// conversation.
fn record_late_start(
    hub: &Rc<RefCell<Hub>>,
    agent: &AgentKey,
    outcome: Result<Value>,
    conversation: &ConversationId,
    cx: &mut AsyncApp,
) {
    let call_id = format!("late-{:08x}", rand::random::<u32>());
    let target = format!("a thread on {}", agent.machine);
    let step = match &outcome {
        Ok(data) => dispatch::step(
            &call_id,
            "Started",
            &target,
            "other",
            "completed",
            data["sessionId"].as_str(),
            data["project"].as_str().unwrap_or_default(),
        ),
        Err(error) => {
            log::warn!("Dispatch starting a thread on {}: {error:#}", agent.machine);
            dispatch::step(
                &call_id,
                "Couldn't start",
                &target,
                "other",
                "failed",
                None,
                &format!("{error:#}"),
            )
        }
    };
    let Some(id) = conversation.borrow().clone() else {
        return;
    };
    let outputs = hub
        .borrow_mut()
        .relay
        .dispatch_conversation_record(&id, step);
    apply(hub, outputs, cx);
}

/// Turns `folder` (absolute, or under ~) into the absolute path on
/// `machine`, and checks it's there.
async fn resolve_folder(
    hub: &Rc<RefCell<Hub>>,
    machine: &str,
    folder: &str,
    cx: &mut AsyncApp,
) -> Result<String> {
    // `$1` is the folder as given; only a leading ~ means the home folder.
    const SCRIPT: &str = r#"case $1 in "~") d=$HOME ;; "~/"*) d=$HOME/${1#"~/"} ;; *) d=$1 ;; esac; cd -- "$d" 2>/dev/null && pwd"#;
    let project = cx.update(|cx| hub.borrow_mut().machines.project(machine, cx));
    let project = project
        .await
        .map_err(|error| anyhow!("couldn't reach {machine}: {error:#}"))?;
    let command = AgentServerCommand {
        path: "sh".into(),
        args: vec!["-c".into(), SCRIPT.into(), "sh".into(), folder.into()],
        env: None,
    };
    let mut process = spawn_agent_process(&project, command, cx)
        .with_context(|| format!("checking {folder} on {machine}"))?;
    let line = within(
        cx.background_executor(),
        FOLDER_CHECK_TIMEOUT,
        process.incoming.next(),
    )
    .await
    .with_context(|| format!("checking {folder} on {machine} timed out"))?;
    match line {
        Some(Ok(path)) if path.trim().starts_with('/') => Ok(path.trim().to_owned()),
        _ => bail!("There's no folder {folder} on {machine}"),
    }
}

/// Asks the agent for a new thread in `cwd`, as an app would.
async fn open_thread(
    hub: &Rc<RefCell<Hub>>,
    agent: &AgentKey,
    cwd: &str,
    executor: &BackgroundExecutor,
) -> Result<String> {
    let mut connection = HubConnection::open(hub, Endpoint::Agent(agent.clone()));
    connection
        .request_within(
            executor,
            AGENT_READY_TIMEOUT,
            "initialize",
            json!({ "protocolVersion": 2, "clientCapabilities": {} }),
        )
        .await
        .with_context(|| format!("starting {} on {}", agent.agent, agent.machine))?;
    let result = connection
        .request_within(
            executor,
            THREAD_REQUEST_TIMEOUT,
            "session/new",
            json!({ "cwd": cwd, "mcpServers": [] }),
        )
        .await
        .context("starting the thread")?;
    result["sessionId"]
        .as_str()
        .map(str::to_owned)
        .context("the agent didn't return the new thread's ID")
}

struct ThreadMessage {
    text: String,
    progress: mpsc::UnboundedSender<Delivery>,
}

enum Delivery {
    Queued,
    Sent,
    Replied(String),
    Failed(String),
}

/// Queues a message for a thread and reports how far it got before the
/// tool has to answer. Delivery carries on after that.
async fn send_to_thread(
    hub: Rc<RefCell<Hub>>,
    session: String,
    text: String,
    wait_for_reply: bool,
    conversation: ConversationId,
    cx: AsyncApp,
) -> Result<ToolResult> {
    let (progress, mut updates) = mpsc::unbounded();
    let start_delivery = {
        let mut state = hub.borrow_mut();
        let start = !state.outbox.contains_key(&session);
        state
            .outbox
            .entry(session.clone())
            .or_default()
            .push_back(ThreadMessage { text, progress });
        start
    };
    if start_delivery {
        cx.spawn({
            let hub = hub.clone();
            let session = session.clone();
            async move |cx| deliver_messages(hub, session, cx).await
        })
        .detach();
    }
    let wait = if wait_for_reply {
        REPLY_WAIT
    } else {
        SEND_WAIT
    };
    let deadline = cx.background_executor().timer(wait);
    futures::pin_mut!(deadline);
    let mut status = "sending";
    loop {
        match select(updates.next(), deadline.as_mut()).await {
            Either::Left((Some(Delivery::Queued), _)) => {
                status = "queued";
                if !wait_for_reply {
                    break;
                }
            }
            Either::Left((Some(Delivery::Sent), _)) => {
                status = "sent";
                if !wait_for_reply {
                    break;
                }
            }
            Either::Left((Some(Delivery::Replied(reply)), _)) => {
                return Ok(ToolResult::new(
                    json!({ "status": "answered", "reply": reply }),
                ));
            }
            Either::Left((Some(Delivery::Failed(error)), _)) => bail!("{error}"),
            Either::Left((None, _)) => bail!("The hub stopped delivering the message"),
            Either::Right(_) => break,
        }
    }
    record_late_delivery(&hub, &session, status, updates, conversation, &cx);
    let note = match (status, wait_for_reply) {
        ("queued", _) => {
            "The thread is busy. The message is queued and goes out when its current turn ends."
        }
        ("sent", false) => "Sent. The agent is working on it.",
        ("sent", true) => "Sent, but the agent hasn't answered yet. Check later with read_session.",
        _ => "The thread's agent is starting. The message goes out when it's ready.",
    };
    Ok(ToolResult::new(json!({ "status": status, "note": note })))
}

/// Delivery carries on after the tool answers. What happens later (a queued
/// message going out, the thread's reply, a failure) goes into the
/// conversation as steps of their own.
fn record_late_delivery(
    hub: &Rc<RefCell<Hub>>,
    session: &str,
    status: &'static str,
    mut updates: mpsc::UnboundedReceiver<Delivery>,
    conversation: ConversationId,
    cx: &AsyncApp,
) {
    let hub = hub.clone();
    let session = session.to_owned();
    cx.spawn(async move |cx| {
        let mut sent = status == "sent";
        while let Some(update) = updates.next().await {
            let thread = hub
                .borrow()
                .relay
                .dispatch_title(&session)
                .unwrap_or("the thread")
                .to_owned();
            let call_id = format!("late-{:08x}", rand::random::<u32>());
            let step = match update {
                Delivery::Queued => continue,
                Delivery::Sent if sent => continue,
                Delivery::Sent => {
                    sent = true;
                    dispatch::step(
                        &call_id,
                        "Sent",
                        &format!("the queued message to {thread}"),
                        "other",
                        "completed",
                        Some(&session),
                        "",
                    )
                }
                Delivery::Replied(reply) => dispatch::step(
                    &call_id,
                    "Got",
                    &format!("a reply from {thread}"),
                    "read",
                    "completed",
                    Some(&session),
                    &reply,
                ),
                Delivery::Failed(error) => dispatch::step(
                    &call_id,
                    "Couldn't send",
                    &format!("the message to {thread}"),
                    "other",
                    "failed",
                    Some(&session),
                    &error,
                ),
            };
            let Some(id) = conversation.borrow().clone() else {
                continue;
            };
            let outputs = hub
                .borrow_mut()
                .relay
                .dispatch_conversation_record(&id, step);
            apply(&hub, outputs, cx);
        }
    })
    .detach();
}

async fn deliver_messages(hub: Rc<RefCell<Hub>>, session: String, cx: &mut AsyncApp) {
    let executor = cx.background_executor().clone();
    loop {
        let message = {
            let mut state = hub.borrow_mut();
            match state.outbox.get_mut(&session).and_then(VecDeque::pop_front) {
                Some(message) => message,
                None => {
                    state.outbox.remove(&session);
                    return;
                }
            }
        };
        if let Err(error) = deliver(&hub, &session, &message, &executor).await {
            log::warn!("Dispatch message to {session}: {error:#}");
            message
                .progress
                .unbounded_send(Delivery::Failed(format!("{error:#}")))
                .ok();
        }
    }
}

/// Sends one message once the thread is free, then follows the turn it
/// starts so the next queued message waits for that turn as well.
async fn deliver(
    hub: &Rc<RefCell<Hub>>,
    session: &str,
    message: &ThreadMessage,
    executor: &BackgroundExecutor,
) -> Result<()> {
    if hub.borrow().relay.dispatch_session_busy(session) {
        message.progress.unbounded_send(Delivery::Queued).ok();
        while hub.borrow().relay.dispatch_session_busy(session) {
            executor.timer(Duration::from_secs(1)).await;
        }
    }
    let (key, cwd) = hub
        .borrow()
        .relay
        .dispatch_target(session)
        .context("unknown hub session ID")?;
    let mut connection = HubConnection::open(hub, Endpoint::Agent(key));
    connection
        .request_within(
            executor,
            AGENT_READY_TIMEOUT,
            "initialize",
            json!({ "protocolVersion": 2, "clientCapabilities": {} }),
        )
        .await
        .context("starting the thread's agent")?;
    connection
        .request_within(
            executor,
            THREAD_REQUEST_TIMEOUT,
            "session/resume",
            json!({ "sessionId": session, "cwd": cwd, "mcpServers": [] }),
        )
        .await
        .context("opening the thread")?;
    connection.discard_received();
    connection
        .request_within(
            executor,
            THREAD_REQUEST_TIMEOUT,
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": message.text }] }),
        )
        .await
        .context("sending the message")?;
    message.progress.unbounded_send(Delivery::Sent).ok();
    let reply = follow_turn(&mut connection, hub, session, executor).await?;
    if !reply.is_empty() {
        message
            .progress
            .unbounded_send(Delivery::Replied(reply))
            .ok();
    }
    Ok(())
}

/// Follows the turn a message started and returns the agent's last message.
async fn follow_turn(
    connection: &mut HubConnection,
    hub: &Rc<RefCell<Hub>>,
    session: &str,
    executor: &BackgroundExecutor,
) -> Result<String> {
    let mut reply = String::new();
    let mut reply_id: Option<String> = None;
    let mut started = false;
    let sent_at = std::time::Instant::now();
    loop {
        let Some(message) =
            within(executor, Duration::from_secs(5), connection.next_message()).await
        else {
            if !hub.borrow().relay.dispatch_session_live(session) {
                bail!("The thread's agent stopped before answering");
            }
            if !started && sent_at.elapsed() > TURN_START_WAIT {
                return Ok(String::new());
            }
            continue;
        };
        let message = message?;
        if message["method"] != "session/update"
            || message.pointer("/params/sessionId").and_then(Value::as_str) != Some(session)
        {
            continue;
        }
        let update = &message["params"]["update"];
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                let id = update["messageId"].as_str();
                if id != reply_id.as_deref() {
                    reply_id = id.map(str::to_owned);
                    reply.clear();
                }
                if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
                    reply.push_str(text);
                }
            }
            Some("state_update") => match update["state"].as_str() {
                Some("running") => started = true,
                Some("idle") if started => return Ok(crate::relay::tail(&reply, REPLY_CHARS)),
                _ => {}
            },
            _ => {}
        }
    }
}

async fn within<T>(
    executor: &BackgroundExecutor,
    duration: Duration,
    future: impl Future<Output = T>,
) -> Option<T> {
    let future = std::pin::pin!(future);
    match select(future, executor.timer(duration)).await {
        Either::Left((value, _)) => Some(value),
        Either::Right(_) => None,
    }
}

/// Dispatch's own connection to an agent, which the relay handles like any
/// websocket client's.
struct HubConnection {
    client: ClientId,
    events: mpsc::UnboundedSender<Event>,
    incoming: mpsc::UnboundedReceiver<String>,
    received: VecDeque<Value>,
    next_id: u64,
}

impl HubConnection {
    fn open(hub: &Rc<RefCell<Hub>>, endpoint: Endpoint) -> Self {
        let mut state = hub.borrow_mut();
        let client = state.next_internal_client;
        state.next_internal_client -= 1;
        let (tx, incoming) = mpsc::unbounded();
        state
            .events
            .unbounded_send(Event::ClientConnected {
                client,
                endpoint,
                tx,
            })
            .ok();
        Self {
            client,
            events: state.events.clone(),
            incoming,
            received: VecDeque::new(),
            next_id: 0,
        }
    }

    async fn request_within(
        &mut self,
        executor: &BackgroundExecutor,
        timeout: Duration,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.next_id += 1;
        let id = json!(format!("dispatch-{}", self.next_id));
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.events
            .unbounded_send(Event::ClientLine {
                client: self.client,
                line: request.to_string(),
            })
            .map_err(|_| anyhow!("the hub stopped"))?;
        let response = within(executor, timeout, async {
            loop {
                let message = self.receive().await?;
                if message.get("method").is_none() && message["id"] == id {
                    return anyhow::Ok(message);
                }
                self.received.push_back(message);
            }
        })
        .await
        .with_context(|| format!("{method} timed out"))??;
        if let Some(error) = response.get("error") {
            bail!(
                "{method}: {}",
                error["message"].as_str().unwrap_or("failed")
            );
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn next_message(&mut self) -> Result<Value> {
        match self.received.pop_front() {
            Some(message) => Ok(message),
            None => self.receive().await,
        }
    }

    async fn receive(&mut self) -> Result<Value> {
        let line = self
            .incoming
            .next()
            .await
            .context("the hub closed Dispatch's connection")?;
        Ok(serde_json::from_str(&line)?)
    }

    /// Drops what arrived before a message is sent, so an older turn's end
    /// isn't taken for the new one's.
    fn discard_received(&mut self) {
        self.received.clear();
        while self.incoming.try_recv().is_ok() {}
    }
}

impl Drop for HubConnection {
    fn drop(&mut self) {
        self.events
            .unbounded_send(Event::ClientGone(self.client))
            .ok();
    }
}

fn start_agent(hub: Rc<RefCell<Hub>>, key: AgentKey, cx: &mut AsyncApp) {
    log::info!("starting {} on {}", key.agent, key.machine);
    let project = cx.update(|cx| hub.borrow_mut().machines.project(&key.machine, cx));
    cx.spawn(async move |cx| {
        let started = async {
            let project = project.await.map_err(|error| anyhow!("{error:#}"))?;
            let agent_id = AgentId(key.agent.clone().into());
            wait_for_agent(&project, &agent_id, cx).await?;
            let store = project.read_with(cx, |project, _| project.agent_server_store().clone());
            let command = cx
                .update(|cx| CustomAgentServer::new(agent_id.clone()).command(store, cx))
                .await?;
            spawn_agent_process(&project, command, cx)
        }
        .await;

        match started {
            Ok(process) => {
                let outputs = {
                    let mut hub_state = hub.borrow_mut();
                    hub_state.next_generation += 1;
                    let generation = hub_state.next_generation;
                    let tx = wire_agent(key.clone(), generation, process, &hub_state.events, cx);
                    hub_state.agents.insert(key.clone(), tx);
                    hub_state.relay.agent_started(&key)
                };
                apply(&hub, outputs, cx);
            }
            Err(error) => {
                log::error!("starting {} on {}: {error:#}", key.agent, key.machine);
                let outputs = {
                    let mut hub_state = hub.borrow_mut();
                    hub_state.machines.forget(&key.machine);
                    hub_state
                        .relay
                        .agent_gone(&key, &format!("couldn't start the agent: {error:#}"))
                };
                apply(&hub, outputs, cx);
            }
        }
    })
    .detach();
}

/// Pumps lines between an agent process and the relay.
fn wire_agent(
    key: AgentKey,
    generation: u64,
    process: AgentProcess,
    events: &mpsc::UnboundedSender<Event>,
    cx: &AsyncApp,
) -> RunningAgent {
    let AgentProcess {
        mut incoming,
        mut outgoing,
        stderr,
        guard,
    } = process;
    let (tx, mut rx) = mpsc::unbounded::<String>();
    cx.background_spawn(async move {
        while let Some(line) = rx.next().await {
            if outgoing.send(line).await.is_err() {
                break;
            }
        }
    })
    .detach();
    cx.background_spawn(async move {
        stderr.await.ok();
    })
    .detach();
    let events = events.clone();
    cx.background_spawn(async move {
        while let Some(line) = incoming.next().await {
            match line {
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    events
                        .unbounded_send(Event::AgentLine {
                            key: key.clone(),
                            generation,
                            line: line.to_string(),
                        })
                        .ok();
                }
                Err(_) => break,
            }
        }
        events
            .unbounded_send(Event::AgentGone { key, generation })
            .ok();
    })
    .detach();
    RunningAgent {
        tx,
        generation,
        _guard: guard,
    }
}

/// The agent registry loads in the background; wait until it knows the agent.
pub async fn wait_for_agent(
    project: &Entity<Project>,
    agent_id: &AgentId,
    cx: &mut AsyncApp,
) -> Result<()> {
    for _ in 0..240 {
        let known = project.read_with(cx, |project, cx| {
            project
                .agent_server_store()
                .read(cx)
                .external_agents()
                .any(|id| id == agent_id)
        });
        if known {
            return Ok(());
        }
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
    }
    anyhow::bail!("agent {agent_id} isn't in the hub's settings or the agent registry")
}

async fn accept_loop(
    listener: smol::net::TcpListener,
    token: String,
    events: mpsc::UnboundedSender<Event>,
) {
    let next_client = Arc::new(AtomicU64::new(1));
    loop {
        let Ok((stream, address)) = listener.accept().await else {
            continue;
        };
        let token = token.clone();
        let events = events.clone();
        let client = next_client.fetch_add(1, Ordering::SeqCst);
        smol::spawn(async move {
            if let Err(error) = handle_client(client, stream, token, events).await {
                log::info!("client {client} from {address}: {error:#}");
            }
        })
        .detach();
    }
}

#[allow(
    clippy::result_large_err,
    reason = "tungstenite's handshake callback returns its own error response type"
)]
async fn handle_client(
    client: ClientId,
    stream: smol::net::TcpStream,
    token: String,
    events: mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let endpoint = Arc::new(std::sync::Mutex::new(None));
    let websocket = {
        let endpoint = endpoint.clone();
        async_tungstenite::accept_hdr_async(stream, move |request: &Request, response: Response| {
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok());
            let path = request
                .uri()
                .path_and_query()
                .map(|path| path.as_str())
                .unwrap_or_default();
            match check_request(path, authorization, &token) {
                Ok(found) => {
                    *endpoint
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(found);
                    Ok(response)
                }
                Err((status, message)) => {
                    let mut error = ErrorResponse::new(Some(message.to_string()));
                    *error.status_mut() = status;
                    Err(error)
                }
            }
        })
        .await?
    };
    let endpoint = endpoint
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .context("no endpoint after the handshake")?;
    let (mut sink, mut stream) = websocket.split();
    let (tx, mut rx) = mpsc::unbounded::<String>();
    events
        .unbounded_send(Event::ClientConnected {
            client,
            endpoint,
            tx,
        })
        .ok();
    let writer = smol::spawn(async move {
        while let Some(line) = rx.next().await {
            if sink.send(Message::text(line)).await.is_err() {
                break;
            }
        }
    });
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Text(text)) => {
                events
                    .unbounded_send(Event::ClientLine {
                        client,
                        line: text.to_string(),
                    })
                    .ok();
            }
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    events.unbounded_send(Event::ClientGone(client)).ok();
    writer.cancel().await;
    Ok(())
}

/// Checks the token and works out what a client connected to:
/// `/hub` or `/acp/<machine>/<agent>`, with `?token=` or a bearer header.
pub fn check_request(
    path_and_query: &str,
    authorization: Option<&str>,
    token: &str,
) -> std::result::Result<Endpoint, (StatusCode, &'static str)> {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let query_token = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="));
    let header_token = authorization.and_then(|value| value.strip_prefix("Bearer "));
    let given = query_token.or(header_token).unwrap_or_default();
    if given.is_empty() || !constant_time_eq(given.as_bytes(), token.as_bytes()) {
        return Err((StatusCode::UNAUTHORIZED, "missing or wrong hub token"));
    }
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["hub"] => Ok(Endpoint::Hub),
        ["acp", machine, agent] if !machine.is_empty() && !agent.is_empty() => {
            Ok(Endpoint::Agent(AgentKey {
                machine: percent_decode(machine),
                agent: percent_decode(agent),
            }))
        }
        _ => Err((StatusCode::NOT_FOUND, "use /hub or /acp/<machine>/<agent>")),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Agent ids like `claude-acp@personal` may arrive percent-encoded.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_need_the_token() {
        assert_eq!(
            check_request("/hub?token=abc", None, "abc"),
            Ok(Endpoint::Hub)
        );
        assert_eq!(
            check_request("/hub", Some("Bearer abc"), "abc"),
            Ok(Endpoint::Hub)
        );
        assert_eq!(
            check_request("/hub?token=abd", None, "abc").unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            check_request("/hub", None, "abc").unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn agent_endpoints_name_a_machine_and_an_agent() {
        assert_eq!(
            check_request("/acp/gpu-07/claude-acp%40personal?token=t", None, "t"),
            Ok(Endpoint::Agent(AgentKey {
                machine: "gpu-07".into(),
                agent: "claude-acp@personal".into(),
            }))
        );
        assert_eq!(
            check_request("/acp/gpu-07?token=t", None, "t")
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
    }
}
