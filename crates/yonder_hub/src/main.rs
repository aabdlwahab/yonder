//! Yonder hub: a headless Yonder that owns agent threads, so they keep
//! running with the laptop shut. Clients (the Mac app, a phone, the CLI here)
//! speak ACP to it over a websocket and it relays to agents it starts on each
//! machine.

mod client;
mod dispatch;
mod dispatch_web;
mod headless;
mod machines;
mod push;
mod registry;
mod relay;
mod server;

use std::path::PathBuf;
use std::process;
use std::rc::Rc;
use std::time::Duration;

use acp_thread::{AgentThreadEntry, AssistantMessageChunk, ForegroundActivity, ThreadStatus};
use agent_client_protocol::v2::schema as acp_v2;
use agent_ui::{Agent, AgentConnectionStore};
use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use gpui::{AppContext as _, AsyncApp, BorrowAppContext as _};
use project::{AgentId, Project};
use settings::SettingsStore;
use util::path_list::PathList;

#[derive(Parser)]
#[command(
    name = "yonder-hub",
    about = "Run agent threads that outlive the laptop"
)]
struct Cli {
    /// The hub to talk to, as host:port.
    #[arg(long, global = true, default_value_t = format!("127.0.0.1:{}", server::DEFAULT_PORT))]
    hub: String,
    /// The hub's token. Defaults to $YONDER_HUB_TOKEN, then the token file
    /// the hub writes on this machine.
    #[arg(long, global = true)]
    token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the hub.
    Serve {
        /// Address to listen on. Defaults to this machine's Tailscale address,
        /// or 127.0.0.1 without Tailscale.
        #[arg(long)]
        listen: Option<String>,
        /// What this machine is called in the session list. Defaults to the
        /// host name.
        #[arg(long)]
        local_name: Option<String>,
        /// Enable Dispatch with the path to a signed-in Codex CLI.
        #[arg(long)]
        dispatch_codex: Option<PathBuf>,
        #[command(flatten)]
        dispatch_web: dispatch_web::Options,
    },
    /// List every session the hub knows.
    List,
    /// Ask Dispatch about the hub's agent threads.
    Dispatch { question: String },
    /// Start a new thread and stream it until the turn ends.
    Run {
        #[arg(long)]
        machine: String,
        #[arg(long, default_value = "claude-acp")]
        agent: String,
        #[arg(long)]
        cwd: String,
        prompt: String,
    },
    /// Show a session's history, then follow it live.
    Follow { session: String },
    /// Send a message to a session and stream the reply.
    Send { session: String, text: String },
    /// Show what a session is waiting to have approved, or answer it.
    Approve {
        session: String,
        option: Option<String>,
    },
    /// Without a hub: start an agent here, ask one thing, print the answer.
    Probe {
        #[arg(long)]
        workdir: PathBuf,
        #[arg(long, default_value = "claude-acp")]
        agent: String,
        #[arg(long)]
        prompt: String,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
}

struct Args {
    agent: String,
    prompt: String,
    timeout: u64,
}

fn main() {
    let cli = Cli::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("yonder_hub=info"))
        .init();

    let result = match cli.command {
        Command::Serve {
            listen,
            local_name,
            dispatch_codex,
            dispatch_web,
        } => {
            run_headless(move |app_state, cx| {
                let local_name = local_name.unwrap_or_else(host_name);
                let listen = listen.unwrap_or_else(default_listen);
                server::serve(
                    app_state,
                    listen,
                    local_name,
                    dispatch_codex,
                    dispatch_web,
                    cx,
                )
            });
            Ok(())
        }
        Command::Probe {
            workdir,
            agent,
            prompt,
            timeout,
        } => {
            let workdir = workdir.canonicalize().unwrap_or_else(|error| {
                eprintln!("Invalid --workdir {workdir:?}: {error}");
                process::exit(2);
            });
            run_headless(move |app_state, cx| {
                cx.spawn(async move |cx| {
                    run_thread(
                        app_state,
                        workdir,
                        Args {
                            agent,
                            prompt,
                            timeout,
                        },
                        cx,
                    )
                    .await
                })
            });
            Ok(())
        }
        command => hub_address(cli.hub, cli.token).and_then(|hub| match command {
            Command::List => client::list(&hub),
            Command::Dispatch { question } => client::dispatch(&hub, &question),
            Command::Run {
                machine,
                agent,
                cwd,
                prompt,
            } => client::run(&hub, &machine, &agent, &cwd, &prompt),
            Command::Follow { session } => client::follow(&hub, &session),
            Command::Send { session, text } => client::send(&hub, &session, &text),
            Command::Approve { session, option } => {
                client::approve(&hub, &session, option.as_deref())
            }
            Command::Serve { .. } | Command::Probe { .. } => unreachable!(),
        }),
    };
    if let Err(error) = result {
        eprintln!("yonder-hub: {error:#}");
        process::exit(1);
    }
}

