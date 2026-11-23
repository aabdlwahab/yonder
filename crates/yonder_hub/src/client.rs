//! A small command-line client for the hub, speaking the same ACP over
//! websocket a Mac or phone does.

use std::collections::VecDeque;
use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use async_tungstenite::tungstenite::Message;
use futures::{FutureExt, StreamExt as _};
use serde_json::{Value, json};

use crate::dispatch::{ASK_METHOD, DispatchReply};
use crate::relay::SessionMeta;

type Socket = async_tungstenite::WebSocketStream<smol::net::TcpStream>;

pub struct HubAddress {
    /// `host:port`
    pub address: String,
    pub token: String,
}

struct Connection {
    socket: Socket,
    next_id: u64,
    /// Messages that arrived while waiting for a response.
    backlog: VecDeque<Value>,
}

impl Connection {
    async fn open(hub: &HubAddress, path: &str) -> Result<Self> {
        let stream = smol::net::TcpStream::connect(&hub.address)
            .await
            .with_context(|| format!("connecting to the hub at {}", hub.address))?;
        let url = format!("ws://{}{path}?token={}", hub.address, hub.token);
        let (socket, _) = async_tungstenite::client_async(url, stream)
            .await
            .context("opening the hub connection")?;
        let mut connection = Self {
            socket,
            next_id: 0,
            backlog: VecDeque::new(),
        };
        connection
            .request(
                "initialize",
                json!({
                    "protocolVersion": 2,
                    "info": { "name": "yonder-hub-cli", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": {},
                }),
            )
            .await?;
        Ok(connection)
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        self.socket
            .send(Message::text(message.to_string()))
            .await
            .context("sending to the hub")
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = format!("cli-{}", self.next_id);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let message = self
                .receive()
                .await?
                .context("the hub closed the connection")?;
            if message.get("id").and_then(Value::as_str) == Some(id.as_str())
                && message.get("method").is_none()
            {
                if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("error");
                    bail!("{method}: {text}");
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.backlog.push_back(message);
        }
    }

    async fn receive(&mut self) -> Result<Option<Value>> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Ok(Some(
                        serde_json::from_str(&text).context("parsing a hub message")?,
                    ));
                }
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(anyhow!(error)),
            }
        }
    }

    async fn next_message(&mut self) -> Result<Option<Value>> {
        if let Some(message) = self.backlog.pop_front() {
            return Ok(Some(message));
        }
        self.receive().await
    }

    /// The next message, or `None` after `wait` with nothing new.
    #[allow(
        clippy::disallowed_methods,
        reason = "the CLI client runs outside gpui, so there's no gpui executor timer"
    )]
    async fn next_message_within(&mut self, wait: Duration) -> Result<Option<Value>> {
        if let Some(message) = self.backlog.pop_front() {
            return Ok(Some(message));
        }
        let timer = FutureExt::fuse(smol::Timer::after(wait));
        let receive = self.receive().fuse();
        futures::pin_mut!(timer, receive);
        futures::select! {
            message = receive => message,
            _ = timer => Ok(None),
        }
    }
}

pub fn list(hub: &HubAddress) -> Result<()> {
    smol::block_on(async {
        let sessions = sessions(hub).await?;
        if sessions.is_empty() {
            println!("No sessions yet.");
            return Ok(());
        }
        let now = now();
        for session in sessions {
            let title = session.title.clone().unwrap_or_else(|| "(untitled)".into());
            println!(
                "{}  {:<8} {:<8} {:<20} {:>6} ago  {}",
                short(&session.id),
                session.state,
                session.machine,
                session.agent,
                age(now.saturating_sub(session.updated_at)),
                title
            );
            let last = session.last_text.lines().last().unwrap_or_default().trim();
            if !last.is_empty() {
                println!("          {}", truncate(last, 100));
            }
        }
        Ok(())
    })
}

