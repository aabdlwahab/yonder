//! The job each machine does, on the Machines page: whether Yonder is
//! installed and up to date, which machine is the hub, and changing that.

use super::*;
use machine_setup::{HubService, INSTALLED_PROGRAM, Report, Target};

#[derive(Clone, Debug)]
pub(super) enum MachineStatus {
    Checking,
    Ready(Report),
    Unreachable(SharedString),
}

#[derive(Clone, Debug)]
pub(super) enum Action {
    Working(SharedString),
    Done(SharedString),
    Failed(SharedString),
}

/// What the page is asking the user to confirm for one machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Confirm {
    MakeHub,
    StopHub,
}

pub(super) const THIS_COMPUTER: &str = "this-computer";

fn target_key(target: &Target) -> String {
    match target {
        Target::ThisComputer => THIS_COMPUTER.to_string(),
        Target::Ssh(connection) => connection.host.clone(),
    }
}

fn short_version(version: &str) -> &str {
    version.get(..10).unwrap_or(version)
}

fn os_name(os: &str) -> &str {
    match os {
        "linux" => "Linux",
        "macos" => "macOS",
        other => other,
    }
}

/// The path of the `yonder` program on `target`, as the hub's service
/// definition names it.
fn program_on(target: &Target, report: &Report) -> String {
    match target {
        Target::ThisComputer => std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .unwrap_or_default(),
        // launchd's plist is written by the shell, which expands $HOME.
        Target::Ssh(_) if report.os == "macos" => "$HOME/.local/bin/yonder".to_string(),
        // systemd expands %h to the home folder.
        Target::Ssh(_) => "%h/.local/bin/yonder".to_string(),
    }
}

/// The machines as the hub needs them. Each machine's recent projects change
/// whenever a project opens, which the hub doesn't care about.
fn machines_for_hub(cx: &App) -> Vec<SshConnection> {
    configured_connections(cx)
        .into_iter()
        .map(|mut connection| {
            connection.projects.clear();
            connection
        })
        .collect()
}

fn user_agent_servers(cx: &App) -> Option<serde_json::Value> {
    SettingsStore::global(cx)
        .raw_user_settings()
        .and_then(|user| user.content.agent_servers.as_ref())
        .and_then(|servers| serde_json::to_value(servers).ok())
}

/// Whether Yonder on a machine needs installing or updating from `carried`.
fn needs_install(report: &Report, carried: &machine_setup::Program) -> bool {
    match (&report.version, &carried.version) {
        (None, _) => true,
        (Some(installed), Some(carried)) => installed != carried,
        // A Mac's own binary has no version file; it's offered as a reinstall.
        (Some(_), None) => false,
    }
}

impl MachinesPage {
    /// Checks every machine once, the first time the page shows.
    pub(super) fn ensure_roles_checked(&mut self, cx: &mut Context<Self>) {
        // The checks run `sh` and `ssh`, which deterministic tests can't host.
        if !self.roles_checked && !cfg!(test) {
            self.roles_checked = true;
            self.synced_machines = Some(machines_for_hub(cx));
            self.check_all_roles(cx);
        }
    }

    pub(super) fn check_all_roles(&mut self, cx: &mut Context<Self>) {
        self.check_role(Target::ThisComputer, cx);
        for connection in configured_connections(cx) {
            self.check_role(Target::Ssh(connection), cx);
        }
    }