/// Runs `body` in a windowless Yonder and exits with its result.
fn run_headless(
    body: impl FnOnce(
        std::sync::Arc<headless::AgentCliAppState>,
        &mut gpui::App,
    ) -> gpui::Task<Result<()>>
    + 'static,
) {
    let http_client = std::sync::Arc::new(reqwest_client::ReqwestClient::new());
    let app = gpui_platform::headless().with_http_client(http_client);
    app.run(move |cx| {
        let app_state = headless::init(cx);
        load_user_settings(cx);
        let task = body(app_state, cx);
        cx.spawn(async move |cx| {
            let code = match task.await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("yonder-hub: {error:#}");
                    1
                }
            };
            cx.update(|cx| cx.quit());
            process::exit(code);
        })
        .detach();
    });
}

fn hub_address(address: String, token: Option<String>) -> Result<client::HubAddress> {
    let token = match token.or_else(|| std::env::var("YONDER_HUB_TOKEN").ok()) {
        Some(token) => token,
        None => {
            let path = registry::Registry::default_dir().join("token");
            std::fs::read_to_string(&path)
                .map(|token| token.trim().to_string())
                .with_context(|| {
                    format!(
                        "no hub token: pass --token, set YONDER_HUB_TOKEN, or run this on the hub ({})",
                        path.display()
                    )
                })?
        }
    };
    Ok(client::HubAddress { address, token })
}

#[allow(
    clippy::disallowed_methods,
    reason = "runs once at startup, before the event loop exists"
)]
fn host_name() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// The Tailscale address, so the hub is only reachable inside the tailnet.
#[allow(
    clippy::disallowed_methods,
    reason = "runs once at startup, before the event loop exists"
)]
fn default_listen() -> String {
    let tailnet = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.lines().next().map(|line| line.trim().to_string()))
        .filter(|address| !address.is_empty());
    let host = tailnet.unwrap_or_else(|| "127.0.0.1".to_string());
    format!("{host}:{}", server::DEFAULT_PORT)
}

/// The user's Yonder settings carry the agent servers (Claude accounts, Codex)
/// and the machines (`ssh_connections`).
fn load_user_settings(cx: &mut gpui::App) {
    let Ok(content) = std::fs::read_to_string(paths::settings_file()) else {
        log::info!("no settings file at {:?}", paths::settings_file());
        return;
    };
    cx.update_global::<SettingsStore, _>(|store, cx| {
        let result = store.set_user_settings(&content, cx);
        log::info!("loaded settings: {:?}", result.parse_status);
    });
}

