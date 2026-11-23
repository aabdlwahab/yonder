use collections::HashMap;
use editor::Editor;
use fs::Fs;
use gpui::{Entity, Focusable as _, ReadGlobal as _, Subscription, Task, prelude::*};
use remote::{AddressCheck, SshConnectionOptions};
use settings::{
    Settings as _, SettingsStore, SshConnection, SshSecondaryHost, update_settings_file,
};
use std::sync::Arc;
use ui::{
    Checkbox, Divider, Indicator, ListItem, ListItemSpacing, ToggleState, Tooltip, prelude::*,
};

use crate::SettingsWindow;
use crate::pages::AgentAccountsPage;

pub(crate) fn render_machines_page(
    settings_window: &SettingsWindow,
    _window: &mut Window,
    _cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    match &settings_window.machines_page {
        Some(page) => page.clone().into_any_element(),
        None => gpui::Empty.into_any_element(),
    }
}

/// The SSH machines from the user's settings file, in file order.
fn configured_connections(cx: &App) -> Vec<SshConnection> {
    SettingsStore::global(cx)
        .raw_user_settings()
        .and_then(|user| user.content.remote.ssh_connections.clone())
        .unwrap_or_default()
}

fn machine_name(connection: &SshConnection) -> String {
    connection
        .nickname
        .clone()
        .unwrap_or_else(|| connection.host.clone())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AddressDraft {
    pub label: String,
    pub host: String,
    pub port: String,
}

/// What the page edits for one machine, as the text in its inputs. The first
/// address is the primary, which also names the machine in Yonder's history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MachineDraft {
    pub name: String,
    pub user: String,
    pub addresses: Vec<AddressDraft>,
    pub timeout: String,
    pub args: String,
    pub upload_binary: bool,
}

fn optional_text(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn parse_port(text: &str, row: usize) -> Result<Option<u16>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<u16>() {
        Ok(port) if port > 0 => Ok(Some(port)),
        _ => Err(format!(
            "Address {row} has port \"{text}\". Use a number from 1 to 65535, or leave it empty for 22."
        )),
    }
}

impl MachineDraft {
    pub fn from_connection(connection: &SshConnection) -> Self {
        let primary = AddressDraft {
            label: connection.host_label.clone().unwrap_or_default(),
            host: connection.host.clone(),
            port: connection
                .port
                .map(|port| port.to_string())
                .unwrap_or_default(),
        };
        let secondaries = connection
            .secondary_hosts
            .iter()
            .map(|secondary| AddressDraft {
                label: secondary.label.clone().unwrap_or_default(),
                host: secondary.host.clone(),
                port: secondary
                    .port
                    .map(|port| port.to_string())
                    .unwrap_or_default(),
            });
        Self {
            name: connection.nickname.clone().unwrap_or_default(),
            user: connection.username.clone().unwrap_or_default(),
            addresses: std::iter::once(primary).chain(secondaries).collect(),
            timeout: connection
                .connection_timeout
                .map(|timeout| timeout.to_string())
                .unwrap_or_default(),
            args: connection.args.join(" "),
            upload_binary: connection.upload_binary_over_ssh.unwrap_or(false),
        }
    }

    /// Writes the draft into `connection`, keeping what the page doesn't edit,
    /// such as projects and port forwards. Secondary rows left without a host
    /// are dropped.
    pub fn apply(&self, connection: &mut SshConnection) -> Result<(), String> {
        let mut addresses = self.addresses.iter().enumerate();
        let Some((_, primary)) = addresses.next() else {
            return Err("Add at least one address.".to_string());
        };
        let host = primary.host.trim();
        if host.is_empty() {
            return Err("The first address needs a host.".to_string());
        }
        let port = parse_port(&primary.port, 1)?;
        let mut secondary_hosts = Vec::new();
        for (index, address) in addresses {
            let Some(secondary) = optional_text(&address.host) else {
                continue;
            };
            secondary_hosts.push(SshSecondaryHost {
                host: secondary,
                port: parse_port(&address.port, index + 1)?,
                label: optional_text(&address.label),
            });
        }
        let connection_timeout = match self.timeout.trim() {
            "" => None,
            text => Some(text.parse::<u16>().map_err(|_| {
                format!("Connection timeout is \"{text}\". Use a whole number of seconds.")
            })?),
        };

        connection.host = host.to_string();
        connection.port = port;
        connection.host_label = optional_text(&primary.label);
        connection.secondary_hosts = secondary_hosts;
        connection.nickname = optional_text(&self.name);
        connection.username = optional_text(&self.user);
        connection.connection_timeout = connection_timeout;
        connection.args = self.args.split_whitespace().map(str::to_string).collect();
        connection.upload_binary_over_ssh = self.upload_binary.then_some(true);
        Ok(())
    }
}