    pub(super) fn check_role(&mut self, target: Target, cx: &mut Context<Self>) {
        let key = target_key(&target);
        self.statuses.insert(key.clone(), MachineStatus::Checking);
        let task_key = format!("check:{key}");
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { machine_setup::check(&target).await })
                .await;
            this.update(cx, |this, cx| {
                this.statuses.insert(
                    key,
                    match result {
                        Ok(report) => MachineStatus::Ready(report),
                        Err(error) => MachineStatus::Unreachable(format!("{error:#}").into()),
                    },
                );
                cx.notify();
            })
            .ok();
        });
        self.role_tasks.insert(task_key, task);
        cx.notify();
    }

    fn report(&self, key: &str) -> Option<Report> {
        match self.statuses.get(key) {
            Some(MachineStatus::Ready(report)) => Some(report.clone()),
            _ => None,
        }
    }

    fn is_hub(&self, key: &str) -> bool {
        self.report(key)
            .is_some_and(|report| report.hub == HubService::Running)
    }

    /// Names of the machines reported to run a hub, other than `except`.
    fn other_hubs(&self, except: &str, cx: &App) -> Vec<String> {
        let mut names = Vec::new();
        if except != THIS_COMPUTER && self.is_hub(THIS_COMPUTER) {
            names.push("this computer".to_string());
        }
        for connection in configured_connections(cx) {
            if connection.host != except && self.is_hub(&connection.host) {
                names.push(machine_name(&connection));
            }
        }
        names
    }

    fn set_action(&mut self, key: &str, action: Action, cx: &mut Context<Self>) {
        self.actions.insert(key.to_string(), action);
        cx.notify();
    }

    /// Installs or updates Yonder on a machine. A hub switches to the new
    /// program once none of its threads is running.
    pub(super) fn install_on(&mut self, connection: SshConnection, cx: &mut Context<Self>) {
        let key = connection.host.clone();
        let Some(report) = self.report(&key) else {
            return;
        };
        let Some(program) = machine_setup::carried_program(&report.platform()) else {
            self.set_action(
                &key,
                Action::Failed(format!("This app has no Yonder for {}.", report.platform()).into()),
                cx,
            );
            return;
        };
        let hub_address = (report.hub == HubService::Running)
            .then(|| agent_servers::HubSettings::get_global(cx).address.clone())
            .flatten();
        self.set_action(&key, Action::Working("Installing Yonder…".into()), cx);
        let target = Target::Ssh(connection);
        let task_key = format!("action:{key}");
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let target = target.clone();
                    let hub_address = hub_address.clone();
                    async move {
                        let version = machine_setup::install(&target, &program).await?;
                        if let Some(address) = &hub_address {
                            machine_setup::restart_hub_when_idle(
                                &target,
                                &report.os,
                                INSTALLED_PROGRAM,
                                address,
                            )
                            .await?;
                        }
                        anyhow::Ok(version)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                let action = match result {
                    Ok(version) if hub_address.is_some() => Action::Done(
                        format!(
                            "Installed Yonder {}. The hub switches to it once no thread is running.",
                            short_version(&version)
                        )
                        .into(),
                    ),
                    Ok(version) => Action::Done(
                        format!("Installed Yonder {}.", short_version(&version)).into(),
                    ),
                    Err(error) => {
                        Action::Failed(format!("Couldn't install Yonder: {error:#}").into())
                    }
                };
                this.set_action(&key, action, cx);
                this.check_role(target, cx);
            })
            .ok();
        });
        self.role_tasks.insert(task_key, task);
    }

    /// Makes `target` the hub: installs Yonder there if needed, gives it the
    /// machines and agents, starts it as a service, and points this app at it.
    pub(super) fn make_hub(&mut self, target: Target, cx: &mut Context<Self>) {
        let key = target_key(&target);
        self.confirming.remove(&key);
        let Some(report) = self.report(&key) else {
            return;
        };
        let install = match &target {
            Target::ThisComputer => None,
            Target::Ssh(_) => match machine_setup::carried_program(&report.platform()) {
                Some(program) if needs_install(&report, &program) => Some(program),
                Some(_) => None,
                None if report.version.is_some() => None,
                None => {
                    self.set_action(
                        &key,
                        Action::Failed(
                            format!("This app has no Yonder for {}.", report.platform()).into(),
                        ),
                        cx,
                    );
                    return;
                }
            },
        };
        let program = program_on(&target, &report);
        let machines = configured_connections(cx);
        let agent_servers = user_agent_servers(cx);
        let others = self.other_hubs(&key, cx);
        let name = target.name();
        self.set_action(&key, Action::Working("Setting up the hub…".into()), cx);
        let task_key = format!("action:{key}");
        let fs = self.fs.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let target = target.clone();
                    async move {
                        if let Target::Ssh(connection) = &target {
                            if let Some(program) = &install {
                                machine_setup::install(&target, program).await?;
                            }
                            machine_setup::sync_hub_machines(
                                connection,
                                &machines,
                                agent_servers.as_ref(),
                            )
                            .await?;
                        }
                        machine_setup::make_hub(&target, &report, &program).await
                    }
                })
                .await;
            let hub = match result {
                Ok(hub) => hub,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.set_action(
                            &key,
                            Action::Failed(format!("Couldn't set up the hub: {error:#}").into()),
                            cx,
                        );
                        this.check_role(target, cx);
                    })
                    .ok();
                    return;
                }
            };
            let saved = agent_servers::write_hub_token(&hub.address, &hub.token, cx).await;
            this.update(cx, |this, cx| {
                let address = hub.address.clone();
                update_settings_file(fs, cx, move |settings, _| {
                    let settings = settings.remote.hub.get_or_insert_default();
                    settings.address = Some(address);
                    settings.enabled = Some(true);
                });
                // The Hub row's address input shows the new address.
                this.hub_address = None;
                let message = match saved {
                    Err(error) => Action::Failed(
                        format!(
                            "The hub runs on {name}, but its token couldn't go in your {}: {error:#}",
                            agent_servers::TOKEN_STORE_NAME
                        )
                        .into(),
                    ),
                    Ok(()) if others.is_empty() => Action::Done(
                        format!("{} is the hub now, at {}.", capitalize(&name), hub.address).into(),
                    ),
                    Ok(()) => Action::Done(
                        format!(
                            "{} is the hub now, at {}. The hub on {} keeps its threads and keeps running until you stop it.",
                            capitalize(&name),
                            hub.address,
                            others.join(", ")
                        )
                        .into(),
                    ),
                };
                this.set_action(&key, message, cx);
                agent_servers::HubBoard::refresh(cx);
                this.check_role(target, cx);
            })
            .ok();
        });
        self.role_tasks.insert(task_key, task);
    }

    /// Stops the hub on `target`. Its threads stay on disk there.
    pub(super) fn stop_hub(&mut self, target: Target, cx: &mut Context<Self>) {
        let key = target_key(&target);
        self.confirming.remove(&key);
        let Some(report) = self.report(&key) else {
            return;
        };
        self.set_action(&key, Action::Working("Stopping the hub…".into()), cx);
        let task_key = format!("action:{key}");
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let target = target.clone();
                    async move { machine_setup::stop_hub(&target, &report.os).await }
                })
                .await;
            this.update(cx, |this, cx| {
                let action = match result {
                    Ok(()) => {
                        Action::Done("Stopped the hub. Its threads stay on this machine.".into())
                    }
                    Err(error) => {
                        Action::Failed(format!("Couldn't stop the hub: {error:#}").into())
                    }
                };
                this.set_action(&key, action, cx);
                agent_servers::HubBoard::refresh(cx);
                this.check_role(target, cx);
            })
            .ok();
        });
        self.role_tasks.insert(task_key, task);
    }

    /// Keeps a hub on another machine in step when machines are added,
    /// edited or removed here, restarting it once it's idle.
    pub(super) fn machines_changed(&mut self, cx: &mut Context<Self>) {
        let machines = machines_for_hub(cx);
        if self.roles_checked {
            for connection in &machines {
                if !self.statuses.contains_key(&connection.host) {
                    self.check_role(Target::Ssh(connection.clone()), cx);
                }
            }
        }
        let Some(synced) = self.synced_machines.as_ref() else {
            return;
        };
        if *synced == machines {
            return;
        }
        self.synced_machines = Some(machines.clone());
        let Some(address) = agent_servers::HubSettings::get_global(cx).address.clone() else {
            return;
        };
        // The hub this app uses, when several machines run one.
        let hub_host = address
            .rsplit_once(':')
            .map_or(address.as_str(), |(host, _)| host);
        let serves_address = |connection: &SshConnection| {
            connection.host == hub_host
                || connection
                    .secondary_hosts
                    .iter()
                    .any(|secondary| secondary.host == hub_host)
                || self
                    .report(&connection.host)
                    .is_some_and(|report| report.tailscale.as_deref() == Some(hub_host))
        };
        let Some(hub) = machines
            .iter()
            .find(|connection| self.is_hub(&connection.host) && serves_address(connection))
            .cloned()
        else {
            return;
        };
        let Some(report) = self.report(&hub.host) else {
            return;
        };
        let agent_servers = user_agent_servers(cx);
        let key = hub.host.clone();
        let task_key = format!("sync:{key}");
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    machine_setup::sync_hub_machines(&hub, &machines, agent_servers.as_ref())
                        .await?;
                    machine_setup::restart_hub_when_idle(
                        &Target::Ssh(hub.clone()),
                        &report.os,
                        INSTALLED_PROGRAM,
                        &address,
                    )
                    .await
                })
                .await;
            this.update(cx, |this, cx| {
                let action = match result {
                    Ok(()) => Action::Done(
                        "Gave the hub the new machine list. It switches to it once no thread is running."
                            .into(),
                    ),
                    Err(error) => Action::Failed(
                        format!("Couldn't give the hub the new machine list: {error:#}").into(),
                    ),
                };
                this.set_action(&key, action, cx);
            })
            .ok();
        });
        self.role_tasks.insert(task_key, task);
    }

    /// The short role and version shown at the end of a machine's row.
    pub(super) fn render_role_chip(&self, key: &str) -> AnyElement {
        let (text, color): (SharedString, Color) = match self.statuses.get(key) {
            None if cfg!(test) => return gpui::Empty.into_any_element(),
            None | Some(MachineStatus::Checking) => ("Checking…".into(), Color::Muted),
            Some(MachineStatus::Unreachable(_)) => ("Can't reach".into(), Color::Warning),
            Some(MachineStatus::Ready(report)) => match (report.hub, &report.version) {
                (HubService::Running, _) => ("Hub".into(), Color::Accent),
                (_, Some(_)) => ("Machine".into(), Color::Muted),
                (_, None) if key == THIS_COMPUTER => ("App".into(), Color::Muted),
                (_, None) => ("Not installed".into(), Color::Muted),
            },
        };
        Label::new(text)
            .size(LabelSize::Small)
            .color(color)
            .into_any_element()
    }

    /// The "Yonder on this machine" part of a machine's editor, or the This
    /// Computer card.
    pub(super) fn render_role_section(&self, target: Target, cx: &mut Context<Self>) -> AnyElement {
        let key = target_key(&target);
        let name = match &target {
            Target::ThisComputer => "This computer".to_string(),
            Target::Ssh(connection) => machine_name(connection),
        };
        let mut section = v_flex().gap_1p5().child(
            Label::new(match &target {
                Target::ThisComputer => "Yonder on this computer".to_string(),
                Target::Ssh(_) => format!("Yonder on {name}"),
            })
            .size(LabelSize::Small)
            .color(Color::Muted),
        );

        let report = match self.statuses.get(&key) {
            None | Some(MachineStatus::Checking) => {
                return section
                    .child(
                        Label::new("Checking…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element();
            }
            Some(MachineStatus::Unreachable(error)) => {
                return section
                    .child(
                        Label::new(format!("Couldn't reach {name}: {error}"))
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                    .child(self.check_again_button(target, cx))
                    .into_any_element();
            }
            Some(MachineStatus::Ready(report)) => report.clone(),
        };

        let carried = machine_setup::carried_program(&report.platform());
        let is_hub = report.hub == HubService::Running;
        let mut facts = vec![format!("{} {}", os_name(&report.os), report.arch)];
        match (&target, &report.version) {
            (Target::ThisComputer, _) => facts.push("this app".to_string()),
            (_, Some(version)) => facts.push(format!("Yonder {}", short_version(version))),
            (_, None) => facts.push("Yonder isn't installed".to_string()),
        }
        match report.hub {
            HubService::Running => facts.push("runs the hub".to_string()),
            HubService::Stopped => facts.push("hub stopped".to_string()),
            HubService::None => {}
        }
        if let Some(ip) = &report.tailscale {
            facts.push(format!("Tailscale {ip}"));
        }
        section = section.child(Label::new(facts.join(" · ")).size(LabelSize::Small));

        let working = matches!(self.actions.get(&key), Some(Action::Working(_)));
        let mut buttons = h_flex().gap_1().flex_wrap();
        if let (Target::Ssh(connection), Some(carried)) = (&target, &carried)
            && needs_install(&report, carried)
        {
            let connection = connection.clone();
            buttons = buttons.child(
                Button::new(
                    ("machines-install", key_id(&key)),
                    if report.version.is_some() {
                        "Update Yonder"
                    } else {
                        "Install Yonder"
                    },
                )
                .tab_index(0_isize)
                .style(ButtonStyle::Filled)
                .label_size(LabelSize::Small)
                .disabled(working)
                .on_click(
                    cx.listener(move |this, _, _, cx| this.install_on(connection.clone(), cx)),
                ),
            );
        }
        let confirming = self.confirming.get(&key).copied();
        if !is_hub && confirming.is_none() {
            let key = key.clone();
            buttons = buttons.child(
                Button::new(("machines-make-hub", key_id(&key)), "Make Hub…")
                    .tab_index(0_isize)
                    .style(ButtonStyle::OutlinedGhost)
                    .label_size(LabelSize::Small)
                    .disabled(working)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirming.insert(key.clone(), Confirm::MakeHub);
                        cx.notify();
                    })),
            );
        }
        if is_hub && confirming.is_none() {
            let key = key.clone();
            buttons = buttons.child(
                Button::new(("machines-stop-hub", key_id(&key)), "Stop Hub…")
                    .tab_index(0_isize)
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .disabled(working)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirming.insert(key.clone(), Confirm::StopHub);
                        cx.notify();
                    })),
            );
        }
        buttons = buttons.child(self.check_again_button(target.clone(), cx));
        section = section.child(buttons);

        if let Some(confirm) = confirming {
            let others = self.other_hubs(&key, cx);
            let question = match confirm {
                Confirm::MakeHub => {
                    let mut text = format!(
                        "Run the hub on {}? It {} and runs agent threads for your other machines, so they keep going with this app closed. Yonder then uses it as its hub.",
                        name.to_lowercase_first(),
                        if report.os == "macos" {
                            "starts when you log in"
                        } else {
                            "starts when the machine boots"
                        }
                    );
                    if !others.is_empty() {
                        text.push_str(&format!(
                            " The hub on {} keeps its threads and keeps running until you stop it.",
                            others.join(", ")
                        ));
                    }
                    text
                }
                Confirm::StopHub => format!(
                    "Stop the hub on {}? Threads it runs stop until a hub runs again. They stay on the machine.",
                    name.to_lowercase_first()
                ),
            };
            let confirm_label = match confirm {
                Confirm::MakeHub => format!("Make {name} the Hub"),
                Confirm::StopHub => "Stop Hub".to_string(),
            };
            let cancel_key = key.clone();
            section = section.child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(question)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(("machines-confirm", key_id(&key)), confirm_label)
                                    .tab_index(0_isize)
                                    .style(match confirm {
                                        Confirm::MakeHub => ButtonStyle::Filled,
                                        Confirm::StopHub => {
                                            ButtonStyle::Tinted(ui::TintColor::Error)
                                        }
                                    })
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |this, _, _, cx| match confirm {
                                        Confirm::MakeHub => this.make_hub(target.clone(), cx),
                                        Confirm::StopHub => this.stop_hub(target.clone(), cx),
                                    })),
                            )
                            .child(
                                Button::new(("machines-cancel", key_id(&key)), "Cancel")
                                    .tab_index(0_isize)
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.confirming.remove(&cancel_key);
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }

        if let Some(action) = self.actions.get(&key) {
            let (text, color) = match action {
                Action::Working(text) => (text.clone(), Color::Muted),
                Action::Done(text) => (text.clone(), Color::Success),
                Action::Failed(text) => (text.clone(), Color::Error),
            };
            section = section.child(Label::new(text).size(LabelSize::Small).color(color));
        }
        section.into_any_element()
    }

    fn check_again_button(&self, target: Target, cx: &mut Context<Self>) -> Button {
        let key = target_key(&target);
        Button::new(("machines-check-role", key_id(&key)), "Check Again")
            .tab_index(0_isize)
            .style(ButtonStyle::Subtle)
            .label_size(LabelSize::Small)
            .on_click(cx.listener(move |this, _, _, cx| this.check_role(target.clone(), cx)))
    }

    /// The This Computer card under the machine list.
    pub(super) fn render_this_computer(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_2()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Screen).color(Color::Muted))
                            .child(Label::new("This Computer")),
                    )
                    .child(self.render_role_chip(THIS_COMPUTER)),
            )
            .child(self.render_role_section(Target::ThisComputer, cx))
            .into_any_element()
    }
}

/// A stable element id number for a machine key.
fn key_id(key: &str) -> usize {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish() as usize
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

trait LowercaseFirst {
    fn to_lowercase_first(&self) -> String;
}

impl LowercaseFirst for String {
    /// "This computer" reads as "this computer" mid-sentence; machine names
    /// stay as they are.
    fn to_lowercase_first(&self) -> String {
        if self == "This computer" {
            "this computer".to_string()
        } else {
            self.clone()
        }
    }
}