async fn run_thread(
    app_state: std::sync::Arc<headless::AgentCliAppState>,
    workdir: PathBuf,
    args: Args,
    cx: &mut AsyncApp,
) -> Result<()> {
    let project = cx.update(|cx| {
        Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            None,
            project::LocalProjectFlags {
                init_worktree_trust: false,
                ..Default::default()
            },
            cx,
        )
    });
    project
        .update(cx, |project, cx| {
            project.create_worktree(&workdir, true, cx)
        })
        .await
        .context("opening the folder")?;

    let agent_id = AgentId(args.agent.clone().into());
    server::wait_for_agent(&project, &agent_id, cx).await?;
    let connection_store = cx.new(|cx| AgentConnectionStore::new(project.clone(), cx));
    let entry = connection_store.update(cx, |store, cx| {
        store.request_connection(
            Agent::Custom {
                id: agent_id.clone(),
            },
            Rc::new(agent_servers::CustomAgentServer::new(agent_id.clone())),
            cx,
        )
    });
    let connect = entry.read_with(cx, |entry, _| entry.wait_for_connection());
    eprintln!("starting {} in {}", args.agent, workdir.display());
    let connection = connect
        .await
        .map_err(|error| anyhow::anyhow!("starting the agent: {error:?}"))?
        .connection;

    let thread = cx
        .update(|cx| {
            connection.clone().new_session(
                project.clone(),
                PathList::new(std::slice::from_ref(&workdir)),
                cx,
            )
        })
        .await
        .context("starting a session")?;
    let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
    eprintln!("session {session_id}");

    let stop_error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let _events = cx.update(|cx| {
        let stop_error = stop_error.clone();
        cx.subscribe(
            &thread,
            move |_, event: &acp_thread::AcpThreadEvent, _| match event {
                acp_thread::AcpThreadEvent::Stopped { stop_reason, .. } => {
                    log::info!("stopped: {stop_reason:?}");
                    if let Some(acp_v2::StopReason::Error(reason)) = stop_reason {
                        let message = reason
                            .error
                            .as_ref()
                            .map(|error| error.message.clone())
                            .unwrap_or_else(|| "the agent stopped with an error".to_string());
                        *stop_error.borrow_mut() = Some(message);
                    }
                }
                acp_thread::AcpThreadEvent::Error => log::warn!("thread error"),
                acp_thread::AcpThreadEvent::Refusal => log::warn!("refusal"),
                acp_thread::AcpThreadEvent::Retry(status) => log::warn!("retry: {status:?}"),
                other => log::debug!("event: {other:?}"),
            },
        )
    });
    let start_ix = thread.read_with(cx, |thread, _| thread.entries().len());
    let submission = thread.update(cx, |thread, cx| {
        thread.send(
            vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                args.prompt.clone(),
            ))],
            cx,
        )
    });
    let submission = cx.background_spawn(submission);

    // The turn is over once the thread stops generating after our message.
    let deadline = std::time::Instant::now() + Duration::from_secs(args.timeout);
    let mut warned_about_approval = false;
    let mut seen_generating = false;
    let mut last_observed = String::new();
    loop {
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
        let (status, activity, entries, waiting) = thread.read_with(cx, |thread, _| {
            (
                thread.status(),
                thread.foreground_activity(),
                thread.entries().len(),
                thread.pending_permission_requests().next().is_some(),
            )
        });
        let observed = format!("{status:?}/{activity:?}/{entries}");
        if observed != last_observed {
            log::info!("thread: {observed}");
            last_observed = observed;
        }
        if waiting && !warned_about_approval {
            eprintln!("(the agent is waiting for an approval the hub can't give yet)");
            warned_about_approval = true;
        }
        // Agents on ACP v2 accept the prompt first and report the work
        // separately, so the turn is over once they've reported running and
        // then idle again.
        // The thread marks itself generating as soon as we send, before the
        // agent has done anything, so only the agent's own report counts.
        if activity.is_active() {
            seen_generating = true;
        }
        if seen_generating
            && status != ThreadStatus::Generating
            && activity == ForegroundActivity::Idle
            && entries > start_ix
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("no answer within {}s", args.timeout);
        }
    }
    match futures::FutureExt::now_or_never(submission) {
        Some(Err(error)) => anyhow::bail!("the prompt failed: {error:#}"),
        Some(Ok(response)) => log::info!("prompt response: {response:?}"),
        None => log::info!("prompt still pending when the turn looked done"),
    }

    let reply = thread.read_with(cx, |thread, cx| {
        let mut parts = Vec::new();
        for entry in thread.entries().iter().skip(start_ix) {
            match entry {
                AgentThreadEntry::AssistantMessage(message) => {
                    for chunk in &message.chunks {
                        if let AssistantMessageChunk::Message { block, .. } = chunk {
                            parts.push(block.to_markdown(cx));
                        }
                    }
                }
                AgentThreadEntry::ToolCall(call) => {
                    parts.push(format!("[{}]", call.label.read(cx).source()));
                }
                _ => {}
            }
        }
        parts.join("\n")
    });
    if let Some(message) = stop_error.borrow_mut().take() {
        anyhow::bail!("{message}");
    }
    if std::env::var("YONDER_HUB_TRACE").is_ok()
        && let Some(connection) = connection
            .clone()
            .downcast::<agent_servers::AcpV2Connection>()
    {
        let (messages, _) = connection.subscribe_debug_messages();
        for message in messages {
            let direction = match message.direction {
                agent_servers::AcpDebugMessageDirection::Incoming => "<-",
                agent_servers::AcpDebugMessageDirection::Outgoing => "->",
                agent_servers::AcpDebugMessageDirection::Stderr => "!!",
            };
            let body = match message.message {
                agent_servers::AcpDebugMessageContent::Request { id, method, params } => {
                    format!("request {id:?} {method} {}", params.unwrap_or_default())
                }
                agent_servers::AcpDebugMessageContent::Response { id, result } => {
                    format!(
                        "response {id:?} {:?}",
                        result.map(|value| value.unwrap_or_default())
                    )
                }
                agent_servers::AcpDebugMessageContent::Notification { method, params } => {
                    format!("notification {method} {}", params.unwrap_or_default())
                }
                agent_servers::AcpDebugMessageContent::Stderr { line } => line.to_string(),
            };
            eprintln!("{direction} {body}");
        }
    }
    if std::env::var("YONDER_HUB_DUMP").is_ok() {
        let dump = thread.read_with(cx, |thread, cx| thread.to_markdown(cx));
        eprintln!("--- thread ({start_ix} entries before the prompt) ---\n{dump}");
    }
    println!("{reply}");
    Ok(())
}
