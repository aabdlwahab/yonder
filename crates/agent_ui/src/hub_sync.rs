//! Keeps the app connected to the Yonder hub.
//!
//! The hub runs agent threads for the machines it serves, so they keep working
//! with the laptop shut. This tracks which machines those are, hands the app's
//! existing threads on them to the hub once, and adds threads the hub started
//! for someone else (the CLI, a phone, Dispatch) to the thread list.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::v2::schema as acp;
use agent_servers::{HubBoard, HubRoute, HubSettings, HubStatus};
use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{TimeZone as _, Utc};
use futures::future::{Either, select};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Task};
use project::{AgentId, Project, WorktreePaths};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use settings::{Settings as _, SettingsStore};
use util::path_list::PathList;

use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore};

const SYNC_INTERVAL: Duration = Duration::from_secs(30);
/// How often to check while a Dispatch conversation is going, so its row
/// shows when it ends.
const LIVE_SYNC_INTERVAL: Duration = Duration::from_secs(5);
const CONVERSATIONS_LIMIT: usize = 200;

pub struct HubSync {
    settings: HubSettings,
    token: Option<String>,
    machines: Vec<String>,
    status: HubStatus,
    conversations: Vec<DispatchConversation>,
    conversation_total: usize,
    /// The hub address the app's threads were last handed to.
    imported_to: Option<String>,
    _sync: Option<Task<()>>,
    _settings_subscription: gpui::Subscription,
}

struct GlobalHubSync(Entity<HubSync>);

impl Global for GlobalHubSync {}

/// A session as the hub lists it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HubSession {
    id: String,
    machine: String,
    agent: String,
    cwd: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
}

#[derive(Deserialize)]
struct HubSessions {
    sessions: Vec<HubSession>,
}

impl HubSessions {
    fn import_session(self, session_id: &acp::SessionId, cx: &mut App) -> Result<ThreadMetadata> {
        let requested_id = session_id.to_string();
        let session = self
            .sessions
            .into_iter()
            .find(|session| session.id == requested_id)
            .context("That thread is no longer listed on the Yonder hub.")?;
        let machine = session.machine.clone();
        add_hub_sessions(vec![session], cx);
        ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry_by_session(session_id).cloned())
            .with_context(|| format!("Add {machine} in Settings → Machines to open this thread."))
    }
}

/// One of Dispatch's saved conversations, a call or a typed question, as the
/// hub lists it.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DispatchConversation {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// `voice` or `typed`.
    #[serde(default)]
    pub kind: String,
    /// `running` while it's going, `idle` after.
    #[serde(default)]
    pub state: String,
    /// What it touched, e.g. "2 threads, 1 command".
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

impl DispatchConversation {
    pub fn is_voice(&self) -> bool {
        self.kind == "voice"
    }

    pub fn is_live(&self) -> bool {
        self.state == "running"
    }

    pub fn title(&self) -> SharedString {
        match self
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
        {
            Some(title) => SharedString::from(title.to_owned()),
            None if self.is_voice() => "Voice call".into(),
            None => "Typed question".into(),
        }
    }

    pub fn session_id(&self) -> acp::SessionId {
        acp::SessionId::new(self.id.clone())
    }
}

#[derive(Deserialize)]
struct DispatchConversations {
    conversations: Vec<DispatchConversation>,
    #[serde(default)]
    total: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DispatchReply {
    pub text: gpui::SharedString,
    pub open_session_id: Option<acp::SessionId>,
}

pub fn init(cx: &mut App) {
    let hub = cx.new(|cx| {
        let subscription = cx.observe_global::<SettingsStore>(|this: &mut HubSync, cx| {
            if *HubSettings::get_global(cx) != this.settings {
                this.restart(cx);
            }
        });
        let mut hub = HubSync {
            settings: HubSettings::default(),
            token: None,
            machines: Vec::new(),
            status: HubStatus::Off,
            conversations: Vec::new(),
            conversation_total: 0,
            imported_to: None,
            _sync: None,
            _settings_subscription: subscription,
        };
        hub.restart(cx);
        hub
    });
    cx.observe(&hub, |hub, cx| {
        let status = hub.read(cx).status.clone();
        if HubBoard::status(cx) != status {
            HubBoard::set_status(status, cx);
        }
    })
    .detach();
    HubBoard::set_refresh(
        {
            let hub = hub.downgrade();
            move |cx| {
                hub.update(cx, |hub, cx| hub.refresh(cx)).ok();
            }
        },
        cx,
    );
    cx.set_global(GlobalHubSync(hub));
}

impl HubSync {
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalHubSync>()
            .map(|global| global.0.clone())
    }

