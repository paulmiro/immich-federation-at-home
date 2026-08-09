//! `immich-federation-at-home` — mirror a shared Immich album into a local album.
//!
//! This binary is a thin wrapper over the `immich_federation_at_home` library crate (see
//! `src/lib.rs`), which is what lets integration tests exercise the real modules. Config
//! parsing is wired up here; the real startup sequence (validation, logging init, startup
//! checks) and the sync loop are implemented in later steps of `PLAN.md`.

use clap::Parser;
use immich_federation_at_home::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _config = Config::parse();
    Ok(())
}
