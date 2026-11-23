use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use agent_ui::{
    Agent, AgentPanel, ThreadId,
    thread_metadata_store::{ThreadMetadata, ThreadMetadataStore},
};
use anyhow::{Result, anyhow};
use askpass::EncryptedPassword;
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent};
use futures::channel::oneshot;
use gpui::{
    Action, App, AsyncApp, AsyncWindowContext, Context, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, Global, Pixels, Render, SharedString, Subscription, Task,
    WeakEntity, Window, actions, px, relative,
};
use picker::{Picker, PickerDelegate};
use project::{Event as ProjectEvent, Project};
use recent_projects::{RemoteSettings, open_remote_project};
use release_channel::ReleaseChannel;
use remote::{
    ConnectionRoute, ConnectionState, RemoteClient, RemoteClientDelegate, RemoteClientEvent,
    RemoteConnection, RemoteConnectionOptions, RemotePlatform, SshConnectionOptions, SshRoutes,
    primary_answers, route_names,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};
use sidebar::{Sidebar, ThreadActivity, ThreadActivityChanged};
use ui::{
    ButtonLike, ContextMenu, Indicator, ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    AppState, MultiWorkspace, MultiWorkspaceEvent, OpenMode, OpenOptions, PathList,
    RecentWorkspace, SerializedWorkspaceLocation, Toast, Workspace, WorkspaceDb,
    dock::{DockPosition, Panel, PanelEvent, PanelHandle},
    notifications::{DetachAndPromptErr, NotificationId},
};
use zed_actions::OpenRemote;

mod dispatch_section;

actions!(
    fleet_panel,
    [
        /// Toggles focus on the fleet panel, which lists your machines and their projects.
        ToggleFocus,
        /// Searches projects and threads across machines.
        OpenSwitcher,
        /// Moves every machine with several addresses to the next network:
        /// Auto, then each network label in turn.
        CycleNetwork,
        /// Lets every machine pick its address automatically again.
        UseAutomaticNetwork
    ]
);

const FLEET_PANEL_KEY: &str = "FleetPanel";

/// How long the pointer must rest on a project before its machine is connected in advance.
const PREWARM_HOVER_DELAY: Duration = Duration::from_millis(250);

/// The laptop itself, listed above the SSH machines.
const LOCAL_MACHINE_KEY: &str = "local";
const LOCAL_MACHINE_LABEL: &str = "This Mac";
/// Past projects listed per machine, after the open ones. High enough that a
/// machine's whole project history fits; machines can be collapsed.
const RECENT_PROJECTS_PER_MACHINE: usize = 50;

struct FleetActivity;
impl Global for FleetActivity {}

pub fn init(cx: &mut App) {
    cx.set_global(FleetActivity);
    agent_ui::set_thread_opener(open_thread_metadata, cx);
    load_routes(cx);
    watch_primaries(cx);
    cx.on_window_closed(|cx, _| cx.update_global::<FleetActivity, _>(|_, _| {}))
        .detach();
    cx.observe_new::<Sidebar>(|_, _, cx| {
        cx.subscribe(&cx.entity(), |_, _, _: &ThreadActivityChanged, cx| {
            cx.update_global::<FleetActivity, _>(|_, _| {});
        })
        .detach();
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<FleetPanel>(window, cx);
        });
        workspace.register_action(|_, _: &CycleNetwork, _, cx| {
            let names = network_names(&configured_machines(cx));
            let current = cx
                .try_global::<SshRoutes>()
                .and_then(|routes| routes.fleet.clone());
            set_fleet_route(next_route(current.as_deref(), &names), cx);
        });
        workspace.register_action(|_, _: &UseAutomaticNetwork, _, cx| {
            set_fleet_route(None, cx);
        });
        workspace.register_action(|workspace, _: &OpenSwitcher, window, cx| {
            let workspace_handle = cx.weak_entity();
            let projects = workspace
                .panel::<FleetPanel>(cx)
                .map(|panel| {
                    panel
                        .read(cx)
                        .machines(workspace, cx)
                        .into_iter()
                        .flat_map(|machine| machine.projects)
                        .collect()
                })
                .unwrap_or_default();
            let threads = if window
                .root::<MultiWorkspace>()
                .flatten()
                .is_some_and(|workspace| workspace.read(cx).multi_workspace_enabled(cx))
            {
                fleet_threads(false, window, cx)
            } else {
                Vec::new()
            };
            workspace.toggle_modal(window, cx, |window, cx| {
                Picker::uniform_list(
                    FleetSwitcherDelegate::new(workspace_handle, projects, threads),
                    window,
                    cx,
                )
            });
        });
    })
    .detach();
}

#[derive(Clone)]
struct FleetThread {
    metadata: ThreadMetadata,
    location: SharedString,
    activity: Option<ThreadActivity>,
    remote_control: bool,
}

fn fleet_threads(active_only: bool, window: &Window, cx: &App) -> Vec<FleetThread> {
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return Vec::new();
    };
    let mut activity = HashMap::<ThreadId, ThreadActivity>::default();
    let mut remote_control = HashSet::<ThreadId>::default();
    let current_sidebar = window
        .root::<MultiWorkspace>()
        .flatten()
        .and_then(|workspace| sidebar_for_window(workspace.read(cx)));
    let other_sidebars = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .filter_map(|window| sidebar_for_window(window.read(cx).ok()?));
    for sidebar in current_sidebar.into_iter().chain(other_sidebars) {
        let sidebar = sidebar.read(cx);
        for (&id, &status) in sidebar.thread_activity() {
            activity
                .entry(id)
                .and_modify(|current| *current = (*current).max(status))
                .or_insert(status);
        }
        remote_control.extend(sidebar.thread_remote_control().iter().copied());
    }
    let mut threads = store
        .read(cx)
        .entries()
        .filter(|thread| {
            !thread.archived
                && (!thread.is_draft() || !active_only && thread.has_saved_draft_prompt(cx))
        })
        .filter(|thread| !active_only || activity.contains_key(&thread.thread_id))
        .map(|thread| {
            let machine = match &thread.remote_connection {
                Some(RemoteConnectionOptions::Ssh(options)) => ssh_machine_label(options),
                Some(options) => options.display_name().into(),
                None => LOCAL_MACHINE_LABEL.into(),
            };
            let project = thread
                .folder_paths()
                .ordered_paths()
                .next()
                .and_then(|path| path.file_name());
            let location = project.map_or_else(
                || machine.clone(),
                |name| format!("{} · {machine}", name.to_string_lossy()).into(),
            );
            FleetThread {
                metadata: thread.clone(),
                location,
                activity: activity.get(&thread.thread_id).copied(),
                remote_control: remote_control.contains(&thread.thread_id),
            }
        })
        .collect::<Vec<_>>();
    sort_by_attention(&mut threads);
    threads
}

/// Approvals first, then running, then unread; newest first within each.
fn sort_by_attention(threads: &mut [FleetThread]) {
    threads.sort_by_key(|thread| std::cmp::Reverse((thread.activity, thread.metadata.updated_at)));
}

fn sidebar_for_window(workspace: &MultiWorkspace) -> Option<Entity<Sidebar>> {
    workspace.sidebar()?.to_any().downcast::<Sidebar>().ok()
}

fn open_fleet_thread(thread: FleetThread, window: &mut Window, cx: &mut App) {
    open_thread_metadata(thread.metadata, window, cx);
}

/// Opens a thread in its own project, on its own machine, switching windows
/// or opening the project when needed.
fn open_thread_metadata(metadata: ThreadMetadata, window: &mut Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        let Some(sidebar) = window
            .root::<MultiWorkspace>()
            .flatten()
            .and_then(|workspace| sidebar_for_window(workspace.read(cx)))
        else {
            return;
        };
        sidebar.update(cx, |sidebar, cx| {
            sidebar.open_known_thread(metadata, window, cx)
        });
    });
}

fn thread_activity_style(activity: ThreadActivity) -> (IconName, Color, &'static str) {
    match activity {
        ThreadActivity::WaitingForApproval => {
            (IconName::Warning, Color::Warning, "Waiting for approval")
        }
        ThreadActivity::Running => (IconName::PlayFilled, Color::Success, "Running"),
        ThreadActivity::Unread => (IconName::Circle, Color::Accent, "Unread"),
    }
}

