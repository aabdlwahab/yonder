use agent_client_protocol::v2::schema::SessionId;
use anyhow::{Context as _, Result, anyhow, ensure};
use gpui::{App, Entity, Task};
use project::{AgentId, AgentServerStore, Project, agent_server_store::registry_id_for_agent};
use remote::{CommandTemplate, Interactive, RemoteOs};

use crate::CLAUDE_AGENT_ID;

// The path component above the JSONL is Claude's own project key. Discover it
// from the source file so long, symlinked, and explicitly named paths work.
const COPY_SESSION: &str = r#"
set -eu
umask 077
fail() { printf '%s\n' "$1" >&2; exit 1; }
config_dir() {
    case "$1" in
        '') printf '%s/.claude\n' "$2" ;;
        '~/'*) printf '%s/%s\n' "$2" "$(printf '%s' "$1" | cut -c3-)" ;;
        /*) printf '%s\n' "$1" ;;
        *) printf '%s/%s\n' "$PWD" "$1" ;;
    esac
}
source_root=$(config_dir "$1" "${2:-$HOME}")
target_root=$(config_dir "$3" "${4:-$HOME}")
session=$5
target_key=$6
source_file=''
for candidate in "$source_root/projects/"*/"$session.jsonl"; do
    if [ -f "$candidate" ] && [ ! -L "$candidate" ]; then
        [ -z "$source_file" ] || fail 'This Claude session exists in more than one source project.'
        source_file=$candidate
    fi