struct AddressInputs {
    label: Entity<Editor>,
    host: Entity<Editor>,
    port: Entity<Editor>,
}

/// Inputs for the machine that is open for editing.
struct MachineEditor {
    index: usize,
    /// The primary host when the editor opened, so a save can't land on an
    /// entry that moved in the file meanwhile.
    original_host: String,
    name: Entity<Editor>,
    user: Entity<Editor>,
    timeout: Entity<Editor>,
    args: Entity<Editor>,
    upload_binary: bool,
    addresses: Vec<AddressInputs>,
    error: Option<SharedString>,
    confirm_remove: bool,
}

struct NewMachineForm {
    name: Entity<Editor>,
    user: Entity<Editor>,
    host: Entity<Editor>,
    label: Entity<Editor>,
    error: Option<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckState {
    Checking,
    Done(AddressCheck),
}

pub(crate) struct MachinesPage {
    fs: Arc<dyn Fs>,
    /// Shows each machine's agent sign-ins in its editor.
    accounts: Option<Entity<AgentAccountsPage>>,
    editing: Option<MachineEditor>,
    adding: Option<NewMachineForm>,
    /// Address checks by `host:port`, so a result follows its address when
    /// rows move.
    checks: HashMap<String, CheckState>,
    check_tasks: Vec<Task<()>>,
    /// The Hub row's inputs, made on first render because editors need a window.
    hub_address: Option<Entity<Editor>>,
    hub_token: Option<Entity<Editor>>,
    hub_message: Option<SharedString>,
    hub_task: Option<Task<()>>,
    _settings_subscription: Subscription,
    _accounts_subscription: Option<Subscription>,
}

fn text_input(
    placeholder: &str,
    text: &str,
    window: &mut Window,
    cx: &mut Context<MachinesPage>,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text(text, window, cx);
        editor
    })
}

fn editor_text(editor: &Entity<Editor>, cx: &App) -> String {
    editor.read(cx).text(cx)
}

fn input_box(editor: &Entity<Editor>, cx: &App) -> Div {
    let colors = cx.theme().colors();
    let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);
    div()
        .py_1()
        .px_2()
        .h_7()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .track_focus(&focus_handle)
        .focus(|style| style.border_color(colors.border_focused))
        .child(editor.clone())
}

fn field(label: &'static str, editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(input_box(editor, cx))
}

fn check_key(host: &str, port: &str) -> String {
    format!("{}:{}", host.trim(), port.trim())
}

