use std::{
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
use futures::channel::oneshot;
use gpui::{
    Action, App, AsyncApp, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Global, Pixels, Render, SharedString, Subscription, Task, WeakEntity,
    Window, actions, px, relative,
};
use picker::{Picker, PickerDelegate};
use project::{Event as ProjectEvent, Project};
use recent_projects::{RemoteSettings, open_remote_project};
use release_channel::ReleaseChannel;
use remote::{
    RemoteClientDelegate, RemoteConnection, RemoteConnectionOptions, RemotePlatform,
    SshConnectionOptions,
};
use semver::Version;
use settings::{Settings, SettingsStore};
use sidebar::{Sidebar, ThreadActivity, ThreadActivityChanged};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    AppState, MultiWorkspace, MultiWorkspaceEvent, OpenMode, OpenOptions, PathList,
    RecentWorkspace, SerializedWorkspaceLocation, Workspace, WorkspaceDb,
    dock::{DockPosition, Panel, PanelEvent, PanelHandle},
    notifications::DetachAndPromptErr,
};
use zed_actions::OpenRemote;

actions!(
    fleet_panel,
    [
        /// Toggles focus on the fleet panel, which lists your machines and their projects.
        ToggleFocus,
        /// Searches projects and threads across machines.
        OpenSwitcher
    ]
);

const FLEET_PANEL_KEY: &str = "FleetPanel";

/// How long the pointer must rest on a project before its machine is connected in advance.
const PREWARM_HOVER_DELAY: Duration = Duration::from_millis(250);

/// The laptop itself, listed above the SSH machines.
const LOCAL_MACHINE_KEY: &str = "local";
const LOCAL_MACHINE_LABEL: &str = "This Mac";
const RECENT_PROJECTS_PER_MACHINE: usize = 8;

struct FleetActivity;
impl Global for FleetActivity {}

pub fn init(cx: &mut App) {
    cx.set_global(FleetActivity);
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

fn open_fleet_thread(thread: FleetThread, window: &Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        let Some(sidebar) = window
            .root::<MultiWorkspace>()
            .flatten()
            .and_then(|workspace| sidebar_for_window(workspace.read(cx)))
        else {
            return;
        };
        sidebar.update(cx, |sidebar, cx| {
            sidebar.open_known_thread(thread.metadata, window, cx)
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

fn group_projects(
    configured_hosts: impl IntoIterator<Item = SshConnectionOptions>,
    open_projects: impl IntoIterator<Item = FleetProject>,
    recent_projects: impl IntoIterator<Item = FleetProject>,
) -> Vec<FleetMachine> {
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

    let open_projects = open_projects.into_iter().map(|project| (true, project));
    let recent_projects = recent_projects.into_iter().map(|project| (false, project));
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
    _activity_subscription: Subscription,
    _thread_subscription: Option<Subscription>,
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
            _activity_subscription: cx.observe_global::<FleetActivity>(|_, cx| cx.notify()),
            _thread_subscription: ThreadMetadataStore::try_global(cx)
                .map(|store| cx.observe(&store, |_, _, cx| cx.notify())),
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

    fn machines(&self, workspace: &Workspace, cx: &App) -> Vec<FleetMachine> {
        let current = FleetProject::from_workspace(workspace, cx);
        let multi = workspace.multi_workspace().and_then(WeakEntity::upgrade);
        let others = multi
            .iter()
            .flat_map(|multi| multi.read(cx).workspaces())
            .filter(|other| other.entity_id() != self.workspace.entity_id())
            .filter_map(|other| FleetProject::from_workspace(other.read(cx), cx));
        group_projects(
            RemoteSettings::get_global(cx)
                .ssh_connections()
                .map(SshConnectionOptions::from),
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
                                Agent::from(agent_id)
                                    .icon()
                                    .unwrap_or(IconName::ZedAgent),
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
            let active = FleetProject::from_workspace(workspace, cx);
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

        let mut list = v_flex()
            .id("fleet-panel-list")
            .size_full()
            .overflow_y_scroll()
            .py_1()
            .when(
                window
                    .root::<MultiWorkspace>()
                    .flatten()
                    .is_some_and(|workspace| workspace.read(cx).multi_workspace_enabled(cx)),
                |list| list.child(self.render_active_threads(window, cx)),
            );

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
                        Label::new(machine.projects.len().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
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
    fn machine_keys_include_user_and_port_when_set() {
        assert_eq!(machine_key(None, "gpu-06", None), "gpu-06");
        assert_eq!(
            machine_key(Some("ayasser"), "172.17.21.106", Some(2222)),
            "ayasser@172.17.21.106:2222"
        );
    }
}
