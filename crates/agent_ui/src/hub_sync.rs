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
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, Task};
use project::{AgentId, Project, WorktreePaths};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use settings::{Settings as _, SettingsStore};
use util::path_list::PathList;

use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore};

const SYNC_INTERVAL: Duration = Duration::from_secs(30);

pub struct HubSync {
    settings: HubSettings,
    token: Option<String>,
    machines: Vec<String>,
    status: HubStatus,
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

    pub(crate) fn address(&self) -> Option<&str> {
        self.settings
            .address
            .as_deref()
            .filter(|_| self.settings.enabled)
    }

    pub(crate) fn ask_dispatch(&self, prompt: String, cx: &App) -> Task<Result<DispatchReply>> {
        // Give the hub time to return its own error if the 90-second turn times out.
        self.request(
            "_yonder/dispatch/ask",
            json!({ "prompt": prompt }),
            Duration::from_secs(100),
            cx,
        )
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
            "_yonder/hub/sessions",
            json!({}),
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
        method: &'static str,
        params: Value,
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
            let request = agent_servers::call_hub(&address, &token, vec![(method, params)]);
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
                let keep_going = this
                    .update(cx, |this, cx| {
                        this.apply(&address, &token, result, cx);
                    })
                    .is_ok();
                if !keep_going {
                    return;
                }
                cx.background_executor().timer(SYNC_INTERVAL).await;
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

    /// Hands the app's threads on served machines to the hub, so it (and
    /// Dispatch) list them. The hub ignores ones it already knows.
    fn import_threads(&self, address: String, token: String, cx: &mut Context<Self>) {
        let Some(store) = ThreadMetadataStore::try_global(cx) else {
            return;
        };
        let sessions: Vec<Value> = store
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
                    "updatedAt": thread.updated_at.timestamp(),
                }))
            })
            .collect();
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
