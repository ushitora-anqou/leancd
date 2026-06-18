//! leancd — a minimal, low-memory Kubernetes continuous-delivery controller.

mod cli;
mod config;
mod drift;
mod error;
mod git_sync;
mod health;
mod hooks;
mod kube_util;
mod manifest;
mod metrics;
mod prune;
mod reconcile;
mod state;
mod version;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;
use kube::Client;

use crate::cli::{Cli, Command};
use crate::error::Result;
use opentelemetry::metrics::MeterProvider as _;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let builder = tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .with_filter_reloading();
    let reload_handle = builder.reload_handle();
    builder.init();

    // Reload the log filter from RUST_LOG on SIGHUP so operators can change
    // the log level (e.g. to `debug`) at runtime without a redeploy.
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to install SIGHUP handler; runtime log reload disabled"
                );
                return;
            }
        };
        loop {
            if sighup.recv().await.is_none() {
                break;
            }
            match reload_handle.reload(log_filter()) {
                Ok(()) => tracing::info!("log filter reloaded from RUST_LOG"),
                Err(e) => tracing::warn!(error = %e, "failed to reload log filter"),
            }
        }
    });
    #[cfg(not(unix))]
    let _ = reload_handle;

    let cli = Cli::parse();
    tracing::info!(
        version = %crate::version::VERSION,
        git_sha = %crate::version::GIT_SHA,
        "leancd"
    );
    match cli.command {
        Command::Controller(args) => run_controller(args.to_config()?).await?,
        Command::Sync(common) => run_sync(common.to_config()?).await?,
        Command::Status(args) => run_status(args.to_config()?).await?,
        Command::Health(common) => {
            let cfg = common.to_config()?;
            match health::run_health(&cfg).await {
                Ok(status) => {
                    tracing::info!(status = ?status, "health check");
                    std::process::exit(status.exit_code());
                }
                Err(e) => {
                    tracing::error!(error = %e, "health check failed");
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

/// Run as a long-lived controller: metrics server + polling reconciliation loop.
async fn run_controller(cfg: config::Config) -> Result<()> {
    let client = Client::try_default().await?;
    let provider = metrics::init_meter_provider()?;
    let meter = provider.meter("leancd");
    let metrics = Arc::new(metrics::Metrics::new(&meter));

    let shutdown_timeout = cfg.shutdown_timeout;
    let stop = Arc::new(AtomicBool::new(false));
    let recon = reconcile::Reconciler {
        client,
        cfg,
        metrics,
        stop: stop.clone(),
    };
    let mut recon_handle = tokio::spawn(async move {
        let _ = recon.run_loop().await;
    });

    tracing::info!("leancd controller started");
    shutdown_signal().await;
    tracing::info!("shutdown signal received, stopping");
    // Cooperative: let the in-flight pass finish, but fall back to aborting
    // after the grace period so a wedged pass cannot block Pod termination.
    stop.store(true, Ordering::Release);
    match tokio::time::timeout(shutdown_timeout, &mut recon_handle).await {
        Ok(_) => {}
        Err(_) => {
            tracing::warn!(
                timeout_secs = shutdown_timeout.as_secs(),
                "reconciliation did not finish in time; aborting task"
            );
            recon_handle.abort();
        }
    }
    if let Err(e) = provider.shutdown() {
        tracing::warn!(error = %e, "failed to flush metrics on shutdown");
    }
    Ok(())
}

/// Perform a single reconciliation pass.
async fn run_sync(cfg: config::Config) -> Result<()> {
    let client = Client::try_default().await?;
    let provider = metrics::init_meter_provider()?;
    let meter = provider.meter("leancd");
    let metrics = Arc::new(metrics::Metrics::new(&meter));
    let recon = reconcile::Reconciler {
        client,
        cfg,
        metrics,
        stop: Arc::new(AtomicBool::new(false)),
    };
    let res = recon.run_once().await;
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

/// Build the tracing `EnvFilter` from `RUST_LOG`, falling back to `info` when
/// unset or invalid. Called at startup and on each `SIGHUP` (runtime reload).
fn log_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_filter_does_not_panic() {
        // The filter is built from RUST_LOG (defaulting to "info"); it must
        // construct successfully regardless of the environment.
        let _ = log_filter();
    }
}
