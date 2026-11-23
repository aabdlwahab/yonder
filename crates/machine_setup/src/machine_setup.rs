//! Puts the `yonder` program on machines and gives them jobs.
//!
//! Every machine runs the same program (see `crates/yonder_headless`). This
//! crate is what the app uses to look after it: report what a machine has,
//! install or update the program, and make a machine the hub. The work is done
//! with small POSIX shell scripts run over SSH, or locally for this computer,
//! so machines need nothing installed beforehand.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncWriteExt as _;
use settings::SshConnection;
use util::command::{Stdio, new_command};

/// The port the hub listens on.
pub const HUB_PORT: u16 = 7766;
/// The hub's systemd user unit on Linux.
pub const HUB_UNIT: &str = "yonder-hub";
/// The hub's launchd agent on macOS.
pub const HUB_LAUNCHD_LABEL: &str = "io.github.aabdlwahab.Yonder.hub";

/// Where a job runs: this computer, or a machine reached over SSH.
#[derive(Clone, Debug)]
pub enum Target {
    ThisComputer,
    Ssh(SshConnection),
}

impl Target {
    pub fn name(&self) -> String {
        match self {
            Target::ThisComputer => "this computer".to_string(),
            Target::Ssh(connection) => connection
                .nickname
                .clone()
                .unwrap_or_else(|| connection.host.clone()),
        }
    }
}

/// Where [`install`] puts the program on a machine.
pub const INSTALLED_PROGRAM: &str = "~/.local/bin/yonder";

/// A path for a POSIX shell: a leading `~/` becomes `"$HOME"/`, the rest is
/// quoted.
pub fn shell_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None => shell_quote(path),
    }
}

/// Single-quotes `text` for a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Arguments for `ssh` that run `command` on one address of `machine`.
/// Address 0 is the primary; later ones are the secondary addresses, checked
/// against the primary's host key.
pub fn ssh_args(machine: &SshConnection, address: usize, command: &str) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=5".to_string(),
    ];
    args.extend(machine.args.iter().cloned());
    let (host, port) = if address == 0 {
        (machine.host.clone(), machine.port)
    } else {
        let secondary = &machine.secondary_hosts[address - 1];
        args.extend(["-o".to_string(), format!("HostKeyAlias={}", machine.host)]);
        (secondary.host.clone(), secondary.port.or(machine.port))
    };
    if let Some(port) = port {
        args.extend(["-p".to_string(), port.to_string()]);
    }
    if let Some(user) = &machine.username {
        args.extend(["-l".to_string(), user.clone()]);
    }
    args.extend(["--".to_string(), host, command.to_string()]);
    args
}

/// Runs `script` with `sh` on `target`, feeding it `input`, and returns what
/// it printed. Over SSH, each address is tried in turn until one connects.
pub async fn run_script(target: &Target, script: &str, input: Option<&[u8]>) -> Result<String> {
    let machine = match target {
        Target::ThisComputer => {
            let mut command = new_command("sh");
            command.args(["-c", script]);
            return run(command, input).await;
        }
        Target::Ssh(machine) => machine,
    };
    let remote_command = format!("sh -c {}", shell_quote(script));
    let mut last_error = anyhow!("No address of {} answered.", target.name());
    for address in 0..=machine.secondary_hosts.len() {
        let mut command = new_command("ssh");
        command.args(ssh_args(machine, address, &remote_command));
        match run(command, input).await {
            Ok(output) => return Ok(output),
            // ssh exits 255 when it can't connect; then the next address may.
            Err(error) if error.downcast_ref::<ConnectFailed>().is_some() => last_error = error,
            Err(error) => return Err(error),
        }
    }
    Err(last_error)
}

#[derive(Debug)]
struct ConnectFailed(String);

impl std::fmt::Display for ConnectFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConnectFailed {}