/// The status pill on an inbox card. An unread thread is one that finished
/// while you were looking elsewhere.
fn inbox_status(activity: ThreadActivity) -> (&'static str, Color) {
    match activity {
        ThreadActivity::WaitingForApproval => ("Needs approval", Color::Error),
        ThreadActivity::Running => ("Running", Color::Warning),
        ThreadActivity::Unread => ("Done", Color::Success),
    }
}

fn short_age(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - updated_at).num_seconds().max(0);
    match seconds {
        0..60 => "now".to_string(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[derive(Clone)]
struct FleetMachine {
    key: String,
    label: SharedString,
    projects: Vec<FleetProject>,
}

/// Where a project's files live.
#[derive(Clone)]
enum ProjectLocation {
    /// On this laptop, opened directly.
    Local,
    /// On an SSH machine, opened through the Yonder server there.
    Ssh(SshConnectionOptions),
}

#[derive(Clone)]
struct FleetProject {
    location: ProjectLocation,
    machine_key: String,
    machine_label: SharedString,
    name: SharedString,
    paths: PathList,
}

impl FleetProject {
    fn new(location: ProjectLocation, paths: PathList) -> Option<Self> {
        let first = paths.ordered_paths().next()?;
        let name = first
            .file_name()
            .unwrap_or(first.as_os_str())
            .to_string_lossy()
            .into_owned();
        let (machine_key, machine_label) = match &location {
            ProjectLocation::Local => (LOCAL_MACHINE_KEY.to_string(), LOCAL_MACHINE_LABEL.into()),
            ProjectLocation::Ssh(options) => (ssh_machine_key(options), ssh_machine_label(options)),
        };
        Some(Self {
            location,
            machine_key,
            machine_label,
            name: name.into(),
            paths,
        })
    }

    fn from_workspace(workspace: &Workspace, cx: &App) -> Option<Self> {
        let project = workspace.project().read(cx);
        let location = match project.remote_connection_options(cx) {
            None if project.is_local() => ProjectLocation::Local,
            Some(RemoteConnectionOptions::Ssh(options)) => ProjectLocation::Ssh(options.clone()),
            _ => return None,
        };
        let paths = project
            .visible_worktrees(cx)
            .filter_map(|worktree| {
                let worktree = worktree.read(cx);
                worktree
                    .root_entry()
                    .is_none_or(|entry| entry.is_dir())
                    .then(|| worktree.abs_path().to_path_buf())
            })
            .collect::<Vec<_>>();
        Self::new(location, PathList::new(&paths))
    }

    fn from_recent(recent: RecentWorkspace) -> Option<Self> {
        let location = match recent.location {
            SerializedWorkspaceLocation::Local => ProjectLocation::Local,
            SerializedWorkspaceLocation::Remote(RemoteConnectionOptions::Ssh(options)) => {
                ProjectLocation::Ssh(options)
            }
            _ => return None,
        };
        Self::new(location, recent.paths)
    }

    fn description(&self) -> String {
        self.paths
            .ordered_paths()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn ssh_machine_key(options: &SshConnectionOptions) -> String {
    machine_key(
        options.username.as_deref(),
        &options.host.to_string(),
        options.port,
    )
}

fn ssh_machine_label(options: &SshConnectionOptions) -> SharedString {
    options
        .nickname
        .clone()
        .unwrap_or_else(|| options.host.to_string())
        .into()
}

/// Machines from the `ssh_connections` setting.
fn configured_machines(cx: &App) -> Vec<SshConnectionOptions> {
    RemoteSettings::get_global(cx)
        .ssh_connections()
        .map(SshConnectionOptions::from)
        .collect()
}

/// Maps the key of each secondary address of a configured machine to that
/// machine, so a project opened over any of its addresses lists under it.
fn primary_addresses(configured: &[SshConnectionOptions]) -> HashMap<String, SshConnectionOptions> {
    let mut primaries = HashMap::default();
    for options in configured {
        for secondary in &options.secondary_hosts {
            let key = machine_key(
                options.username.as_deref(),
                &secondary.host,
                secondary.port.or(options.port),
            );
            primaries.entry(key).or_insert_with(|| options.clone());
        }
    }
    primaries
}

impl FleetProject {
    /// Moves a project saved under a secondary address to its machine's primary.
    fn on_primary(mut self, primaries: &HashMap<String, SshConnectionOptions>) -> Self {
        if let Some(primary) = primaries.get(&self.machine_key) {
            self.machine_key = ssh_machine_key(primary);
            self.machine_label = ssh_machine_label(primary);
            if let ProjectLocation::Ssh(options) = &mut self.location {
                options.host = primary.host.clone();
                options.port = primary.port;
            }
        }
        self
    }
}

fn group_projects(
    configured_hosts: impl IntoIterator<Item = SshConnectionOptions>,
    open_projects: impl IntoIterator<Item = FleetProject>,
    recent_projects: impl IntoIterator<Item = FleetProject>,
) -> Vec<FleetMachine> {
    let configured_hosts = configured_hosts.into_iter().collect::<Vec<_>>();
    let primaries = primary_addresses(&configured_hosts);
    let mut machines = vec![FleetMachine {
        key: LOCAL_MACHINE_KEY.to_string(),
        label: LOCAL_MACHINE_LABEL.into(),
        projects: Vec::new(),
    }];
    for options in configured_hosts {
        let key = ssh_machine_key(&options);
        if !machines.iter().any(|machine| machine.key == key) {
            machines.push(FleetMachine {
                key,
                label: ssh_machine_label(&options),
                projects: Vec::new(),
            });
        }
    }

    let open_projects = open_projects
        .into_iter()
        .map(|project| (true, project.on_primary(&primaries)));
    let recent_projects = recent_projects
        .into_iter()
        .map(|project| (false, project.on_primary(&primaries)));
    let mut recent_counts = HashMap::default();
    for (is_open, mut project) in open_projects.chain(recent_projects) {
        let index = machines
            .iter()
            .position(|machine| machine.key == project.machine_key)
            .unwrap_or_else(|| {
                let index = machines.len();
                machines.push(FleetMachine {
                    key: project.machine_key.clone(),
                    label: project.machine_label.clone(),
                    projects: Vec::new(),
                });
                index
            });
        let machine = &mut machines[index];
        if machine
            .projects
            .iter()
            .any(|listed| listed.paths == project.paths)
        {
            continue;
        }
        if !is_open {
            let count = recent_counts.entry(index).or_insert(0);
            if *count >= RECENT_PROJECTS_PER_MACHINE {
                continue;
            }
            *count += 1;
        }
        project.machine_label = machine.label.clone();
        machine.projects.push(project);
    }

    machines
}

fn connection_options(connection: &SshConnectionOptions, cx: &App) -> RemoteConnectionOptions {
    let mut options = connection.clone();
    RemoteSettings::get_global(cx).fill_connection_options_from_settings(&mut options);
    RemoteConnectionOptions::Ssh(options)
}

fn machine_key(username: Option<&str>, host: &str, port: Option<u16>) -> String {
    let mut key = String::new();
    if let Some(username) = username {
        key.push_str(username);
        key.push('@');
    }
    key.push_str(host);
    if let Some(port) = port {
        key.push_str(&format!(":{port}"));
    }
    key
}

/// Opens `project` in the window that holds `workspace`, logging how long the switch took.
fn open_fleet_project(
    project: &FleetProject,
    app_state: Arc<AppState>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let connection = match &project.location {
        ProjectLocation::Local => {
            open_local_project(project, window, cx);
            return;
        }
        ProjectLocation::Ssh(connection) => connection,
    };
    let options = connection_options(connection, cx);
    let connection_state = if remote::has_active_connection(&options, cx) {
        "warm"
    } else {
        "cold"
    };
    let open_options = OpenOptions {
        requesting_window: window.window_handle().downcast::<MultiWorkspace>(),
        ..Default::default()
    };
    let paths = project.paths.ordered_paths().cloned().collect();
    let label = format!("{}/{}", project.machine_label, project.name);
    let started = Instant::now();
    cx.spawn_in(window, async move |_, cx| {
        open_remote_project(options, paths, app_state, open_options, cx).await?;
        log::info!(
            "fleet: switched to {label} in {} ms (connection was {connection_state})",
            started.elapsed().as_millis()
        );
        anyhow::Ok(())
    })
    .detach_and_prompt_err("Failed to open project", window, cx, |_, _, _| None);
}

/// Opens a project on this laptop in the current window. Deferred, because the
/// window is still being updated while this runs.
fn open_local_project(project: &FleetProject, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
        return;
    };
    let paths = project.paths.ordered_paths().cloned().collect();
    let label = format!("{}/{}", project.machine_label, project.name);
    cx.defer(move |cx| {
        let started = Instant::now();
        let Some(task) = handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.open_project(paths, OpenMode::Activate, window, cx)
            })
            .log_err()
        else {
            return;
        };
        cx.spawn(async move |_| {
            if task.await.log_err().is_some() {
                log::info!(
                    "fleet: switched to {label} in {} ms (local)",
                    started.elapsed().as_millis()
                );
            }
        })
        .detach();
    });
}

const ROUTES_KEY: &str = "fleet_panel_ssh_routes";

#[derive(Default, Serialize, Deserialize)]
struct SavedRoutes {
    fleet: Option<String>,
    #[serde(default)]
    pins: BTreeMap<String, String>,
}

fn load_routes(cx: &mut App) {
    let saved = KeyValueStore::global(cx)
        .read_kvp(ROUTES_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<SavedRoutes>(&json).log_err())
        .unwrap_or_default();
    cx.set_global(SshRoutes {
        fleet: saved.fleet,
        pins: saved.pins.into_iter().collect(),
    });
}

fn save_routes(cx: &App) {
    let Some(routes) = cx.try_global::<SshRoutes>() else {
        return;
    };
    let saved = SavedRoutes {
        fleet: routes.fleet.clone(),
        pins: routes
            .pins
            .iter()
            .map(|(machine, route)| (machine.clone(), route.clone()))
            .collect(),
    };
    let Some(json) = serde_json::to_string(&saved).log_err() else {
        return;
    };
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.write_kvp(ROUTES_KEY.to_string(), json).await.log_err();
    })
    .detach();
}