    pub fn status(&self) -> &HubStatus {
        &self.status
    }

    /// Whether the app is still finding out what the hub serves, e.g. at
    /// startup. Agents for remote projects wait for this rather than start
    /// locally and collide with the hub's copy of the same session.
    pub fn is_settling(&self) -> bool {
        self.status == HubStatus::Connecting
    }

    /// Where `project`'s agents run when they run on the hub.
    pub fn route(&self, project: &Project, cx: &App) -> Option<HubRoute> {
        let machine = project
            .remote_connection_options(cx)
            .and_then(agent_servers::machine_name);
        agent_servers::route_for(
            &self.settings,
            self.token.as_deref(),
            &self.machines,
            machine.as_deref(),
        )
    }

    /// Dispatch's saved conversations, newest first.
    pub fn conversations(&self) -> &[DispatchConversation] {
        &self.conversations
    }

    /// How many conversations the hub has, including ones not listed.
    pub fn conversation_total(&self) -> usize {
        self.conversation_total
    }

    /// Where Dispatch's saved conversations open: the hub serves them as if
    /// Dispatch were an agent on a machine of its own.
    pub fn dispatch_route(&self) -> Option<HubRoute> {
        if !matches!(self.status, HubStatus::Connected { .. }) {
            return None;
        }
        Some(HubRoute {
            address: self.address()?.to_owned(),
            token: self.token.clone()?,
            machine: agent_servers::DISPATCH_MACHINE.to_owned(),
        })
    }