async fn run(mut command: util::command::Command, input: Option<&[u8]>) -> Result<String> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("starting the command")?;
    // When ssh can't connect it exits before reading its input, so a failed
    // write is judged by the exit status below: 255 means try the next address.
    let mut write_error = None;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().context("no stdin")?;
        if let Err(error) = stdin.write_all(input).await {
            write_error = Some(error);
        }
        stdin.close().await.ok();
    }
    let output = child.output().await.context("waiting for the command")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        if let Some(error) = write_error {
            return Err(error).context("sending the program");
        }
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Scripts report what went wrong as an `error=` line.
    let message = parse_lines(&stdout)
        .find(|(key, _)| *key == "error")
        .map(|(_, value)| value.to_string())
        .or_else(|| {
            stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
        })
        .unwrap_or_else(|| format!("exited with {}", output.status));
    if output.status.code() == Some(255) {
        return Err(ConnectFailed(message).into());
    }
    Err(anyhow!(message))
}

fn parse_lines(output: &str) -> impl Iterator<Item = (&str, &str)> {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
}

/// The hub on a machine, as its service manager sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HubService {
    #[default]
    None,
    Running,
    Stopped,
}

/// What a machine has, from [`status_script`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// `linux` or `macos`.
    pub os: String,
    /// `x86_64` or `aarch64`.
    pub arch: String,
    /// What `yonder server version` prints for the installed program.
    pub version: Option<String>,
    pub hub: HubService,
    /// The machine's Tailscale address, when it's on a tailnet.
    pub tailscale: Option<String>,
}

impl Report {
    pub fn platform(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
}

/// A script that reports what a machine has, one `key=value` per line.
pub fn status_script() -> String {
    format!(
        r#"os=$(uname -s); arch=$(uname -m)
echo "os=$os"; echo "arch=$arch"
y="$HOME/.local/bin/yonder"
if [ -x "$y" ]; then echo "version=$("$y" server version 2>/dev/null | head -n 1)"; fi
case "$os" in
Linux)
  if command -v systemctl >/dev/null 2>&1; then
    if systemctl --user is-active --quiet {HUB_UNIT} 2>/dev/null; then echo hub=running
    elif systemctl --user cat {HUB_UNIT} >/dev/null 2>&1; then echo hub=stopped; fi
  fi ;;
Darwin)
  state=$(launchctl print "gui/$(id -u)/{HUB_LAUNCHD_LABEL}" 2>/dev/null | sed -n 's/^[[:space:]]*state = //p' | head -n 1)
  if [ "$state" = running ]; then echo hub=running
  elif [ -n "$state" ]; then echo hub=stopped; fi ;;
esac
for t in tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do
  ip=$("$t" ip -4 2>/dev/null | head -n 1)
  if [ -n "$ip" ]; then echo "tailscale=$ip"; break; fi
done
"#
    )
}

pub fn parse_report(output: &str) -> Report {
    let mut report = Report::default();
    for (key, value) in parse_lines(output) {
        match key {
            "os" => {
                report.os = match value {
                    "Linux" => "linux".to_string(),
                    "Darwin" => "macos".to_string(),
                    other => other.to_lowercase(),
                }
            }
            "arch" => {
                report.arch = match value {
                    "arm64" => "aarch64".to_string(),
                    "amd64" => "x86_64".to_string(),
                    other => other.to_string(),
                }
            }
            "version" if !value.is_empty() => report.version = Some(value.to_string()),
            "hub" => {
                report.hub = match value {
                    "running" => HubService::Running,
                    "stopped" => HubService::Stopped,
                    _ => HubService::None,
                }
            }
            "tailscale" if !value.is_empty() => report.tailscale = Some(value.to_string()),
            _ => {}
        }
    }
    report
}

pub async fn check(target: &Target) -> Result<Report> {
    let output = run_script(target, &status_script(), None).await?;
    let report = parse_report(&output);
    if report.os.is_empty() {
        bail!("{} didn't say what it is.", target.name());
    }
    Ok(report)
}

/// A copy of the `yonder` program this app carries for one platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub path: PathBuf,
    pub compressed: bool,
    /// What the program prints for `yonder server version`, when known.
    pub version: Option<String>,
}