pub fn dispatch(hub: &HubAddress, question: &str) -> Result<()> {
    smol::block_on(async {
        let mut connection = Connection::open(hub, "/hub").await?;
        let result = connection
            .request(ASK_METHOD, json!({ "prompt": question }))
            .await?;
        let reply: DispatchReply = serde_json::from_value(result)?;
        println!("{}", reply.text);
        if let Some(session_id) = reply.open_session_id {
            println!("Open thread: {session_id}");
        }
        Ok(())
    })
}

pub fn run(hub: &HubAddress, machine: &str, agent: &str, cwd: &str, prompt: &str) -> Result<()> {
    smol::block_on(async {
        let mut connection = Connection::open(hub, &agent_path(machine, agent)).await?;
        let session = connection
            .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        let session_id = session
            .get("sessionId")
            .and_then(Value::as_str)
            .context("the agent didn't return a session id")?
            .to_string();
        eprintln!("session {session_id} ({agent} on {machine})");
        prompt_and_stream(&mut connection, &session_id, prompt).await
    })
}

pub fn follow(hub: &HubAddress, session: &str) -> Result<()> {
    smol::block_on(async {
        let (mut connection, meta) = attach(hub, session, true).await?;
        eprintln!(
            "following {} ({} on {}); Ctrl-C to stop",
            meta.id, meta.agent, meta.machine
        );
        let mut last_message = None;
        while let Some(message) = connection.next_message().await? {
            print_message(&message, &mut last_message);
        }
        Ok(())
    })
}

pub fn send(hub: &HubAddress, session: &str, text: &str) -> Result<()> {
    smol::block_on(async {
        let (mut connection, meta) = attach(hub, session, false).await?;
        prompt_and_stream(&mut connection, &meta.id, text).await
    })
}

pub fn approve(hub: &HubAddress, session: &str, option: Option<&str>) -> Result<()> {
    smol::block_on(async {
        let (mut connection, meta) = attach(hub, session, false).await?;
        let mut request = None;
        while let Some(message) = connection
            .next_message_within(Duration::from_secs(2))
            .await?
        {
            if message
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|method| method.contains("request_permission"))
            {
                request = Some(message);
            }
        }
        let Some(request) = request else {
            println!("Nothing in {} is waiting for approval.", short(&meta.id));
            return Ok(());
        };
        let options = request
            .pointer("/params/options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(option) = option else {
            println!("{}", describe_permission(&request));
            for choice in &options {
                println!(
                    "  {}  {}",
                    choice
                        .get("optionId")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    choice.get("name").and_then(Value::as_str).unwrap_or("")
                );
            }
            println!(
                "Answer with: yonder-hub approve {} <option>",
                short(&meta.id)
            );
            return Ok(());
        };
        if !options
            .iter()
            .any(|choice| choice.get("optionId").and_then(Value::as_str) == Some(option))
        {
            bail!("{option} isn't one of the offered options");
        }
        connection
            .send(json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": { "outcome": { "outcome": "selected", "optionId": option } },
            }))
            .await?;
        println!("Answered {option}.");
        Ok(())
    })
}

async fn sessions(hub: &HubAddress) -> Result<Vec<SessionMeta>> {
    let mut connection = Connection::open(hub, "/hub").await?;
    let result = connection
        .request("_yonder/hub/sessions", json!({}))
        .await?;
    Ok(serde_json::from_value(
        result.get("sessions").cloned().unwrap_or_default(),
    )?)
}

