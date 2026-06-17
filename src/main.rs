//! leancd — a minimal, low-memory Kubernetes continuous-delivery controller.

mod cli;
mod config;
mod drift;
mod error;
mod git_sync;
mod hooks;
mod kube_util;
mod manifest;
mod metrics;
mod prune;
mod reconcile;
mod state;

use std::sync::Arc;

use clap::Parser;
use kube::Client;

use crate::cli::{Cli, Command};
use crate::error::Result;
use opentelemetry::metrics::MeterProvider as _;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Controller(args) => run_controller(args.to_config()?).await?,
        Command::Sync { common, force } => run_sync(common.to_config()?, force).await?,
        Command::Status(args) => run_status(args.to_config()?).await?,
    }
    Ok(())
}

/// Run as a long-lived controller: metrics server + polling reconciliation loop.
async fn run_controller(cfg: config::Config) -> Result<()> {
    let client = Client::try_default().await?;
    let provider = metrics::init_meter_provider()?;
    let meter = provider.meter("leancd");
    let metrics = Arc::new(metrics::Metrics::new(&meter));

    let recon = reconcile::Reconciler {
        client,
        cfg,
        metrics,
    };
    let recon_handle = tokio::spawn(async move {
        let _ = recon.run_loop().await;
    });

    tracing::info!("leancd controller started");
    shutdown_signal().await;
    tracing::info!("shutdown signal received, stopping");
    recon_handle.abort();
    if let Err(e) = provider.shutdown() {
        tracing::warn!(error = %e, "failed to flush metrics on shutdown");
    }
    Ok(())
}

/// Perform a single reconciliation pass (optionally with force-conflict apply).
async fn run_sync(cfg: config::Config, force: bool) -> Result<()> {
    let client = Client::try_default().await?;
    let provider = metrics::init_meter_provider()?;
    let meter = provider.meter("leancd");
    let metrics = Arc::new(metrics::Metrics::new(&meter));
    let recon = reconcile::Reconciler {
        client,
        cfg,
        metrics,
    };
    let res = recon.run_once(force).await;
    if let Err(e) = provider.shutdown() {
        tracing::warn!(error = %e, "failed to flush metrics on shutdown");
    }
    res
}

/// Print the persisted sync state.
async fn run_status(cfg: config::Config) -> Result<()> {
    let client = Client::try_default().await?;
    match state::read(&client, &cfg).await? {
        None => println!("no sync state recorded yet"),
        Some(s) => {
            println!("leancd status ({}/{})", cfg.namespace, cfg.state_configmap);
            println!(
                "  last sha:   {}",
                s.last_sha.as_deref().unwrap_or("(none)")
            );
            println!("  sync count: {}", s.sync_count);
            println!("  managed:    {}", s.managed_count);
            println!("  drift:      {}", s.drift_count);
            if let Some(epoch) = s.last_sync_epoch {
                println!("  last sync:  unix {epoch}");
            }
            if let Some(e) = &s.last_error {
                println!("  last error: {e}");
            }
        }
    }
    Ok(())
}

/// Block until an interrupt or termination signal arrives.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
