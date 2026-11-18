use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use askpass::EncryptedPassword;
use collections::{HashMap, HashSet};
use futures::channel::oneshot;
use gpui::{
    Action, App, AsyncApp, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Global, Pixels, Render, SharedString, Subscription, Task, WeakEntity,
    Window, actions, px,
};
use picker::{Picker, PickerDelegate};
use recent_projects::{RemoteSettings, open_remote_project};
use release_channel::ReleaseChannel;
use remote::{RemoteClientDelegate, RemoteConnection, RemoteConnectionOptions, RemotePlatform};
use semver::Version;
use settings::{Settings, SettingsStore, SshConnection};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use workspace::{
    AppState, MultiWorkspace, OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::DetachAndPromptErr,
};
use zed_actions::OpenRemote;

actions!(
    fleet_panel,
    [
        /// Toggles focus on the fleet panel, which lists your machines and their projects.
        ToggleFocus,
        /// Opens a searchable list of every project on every machine.
        OpenSwitcher
    ]
);

const FLEET_PANEL_KEY: &str = "FleetPanel";

/// How long the pointer must rest on a project before its machine is connected in advance.
const PREWARM_HOVER_DELAY: Duration = Duration::from_millis(250);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<FleetPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &OpenSwitcher, window, cx| {
            let workspace_handle = cx.weak_entity();
            let projects = fleet_machines(cx)
                .into_iter()
                .flat_map(|machine| machine.projects)
                .collect();
            workspace.toggle_modal(window, cx, |window, cx| {
                Picker::uniform_list(
                    FleetSwitcherDelegate::new(workspace_handle, projects),
                    window,
                    cx,
                )
            });
        });
    })
    .detach();
}

#[derive(Clone)]
struct FleetMachine {
    key: String,
    label: SharedString,
    projects: Vec<FleetProject>,
}

#[derive(Clone)]
struct FleetProject {
    connection: SshConnection,
    machine_key: String,
    machine_label: SharedString,
    name: SharedString,
    paths: Vec<PathBuf>,
}

impl FleetProject {
    fn description(&self) -> String {
        self.paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn fleet_machines(cx: &App) -> Vec<FleetMachine> {
    RemoteSettings::get_global(cx)
        .ssh_connections()
        .map(|connection| {
            let key = machine_key(
                connection.username.as_deref(),
                &connection.host,
                connection.port,
            );
            let label: SharedString = connection
                .nickname
                .clone()
                .unwrap_or_else(|| connection.host.clone())
                .into();
            let projects = connection
                .projects
                .iter()
                .map(|project| {
                    let paths: Vec<PathBuf> = project.paths.iter().map(PathBuf::from).collect();
                    let name = paths
                        .first()
                        .and_then(|path| path.file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| project.paths.join(", "));
                    FleetProject {
                        connection: connection.clone(),
                        machine_key: key.clone(),
                        machine_label: label.clone(),
                        name: name.into(),
                        paths,
                    }
                })
                .collect();
            FleetMachine {
                key,
                label,
                projects,
            }
        })
        .collect()
}

fn connection_options(connection: &SshConnection, cx: &App) -> RemoteConnectionOptions {
    RemoteConnectionOptions::Ssh(RemoteSettings::get_global(cx).connection_options_for(
        connection.host.clone(),
        connection.port,
        connection.username.clone(),
    ))
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

/// Settings may name a project relative to the home directory (`~/code/app`),
/// while an open project reports its resolved path (`/home/me/code/app`).
fn same_remote_path(settings_path: &Path, open_path: &Path) -> bool {
    match settings_path.strip_prefix("~") {
        Ok(relative) if !relative.as_os_str().is_empty() => open_path.ends_with(relative),
        _ => settings_path == open_path,
    }
}

/// Opens `project` in the window that holds `workspace`, logging how long the switch took.
fn open_fleet_project(
    project: &FleetProject,
    app_state: Arc<AppState>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let options = connection_options(&project.connection, cx);
    let connection_state = if remote::has_active_connection(&options, cx) {
        "warm"
    } else {
        "cold"
    };
    let open_options = OpenOptions {
        requesting_window: window.window_handle().downcast::<MultiWorkspace>(),
        ..Default::default()
    };
    let paths = project.paths.clone();
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
    let options = connection_options(&project.connection, cx);
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

/// Lists every SSH machine from settings with its projects, and opens a project
/// in the current window so switching between machines stays in one place.
pub struct FleetPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    collapsed_machines: HashSet<String>,
    hover_prewarm: Option<Task<()>>,
    _settings_subscription: Subscription,
}

impl FleetPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let workspace_handle = workspace.clone();
        workspace.update_in(&mut cx, |_, _, cx| {
            cx.new(|cx| Self::new(workspace_handle, cx))
        })
    }

    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            collapsed_machines: HashSet::default(),
            hover_prewarm: None,
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
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

    /// The machine and folders of the project shown in this workspace, if it is an SSH project.
    fn active_location(&self, cx: &App) -> Option<(String, Vec<PathBuf>)> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        let RemoteConnectionOptions::Ssh(options) = project.remote_connection_options(cx)? else {
            return None;
        };
        let key = machine_key(
            options.username.as_deref(),
            &options.host.to_string(),
            options.port,
        );
        let paths = project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect();
        Some((key, paths))
    }

