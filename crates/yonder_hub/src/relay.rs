//! Routes ACP messages between the hub's clients (Macs, phones, the CLI) and
//! the agents it runs.
//!
//! This is a plain state machine: messages go in, messages to send come out.
//! It knows nothing about sockets or processes, so the routing rules can be
//! tested directly:
//!
//! - Requests from clients get a hub request id; the agent's response goes back
//!   to the client that asked, under its own id.
//! - `session/update` notifications are logged and sent to every client
//!   attached to the session.
//! - Requests from the agent (approvals) go to every attached client and the
//!   first answer wins. With nobody attached they wait for the next client.
//! - A client that asks for history (`session/load`, or `session/resume` with
//!   `replayFrom`) gets it exactly once: from the hub's log when the hub has
//!   one, otherwise from the agent, which the hub then logs.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dispatch::{ASK_METHOD, MAX_PROMPT_BYTES, SessionFilter};

pub type ClientId = u64;

/// One agent on one machine; the hub runs one process for each.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentKey {
    pub machine: String,
    pub agent: String,
}

/// What a client connected to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Agent(AgentKey),
    /// Hub-wide methods such as listing every session.
    Hub,
}

/// Something the runtime has to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    ToClient(ClientId, String),
    ToAgent(AgentKey, String),
    StartAgent(AgentKey),
    AskDispatch {
        client: ClientId,
        id: Value,
        prompt: String,
    },
    SaveSessions,
    AppendLog {
        session: String,
        line: String,
    },
}

/// A session as the hub lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub machine: String,
    pub agent: String,
    pub cwd: String,
    #[serde(default)]
    pub title: Option<String>,
    /// `running`, `idle`, `waiting` (for an approval) or whatever the agent's
    /// last state update said.
    #[serde(default)]
    pub state: String,
    /// The end of the agent's last message.
    #[serde(default)]
    pub last_text: String,
    pub created_at: u64,
    pub updated_at: u64,
}

const LAST_TEXT_CHARS: usize = 400;

struct Client {
    endpoint: Endpoint,
    sessions: HashSet<String>,
}

#[derive(PartialEq)]
enum AgentStatus {
    Stopped,
    Starting,
    Ready,
}

struct Agent {
    status: AgentStatus,
    init_result: Option<Value>,
    /// Clients whose `initialize` waits for the agent to start.
    waiting: Vec<(ClientId, Value)>,
    /// Requests from the agent that nobody has answered yet.
    pending: Vec<PendingRequest>,
}

struct PendingRequest {
    id: Value,
    session: Option<String>,
    line: String,
}

struct Session {
    meta: SessionMeta,
    /// The agent's answer to `session/new` or `session/resume`, given again
    /// to clients that attach later.
    setup: Option<Value>,
    log: Vec<String>,
    /// Whether the running agent process knows this session.
    live: bool,
    attached: HashSet<ClientId>,
    message_id: Option<String>,
    message_text: String,
}

enum Route {
    Client {
        client: ClientId,
        agent: AgentKey,
        id: Value,
        kind: RouteKind,
    },
    Initialize(AgentKey),
}

enum RouteKind {
    Plain,
    New {
        cwd: String,
    },
    /// `replay`: whether the hub replays its own log after the agent's answer.
    /// When it doesn't, either nobody asked for history or the agent replays it.
    Resume {
        session: String,
        replay: bool,
    },
}

pub struct Relay {
    clock: Box<dyn Fn() -> u64>,
    next_request: u64,
    clients: HashMap<ClientId, Client>,
    agents: HashMap<AgentKey, Agent>,
    sessions: HashMap<String, Session>,
    routes: HashMap<String, Route>,
    machines: Vec<String>,
}

impl Relay {
    pub fn new(clock: impl Fn() -> u64 + 'static) -> Self {
        Self {
            clock: Box::new(clock),
            next_request: 0,
            clients: HashMap::default(),
            agents: HashMap::default(),
            sessions: HashMap::default(),
            routes: HashMap::default(),
            machines: Vec::new(),
        }
    }

    /// The machines clients can start agents on.
    pub fn set_machines(&mut self, machines: Vec<String>) {
        self.machines = machines;
    }

    /// Sessions saved before a restart. The agents don't know them until a
    /// client resumes them.
    pub fn restore(&mut self, sessions: Vec<(SessionMeta, Vec<String>)>) {
        for (meta, log) in sessions {
            self.sessions.insert(
                meta.id.clone(),
                Session {
                    meta,
                    setup: None,
                    log,
                    live: false,
                    attached: HashSet::default(),
                    message_id: None,
                    message_text: String::new(),
                },
            );
        }
    }

    /// Every session, most recently updated first.
    pub fn sessions(&self) -> Vec<SessionMeta> {
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .map(|session| session.meta.clone())
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        sessions
    }

    pub fn dispatch_sessions(&self, filter: SessionFilter, limit: usize) -> Value {
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .filter(|session| filter.matches(dispatch_state(session)))
            .collect();
        sessions.sort_unstable_by_key(|session| {
            (
                !SessionFilter::Active.matches(dispatch_state(session)),
                std::cmp::Reverse(session.meta.updated_at),
                session.meta.id.as_str(),
            )
        });
        let total = sessions.len();
        let sessions: Vec<_> = sessions
            .into_iter()
            .take(limit)
            .map(|session| {
                let meta = &session.meta;
                json!({
                    "id": meta.id,
                    "title": meta.title.as_deref().map(|text| tail(text, 160)),
                    "machine": meta.machine,
                    "agent": meta.agent,
                    "project": meta.cwd,
                    "state": dispatch_state(session),
                    "createdAt": meta.created_at,
                    "updatedAt": meta.updated_at,
                    "lastText": meta.last_text,
                })
            })
            .collect();
        json!({ "sessions": sessions, "total": total })
    }

    pub fn latest_project_session(&self, project: &str) -> Option<&SessionMeta> {
        let normalize = |name: &str| {
            name.split(|character: char| !character.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        };
        let name = normalize(project);
        if name.is_empty() {
            return None;
        }
        self.sessions
            .values()
            .map(|session| &session.meta)
            .filter(|session| {
                if project.contains('/') {
                    session.cwd.trim_end_matches('/') == project.trim_end_matches('/')
                } else {
                    Path::new(&session.cwd)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|directory| normalize(directory) == name)
                }
            })
            .max_by_key(|meta| (meta.created_at, meta.updated_at, meta.id.as_str()))
    }

    pub fn dispatch_session_now(&self, id: &str) -> Option<Value> {
        let session = self.sessions.get(id)?;
        let meta = &session.meta;
        let state = dispatch_state(session);
        let pending_approval = (state == "waiting")
            .then(|| self.pending_approval(id))
            .flatten()
            .map(|(_, pending, request)| describe_approval(pending, &request));
        let recent_tool = SessionFilter::Active
            .matches(state)
            .then(|| recent_dispatch_tool(&session.log))
            .flatten();
        Some(json!({
            "id": id,
            "state": state,
            "updatedAt": meta.updated_at,
            "lastText": meta.last_text,
            "recentTool": recent_tool,
            "pendingApproval": pending_approval,
        }))
    }