done
[ -n "$source_file" ] && [ -s "$source_file" ] || fail 'The source Claude account has no saved history for this session.'
source_project=${source_file%/*}
case "$target_key" in
    '') target_key=${source_project##*/} ;;
    */*|.|..) fail 'The destination Claude project key is invalid.' ;;
esac
target_project="$target_root/projects/$target_key"
target_file="$target_project/$session.jsonl"
[ ! -L "$target_file" ] || fail 'The destination Claude session file is a symbolic link.'
if [ "$source_file" -ef "$target_file" ]; then exit 0; fi
mkdir -p "$target_project"
staging=$(mktemp -d "$target_project/.yonder-resume.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
cp -P "$source_file" "$staging/session.jsonl"
if [ -e "$target_file" ]; then
    [ -f "$target_file" ] || fail 'The destination Claude session is not a file.'
    cp -P "$target_file" "$staging/previous"
    length=$(wc -c < "$staging/previous" | tr -d '[:space:]')
    head -c "$length" "$staging/session.jsonl" > "$staging/prefix"
    cmp -s "$staging/prefix" "$staging/previous" || fail 'The destination Claude session has diverged. Its existing history was kept.'
fi
if [ -d "$source_project/$session" ] && [ ! -L "$source_project/$session" ] && [ ! -e "$target_project/$session" ] && [ ! -L "$target_project/$session" ]; then
    cp -RP "$source_project/$session" "$staging/sidecars"
    mv -n "$staging/sidecars" "$target_project/$session"
fi
if [ -f "$staging/previous" ]; then
    cmp -s "$staging/previous" "$target_file" || fail 'The destination Claude session changed during the copy.'
    if ! cmp -s "$staging/session.jsonl" "$target_file"; then
        mv -f "$staging/session.jsonl" "$target_file"
    fi
else
    mv -n "$staging/session.jsonl" "$target_file"
    [ ! -e "$staging/session.jsonl" ] || fail 'The destination Claude session appeared during the copy.'
fi
"#;

pub fn copy_claude_session(
    project: Entity<Project>,
    source: AgentId,
    target: AgentId,
    session: SessionId,
    cx: &App,
) -> Task<Result<()>> {
    let store = project.read(cx).agent_server_store().clone();
    cx.spawn(async move |cx| {
        let session =
            uuid::Uuid::parse_str(&session.0).context("Claude returned an invalid session ID")?;
        ensure!(
            source != target
                && [&source, &target]
                    .into_iter()
                    .all(|id| registry_id_for_agent(id.as_ref()) == CLAUDE_AGENT_ID),
            "Select another Claude account to resume this session"
        );

        let source_command = claude_command(&store, &source, cx).await?;
        let target_command = claude_command(&store, &target, cx).await?;
        let env_value = |command: &project::agent_server_store::AgentServerCommand, key: &str| {
            command
                .env
                .as_ref()
                .and_then(|env| env.get(key))
                .cloned()
                .unwrap_or_default()
        };
        let args = vec![
            "-c".to_string(),
            COPY_SESSION.to_string(),
            "yonder-resume".to_string(),
            env_value(&source_command, "CLAUDE_CONFIG_DIR"),
            env_value(&source_command, "HOME"),
            env_value(&target_command, "CLAUDE_CONFIG_DIR"),
            env_value(&target_command, "HOME"),
            session.to_string(),
            env_value(&target_command, "CLAUDE_CODE_PROJECT_DIR_NAME"),
        ];
        let command = project.read_with(cx, |project, cx| {
            let working_dir = project
                .default_path_list(cx)
                .ordered_paths()
                .next()
                .map(|p| p.to_string_lossy().into_owned());
            if let Some(client) = project.remote_client() {
                let client = client.read(cx);
                ensure!(
                    client.remote_platform().os != RemoteOs::Windows,
                    "Resuming across accounts requires a macOS or Linux Claude host"
                );
                Ok((
                    client.build_command(
                        Some("sh".into()),
                        &args,
                        &Default::default(),
                        working_dir,
                        None,
                        Interactive::No,
                    )?,
                    None,
                ))
            } else {
                ensure!(
                    project.is_local() && cfg!(unix),
                    "Resuming across accounts requires a macOS or Linux Claude host"
                );
                Ok((
                    CommandTemplate {
                        program: "/bin/sh".into(),
                        args,
                        env: Default::default(),
                    },
                    working_dir,
                ))
            }
        })?;
        let (command, working_dir) = command;
        let mut process = util::command::new_command(command.program);
        process.args(command.args).envs(command.env);
        if let Some(working_dir) = working_dir {
            process.current_dir(working_dir);
        }
        let output = process
            .output()
            .await
            .context("Could not copy the Claude session on its host")?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    })
}

fn claude_command(
    store: &Entity<AgentServerStore>,
    id: &AgentId,
    cx: &mut gpui::AsyncApp,
) -> Task<Result<project::agent_server_store::AgentServerCommand>> {
    store.update(cx, |store, cx| match store.get_external_agent(id) {
        Some(agent) => agent.get_command(Vec::new(), Default::default(), &mut cx.to_async()),
        None => Task::ready(Err(anyhow!(
            "Claude account {id} is unavailable on this machine"
        ))),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "synchronous test runs the copy script directly"
    )]
    fn native_session_copy_preserves_the_id_and_fast_forwards_without_overwriting_diverged_history()
    {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let source_profile = dir.path().join(".claude");
        let source_project = source_profile.join("projects/encoded-source-key");
        let target_profile = dir.path().join("work profile");
        let target_project = target_profile.join("projects/custom-project-key");
        let session = uuid::Uuid::new_v4().to_string();
        let filename = format!("{session}.jsonl");
        let source = source_project.join(&filename);
        let target = target_project.join(&filename);
        let original = "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
        fs::create_dir_all(source_project.join(&session)).unwrap();
        fs::write(
            source_project.join(&session).join("subagent.jsonl"),
            original,
        )
        .unwrap();
        fs::write(&source, original).unwrap();
        fs::write(source_profile.join("credentials"), "do not copy").unwrap();

        let copy = || {
            Command::new("/bin/sh")
                .args([
                    "-c",
                    COPY_SESSION,
                    "yonder-resume",
                    "",
                    home,
                    "~/work profile",
                    home,
                    &session,
                    "custom-project-key",
                ])
                .output()
                .unwrap()
        };
        let result = copy();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), original);
        assert_eq!(
            fs::read_to_string(target_project.join(&session).join("subagent.jsonl")).unwrap(),
            original
        );
        assert!(!target_profile.join("credentials").exists());
        let continued = format!("{original}{{\"type\":\"user\"}}\n");
        fs::write(&source, &continued).unwrap();
        assert!(copy().status.success());
        assert_eq!(fs::read_to_string(&target).unwrap(), continued);

        fs::write(&target, "divergent history\n").unwrap();
        assert!(!copy().status.success());
        assert_eq!(fs::read_to_string(target).unwrap(), "divergent history\n");
        assert_eq!(fs::read_to_string(source).unwrap(), continued);
    }
}
