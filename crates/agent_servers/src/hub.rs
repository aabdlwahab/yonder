//! Agents that run on the Yonder hub instead of in the app.
//!
//! The hub (`crates/yonder_hub`) owns threads on the machines it serves so they
//! keep running with the laptop shut. It relays ACP, so the app talks to it the
//! same way it talks to an agent process, just over a websocket.

use std::any::Any;
use std::rc::Rc;
use std::sync::Arc;

use acp_thread::AgentConnection;
use agent_client_protocol::v2::schema as acp_v2;
use anyhow::{Context as _, Result};
use async_tungstenite::tungstenite::Message;
use collections::HashSet;
use fs::Fs;
use futures::{SinkExt as _, StreamExt as _};
use gpui::{App, Entity, Task};
use project::{Project, agent_server_store::AgentId};
use settings::{AgentConfigOptionValue, RegisterSetting, Settings};

use crate::{AcpV2Connection, AgentServer, AgentServerDelegate, CustomAgentServer};

#[derive(Clone, Debug, Default, PartialEq, Eq, RegisterSetting)]
pub struct HubSettings {
    /// `host:port` of the hub, when one is configured.
    pub address: Option<String>,
    pub enabled: bool,
}

impl Settings for HubSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let hub = content.remote.hub.clone().unwrap_or_default();
        let address = hub.address.filter(|address| !address.trim().is_empty());
        Self {
            enabled: address.is_some() && hub.enabled.unwrap_or(true),
            address,
        }
    }
}

/// Dispatch's saved conversations open from the hub as threads of this agent
/// on this machine; no real agent or machine is behind them.
pub const DISPATCH_AGENT_ID: &str = "dispatch";
pub const DISPATCH_MACHINE: &str = "hub";
/// Lists Dispatch's saved conversations on the hub's `/hub` endpoint.
pub const DISPATCH_CONVERSATIONS_METHOD: &str = "_yonder/dispatch/conversations";

pub fn is_dispatch(agent_id: &AgentId) -> bool {
    agent_id.as_ref() == DISPATCH_AGENT_ID
}

/// Where a thread's agent runs when it runs on the hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubRoute {
    pub address: String,
    pub token: String,
    /// The machine's name as the hub knows it, e.g. `gpu-07`.
    pub machine: String,
}

impl HubRoute {
    pub fn url(&self, path: &str) -> String {
        format!("ws://{}{path}?token={}", self.address, self.token)
    }

    pub fn agent_path(&self, agent_id: &AgentId) -> String {
        format!(
            "/acp/{}/{}",
            self.machine,
            agent_id.as_ref().replace('%', "%25").replace('@', "%40")
        )
    }
}

/// An agent reached through the hub. Settings-based behaviour (icon, default
/// mode, favourites) comes from the agent's usual server; only the connection
/// goes to the hub.
pub struct HubAgentServer {
    inner: CustomAgentServer,
    route: HubRoute,
}

impl HubAgentServer {
    pub fn new(agent_id: AgentId, route: HubRoute) -> Self {
        Self {
            inner: CustomAgentServer::new(agent_id),
            route,
        }
    }

    pub fn route(&self) -> &HubRoute {
        &self.route
    }
}

impl AgentServer for HubAgentServer {
    fn logo(&self) -> ui::IconName {
        if is_dispatch(&self.inner.agent_id()) {
            return ui::IconName::Sparkle;
        }
        self.inner.logo()
    }

    fn agent_id(&self) -> AgentId {
        self.inner.agent_id()
    }

