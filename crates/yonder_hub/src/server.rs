//! The running hub: a websocket listener for clients, agent processes, and
//! the relay between them.

use std::cell::RefCell;
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
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use gpui::{App, AppContext as _, AsyncApp, BackgroundExecutor, Entity, Task};
use project::{AgentId, Project};
use serde_json::{Value, json};

use crate::dispatch::{self, Action, Consent, Dispatch, ToolResult, ToolStep, VoiceCall};
use crate::headless::AgentCliAppState;
use crate::machines::Machines;
use crate::registry::Registry;
use crate::relay::{AgentKey, ClientId, Endpoint, Output, Relay, error_response, result_response};

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
        let mut relay = Relay::new(now);
        let saved = registry.load()?;
        log::info!("restored {} sessions", saved.len());
        relay.restore(saved);
        let machines = Machines::new(app_state, local_name.clone());
        relay.set_machines(cx.update(|cx| machines.names(cx)));

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
        }));

        let listener = smol::net::TcpListener::bind(&listen)
            .await
            .with_context(|| format!("listening on {listen}"))?;
        eprintln!("yonder-hub on {local_name}: listening on ws://{listen}");
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
                            Some(id) => {
                                state.relay.dispatch_history(&id, 12).unwrap_or(Value::Null)
                            }
                            None => state
                                .relay
                                .dispatch_sessions(dispatch::SessionFilter::All, 50),
                        };
                        reply.send(sessions).ok();
                        Vec::new()
                    }
                    Event::Voice(call) => {
                        start_dispatch_voice(hub.clone(), state.dispatch.clone(), call, cx);
                        Vec::new()
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
                let tools = dispatch_tools(hub, dispatch.consent(), cx.clone());
                dispatch
                    .voice(&mut call, &tools, cx.background_executor())
                    .await
            }
            None => Err(anyhow!("Dispatch is disabled on this hub")),
        };
        if let Err(error) = result {
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
            Output::SaveSessions => save = true,
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
    if let Some(reply) = dispatch::open_latest_project_thread(&hub.borrow().relay, &prompt) {
        tx.unbounded_send(result_response(&id, json!(reply))).ok();
        return;
    }
    cx.spawn(async move |cx| {
        let tools = dispatch_tools(hub, dispatch.consent(), cx.clone());
        let ask = dispatch.ask(&prompt, &tools);
        let timeout = cx.background_executor().timer(dispatch::ASK_TIMEOUT);
        let result = smol::future::race(ask, async move {
            timeout.await;
            Err(anyhow!("Dispatch timed out"))
        })
        .await;
        let reply = match result {
            Ok(reply) => result_response(&id, json!(reply)),
            Err(error) => error_response(&id, &format!("{error:#}")),
        };
        tx.unbounded_send(reply).ok();
    })
    .detach();
}

const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(300);
const THREAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const SEND_WAIT: Duration = Duration::from_secs(20);
const REPLY_WAIT: Duration = Duration::from_secs(45);
const TURN_START_WAIT: Duration = Duration::from_secs(60);
const REPLY_CHARS: usize = 2_000;

/// Dispatch's tools: reads answer at once, and sends and approvals act
/// through the hub.
fn dispatch_tools(
    hub: Rc<RefCell<Hub>>,
    consent: Rc<RefCell<Consent>>,
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
            Ok(ToolStep::Act(action)) => run_action(hub.clone(), action, cx.clone()).boxed_local(),
            Err(error) => future::ready(Err(error)).boxed_local(),
        }
    }
}

async fn run_action(hub: Rc<RefCell<Hub>>, action: Action, mut cx: AsyncApp) -> Result<ToolResult> {
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
        } => send_to_thread(hub, session, text, wait_for_reply, cx).await,
    }
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