    /// Lists Dispatch's conversations again now, e.g. after asking it
    /// something, instead of waiting for the next check.
    pub fn reload_conversations(&self, cx: &mut Context<Self>) {
        let request = self.request::<DispatchConversations>(
            vec![(
                agent_servers::DISPATCH_CONVERSATIONS_METHOD,
                json!({ "limit": CONVERSATIONS_LIMIT }),
            )],
            Duration::from_secs(10),
            cx,
        );
        let address = self.address().map(str::to_owned);
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                if this.address() == address.as_deref() {
                    this.apply_conversations(result, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn apply_conversations(
        &mut self,
        result: Result<DispatchConversations>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(listed) => {
                if listed.conversations != self.conversations
                    || listed.total != self.conversation_total
                {
                    self.conversations = listed.conversations;
                    self.conversation_total = listed.total;
                    cx.notify();
                }
            }
            // A hub from before saved conversations has no such method.
            Err(error) => log::debug!("listing Dispatch conversations: {error:#}"),
        }
    }

    pub(crate) fn address(&self) -> Option<&str> {
        self.settings
            .address
            .as_deref()
            .filter(|_| self.settings.enabled)
    }

    pub(crate) fn ask_dispatch(&self, prompt: String, cx: &App) -> Task<Result<DispatchReply>> {
        let sessions = self.importable_sessions(cx);
        let mut calls = Vec::new();
        if !sessions.is_empty() {
            calls.push(("_yonder/hub/import", json!({ "sessions": sessions })));
        }
        calls.push(("_yonder/dispatch/ask", json!({ "prompt": prompt })));
        // Give the hub time to return its own error if the 90-second turn times out.
        self.request(calls, Duration::from_secs(100), cx)
    }

    pub(crate) fn sync_session(
        &self,
        session_id: acp::SessionId,
        cx: &App,
    ) -> Task<Result<ThreadMetadata>> {
        let existing = ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry_by_session(&session_id).cloned());
        if let Some(existing) = existing {
            return Task::ready(Ok(existing));
        }

        let address = self.address().map(str::to_owned);
        let request = self.request::<HubSessions>(
            vec![("_yonder/hub/sessions", json!({}))],
            Duration::from_secs(10),
            cx,
        );
        cx.spawn(async move |cx| {
            let response = request.await?;
            cx.update(|cx| {
                let hub = HubSync::try_global(cx).context("The Yonder hub is unavailable.")?;
                if hub.read(cx).address() != address.as_deref() {
                    bail!("The Yonder hub changed. Ask Dispatch again.");
                }
                response.import_session(&session_id, cx)
            })
        })
    }

    fn request<Response: DeserializeOwned + Send + 'static>(
        &self,
        calls: Vec<(&'static str, Value)>,
        timeout: Duration,
        cx: &App,
    ) -> Task<Result<Response>> {
        let (Some(address), Some(token)) = (self.address(), self.token.as_ref()) else {
            return Task::ready(Err(anyhow!("Connect to the Yonder hub first.")));
        };
        if !matches!(self.status, HubStatus::Connected { .. }) {
            return Task::ready(Err(anyhow!("Connect to the Yonder hub first.")));
        }
        let address = address.to_owned();
        let token = token.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let request = agent_servers::call_hub(&address, &token, calls);
            futures::pin_mut!(request);
            match select(request, executor.timer(timeout)).await {
                Either::Left((result, _)) => {
                    let response = result?.pop().context("The hub returned no response.")?;
                    serde_json::from_value(response)
                        .context("The hub returned an invalid response.")
                }
                Either::Right(_) => bail!("The request to the Yonder hub timed out."),
            }
        })
    }

    /// Saves the hub's token in the Keychain and reconnects.
    pub fn set_token(&mut self, token: String, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        let Some(address) = self.settings.address.clone() else {
            return Task::ready(Err(anyhow::anyhow!("set the hub address first")));
        };
        cx.spawn(async move |this, cx| {
            agent_servers::write_hub_token(&address, token.trim(), cx).await?;
            this.update(cx, |this, cx| this.restart(cx))?;
            Ok(())
        })
    }

    /// Checks the hub again now.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.restart(cx);
    }

    fn restart(&mut self, cx: &mut Context<Self>) {
        self.settings = HubSettings::get_global(cx).clone();
        self._sync = None;
        self.conversations.clear();
        self.conversation_total = 0;
        let Some(address) = self
            .settings
            .address
            .clone()
            .filter(|_| self.settings.enabled)
        else {
            self.token = None;
            self.machines.clear();
            self.status = HubStatus::Off;
            cx.notify();
            return;
        };
        self.status = HubStatus::Connecting;
        cx.notify();
        self._sync = Some(cx.spawn(async move |this, cx| {
            let token = match agent_servers::read_hub_token(&address, cx).await {
                Ok(Some(token)) => token,
                Ok(None) => {
                    this.update(cx, |this, cx| this.set_status(HubStatus::NoToken, cx))
                        .ok();
                    return;
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.set_status(
                            HubStatus::Failed(format!("reading the token: {error:#}").into()),
                            cx,
                        )
                    })
                    .ok();
                    return;
                }
            };
            loop {
                let result = agent_servers::call_hub(
                    &address,
                    &token,
                    vec![
                        ("_yonder/hub/machines", json!({})),
                        ("_yonder/hub/sessions", json!({})),
                    ],
                )
                .await;
                let connected = result.is_ok();
                let keep_going = this
                    .update(cx, |this, cx| {
                        this.apply(&address, &token, result, cx);
                    })
                    .is_ok();
                if !keep_going {
                    return;
                }
                let mut interval = SYNC_INTERVAL;
                if connected {
                    let listed = agent_servers::call_hub(
                        &address,
                        &token,
                        vec![(
                            agent_servers::DISPATCH_CONVERSATIONS_METHOD,
                            json!({ "limit": CONVERSATIONS_LIMIT }),
                        )],
                    )
                    .await
                    .and_then(|mut results| {
                        let listed = results.pop().context("The hub returned no response.")?;
                        Ok(serde_json::from_value::<DispatchConversations>(listed)?)
                    });
                    let live = this
                        .update(cx, |this, cx| {
                            this.apply_conversations(listed, cx);
                            this.conversations.iter().any(DispatchConversation::is_live)
                        })
                        .unwrap_or(false);
                    if live {
                        interval = LIVE_SYNC_INTERVAL;
                    }
                }
                cx.background_executor().timer(interval).await;
            }
        }));
    }

    fn set_status(&mut self, status: HubStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn apply(
        &mut self,
        address: &str,
        token: &str,
        result: anyhow::Result<Vec<Value>>,
        cx: &mut Context<Self>,
    ) {
        let results = match result {
            Ok(results) => results,
            Err(error) => {
                // Keep the last known machines so open threads keep their
                // route while the hub is briefly unreachable.
                self.set_status(HubStatus::Failed(format!("{error:#}").into()), cx);
                return;
            }
        };
        let machines: Vec<String> = results
            .first()
            .and_then(|result| result.get("machines"))
            .and_then(|machines| serde_json::from_value(machines.clone()).ok())
            .unwrap_or_default();
        let sessions: Vec<HubSession> = results
            .get(1)
            .and_then(|result| result.get("sessions"))
            .and_then(|sessions| serde_json::from_value(sessions.clone()).ok())
            .unwrap_or_default();
        self.token = Some(token.to_string());
        self.machines = machines.clone();
        self.status = HubStatus::Connected {
            machines,
            sessions: sessions.len(),
        };
        cx.notify();

        if self.imported_to.as_deref() != Some(address) {
            self.imported_to = Some(address.to_string());
            self.import_threads(address.to_string(), token.to_string(), cx);
        }
        add_hub_sessions(sessions, cx);
    }

    fn importable_sessions(&self, cx: &App) -> Vec<Value> {
        let Some(store) = ThreadMetadataStore::try_global(cx) else {
            return Vec::new();
        };
        store
            .read(cx)
            .entries()
            .filter(|thread| !thread.archived && thread.agent_id != *agent::ZED_AGENT_ID)
            .filter_map(|thread| {
                let session_id = thread.session_id.as_ref()?;
                let machine = thread
                    .remote_connection
                    .as_ref()
                    .and_then(agent_servers::machine_name)?;
                if !self.machines.contains(&machine) {
                    return None;
                }
                let cwd = thread.folder_paths().ordered_paths().next()?.clone();
                Some(json!({
                    "id": session_id.to_string(),
                    "machine": machine,
                    "agent": thread.agent_id.to_string(),
                    "cwd": cwd.to_string_lossy(),
                    "title": thread.title().map(|title| title.to_string()),
                    "createdAt": thread.created_at.map(|created_at| created_at.timestamp()),
                    "updatedAt": thread.updated_at.timestamp(),
                }))
            })
            .collect()
    }

    fn import_threads(&self, address: String, token: String, cx: &mut Context<Self>) {
        let sessions = self.importable_sessions(cx);
        if sessions.is_empty() {
            return;
        }
        let count = sessions.len();
        cx.background_spawn(async move {
            match agent_servers::call_hub(
                &address,
                &token,
                vec![("_yonder/hub/import", json!({ "sessions": sessions }))],
            )
            .await
            {
                Ok(_) => log::info!("handed {count} threads to the hub"),
                Err(error) => log::warn!("handing threads to the hub: {error:#}"),
            }
        })
        .detach();
    }
}