/// Network labels of machines that have several addresses, in the order they
/// first appear in settings.
fn network_names(configured: &[SshConnectionOptions]) -> Vec<String> {
    let mut names = Vec::new();
    for options in configured
        .iter()
        .filter(|options| !options.secondary_hosts.is_empty())
    {
        let labels = std::iter::once(options.host_label.clone()).chain(
            options
                .secondary_hosts
                .iter()
                .map(|secondary| secondary.label.clone()),
        );
        for label in labels.flatten() {
            if !names.contains(&label) {
                names.push(label);
            }
        }
    }
    names
}

/// The route after `current` when cycling: Auto, then each name, then Auto.
fn next_route(current: Option<&str>, names: &[String]) -> Option<String> {
    match current {
        None => names.first().cloned(),
        Some(current) => names
            .iter()
            .position(|name| name == current)
            .and_then(|index| names.get(index + 1))
            .cloned(),
    }
}

/// A machine's addresses as (label, host, is_primary), primary first.
fn machine_addresses(options: &SshConnectionOptions) -> Vec<(Option<String>, String, bool)> {
    std::iter::once((options.host_label.clone(), options.host.to_string(), true))
        .chain(
            options
                .secondary_hosts
                .iter()
                .map(|secondary| (secondary.label.clone(), secondary.host.clone(), false)),
        )
        .collect()
}

fn machine_has_route(options: &SshConnectionOptions, route: &str) -> bool {
    machine_addresses(options)
        .iter()
        .any(|(label, host, _)| route_names(route, label.as_deref(), host))
}

/// The route the user chose for this machine, if it has an address on it.
fn chosen_route<'a>(options: &SshConnectionOptions, routes: &'a SshRoutes) -> Option<&'a str> {
    routes
        .route_for(options)
        .filter(|route| machine_has_route(options, route))
}

/// Whether the connection already uses the route the user chose, or the
/// primary when the machine is on Auto.
fn route_satisfied(
    options: &SshConnectionOptions,
    routes: &SshRoutes,
    current: &ConnectionRoute,
) -> bool {
    match chosen_route(options, routes) {
        Some(route) => route_names(route, current.label.as_deref(), &current.host),
        None => current.is_primary,
    }
}

fn set_fleet_route(route: Option<String>, cx: &mut App) {
    let routes = cx.default_global::<SshRoutes>();
    if routes.fleet == route {
        return;
    }
    routes.fleet = route;
    save_routes(cx);
    // Deferred so every window is back in the app: the one handling this
    // click is checked out while its handlers run.
    cx.defer(apply_routes);
}

fn set_machine_pin(machine: &SshConnectionOptions, route: Option<String>, cx: &mut App) {
    let key = machine.connection_string();
    let routes = cx.default_global::<SshRoutes>();
    if routes.pins.get(&key) == route.as_ref() {
        return;
    }
    match route {
        Some(route) => {
            routes.pins.insert(key, route);
        }
        None => {
            routes.pins.remove(&key);
        }
    }
    save_routes(cx);
    // Deferred so every window is back in the app: the one handling this
    // click is checked out while its handlers run.
    cx.defer(apply_routes);
}

/// Every remote client in every window, once each. While a window is being
/// drawn or handles an event it can't be read through the app, so the caller
/// passes it as `current`.
fn remote_clients(current: Option<&Window>, cx: &App) -> Vec<Entity<RemoteClient>> {
    let current = current.and_then(|window| window.root::<MultiWorkspace>().flatten());
    let others = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .filter_map(|window| window.read(cx).ok());
    let mut seen = HashSet::default();
    let mut clients = Vec::new();
    for multi in current
        .as_ref()
        .map(|multi| multi.read(cx))
        .into_iter()
        .chain(others)
    {
        for workspace in multi.workspaces() {
            if let Some(client) = workspace.read(cx).project().read(cx).remote_client()
                && seen.insert(client.entity_id())
            {
                clients.push(client);
            }
        }
    }
    clients
}

fn ssh_options(client: &RemoteClient) -> Option<&SshConnectionOptions> {
    match client.connection_options() {
        RemoteConnectionOptions::Ssh(options) => Some(options),
        _ => None,
    }
}

/// Reconnects every machine whose connection doesn't match the chosen route.
/// The remote server keeps running, so open projects carry on.
fn apply_routes(cx: &mut App) {
    let routes = cx.try_global::<SshRoutes>().cloned().unwrap_or_default();
    for client in remote_clients(None, cx) {
        let stale = {
            let client = client.read(cx);
            match (ssh_options(client), client.route()) {
                (Some(options), Some(current)) => !route_satisfied(options, &routes, &current),
                _ => false,
            }
        };
        if stale {
            client.update(cx, |client, cx| {
                if let Some(options) = ssh_options(client) {
                    log::info!(
                        "fleet: reconnecting {} for its network choice",
                        options.connection_string()
                    );
                }
                client.reconnect_now(cx).log_err();
            });
        }
    }

    let warm = cx.default_global::<WarmConnections>();
    let stale_keys = warm
        .connections
        .iter()
        .filter(
            |(_, connection)| match (connection.connection_options(), connection.route()) {
                (RemoteConnectionOptions::Ssh(options), Some(current)) => {
                    !route_satisfied(&options, &routes, &current)
                }
                _ => false,
            },
        )
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let stale = stale_keys
        .iter()
        .filter_map(|key| warm.connections.remove(key))
        .collect::<Vec<_>>();
    for connection in stale {
        cx.spawn(async move |_| connection.kill().await.log_err())
            .detach();
    }

    revive_disconnected(cx);
}

/// Reopens projects on machines that gave up reconnecting, so changing the
/// network brings them back. Once a client gives up, its project has dropped
/// its files and language servers, so this reopens the project the way the
/// Disconnected overlay's Reconnect button does.
fn revive_disconnected(cx: &mut App) {
    let mut reopen = Vec::new();
    for window in cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
    {
        let Ok(multi) = window.read(cx) else {
            continue;
        };
        for workspace in multi.workspaces() {
            let workspace = workspace.read(cx);
            let Some(client) = workspace.project().read(cx).remote_client() else {
                continue;
            };
            let client = client.read(cx);
            if client.connection_state() != ConnectionState::Disconnected {
                continue;
            }
            let Some(options) =
                ssh_options(client).filter(|options| !options.secondary_hosts.is_empty())
            else {
                continue;
            };
            let paths = workspace
                .root_paths(cx)
                .iter()
                .map(|path| path.to_path_buf())
                .collect::<Vec<_>>();
            reopen.push((
                window,
                options.clone(),
                paths,
                workspace.app_state().clone(),
            ));
        }
    }
    for (window, options, paths, app_state) in reopen {
        log::info!(
            "fleet: reopening {:?} on {} after a network change",
            paths,
            options.connection_string()
        );
        cx.spawn(async move |cx| {
            open_remote_project(
                RemoteConnectionOptions::Ssh(options),
                paths,
                app_state,
                OpenOptions {
                    requesting_window: Some(window),
                    ..Default::default()
                },
                cx,
            )
            .await
            .log_err();
        })
        .detach();
    }
}

