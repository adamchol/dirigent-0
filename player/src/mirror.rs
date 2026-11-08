//! The workspace, mirrored into the session's log (see `dirigent_sync`): committed as the player
//! starts and after each tool call that may have changed it, each commit's bundle published as
//! `FilesChanged`, so `dirigent sync` (or a player restoring the workspace) has its files from
//! the log. The repository is the player's, in its state, never the workspace; a player on a
//! new machine makes it again from the log.

use std::path::Path;
use std::time::Duration;

use dirigent_protocol::Event;
use dirigent_sync::{events, Bundles, Repo, Workspace};
use tracing::{info, warn};

/// How long a snapshot may take; then the change is in the next one.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Mirror {
    repo: Repo,
    workdir: &'static Path,
    files: Workspace,
    /// The last commit in the log, which the next is made on.
    last: Option<String>,
}

impl Mirror {
    /// The mirror of `workdir`, its repository in `dir`, having fetched what `files` (the log's
    /// `FilesChanged`, in order) has.
    pub async fn open(dir: &Path, workdir: &'static Path, files: &[Event]) -> anyhow::Result<Self> {
        let repo = Repo::open(dir).await?;
        let mut bundles = Bundles::default();
        let whole: Vec<_> = files.iter().filter_map(|e| bundles.push(e)).collect();
        let (last, error) = repo.replay(whole).await;
        if let Some(error) = error {
            warn!("the log's workspace stops at {last:?}: {error:#}");
        }
        Ok(Mirror {
            repo,
            workdir,
            files: Workspace::new(workdir),
            last,
        })
    }

    /// Make the workspace the last commit's, over what was restored from a checkpoint (which
    /// may be older): the changes since come back, and what was deleted since goes.
    pub async fn restore(&self) -> anyhow::Result<()> {
        let Some(last) = &self.last else {
            return Ok(());
        };
        self.repo.checkout(self.workdir, last, false).await?;
        info!(commit = last, "the workspace is the log's last commit");
        Ok(())
    }

    /// The workspace's changes since the last commit, as the events that publish them: none
    /// when nothing changed, or when it couldn't be committed (the next snapshot has it).
    pub async fn snapshot(&mut self, why: &str) -> Vec<Event> {
        let made = tokio::time::timeout(
            SNAPSHOT_TIMEOUT,
            self.repo
                .snapshot(&mut self.files, self.last.as_deref(), why),
        )
        .await;
        match made {
            Ok(Ok(Some(snapshot))) => {
                self.last = Some(snapshot.commit.clone());
                events(&snapshot)
            }
            Ok(Ok(None)) => Vec::new(),
            Ok(Err(e)) => {
                warn!("the workspace couldn't be mirrored: {e:#}");
                Vec::new()
            }
            Err(_) => {
                warn!(
                    "mirroring the workspace timed out after {}s",
                    SNAPSHOT_TIMEOUT.as_secs()
                );
                Vec::new()
            }
        }
    }
}
