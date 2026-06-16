//! Read leancd's metrics over a port-forward to the OTel Collector's
//! Prometheus exporter. leancd itself exposes no HTTP endpoint; it pushes
//! OTLP/HTTP to the collector, which re-exports the series here.

use std::process::Command;

use crate::common::portforward::PortForward;

/// Scrape the collector's Prometheus exporter once, retrying briefly until
/// leancd's series appear (the first OTLP export may not have landed yet).
pub fn scrape() -> String {
    let pf = PortForward::new("leancd", "svc/otel-collector", 8889);
    let url = format!("http://127.0.0.1:{}/metrics", pf.local_port);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let out = Command::new("curl")
            .args(["-fsS", &url])
            .output()
            .expect("curl /metrics");
        assert!(
            out.status.success(),
            "scrape failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let body = String::from_utf8_lossy(&out.stdout).to_string();
        if body.contains("leancd_") || std::time::Instant::now() > deadline {
            return body;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
