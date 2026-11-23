//! The running hub: a websocket listener for clients, agent processes, and
//! the relay between them.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_servers::{AgentProcess, AgentProcessGuard, CustomAgentServer, spawn_agent_process};
use anyhow::{Context as _, Result, anyhow};
use async_tungstenite::tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::StatusCode,
};
use collections::HashMap;
use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _};
use gpui::{App, AppContext as _, AsyncApp, Entity, Task};
use project::{AgentId, Project};
use serde_json::{Value, json};

use crate::dispatch::{self, Dispatch};
use crate::headless::AgentCliAppState;
use crate::machines::Machines;
use crate::registry::Registry;
use crate::relay::{AgentKey, ClientId, Endpoint, Output, Relay, error_response, result_response};

pub const DEFAULT_PORT: u16 = 7766;

enum Event {
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
}

/// Starts the hub and runs until the process exits.
pub fn serve(
    app_state: Arc<AgentCliAppState>,
    listen: String,
    local_name: String,
    dispatch_codex: Option<PathBuf>,
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

        let (events, mut event_rx) = mpsc::unbounded::<Event>();
        let hub = Rc::new(RefCell::new(Hub {
            relay,
            registry,
            machines: Machines::new(app_state, local_name.clone()),
            clients: HashMap::default(),
            agents: HashMap::default(),
            dispatch,
            next_generation: 0,
            events: events.clone(),
        }));

        let listener = smol::net::TcpListener::bind(&listen)
            .await
            .with_context(|| format!("listening on {listen}"))?;
        eprintln!("yonder-hub on {local_name}: listening on ws://{listen}");
        eprintln!("token: {}", Registry::default_dir().join("token").display());
        cx.background_spawn(accept_loop(listener, token, events))
            .detach();

        while let Some(event) = event_rx.next().await {
            let outputs = {
                let mut hub = hub.borrow_mut();
                match event {
                    Event::ClientConnected {
                        client,
                        endpoint,
                        tx,
                    } => {
                        log::info!("client {client} connected to {endpoint:?}");
                        hub.clients.insert(client, tx);
                        hub.relay.client_connected(client, endpoint);
                        Vec::new()
                    }
                    Event::ClientLine { client, line } => hub.relay.client_message(client, &line),
                    Event::ClientGone(client) => {
                        log::info!("client {client} left");
                        hub.clients.remove(&client);
                        hub.relay.client_gone(client);
                        Vec::new()
                    }
                    Event::AgentLine {
                        key,
                        generation,
                        line,
                    } => {
                        if hub.agents.get(&key).map(|agent| agent.generation) == Some(generation) {
                            hub.relay.agent_line(&key, &line)
                        } else {
                            Vec::new()
                        }
                    }
                    Event::AgentGone { key, generation } => {
                        if hub.agents.get(&key).map(|agent| agent.generation) == Some(generation) {
                            log::warn!("{} on {} exited", key.agent, key.machine);
                            hub.agents.remove(&key);
                            hub.relay.agent_gone(&key, "the agent exited")
                        } else {
                            Vec::new()
                        }
                    }
                }
            };
            apply(&hub, outputs, cx);
        }
        Ok(())
    })
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
    cx.spawn(async move |cx| {
        let ask = dispatch.ask(&prompt, |name, arguments| {
            dispatch::call_tool(&hub.borrow().relay, name, arguments)
        });
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