    pub fn dispatch_history(&self, id: &str, limit: usize) -> Option<Value> {
        let session = self.sessions.get(id)?;
        let mut messages: Vec<(&str, Option<String>, String)> = Vec::new();
        let mut remaining_chars = 6_000;
        let mut after_turn_boundary = false;
        for line in session.log.iter().rev().take(2_048) {
            let Ok(message) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(update) = message.pointer("/params/update") else {
                continue;
            };
            let role = match update.get("sessionUpdate").and_then(Value::as_str) {
                Some("user_message_chunk") => "user",
                Some("agent_message_chunk") => "assistant",
                Some("state_update") if update["state"] == "idle" => {
                    after_turn_boundary = true;
                    continue;
                }
                _ => continue,
            };
            let Some(text) = update.pointer("/content/text").and_then(Value::as_str) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let message_id = update.get("messageId").and_then(Value::as_str);
            if after_turn_boundary
                || messages.last().is_none_or(|(last_role, last_id, _)| {
                    *last_role != role || last_id.as_deref() != message_id
                })
            {
                if messages.len() == limit {
                    break;
                }
                messages.push((role, message_id.map(str::to_owned), String::new()));
            }
            after_turn_boundary = false;
            if let Some((_, _, message)) = messages.last_mut() {
                let available =
                    remaining_chars.min(1_200_usize.saturating_sub(message.chars().count()));
                let chunk = tail(text, available);
                remaining_chars -= chunk.chars().count();
                message.insert_str(0, &chunk);
            }
            if remaining_chars == 0 {
                break;
            }
        }
        let messages: Vec<_> = messages
            .into_iter()
            .map(|(role, _, text)| json!({ "role": role, "text": text }))
            .collect();
        Some(json!({
            "id": id,
            "title": session.meta.title.as_deref().map(|text| tail(text, 160)),
            "messages": messages,
            "order": "newest_first",
        }))
    }