/// Adds hub sessions the app doesn't know yet to the thread list, so threads
/// started elsewhere show up in the sidebar and switcher.
fn add_hub_sessions(sessions: Vec<HubSession>, cx: &mut App) {
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return;
    };
    let connections = cx
        .global::<SettingsStore>()
        .merged_settings()
        .remote
        .ssh_connections
        .clone()
        .unwrap_or_default();
    let missing: Vec<ThreadMetadata> = sessions
        .into_iter()
        .filter(|session| {
            store
                .read(cx)
                .entry_by_session(&acp::SessionId::new(session.id.clone()))
                .is_none()
        })
        .filter_map(|session| {
            let connection = connections.iter().find(|connection| {
                connection.nickname.as_deref() == Some(session.machine.as_str())
                    || connection.host == session.machine
            })?;
            let updated_at = Utc
                .timestamp_opt(session.updated_at, 0)
                .single()
                .unwrap_or_else(Utc::now);
            let created_at = Utc.timestamp_opt(session.created_at, 0).single();
            Some(ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: Some(acp::SessionId::new(session.id)),
                agent_id: AgentId(session.agent.into()),
                title: session.title.map(Into::into),
                title_override: None,
                updated_at,
                created_at,
                interacted_at: None,
                worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[PathBuf::from(
                    session.cwd,
                )])),
                remote_connection: Some(remote::RemoteConnectionOptions::Ssh(
                    remote::SshConnectionOptions::from(connection.clone()),
                )),
                archived: false,
            })
        })
        .collect();
    if missing.is_empty() {
        return;
    }
    log::info!("adding {} threads from the hub", missing.len());
    store.update(cx, |store, cx| {
        for thread in missing {
            store.save(thread, cx);
        }
    });
}

