//! The hub's saved state: the session list, each session's message log and
//! the access token, under `<data dir>/hub`.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::relay::SessionMeta;

pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    pub fn default_dir() -> PathBuf {
        paths::data_dir().join("hub")
    }

    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join("logs"))
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// Every saved session with its log.
    pub fn load(&self) -> Result<Vec<(SessionMeta, Vec<String>)>> {
        let index = self.dir.join("sessions.json");
        if !index.exists() {
            return Ok(Vec::new());
        }
        let sessions: Vec<SessionMeta> = serde_json::from_str(
            &fs::read_to_string(&index).with_context(|| format!("reading {}", index.display()))?,
        )
        .with_context(|| format!("parsing {}", index.display()))?;
        Ok(sessions
            .into_iter()
            .map(|meta| {
                let log = fs::read_to_string(self.log_path(&meta.id))
                    .map(|text| text.lines().map(str::to_string).collect())
                    .unwrap_or_default();
                (meta, log)
            })
            .collect())
    }

    pub fn save_sessions(&self, sessions: &[SessionMeta]) -> Result<()> {
        let index = self.dir.join("sessions.json");
        let temporary = self.dir.join("sessions.json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(sessions)?)?;
        fs::rename(&temporary, &index)?;
        Ok(())
    }

    pub fn append_log(&self, session: &str, line: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(session))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }

    /// The token clients need to connect, created on first use and readable
    /// only by this user.
    pub fn token(&self) -> Result<String> {
        read_or_create_token(&self.dir.join("token"))
    }

    pub fn dispatch_dir(&self) -> PathBuf {
        self.dir.join("dispatch")
    }

    fn log_path(&self, session: &str) -> PathBuf {
        let name: String = session
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join("logs").join(format!("{name}.jsonl"))
    }
}

pub fn read_or_create_token(path: &Path) -> Result<String> {
    if let Ok(token) = fs::read_to_string(path) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    use rand::Rng as _;
    let token: String = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(40)
        .map(char::from)
        .collect();
    fs::write(path, &token).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_and_logs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(dir.path()).unwrap();
        let meta = SessionMeta {
            id: "01a2-b3".into(),
            machine: "gpu-07".into(),
            agent: "codex-acp".into(),
            cwd: "/home/a/api".into(),
            title: Some("Fix login race".into()),
            state: "idle".into(),
            last_text: "Done.".into(),
            created_at: 1,
            updated_at: 2,
        };
        registry.save_sessions(std::slice::from_ref(&meta)).unwrap();
        registry.append_log("01a2-b3", r#"{"a":1}"#).unwrap();
        registry.append_log("01a2-b3", r#"{"b":2}"#).unwrap();

        let loaded = Registry::open(dir.path()).unwrap().load().unwrap();
        assert_eq!(
            loaded,
            vec![(
                meta,
                vec![r#"{"a":1}"#.to_string(), r#"{"b":2}"#.to_string()]
            )]
        );
    }

    #[test]
    fn token_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(dir.path()).unwrap();
        let token = registry.token().unwrap();
        assert_eq!(token.len(), 40);
        assert_eq!(registry.token().unwrap(), token);
    }
}