    pub fn dispatch_has_session(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    /// The agent that runs a session and the session's project directory.
    pub fn dispatch_target(&self, id: &str) -> Option<(AgentKey, String)> {
        let meta = &self.sessions.get(id)?.meta;
        Some((
            AgentKey {
                machine: meta.machine.clone(),
                agent: meta.agent.clone(),
            },
            meta.cwd.clone(),
        ))
    }

    /// Whether a running agent is working on the session or waiting for an
    /// approval in it.
    pub fn dispatch_session_busy(&self, id: &str) -> bool {
        self.sessions
            .get(id)
            .is_some_and(|session| SessionFilter::Active.matches(dispatch_state(session)))
    }

    pub fn dispatch_session_live(&self, id: &str) -> bool {
        self.sessions.get(id).is_some_and(|session| session.live)
    }

    /// Answers the approval a session waits on with the agent's one-time
    /// option, never an "always" one.
    pub fn dispatch_answer_approval(
        &mut self,
        session_id: &str,
        approval_id: &str,
        approve: bool,
    ) -> Result<Vec<Output>> {
        if !self.sessions.contains_key(session_id) {
            bail!("unknown hub session ID");
        }
        let (key, pending, request) = self
            .pending_approval(session_id)
            .filter(|(_, pending, _)| approval_id_of(&pending.id) == approval_id)
            .context("That approval is no longer waiting. Check the thread again.")?;
        let kind = if approve { "allow_once" } else { "reject_once" };
        let option = request
            .pointer("/params/options")
            .and_then(Value::as_array)
            .and_then(|options| options.iter().find(|option| option["kind"] == kind))
            .and_then(|option| option["optionId"].as_str())
            .with_context(|| format!("The agent doesn't offer a {kind} answer"))?;
        let id = pending.id.clone();
        let answer = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "outcome": { "outcome": "selected", "optionId": option } },
        });
        Ok(self.client_response(key, id, &answer.to_string()))
    }

    /// The newest approval request a session waits on.
    fn pending_approval(&self, id: &str) -> Option<(AgentKey, &PendingRequest, Value)> {
        let (key, _) = self.dispatch_target(id)?;
        let agent = self.agents.get(&key)?;
        agent.pending.iter().rev().find_map(|pending| {
            if pending.session.as_deref() != Some(id) {
                return None;
            }
            let request: Value = serde_json::from_str(&pending.line).ok()?;
            request
                .get("method")?
                .as_str()?
                .contains("request_permission")
                .then(|| (key.clone(), pending, request))
        })
    }

    pub fn client_connected(&mut self, client: ClientId, endpoint: Endpoint) {
        self.clients.insert(
            client,
            Client {
                endpoint,
                sessions: HashSet::default(),
            },
        );
    }

    pub fn client_gone(&mut self, client: ClientId) {
        if let Some(gone) = self.clients.remove(&client) {
            for session in gone.sessions {
                if let Some(session) = self.sessions.get_mut(&session) {
                    session.attached.remove(&client);
                }
            }
        }
        for agent in self.agents.values_mut() {
            agent.waiting.retain(|(waiting, _)| *waiting != client);
        }
        self.routes.retain(
            |_, route| !matches!(route, Route::Client { client: owner, .. } if *owner == client),
        );
    }

    pub fn client_message(&mut self, client: ClientId, line: &str) -> Vec<Output> {
        let Some(endpoint) = self.clients.get(&client).map(|c| c.endpoint.clone()) else {
            return Vec::new();
        };
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = message.get("id").cloned();
        match (endpoint, method.as_deref(), id) {
            (Endpoint::Hub, Some(method), Some(id)) => {
                self.hub_request(client, method, id, message.get("params"))
            }
            (Endpoint::Hub, _, _) => Vec::new(),
            (Endpoint::Agent(agent), Some(method), Some(id)) => {
                self.client_request(client, agent, method, id, message)
            }
            (Endpoint::Agent(agent), Some(method), None) => {
                self.client_notification(client, agent, method, message)
            }
            (Endpoint::Agent(agent), None, Some(id)) => self.client_response(agent, id, line),
            _ => Vec::new(),
        }
    }

    pub fn agent_started(&mut self, key: &AgentKey) -> Vec<Output> {
        let id = self.next_request_id();
        self.routes
            .insert(id.clone(), Route::Initialize(key.clone()));
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": 2,
                "info": { "name": "yonder-hub", "title": "Yonder Hub", "version": env!("CARGO_PKG_VERSION") },
                // No fs or terminal: agents use their own file and shell tools,
                // so they keep working with no client attached.
                "capabilities": {},
            },
        });
        vec![Output::ToAgent(key.clone(), request.to_string())]
    }

    /// The agent process exited or couldn't start.
    pub fn agent_gone(&mut self, key: &AgentKey, reason: &str) -> Vec<Output> {
        let mut outputs = Vec::new();
        if let Some(agent) = self.agents.get_mut(key) {
            agent.status = AgentStatus::Stopped;
            agent.init_result = None;
            agent.pending.clear();
            for (client, id) in agent.waiting.drain(..) {
                outputs.push(Output::ToClient(client, error_response(&id, reason)));
            }
        }
        for session in self.sessions.values_mut() {
            if session.meta.machine == key.machine && session.meta.agent == key.agent {
                session.live = false;
            }
        }
        let failed: Vec<String> = self
            .routes
            .iter()
            .filter(|(_, route)| match route {
                Route::Client { agent, .. } => agent == key,
                Route::Initialize(agent) => agent == key,
            })
            .map(|(id, _)| id.clone())
            .collect();
        for route_id in failed {
            if let Some(Route::Client { client, id, .. }) = self.routes.remove(&route_id) {
                outputs.push(Output::ToClient(client, error_response(&id, reason)));
            }
        }
        outputs
    }

    pub fn agent_line(&mut self, key: &AgentKey, line: &str) -> Vec<Output> {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            (None, Some(id)) => self.agent_response(key, id, message),
            (Some(_), Some(id)) => self.agent_request(key, id, &message, line),
            (Some(method), None) => self.agent_notification(key, method, &message, line),
            _ => Vec::new(),
        }
    }

    fn hub_request(
        &mut self,
        client: ClientId,
        method: &str,
        id: Value,
        params: Option<&Value>,
    ) -> Vec<Output> {
        let mut outputs = Vec::new();
        let result = match method {
            "initialize" => json!({
                "protocolVersion": 2,
                "info": { "name": "yonder-hub", "title": "Yonder Hub" },
                "capabilities": {},
            }),
            "_yonder/hub/sessions" => json!({ "sessions": self.sessions() }),
            ASK_METHOD => {
                let prompt = params
                    .and_then(|params| params.get("prompt"))
                    .and_then(Value::as_str)
                    .map(str::trim);
                if let Some(prompt) =
                    prompt.filter(|prompt| !prompt.is_empty() && prompt.len() <= MAX_PROMPT_BYTES)
                {
                    return vec![Output::AskDispatch {
                        client,
                        id,
                        prompt: prompt.to_owned(),
                    }];
                }
                return vec![Output::ToClient(
                    client,
                    error_response_with_code(
                        &id,
                        -32602,
                        "Dispatch requires a nonempty prompt of at most 8192 bytes",
                    ),
                )];
            }
            "_yonder/hub/machines" => json!({ "machines": self.machines }),
            "_yonder/hub/import" => {
                let sessions = params
                    .and_then(|params| params.get("sessions"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let sessions = match serde_json::from_value::<Vec<ImportedSession>>(sessions) {
                    Ok(sessions) => sessions,
                    Err(error) => {
                        return vec![Output::ToClient(
                            client,
                            error_response_with_code(
                                &id,
                                -32602,
                                &format!("invalid sessions: {error}"),
                            ),
                        )];
                    }
                };
                let (imported, corrected) = self.import(sessions);
                if imported > 0 || corrected {
                    outputs.push(Output::SaveSessions);
                }
                json!({ "imported": imported })
            }
            _ => {
                return vec![Output::ToClient(
                    client,
                    error_response_with_code(&id, -32601, &format!("unknown method {method}")),
                )];
            }
        };
        outputs.push(Output::ToClient(client, result_response(&id, result)));
        outputs
    }

    /// Registers client sessions and recovers creation times for older imports.
    fn import(&mut self, sessions: Vec<ImportedSession>) -> (usize, bool) {
        let now = (self.clock)();
        let mut imported = 0;
        let mut corrected = false;
        for session in sessions {
            if let Some(existing) = self.sessions.get_mut(&session.id) {
                if existing.meta.machine == session.machine
                    && existing.meta.agent == session.agent
                    && let Some(created_at) = session.created_at
                    && created_at < existing.meta.created_at
                {
                    existing.meta.created_at = created_at;
                    corrected = true;
                }
                continue;
            }
            let updated_at = session.updated_at.or(session.created_at).unwrap_or(now);
            let created_at = session.created_at.unwrap_or(updated_at).min(updated_at);
            self.sessions.insert(
                session.id.clone(),
                Session {
                    meta: SessionMeta {
                        id: session.id,
                        machine: session.machine,
                        agent: session.agent,
                        cwd: session.cwd,
                        title: session.title,
                        state: "idle".to_string(),
                        last_text: String::new(),
                        created_at,
                        updated_at,
                    },
                    setup: None,
                    log: Vec::new(),
                    live: false,
                    attached: HashSet::default(),
                    message_id: None,
                    message_text: String::new(),
                },
            );
            imported += 1;
        }
        (imported, corrected)
    }

    fn client_request(
        &mut self,
        client: ClientId,
        agent: AgentKey,
        method: &str,
        id: Value,
        mut message: Value,
    ) -> Vec<Output> {
        let entry = self.agents.entry(agent.clone()).or_insert_with(|| Agent {
            status: AgentStatus::Stopped,
            init_result: None,
            waiting: Vec::new(),
            pending: Vec::new(),
        });

        if method == "initialize" {
            if let Some(result) = &entry.init_result {
                return vec![Output::ToClient(
                    client,
                    result_response(&id, result.clone()),
                )];
            }
            entry.waiting.push((client, id));
            if entry.status == AgentStatus::Stopped {
                entry.status = AgentStatus::Starting;
                return vec![Output::StartAgent(agent)];
            }
            return Vec::new();
        }

        if entry.status != AgentStatus::Ready {
            let start = entry.status == AgentStatus::Stopped;
            if start {
                entry.status = AgentStatus::Starting;
            }
            let mut outputs = vec![Output::ToClient(
                client,
                error_response(&id, "the agent is starting; initialize first"),
            )];
            if start {
                outputs.push(Output::StartAgent(agent));
            }
            return outputs;
        }

        let session_id = message
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut outputs = Vec::new();
        let kind = match (method, &session_id) {
            ("session/new", _) => RouteKind::New {
                cwd: message
                    .pointer("/params/cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            ("session/resume" | "session/load", Some(session_id)) => {
                let load = method == "session/load";
                let wants_history = load
                    || message
                        .pointer("/params/replayFrom")
                        .is_some_and(|replay_from| !replay_from.is_null());
                if let Some(session) = self.sessions.get(session_id)
                    && session.live
                {
                    return self.attach_live(client, session_id.clone(), &id, wants_history);
                }
                let cwd = message
                    .pointer("/params/cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let now = (self.clock)();
                let session = self
                    .sessions
                    .entry(session_id.clone())
                    .or_insert_with(|| Session {
                        meta: SessionMeta {
                            id: session_id.clone(),
                            machine: agent.machine.clone(),
                            agent: agent.agent.clone(),
                            cwd,
                            title: None,
                            state: "idle".to_string(),
                            last_text: String::new(),
                            created_at: now,
                            updated_at: now,
                        },
                        setup: None,
                        log: Vec::new(),
                        live: false,
                        attached: HashSet::default(),
                        message_id: None,
                        message_text: String::new(),
                    });
                let replay = if load {
                    // `session/load` always makes the agent replay the history.
                    session.log.clear();
                    false
                } else if wants_history && !session.log.is_empty() {
                    // The hub replays its own log, so the agent mustn't as well.
                    if let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) {
                        params.remove("replayFrom");
                    }
                    true
                } else {
                    // Either no history was asked for, or the hub has none (an
                    // imported session) and the agent's replay gets logged.
                    false
                };
                self.attach(client, session_id);
                RouteKind::Resume {
                    session: session_id.clone(),
                    replay,
                }
            }
            (_, Some(session_id)) => {
                if self.sessions.contains_key(session_id) {
                    self.attach(client, session_id);
                }
                RouteKind::Plain
            }
            _ => RouteKind::Plain,
        };

        let route_id = self.next_request_id();
        message["id"] = Value::String(route_id.clone());
        self.routes.insert(
            route_id,
            Route::Client {
                client,
                agent: agent.clone(),
                id,
                kind,
            },
        );
        outputs.push(Output::ToAgent(agent, message.to_string()));
        outputs
    }

    fn client_notification(
        &mut self,
        client: ClientId,
        agent: AgentKey,
        method: &str,
        mut message: Value,
    ) -> Vec<Output> {
        if method == "$/cancel_request" {
            let Some(client_request) = message.pointer("/params/requestId").cloned() else {
                return Vec::new();
            };
            let route_id = self
                .routes
                .iter()
                .find_map(|(route_id, route)| match route {
                    Route::Client {
                        client: owner, id, ..
                    } if *owner == client && *id == client_request => Some(route_id.clone()),
                    _ => None,
                });
            let Some(route_id) = route_id else {
                return Vec::new();
            };
            message["params"]["requestId"] = Value::String(route_id);
        }
        if let Some(session_id) = message.pointer("/params/sessionId").and_then(Value::as_str)
            && self.sessions.contains_key(session_id)
        {
            let session_id = session_id.to_string();
            self.attach(client, &session_id);
        }
        vec![Output::ToAgent(agent, message.to_string())]
    }

    /// A client answering one of the agent's requests. The first answer wins.
    fn client_response(&mut self, agent: AgentKey, id: Value, line: &str) -> Vec<Output> {
        let Some(entry) = self.agents.get_mut(&agent) else {
            return Vec::new();
        };
        let Some(index) = entry.pending.iter().position(|pending| pending.id == id) else {
            return Vec::new();
        };
        let answered = entry.pending.remove(index);
        let mut outputs = vec![Output::ToAgent(agent, line.to_string())];
        if let Some(session_id) = answered.session
            && let Some(session) = self.sessions.get_mut(&session_id)
            && session.meta.state == "waiting"
        {
            session.meta.state = "running".to_string();
            session.meta.updated_at = (self.clock)();
            outputs.push(Output::SaveSessions);
        }
        outputs
    }

    fn agent_response(&mut self, key: &AgentKey, id: Value, message: Value) -> Vec<Output> {
        let Some(route_id) = id.as_str() else {
            return Vec::new();
        };
        match self.routes.remove(route_id) {
            Some(Route::Initialize(agent)) => {
                let Some(entry) = self.agents.get_mut(&agent) else {
                    return Vec::new();
                };
                let waiting = std::mem::take(&mut entry.waiting);
                match message.get("result") {
                    Some(result) => {
                        entry.status = AgentStatus::Ready;
                        entry.init_result = Some(result.clone());
                        waiting
                            .into_iter()
                            .map(|(client, id)| {
                                Output::ToClient(client, result_response(&id, result.clone()))
                            })
                            .collect()
                    }
                    None => {
                        entry.status = AgentStatus::Stopped;
                        let error = message.get("error").cloned().unwrap_or(Value::Null);
                        waiting
                            .into_iter()
                            .map(|(client, id)| {
                                Output::ToClient(
                                    client,
                                    json!({"jsonrpc": "2.0", "id": id, "error": error}).to_string(),
                                )
                            })
                            .collect()
                    }
                }
            }
            Some(Route::Client {
                client, id, kind, ..
            }) => {
                let mut outputs = Vec::new();
                let result = message.get("result").cloned();
                match (kind, &result) {
                    (RouteKind::New { cwd }, Some(result)) => {
                        if let Some(session_id) = result.get("sessionId").and_then(Value::as_str) {
                            let now = (self.clock)();
                            self.sessions.insert(
                                session_id.to_string(),
                                Session {
                                    meta: SessionMeta {
                                        id: session_id.to_string(),
                                        machine: key.machine.clone(),
                                        agent: key.agent.clone(),
                                        cwd,
                                        title: None,
                                        state: "idle".to_string(),
                                        last_text: String::new(),
                                        created_at: now,
                                        updated_at: now,
                                    },
                                    setup: Some(result.clone()),
                                    log: Vec::new(),
                                    live: true,
                                    attached: HashSet::default(),
                                    message_id: None,
                                    message_text: String::new(),
                                },
                            );
                            self.attach(client, session_id);
                            outputs.push(Output::SaveSessions);
                        }
                    }
                    (RouteKind::Resume { session, replay }, Some(result)) => {
                        if let Some(entry) = self.sessions.get_mut(&session) {
                            entry.live = true;
                            entry.setup = Some(result.clone());
                            if replay {
                                outputs.extend(
                                    entry
                                        .log
                                        .iter()
                                        .map(|line| Output::ToClient(client, line.clone())),
                                );
                            }
                        }
                        outputs.push(Output::SaveSessions);
                    }
                    (RouteKind::Resume { session, .. }, None) => {
                        self.detach(client, &session);
                    }
                    _ => {}
                }
                let mut response = message;
                response["id"] = id;
                outputs.push(Output::ToClient(client, response.to_string()));
                outputs
            }
            None => Vec::new(),
        }
    }

    fn agent_request(
        &mut self,
        key: &AgentKey,
        id: Value,
        message: &Value,
        line: &str,
    ) -> Vec<Output> {
        let session_id = message
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut outputs = self.recipients(key, session_id.as_deref());
        for output in &mut outputs {
            if let Output::ToClient(_, text) = output {
                *text = line.to_string();
            }
        }
        if let Some(agent) = self.agents.get_mut(key) {
            agent.pending.push(PendingRequest {
                id,
                session: session_id.clone(),
                line: line.to_string(),
            });
        }
        let is_approval = message
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| method.contains("request_permission"));
        if is_approval
            && let Some(session_id) = session_id
            && let Some(session) = self.sessions.get_mut(&session_id)
        {
            session.meta.state = "waiting".to_string();
            session.meta.updated_at = (self.clock)();
            outputs.push(Output::SaveSessions);
        }
        outputs
    }

    fn agent_notification(
        &mut self,
        key: &AgentKey,
        method: &str,
        message: &Value,
        line: &str,
    ) -> Vec<Output> {
        if method == "$/cancel_request"
            && let Some(request) = message.pointer("/params/requestId")
            && let Some(agent) = self.agents.get_mut(key)
        {
            agent.pending.retain(|pending| pending.id != *request);
        }
        let session_id = message
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut outputs = Vec::new();
        if method == "session/update"
            && let Some(session_id) = &session_id
            && let Some(session) = self.sessions.get_mut(session_id)
        {
            session.log.push(line.to_string());
            outputs.push(Output::AppendLog {
                session: session_id.clone(),
                line: line.to_string(),
            });
            if let Some(update) = message.pointer("/params/update")
                && apply_update(session, update, (self.clock)())
            {
                outputs.push(Output::SaveSessions);
            }
        }
        let mut recipients = self.recipients(key, session_id.as_deref());
        for output in &mut recipients {
            if let Output::ToClient(_, text) = output {
                *text = line.to_string();
            }
        }
        outputs.extend(recipients);
        outputs
    }

    /// The clients a message about `session` goes to: those attached to it,
    /// or every client of the agent when it isn't about a session.
    fn recipients(&self, key: &AgentKey, session: Option<&str>) -> Vec<Output> {
        let clients: Vec<ClientId> = match session.and_then(|id| self.sessions.get(id)) {
            Some(session) => session.attached.iter().copied().collect(),
            None if session.is_some() => Vec::new(),
            None => self
                .clients
                .iter()
                .filter(|(_, client)| client.endpoint == Endpoint::Agent(key.clone()))
                .map(|(id, _)| *id)
                .collect(),
        };
        let mut clients = clients;
        clients.sort_unstable();
        clients
            .into_iter()
            .map(|client| Output::ToClient(client, String::new()))
            .collect()
    }

    /// Attaches a client to a session the agent already runs: the log if the
    /// client asked for history, then the setup answer, then any approvals
    /// still waiting.
    fn attach_live(
        &mut self,
        client: ClientId,
        session_id: String,
        id: &Value,
        wants_history: bool,
    ) -> Vec<Output> {
        self.attach(client, &session_id);
        let Some(session) = self.sessions.get(&session_id) else {
            return Vec::new();
        };
        let mut outputs: Vec<Output> = Vec::new();
        if wants_history {
            outputs.extend(
                session
                    .log
                    .iter()
                    .map(|line| Output::ToClient(client, line.clone())),
            );
        }
        let setup = session.setup.clone().unwrap_or_else(|| json!({}));
        outputs.push(Output::ToClient(client, result_response(id, setup)));
        let key = AgentKey {
            machine: session.meta.machine.clone(),
            agent: session.meta.agent.clone(),
        };
        if let Some(agent) = self.agents.get(&key) {
            outputs.extend(
                agent
                    .pending
                    .iter()
                    .filter(|pending| pending.session.as_deref() == Some(session_id.as_str()))
                    .map(|pending| Output::ToClient(client, pending.line.clone())),
            );
        }
        outputs
    }

    fn attach(&mut self, client: ClientId, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.attached.insert(client);
        }
        if let Some(client) = self.clients.get_mut(&client) {
            client.sessions.insert(session_id.to_string());
        }
    }

    fn detach(&mut self, client: ClientId, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.attached.remove(&client);
        }
        if let Some(client) = self.clients.get_mut(&client) {
            client.sessions.remove(session_id);
        }
    }

    fn next_request_id(&mut self) -> String {
        self.next_request += 1;
        format!("hub-{}", self.next_request)
    }
}

