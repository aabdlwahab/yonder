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
//! - Resuming a session the hub already runs replays its log, so a client that
//!   attaches late sees the whole thread.

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
    /// `replay`: whether the hub replays its own log (`session/resume`) or the
    /// agent replays the history itself (`session/load`).
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
        }
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
                    "updatedAt": meta.updated_at,
                    "lastText": meta.last_text,
                })
            })
            .collect();
        json!({ "sessions": sessions, "total": total })
    }

    pub fn dispatch_session_now(&self, id: &str) -> Option<Value> {
        let session = self.sessions.get(id)?;
        let meta = &session.meta;
        let state = dispatch_state(session);
        let pending_approval = (state == "waiting")
            .then(|| {
                self.agents
                    .iter()
                    .find(|(key, _)| key.machine == meta.machine && key.agent == meta.agent)
                    .and_then(|(_, agent)| {
                        agent.pending.iter().rev().find_map(|pending| {
                            if pending.session.as_deref() != Some(id) {
                                return None;
                            }
                            let request: Value = serde_json::from_str(&pending.line).ok()?;
                            let method = request.get("method")?.as_str()?;
                            if !method.contains("request_permission") {
                                return None;
                            }
                            Some(json!({
                                "title": request.pointer("/params/toolCall/title")
                                    .and_then(Value::as_str)
                                    .map(|title| tail(title, 400)),
                            }))
                        })
                    })
            })
            .flatten();
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
        Some(json!({ "id": id, "messages": messages, "order": "newest_first" }))
    }

    pub fn dispatch_has_session(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
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
            _ => {
                return vec![Output::ToClient(
                    client,
                    error_response_with_code(&id, -32601, &format!("unknown method {method}")),
                )];
            }
        };
        vec![Output::ToClient(client, result_response(&id, result))]
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
                if let Some(session) = self.sessions.get(session_id)
                    && session.live
                {
                    return self.attach_live(client, session_id.clone(), &id);
                }
                let replay = method == "session/resume";
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
                if !replay {
                    // The agent replays the history itself.
                    session.log.clear();
                }
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

    /// Attaches a client to a session the agent already runs: the log, then
    /// the setup answer, then any approvals still waiting.
    fn attach_live(&mut self, client: ClientId, session_id: String, id: &Value) -> Vec<Output> {
        self.attach(client, &session_id);
        let Some(session) = self.sessions.get(&session_id) else {
            return Vec::new();
        };
        let mut outputs: Vec<Output> = session
            .log
            .iter()
            .map(|line| Output::ToClient(client, line.clone()))
            .collect();
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
                session.meta.state = state.to_string();
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

fn tail(text: &str, chars: usize) -> String {
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
            r#"{"jsonrpc":"2.0","id":9,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x"}}"#,
        );
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

    #[test]
    fn restored_session_is_resumed_through_the_agent_then_replayed() {
        let mut relay = ready_relay(&[1]);
        let old = update(
            "agent_message_chunk",
            json!({"content":{"type":"text","text":"old"}}),
        );
        relay.restore(vec![(
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
            },
            vec![old.clone()],
        )]);
        let outputs = relay.client_message(
            1,
            r#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"s1","cwd":"/x"}}"#,
        );
        let forwarded = parse(&outputs[0]);
        assert_eq!(forwarded["method"], "session/resume");
        let outputs = relay.agent_line(
            &key(),
            &json!({"jsonrpc":"2.0","id":forwarded["id"],"result":{}}).to_string(),
        );
        assert_eq!(outputs[0], Output::ToClient(1, old));
        assert_eq!(parse(outputs.last().unwrap())["id"], 2);
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