/// How often to check whether machines that fell back can reach their
/// primary address again.
const PRIMARY_RECHECK: Duration = Duration::from_secs(30);

/// Machines on Auto that fell back to a secondary address and whose primary
/// answers again, by machine key.
#[derive(Default)]
struct PrimaryWatch {
    available: HashSet<String>,
}

impl Global for PrimaryWatch {}

fn watch_primaries(cx: &mut App) {
    cx.set_global(PrimaryWatch::default());
    cx.spawn(async move |cx| {
        let executor = cx.background_executor().clone();
        loop {
            executor.timer(PRIMARY_RECHECK).await;
            let candidates = cx.update(|cx| fallback_machines(cx));
            let mut available = HashSet::default();
            for (key, options) in candidates {
                if primary_answers(&options, &executor).await {
                    available.insert(key);
                }
            }
            cx.update(|cx| {
                if cx.global::<PrimaryWatch>().available != available {
                    cx.update_global::<PrimaryWatch, _>(|watch, _| watch.available = available);
                }
            });
        }
    })
    .detach();
}

/// Connected machines on Auto that use a secondary address.
fn fallback_machines(cx: &App) -> Vec<(String, SshConnectionOptions)> {
    let routes = cx.try_global::<SshRoutes>().cloned().unwrap_or_default();
    let mut machines = Vec::<(String, SshConnectionOptions)>::new();
    for client in remote_clients(None, cx) {
        let client = client.read(cx);
        let (Some(options), Some(route)) = (ssh_options(client), client.route()) else {
            continue;
        };
        if route.is_primary || chosen_route(options, &routes).is_some() {
            continue;
        }
        let key = ssh_machine_key(options);
        if !machines.iter().any(|(existing, _)| *existing == key) {
            machines.push((key, options.clone()));
        }
    }
    machines
}

/// Reconnects a machine's clients so Auto picks its address again.
fn reconnect_machine(key: &str, cx: &mut App) {
    for client in remote_clients(None, cx) {
        let matches =
            ssh_options(client.read(cx)).is_some_and(|options| ssh_machine_key(options) == key);
        if matches {
            log::info!("fleet: reconnecting {key} to move back to its primary address");
            client.update(cx, |client, cx| client.reconnect_now(cx).log_err());
        }
    }
}

struct RouteToast;

fn route_menu(
    options: &SshConnectionOptions,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let routes = cx.try_global::<SshRoutes>().cloned().unwrap_or_default();
    let pin = routes.pins.get(&options.connection_string()).cloned();
    let fleet = routes
        .fleet
        .filter(|route| machine_has_route(options, route));
    let machine = ssh_machine_label(options);
    let options = options.clone();
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        menu = menu
            .header(format!("Connect {machine} via"))
            .toggleable_entry(
                format!("Fleet setting ({})", fleet.as_deref().unwrap_or("Auto")),
                pin.is_none(),
                IconPosition::Start,
                None,
                {
                    let options = options.clone();
                    move |_, cx| set_machine_pin(&options, None, cx)
                },
            );
        for (label, host, is_primary) in machine_addresses(&options) {
            let route = label.clone().unwrap_or_else(|| host.clone());
            let mut text = match &label {
                Some(label) => format!("{label} · {host}"),
                None => host,
            };
            if is_primary {
                text.push_str(" (primary)");
            }
            menu = menu.toggleable_entry(
                text,
                pin.as_deref() == Some(route.as_str()),
                IconPosition::Start,
                None,
                {
                    let options = options.clone();
                    move |_, cx| set_machine_pin(&options, Some(route.clone()), cx)
                },
            );
        }
        menu.separator().entry(
            "Edit Addresses…",
            Some(zed_actions::OpenSettingsFile.boxed_clone()),
            |window, cx| window.dispatch_action(zed_actions::OpenSettingsFile.boxed_clone(), cx),
        )
    })
}

/// Connections opened ahead of a click. The global connection pool only keeps
/// connections that something still holds, so they are held here.
#[derive(Default)]
struct WarmConnections {
    connections: HashMap<String, Arc<dyn RemoteConnection>>,
    connecting: HashSet<String>,
}

impl Global for WarmConnections {}

/// Connects to `project`'s machine in the background so opening it skips the
/// SSH handshake and server check. Does nothing if a connection already exists.
fn prewarm(project: &FleetProject, cx: &mut App) {
    let ProjectLocation::Ssh(connection) = &project.location else {
        return;
    };
    let options = connection_options(connection, cx);
    if remote::has_active_connection(&options, cx) {
        return;
    }
    let key = project.machine_key.clone();
    if !cx
        .default_global::<WarmConnections>()
        .connecting
        .insert(key.clone())
    {
        return;
    }
    let started = Instant::now();
    let delegate: Arc<dyn RemoteClientDelegate> = Arc::new(PrewarmDelegate);
    cx.spawn(async move |cx| {
        let result = remote::connect(options, delegate, cx).await;
        cx.update(|cx| {
            let warm = cx.default_global::<WarmConnections>();
            warm.connecting.remove(&key);
            match result {
                Ok(connection) => {
                    log::info!(
                        "fleet: pre-warmed {key} in {} ms",
                        started.elapsed().as_millis()
                    );
                    warm.connections.insert(key, connection);
                }
                Err(error) => log::warn!("fleet: could not pre-warm {key}: {error:#}"),
            }
        });
    })
    .detach();
}

/// Background connections never show UI. A machine that needs a password is
/// left for the regular connection dialog when the project is actually opened.
struct PrewarmDelegate;

impl RemoteClientDelegate for PrewarmDelegate {
    fn ask_password(
        &self,
        prompt: String,
        _tx: oneshot::Sender<EncryptedPassword>,
        _cancellation: oneshot::Receiver<()>,
        _cx: &mut AsyncApp,
    ) {
        log::info!("fleet: not pre-warming a machine that asks for a password ({prompt})");
    }

    fn get_download_url(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        Task::ready(Ok(None))
    }

    fn download_server_binary_locally(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>> {
        Task::ready(Err(anyhow!(
            "pre-warming does not install the Yonder server on a machine"
        )))
    }

    fn set_status(&self, _status: Option<&str>, _cx: &mut AsyncApp) {}
}

/// Lists open and recent projects by machine and opens them in the current window.
pub struct FleetPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    file_tree: Option<Arc<dyn PanelHandle>>,
    collapsed_machines: HashSet<String>,
    hover_prewarm: Option<Task<()>>,
    app_state: Arc<AppState>,
    recent_projects: Vec<FleetProject>,
    load_recent_projects: Option<Task<()>>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    _workspace_subscription: Option<Subscription>,
    _project_subscription: Subscription,
    _activation_subscription: Subscription,
    _settings_subscription: Subscription,
    _primary_watch_subscription: Subscription,
    _activity_subscription: Subscription,
    _thread_subscription: Option<Subscription>,
    /// Remote clients the panel re-renders for, so route badges stay current.
    observed_clients: HashMap<EntityId, [Subscription; 2]>,
    /// The route each machine was last seen on, to tell when it moves.
    last_routes: HashMap<String, ConnectionRoute>,
    show_all_conversations: bool,
    conversation_filter: Entity<Editor>,
    /// Re-renders every second while a Dispatch conversation is going.
    live_tick: Option<Task<()>>,
    _hub_subscription: Option<Subscription>,
    _filter_subscription: Subscription,
}

