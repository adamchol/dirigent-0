//! Deleting sessions: `DELETE /sessions/{id}` only marks an archived one, taking everyone's
//! access ([`Store::delete`]); [`prune`] then removes what it left, a step at a time, each safe
//! to repeat, until its row goes last: what its players left on the runner, its checkpoints
//! (kopia), and its events (ClickHouse). Kafka keeps its messages until the topic's retention;
//! chlive drops the snapshots that still have them. Control also runs the checkpoints'
//! repository's maintenance ([`maintain`]), which is what frees the storage of deleted
//! snapshots.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::OnceCell;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::leases::Store;
use crate::runner::Runner;

/// How long after its marking a session's events are deleted: those its users and player sent
/// before have reached ClickHouse by then (a later one waits for the next round).
const SETTLED: Duration = Duration::from_secs(60);
const KOPIA_TIMEOUT: Duration = Duration::from_secs(600);
/// Who control is to the checkpoints' repository: the owner of its maintenance.
const KOPIA_USER: &str = "control";
const KOPIA_HOST: &str = "dirigent";

#[derive(Args, Debug)]
pub struct PruneArgs {
    /// ClickHouse's HTTP interface, where the sessions' events are: without it, sessions can't
    /// be deleted.
    #[arg(long, env = "DIRIGENT_CLICKHOUSE_URL")]
    pub clickhouse_url: Option<String>,

    /// The ClickHouse user control deletes events as (`ALTER DELETE`, `ALTER UPDATE(_row_exists)`
    /// and `SELECT` on `dirigent.session_events`).
    #[arg(
        long,
        env = "DIRIGENT_CLICKHOUSE_USER",
        default_value = "dirigent_control"
    )]
    pub clickhouse_user: String,

    #[arg(long, env = "DIRIGENT_CLICKHOUSE_PASSWORD", default_value = "")]
    pub clickhouse_password: String,

    /// The checkpoints' kopia repository (`kopia repository status -t`), which deleted
    /// sessions' snapshots are deleted from and control maintains. Without it (or
    /// `--kopia-token-secret`), checkpoints are off.
    #[arg(long, env = "DIRIGENT_KOPIA_TOKEN")]
    pub kopia_token: Option<String>,

    /// The Secret in control's namespace whose `DIRIGENT_KOPIA_TOKEN` is the token, read when
    /// it's needed (it may come after control: Helm's hook makes it).
    #[arg(
        long,
        env = "DIRIGENT_KOPIA_TOKEN_SECRET",
        conflicts_with = "kopia_token"
    )]
    pub kopia_token_secret: Option<String>,

    /// Control's kopia state: its config, cache and logs.
    #[arg(long, env = "DIRIGENT_KOPIA_STATE")]
    pub kopia_state: Option<PathBuf>,
}

/// What removes deleted sessions' events and checkpoints.
pub struct Pruner {
    clickhouse: ClickHouse,
    kopia: Option<Kopia>,
    /// How long after its marking a session's events are deleted ([`SETTLED`]).
    pub settled: Duration,
}

impl Pruner {
    /// From `args`, unless there's no ClickHouse to delete from.
    pub fn new(args: &PruneArgs) -> Option<Self> {
        let url = args.clickhouse_url.clone()?;
        let token = match (&args.kopia_token, &args.kopia_token_secret) {
            (Some(t), _) => Some(Token::Given(t.clone())),
            (None, Some(s)) => Some(Token::Secret(s.clone())),
            (None, None) => None,
        };
        let state = args
            .kopia_state
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("dirigent-control-kopia"));
        Some(Pruner {
            clickhouse: ClickHouse {
                http: reqwest::Client::new(),
                url,
                user: args.clickhouse_user.clone(),
                password: args.clickhouse_password.clone(),
            },
            kopia: token.map(|token| Kopia {
                token,
                state,
                connected: OnceCell::new(),
            }),
            settled: SETTLED,
        })
    }
}