    fn connect(
        &self,
        delegate: AgentServerDelegate,
        _project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let agent_id = self.agent_id();
        let route = self.route.clone();
        let store = delegate.store.downgrade();
        cx.spawn(async move |cx| {
            let url = route.url(&route.agent_path(&agent_id));
            let stream = smol::net::TcpStream::connect(&route.address)
                .await
                .with_context(|| format!("connecting to the Yonder hub at {}", route.address))?;
            let (socket, _) = async_tungstenite::client_async(url, stream)
                .await
                .context("opening the Yonder hub connection")?;
            let (sink, stream) = socket.split();
            let incoming = stream
                .take_while(|message| {
                    futures::future::ready(!matches!(message, Ok(Message::Close(_))))
                })
                .filter_map(|message| {
                    futures::future::ready(match message {
                        Ok(Message::Text(text)) => Some(Ok(text.to_string())),
                        Ok(_) => None,
                        Err(error) => Some(Err(std::io::Error::other(error))),
                    })
                })
                .boxed();
            let outgoing = sink
                .with(|line: String| {
                    futures::future::ready(Ok::<_, async_tungstenite::tungstenite::Error>(
                        Message::text(line),
                    ))
                })
                .sink_map_err(std::io::Error::other);
            let connection =
                AcpV2Connection::over_lines(agent_id, store, incoming, Box::pin(outgoing), cx)
                    .await?;
            Ok(Rc::new(connection) as Rc<dyn AgentConnection>)
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }

    fn default_mode(&self, cx: &App) -> Option<acp_v2::SessionConfigValueId> {
        self.inner.default_mode(cx)
    }

    fn set_default_mode(
        &self,
        mode_id: Option<acp_v2::SessionConfigValueId>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        self.inner.set_default_mode(mode_id, fs, cx)
    }

    fn default_config_option(&self, config_id: &str, cx: &App) -> Option<AgentConfigOptionValue> {
        self.inner.default_config_option(config_id, cx)
    }

    fn set_default_config_option(
        &self,
        config_id: &str,
        value: Option<AgentConfigOptionValue>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        self.inner
            .set_default_config_option(config_id, value, fs, cx)
    }

    fn favorite_config_option_value_ids(
        &self,
        config_id: &acp_v2::SessionConfigId,
        cx: &mut App,
    ) -> HashSet<acp_v2::SessionConfigValueId> {
        self.inner.favorite_config_option_value_ids(config_id, cx)
    }

    fn toggle_favorite_config_option_value(
        &self,
        config_id: acp_v2::SessionConfigId,
        value_id: acp_v2::SessionConfigValueId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        self.inner.toggle_favorite_config_option_value(
            config_id,
            value_id,
            should_be_favorite,
            fs,
            cx,
        )
    }
}

/// How the app's link to the hub is doing, for Settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum HubStatus {
    #[default]
    Off,
    Connecting,
    NoToken,
    Connected {
        machines: Vec<String>,
        sessions: usize,
    },
    Failed(gpui::SharedString),
}

/// The hub status the app shares with Settings, and a way to check again.
#[derive(Default)]
pub struct HubBoard {
    status: HubStatus,
    refresh: Option<Rc<dyn Fn(&mut App)>>,
}

impl gpui::Global for HubBoard {}

impl HubBoard {
    pub fn status(cx: &App) -> HubStatus {
        cx.try_global::<Self>()
            .map(|board| board.status.clone())
            .unwrap_or_default()
    }

    pub fn set_status(status: HubStatus, cx: &mut App) {
        cx.default_global::<Self>().status = status;
        cx.refresh_windows();
    }

    pub fn set_refresh(refresh: impl Fn(&mut App) + 'static, cx: &mut App) {
        cx.default_global::<Self>().refresh = Some(Rc::new(refresh));
    }

    /// Connects to the hub again, e.g. after its address or token changed.
    pub fn refresh(cx: &mut App) {
        if let Some(refresh) = cx
            .try_global::<Self>()
            .and_then(|board| board.refresh.clone())
        {
            refresh(cx);
        }
    }
}

/// The machine a remote project is on, by the name the hub knows it under:
/// the SSH nickname when there is one, else the host.
pub fn machine_name(options: &remote::RemoteConnectionOptions) -> Option<String> {
    match options {
        remote::RemoteConnectionOptions::Ssh(ssh) => {
            Some(ssh.nickname.clone().unwrap_or_else(|| ssh.host.to_string()))
        }
        _ => None,
    }
}

/// Whether a project on `machine` runs its agents on the hub, and how.
pub fn route_for(
    settings: &HubSettings,
    token: Option<&str>,
    served: &[String],
    machine: Option<&str>,
) -> Option<HubRoute> {
    if !settings.enabled {
        return None;
    }
    let address = settings.address.clone()?;
    let token = token.filter(|token| !token.is_empty())?;
    let machine = machine?;
    served.iter().any(|name| name == machine).then(|| HubRoute {
        address,
        token: token.to_string(),
        machine: machine.to_string(),
    })
}

/// Where the hub's token is kept, as people know it on this system: the
/// macOS Keychain, or the Secret Service keyring on Linux.
pub const TOKEN_STORE_NAME: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else {
    "keyring"
};

fn keychain_url(address: &str) -> String {
    format!("yonder-hub://{address}")
}

pub async fn read_hub_token(address: &str, cx: &gpui::AsyncApp) -> Result<Option<String>> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    Ok(provider
        .read_credentials(&keychain_url(address), cx)
        .await?
        .and_then(|(_, token)| String::from_utf8(token).ok())
        .filter(|token| !token.is_empty()))
}