/// Opens a thread the hub runs, adding it to the thread list first if the
/// app hasn't seen it yet, e.g. from a step in a Dispatch conversation.
pub(crate) fn open_hub_thread(
    session_id: acp::SessionId,
    workspace: gpui::WeakEntity<workspace::Workspace>,
    window: &mut gpui::Window,
    cx: &mut App,
) {
    let Some(hub) = HubSync::try_global(cx) else {
        return;
    };
    let request = hub.read(cx).sync_session(session_id, cx);
    window
        .spawn(cx, async move |cx| match request.await {
            Ok(metadata) => {
                cx.update(|window, cx| {
                    crate::forward::open_thread(metadata, workspace.upgrade(), window, cx)
                })
                .ok();
            }
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| workspace.show_error(error, cx))
                    .ok();
            }
        })
        .detach();
}

/// Waits until the hub has said which machines it serves, for at most a few
/// seconds, so an agent starting at app launch picks the right place to run.
pub async fn wait_until_settled(hub: &Entity<HubSync>, cx: &mut AsyncApp) {
    for _ in 0..40 {
        if !hub.read_with(cx, |hub, _| hub.is_settling()) {
            return;
        }
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[test]
    fn dispatch_conversations_read_the_hub_listing() {
        let listed: DispatchConversations = serde_json::from_value(json!({
            "conversations": [
                {"id":"dispatch-call-1","title":null,"kind":"voice","state":"running","summary":"","createdAt":1,"updatedAt":2},
                {"id":"dispatch-typed-2","title":"Is it done?","kind":"typed","state":"idle","summary":"1 thread","createdAt":3,"updatedAt":4},
            ],
            "total": 9,
        }))
        .expect("the hub's listing parses");
        assert_eq!(listed.total, 9);
        let [call, typed] = listed.conversations.as_slice() else {
            panic!("two conversations");
        };
        assert!(call.is_voice() && call.is_live());
        assert_eq!(
            call.title().as_ref(),
            "Voice call",
            "untitled calls get a name"
        );
        assert!(!typed.is_voice() && !typed.is_live());
        assert_eq!(typed.title().as_ref(), "Is it done?");
        assert_eq!(typed.session_id().to_string(), "dispatch-typed-2");
        assert!(crate::Agent::from(project::AgentId("dispatch".into())).is_dispatch());
        assert_eq!(
            crate::Agent::from(project::AgentId("dispatch".into()))
                .label()
                .as_ref(),
            "Dispatch"
        );
        assert!(!crate::Agent::from(project::AgentId("codex-acp".into())).is_dispatch());
    }

    #[test]
    fn dispatch_reply_can_omit_the_thread() {
        let reply: DispatchReply = serde_json::from_value(json!({ "text": "No running threads." }))
            .expect("a reply can contain only text");
        assert_eq!(reply.text.as_ref(), "No running threads.");
        assert!(reply.open_session_id.is_none());
    }

    #[gpui::test]
    fn dispatch_imports_only_the_requested_session(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings = SettingsStore::test(cx);
            settings
                .set_user_settings(
                    r#"{ "ssh_connections": [{ "host": "gpu.example.test", "nickname": "gpu" }] }"#,
                    cx,
                )
                .expect("SSH connection settings should parse");
            cx.set_global(settings);
            ThreadMetadataStore::init_global(cx);

            let reply: DispatchReply = serde_json::from_value(json!({
                "text": "I found the thread.", "openSessionId": "requested"
            }))
            .expect("a reply can name a thread by its session ID");
            let requested_id = reply.open_session_id.expect("reply should include a session ID");
            let sessions: HubSessions = serde_json::from_value(json!({
                "sessions": [
                    { "id": "other", "machine": "gpu", "agent": "codex-acp", "cwd": "/other", "title": "Build" },
                    { "id": "requested", "machine": "gpu", "agent": "codex-acp", "cwd": "/project", "title": "Build" }
                ]
            }))
            .expect("the hub session list should parse");
            let metadata = sessions.import_session(&requested_id, cx).expect("thread should import");
            assert_eq!(metadata.session_id.as_ref(), Some(&requested_id));
            assert_eq!(
                metadata.remote_connection.as_ref().and_then(agent_servers::machine_name).as_deref(),
                Some("gpu")
            );
            let store = ThreadMetadataStore::global(cx);
            assert!(store.read(cx).entry_by_session(&acp::SessionId::new("other")).is_none());
        });
    }
}
