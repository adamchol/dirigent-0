//! The player's binary, as the cluster's VMs run it (see the library, `dirigent_player`).

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dirigent_player::run(dirigent_player::Args::parse()).await
}