/// Every 10 seconds, remove what the sessions being deleted left, the oldest first; a step that
/// fails is tried again next time.
pub async fn prune(store: &'static Store, runner: Option<&'static dyn Runner>, pruner: &Pruner) {
    let mut every = tokio::time::interval(Duration::from_secs(10));
    loop {
        every.tick().await;
        let sessions = match store.deleting(pruner.settled).await {
            Ok(s) => s,
            Err(e) => {
                error!("listing the sessions being deleted: {e:#}");
                continue;
            }
        };
        for (session, settled) in sessions {
            match prune_one(store, runner, pruner, session, settled).await {
                Ok(true) => info!(session = %session, "session deleted"),
                Ok(false) => {}
                Err(e) => warn!(session = %session, "deleting the session: {e:#}"),
            }
        }
    }
}

/// One round for `session`: whether it's gone now.
async fn prune_one(
    store: &Store,
    runner: Option<&dyn Runner>,
    pruner: &Pruner,
    session: Uuid,
    settled: bool,
) -> anyhow::Result<bool> {
    if let Some(runner) = runner {
        runner.forget(session).await.context("on the runner")?;
    }
    if let Some(kopia) = &pruner.kopia {
        kopia.forget(session).await.context("its checkpoints")?;
    }
    if !settled {
        return Ok(false);
    }
    let left = pruner
        .clickhouse
        .forget(session)
        .await
        .context("its events")?;
    if left > 0 {
        info!(session = %session, left, "events came after the deletion; deleting again");
        return Ok(false);
    }
    store.deleted(session).await.context("its row")?;
    Ok(true)
}

/// Take the checkpoints' repository's maintenance, then run it: quick maintenance every hour,
/// full every day (which frees what deleted snapshots held, after kopia's safety margin).
pub async fn maintain(pruner: &'static Pruner) {
    let Some(kopia) = &pruner.kopia else {
        return;
    };
    let mut every = tokio::time::interval(Duration::from_secs(3600));
    let (mut owned, mut hours) = (false, 0u64);
    loop {
        every.tick().await;
        let full = hours % 24 == 0;
        hours += 1;
        if !owned {
            match kopia.own().await {
                Ok(()) => owned = true,
                Err(e) => {
                    warn!("taking the checkpoints' maintenance: {e:#}");
                    continue;
                }
            }
        }
        match kopia.maintain(full).await {
            Ok(()) => info!(full, "checkpoints maintained"),
            Err(e) => warn!(full, "maintaining the checkpoints: {e:#}"),
        }
    }
}

struct ClickHouse {
    http: reqwest::Client,
    url: String,
    user: String,
    password: String,
}

impl ClickHouse {
    /// Delete `session`'s events; how many are left (that came since).
    async fn forget(&self, session: Uuid) -> anyhow::Result<u64> {
        self.query(
            "DELETE FROM dirigent.session_events WHERE session_id = {s:UUID}",
            session,
        )
        .await?;
        let left = self
            .query(
                "SELECT count() FROM dirigent.session_events WHERE session_id = {s:UUID}",
                session,
            )
            .await?;
        left.trim()
            .parse()
            .with_context(|| format!("ClickHouse counted {left:?}"))
    }

    async fn query(&self, sql: &str, session: Uuid) -> anyhow::Result<String> {
        let mut url = reqwest::Url::parse(&self.url).context("ClickHouse's URL")?;
        // the user is control's own, whoever the URL names
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.query_pairs_mut()
            .append_pair("param_s", &session.to_string());
        let res = self
            .http
            .post(url)
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .body(sql.to_string())
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .context("reaching ClickHouse")?;
        let status = res.status();
        let body = res.text().await?;
        anyhow::ensure!(status.is_success(), "ClickHouse answered {status}: {body}");
        Ok(body)
    }
}

enum Token {
    Given(String),
    Secret(String),
}

struct Kopia {
    token: Token,
    state: PathBuf,
    connected: OnceCell<()>,
}

impl Kopia {
    /// Delete `session`'s snapshots (those tagged with it, of its source `<session>@dirigent`).
    async fn forget(&self, session: Uuid) -> anyhow::Result<()> {
        self.connect().await?;
        let mut list = self.kopia();
        list.args(["snapshot", "list", "--all", "--json"])
            .args(["--tags", &format!("session:{session}")]);
        let ids = snapshots(&run(list).await?, session)?;
        if ids.is_empty() {
            return Ok(());
        }
        let mut delete = self.kopia();
        delete.args(["snapshot", "delete", "--delete"]).args(&ids);
        run(delete).await?;
        info!(session = %session, n = ids.len(), "deleted its checkpoints");
        Ok(())
    }

