use std::path::{Path, PathBuf};

use collections::HashSet;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, Render, SharedString, Subscription, WeakEntity, Window, actions, px,
};
use recent_projects::{RemoteSettings, open_remote_project};
use remote::RemoteConnectionOptions;
use settings::{Settings, SettingsStore, SshConnection};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use workspace::{
    MultiWorkspace, OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::DetachAndPromptErr,
};
use zed_actions::OpenRemote;

actions!(
    fleet_panel,
    [
        /// Toggles focus on the fleet panel, which lists your machines and their projects.
        ToggleFocus
    ]
);

const FLEET_PANEL_KEY: &str = "FleetPanel";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<FleetPanel>(window, cx);
        });
    })
    .detach();
}

/// Lists every SSH machine from settings with its projects, and opens a project
/// in the current window so switching between machines stays in one place.
pub struct FleetPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    collapsed_machines: HashSet<String>,
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
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }

    fn toggle_machine(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed_machines.remove(key) {
            self.collapsed_machines.insert(key.to_string());
        }
        cx.notify();
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

    fn open_project(
        &mut self,
        connection: &SshConnection,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let app_state = workspace.read(cx).app_state().clone();
        let options = RemoteSettings::get_global(cx).connection_options_for(
            connection.host.clone(),
            connection.port,
            connection.username.clone(),
        );
        let open_options = OpenOptions {
            requesting_window: window.window_handle().downcast::<MultiWorkspace>(),
            ..Default::default()
        };
        cx.spawn_in(window, async move |_, cx| {
            open_remote_project(
                RemoteConnectionOptions::Ssh(options),
                paths,
                app_state,
                open_options,
                cx,
            )
            .await?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to open project", window, cx, |_, _, _| None);
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

impl Render for FleetPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connections: Vec<SshConnection> =
            RemoteSettings::get_global(cx).ssh_connections().collect();
        let active = self.active_location(cx);

        let mut list = v_flex()
            .id("fleet-panel-list")
            .size_full()
            .overflow_y_scroll()
            .py_1();

        if connections.is_empty() {
            list = list.child(self.render_empty_state());
        }

        for (machine_index, connection) in connections.iter().enumerate() {
            let key = machine_key(
                connection.username.as_deref(),
                &connection.host,
                connection.port,
            );
            let expanded = !self.collapsed_machines.contains(&key);
            let label = connection
                .nickname
                .clone()
                .unwrap_or_else(|| connection.host.clone());

            list = list.child(
                ListItem::new(SharedString::from(format!("fleet-machine-{machine_index}")))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle(Some(expanded))
                    .on_toggle(cx.listener({
                        let key = key.clone();
                        move |this, _, _, cx| this.toggle_machine(&key, cx)
                    }))
                    .on_click(cx.listener({
                        let key = key.clone();
                        move |this, _, _, cx| this.toggle_machine(&key, cx)
                    }))
                    .start_slot(Icon::new(IconName::Server).color(Color::Muted))
                    .child(Label::new(label))
                    .end_slot(
                        Label::new(connection.projects.len().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(key.clone())),
            );

            if !expanded {
                continue;
            }

            for (project_index, project) in connection.projects.iter().enumerate() {
                let paths: Vec<PathBuf> = project.paths.iter().map(PathBuf::from).collect();
                let name = paths
                    .first()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| project.paths.join(", "));
                let is_active = active.as_ref().is_some_and(|(active_key, open_paths)| {
                    *active_key == key
                        && paths.iter().all(|path| {
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
                    .child(Label::new(name))
                    .tooltip(Tooltip::text(project.paths.join(", ")))
                    .on_click(cx.listener({
                        let connection = connection.clone();
                        move |this, _, window, cx| {
                            this.open_project(&connection, paths.clone(), window, cx)
                        }
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
