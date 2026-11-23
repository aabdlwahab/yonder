//! Opens a project on each machine the hub runs agents on: a local project
//! for the hub's own machine, and a remote project over SSH for the others,
//! the same way the Mac app does. Agents are started through these projects,
//! so their commands, installs and accounts resolve exactly as in the app.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::Shared;
use gpui::{App, AsyncApp, BorrowAppContext as _, Entity, Task};
use project::Project;
use recent_projects::RemoteSettings;
use release_channel::ReleaseChannel;
use remote::{
    ConnectionIdentifier, RemoteClient, RemoteConnectionOptions, RemotePlatform,
    SshConnectionOptions,
};
use settings::{Settings as _, SettingsStore};

use crate::headless::AgentCliAppState;

pub type ProjectTask = Shared<Task<Result<Entity<Project>, Arc<anyhow::Error>>>>;

pub struct Machines {
    app_state: Arc<AgentCliAppState>,
    /// The name of the machine the hub itself runs on.
    local_name: String,
    projects: HashMap<String, ProjectTask>,
    connections_opened: i64,
}

/// The hub's connections get their own ids on each machine's Yonder server.
/// Generic setup ids (`setup-1`, ...) would collide with Yonder Dev's own
/// background connections, and the server stops whichever process already
/// uses an id.
const HUB_CONNECTION_IDS: i64 = -7766_000;

impl Machines {
    pub fn new(app_state: Arc<AgentCliAppState>, local_name: String) -> Self {
        Self {
            app_state,
            local_name,
            projects: HashMap::default(),
            connections_opened: 0,
        }
    }

    /// The project for `machine`, opened once and shared.
    pub fn project(&mut self, machine: &str, cx: &mut App) -> ProjectTask {
        if let Some(task) = self.projects.get(machine) {
            return task.clone();
        }
        let task = if machine == self.local_name || machine == "local" {
            let project = Project::local(
                self.app_state.client.clone(),
                self.app_state.node_runtime.clone(),
                self.app_state.user_store.clone(),
                self.app_state.languages.clone(),
                self.app_state.fs.clone(),
                None,
                project::LocalProjectFlags {
                    init_worktree_trust: false,
                    ..Default::default()
                },
                cx,
            );
            Task::ready(Ok(project))
        } else {
            self.open_remote(machine, cx)
        };
        let task = task.map(|result| result.map_err(Arc::new)).boxed_local();
        let task = cx.foreground_executor().spawn(task).shared();
        self.projects.insert(machine.to_string(), task.clone());
        task
    }

    /// Forgets a machine's project, so the next agent start reconnects.
    pub fn forget(&mut self, machine: &str) {
        self.projects.remove(machine);
    }

    fn open_remote(&mut self, machine: &str, cx: &mut App) -> Task<Result<Entity<Project>>> {
        let Some(options) = RemoteSettings::get_global(cx)
            .ssh_connections()
            .find(|connection| {
                connection.nickname.as_deref() == Some(machine) || connection.host == machine
            })
            .map(SshConnectionOptions::from)
        else {
            return Task::ready(Err(anyhow!(
                "no machine named {machine} in ssh_connections on the hub"
            )));
        };
        let app_state = self.app_state.clone();
        let machine = machine.to_string();
        self.connections_opened += 1;
        let identifier =
            ConnectionIdentifier::Workspace(HUB_CONNECTION_IDS - self.connections_opened);
        cx.spawn(async move |cx| {
            let delegate: Arc<dyn remote::RemoteClientDelegate> = Arc::new(HubDelegate {
                machine: machine.clone(),
            });
            let connection =
                remote::connect(RemoteConnectionOptions::Ssh(options), delegate.clone(), cx)
                    .await
                    .with_context(|| format!("connecting to {machine}"))?;
            let (_cancel, cancellation) = oneshot::channel::<()>();
            let client = cx
                .update(|cx| RemoteClient::new(identifier, connection, cancellation, delegate, cx))
                .await
                .with_context(|| format!("starting Yonder's server on {machine}"))?
                .context("the connection was canceled")?;
            let project = cx.update(|cx| {
                Project::remote(
                    client,
                    app_state.client.clone(),
                    app_state.node_runtime.clone(),
                    app_state.user_store.clone(),
                    app_state.languages.clone(),
                    app_state.fs.clone(),
                    false,
                    cx,
                )
            });
            // A remote project sends the user's settings (agents, accounts) to
            // the machine's server only when they change. The hub loaded them
            // before any project existed, so touch the store to send them now;
            // without them the server lists no agents.
            cx.update(|cx| cx.update_global::<SettingsStore, _>(|_, _| {}));
            Ok(project)
        })
    }
}

/// The hub runs unattended: it never prompts, and it uses the Yonder server
/// already installed on each machine instead of downloading one.
struct HubDelegate {
    machine: String,
}

impl remote::RemoteClientDelegate for HubDelegate {
    fn ask_password(
        &self,
        prompt: String,
        _tx: oneshot::Sender<askpass::EncryptedPassword>,
        _cancellation: oneshot::Receiver<()>,
        _cx: &mut AsyncApp,
    ) {
        log::warn!(
            "{} asked for a password ({prompt}); the hub needs key-based SSH",
            self.machine
        );
    }

    fn get_download_url(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<semver::Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        Task::ready(Ok(None))
    }

    fn download_server_binary_locally(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<semver::Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>> {
        Task::ready(Err(anyhow!(
            "Yonder's server isn't installed on {}; connect to it once from the Mac app",
            self.machine
        )))
    }

    fn set_status(&self, status: Option<&str>, _cx: &mut AsyncApp) {
        if let Some(status) = status {
            log::info!("{}: {status}", self.machine);
        }
    }
}