/// The program for `platform` (`linux-x86_64`, `macos-aarch64` …) that this
/// app can install, if it has one: the copy inside the app bundle, else one
/// left in `remote_servers/yonder/<platform>/` of Yonder's data folder. A Mac
/// of this Mac's kind gets this app's own binary.
pub fn carried_program(platform: &str) -> Option<Program> {
    let exe = std::env::current_exe().ok()?;
    let bundle = exe
        .parent()
        .and_then(Path::parent)
        .map(|contents| contents.join("Resources").join("machines").join(platform));
    let data = paths::remote_servers_dir().join("yonder").join(platform);
    for directory in bundle.into_iter().chain([data]) {
        let path = directory.join("yonder.gz");
        if path.is_file() {
            let version = std::fs::read_to_string(directory.join("yonder.version"))
                .ok()
                .map(|version| version.trim().to_string())
                .filter(|version| !version.is_empty());
            return Some(Program {
                path,
                compressed: true,
                version,
            });
        }
    }
    if platform == this_platform() {
        return Some(Program {
            path: exe,
            compressed: false,
            version: None,
        });
    }
    None
}

pub fn this_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A script that reads the program on stdin and installs it as
/// `~/.local/bin/yonder`, with `yonder-hub` as a link to it.
pub fn install_script(compressed: bool) -> String {
    let unpack = if compressed { "gzip -dc" } else { "cat" };
    format!(
        r#"set -e
bin="$HOME/.local/bin"; mkdir -p "$bin"
new="$bin/.yonder.new.$$"
{unpack} > "$new"
chmod 755 "$new"
if ! "$new" server version >/dev/null 2>&1; then rm -f "$new"; echo "error=The program doesn't run on this machine."; exit 1; fi
mv -f "$new" "$bin/yonder"
ln -sfn yonder "$bin/yonder-hub"
echo "version=$("$bin/yonder" server version | head -n 1)"
"#
    )
}

/// Installs `program` on `target` and returns the installed version.
pub async fn install(target: &Target, program: &Program) -> Result<String> {
    let bytes = std::fs::read(&program.path)
        .with_context(|| format!("reading {}", program.path.display()))?;
    let output = run_script(target, &install_script(program.compressed), Some(&bytes)).await?;
    parse_lines(&output)
        .find(|(key, _)| *key == "version")
        .map(|(_, version)| version.to_string())
        .context("the machine didn't report the installed version")
}

/// A script that restarts the hub once none of its threads is running, so an
/// update doesn't cut a turn short. `program` is the `yonder` program there
/// and `address` is where the hub listens.
pub fn restart_when_idle_script(os: &str, program: &str, address: &str) -> String {
    let restart = if os == "macos" {
        format!("launchctl kickstart -k \"gui/$(id -u)/{HUB_LAUNCHD_LABEL}\"")
    } else {
        format!("systemctl --user restart {HUB_UNIT}")
    };
    let wait = format!(
        "while {} hub --hub {} list 2>/dev/null | grep -E '^[0-9a-f]{{8}} ' | grep -qvw idle; do sleep 30; done; {restart}",
        shell_path(program),
        shell_quote(address)
    );
    format!(
        "nohup sh -c {} >/dev/null 2>&1 &\necho restart=scheduled\n",
        shell_quote(&wait)
    )
}

/// What the app needs to use a hub it just set up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewHub {
    /// `host:port` the app connects to.
    pub address: String,
    pub token: String,
}

/// Where a new hub should listen, and the address the app should use for it.
/// On a tailnet the hub listens only on its Tailscale address; this computer's
/// own hub without Tailscale stays on loopback; anything else listens on all
/// addresses (the token still guards it) and is reached by its SSH host.
pub fn hub_listen(target: &Target, report: &Report) -> (String, String) {
    if let Some(ip) = &report.tailscale {
        let address = format!("{ip}:{HUB_PORT}");
        return (address.clone(), address);
    }
    match target {
        Target::ThisComputer => {
            let address = format!("127.0.0.1:{HUB_PORT}");
            (address.clone(), address)
        }
        Target::Ssh(machine) => (
            format!("0.0.0.0:{HUB_PORT}"),
            format!("{}:{HUB_PORT}", machine.host),
        ),
    }
}

