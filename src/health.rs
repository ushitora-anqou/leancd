//! Health check: classify the last sync state for liveness/readiness probes.
//!
//! `leancd health` reads the state ConfigMap and reports whether the last
//! reconciliation was recent and error-free. It exposes no HTTP listener — it
//! is meant for a Kubernetes `exec` probe. Exit codes:
//!   0 = fresh, 1 = never synced, 2 = stale, 3 = failing (last_error set).

use crate::config::Config;
use crate::error::Result;
use crate::state;
use kube::Client;

/// Outcome of a health check, mapped to an exit code by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// No state recorded yet (first run, or the state ConfigMap is absent).
    Never,
    /// Last sync is within the staleness threshold and recorded no error.
    Fresh,
    /// Last sync is older than the staleness threshold.
    Stale,
    /// The last sync recorded an error (takes priority over staleness).
    Failing,
}

impl HealthStatus {
    /// Probe exit code: fresh = 0, never = 1, stale = 2, failing = 3.
    pub fn exit_code(self) -> i32 {
        match self {
            HealthStatus::Fresh => 0,
            HealthStatus::Never => 1,
            HealthStatus::Stale => 2,
            HealthStatus::Failing => 3,
        }
    }
}

/// Classify a persisted [`state::State`] against the current time. Pure: no
/// I/O, fully unit-testable. `stale_threshold` is in seconds. A recorded
/// `last_error` takes priority over both freshness and staleness.
pub fn classify_health(
    state: Option<&state::State>,
    now: u64,
    stale_threshold: u64,
) -> HealthStatus {
    let Some(state) = state else {
        return HealthStatus::Never;
    };
    // A recorded error takes priority: even a fresh sync that failed is failing.
    if state.last_error.is_some() {
        return HealthStatus::Failing;
    }
    let Some(epoch) = state.last_sync_epoch else {
        return HealthStatus::Never;
    };
    if now.saturating_sub(epoch) > stale_threshold {
        HealthStatus::Stale
    } else {
        HealthStatus::Fresh
    }
}

/// Read the state ConfigMap and classify the current sync health. The
/// staleness threshold is `poll_interval * health_stale_factor` seconds.
pub async fn run_health(cfg: &Config) -> Result<HealthStatus> {
    let client = Client::try_default().await?;
    let state = state::read(&client, cfg).await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stale_threshold = cfg
        .poll_interval
        .as_secs()
        .saturating_mul(u64::from(cfg.health_stale_factor));
    Ok(classify_health(state.as_ref(), now, stale_threshold))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

    fn state_with(epoch: Option<u64>, error: Option<&str>) -> State {
        State {
            last_sync_epoch: epoch,
            last_error: error.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn no_state_is_never() {
        assert_eq!(classify_health(None, 1000, 60), HealthStatus::Never);
    }

    #[test]
    fn no_epoch_is_never() {
        let s = state_with(None, None);
        assert_eq!(classify_health(Some(&s), 1000, 60), HealthStatus::Never);
    }

    #[test]
    fn fresh_within_threshold() {
        let s = state_with(Some(1000), None);
        assert_eq!(classify_health(Some(&s), 1059, 60), HealthStatus::Fresh);
    }

    #[test]
    fn stale_beyond_threshold() {
        let s = state_with(Some(1000), None);
        assert_eq!(classify_health(Some(&s), 1061, 60), HealthStatus::Stale);
    }

    #[test]
    fn failing_takes_priority_over_fresh() {
        let s = state_with(Some(1000), Some("boom"));
        assert_eq!(classify_health(Some(&s), 1059, 60), HealthStatus::Failing);
    }

    #[test]
    fn failing_takes_priority_over_stale() {
        let s = state_with(Some(1000), Some("boom"));
        assert_eq!(classify_health(Some(&s), 9999, 60), HealthStatus::Failing);
    }

    #[test]
    fn failing_even_without_epoch() {
        // last_error set but no epoch: an error was recorded, so it is failing.
        let s = state_with(None, Some("boom"));
        assert_eq!(classify_health(Some(&s), 1000, 60), HealthStatus::Failing);
    }

    #[test]
    fn exit_codes() {
        assert_eq!(HealthStatus::Fresh.exit_code(), 0);
        assert_eq!(HealthStatus::Never.exit_code(), 1);
        assert_eq!(HealthStatus::Stale.exit_code(), 2);
        assert_eq!(HealthStatus::Failing.exit_code(), 3);
    }
}