/// A session a client already has, registered with `_yonder/hub/import`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportedSession {
    id: String,
    machine: String,
    agent: String,
    cwd: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, deserialize_with = "deserialize_timestamp")]
    created_at: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_timestamp")]
    updated_at: Option<u64>,
}

/// ACP gives times as RFC 3339 strings; the hub keeps Unix seconds. Either is
/// accepted.
fn deserialize_timestamp<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => parse_rfc3339(&text),
        _ => None,
    })
}

/// Unix seconds from an RFC 3339 time such as `2026-10-01T10:20:30.5+02:00`.
fn parse_rfc3339(text: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let separator = |index: usize, allowed: &[u8]| {
        text.as_bytes()
            .get(index)
            .is_some_and(|byte| allowed.contains(byte))
    };
    if !(separator(4, b"-")
        && separator(7, b"-")
        && separator(10, b"Tt ")
        && separator(13, b":")
        && separator(16, b":"))
    {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let mut rest = text.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        rest = fraction.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (hours, minutes) = rest.get(1..)?.split_once(':')?;
            sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
        }
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 in the proleptic Gregorian calendar.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok()
}

/// Updates a session's listing from one `session/update`. Returns whether the
/// listing changed enough to save.
fn apply_update(session: &mut Session, update: &Value, now: u64) -> bool {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "agent_message_chunk" => {
            let message_id = update
                .get("messageId")
                .and_then(Value::as_str)
                .map(str::to_string);
            if message_id != session.message_id {
                session.message_id = message_id;
                session.message_text.clear();
            }
            if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
                session.message_text.push_str(text);
            }
            session.meta.updated_at = now;
            false
        }
        "state_update" => {
            if let Some(state) = update.get("state").and_then(Value::as_str) {
                // Codex reports an approval it waits on as `requires_action`.
                session.meta.state = match state {
                    "requires_action" => "waiting".to_string(),
                    state => state.to_string(),
                };
            }
            session.meta.last_text = tail(&session.message_text, LAST_TEXT_CHARS);
            session.meta.updated_at = now;
            true
        }
        "session_info_update" => {
            if let Some(title) = update.get("title").and_then(Value::as_str) {
                session.meta.title = Some(title.to_string());
                session.meta.updated_at = now;
                return true;
            }
            false
        }
        _ => false,
    }
}