/// A script that runs the hub as a service on `os`, starting at boot (Linux)
/// or login (macOS), and prints its token once it's up. A hub that is already
/// running keeps running; only its service definition is written.
pub fn make_hub_script(os: &str, program: &str, listen: &str) -> String {
    let token_file = if os == "macos" {
        "$HOME/Library/Application Support/Yonder/hub/token"
    } else {
        "${XDG_DATA_HOME:-$HOME/.local/share}/yonder/hub/token"
    };
    let service = if os == "macos" {
        // caffeinate keeps the Mac from idle sleep while the hub runs, so
        // threads keep going; a closed lid still sleeps.
        format!(
            r#"plist="$HOME/Library/LaunchAgents/{HUB_LAUNCHD_LABEL}.plist"
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/Yonder"
cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{HUB_LAUNCHD_LABEL}</string>
<key>ProgramArguments</key><array>
<string>/usr/bin/caffeinate</string><string>-i</string>
<string>{program}</string><string>hub</string><string>serve</string><string>--listen</string><string>{listen}</string>
</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>StandardErrorPath</key><string>$HOME/Library/Logs/Yonder/hub.log</string>
</dict></plist>
EOF
if ! launchctl print "gui/$(id -u)/{HUB_LAUNCHD_LABEL}" >/dev/null 2>&1; then
  launchctl bootstrap "gui/$(id -u)" "$plist"
fi
running() {{ launchctl print "gui/$(id -u)/{HUB_LAUNCHD_LABEL}" 2>/dev/null | grep -q 'state = running'; }}
"#
        )
    } else {
        format!(
            r#"command -v systemctl >/dev/null 2>&1 || {{ echo "error=This machine has no systemd to run the hub."; exit 1; }}
unit="$HOME/.config/systemd/user/{HUB_UNIT}.service"
mkdir -p "$(dirname "$unit")"
cat > "$unit" <<'EOF'
# Written by Yonder (Settings > Machines > Make Hub).
[Unit]
Description=Yonder hub: runs agent threads for your other computers and phone
After=network-online.target tailscaled.service
Wants=network-online.target

[Service]
ExecStart={program} hub serve --listen {listen}
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=yonder_hub=info

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now {HUB_UNIT} >/dev/null 2>&1 || {{ echo "error=systemd couldn't start the hub."; exit 1; }}
# Keeps the hub running after you log out; some systems only let an admin
# turn this on.
loginctl enable-linger "$(id -un)" >/dev/null 2>&1 || echo "linger=off"
running() {{ systemctl --user is-active --quiet {HUB_UNIT}; }}
"#
        )
    };
    format!(
        r#"set -e
{service}
token="{token_file}"
# The service manager starts the hub in the background; give it a moment to
# come up and write its token (a hub set up before already has one).
i=0
until running && [ -s "$token" ]; do
  [ $i -ge 30 ] && break
  sleep 1; i=$((i + 1))
done
if ! running; then echo "error=The hub stopped right after starting."; exit 1; fi
if [ ! -s "$token" ]; then echo "error=The hub started but wrote no token."; exit 1; fi
echo "token=$(cat "$token")"
"#
    )
}

/// Makes `target` the hub and returns how to reach it. `program` is the path
/// of the `yonder` program there.
pub async fn make_hub(target: &Target, report: &Report, program: &str) -> Result<NewHub> {
    let (listen, address) = hub_listen(target, report);
    let output = run_script(target, &make_hub_script(&report.os, program, &listen), None).await?;
    let token = parse_lines(&output)
        .find(|(key, _)| *key == "token")
        .map(|(_, token)| token.to_string())
        .filter(|token| !token.is_empty())
        .context("the hub didn't report its token")?;
    Ok(NewHub { address, token })
}