impl MachinesPage {
    pub fn new(
        fs: Arc<dyn Fs>,
        accounts: Option<Entity<AgentAccountsPage>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            fs,
            accounts: accounts.clone(),
            editing: None,
            adding: None,
            checks: HashMap::default(),
            check_tasks: Vec::new(),
            hub_address: None,
            hub_token: None,
            hub_message: None,
            hub_task: None,
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
            _accounts_subscription: accounts
                .map(|accounts| cx.observe(&accounts, |_, _, cx| cx.notify())),
        }
    }

    pub(crate) fn toggle_machine(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .editing
            .as_ref()
            .is_some_and(|editor| editor.index == index)
        {
            self.editing = None;
            cx.notify();
            return;
        }
        let Some(connection) = configured_connections(cx).into_iter().nth(index) else {
            return;
        };
        let draft = MachineDraft::from_connection(&connection);
        let addresses = draft
            .addresses
            .iter()
            .map(|address| self.address_inputs(address, window, cx))
            .collect();
        self.editing = Some(MachineEditor {
            index,
            original_host: connection.host.clone(),
            name: text_input(&connection.host, &draft.name, window, cx),
            user: text_input("Your user on this machine", &draft.user, window, cx),
            timeout: text_input("10", &draft.timeout, window, cx),
            args: text_input("-o ServerAliveInterval=30", &draft.args, window, cx),
            upload_binary: draft.upload_binary,
            addresses,
            error: None,
            confirm_remove: false,
        });
        cx.notify();
    }

    fn address_inputs(
        &self,
        address: &AddressDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AddressInputs {
        AddressInputs {
            label: text_input("Label", &address.label, window, cx),
            host: text_input("Host or IP", &address.host, window, cx),
            port: text_input("22", &address.port, window, cx),
        }
    }

    fn editor_draft(&self, editor: &MachineEditor, cx: &App) -> MachineDraft {
        MachineDraft {
            name: editor_text(&editor.name, cx),
            user: editor_text(&editor.user, cx),
            addresses: editor
                .addresses
                .iter()
                .map(|address| AddressDraft {
                    label: editor_text(&address.label, cx),
                    host: editor_text(&address.host, cx),
                    port: editor_text(&address.port, cx),
                })
                .collect(),
            timeout: editor_text(&editor.timeout, cx),
            args: editor_text(&editor.args, cx),
            upload_binary: editor.upload_binary,
        }
    }

    fn move_address(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if let Some(editor) = self.editing.as_mut()
            && from < editor.addresses.len()
            && to < editor.addresses.len()
        {
            editor.addresses.swap(from, to);
            cx.notify();
        }
    }

    fn add_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = self.address_inputs(&AddressDraft::default(), window, cx);
        if let Some(editor) = self.editing.as_mut() {
            editor.addresses.push(inputs);
            cx.notify();
        }
    }

    fn delete_address(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(editor) = self.editing.as_mut()
            && editor.addresses.len() > 1
            && index < editor.addresses.len()
        {
            editor.addresses.remove(index);
            cx.notify();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editing.as_ref() else {
            return;
        };
        let draft = self.editor_draft(editor, cx);
        let Some(mut current) = configured_connections(cx).into_iter().nth(editor.index) else {
            return;
        };
        if let Err(error) = draft.apply(&mut current) {
            if let Some(editor) = self.editing.as_mut() {
                editor.error = Some(error.into());
            }
            cx.notify();
            return;
        }
        let index = editor.index;
        let original_host = editor.original_host.clone();
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            if let Some(connection) = settings
                .remote
                .ssh_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(index))
                .filter(|connection| connection.host == original_host)
            {
                *connection = current;
            }
        });
        if let Some(editor) = self.editing.as_mut() {
            editor.error = None;
            editor.original_host = draft.addresses[0].host.trim().to_string();
        }
        cx.notify();
    }

    fn remove(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editing.take() else {
            return;
        };
        let index = editor.index;
        let original_host = editor.original_host;
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            if let Some(connections) = settings.remote.ssh_connections.as_mut()
                && connections
                    .get(index)
                    .is_some_and(|connection| connection.host == original_host)
            {
                connections.remove(index);
            }
        });
        cx.notify();
    }

    fn check_addresses(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editing.as_ref() else {
            return;
        };
        let draft = self.editor_draft(editor, cx);
        let user = optional_text(&draft.user);
        let args: Vec<String> = draft.args.split_whitespace().map(str::to_string).collect();
        let targets = draft
            .addresses
            .iter()
            .filter(|address| !address.host.trim().is_empty())
            .map(|address| {
                (
                    check_key(&address.host, &address.port),
                    SshConnectionOptions {
                        host: address.host.trim().into(),
                        username: user.clone(),
                        port: address.port.trim().parse().ok(),
                        args: Some(args.clone()),
                        ..Default::default()
                    },
                )
            })
            .collect::<Vec<_>>();
        self.start_checks(targets, cx);
    }

    fn start_checks(
        &mut self,
        targets: Vec<(String, SshConnectionOptions)>,
        cx: &mut Context<Self>,
    ) {
        let executor = cx.background_executor().clone();
        for (key, options) in targets {
            self.checks.insert(key.clone(), CheckState::Checking);
            let executor = executor.clone();
            self.check_tasks
                .push(cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
                    let result = remote::check_address(&options, &executor).await;
                    this.update(cx, |this, cx| {
                        this.checks.insert(key, CheckState::Done(result));
                        cx.notify();
                    })
                    .ok();
                }));
        }
        cx.notify();
    }

    fn open_new_machine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.adding.is_some() {
            self.adding = None;
            cx.notify();
            return;
        }
        let user = configured_connections(cx)
            .into_iter()
            .find_map(|connection| connection.username)
            .unwrap_or_default();
        self.adding = Some(NewMachineForm {
            name: text_input("gpu-08", "", window, cx),
            user: text_input("Your user on this machine", &user, window, cx),
            host: text_input("172.17.21.108 or a host name", "", window, cx),
            label: text_input("Office", "", window, cx),
            error: None,
        });
        cx.notify();
    }

    fn check_new_machine(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.adding.as_ref() else {
            return;
        };
        let host = editor_text(&form.host, cx);
        if host.trim().is_empty() {
            return;
        }
        let options = SshConnectionOptions {
            host: host.trim().into(),
            username: optional_text(&editor_text(&form.user, cx)),
            ..Default::default()
        };
        self.start_checks(vec![(check_key(&host, ""), options)], cx);
    }

    fn save_new_machine(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.adding.as_mut() else {
            return;
        };
        let Some(host) = optional_text(&editor_text(&form.host, cx)) else {
            form.error = Some("Enter the machine's address.".into());
            cx.notify();
            return;
        };
        if configured_connections(cx)
            .iter()
            .any(|connection| connection.host == host)
        {
            form.error = Some(format!("{host} is already in your machines.").into());
            cx.notify();
            return;
        }
        let connection = SshConnection {
            host,
            username: optional_text(&editor_text(&form.user, cx)),
            nickname: optional_text(&editor_text(&form.name, cx)),
            host_label: optional_text(&editor_text(&form.label, cx)),
            ..Default::default()
        };
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings
                .remote
                .ssh_connections
                .get_or_insert_with(Vec::new)
                .push(connection);
        });
        self.adding = None;
        cx.notify();
    }

    fn render_check(&self, key: &str) -> impl IntoElement {
        let (color, text) = match self.checks.get(key) {
            None => (Color::Muted, "Not checked".to_string()),
            Some(CheckState::Checking) => (Color::Muted, "Checking…".to_string()),
            Some(CheckState::Done(AddressCheck::Answered(time))) => (
                Color::Success,
                format!("Answered in {} ms", time.as_millis()),
            ),
            Some(CheckState::Done(AddressCheck::NoAnswer)) => {
                (Color::Error, "No answer from here".to_string())
            }
            Some(CheckState::Done(AddressCheck::Unchecked)) => (
                Color::Muted,
                "Can't check: goes through a proxy".to_string(),
            ),
        };
        h_flex()
            .w_40()
            .flex_none()
            .gap_1p5()
            .child(Indicator::dot().color(color))
            .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
    }

    fn render_hub(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = agent_servers::HubSettings::get_global(cx).clone();
        let address = self
            .hub_address
            .get_or_insert_with(|| {
                text_input(
                    "100.80.18.21:7766",
                    settings.address.as_deref().unwrap_or_default(),
                    window,
                    cx,
                )
            })
            .clone();
        let token = self
            .hub_token
            .get_or_insert_with(|| {
                let editor = text_input("Paste the hub's token to change it", "", window, cx);
                editor.update(cx, |editor, cx| editor.set_masked(true, cx));
                editor
            })
            .clone();

        let (color, status): (Color, SharedString) = match agent_servers::HubBoard::status(cx) {
            agent_servers::HubStatus::Off => (Color::Muted, "Off: threads run in this app".into()),
            agent_servers::HubStatus::Connecting => (Color::Muted, "Connecting…".into()),
            agent_servers::HubStatus::NoToken => (Color::Warning, "Needs the hub's token".into()),
            agent_servers::HubStatus::Connected { machines, sessions } => (
                Color::Success,
                format!(
                    "Connected · runs threads for {} · {sessions} sessions",
                    if machines.is_empty() {
                        "no machines".to_string()
                    } else {
                        machines.join(", ")
                    }
                )
                .into(),
            ),
            agent_servers::HubStatus::Failed(error) => (Color::Error, error),
        };

        v_flex()
            .gap_2()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .justify_between()
                    .gap_4()
                    .child(
                        v_flex()
                            .child(Label::new("Hub"))
                            .child(
                                Label::new(
                                    "An always-on server that runs agent threads for the machines it serves, so they keep working with the laptop shut.",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .flex_none()
                            .child(Indicator::dot().color(color))
                            .child(Label::new(status).size(LabelSize::Small).color(Color::Muted)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(div().w_56().child(field("Address", &address, cx)))
                    .child(div().flex_1().child(field("Token", &token, cx)))
                    .child(
                        Button::new("hub-save", "Save")
                            .tab_index(0_isize)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.save_hub(window, cx))),
                    )
                    .child(
                        Button::new("hub-check", "Check")
                            .tab_index(0_isize)
                            .style(ButtonStyle::OutlinedGhost)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.hub_message = None;
                                agent_servers::HubBoard::refresh(cx);
                            })),
                    ),
            )
            .when(settings.address.is_some(), |this| {
                let enabled = settings.enabled;
                this.child(
                    Checkbox::new("hub-enabled", ToggleState::from(enabled))
                        .label("Run threads on the hub for the machines it serves")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            update_settings_file(this.fs.clone(), cx, move |settings, _| {
                                settings.remote.hub.get_or_insert_default().enabled = Some(!enabled);
                            });
                        })),
                )
            })
            .when_some(self.hub_message.clone(), |this, message| {
                this.child(Label::new(message).size(LabelSize::Small).color(Color::Muted))
            })
    }

    /// Saves the address to settings and the token to the Keychain.
    fn save_hub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(address), Some(token)) = (self.hub_address.clone(), self.hub_token.clone())
        else {
            return;
        };
        let address = editor_text(&address, cx).trim().to_string();
        let token_text = editor_text(&token, cx).trim().to_string();
        if address.is_empty() {
            self.hub_message = Some("Enter the hub's address first.".into());
            cx.notify();
            return;
        }
        let saved_address = address.clone();
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let hub = settings.remote.hub.get_or_insert_default();
            hub.address = Some(saved_address);
            hub.enabled.get_or_insert(true);
        });
        token.update(cx, |editor, cx| editor.set_text("", window, cx));
        if token_text.is_empty() {
            self.hub_message = Some("Saved.".into());
            cx.notify();
            return;
        }
        self.hub_message = Some("Saving the token…".into());
        cx.notify();
        self.hub_task = Some(cx.spawn(async move |this, cx| {
            let result = agent_servers::write_hub_token(&address, &token_text, cx).await;
            this.update(cx, |this, cx| {
                this.hub_message = Some(match &result {
                    Ok(()) => "Saved. The token is in your Keychain.".into(),
                    Err(error) => format!("Couldn't save the token: {error:#}").into(),
                });
                agent_servers::HubBoard::refresh(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_new_machine(
        &self,
        form: &NewMachineForm,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let host = editor_text(&form.host, cx);
        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("New machine"))
            .child(
                h_flex()
                    .gap_2()
                    .child(field("Name", &form.name, cx))
                    .child(field("User", &form.user, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(field("Address", &form.host, cx))
                    .child(field("Network label", &form.label, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Address check:").size(LabelSize::Small).color(Color::Muted))
                    .child(self.render_check(&check_key(&host, ""))),
            )
            .when_some(form.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                Label::new(
                    "Yonder installs its server the first time you open a project on the machine. You can add more addresses after saving.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("machines-new-save", "Save Machine")
                            .tab_index(0_isize)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _, cx| this.save_new_machine(cx))),
                    )
                    .child(
                        Button::new("machines-new-check", "Check Address")
                            .tab_index(0_isize)
                            .style(ButtonStyle::OutlinedGhost)
                            .on_click(cx.listener(|this, _, _, cx| this.check_new_machine(cx))),
                    )
                    .child(
                        Button::new("machines-new-cancel", "Cancel")
                            .tab_index(0_isize)
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.adding = None;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_editor(
        &self,
        editor: &MachineEditor,
        connection: &SshConnection,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let draft = self.editor_draft(editor, cx);
        let saved = MachineDraft::from_connection(connection);
        let dirty = draft != saved;
        let primary_changed = draft
            .addresses
            .first()
            .is_some_and(|address| address.host.trim() != connection.host);
        let address_count = editor.addresses.len();
        let name = machine_name(connection);

        let address_rows = editor.addresses.iter().enumerate().map(|(index, inputs)| {
            let host = editor_text(&inputs.host, cx);
            let port = editor_text(&inputs.port, cx);
            h_flex()
                .gap_2()
                .child(
                    h_flex()
                        .flex_none()
                        .child(
                            IconButton::new(("machines-address-up", index), IconName::ArrowUp)
                                .tab_index(0_isize)
                                .icon_size(IconSize::Small)
                                .disabled(index == 0)
                                .tooltip(Tooltip::text("Try earlier"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.move_address(index, index.saturating_sub(1), cx)
                                })),
                        )
                        .child(
                            IconButton::new(("machines-address-down", index), IconName::ArrowDown)
                                .tab_index(0_isize)
                                .icon_size(IconSize::Small)
                                .disabled(index + 1 == address_count)
                                .tooltip(Tooltip::text("Try later"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.move_address(index, index + 1, cx)
                                })),
                        ),
                )
                .child(div().w_32().flex_none().child(input_box(&inputs.label, cx)))
                .child(div().flex_1().min_w_0().child(input_box(&inputs.host, cx)))
                .child(div().w_16().flex_none().child(input_box(&inputs.port, cx)))
                .child(self.render_check(&check_key(&host, &port)))
                .child(div().w_16().flex_none().child(if index == 0 {
                    Label::new("Primary")
                        .size(LabelSize::Small)
                        .color(Color::Accent)
                        .into_any_element()
                } else {
                    IconButton::new(("machines-address-delete", index), IconName::Trash)
                        .tab_index(0_isize)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Delete address"))
                        .on_click(cx.listener(move |this, _, _, cx| this.delete_address(index, cx)))
                        .into_any_element()
                }))
        });

        v_flex()
            .gap_3()
            .pl_7()
            .pr_2()
            .pt_1()
            .pb_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(field("Name", &editor.name, cx))
                    .child(field("User", &editor.user, cx)),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                Label::new("Addresses, tried top to bottom")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("machines-check", "Check Addresses")
                                            .tab_index(0_isize)
                                            .style(ButtonStyle::OutlinedGhost)
                                            .label_size(LabelSize::Small)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.check_addresses(cx)),
                                            ),
                                    )
                                    .child(
                                        Button::new("machines-add-address", "Add Address")
                                            .tab_index(0_isize)
                                            .style(ButtonStyle::OutlinedGhost)
                                            .label_size(LabelSize::Small)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.add_address(window, cx)
                                            })),
                                    ),
                            ),
                    )
                    .children(address_rows),
            )
            .when(primary_changed, |this| {
                this.child(
                    Label::new(
                        "The first address names the machine. Projects saved under the old one still list under it as long as it stays one of the addresses.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Warning),
                )
            })
            .when_some(self.accounts.clone(), |this, accounts| {
                this.child(accounts.update(cx, |accounts, cx| {
                    accounts.render_machine_agents(connection, cx)
                }))
            })
            .child(
                v_flex()
                    .gap_2()
                    .child(Label::new("Advanced").size(LabelSize::Small).color(Color::Muted))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(field("Connection timeout, seconds", &editor.timeout, cx))
                            .child(field("Extra SSH arguments", &editor.args, cx)),
                    )
                    .child(
                        Checkbox::new(
                            "machines-upload-binary",
                            ToggleState::from(editor.upload_binary),
                        )
                        .label("Upload Yonder's server from this Mac instead of downloading it on the machine")
                        .label_size(LabelSize::Small)
                        .on_click({
                            let this = cx.weak_entity();
                            move |state, _, cx| {
                                this.update(cx, |this, cx| {
                                    if let Some(editor) = this.editing.as_mut() {
                                        editor.upload_binary = *state == ToggleState::Selected;
                                        cx.notify();
                                    }
                                })
                                .ok();
                            }
                        }),
                    ),
            )
            .when_some(editor.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("machines-save", "Save")
                                    .tab_index(0_isize)
                                    .style(ButtonStyle::Filled)
                                    .disabled(!dirty)
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            )
                            .child(
                                Button::new("machines-discard", "Discard Changes")
                                    .tab_index(0_isize)
                                    .style(ButtonStyle::Subtle)
                                    .disabled(!dirty)
                                    .on_click(cx.listener({
                                        let index = editor.index;
                                        move |this, _, window, cx| {
                                            this.editing = None;
                                            this.toggle_machine(index, window, cx);
                                        }
                                    })),
                            ),
                    )
                    .child(if editor.confirm_remove {
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(format!(
                                    "Remove {name} from your settings? Files on {name} aren't touched."
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .child(
                                Button::new("machines-remove-confirm", format!("Remove {name}"))
                                    .tab_index(0_isize)
                                    .style(ButtonStyle::Tinted(ui::TintColor::Error))
                                    .on_click(cx.listener(|this, _, _, cx| this.remove(cx))),
                            )
                            .child(
                                Button::new("machines-remove-cancel", "Keep")
                                    .tab_index(0_isize)
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(editor) = this.editing.as_mut() {
                                            editor.confirm_remove = false;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .into_any_element()
                    } else {
                        Button::new("machines-remove", format!("Remove {name}…"))
                            .tab_index(0_isize)
                            .style(ButtonStyle::Subtle)
                            .color(Color::Error)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(editor) = this.editing.as_mut() {
                                    editor.confirm_remove = true;
                                    cx.notify();
                                }
                            }))
                            .into_any_element()
                    }),
            )
    }
}