fn dispatch_state(session: &Session) -> &str {
    if session.live {
        &session.meta.state
    } else {
        "inactive"
    }
}

fn recent_dispatch_tool(log: &[String]) -> Option<Value> {
    let mut statuses = HashMap::default();
    for line in log.iter().rev().take(128) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(update) = message.pointer("/params/update") else {
            continue;
        };
        match update["sessionUpdate"].as_str() {
            Some("state_update") if update["state"] == "idle" => break,
            Some("tool_call" | "tool_call_update") => {
                let id = update["toolCallId"].as_str().unwrap_or_default();
                if let Some(status) = update["status"].as_str() {
                    statuses
                        .entry(id.to_owned())
                        .or_insert_with(|| status.to_owned());
                }
                if let Some(title) = update["title"].as_str() {
                    return Some(json!({
                        "title": tail(title, 400),
                        "status": statuses.get(id),
                    }));
                }
            }
            _ => {}
        }
    }
    None
}

const APPROVAL_COMMAND_CHARS: usize = 1_000;

/// What Dispatch reads back before an approval: the command as the agent
/// would run it, keeping its start when it's long.
fn describe_approval(pending: &PendingRequest, request: &Value) -> Value {
    let tool_call = approval_tool_call(request);
    let command = match tool_call.pointer("/rawInput/command") {
        Some(Value::String(command)) => Some(command.clone()),
        Some(Value::Array(parts)) => {
            let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [_, flag, script] if matches!(*flag, "-c" | "-lc") => Some(script.to_string()),
                _ => Some(parts.join(" ")),
            }
        }
        _ => None,
    };
    let details = match (&command, tool_call.get("rawInput")) {
        (None, Some(input)) if !input.is_null() => Some(input.to_string()),
        _ => None,
    };
    let shown = command.or(details);
    let truncated = shown
        .as_ref()
        .is_some_and(|text| text.chars().count() > APPROVAL_COMMAND_CHARS);
    json!({
        "approvalId": approval_id_of(&pending.id),
        "question": request["params"]["description"].as_str().map(|text| tail(text, 400)),
        "title": tool_call["title"].as_str().map(|title| tail(title, 400)),
        "kind": tool_call["kind"],
        "command": shown.map(|text| text.chars().take(APPROVAL_COMMAND_CHARS).collect::<String>()),
        "commandTruncated": truncated,
    })
}

/// The id Dispatch passes back to answer an approval: the agent's request
/// id, without JSON quotes.
fn approval_id_of(id: &Value) -> String {
    match id {
        Value::String(id) => id.clone(),
        id => id.to_string(),
    }
}

/// The tool call an approval request is about. ACP v2 puts it under
/// `subject`; v1 put it directly in the params.
pub(crate) fn approval_tool_call(request: &Value) -> &Value {
    let params = &request["params"];
    params
        .pointer("/subject/toolCall")
        .unwrap_or(&params["toolCall"])
}

pub(crate) fn tail(text: &str, chars: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(chars)).collect()
}