impl FleetPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let workspace_handle = workspace.clone();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let app_state = workspace.app_state().clone();
            cx.new(|cx| Self::new(workspace_handle, app_state, workspace.project(), window, cx))
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        app_state: Arc<AppState>,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let conversation_filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search conversations…", window, cx);
            editor
        });
        let filter_subscription = cx.subscribe(&conversation_filter, |_, _, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.notify();
            }
        });
        let mut panel = Self {
            workspace,
            app_state,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            file_tree: None,
            collapsed_machines: HashSet::default(),
            hover_prewarm: None,
            recent_projects: Vec::new(),
            load_recent_projects: None,
            multi_workspace: None,
            _workspace_subscription: None,
            _project_subscription: cx.subscribe(project, |_, _, event, cx| {
                if matches!(
                    event,
                    ProjectEvent::WorktreeAdded(_)
                        | ProjectEvent::WorktreeRemoved(_)
                        | ProjectEvent::WorktreeOrderChanged
                        | ProjectEvent::WorktreePathsChanged { .. }
                ) {
                    cx.notify();
                }
            }),
            _activation_subscription: cx.observe_window_activation(window, |panel, window, cx| {
                if window.is_window_active() {
                    panel.refresh_recent_projects(cx);
                }
            }),
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
            _primary_watch_subscription: cx.observe_global::<PrimaryWatch>(|_, cx| cx.notify()),
            _activity_subscription: cx.observe_global::<FleetActivity>(|_, cx| cx.notify()),
            _thread_subscription: ThreadMetadataStore::try_global(cx)
                .map(|store| cx.observe(&store, |_, _, cx| cx.notify())),
            observed_clients: HashMap::default(),
            last_routes: HashMap::default(),
            show_all_conversations: false,
            conversation_filter,
            live_tick: None,
            _hub_subscription: Self::observe_hub(cx),
            _filter_subscription: filter_subscription,
        };
        panel.refresh_recent_projects(cx);
        panel
    }

    pub fn set_file_tree(&mut self, file_tree: Arc<dyn PanelHandle>, cx: &mut Context<Self>) {
        self.file_tree = Some(file_tree);
        cx.notify();
    }

    /// Runs while the workspace may still be mid-update (the panel is created
    /// inside it), so it must not read the workspace entity.
    fn refresh_recent_projects(&mut self, cx: &mut Context<Self>) {
        let fs = self.app_state.fs.clone();
        let db = WorkspaceDb::global(cx);
        self.load_recent_projects = Some(cx.spawn(async move |this, cx| {
            let recent = db
                .recent_project_workspaces(fs.as_ref())
                .await
                .log_err()
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                this.recent_projects = recent
                    .into_iter()
                    .filter_map(FleetProject::from_recent)
                    .collect();
                cx.notify();
            })
            .log_err();
        }));
    }

    fn toggle_machine(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed_machines.remove(key) {
            self.collapsed_machines.insert(key.to_string());
        }
        cx.notify();
    }

    fn hover_project(&mut self, project: &FleetProject, hovered: bool, cx: &mut Context<Self>) {
        if !hovered {
            self.hover_prewarm = None;
            return;
        }
        let project = project.clone();
        self.hover_prewarm = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(PREWARM_HOVER_DELAY).await;
            cx.update(|cx| prewarm(&project, cx));
        }));
    }

    fn observe_workspace(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let multi = workspace
            .read(cx)
            .multi_workspace()
            .and_then(WeakEntity::upgrade);
        let handle = multi.as_ref().map(Entity::downgrade);
        if handle == self.multi_workspace {
            return;
        }
        self.multi_workspace = handle;
        // A Fleet repaint already notifies the window through the dock and sidebar.
        // Listen for project events only, so a repaint cannot trigger itself again.
        self._workspace_subscription = multi.as_ref().map(|multi| {
            cx.subscribe(multi, |panel, _, event, cx| {
                if matches!(
                    event,
                    MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }
                        | MultiWorkspaceEvent::WorkspaceRemoved(_)
                ) {
                    panel.refresh_recent_projects(cx);
                }
                cx.notify();
            })
        });
    }

    fn observe_clients(&mut self, clients: &[Entity<RemoteClient>], cx: &mut Context<Self>) {
        self.observed_clients
            .retain(|id, _| clients.iter().any(|client| client.entity_id() == *id));
        for client in clients {
            if self.observed_clients.contains_key(&client.entity_id()) {
                continue;
            }
            let remote = client.read(cx);
            if let Some(options) = ssh_options(remote)
                && let Some(route) = remote.route()
            {
                self.last_routes
                    .entry(options.connection_string())
                    .or_insert(route);
            }
            let observe = cx.observe(client, |_, _, cx| cx.notify());
            let subscribe = cx.subscribe(client, |panel, client, event, cx| {
                if matches!(event, RemoteClientEvent::Reconnected) {
                    panel.route_changed(&client, cx);
                }
            });
            self.observed_clients
                .insert(client.entity_id(), [observe, subscribe]);
        }
    }

    fn route_changed(&mut self, client: &Entity<RemoteClient>, cx: &mut Context<Self>) {
        let remote = client.read(cx);
        let (Some(options), Some(route)) = (ssh_options(remote), remote.route()) else {
            return;
        };
        let key = options.connection_string();
        let machine = ssh_machine_label(options);
        let previous = self.last_routes.insert(key.clone(), route.clone());
        if previous.is_some_and(|previous| previous != route)
            && let Some(workspace) = self.workspace.upgrade()
        {
            let message = format!("{machine} is now connected over {}", route.name());
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::composite::<RouteToast>(key), message).autohide(),
                    cx,
                );
            });
        }
        cx.notify();
    }

    fn render_network_switch(&self, names: &[String], cx: &App) -> impl IntoElement {
        let current = cx
            .try_global::<SshRoutes>()
            .and_then(|routes| routes.fleet.clone());
        let button = |id: SharedString, label: SharedString, route: Option<String>| {
            let selected = current == route;
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Subtle)
                .toggle_state(selected)
                .selected_style(ButtonStyle::Filled)
                .on_click(move |_, _, cx| set_fleet_route(route.clone(), cx))
        };
        h_flex()
            .px_2()
            .pb_1()
            .gap_1()
            .flex_wrap()
            .child(
                Label::new("Network")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .mr_1(),
            )
            .child(button("fleet-network-auto".into(), "Auto".into(), None))
            .children(names.iter().enumerate().map(|(index, name)| {
                button(
                    SharedString::from(format!("fleet-network-{index}")),
                    SharedString::from(name.clone()),
                    Some(name.clone()),
                )
            }))
    }

    fn render_route_badge(
        &self,
        machine_index: usize,
        options: &SshConnectionOptions,
        status: Option<&(Option<ConnectionRoute>, ConnectionState)>,
        cx: &App,
    ) -> AnyElement {
        let routes = cx.try_global::<SshRoutes>().cloned().unwrap_or_default();
        let chosen = chosen_route(options, &routes).map(str::to_owned);
        let pinned = routes.pins.contains_key(&options.connection_string());
        let setting = || chosen.clone().unwrap_or_else(|| "Auto".to_string());
        let (color, text, tooltip) = match status {
            Some((Some(route), ConnectionState::Connected)) => {
                let color = match &chosen {
                    Some(chosen) if route_names(chosen, route.label.as_deref(), &route.host) => {
                        Color::Success
                    }
                    Some(_) => Color::Muted,
                    None if route.is_primary => Color::Success,
                    None => Color::Warning,
                };
                let tooltip = if color == Color::Warning {
                    format!(
                        "Connected over {}: the primary address didn't answer",
                        route.name()
                    )
                } else {
                    format!("Connected over {} ({})", route.name(), route.host)
                };
                (color, route.name().to_string(), tooltip)
            }
            Some((_, ConnectionState::Disconnected)) => (
                Color::Error,
                setting(),
                format!("Can't reach this machine over {}", setting()),
            ),
            Some(_) => (
                Color::Muted,
                "Connecting".to_string(),
                format!("Connecting over {}", setting()),
            ),
            None => (
                Color::Muted,
                setting(),
                format!("Not connected. Next connection uses {}", setting()),
            ),
        };
        let key = ssh_machine_key(options);
        let primary_is_back = chosen.is_none()
            && matches!(status, Some((Some(route), ConnectionState::Connected)) if !route.is_primary)
            && cx
                .try_global::<PrimaryWatch>()
                .is_some_and(|watch| watch.available.contains(&key));
        let primary_name = options
            .host_label
            .clone()
            .unwrap_or_else(|| options.host.to_string());
        let options = options.clone();
        let badge = PopoverMenu::new(SharedString::from(format!("fleet-route-{machine_index}")))
            .trigger(
                ButtonLike::new(SharedString::from(format!(
                    "fleet-route-button-{machine_index}"
                )))
                .child(
                    h_flex()
                        .gap_1()
                        .child(Indicator::dot().color(color))
                        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
                        .when(pinned, |row| {
                            row.child(
                                Icon::new(IconName::Pin)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .tooltip(Tooltip::text(tooltip)),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| Some(route_menu(&options, window, cx)));
        h_flex()
            .gap_1()
            .when(primary_is_back, |row| {
                row.child(
                    Button::new(
                        SharedString::from(format!("fleet-primary-back-{machine_index}")),
                        format!("{primary_name} available"),
                    )
                    .label_size(LabelSize::Small)
                    .color(Color::Accent)
                    .tooltip(Tooltip::text(format!("Reconnect over {primary_name}")))
                    .on_click(move |_, _, cx| {
                        cx.update_global::<PrimaryWatch, _>(|watch, _| {
                            watch.available.remove(&key);
                        });
                        let key = key.clone();
                        // Deferred so this window is back in the app.
                        cx.defer(move |cx| reconnect_machine(&key, cx));
                    }),
                )
            })
            .child(badge)
            .into_any_element()
    }

    fn machines(&self, workspace: &Workspace, cx: &App) -> Vec<FleetMachine> {
        let current = FleetProject::from_workspace(workspace, cx);
        let multi = workspace.multi_workspace().and_then(WeakEntity::upgrade);
        let others = multi
            .iter()
            .flat_map(|multi| multi.read(cx).workspaces())
            .filter(|other| other.entity_id() != self.workspace.entity_id())
            .filter_map(|other| FleetProject::from_workspace(other.read(cx), cx));
        group_projects(
            configured_machines(cx),
            current.into_iter().chain(others),
            self.recent_projects.iter().cloned(),
        )
    }

    fn open_project(
        &mut self,
        project: &FleetProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hover_prewarm = None;
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            open_fleet_project(project, workspace.app_state().clone(), window, cx)
        });
    }

    fn render_empty_state(&self) -> AnyElement {
        v_flex()
            .p_4()
            .gap_2()
            .child(Label::new("No machines yet").color(Color::Muted))
            .child(
                Label::new("Connect to a server over SSH and its projects appear here.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("fleet-add-machine", "Add Machine").on_click(|_, window, cx| {
                    window.dispatch_action(OpenRemote::default().boxed_clone(), cx)
                }),
            )
            .into_any_element()
    }

    /// Threads that are running, need approval, or finished unread, as cards
    /// across every machine. Approvals sort first (see `fleet_threads`).
    fn render_active_threads(&self, window: &Window, cx: &App) -> impl IntoElement {
        let threads = fleet_threads(true, window, cx);
        let workspace = self.workspace.upgrade();
        let visible_thread = workspace.as_ref().and_then(|workspace| {
            let workspace = workspace.read(cx);
            workspace
                .panel::<AgentPanel>(cx)
                .and_then(|panel| panel.read(cx).visible_thread_id(workspace, cx))
        });
        let agent_store = workspace.as_ref().map(|workspace| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .agent_server_store()
                .clone()
        });
        let colors = cx.theme().colors();
        let (idle_border, focused_border, card_background) = (
            colors.border_variant,
            colors.border_focused,
            colors.editor_background,
        );
        let now = Utc::now();
        let count = threads.len();

        v_flex()
            .pb_2()
            .mb_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_1()
                    .child(
                        Label::new("Active")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(count > 0, |this| {
                        this.child(
                            Label::new(format!("· {count}"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when(threads.is_empty(), |this| {
                this.child(
                    div().px_3().py_1().child(
                        Label::new("No active threads")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(
                v_flex()
                    .px_2()
                    .gap_1p5()
                    .children(threads.into_iter().filter_map(|thread| {
                        let activity = thread.activity?;
                        let (status, status_color) = inbox_status(activity);
                        let agent_id = thread.metadata.agent_id.clone();
                        let agent_name = agent_store
                            .as_ref()
                            .and_then(|store| store.read(cx).agent_display_name(&agent_id))
                            .unwrap_or_else(|| agent_id.0.clone());
                        let agent_icon = match agent_store
                            .as_ref()
                            .and_then(|store| store.read(cx).agent_icon(&agent_id))
                        {
                            Some(svg) => Icon::from_external_svg(svg),
                            None => Icon::new(
                                Agent::from(agent_id).icon().unwrap_or(IconName::ZedAgent),
                            ),
                        };
                        let title = thread.metadata.display_title();
                        let selected = visible_thread == Some(thread.metadata.thread_id);
                        let description =
                            format!("{title} · {agent_name} · {} · {status}", thread.location);
                        Some(
                            v_flex()
                                .id(SharedString::from(format!(
                                    "fleet-inbox-{}",
                                    thread.metadata.thread_id.to_key_string()
                                )))
                                .w_full()
                                .gap_0p5()
                                .px_2()
                                .py_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(if selected {
                                    focused_border
                                } else {
                                    idle_border
                                })
                                .bg(card_background)
                                .cursor_pointer()
                                .hover(move |style| style.border_color(focused_border))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(agent_icon.size(IconSize::Small).color(Color::Muted))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .child(Label::new(title).truncate()),
                                        )
                                        .child(
                                            Label::new(short_age(thread.metadata.updated_at, now))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .flex_none()
                                                .px_1p5()
                                                .rounded_full()
                                                .bg(status_color.color(cx).opacity(0.14))
                                                .child(
                                                    Label::new(status)
                                                        .size(LabelSize::XSmall)
                                                        .color(status_color),
                                                ),
                                        )
                                        .child(
                                            Label::new(agent_name)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .single_line(),
                                        )
                                        .when(thread.remote_control, |this| {
                                            this.child(
                                                Icon::new(IconName::Share)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Accent),
                                            )
                                        })
                                        .child(
                                            div().flex_1().min_w_0().child(
                                                Label::new(thread.location.clone())
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted)
                                                    .truncate(),
                                            ),
                                        ),
                                )
                                .tooltip(Tooltip::text(description))
                                .on_click(move |_, window, cx| {
                                    open_fleet_thread(thread.clone(), window, cx)
                                }),
                        )
                    })),
            )
    }
}

impl Render for FleetPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.upgrade();
        let (machines, active) = if let Some(workspace) = &workspace {
            let workspace = workspace.read(cx);
            let primaries = primary_addresses(&configured_machines(cx));
            let active = FleetProject::from_workspace(workspace, cx)
                .map(|project| project.on_primary(&primaries));
            (self.machines(workspace, cx), active)
        } else {
            (Vec::new(), None)
        };
        if let Some(workspace) = workspace {
            self.observe_workspace(&workspace, cx);
        }
        let has_remote_machines = machines
            .iter()
            .any(|machine| machine.key != LOCAL_MACHINE_KEY);
        let configured = configured_machines(cx);
        let network_names = network_names(&configured);
        let configured = configured
            .into_iter()
            .map(|options| (ssh_machine_key(&options), options))
            .collect::<HashMap<_, _>>();
        let clients = remote_clients(Some(window), cx);
        self.observe_clients(&clients, cx);
        let mut machine_status =
            HashMap::<String, (Option<ConnectionRoute>, ConnectionState)>::default();
        for client in &clients {
            let client = client.read(cx);
            let Some(options) = ssh_options(client) else {
                continue;
            };
            let status = (client.route(), client.connection_state());
            machine_status
                .entry(ssh_machine_key(options))
                .and_modify(|current| {
                    if current.1 != ConnectionState::Connected {
                        *current = status.clone();
                    }
                })
                .or_insert(status);
        }

        let dispatch_section = self.render_dispatch_section(window, cx);
        let mut list = v_flex()
            .id("fleet-panel-list")
            .size_full()
            .overflow_y_scroll()
            .py_1()
            .when(!network_names.is_empty(), |list| {
                list.child(self.render_network_switch(&network_names, cx))
            })
            .when(
                window
                    .root::<MultiWorkspace>()
                    .flatten()
                    .is_some_and(|workspace| workspace.read(cx).multi_workspace_enabled(cx)),
                |list| list.child(self.render_active_threads(window, cx)),
            )
            .children(dispatch_section);

        for (machine_index, machine) in machines.into_iter().enumerate() {
            let is_local = machine.key == LOCAL_MACHINE_KEY;
            let expanded = !self.collapsed_machines.contains(&machine.key);

            list = list.child(
                ListItem::new(SharedString::from(format!("fleet-machine-{machine_index}")))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle(Some(expanded))
                    .on_toggle(cx.listener({
                        let key = machine.key.clone();
                        move |this, _, _, cx| this.toggle_machine(&key, cx)
                    }))
                    .on_click(cx.listener({
                        let key = machine.key.clone();
                        move |this, _, _, cx| this.toggle_machine(&key, cx)
                    }))
                    .start_slot(
                        Icon::new(if is_local {
                            IconName::Screen
                        } else {
                            IconName::Server
                        })
                        .color(Color::Muted),
                    )
                    .child(Label::new(machine.label.clone()))
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .when_some(
                                configured
                                    .get(&machine.key)
                                    .filter(|options| !options.secondary_hosts.is_empty()),
                                |row, options| {
                                    row.child(self.render_route_badge(
                                        machine_index,
                                        options,
                                        machine_status.get(&machine.key),
                                        cx,
                                    ))
                                },
                            )
                            .child(
                                Label::new(machine.projects.len().to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .tooltip(Tooltip::text(machine.key.clone())),
            );

            if !expanded {
                continue;
            }

            if is_local {
                list = list.child(
                    ListItem::new("fleet-open-folder")
                        .spacing(ListItemSpacing::Sparse)
                        .indent_level(1)
                        .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                        .child(Label::new("Open Folder…").color(Color::Muted))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(workspace::Open::default().boxed_clone(), cx)
                        }),
                );
            }

            for (project_index, project) in machine.projects.iter().enumerate() {
                let is_active = active.as_ref().is_some_and(|current| {
                    current.machine_key == machine.key && current.paths == project.paths
                });

                list = list.child(
                    ListItem::new(SharedString::from(format!(
                        "fleet-project-{machine_index}-{project_index}"
                    )))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1)
                    .toggle_state(is_active)
                    .start_slot(Icon::new(IconName::Folder).color(Color::Muted))
                    .child(Label::new(project.name.clone()))
                    .tooltip(Tooltip::text(project.description()))
                    .on_hover(cx.listener({
                        let project = project.clone();
                        move |this, hovered: &bool, _, cx| {
                            this.hover_project(&project, *hovered, cx)
                        }
                    }))
                    .on_click(cx.listener({
                        let project = project.clone();
                        move |this, _, window, cx| this.open_project(&project, window, cx)
                    })),
                );
            }
        }

        if !has_remote_machines {
            list = list.child(self.render_empty_state());
        }

        let content = if let Some(files) = self
            .file_tree
            .as_ref()
            .filter(|files| files.position(window, cx) == self.position)
        {
            let section_title = |text| {
                h_flex()
                    .flex_none()
                    .px_3()
                    .py_1p5()
                    .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
            };
            v_flex()
                .size_full()
                .child(
                    v_flex()
                        .h(relative(0.44))
                        .flex_none()
                        .min_h_0()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(section_title("Fleet"))
                        .child(list.flex_1().min_h_0()),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .child(section_title("Files"))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .overflow_hidden()
                                .child(files.to_any()),
                        ),
                )
                .into_any_element()
        } else {
            list.into_any_element()
        };
        v_flex()
            .key_context("FleetPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(content)
    }
}

impl Focusable for FleetPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for FleetPanel {}

impl Panel for FleetPanel {
    fn persistent_name() -> &'static str {
        "FleetPanel"
    }

    fn panel_key() -> &'static str {
        FLEET_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Fleet")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.refresh_recent_projects(cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

/// A searchable list of every project on every machine. Highlighting a project
/// connects to its machine in advance, so confirming it opens faster.
struct FleetSwitcherDelegate {
    workspace: WeakEntity<Workspace>,
    entries: Vec<SwitcherEntry>,
    matches: Vec<usize>,
    selected_index: usize,
}

#[derive(Clone)]
enum SwitcherEntry {
    Project(FleetProject),
    Thread(FleetThread),
}

impl SwitcherEntry {
    fn matches(&self, query: &str) -> bool {
        let text = match self {
            Self::Project(project) => format!(
                "{} {} {}",
                project.name,
                project.machine_label,
                project.description()
            ),
            Self::Thread(thread) => format!(
                "{} {} {} {}",
                thread.metadata.display_title(),
                thread.location,
                thread.metadata.agent_id.0,
                thread
                    .activity
                    .map(|activity| thread_activity_style(activity).2)
                    .unwrap_or_default()
            ),
        };
        query
            .split_whitespace()
            .all(|word| matches_in_order(word, &text))
    }
}

impl FleetSwitcherDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        projects: Vec<FleetProject>,
        threads: Vec<FleetThread>,
    ) -> Self {
        let entries = projects
            .into_iter()
            .map(SwitcherEntry::Project)
            .chain(threads.into_iter().map(SwitcherEntry::Thread))
            .collect::<Vec<_>>();
        let matches = (0..entries.len()).collect();
        Self {
            workspace,
            entries,
            matches,
            selected_index: 0,
        }
    }

    fn selected_entry(&self) -> Option<&SwitcherEntry> {
        self.entries.get(*self.matches.get(self.selected_index)?)
    }
}