pub async fn write_hub_token(address: &str, token: &str, cx: &gpui::AsyncApp) -> Result<()> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    provider
        .write_credentials(&keychain_url(address), "token", token.as_bytes(), cx)
        .await
}

/// Calls hub-wide methods (`_yonder/hub/*`) in order and returns their results.
pub async fn call_hub(
    address: &str,
    token: &str,
    calls: Vec<(&str, serde_json::Value)>,
) -> Result<Vec<serde_json::Value>> {
    let route = HubRoute {
        address: address.to_string(),
        token: token.to_string(),
        machine: String::new(),
    };
    let stream = smol::net::TcpStream::connect(address)
        .await
        .with_context(|| format!("connecting to the Yonder hub at {address}"))?;
    let (mut socket, _) = async_tungstenite::client_async(route.url("/hub"), stream)
        .await
        .context("opening the Yonder hub connection")?;
    let mut results = Vec::new();
    let calls =
        std::iter::once(("initialize", serde_json::json!({ "protocolVersion": 2 }))).chain(calls);
    for (index, (method, params)) in calls.enumerate() {
        let id = format!("app-{index}");
        socket
            .send(Message::text(
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                    .to_string(),
            ))
            .await?;
        loop {
            let message = match socket.next().await {
                Some(Ok(Message::Text(text))) => serde_json::from_str::<serde_json::Value>(&text)?,
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(error.into()),
                None => anyhow::bail!("the hub closed the connection"),
            };
            if message.get("id").and_then(serde_json::Value::as_str) != Some(id.as_str()) {
                continue;
            }
            if let Some(error) = message.get("error") {
                anyhow::bail!(
                    "{method}: {}",
                    error
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("error")
                );
            }
            if index > 0 {
                results.push(message.get("result").cloned().unwrap_or_default());
            }
            break;
        }
    }
    socket.close(None).await.ok();
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(enabled: bool) -> HubSettings {
        HubSettings {
            address: Some("100.80.18.21:7766".into()),
            enabled,
        }
    }

    #[test]
    fn only_served_remote_machines_go_to_the_hub() {
        let served = vec!["gpu-06".to_string(), "gpu-07".to_string()];
        let route = route_for(&hub(true), Some("t"), &served, Some("gpu-07")).unwrap();
        assert_eq!(route.machine, "gpu-07");
        assert_eq!(route.address, "100.80.18.21:7766");
        assert!(route_for(&hub(true), Some("t"), &served, Some("laptop-vm")).is_none());
        assert!(
            route_for(&hub(true), Some("t"), &served, None).is_none(),
            "local project"
        );
        assert!(route_for(&hub(false), Some("t"), &served, Some("gpu-07")).is_none());
        assert!(
            route_for(&hub(true), None, &served, Some("gpu-07")).is_none(),
            "no token"
        );
    }

    #[test]
    fn agent_paths_encode_account_names() {
        let route = HubRoute {
            address: "100.80.18.21:7766".into(),
            token: "t".into(),
            machine: "gpu-07".into(),
        };
        assert_eq!(
            route.agent_path(&AgentId("claude-acp@personal".into())),
            "/acp/gpu-07/claude-acp%40personal"
        );
        assert_eq!(route.url("/hub"), "ws://100.80.18.21:7766/hub?token=t");
    }
}
