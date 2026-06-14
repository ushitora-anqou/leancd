//! Prometheus metrics exposed at `/metrics` over a minimal HTTP listener.
//!
//! Only pull-based scraping is supported (no push queue), keeping memory flat.

use std::sync::Arc;

use prometheus::{Encoder, IntCounter, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::error::{Error, Result};

/// Container for all leancd metrics, registered on a single [`Registry`].
#[derive(Debug)]
pub struct Metrics {
    pub registry: Registry,
    pub sync_total: IntCounter,
    pub sync_errors: IntCounter,
    pub last_success_epoch: IntGauge,
    pub drift_detected: IntGaugeVec,
    pub managed_resources: IntGauge,
    pub rss_bytes: IntGauge,
}

impl Metrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();
        let sync_total =
            IntCounter::new("leancd_sync_total", "Number of reconciliation passes").map_err(map)?;
        let sync_errors = IntCounter::new(
            "leancd_sync_errors_total",
            "Number of failed reconciliations",
        )
        .map_err(map)?;
        let last_success_epoch = IntGauge::new(
            "leancd_sync_last_success_timestamp_seconds",
            "Unix timestamp of the last successful sync",
        )
        .map_err(map)?;
        let drift_detected = IntGaugeVec::new(
            Opts::new(
                "leancd_drift_detected",
                "Number of drifted resources, broken down by GVK",
            ),
            &["group", "version", "kind"],
        )
        .map_err(map)?;
        let managed_resources = IntGauge::new(
            "leancd_managed_resources",
            "Number of resources managed by leancd",
        )
        .map_err(map)?;
        let rss_bytes = IntGauge::new(
            "leancd_rss_bytes",
            "Resident set size of the leancd process (bytes)",
        )
        .map_err(map)?;

        registry
            .register(Box::new(sync_total.clone()))
            .map_err(map)?;
        registry
            .register(Box::new(sync_errors.clone()))
            .map_err(map)?;
        registry
            .register(Box::new(last_success_epoch.clone()))
            .map_err(map)?;
        registry
            .register(Box::new(drift_detected.clone()))
            .map_err(map)?;
        registry
            .register(Box::new(managed_resources.clone()))
            .map_err(map)?;
        registry
            .register(Box::new(rss_bytes.clone()))
            .map_err(map)?;

        Ok(Self {
            registry,
            sync_total,
            sync_errors,
            last_success_epoch,
            drift_detected,
            managed_resources,
            rss_bytes,
        })
    }
}

fn map(e: prometheus::Error) -> Error {
    Error::Other(format!("metrics: {e}"))
}

/// Serve the metrics endpoint until the runtime is shut down.
pub async fn serve(addr: &str, metrics: Arc<Metrics>) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "metrics endpoint listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let m = metrics.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &m).await {
                tracing::debug!(error = %e, "metrics: failed to handle scrape");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, metrics: &Metrics) -> std::io::Result<()> {
    // Refresh self RSS on every scrape so the gauge is fresh.
    if let Some(rss) = current_rss_bytes() {
        metrics.rss_bytes.set(rss as i64);
    }

    // Read and discard the request (we only care about serving /metrics).
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf).await;

    let encoder = TextEncoder::new();
    let metric_families = metrics.registry.gather();
    let mut body = Vec::with_capacity(2048);
    let _ = encoder.encode(&metric_families, &mut body);

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

/// Current process RSS in bytes from `/proc/self/status`, if available.
pub fn current_rss_bytes() -> Option<u64> {
    let me = procfs::process::Process::myself().ok()?;
    let statm = me.statm().ok()?;
    Some(statm.resident * procfs::page_size())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headline guarantee: even the test process itself must sit well under
    /// the 100MiB RSS budget. This is a sanity check for the measurement code;
    /// the authoritative benchmark runs against a simulated cluster (see
    /// `bench/`).
    #[test]
    fn rss_is_bounded_under_budget() {
        match current_rss_bytes() {
            Some(rss) => {
                assert!(rss > 0, "rss should be positive");
                let budget = 100 * 1024 * 1024;
                assert!(
                    rss < budget,
                    "current RSS {rss} bytes >= 100MiB budget {budget}"
                );
            }
            None => {
                // /proc unavailable (non-Linux test host) — nothing to assert.
            }
        }
    }

    #[test]
    fn drift_detected_records_per_gvk() {
        let m = Metrics::new().unwrap();
        m.drift_detected
            .with_label_values(&["apps", "v1", "Deployment"])
            .set(2);
        m.drift_detected
            .with_label_values(&["", "v1", "ConfigMap"])
            .set(1);

        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&m.registry.gather(), &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // Prometheus emits labels in alphabetical order (group, kind, version).
        assert!(
            out.contains(
                "leancd_drift_detected{group=\"apps\",kind=\"Deployment\",version=\"v1\"} 2"
            ),
            "missing or wrong Deployment drift series:\n{out}"
        );
        assert!(
            out.contains("leancd_drift_detected{group=\"\",kind=\"ConfigMap\",version=\"v1\"} 1"),
            "missing or wrong ConfigMap drift series:\n{out}"
        );
    }

    #[test]
    fn drift_detected_reset_clears_values() {
        let m = Metrics::new().unwrap();
        m.drift_detected
            .with_label_values(&["apps", "v1", "Deployment"])
            .set(2);
        m.drift_detected.reset();
        assert_eq!(
            m.drift_detected
                .with_label_values(&["apps", "v1", "Deployment"])
                .get(),
            0
        );
    }
}
