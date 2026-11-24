use collections::HashMap;
use editor::Editor;
use fs::Fs;
use gpui::{
    ClipboardItem, Entity, Focusable as _, ReadGlobal as _, Subscription, Task, prelude::*,
};
use settings::{
    AllAgentServersSettings, CustomAgentServerSettings, SettingsStore, SshConnection,
    update_settings_file,
};
use std::sync::Arc;
use ui::{Divider, Indicator, Tooltip, prelude::*};
use util::command::{Stdio, new_command};

use crate::SettingsWindow;
use machine_setup::{shell_quote, ssh_args};

const CLAUDE_AGENT: &str = "claude-acp";
const CODEX_AGENT: &str = "codex-acp";
const DEFAULT_CLAUDE_DIR: &str = "~/.claude";
const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";
const LOCAL_MACHINE: &str = if cfg!(target_os = "macos") {
    "This Mac"
} else {
    "This Computer"
};

pub(crate) fn render_agent_accounts_page(
    settings_window: &SettingsWindow,
    _window: &mut Window,
    _cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    match &settings_window.agent_accounts_page {
        Some(page) => page.clone().into_any_element(),
        None => gpui::Empty.into_any_element(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountKind {
    Claude { config_dir: String },
    Codex,
}

/// One agent account from `agent_servers`, such as `claude-acp@work`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Account {
    pub agent_id: String,
    pub kind: AccountKind,
}

impl Account {
    pub fn title(&self) -> String {
        match &self.kind {
            AccountKind::Codex => "Codex".to_string(),
            AccountKind::Claude { .. } => match self.agent_id.split_once('@') {
                Some((_, name)) => format!("Claude ({name})"),
                None => "Claude (default)".to_string(),
            },
        }
    }

    pub fn detail(&self) -> String {
        match &self.kind {
            AccountKind::Codex => "~/.codex · ChatGPT sign-in".to_string(),
            AccountKind::Claude { config_dir } => config_dir.clone(),
        }
    }

    /// What to run on a machine to sign this account in.
    pub fn sign_in_command(&self) -> Option<String> {
        match &self.kind {
            AccountKind::Codex => None,
            AccountKind::Claude { config_dir } if config_dir == DEFAULT_CLAUDE_DIR => {
                Some("claude auth login".to_string())
            }
            AccountKind::Claude { config_dir } => {
                Some(format!("{CONFIG_DIR_VAR}={config_dir} claude auth login"))
            }
        }
    }
}

/// The Claude and Codex accounts configured in `agent_servers`, default
/// Claude first.
pub(crate) fn accounts_from_settings(servers: Option<&AllAgentServersSettings>) -> Vec<Account> {
    let mut accounts = Vec::new();
    let Some(servers) = servers else {
        return accounts;
    };
    let mut ids = servers.0.keys().cloned().collect::<Vec<_>>();
    ids.sort();
    for id in ids {
        let Some(server) = servers.0.get(&id) else {
            continue;
        };
        let base = id.split('@').next().unwrap_or_default();
        let env = match server {
            CustomAgentServerSettings::Registry { env, .. } => env,
            CustomAgentServerSettings::Custom { .. } => continue,
        };
        if base == CLAUDE_AGENT {
            let config_dir = env
                .get(CONFIG_DIR_VAR)
                .cloned()
                .unwrap_or_else(|| DEFAULT_CLAUDE_DIR.to_string());
            accounts.push(Account {
                agent_id: id,
                kind: AccountKind::Claude { config_dir },
            });
        } else if id == CODEX_AGENT {
            accounts.push(Account {
                agent_id: id,
                kind: AccountKind::Codex,
            });
        }
    }
    accounts.sort_by_key(|account| match &account.kind {
        AccountKind::Claude { .. } if account.agent_id == CLAUDE_AGENT => 0,
        AccountKind::Claude { .. } => 1,
        AccountKind::Codex => 2,
    });
    accounts
}

fn configured_accounts(cx: &App) -> Vec<Account> {
    accounts_from_settings(
        SettingsStore::global(cx)
            .raw_user_settings()
            .and_then(|user| user.content.agent_servers.as_ref()),
    )
}

fn configured_machines(cx: &App) -> Vec<SshConnection> {
    SettingsStore::global(cx)
        .raw_user_settings()
        .and_then(|user| user.content.remote.ssh_connections.clone())
        .unwrap_or_default()
}

pub(crate) fn machine_key(connection: &SshConnection) -> String {
    match &connection.username {
        Some(user) => format!("{user}@{}", connection.host),
        None => connection.host.clone(),
    }
}

fn machine_name(connection: &SshConnection) -> String {
    connection
        .nickname
        .clone()
        .unwrap_or_else(|| connection.host.clone())
}

/// Account names become part of an agent id and a folder name.
pub(crate) fn validate_account_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Enter a name, such as work.".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Use only letters, digits, - and _ in the name.".to_string());
    }
    Ok(())
}