pub(crate) fn result_response(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

pub(crate) fn error_response(id: &Value, message: &str) -> String {
    error_response_with_code(id, -32000, message)
}

fn error_response_with_code(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AgentKey {
        AgentKey {
            machine: "gpu-07".into(),
            agent: "claude-acp".into(),
        }
    }

    fn parse(output: &Output) -> Value {
        match output {
            Output::ToClient(_, line) | Output::ToAgent(_, line) => {
                serde_json::from_str(line).unwrap()
            }
            other => panic!("not a message: {other:?}"),
        }
    }

    /// A relay with the agent started and `clients` initialized.
    fn ready_relay(clients: &[ClientId]) -> Relay {
        let mut relay = Relay::new(|| 100);
        for client in clients {
            relay.client_connected(*client, Endpoint::Agent(key()));
        }
        let outputs = relay.client_message(
            clients[0],
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        );
        assert_eq!(outputs, vec![Output::StartAgent(key())]);
        let init = relay.agent_started(&key());
        let init_id = parse(&init[0])["id"].clone();
        let outputs = relay.agent_line(
            &key(),
            &json!({"jsonrpc":"2.0","id":init_id,"result":{"protocolVersion":2,"capabilities":{"x":1}}}).to_string(),
        );
        assert_eq!(parse(&outputs[0])["result"]["capabilities"]["x"], 1);
        for client in &clients[1..] {
            let outputs = relay.client_message(
                *client,
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            );
            assert_eq!(parse(&outputs[0])["result"]["capabilities"]["x"], 1);
        }
        relay
    }

    /// Starts session "s1" from `client` and returns the relay.
    fn with_session(mut relay: Relay, client: ClientId) -> Relay {
        let outputs = relay.client_message(
            client,
            r#"{"jsonrpc":"2.0","id":7,"method":"session/new","params":{"cwd":"/home/a/api"}}"#,
        );
        let forwarded = parse(&outputs[0]);
        assert_ne!(forwarded["id"], 7, "the hub uses its own request id");
        let outputs = relay.agent_line(
            &key(),
            &json!({"jsonrpc":"2.0","id":forwarded["id"],"result":{"sessionId":"s1","modes":{}}})
                .to_string(),
        );
        let response = outputs
            .iter()
            .find(|output| matches!(output, Output::ToClient(..)))
            .map(parse)
            .unwrap();
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"]["sessionId"], "s1");
        relay
    }

    fn update(kind: &str, extra: Value) -> String {
        let mut update = json!({ "sessionUpdate": kind });
        update
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":update}})
            .to_string()
    }

    #[test]
    fn dispatch_routes_a_valid_question_with_the_original_client_and_id() {
        let mut relay = Relay::new(|| 100);
        relay.client_connected(2, Endpoint::Hub);
        let outputs = relay.client_message(
            2,
            r#"{"jsonrpc":"2.0","id":"dispatch-1","method":"_yonder/dispatch/ask","params":{"prompt":"  What's running?  "}}"#,
        );
        assert_eq!(
            outputs,
            vec![Output::AskDispatch {
                client: 2,
                id: json!("dispatch-1"),
                prompt: "What's running?".into(),
            }]
        );
        let outputs = relay.client_message(
            2,
            r#"{"jsonrpc":"2.0","id":2,"method":"_yonder/dispatch/ask","params":{"prompt":" "}}"#,
        );
        assert_eq!(parse(&outputs[0])["error"]["code"], -32602);
    }

    #[test]
    fn dispatch_opens_the_newest_project_thread_even_after_an_older_thread_is_updated() {
        let mut relay = Relay::new(|| 100);
        let project = "/home/a/gamma-qwen";
        let thread = |id: &str, cwd: &str, created_at, updated_at| ImportedSession {
            id: id.to_string(),
            machine: "gpu-06".into(),
            agent: "codex-acp".into(),
            cwd: cwd.to_string(),
            title: None,
            created_at,
            updated_at: Some(updated_at),
        };
        relay.import(vec![thread("older", project, None, 90)]);
        let (imported, corrected) = relay.import(vec![
            thread("older", project, Some(10), 95),
            thread("newest", project, Some(20), 30),
            thread("other", "/home/a/gamma-qwen-tools", Some(40), 50),
        ]);
        assert_eq!((imported, corrected), (2, true));
        assert_eq!(relay.sessions["older"].meta.updated_at, 90);
        assert_eq!(
            relay.dispatch_sessions(SessionFilter::All, 1)["sessions"][0]["id"],
            "older"
        );

        for prompt in [
            "Open the latest Gamma Qwen thread",
            "open newest gamma-qwen session!",
            "open the latest /home/a/gamma-qwen thread",
        ] {
            assert_eq!(
                crate::dispatch::open_latest_project_thread(&relay, prompt)
                    .and_then(|reply| reply.open_session_id)
                    .as_deref(),
                Some("newest")
            );
        }
        assert!(
            crate::dispatch::open_latest_project_thread(&relay, "open the latest missing thread")
                .is_none()
        );
    }

    #[test]
    fn dispatch_reports_live_state_and_keeps_the_order_of_history_chunks() {
        let mut relay = with_session(ready_relay(&[1]), 1);
        for (kind, data) in [
            ("state_update", json!({ "state": "running" })),
            (
                "tool_call",
                json!({ "toolCallId": "tool", "title": "Run checks" }),
            ),
            (
                "tool_call_update",
                json!({ "toolCallId": "tool", "status": "in_progress" }),
            ),
            (
                "agent_message_chunk",
                json!({ "messageId": "a", "content": { "text": "Running " } }),
            ),
            (
                "agent_message_chunk",
                json!({ "messageId": "a", "content": { "text": "checks." } }),
            ),
        ] {
            relay.agent_line(&key(), &update(kind, data));
        }
        assert_eq!(
            relay.dispatch_sessions(SessionFilter::Active, 20)["total"],
            1
        );
        let state = relay.dispatch_session_now("s1").unwrap();
        assert_eq!(state["recentTool"]["title"], "Run checks");
        assert_eq!(state["recentTool"]["status"], "in_progress");
        let history = relay.dispatch_history("s1", 12).unwrap();
        assert_eq!(history["messages"][0]["text"], "Running checks.");

        let mut restored = Relay::new(|| 101);
        restored.restore(
            relay
                .sessions()
                .into_iter()
                .map(|meta| (meta, Vec::new()))
                .collect(),
        );
        assert_eq!(
            restored.dispatch_sessions(SessionFilter::Active, 20)["total"],
            0
        );
        assert_eq!(
            restored.dispatch_session_now("s1").unwrap()["state"],
            "inactive"
        );
    }

    #[test]
    fn updates_reach_every_attached_client_and_the_log() {
        let mut relay = with_session(ready_relay(&[1, 2]), 1);
        // Client 2 resumes the live session: it gets the (empty) log and the setup.
        let outputs = relay.client_message(
            2,
            r#"{"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":"s1","cwd":"/home/a/api"}}"#,
        );
        assert_eq!(parse(&outputs.last().unwrap())["result"]["sessionId"], "s1");

        let line = update(
            "agent_message_chunk",
            json!({"content":{"type":"text","text":"hi"}}),
        );
        let outputs = relay.agent_line(&key(), &line);
        let clients: Vec<_> = outputs
            .iter()
            .filter_map(|output| match output {
                Output::ToClient(client, text) if *text == line => Some(*client),
                _ => None,
            })
            .collect();
        assert_eq!(clients, vec![1, 2]);
        assert!(outputs.contains(&Output::AppendLog {
            session: "s1".into(),
            line: line.clone()
        }));
    }

    #[test]
    fn late_client_gets_the_history_then_the_setup() {
        let mut relay = with_session(ready_relay(&[1, 2]), 1);
        let first = update(
            "agent_message_chunk",
            json!({"content":{"type":"text","text":"a"}}),
        );
        let second = update("state_update", json!({"state":"idle"}));
        relay.agent_line(&key(), &first);
        relay.agent_line(&key(), &second);
        let outputs = relay.client_message(
            2,
            r#"{"jsonrpc":"2.0","id":9,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x","replayFrom":{"type":"start"}}}"#,
        );
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0], Output::ToClient(2, first));
        assert_eq!(outputs[1], Output::ToClient(2, second));
        let response = parse(&outputs[2]);
        assert_eq!(response["id"], 9);
        assert_eq!(response["result"]["sessionId"], "s1");
    }

    #[test]
    fn first_approval_answer_wins_and_late_clients_see_it_pending() {
        let mut relay = with_session(ready_relay(&[1, 2, 3]), 1);
        relay.client_message(
            2,
            r#"{"jsonrpc":"2.0","id":4,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x"}}"#,
        );
        let request = json!({"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{"sessionId":"s1","options":[]}}).to_string();
        let outputs = relay.agent_line(&key(), &request);
        assert!(outputs.contains(&Output::ToClient(1, request.clone())));
        assert!(outputs.contains(&Output::ToClient(2, request.clone())));
        assert_eq!(relay.sessions()[0].state, "waiting");

        // Client 3 attaches later and still sees the approval.
        let outputs = relay.client_message(
            3,
            r#"{"jsonrpc":"2.0","id":5,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x"}}"#,
        );
        assert_eq!(outputs.last(), Some(&Output::ToClient(3, request)));

        let answer = r#"{"jsonrpc":"2.0","id":"perm-1","result":{"outcome":{"outcome":"selected","optionId":"allow"}}}"#;
        let outputs = relay.client_message(2, answer);
        assert_eq!(outputs[0], Output::ToAgent(key(), answer.to_string()));
        assert_eq!(relay.sessions()[0].state, "running");
        assert!(
            relay.client_message(1, answer).is_empty(),
            "a second answer is ignored"
        );
    }

    #[test]
    fn dispatch_answers_only_the_approval_it_read_with_a_one_time_option() {
        let mut relay = with_session(ready_relay(&[1]), 1);
        assert!(!relay.dispatch_session_busy("s1"));
        let request = json!({"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{
            "sessionId":"s1",
            "description":"May I run npm test outside the sandbox?",
            "subject":{"type":"tool_call","toolCall":{"title":"Run tests","kind":"execute","rawInput":{"command":["bash","-lc","npm test"]}}},
            "options":[
                {"optionId":"always","name":"Always allow","kind":"allow_always"},
                {"optionId":"once","name":"Allow","kind":"allow_once"},
                {"optionId":"no","name":"Reject","kind":"reject_once"},
            ],
        }})
        .to_string();
        relay.agent_line(&key(), &request);
        relay.agent_line(
            &key(),
            &update("state_update", json!({"state":"requires_action"})),
        );
        assert!(relay.dispatch_session_busy("s1"));
        assert_eq!(relay.sessions()[0].state, "waiting");

        let pending = relay.dispatch_session_now("s1").unwrap()["pendingApproval"].clone();
        assert_eq!(pending["command"], "npm test");
        assert_eq!(pending["title"], "Run tests");
        assert_eq!(
            pending["question"],
            "May I run npm test outside the sandbox?"
        );
        let approval = pending["approvalId"].as_str().unwrap().to_string();
        assert_eq!(approval, "perm-1");

        assert!(
            relay
                .dispatch_answer_approval("s1", "perm-0", true)
                .is_err(),
            "an approval Dispatch didn't read can't be answered"
        );
        let outputs = relay
            .dispatch_answer_approval("s1", &approval, true)
            .unwrap();
        assert_eq!(
            parse(&outputs[0])["result"]["outcome"],
            json!({"outcome":"selected","optionId":"once"})
        );
        assert!(matches!(&outputs[0], Output::ToAgent(agent, _) if *agent == key()));
        assert_eq!(relay.sessions()[0].state, "running");
        assert!(
            relay
                .dispatch_answer_approval("s1", &approval, false)
                .is_err(),
            "an answered approval is gone"
        );
        assert!(
            relay
                .client_message(
                    1,
                    &json!({"jsonrpc":"2.0","id":"perm-1","result":{}}).to_string()
                )
                .is_empty(),
            "a client's late answer is ignored"
        );

        relay.agent_line(&key(), &request.replace("perm-1", "perm-2"));
        let approval = relay.dispatch_session_now("s1").unwrap()["pendingApproval"]["approvalId"]
            .as_str()
            .unwrap()
            .to_string();
        let outputs = relay
            .dispatch_answer_approval("s1", &approval, false)
            .unwrap();
        assert_eq!(parse(&outputs[0])["result"]["outcome"]["optionId"], "no");
    }

    #[test]
    fn listing_follows_title_state_and_last_text() {
        let mut relay = with_session(ready_relay(&[1]), 1);
        relay.agent_line(
            &key(),
            &update("session_info_update", json!({"title":"Fix login race"})),
        );
        relay.agent_line(
            &key(),
            &update(
                "agent_message_chunk",
                json!({"content":{"type":"text","text":"Done: "},"messageId":"m1"}),
            ),
        );
        relay.agent_line(
            &key(),
            &update(
                "agent_message_chunk",
                json!({"content":{"type":"text","text":"locked the row."},"messageId":"m1"}),
            ),
        );
        relay.agent_line(&key(), &update("state_update", json!({"state":"idle"})));
        let session = &relay.sessions()[0];
        assert_eq!(session.title.as_deref(), Some("Fix login race"));
        assert_eq!(session.state, "idle");
        assert_eq!(session.last_text, "Done: locked the row.");
        assert_eq!(session.machine, "gpu-07");
        assert_eq!(session.cwd, "/home/a/api");
    }

    fn restored_meta() -> SessionMeta {
        SessionMeta {
            id: "s1".into(),
            machine: "gpu-07".into(),
            agent: "claude-acp".into(),
            cwd: "/x".into(),
            title: None,
            state: "idle".into(),
            last_text: String::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn old_update() -> String {
        update(
            "agent_message_chunk",
            json!({"content":{"type":"text","text":"old"}}),
        )
    }

    /// The client messages the relay sends for one agent line, in order.
    fn to_client(outputs: &[Output], client: ClientId) -> Vec<String> {
        outputs
            .iter()
            .filter_map(|output| match output {
                Output::ToClient(to, line) if *to == client => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn restored_session_is_resumed_through_the_agent_then_replayed() {
        let mut relay = ready_relay(&[1]);
        relay.restore(vec![(restored_meta(), vec![old_update()])]);
        let outputs = relay.client_message(
            1,
            r#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x","replayFrom":{"type":"start"}}}"#,
        );
        let forwarded = parse(&outputs[0]);
        assert_eq!(forwarded["method"], "session/resume");
        assert!(
            forwarded["params"].get("replayFrom").is_none(),
            "the hub replays its own log, so the agent mustn't replay too"
        );
        let outputs = relay.agent_line(
            &key(),
            &json!({"jsonrpc":"2.0","id":forwarded["id"],"result":{}}).to_string(),
        );
        let sent = to_client(&outputs, 1);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], old_update());
        assert_eq!(serde_json::from_str::<Value>(&sent[1]).unwrap()["id"], 2);
    }

    #[test]
    fn restored_session_resumed_without_replay_from_replays_nothing() {
        let mut relay = ready_relay(&[1]);
        relay.restore(vec![(restored_meta(), vec![old_update()])]);
        let request = r#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x"}}"#;
        let outputs = relay.client_message(1, request);
        let forwarded = parse(&outputs[0]);
        assert_eq!(forwarded["params"], json!({"sessionId":"s1","cwd":"/x"}));
        let outputs = relay.agent_line(
            &key(),
            &json!({"jsonrpc":"2.0","id":forwarded["id"],"result":{}}).to_string(),
        );
        let sent = to_client(&outputs, 1);
        assert_eq!(sent.len(), 1, "only the response: {sent:?}");
        assert_eq!(serde_json::from_str::<Value>(&sent[0]).unwrap()["id"], 2);

        // The log is kept for a later client that does ask for history.
        relay.client_connected(2, Endpoint::Agent(key()));
        let outputs = relay.client_message(
            2,
            r#"{"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x","replayFrom":{"type":"start"}}}"#,
        );
        assert_eq!(outputs[0], Output::ToClient(2, old_update()));
    }

    #[test]
    fn session_without_a_hub_log_is_replayed_by_the_agent_and_logged() {
        let mut relay = ready_relay(&[1]);
        relay.client_connected(9, Endpoint::Hub);
        relay.client_message(
            9,
            &json!({"jsonrpc":"2.0","id":1,"method":"_yonder/hub/import","params":{"sessions":[
                {"id":"s1","machine":"gpu-07","agent":"claude-acp","cwd":"/x"}
            ]}})
            .to_string(),
        );
        let outputs = relay.client_message(
            1,
            r#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x","replayFrom":{"type":"start"}}}"#,
        );
        let forwarded = parse(&outputs[0]);
        assert_eq!(forwarded["params"]["replayFrom"], json!({"type":"start"}));

        // The agent replays the history before answering.
        let outputs = relay.agent_line(&key(), &old_update());
        assert!(outputs.contains(&Output::ToClient(1, old_update())));
        assert!(outputs.contains(&Output::AppendLog {
            session: "s1".into(),
            line: old_update(),
        }));
        let outputs = relay.agent_line(
            &key(),
            &json!({"jsonrpc":"2.0","id":forwarded["id"],"result":{}}).to_string(),
        );
        let sent = to_client(&outputs, 1);
        assert_eq!(sent.len(), 1, "the hub doesn't replay it again: {sent:?}");
        assert_eq!(serde_json::from_str::<Value>(&sent[0]).unwrap()["id"], 2);
    }

    #[test]
    fn live_session_replays_only_when_asked() {
        let mut relay = with_session(ready_relay(&[1, 2]), 1);
        let line = old_update();
        relay.agent_line(&key(), &line);

        let outputs = relay.client_message(
            2,
            r#"{"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x"}}"#,
        );
        assert_eq!(outputs.len(), 1, "only the setup: {outputs:?}");
        let response = parse(&outputs[0]);
        assert_eq!(response["id"], 3);
        assert_eq!(response["result"]["sessionId"], "s1");

        let outputs = relay.client_message(
            2,
            r#"{"jsonrpc":"2.0","id":4,"method":"session/load","params":{"sessionId":"s1","cwd":"/x"}}"#,
        );
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0], Output::ToClient(2, line));
        assert_eq!(parse(&outputs[1])["id"], 4);
        assert!(
            !outputs
                .iter()
                .any(|output| matches!(output, Output::ToAgent(..))),
            "a live session is answered by the hub"
        );
    }

    #[test]
    fn loading_a_restored_session_lets_the_agent_replay() {
        let mut relay = ready_relay(&[1]);
        relay.restore(vec![(restored_meta(), vec![old_update()])]);
        let outputs = relay.client_message(
            1,
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/x"}}"#,
        );
        let forwarded = parse(&outputs[0]);
        assert_eq!(forwarded["method"], "session/load");
        let outputs = relay.agent_line(
            &key(),
            &json!({"jsonrpc":"2.0","id":forwarded["id"],"result":{}}).to_string(),
        );
        assert_eq!(to_client(&outputs, 1).len(), 1);
    }

    #[test]
    fn hub_endpoint_lists_sessions() {
        let mut relay = with_session(ready_relay(&[1]), 1);
        relay.client_connected(9, Endpoint::Hub);
        let outputs = relay.client_message(
            9,
            r#"{"jsonrpc":"2.0","id":1,"method":"_yonder/hub/sessions"}"#,
        );
        let response = parse(&outputs[0]);
        assert_eq!(response["result"]["sessions"][0]["id"], "s1");
        assert_eq!(response["result"]["sessions"][0]["machine"], "gpu-07");
    }

    #[test]
    fn hub_endpoint_lists_machines() {
        let mut relay = Relay::new(|| 100);
        relay.set_machines(vec!["gpu-06".into(), "gpu-07".into()]);
        relay.client_connected(9, Endpoint::Hub);
        let outputs = relay.client_message(
            9,
            r#"{"jsonrpc":"2.0","id":1,"method":"_yonder/hub/machines"}"#,
        );
        assert_eq!(
            parse(&outputs[0])["result"],
            json!({"machines":["gpu-06","gpu-07"]})
        );
    }

    #[test]
    fn hub_endpoint_imports_only_new_sessions() {
        let mut relay = with_session(ready_relay(&[1]), 1);
        relay.client_connected(9, Endpoint::Hub);
        let outputs = relay.client_message(
            9,
            &json!({"jsonrpc":"2.0","id":1,"method":"_yonder/hub/import","params":{"sessions":[
                {"id":"s1","machine":"other","agent":"codex-acp","cwd":"/elsewhere"},
                {"id":"s2","machine":"gpu-06","agent":"codex-acp","cwd":"/y","title":"Old thread","updatedAt":"2026-10-01T10:20:30+02:00"},
                {"id":"s3","machine":"gpu-07","agent":"claude-acp","cwd":"/z","updatedAt":50},
            ]}})
            .to_string(),
        );
        assert_eq!(outputs[0], Output::SaveSessions);
        assert_eq!(parse(&outputs[1])["result"], json!({"imported": 2}));

        let sessions = relay.sessions();
        let find = |id: &str| sessions.iter().find(|session| session.id == id).unwrap();
        assert_eq!(
            find("s1").machine,
            "gpu-07",
            "known sessions are left alone"
        );
        assert_eq!(find("s1").cwd, "/home/a/api");
        assert_eq!(find("s2").title.as_deref(), Some("Old thread"));
        assert_eq!(find("s2").updated_at, 1_790_842_830);
        assert_eq!(find("s2").state, "idle");
        assert_eq!(find("s3").updated_at, 50);
        assert!(!relay.sessions["s2"].live);

        let outputs = relay.client_message(
            9,
            r#"{"jsonrpc":"2.0","id":2,"method":"_yonder/hub/import","params":{"sessions":[{"id":"s2","machine":"gpu-06","agent":"codex-acp","cwd":"/y"}]}}"#,
        );
        assert_eq!(outputs.len(), 1, "nothing new, nothing to save");
        assert_eq!(parse(&outputs[0])["result"], json!({"imported": 0}));

        let outputs = relay.client_message(
            9,
            r#"{"jsonrpc":"2.0","id":3,"method":"_yonder/hub/import","params":{"sessions":[{"id":"s4"}]}}"#,
        );
        assert_eq!(parse(&outputs[0])["error"]["code"], -32602);
    }

    #[test]
    fn rfc3339_times_become_unix_seconds() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-10-01T08:20:30.123Z"),
            Some(1_790_842_830)
        );
        assert_eq!(
            parse_rfc3339("2026-10-01T10:20:30+02:00"),
            Some(1_790_842_830)
        );
        assert_eq!(
            parse_rfc3339("2024-02-29T00:00:00-01:30"),
            Some(1_709_170_200)
        );
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn agent_exit_fails_requests_in_flight() {
        let mut relay = with_session(ready_relay(&[1]), 1);
        relay.client_message(
            1,
            r#"{"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{"sessionId":"s1","prompt":[]}}"#,
        );
        let outputs = relay.agent_gone(&key(), "the agent exited");
        let response = parse(&outputs[0]);
        assert_eq!(response["id"], 11);
        assert_eq!(response["error"]["message"], "the agent exited");
    }
}
