//! Who the CLI is to dirigent, kept between runs: the server's API and how it's signed in to it,
//! in one file only its user may read (`$DIRIGENT_CREDENTIALS`, or
//! `~/.config/dirigent/credentials.json`), replaced whole when it changes.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    /// Control's HTTP API: the web's `/api`, or control itself.
    pub api: String,
    #[serde(flatten)]
    pub auth: Auth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Auth {
    /// Control's `declared` auth (dev): who the caller says it is.
    Declared { subject: String },
    /// Signed in with Google, as the CLI's (desktop) OAuth client: its ID token is the bearer,
    /// renewed with the refresh token.
    Oidc {
        client_id: String,
        client_secret: String,
        id_token: String,
        refresh_token: Option<String>,
    },
}

pub fn path() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("DIRIGENT_CREDENTIALS") {
        return Ok(p.into());
    }
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").context("no $HOME")?).join(".config"),
    };
    Ok(config.join("dirigent").join("credentials.json"))
}

/// `None`: not signed in.
pub fn load(path: &Path) -> anyhow::Result<Option<Credentials>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Written beside the file, readable by its owner only, then renamed over it.
pub fn save(path: &Path, creds: &Credentials) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .context("the credentials' path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    let tmp = path.with_extension("json.new");
    let mut file = open_private(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(creds)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn remove(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(unix)]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_through_a_file_only_its_owner_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("credentials.json");
        assert_eq!(load(&path).unwrap(), None);
        let creds = Credentials {
            api: "https://d.example/api".into(),
            auth: Auth::Oidc {
                client_id: "c".into(),
                client_secret: "s".into(),
                id_token: "i".into(),
                refresh_token: Some("r".into()),
            },
        };
        save(&path, &creds).unwrap();
        assert_eq!(load(&path).unwrap(), Some(creds));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let declared = Credentials {
            api: "http://127.0.0.1:8090".into(),
            auth: Auth::Declared {
                subject: "user:dev".into(),
            },
        };
        save(&path, &declared).unwrap();
        assert_eq!(load(&path).unwrap(), Some(declared));
        remove(&path).unwrap();
        remove(&path).unwrap();
        assert_eq!(load(&path).unwrap(), None);
    }
}
