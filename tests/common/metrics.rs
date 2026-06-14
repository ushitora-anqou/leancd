//! Scrape leancd's `/metrics` endpoint over a port-forward.

use std::process::Command;

use crate::common::portforward::PortForward;

/// Scrape the metrics endpoint once.
pub fn scrape() -> String {
    let pf = PortForward::new("leancd", "svc/leancd-metrics", 9090);
    let url = format!("http://127.0.0.1:{}/metrics", pf.local_port);
    let out = Command::new("curl")
        .args(["-fsS", &url])
        .output()
        .expect("curl /metrics");
    assert!(
        out.status.success(),
        "scrape failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Parse the value of a labelless metric line (e.g. `leancd_sync_total 5`).
pub fn metric_value(text: &str, name: &str) -> Option<i64> {
    for line in text.lines() {
        if line.starts_with(name) && !line.starts_with('#') {
            if let Some(val) = line.rsplit(' ').next() {
                return val.trim().parse().ok();
            }
        }
    }
    None
}