/// Folders go into a shell script, so only plain path characters are allowed.
pub(crate) fn is_plain_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '~'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SignIn {
    SignedIn { detail: Option<String> },
    FolderOnly,
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct MachineReport {
    /// By Claude config folder as written in settings, plus `codex`.
    pub sign_ins: HashMap<String, SignIn>,
    /// Claude config folders found in the home folder, such as `~/.claude_personal`.
    pub found_folders: Vec<String>,
}

const CODEX_KEY: &str = "codex";

/// A POSIX shell script that reports, for each Claude folder, whether it holds
/// a sign-in. It reads the account email and file existence only, never tokens.
pub(crate) fn status_script(claude_dirs: &[String]) -> String {
    let mut script = String::from(
        r#"check_claude() {
  dir="$1"
  case "$dir" in "~"*) dir="$HOME${dir#\~}";; esac
  if [ "$dir" = "$HOME/.claude" ]; then cfg="$HOME/.claude.json"; else cfg="$dir/.claude.json"; fi
  email=""
  if [ -f "$cfg" ]; then
    email=$(grep -o '"emailAddress"[[:space:]]*:[[:space:]]*"[^"]*"' "$cfg" | head -n 1 | sed 's/.*:[[:space:]]*"\(.*\)"/\1/')
  fi
  if [ -n "$email" ] || [ -f "$dir/.credentials.json" ]; then echo "claude|$1|signed_in|$email"
  elif [ -d "$dir" ]; then echo "claude|$1|folder|"
  else echo "claude|$1|missing|"; fi
}
"#,
    );
    for dir in claude_dirs.iter().filter(|dir| is_plain_path(dir)) {
        script.push_str(&format!("check_claude '{dir}'\n"));
    }
    script.push_str(
        r#"if [ -f "$HOME/.codex/auth.json" ]; then
  refreshed=$(grep -o '"last_refresh"[[:space:]]*:[[:space:]]*"[0-9-]*' "$HOME/.codex/auth.json" | head -n 1 | sed 's/.*"//')
  echo "codex||signed_in|$refreshed"
else echo "codex||missing|"; fi
for d in "$HOME"/.claude_* "$HOME"/.claude-*; do
  [ -d "$d" ] && echo "found|~/${d#$HOME/}||"
done
exit 0
"#,
    );
    script
}