/// Finds a session by id or id prefix and attaches to it, with its history
/// replayed first when `history` is set.
async fn attach(
    hub: &HubAddress,
    session: &str,
    history: bool,
) -> Result<(Connection, SessionMeta)> {
    let matches: Vec<SessionMeta> = sessions(hub)
        .await?
        .into_iter()
        .filter(|meta| meta.id.starts_with(session))
        .collect();
    let meta = match matches.as_slice() {
        [meta] => meta.clone(),
        [] => bail!("no session starts with {session}"),
        _ => bail!("{session} matches more than one session; give more of the id"),
    };
    let mut connection = Connection::open(hub, &agent_path(&meta.machine, &meta.agent)).await?;
    let mut params = json!({ "sessionId": meta.id, "cwd": meta.cwd, "mcpServers": [] });
    if history {
        params["replayFrom"] = json!({ "type": "start" });
    }
    connection.request("session/resume", params).await?;
    Ok((connection, meta))
}

/// Sends a prompt and prints the agent's work until the turn ends.
async fn prompt_and_stream(connection: &mut Connection, session: &str, text: &str) -> Result<()> {
    connection
        .request(
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
        )
        .await?;
    let mut working = false;
    let mut last_message = None;
    while let Some(message) = connection.next_message().await? {
        print_message(&message, &mut last_message);
        if let Some(state) = message
            .pointer("/params/update")
            .filter(|update| update["sessionUpdate"] == "state_update")
            .and_then(|update| update.get("state"))
            .and_then(Value::as_str)
        {
            if state == "running" {
                working = true;
            } else if state == "idle" && working {
                println!();
                return Ok(());
            }
        }
    }
    bail!("the hub closed the connection before the turn ended")
}

fn print_message(message: &Value, last_message: &mut Option<String>) {
    let method = message.get("method").and_then(Value::as_str);
    if method.is_some_and(|method| method.contains("request_permission")) {
        println!("\n⏸ {}", describe_permission(message));
        println!("  (answer with: yonder-hub approve <session> <option>)");
        return;
    }
    let Some(update) = message.pointer("/params/update") else {
        return;
    };
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => {
            // Each agent message gets its own line; chunks of one message don't.
            let message_id = update
                .get("messageId")
                .and_then(Value::as_str)
                .map(str::to_string);
            if message_id.is_some() && *last_message != message_id {
                if last_message.is_some() {
                    println!();
                }
                *last_message = message_id;
            }
            if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
                print!("{text}");
                std::io::stdout().flush().ok();
            }
        }
        Some("user_message") => {
            let text = update
                .get("content")
                .and_then(Value::as_array)
                .and_then(|blocks| blocks.iter().find_map(|block| block.get("text")))
                .and_then(Value::as_str)
                .unwrap_or_default();
            println!("\n> {}", truncate(text, 200));
        }
        Some("tool_call") => {
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            println!("\n• {}", truncate(title, 160));
        }
        Some("state_update") => {
            let stop = update.get("stopReason");
            let reason = stop.and_then(Value::as_str).or_else(|| {
                stop.and_then(|stop| stop.get("type"))
                    .and_then(Value::as_str)
            });
            if let Some(reason) = reason
                && reason != "end_turn"
            {
                let message = update
                    .pointer("/error/message")
                    .or_else(|| stop.and_then(|stop| stop.pointer("/error/message")))
                    .and_then(Value::as_str);
                match message {
                    Some(message) => println!("\n(stopped: {reason}: {message})"),
                    None => println!("\n(stopped: {reason})"),
                }
            }
        }
        _ => {}
    }
}

fn describe_permission(request: &Value) -> String {
    let title = crate::relay::approval_tool_call(request)["title"]
        .as_str()
        .unwrap_or("a tool call");
    format!("Waiting for approval: {}", truncate(title, 160))
}

fn agent_path(machine: &str, agent: &str) -> String {
    format!("/acp/{machine}/{}", agent.replace('@', "%40"))
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn truncate(text: &str, chars: usize) -> String {
    let mut out: String = text.chars().take(chars).collect();
    if text.chars().count() > chars {
        out.push('…');
    }
    out
}

fn age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
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
    fn agent_paths_encode_account_names() {
        assert_eq!(
            agent_path("gpu-07", "claude-acp@personal"),
            "/acp/gpu-07/claude-acp%40personal"
        );
    }
}