/// The settings a hub needs, merged into what its settings file already has:
/// the machines it runs threads on (every machine but the hub itself), and the
/// app's agents when the hub has none of its own.
pub fn hub_settings(
    existing: &str,
    hub: &SshConnection,
    machines: &[SshConnection],
    agent_servers: Option<&serde_json::Value>,
) -> Result<String> {
    let mut settings: serde_json::Map<String, serde_json::Value> = if existing.trim().is_empty() {
        serde_json::Map::new()
    } else {
        settings_json::parse_json_with_comments(existing)
            .context("the hub's settings file isn't valid")?
    };
    let hub_hosts = std::iter::once(hub.host.as_str())
        .chain(hub.secondary_hosts.iter().map(|host| host.host.as_str()))
        .collect::<Vec<_>>();
    let others = machines
        .iter()
        .filter(|machine| !hub_hosts.contains(&machine.host.as_str()))
        .map(|machine| {
            let mut value = serde_json::to_value(machine)?;
            if let Some(object) = value.as_object_mut() {
                // Projects are this app's history; the hub keeps its own.
                object.remove("projects");
                object.retain(|_, value| !value.is_null());
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()?;
    settings.insert(
        "ssh_connections".to_string(),
        serde_json::Value::Array(others),
    );
    if !settings.contains_key("agent_servers")
        && let Some(agent_servers) = agent_servers
    {
        settings.insert("agent_servers".to_string(), agent_servers.clone());
    }
    let body = serde_json::to_string_pretty(&settings)?;
    Ok(format!(
        "// Settings for Yonder on this machine. Yonder keeps \"ssh_connections\" in step\n// with the machines in its settings.\n{body}\n"
    ))
}

/// Gives a hub on another machine the app's machines and agents, so it can run
/// threads on them.
pub async fn sync_hub_machines(
    hub: &SshConnection,
    machines: &[SshConnection],
    agent_servers: Option<&serde_json::Value>,
) -> Result<()> {
    let target = Target::Ssh(hub.clone());
    let path = "${XDG_CONFIG_HOME:-$HOME/.config}/yonder/settings.json";
    let existing = run_script(
        &target,
        &format!("cat \"{path}\" 2>/dev/null || true"),
        None,
    )
    .await?;
    let updated = hub_settings(&existing, hub, machines, agent_servers)?;
    run_script(
        &target,
        &format!(
            "set -e; f=\"{path}\"; mkdir -p \"$(dirname \"$f\")\"; cat > \"$f.new\"; mv -f \"$f.new\" \"$f\""
        ),
        Some(updated.as_bytes()),
    )
    .await?;
    Ok(())
}

/// A script that stops the hub and keeps it from starting again. Its threads
/// and conversations stay on disk.
pub fn stop_hub_script(os: &str) -> String {
    if os == "macos" {
        format!(
            "launchctl bootout \"gui/$(id -u)/{HUB_LAUNCHD_LABEL}\" 2>/dev/null || true\nrm -f \"$HOME/Library/LaunchAgents/{HUB_LAUNCHD_LABEL}.plist\"\necho stopped=1\n"
        )
    } else {
        format!(
            "systemctl --user disable --now {HUB_UNIT} >/dev/null 2>&1 || true\necho stopped=1\n"
        )
    }
}

pub async fn stop_hub(target: &Target, os: &str) -> Result<()> {
    run_script(target, &stop_hub_script(os), None).await?;
    Ok(())
}

/// Restarts the hub on `target` once it's idle, so it picks up a new program
/// or machine list.
pub async fn restart_hub_when_idle(
    target: &Target,
    os: &str,
    program: &str,
    address: &str,
) -> Result<()> {
    run_script(
        target,
        &restart_when_idle_script(os, program, address),
        None,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::SshSecondaryHost;

    fn machine(host: &str, nickname: &str) -> SshConnection {
        SshConnection {
            host: host.to_string(),
            nickname: Some(nickname.to_string()),
            username: Some("ayasser".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn reads_a_linux_report() {
        let report = parse_report(
            "os=Linux\narch=x86_64\nversion=48e0632757\nhub=running\ntailscale=100.80.18.21\n",
        );
        assert_eq!(
            report,
            Report {
                os: "linux".into(),
                arch: "x86_64".into(),
                version: Some("48e0632757".into()),
                hub: HubService::Running,
                tailscale: Some("100.80.18.21".into()),
            }
        );
        assert_eq!(report.platform(), "linux-x86_64");
    }

    #[test]
    fn reads_a_mac_without_yonder() {
        let report = parse_report("os=Darwin\narch=arm64\n");
        assert_eq!(report.platform(), "macos-aarch64");
        assert_eq!(report.version, None);
        assert_eq!(report.hub, HubService::None);
    }

    #[test]
    fn the_status_script_runs_here() {
        #[allow(
            clippy::disallowed_methods,
            reason = "synchronous test runs the script directly"
        )]
        let output = std::process::Command::new("sh")
            .args(["-c", &status_script()])
            .env("HOME", std::env::temp_dir())
            .output()
            .unwrap();
        assert!(output.status.success());
        let report = parse_report(&String::from_utf8_lossy(&output.stdout));
        assert!(!report.os.is_empty() && !report.arch.is_empty());
        assert_eq!(report.version, None);
    }

    #[test]
    fn the_install_script_installs_and_links() {
        let home = std::env::temp_dir().join(format!("machine-setup-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let program = "#!/bin/sh\n[ \"$1 $2\" = \"server version\" ] && echo abc123\n";
        #[allow(
            clippy::disallowed_methods,
            reason = "synchronous test runs the script directly"
        )]
        let mut child = std::process::Command::new("sh")
            .args(["-c", &install_script(false)])
            .env("HOME", &home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(program.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "version=abc123\n");
        let link = std::fs::read_link(home.join(".local/bin/yonder-hub")).unwrap();
        assert_eq!(link, Path::new("yonder"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_hub_listens_on_tailscale_when_it_can() {
        let gpu07 = Target::Ssh(machine("172.17.21.107", "gpu-07"));
        let mut report = parse_report("os=Linux\narch=x86_64\ntailscale=100.64.0.7\n");
        assert_eq!(
            hub_listen(&gpu07, &report),
            ("100.64.0.7:7766".into(), "100.64.0.7:7766".into())
        );
        report.tailscale = None;
        assert_eq!(
            hub_listen(&gpu07, &report),
            ("0.0.0.0:7766".into(), "172.17.21.107:7766".into())
        );
        assert_eq!(
            hub_listen(&Target::ThisComputer, &report),
            ("127.0.0.1:7766".into(), "127.0.0.1:7766".into())
        );
    }

    #[test]
    fn hub_settings_list_the_other_machines_and_keep_the_rest() {
        let mut gpu06 = machine("172.17.21.106", "gpu-06");
        gpu06.secondary_hosts = vec![SshSecondaryHost {
            host: "gpu-06.tail5661e6.ts.net".to_string(),
            ..Default::default()
        }];
        let gpu07 = machine("172.17.21.107", "gpu-07");
        let existing = r#"// Settings for Yonder on this machine.
{
  "agent_servers": { "codex-acp": { "type": "registry" } },
  "ssh_connections": [{ "host": "old" }],
  "theme": "kept"
}"#;
        let app_agents = serde_json::json!({ "claude-acp": { "type": "registry" } });
        let written =
            hub_settings(existing, &gpu06, &[gpu06.clone(), gpu07], Some(&app_agents)).unwrap();
        let parsed: serde_json::Value = settings_json::parse_json_with_comments(&written).unwrap();
        assert_eq!(parsed["theme"], "kept");
        assert_eq!(
            parsed["agent_servers"],
            serde_json::json!({ "codex-acp": { "type": "registry" } })
        );
        let machines = parsed["ssh_connections"].as_array().unwrap();
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0]["host"], "172.17.21.107");
        assert_eq!(machines[0]["nickname"], "gpu-07");
        assert!(machines[0].get("projects").is_none());
    }

    #[test]
    fn a_new_hub_gets_the_apps_agents() {
        let gpu07 = machine("172.17.21.107", "gpu-07");
        let app_agents = serde_json::json!({ "claude-acp": { "type": "registry" } });
        let written =
            hub_settings("", &gpu07, std::slice::from_ref(&gpu07), Some(&app_agents)).unwrap();
        let parsed: serde_json::Value = settings_json::parse_json_with_comments(&written).unwrap();
        assert_eq!(parsed["agent_servers"], app_agents);
        assert_eq!(parsed["ssh_connections"], serde_json::json!([]));
    }

    #[test]
    fn scripts_quote_the_hub_address() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        let script = restart_when_idle_script("linux", INSTALLED_PROGRAM, "1.2.3.4:7766; rm -rf ~");
        assert!(script.starts_with("nohup sh -c '"));
        assert!(!script.contains("7766; rm -rf ~ list"));
    }
}