pub(crate) fn parse_status_output(output: &str) -> MachineReport {
    let mut report = MachineReport::default();
    for line in output.lines() {
        let mut fields = line.splitn(4, '|');
        let (Some(kind), Some(dir), Some(state), detail) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let detail = detail
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
            .map(str::to_string);
        let sign_in = match state {
            "signed_in" => SignIn::SignedIn { detail },
            "folder" => SignIn::FolderOnly,
            "missing" => SignIn::Missing,
            _ => {
                if kind == "found" && is_plain_path(dir) {
                    report.found_folders.push(dir.to_string());
                }
                continue;
            }
        };
        match kind {
            "claude" => {
                report.sign_ins.insert(dir.to_string(), sign_in);
            }
            "codex" => {
                report.sign_ins.insert(CODEX_KEY.to_string(), sign_in);
            }
            _ => {}
        }
    }
    report
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MachineState {
    Checking,
    Done(MachineReport),
    Unreachable(String),
}

async fn check_machine(machine: Option<SshConnection>, script: String) -> MachineState {
    let Some(machine) = machine else {
        let output = new_command("sh")
            .args(["-c", script.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;
        return match output {
            Ok(output) => MachineState::Done(parse_status_output(&String::from_utf8_lossy(
                &output.stdout,
            ))),
            Err(error) => MachineState::Unreachable(error.to_string()),
        };
    };
    let remote_command = format!("sh -c {}", shell_quote(&script));
    let mut last_error = String::from("No address answered.");
    for address in 0..=machine.secondary_hosts.len() {
        let output = new_command("ssh")
            .args(ssh_args(&machine, address, &remote_command))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;
        match output {
            Ok(output) if output.status.success() => {
                return MachineState::Done(parse_status_output(&String::from_utf8_lossy(
                    &output.stdout,
                )));
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if let Some(line) = stderr.lines().rev().find(|line| !line.trim().is_empty()) {
                    last_error = line.trim().to_string();
                }
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    MachineState::Unreachable(last_error)
}

struct NewAccountForm {
    name: Entity<Editor>,
    folder: Entity<Editor>,
    error: Option<SharedString>,
}

pub(crate) struct AgentAccountsPage {
    fs: Arc<dyn Fs>,
    /// By machine key; [`LOCAL_MACHINE`] for this Mac.
    machines: HashMap<String, MachineState>,
    checked_once: bool,
    check_tasks: Vec<Task<()>>,
    adding: Option<NewAccountForm>,
    sign_in: Option<(String, String, String)>,
    copied: bool,
    _settings_subscription: Subscription,
}

fn text_input(
    placeholder: &str,
    text: &str,
    window: &mut Window,
    cx: &mut Context<AgentAccountsPage>,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text(text, window, cx);
        editor
    })
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

impl AgentAccountsPage {
    pub fn new(fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        Self {
            fs,
            machines: HashMap::default(),
            checked_once: false,
            check_tasks: Vec::new(),
            adding: None,
            sign_in: None,
            copied: false,
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }

    /// Checks every machine once, the first time a page shows account status.
    pub fn ensure_checked(&mut self, cx: &mut Context<Self>) {
        // The check runs `sh` and `ssh`, which deterministic tests can't host.
        if !self.checked_once && !cfg!(test) {
            self.checked_once = true;
            self.refresh(cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let dirs = configured_accounts(cx)
            .into_iter()
            .filter_map(|account| match account.kind {
                AccountKind::Claude { config_dir } => Some(config_dir),
                AccountKind::Codex => None,
            })
            .collect::<Vec<_>>();
        let script = status_script(&dirs);
        let targets = std::iter::once((LOCAL_MACHINE.to_string(), None)).chain(
            configured_machines(cx)
                .into_iter()
                .map(|machine| (machine_key(&machine), Some(machine))),
        );
        self.check_tasks.clear();
        for (key, machine) in targets {
            self.machines.insert(key.clone(), MachineState::Checking);
            let script = script.clone();
            self.check_tasks.push(cx.spawn(async move |this, cx| {
                let state = check_machine(machine, script).await;
                this.update(cx, |this, cx| {
                    this.machines.insert(key, state);
                    cx.notify();
                })
                .ok();
            }));
        }
        cx.notify();
    }

    fn open_sign_in(&mut self, account: &Account, machine: String, cx: &mut Context<Self>) {
        if let Some(command) = account.sign_in_command() {
            self.sign_in = Some((account.title(), machine, command));
            self.copied = false;
            cx.notify();
        }
    }

    fn open_new_account(
        &mut self,
        name: &str,
        folder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.adding = Some(NewAccountForm {
            name: text_input("work", name, window, cx),
            folder: text_input("~/.claude-work", folder, window, cx),
            error: None,
        });
        cx.notify();
    }

    fn save_new_account(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.adding.as_mut() else {
            return;
        };
        let name = form.name.read(cx).text(cx).trim().to_string();
        let folder = form.folder.read(cx).text(cx).trim().to_string();
        let folder = if folder.is_empty() {
            format!("~/.claude-{name}")
        } else {
            folder
        };
        let agent_id = format!("{CLAUDE_AGENT}@{name}");
        let error = validate_account_name(&name).err().or_else(|| {
            if !is_plain_path(&folder) {
                Some("Use a plain folder path such as ~/.claude-work.".to_string())
            } else if configured_accounts(cx)
                .iter()
                .any(|account| account.agent_id == agent_id)
            {
                Some(format!("There is already an account named {name}."))
            } else {
                None
            }
        });
        if let Some(error) = error {
            form.error = Some(error.into());
            cx.notify();
            return;
        }
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings
                .agent_servers
                .get_or_insert_with(Default::default)
                .0
                .insert(
                    agent_id,
                    CustomAgentServerSettings::Registry {
                        env: [(CONFIG_DIR_VAR.to_string(), folder)].into_iter().collect(),
                        default_mode: None,
                        default_config_options: Default::default(),
                        favorite_config_option_values: Default::default(),
                    },
                );
        });
        self.adding = None;
        self.checked_once = false;
        cx.notify();
    }

    fn remove_account(&mut self, agent_id: String, cx: &mut Context<Self>) {
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            if let Some(servers) = settings.agent_servers.as_mut() {
                servers.0.remove(&agent_id);
            }
        });
        cx.notify();
    }

    fn render_status(
        &self,
        account: &Account,
        machine: &str,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.machines.get(machine);
        let key = match &account.kind {
            AccountKind::Claude { config_dir } => config_dir.clone(),
            AccountKind::Codex => CODEX_KEY.to_string(),
        };
        let status = |color: Color, text: String, tooltip: Option<String>| {
            h_flex()
                .id(("accounts-status", index))
                .min_w_0()
                .gap_1p5()
                .child(div().flex_none().child(Indicator::dot().color(color)))
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
                .when_some(tooltip, |this, tooltip| {
                    this.tooltip(Tooltip::text(tooltip))
                })
                .into_any_element()
        };
        match state {
            None => status(Color::Muted, "Not checked".to_string(), None),
            Some(MachineState::Checking) => status(Color::Muted, "Checking…".to_string(), None),
            Some(MachineState::Unreachable(error)) => {
                status(Color::Error, "Can't reach".to_string(), Some(error.clone()))
            }
            Some(MachineState::Done(report)) => match report.sign_ins.get(&key) {
                Some(SignIn::SignedIn { detail }) => match (&account.kind, detail) {
                    (AccountKind::Codex, Some(date)) => status(
                        Color::Success,
                        "Signed in".to_string(),
                        Some(format!("Sign-in last refreshed {date}")),
                    ),
                    (_, Some(email)) => status(Color::Success, email.clone(), Some(email.clone())),
                    (_, None) => status(Color::Success, "Signed in".to_string(), None),
                },
                _ if account.sign_in_command().is_none() => {
                    status(Color::Muted, "Not signed in".to_string(), None)
                }
                _ => {
                    let account = account.clone();
                    let machine = machine.to_string();
                    Button::new(("accounts-sign-in", index), "Sign In…")
                        .tab_index(0_isize)
                        .style(ButtonStyle::OutlinedGhost)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_sign_in(&account, machine.clone(), cx)
                        }))
                        .into_any_element()
                }
            },
        }
    }

    /// The sign-in status of every account on one machine, for the Machines page.
    pub fn render_machine_agents(
        &mut self,
        machine: &SshConnection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_checked(cx);
        let key = machine_key(machine);
        let accounts = configured_accounts(cx);
        let rows = accounts.iter().enumerate().map(|(index, account)| {
            h_flex()
                .justify_between()
                .gap_2()
                .py_1()
                .child(
                    v_flex()
                        .child(Label::new(account.title()).size(LabelSize::Small))
                        .child(
                            Label::new(account.detail())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(self.render_status(account, &key, 1000 + index, cx))
        });
        v_flex()
            .gap_1()
            .child(
                Label::new(format!("Agents on {}", machine_name(machine)))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(rows)
            .when_some(self.render_sign_in_panel(cx), |this, panel| {
                this.child(panel)
            })
            .into_any_element()
    }

    fn render_sign_in_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (account, machine, command) = self.sign_in.clone()?;
        Some(
            v_flex()
                .gap_1p5()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .child(Label::new(format!("Sign in to {account} on {machine}")))
                .child(
                    Label::new(if machine == LOCAL_MACHINE {
                        "Run this in a terminal on this Mac:".to_string()
                    } else {
                        format!("Run this in a terminal on {machine}, for example one opened from a project there:")
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(cx.theme().colors().editor_background)
                        .child(Label::new(command.clone()).buffer_font(cx)),
                )
                .child(
                    Label::new(
                        "Finish in the browser it links to, then press Refresh to update the status.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(
                                "accounts-copy",
                                if self.copied { "Copied" } else { "Copy Command" },
                            )
                            .tab_index(0_isize)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
                                this.copied = true;
                                cx.notify();
                            })),
                        )
                        .child(
                            Button::new("accounts-refresh-after-sign-in", "Refresh")
                                .tab_index(0_isize)
                                .style(ButtonStyle::OutlinedGhost)
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        )
                        .child(
                            Button::new("accounts-sign-in-close", "Close")
                                .tab_index(0_isize)
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.sign_in = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_new_account(&self, form: &NewAccountForm, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("New Claude account"))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(Label::new("Name").size(LabelSize::Small).color(Color::Muted))
                            .child(input_box(&form.name, cx)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(
                                Label::new("Config folder on each machine")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(input_box(&form.folder, cx)),
                    ),
            )
            .child(
                Label::new(
                    "Adds a Claude agent named claude-acp@<name> that uses this folder. Sign in once on each machine where you'll use it.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .when_some(form.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("accounts-new-save", "Add Account")
                            .tab_index(0_isize)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _, cx| this.save_new_account(cx))),
                    )
                    .child(
                        Button::new("accounts-new-cancel", "Cancel")
                            .tab_index(0_isize)
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.adding = None;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for AgentAccountsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_checked(cx);
        let accounts = configured_accounts(cx);
        let machines = configured_machines(cx);
        let columns = std::iter::once((LOCAL_MACHINE.to_string(), LOCAL_MACHINE.to_string()))
            .chain(
                machines
                    .iter()
                    .map(|machine| (machine_key(machine), machine_name(machine))),
            )
            .collect::<Vec<_>>();

        let configured_dirs = accounts
            .iter()
            .filter_map(|account| match &account.kind {
                AccountKind::Claude { config_dir } => Some(config_dir.clone()),
                AccountKind::Codex => None,
            })
            .collect::<Vec<_>>();
        let found_on_mac = match self.machines.get(LOCAL_MACHINE) {
            Some(MachineState::Done(report)) => report
                .found_folders
                .iter()
                .filter(|folder| !configured_dirs.contains(folder))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };

        let mut grid = v_flex().w_full();
        grid = grid.child(
            h_flex()
                .py_1()
                .gap_2()
                .child(div().w_48().flex_none())
                .children(columns.iter().map(|(_, name)| {
                    div().flex_1().min_w_0().child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })),
        );
        for (row, account) in accounts.iter().enumerate() {
            let removable = account.agent_id != CLAUDE_AGENT && account.agent_id != CODEX_AGENT;
            grid = grid.child(Divider::horizontal()).child(
                h_flex()
                    .py_2()
                    .gap_2()
                    .child(
                        h_flex()
                            .w_48()
                            .flex_none()
                            .gap_1()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(Label::new(account.title()))
                                    .child(
                                        Label::new(account.detail())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .when(removable, |this| {
                                let agent_id = account.agent_id.clone();
                                this.child(
                                    IconButton::new(("accounts-remove", row), IconName::Trash)
                                        .tab_index(0_isize)
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Muted)
                                        .tooltip(Tooltip::text(
                                            "Remove this account from Yonder. Its sign-ins on your machines stay.",
                                        ))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.remove_account(agent_id.clone(), cx)
                                        })),
                                )
                            }),
                    )
                    .children(columns.iter().enumerate().map(|(column, (key, _))| {
                        h_flex().flex_1().min_w_0().child(self.render_status(
                            account,
                            key,
                            row * 100 + column,
                            cx,
                        ))
                    })),
            );
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
                            .flex_1()
                            .min_w_0()
                            .child(Label::new("Agent Accounts"))
                            .child(
                                Label::new(
                                    "Each Claude account is its own agent with its own config folder on every machine. Codex uses one ChatGPT sign-in per machine.",
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_2()
                            .child(
                                Button::new("accounts-refresh", "Refresh")
                                    .tab_index(0_isize)
                                    .style(ButtonStyle::OutlinedGhost)
                                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                            )
                            .child(
                                Button::new("accounts-add", "Add Claude Account")
                                    .tab_index(0_isize)
                                    .style(ButtonStyle::OutlinedGhost)
                                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_new_account("", "", window, cx)
                                    })),
                            ),
                    ),
            )
            .children(found_on_mac.into_iter().enumerate().map(|(index, folder)| {
                let name = folder
                    .trim_start_matches("~/.claude")
                    .trim_start_matches(['_', '-'])
                    .to_string();
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(format!(
                            "Found {folder} on this Mac. It isn't an account in Yonder yet."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Button::new(("accounts-add-found", index), format!("Add as {name}"))
                            .tab_index(0_isize)
                            .style(ButtonStyle::OutlinedGhost)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_new_account(&name, &folder, window, cx)
                            })),
                    )
            }))
            .when_some(
                self.adding.as_ref().map(|form| self.render_new_account(form, cx)),
                |this, form| this.child(form),
            )
            .child(grid)
            .when_some(self.render_sign_in_panel(cx), |this, panel| this.child(panel))
            .child(
                Label::new(
                    "Status comes from the sign-in files on each machine, checked over SSH when this page first opens. Yonder never reads the tokens.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(env: &[(&str, &str)]) -> CustomAgentServerSettings {
        CustomAgentServerSettings::Registry {
            env: env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            default_mode: None,
            default_config_options: Default::default(),
            favorite_config_option_values: Default::default(),
        }
    }

    #[test]
    fn accounts_come_from_agent_servers_default_first() {
        let servers = AllAgentServersSettings(
            [
                ("codex-acp".to_string(), registry(&[])),
                (
                    "claude-acp@work".to_string(),
                    registry(&[("CLAUDE_CONFIG_DIR", "~/.claude-work")]),
                ),
                ("claude-acp".to_string(), registry(&[])),
                ("gemini".to_string(), registry(&[])),
            ]
            .into_iter()
            .collect(),
        );
        let accounts = accounts_from_settings(Some(&servers));
        assert_eq!(
            accounts.iter().map(Account::title).collect::<Vec<_>>(),
            ["Claude (default)", "Claude (work)", "Codex"]
        );
        assert_eq!(accounts[0].detail(), "~/.claude");
        assert_eq!(
            accounts[1].sign_in_command().as_deref(),
            Some("CLAUDE_CONFIG_DIR=~/.claude-work claude auth login")
        );
        assert_eq!(
            accounts[0].sign_in_command().as_deref(),
            Some("claude auth login")
        );
        assert_eq!(accounts[2].sign_in_command(), None);
    }

    #[test]
    fn parses_the_status_report() {
        let report = parse_status_output(
            "claude|~/.claude|signed_in|me@example.com\n\
             claude|~/.claude-work|folder|\n\
             claude|~/.claude_x|missing|\n\
             codex||signed_in|2026-11-21\n\
             found|~/.claude_personal||\n\
             noise line\n",
        );
        assert_eq!(
            report.sign_ins.get("~/.claude"),
            Some(&SignIn::SignedIn {
                detail: Some("me@example.com".to_string())
            })
        );
        assert_eq!(
            report.sign_ins.get("~/.claude-work"),
            Some(&SignIn::FolderOnly)
        );
        assert_eq!(report.sign_ins.get("~/.claude_x"), Some(&SignIn::Missing));
        assert_eq!(
            report.sign_ins.get(CODEX_KEY),
            Some(&SignIn::SignedIn {
                detail: Some("2026-11-21".to_string())
            })
        );
        assert_eq!(report.found_folders, ["~/.claude_personal"]);
    }

    #[test]
    fn the_script_only_takes_plain_folders() {
        let script = status_script(&["~/.claude".to_string(), "~/x; rm -rf ~".to_string()]);
        assert!(script.contains("check_claude '~/.claude'"));
        assert!(!script.contains("rm -rf"));
        assert!(validate_account_name("work").is_ok());
        assert!(validate_account_name("my work").is_err());
        assert!(validate_account_name("").is_err());
    }

    #[test]
    fn the_status_script_runs_against_a_home_folder() {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path();
        std::fs::write(
            home_path.join(".claude.json"),
            r#"{"oauthAccount": {"emailAddress": "me@example.com", "accountUuid": "x"}}"#,
        )
        .unwrap();
        std::fs::create_dir(home_path.join(".claude_personal")).unwrap();
        std::fs::create_dir_all(home_path.join(".codex")).unwrap();
        std::fs::write(
            home_path.join(".codex/auth.json"),
            r#"{"tokens": {}, "last_refresh": "2026-11-21T18:03:23Z"}"#,
        )
        .unwrap();
        let script = status_script(&[
            "~/.claude".to_string(),
            "~/.claude_personal".to_string(),
            "~/.claude-none".to_string(),
        ]);
        #[allow(
            clippy::disallowed_methods,
            reason = "synchronous test runs the script directly"
        )]
        let output = std::process::Command::new("sh")
            .args(["-c", &script])
            .env("HOME", home_path)
            .output()
            .unwrap();
        let report = parse_status_output(&String::from_utf8_lossy(&output.stdout));
        assert_eq!(
            report.sign_ins.get("~/.claude"),
            Some(&SignIn::SignedIn {
                detail: Some("me@example.com".to_string())
            })
        );
        assert_eq!(
            report.sign_ins.get("~/.claude_personal"),
            Some(&SignIn::FolderOnly)
        );
        assert_eq!(
            report.sign_ins.get("~/.claude-none"),
            Some(&SignIn::Missing)
        );
        assert_eq!(
            report.sign_ins.get(CODEX_KEY),
            Some(&SignIn::SignedIn {
                detail: Some("2026-11-21".to_string())
            })
        );
        assert_eq!(report.found_folders, ["~/.claude_personal"]);
    }

    #[test]
    fn ssh_args_use_each_address_with_the_primary_host_key() {
        let machine = SshConnection {
            host: "10.0.0.6".to_string(),
            username: Some("ayasser".to_string()),
            secondary_hosts: vec![settings::SshSecondaryHost {
                host: "gpu-06.example.ts.net".to_string(),
                port: None,
                label: None,
            }],
            ..Default::default()
        };
        let primary = ssh_args(&machine, 0, "true");
        assert_eq!(&primary[primary.len() - 2..], ["10.0.0.6", "true"]);
        assert!(!primary.iter().any(|arg| arg.starts_with("HostKeyAlias")));
        let secondary = ssh_args(&machine, 1, "true");
        assert!(secondary.contains(&"HostKeyAlias=10.0.0.6".to_string()));
        assert_eq!(
            &secondary[secondary.len() - 2..],
            ["gpu-06.example.ts.net", "true"]
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
