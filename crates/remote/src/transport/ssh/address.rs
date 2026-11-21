//! Picks which of an SSH machine's addresses to connect to.
//!
//! A machine has a primary `host` and an ordered list of `secondary_hosts`.
//! Every address is checked at once by opening its SSH port. The primary gets
//! a short head start; after that the first address in order that answered
//! wins. A check never decides alone: if the chosen address fails to connect,
//! the others are tried in order.

use super::SshConnectionOptions;
use futures::{FutureExt as _, StreamExt as _, select_biased, stream::FuturesUnordered};
use gpui::BackgroundExecutor;
use std::time::{Duration, Instant};
use util::command::Stdio;

/// How long the primary address may take to answer before a secondary that
/// already answered is used instead.
const PRIMARY_HEAD_START: Duration = Duration::from_millis(800);
/// How long one address check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_SSH_PORT: u16 = 22;

/// One address of a machine, as options ready to pass to `ssh`.
#[derive(Debug, Clone)]
pub(super) struct Address {
    pub options: SshConnectionOptions,
    pub label: Option<String>,
}

impl Address {
    pub fn describe(&self) -> String {
        match &self.label {
            Some(label) => format!("{label} ({})", self.options.connection_string()),
            None => self.options.connection_string(),
        }
    }
}

/// The machine's addresses, primary first. Secondary addresses check the host
/// key against the primary's entry, so every address must reach the same
/// machine.
pub(super) fn addresses(options: &SshConnectionOptions) -> Vec<Address> {
    let mut primary = options.clone();
    primary.secondary_hosts = Vec::new();
    let host_key_alias = host_key_alias(options);
    let mut addresses = vec![Address {
        label: options.host_label.clone(),
        options: primary.clone(),
    }];
    for secondary in &options.secondary_hosts {
        let mut dial = primary.clone();
        dial.host = secondary.host.as_str().into();
        dial.port = secondary.port.or(options.port);
        dial.args
            .get_or_insert_with(Vec::new)
            .extend(["-o".to_string(), format!("HostKeyAlias={host_key_alias}")]);
        addresses.push(Address {
            label: secondary.label.clone(),
            options: dial,
        });
    }
    addresses
}