/// Whether every character of `query` appears in `text`, in order, ignoring case.
fn matches_in_order(query: &str, text: &str) -> bool {
    let mut text_characters = text.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| !character.is_whitespace())
        .all(|wanted| text_characters.any(|character| character == wanted))
}

impl PickerDelegate for FleetSwitcherDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "fleet-switcher"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
        if let Some(SwitcherEntry::Project(project)) = self.selected_entry() {
            prewarm(project, cx);
        }
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search projects and threads…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No matching projects or threads.".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.matches = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.matches(&query))
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.matches.len().saturating_sub(1));
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        cx.emit(DismissEvent);
        match entry {
            SwitcherEntry::Project(project) => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        open_fleet_project(&project, workspace.app_state().clone(), window, cx)
                    });
                }
            }
            SwitcherEntry::Thread(thread) => open_fleet_thread(thread, window, cx),
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let (title, location, icon, color, description) =
            match self.entries.get(*self.matches.get(index)?)? {
                SwitcherEntry::Project(project) => (
                    project.name.clone(),
                    project.machine_label.clone(),
                    IconName::Folder,
                    Color::Muted,
                    project.description(),
                ),
                SwitcherEntry::Thread(thread) => {
                    let agent = Agent::from(thread.metadata.agent_id.clone());
                    let (icon, color) = thread
                        .activity
                        .map(|status| {
                            let (icon, color, _) = thread_activity_style(status);
                            (icon, color)
                        })
                        .unwrap_or((agent.icon().unwrap_or(IconName::Sparkle), Color::Muted));
                    let title = thread.metadata.display_title();
                    let description = format!("{title} · {} · {}", thread.location, agent.label());
                    (title, thread.location.clone(), icon, color, description)
                }
            };
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(icon).color(color))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(title).single_line())
                        .child(
                            Label::new(location)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .single_line(),
                        ),
                )
                .tooltip(Tooltip::text(description)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switcher_query_matches_characters_in_order() {
        assert!(matches_in_order("", "yonder-smoke gpu-06"));
        assert!(matches_in_order("ysm", "yonder-smoke gpu-06"));
        assert!(matches_in_order("GPU 06", "yonder-smoke gpu-06"));
        assert!(!matches_in_order("smy", "yonder-smoke"));
        assert!(!matches_in_order("atlas", "yonder-smoke gpu-06"));
    }

    #[test]
    fn switcher_matches_thread_title_account_machine_and_activity() {
        let thread = SwitcherEntry::Thread(FleetThread {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: None,
                agent_id: project::AgentId::new("claude-acp@work"),
                title: Some("Fix indexing".into()),
                title_override: None,
                updated_at: Default::default(),
                created_at: None,
                interacted_at: None,
                worktree_paths: Default::default(),
                remote_connection: None,
                archived: false,
            },
            location: "search · GPU 06".into(),
            activity: Some(ThreadActivity::WaitingForApproval),
            remote_control: false,
        });
        assert!(thread.matches("approval gpu indexing work"));
        assert!(!thread.matches("unread"));
        assert!(!thread.matches("gpu 07"));
    }

    fn inbox_thread(
        title: &str,
        activity: ThreadActivity,
        updated_at: DateTime<Utc>,
    ) -> FleetThread {
        FleetThread {
            metadata: ThreadMetadata {
                thread_id: ThreadId::new(),
                session_id: None,
                agent_id: project::AgentId::new("claude-acp"),
                title: Some(title.to_string().into()),
                title_override: None,
                updated_at,
                created_at: None,
                interacted_at: None,
                worktree_paths: Default::default(),
                remote_connection: None,
                archived: false,
            },
            location: "search · GPU 06".into(),
            activity: Some(activity),
            remote_control: false,
        }
    }

    #[test]
    fn inbox_puts_approvals_first_then_running_then_unread_newest_first() {
        let now = Utc::now();
        let minutes_ago = |minutes| now - chrono::Duration::minutes(minutes);
        let mut threads = vec![
            inbox_thread("old unread", ThreadActivity::Unread, minutes_ago(30)),
            inbox_thread("running", ThreadActivity::Running, minutes_ago(20)),
            inbox_thread("new unread", ThreadActivity::Unread, minutes_ago(1)),
            inbox_thread(
                "approval",
                ThreadActivity::WaitingForApproval,
                minutes_ago(60),
            ),
        ];
        sort_by_attention(&mut threads);
        let titles = threads
            .iter()
            .map(|thread| thread.metadata.display_title().to_string())
            .collect::<Vec<_>>();
        assert_eq!(titles, ["approval", "running", "new unread", "old unread"]);
    }

    #[test]
    fn inbox_ages_are_short() {
        let now = Utc::now();
        let ago = |seconds| now - chrono::Duration::seconds(seconds);
        assert_eq!(short_age(ago(0), now), "now");
        assert_eq!(short_age(ago(59), now), "now");
        assert_eq!(short_age(ago(60), now), "1m");
        assert_eq!(short_age(ago(3_599), now), "59m");
        assert_eq!(short_age(ago(3_600), now), "1h");
        assert_eq!(short_age(ago(86_399), now), "23h");
        assert_eq!(short_age(ago(86_400), now), "1d");
        assert_eq!(short_age(now + chrono::Duration::seconds(30), now), "now");
    }

    #[test]
    fn fleet_uses_open_projects_and_history_instead_of_saved_folders() {
        let configured = SshConnectionOptions {
            host: "build-box".into(),
            nickname: Some("Build Box".to_string()),
            ..Default::default()
        };
        let connection = SshConnectionOptions {
            host: configured.host.clone(),
            ..Default::default()
        };
        let remote = |path: &str| {
            FleetProject::new(
                ProjectLocation::Ssh(connection.clone()),
                PathList::new(&[path]),
            )
            .unwrap()
        };
        let local =
            FleetProject::new(ProjectLocation::Local, PathList::new(&["/local/editor"])).unwrap();
        let machines = group_projects(
            [configured],
            [remote("/remote/just-opened"), local],
            [remote("/remote/previous"), remote("/remote/just-opened")],
        );
        assert_eq!(machines[0].projects[0].name.as_ref(), "editor");
        let machine = &machines[1];
        assert_eq!(machine.label.as_ref(), "Build Box");
        assert_eq!(
            machine
                .projects
                .iter()
                .map(|project| project.name.as_ref())
                .collect::<Vec<_>>(),
            ["just-opened", "previous"]
        );
    }

    #[test]
    fn unconfigured_ssh_machines_appear_when_a_project_is_opened() {
        let project = FleetProject::new(
            ProjectLocation::Ssh(SshConnectionOptions {
                host: "other-box".into(),
                ..Default::default()
            }),
            PathList::new(&["/remote/project"]),
        )
        .unwrap();
        let machines = group_projects([], [project], []);
        assert_eq!(machines[1].label.as_ref(), "other-box");
        assert_eq!(machines[1].projects[0].name.as_ref(), "project");
    }

    #[test]
    fn projects_saved_under_a_secondary_address_list_under_the_machine() {
        let configured = SshConnectionOptions {
            host: "172.17.21.106".into(),
            username: Some("ayasser".to_string()),
            nickname: Some("gpu-06".to_string()),
            secondary_hosts: vec![remote::SshSecondaryHost {
                host: "gpu-06.example.ts.net".to_string(),
                port: None,
                label: Some("Tailscale".to_string()),
            }],
            ..Default::default()
        };
        let project = |host: &str, path: &str| {
            FleetProject::new(
                ProjectLocation::Ssh(SshConnectionOptions {
                    host: host.into(),
                    username: Some("ayasser".to_string()),
                    ..Default::default()
                }),
                PathList::new(&[path]),
            )
            .unwrap()
        };
        let machines = group_projects(
            [configured],
            [project("gpu-06.example.ts.net", "/home/ayasser/a")],
            [
                project("172.17.21.106", "/home/ayasser/a"),
                project("gpu-06.example.ts.net", "/home/ayasser/b"),
            ],
        );
        assert_eq!(machines.len(), 2);
        let machine = &machines[1];
        assert_eq!(machine.label.as_ref(), "gpu-06");
        assert_eq!(
            machine
                .projects
                .iter()
                .map(|project| project.name.as_ref())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        for project in &machine.projects {
            let ProjectLocation::Ssh(options) = &project.location else {
                panic!("expected an SSH project");
            };
            assert_eq!(options.host.to_string(), "172.17.21.106");
        }
    }

    fn two_network_machine(host: &str, tailscale: &str) -> SshConnectionOptions {
        SshConnectionOptions {
            host: host.into(),
            username: Some("ayasser".to_string()),
            host_label: Some("Office".to_string()),
            secondary_hosts: vec![remote::SshSecondaryHost {
                host: tailscale.to_string(),
                port: None,
                label: Some("Tailscale".to_string()),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn network_switch_lists_labels_of_machines_with_several_addresses() {
        let single = SshConnectionOptions {
            host: "box".into(),
            host_label: Some("Lab".to_string()),
            ..Default::default()
        };
        let names = network_names(&[
            single,
            two_network_machine("10.0.0.6", "gpu-06.ts.net"),
            two_network_machine("10.0.0.7", "gpu-07.ts.net"),
        ]);
        assert_eq!(names, ["Office", "Tailscale"]);
        assert_eq!(next_route(None, &names).as_deref(), Some("Office"));
        assert_eq!(
            next_route(Some("Office"), &names).as_deref(),
            Some("Tailscale")
        );
        assert_eq!(next_route(Some("Tailscale"), &names), None);
        assert_eq!(next_route(Some("Gone"), &names), None);
    }

    #[test]
    fn a_connection_matches_the_chosen_route_or_the_primary_on_auto() {
        let machine = two_network_machine("10.0.0.6", "gpu-06.ts.net");
        let office = ConnectionRoute {
            label: Some("Office".to_string()),
            host: "10.0.0.6".to_string(),
            is_primary: true,
        };
        let tailscale = ConnectionRoute {
            label: Some("Tailscale".to_string()),
            host: "gpu-06.ts.net".to_string(),
            is_primary: false,
        };
        let mut routes = SshRoutes::default();
        assert!(route_satisfied(&machine, &routes, &office));
        assert!(!route_satisfied(&machine, &routes, &tailscale));

        routes.fleet = Some("Tailscale".to_string());
        assert!(route_satisfied(&machine, &routes, &tailscale));
        assert!(!route_satisfied(&machine, &routes, &office));

        routes
            .pins
            .insert(machine.connection_string(), "Office".to_string());
        assert!(route_satisfied(&machine, &routes, &office));

        routes.pins.clear();
        routes.fleet = Some("Home".to_string());
        assert!(route_satisfied(&machine, &routes, &office));
        assert_eq!(chosen_route(&machine, &routes), None);
    }

    #[test]
    fn machine_keys_include_user_and_port_when_set() {
        assert_eq!(machine_key(None, "gpu-06", None), "gpu-06");
        assert_eq!(
            machine_key(Some("ayasser"), "172.17.21.106", Some(2222)),
            "ayasser@172.17.21.106:2222"
        );
    }
}