impl Render for MachinesPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hub = self.render_hub(window, cx).into_any_element();
        let connections = configured_connections(cx);
        if self
            .editing
            .as_ref()
            .is_some_and(|editor| editor.index >= connections.len())
        {
            self.editing = None;
        }

        let mut list = v_flex().w_full().gap_1();
        if connections.is_empty() && self.adding.is_none() {
            list = list.child(
                Label::new("No machines yet. Add one and its projects appear in the fleet panel.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        for (index, connection) in connections.iter().enumerate() {
            let expanded = self
                .editing
                .as_ref()
                .is_some_and(|editor| editor.index == index);
            let address_count = 1 + connection.secondary_hosts.len();
            let user_and_host = match &connection.username {
                Some(user) => format!("{user}@{}", connection.host),
                None => connection.host.clone(),
            };
            list = list.child(
                ListItem::new(("machines-row", index))
                    .spacing(ListItemSpacing::Sparse)
                    .toggle(Some(expanded))
                    .on_toggle(cx.listener(move |this, _, window, cx| {
                        this.toggle_machine(index, window, cx)
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_machine(index, window, cx)
                    }))
                    .start_slot(Icon::new(IconName::Server).color(Color::Muted))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(machine_name(connection)))
                            .child(
                                Label::new(user_and_host)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .end_slot(
                        Label::new(if address_count == 1 {
                            "1 address".to_string()
                        } else {
                            format!("{address_count} addresses")
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            );
            if let Some(editor) = self.editing.as_ref().filter(|_| expanded) {
                list = list.child(self.render_editor(editor, connection, cx));
            }
            list = list.child(Divider::horizontal());
        }

        v_flex()
            .px_8()
            .pt_2()
            .pb_6()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .gap_4()
                    .child(
                        v_flex()
                            .child(Label::new("Machines"))
                            .child(
                                Label::new(
                                    "Servers Yonder connects to over SSH. Edits save to your settings file when you press Save.",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        Button::new("machines-add", "Add Machine")
                            .tab_index(0_isize)
                            .style(ButtonStyle::OutlinedGhost)
                            .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_new_machine(window, cx)
                            })),
                    ),
            )
            .when_some(self.adding.as_ref(), |this, form| {
                this.child(self.render_new_machine(form, cx))
            })
            .child(list)
            .child(hub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu06() -> SshConnection {
        SshConnection {
            host: "172.17.21.106".to_string(),
            username: Some("ayasser".to_string()),
            nickname: Some("gpu-06".to_string()),
            host_label: Some("Office".to_string()),
            secondary_hosts: vec![SshSecondaryHost {
                host: "gpu-06.example.ts.net".to_string(),
                port: None,
                label: Some("Tailscale".to_string()),
            }],
            args: vec!["-o".to_string(), "ServerAliveInterval=30".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn a_draft_writes_back_what_it_read() {
        let original = gpu06();
        let draft = MachineDraft::from_connection(&original);
        assert_eq!(draft.addresses.len(), 2);
        assert_eq!(draft.args, "-o ServerAliveInterval=30");
        let mut written = original.clone();
        draft.apply(&mut written).unwrap();
        assert_eq!(written, original);
    }

    #[test]
    fn moving_an_address_to_the_top_makes_it_the_primary() {
        let mut connection = gpu06();
        let mut draft = MachineDraft::from_connection(&connection);
        draft.addresses.swap(0, 1);
        draft.apply(&mut connection).unwrap();
        assert_eq!(connection.host, "gpu-06.example.ts.net");
        assert_eq!(connection.host_label.as_deref(), Some("Tailscale"));
        assert_eq!(connection.secondary_hosts.len(), 1);
        assert_eq!(connection.secondary_hosts[0].host, "172.17.21.106");
        assert_eq!(
            connection.secondary_hosts[0].label.as_deref(),
            Some("Office")
        );
    }

    #[test]
    fn empty_rows_are_dropped_and_ports_are_checked() {
        let mut connection = gpu06();
        let mut draft = MachineDraft::from_connection(&connection);
        draft.addresses.push(AddressDraft::default());
        draft.addresses[1].port = "2222".to_string();
        draft.apply(&mut connection).unwrap();
        assert_eq!(connection.secondary_hosts.len(), 1);
        assert_eq!(connection.secondary_hosts[0].port, Some(2222));

        draft.addresses[1].port = "ssh".to_string();
        assert!(
            draft
                .apply(&mut connection)
                .unwrap_err()
                .contains("Address 2")
        );

        draft.addresses[1].port.clear();
        draft.addresses[0].host = "  ".to_string();
        assert!(draft.apply(&mut connection).is_err());
    }

    #[test]
    fn fields_the_page_does_not_edit_are_kept() {
        let mut connection = gpu06();
        connection.projects.insert(settings::RemoteProject {
            paths: vec!["/home/ayasser/Projects".to_string()],
        });
        let mut draft = MachineDraft::from_connection(&connection);
        draft.name = "GPU 6".to_string();
        draft.timeout = "30".to_string();
        draft.upload_binary = true;
        draft.apply(&mut connection).unwrap();
        assert_eq!(connection.projects.len(), 1);
        assert_eq!(connection.nickname.as_deref(), Some("GPU 6"));
        assert_eq!(connection.connection_timeout, Some(30));
        assert_eq!(connection.upload_binary_over_ssh, Some(true));

        draft.timeout = "soon".to_string();
        assert!(
            draft
                .apply(&mut connection)
                .unwrap_err()
                .contains("timeout")
        );
    }
}