/// The name `ssh` records the primary's host key under in `known_hosts`.
fn host_key_alias(options: &SshConnectionOptions) -> String {
    match options.port {
        Some(port) if port != DEFAULT_SSH_PORT => {
            format!("[{}]:{port}", options.host.to_string())
        }
        _ => options.host.to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reach {
    Pending,
    Answered,
    NoAnswer,
    /// Reached through a jump host or proxy, so opening the port says nothing.
    Unchecked,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Wait,
    Use(usize),
    NoneAnswered,
}

/// Decides from the checks so far. Before the head start ends, an address is
/// used only once every address before it has failed. After it, the first
/// address in order that answered is used.
pub(super) fn decide(states: &[Reach], head_start_over: bool) -> Decision {
    for (index, state) in states.iter().enumerate() {
        match state {
            Reach::Answered | Reach::Unchecked => return Decision::Use(index),
            Reach::NoAnswer => {}
            Reach::Pending if head_start_over => {}
            Reach::Pending => return Decision::Wait,
        }
    }
    if states.iter().all(|state| *state == Reach::NoAnswer) {
        Decision::NoneAnswered
    } else {
        Decision::Wait
    }
}

/// The order to try connecting in: the chosen address, then the others that
/// didn't fail their check, in order. With no answers, every address in order.
pub(super) fn try_order(states: &[Reach], decision: &Decision) -> Vec<usize> {
    match decision {
        Decision::Use(first) => std::iter::once(*first)
            .chain(
                (0..states.len())
                    .filter(|index| index != first && states[*index] != Reach::NoAnswer),
            )
            .collect(),
        Decision::Wait | Decision::NoneAnswered => (0..states.len()).collect(),
    }
}

/// Checks every address at once and returns the order to try connecting in.
pub(super) async fn order_addresses(
    addresses: &[Address],
    executor: &BackgroundExecutor,
) -> Vec<usize> {
    let started = Instant::now();
    let mut states = vec![Reach::Pending; addresses.len()];
    let mut times = vec![None; addresses.len()];
    let mut checks = addresses
        .iter()
        .enumerate()
        .map(|(index, address)| {
            let executor = executor.clone();
            async move { (index, check(&address.options, &executor).await) }
        })
        .collect::<FuturesUnordered<_>>();
    let mut head_start = executor.timer(PRIMARY_HEAD_START).fuse();
    let mut head_start_over = false;

    let decision = loop {
        let decision = decide(&states, head_start_over);
        if decision != Decision::Wait {
            break decision;
        }
        select_biased! {
            result = checks.next() => match result {
                Some((index, reach)) => {
                    states[index] = reach;
                    times[index] = Some(started.elapsed());
                }
                None => break decide(&states, true),
            },
            _ = head_start => head_start_over = true,
        }
    };

    let report = addresses
        .iter()
        .zip(&states)
        .zip(&times)
        .map(|((address, state), time)| match time {
            Some(time) => format!("{} {state:?} in {time:?}", address.describe()),
            None => format!("{} {state:?}", address.describe()),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let order = try_order(&states, &decision);
    log::info!(
        "ssh address check for {} took {:?}: {report}; trying {}",
        addresses[0].options.connection_string(),
        started.elapsed(),
        order
            .iter()
            .map(|index| addresses[*index].describe())
            .collect::<Vec<_>>()
            .join(", then ")
    );
    order
}

async fn check(options: &SshConnectionOptions, executor: &BackgroundExecutor) -> Reach {
    let Some((hostname, port)) = resolve(options).await.flatten() else {
        return Reach::Unchecked;
    };
    let connect = smol::net::TcpStream::connect((hostname.as_str(), port)).fuse();
    let timeout = executor.timer(CHECK_TIMEOUT).fuse();
    futures::pin_mut!(connect, timeout);
    select_biased! {
        result = connect => if result.is_ok() { Reach::Answered } else { Reach::NoAnswer },
        _ = timeout => Reach::NoAnswer,
    }
}

/// Resolves the host and port `ssh` would connect to, from the user's SSH
/// config. `Some(None)` means the address goes through a proxy; `None` means
/// `ssh -G` failed.
async fn resolve(options: &SshConnectionOptions) -> Option<Option<(String, u16)>> {
    let output = util::command::new_command("ssh")
        .args(options.additional_args())
        .arg("-G")
        .args(options.ssh_destination_args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_resolved(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_resolved(ssh_g_output: &str) -> Option<(String, u16)> {
    let mut hostname = None;
    let mut port = DEFAULT_SSH_PORT;
    for line in ssh_g_output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => hostname = Some(value.to_string()),
            "port" => port = value.parse().ok()?,
            "proxyjump" | "proxycommand" if value != "none" && !value.is_empty() => return None,
            _ => {}
        }
    }
    Some((hostname?, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SshSecondaryHost;

    use Reach::*;

    #[test]
    fn primary_wins_inside_its_head_start() {
        assert_eq!(decide(&[Pending, Answered], false), Decision::Wait);
        assert_eq!(decide(&[Answered, Answered], false), Decision::Use(0));
        assert_eq!(decide(&[Answered, Pending], false), Decision::Use(0));
    }

    #[test]
    fn first_answer_in_order_wins_after_the_head_start() {
        assert_eq!(decide(&[Pending, Answered], true), Decision::Use(1));
        assert_eq!(
            decide(&[Pending, Pending, Answered], true),
            Decision::Use(2)
        );
        assert_eq!(
            decide(&[Pending, Answered, Answered], true),
            Decision::Use(1)
        );
        assert_eq!(decide(&[Pending, Pending], true), Decision::Wait);
    }

    #[test]
    fn a_failed_primary_hands_over_without_waiting() {
        assert_eq!(decide(&[NoAnswer, Answered], false), Decision::Use(1));
        assert_eq!(
            decide(&[NoAnswer, Pending, Answered], false),
            Decision::Wait
        );
        assert_eq!(decide(&[NoAnswer, NoAnswer], false), Decision::NoneAnswered);
    }

    #[test]
    fn proxied_addresses_count_as_answered_in_their_place() {
        assert_eq!(decide(&[Unchecked, Answered], false), Decision::Use(0));
        assert_eq!(decide(&[Pending, Unchecked], true), Decision::Use(1));
    }

    #[test]
    fn try_order_puts_the_choice_first_and_skips_failed_checks() {
        assert_eq!(
            try_order(&[NoAnswer, Answered, Pending], &Decision::Use(1)),
            vec![1, 2]
        );
        assert_eq!(
            try_order(&[Pending, Answered], &Decision::Use(1)),
            vec![1, 0]
        );
        assert_eq!(
            try_order(&[NoAnswer, NoAnswer], &Decision::NoneAnswered),
            vec![0, 1]
        );
    }

    #[test]
    fn secondary_addresses_reuse_the_primary_host_key() {
        let options = SshConnectionOptions {
            host: "172.17.21.106".into(),
            username: Some("ayasser".into()),
            host_label: Some("Office".into()),
            secondary_hosts: vec![SshSecondaryHost {
                host: "gpu-06.example.ts.net".into(),
                port: None,
                label: Some("Tailscale".into()),
            }],
            ..Default::default()
        };
        let addresses = addresses(&options);
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].options.host.to_string(), "172.17.21.106");
        assert_eq!(addresses[0].label.as_deref(), Some("Office"));
        assert!(addresses[0].options.secondary_hosts.is_empty());
        assert_eq!(addresses[0].options.args, None);
        assert_eq!(
            addresses[1].options.host.to_string(),
            "gpu-06.example.ts.net"
        );
        assert_eq!(addresses[1].options.username.as_deref(), Some("ayasser"));
        assert_eq!(
            addresses[1].options.args,
            Some(vec![
                "-o".to_string(),
                "HostKeyAlias=172.17.21.106".to_string()
            ])
        );
    }

    #[test]
    fn host_key_alias_includes_a_non_default_port() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            port: Some(2222),
            secondary_hosts: vec![SshSecondaryHost {
                host: "backup.example.com".into(),
                port: Some(22),
                label: None,
            }],
            ..Default::default()
        };
        let addresses = addresses(&options);
        assert_eq!(addresses[1].options.port, Some(22));
        assert_eq!(
            addresses[1]
                .options
                .args
                .as_deref()
                .unwrap()
                .last()
                .unwrap(),
            "HostKeyAlias=[example.com]:2222"
        );
    }

    #[test]
    fn parses_ssh_g_output() {
        assert_eq!(
            parse_resolved("user ayasser\nhostname 100.80.18.21\nport 22\nproxycommand none\n"),
            Some(("100.80.18.21".to_string(), 22))
        );
        assert_eq!(
            parse_resolved("hostname box\nport 2200\n"),
            Some(("box".to_string(), 2200))
        );
        assert_eq!(parse_resolved("hostname box\nproxyjump bastion\n"), None);
    }
}