    /// Control owns the repository's maintenance from now on.
    async fn own(&self) -> anyhow::Result<()> {
        self.connect().await?;
        let mut cmd = self.kopia();
        cmd.args([
            "maintenance",
            "set",
            &format!("--owner={KOPIA_USER}@{KOPIA_HOST}"),
        ]);
        run(cmd).await.map(drop)
    }

    async fn maintain(&self, full: bool) -> anyhow::Result<()> {
        self.connect().await?;
        let mut cmd = self.kopia();
        cmd.args([
            "maintenance",
            "run",
            if full { "--full" } else { "--no-full" },
        ]);
        run(cmd).await.map(drop)
    }

    /// Connected, once: a config left by an earlier run is replaced.
    async fn connect(&self) -> anyhow::Result<()> {
        self.connected
            .get_or_try_init(|| async {
                let token = match &self.token {
                    Token::Given(t) => t.clone(),
                    Token::Secret(name) => secret_token(name).await?,
                };
                std::fs::create_dir_all(&self.state).with_context(|| {
                    format!("making the kopia state in {}", self.state.display())
                })?;
                let _ = std::fs::remove_file(self.state.join("kopia.config"));
                let mut cmd = self.kopia();
                cmd.args(["repository", "connect", "from-config", "--token", &token])
                    .arg("--cache-directory")
                    .arg(self.state.join("cache"))
                    .args(["--override-username", KOPIA_USER])
                    .args(["--override-hostname", KOPIA_HOST]);
                run(cmd)
                    .await
                    .context("connecting to the checkpoints' repository")
                    .map(drop)
            })
            .await
            .map(drop)
    }

    fn kopia(&self) -> Command {
        let mut cmd = Command::new("kopia");
        cmd.env("KOPIA_CONFIG_PATH", self.state.join("kopia.config"))
            .env("KOPIA_LOG_DIR", self.state.join("logs"))
            .env("KOPIA_CHECK_FOR_UPDATES", "false")
            .kill_on_drop(true);
        cmd
    }
}

/// The token in the Secret `name`, in control's namespace.
async fn secret_token(name: &str) -> anyhow::Result<String> {
    use k8s_openapi::api::core::v1::Secret;
    let client = kube::Client::try_default().await?;
    let secrets: kube::Api<Secret> = kube::Api::default_namespaced(client);
    let secret = secrets
        .get(name)
        .await
        .with_context(|| format!("reading the Secret {name}"))?;
    let token = secret
        .data
        .and_then(|d| d.get("DIRIGENT_KOPIA_TOKEN").cloned())
        .with_context(|| format!("the Secret {name} has no DIRIGENT_KOPIA_TOKEN"))?;
    Ok(String::from_utf8(token.0)?)
}

/// Run `cmd`, for at most [`KOPIA_TIMEOUT`]: its output, or why it failed.
async fn run(mut cmd: Command) -> anyhow::Result<String> {
    let out = tokio::time::timeout(KOPIA_TIMEOUT, cmd.output())
        .await
        .context("kopia took too long")?
        .context("running kopia")?;
    anyhow::ensure!(
        out.status.success(),
        "kopia failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The ids of `session`'s snapshots in `kopia snapshot list --json`'s output: those of its
/// source (the tag filter's, checked again).
fn snapshots(json: &str, session: Uuid) -> anyhow::Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Manifest {
        id: String,
        source: Source,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Source {
        user_name: String,
    }
    let all: Vec<Manifest> =
        serde_json::from_str(json).with_context(|| format!("reading kopia's snapshots: {json}"))?;
    let session = session.to_string();
    Ok(all
        .into_iter()
        .filter(|m| m.source.user_name == session)
        .map(|m| m.id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sessions_snapshots_are_those_of_its_source() {
        let s = Uuid::new_v4();
        let json = format!(
            r#"[{{"id":"a1","source":{{"host":"dirigent","userName":"{s}","path":"/w"}}}},
               {{"id":"b2","source":{{"host":"dirigent","userName":"other","path":"/w"}}}},
               {{"id":"c3","source":{{"host":"dirigent","userName":"{s}","path":"/x"}}}}]"#
        );
        assert_eq!(snapshots(&json, s).unwrap(), ["a1", "c3"]);
        assert!(snapshots("[]", s).unwrap().is_empty());
        assert!(snapshots("nope", s).is_err());
    }
}