    fn open_project(&mut self, project: &FleetProject, window: &mut Window, cx: &mut Context<Self>) {
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
}

impl Render for FleetPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let machines = fleet_machines(cx);
        let active = self.active_location(cx);

        let mut list = v_flex()
            .id("fleet-panel-list")
            .size_full()
            .overflow_y_scroll()
            .py_1();

        if machines.is_empty() {
            list = list.child(self.render_empty_state());
        }

        for (machine_index, machine) in machines.iter().enumerate() {
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
                    .start_slot(Icon::new(IconName::Server).color(Color::Muted))
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

            for (project_index, project) in machine.projects.iter().enumerate() {
                let is_active = active.as_ref().is_some_and(|(active_key, open_paths)| {
                    *active_key == machine.key
                        && project.paths.iter().all(|path| {
                            open_paths
                                .iter()
                                .any(|open_path| same_remote_path(path, open_path))
                        })
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

        v_flex()
            .key_context("FleetPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(list)
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

    fn set_position(&mut self, position: DockPosition, _window: &mut Window, cx: &mut Context<Self>) {
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

    fn activation_priority(&self) -> u32 {
        8
    }
}

/// A searchable list of every project on every machine. Highlighting a project
/// connects to its machine in advance, so confirming it opens faster.
struct FleetSwitcherDelegate {
    workspace: WeakEntity<Workspace>,
    projects: Vec<FleetProject>,
    matches: Vec<usize>,
    selected_index: usize,
}

impl FleetSwitcherDelegate {
    fn new(workspace: WeakEntity<Workspace>, projects: Vec<FleetProject>) -> Self {
        let matches = (0..projects.len()).collect();
        Self {
            workspace,
            projects,
            matches,
            selected_index: 0,
        }
    }

    fn selected_project(&self) -> Option<&FleetProject> {
        let project_index = *self.matches.get(self.selected_index)?;
        self.projects.get(project_index)
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
        if let Some(project) = self.selected_project().cloned() {
            prewarm(&project, cx);
        }
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Switch to a project on any machine…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No matching projects. Add machines and projects under ssh_connections in settings.".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.matches = self
            .projects
            .iter()
            .enumerate()
            .filter(|(_, project)| {
                let haystack = format!(
                    "{} {} {}",
                    project.name,
                    project.machine_label,
                    project.description()
                );
                matches_in_order(&query, &haystack)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.matches.len().saturating_sub(1));
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(project) = self.selected_project().cloned() else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                open_fleet_project(&project, workspace.app_state().clone(), window, cx)
            });
        }
        cx.emit(DismissEvent);
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
        let project = self.projects.get(*self.matches.get(index)?)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::Folder).color(Color::Muted))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(project.name.clone()))
                        .child(
                            Label::new(project.machine_label.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .tooltip(Tooltip::text(project.description())),
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
    fn home_relative_settings_paths_match_resolved_paths() {
        assert!(same_remote_path(
            Path::new("~/projects/app"),
            Path::new("/home/me/projects/app")
        ));
        assert!(same_remote_path(Path::new("/srv/app"), Path::new("/srv/app")));
        assert!(!same_remote_path(
            Path::new("~/projects/app"),
            Path::new("/home/me/projects/other")
        ));
        assert!(!same_remote_path(Path::new("~"), Path::new("/home/me/anything")));
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
