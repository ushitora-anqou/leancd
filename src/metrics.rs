//! OpenTelemetry metrics: leancd reports via OTLP/HTTP (push) to a collector.
//!
//! No HTTP listener is started — metrics are push-based only, so the process
//! exposes no inbound network surface and keeps its footprint flat. The OTLP
//! exporter (HTTP/protobuf, port 4318) is configured entirely through the
//! standard `OTEL_EXPORTER_OTLP_*` environment variables.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use opentelemetry::metrics::{Counter, Meter, ObservableGauge};
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::Resource;

use crate::error::{Error, Result};

/// Mutable metric values reported through `ObservableGauge` callbacks. An
/// observable gauge holds no value itself, so the latest values live here and
/// the callbacks read them at collection time.
#[derive(Default, Debug)]
struct MetricsState {
    last_success_epoch: i64,
    managed_resources: i64,
    /// (group, version, kind) -> number of drifted resources for that GVK.
    drift_per_gvk: HashMap<(String, String, String), i64>,
}

/// Container for leancd's OpenTelemetry instruments.
///
/// `Counter`s are incremented directly; the gauges (`last_success_epoch`,
/// `managed_resources`, `drift_detected`, `rss_bytes`) are observable gauges
/// backed by the shared [`MetricsState`] and reported on each collection. The
/// `_gauges` handles must outlive the meter for the callbacks to stay
/// registered, so they are held here for the life of the struct.
#[derive(Debug)]
pub struct Metrics {
    pub sync_total: Counter<u64>,
    pub sync_errors: Counter<u64>,
    pub hooks_total: Counter<u64>,
    state: Arc<Mutex<MetricsState>>,
    _gauges: [ObservableGauge<i64>; 4],
}

impl Metrics {
    /// Build the instruments on `meter` and register the observable-gauge
    /// callbacks that report the latest state (and self RSS) on collection.
    pub fn new(meter: &Meter) -> Self {
        let state: Arc<Mutex<MetricsState>> = Arc::new(Mutex::new(MetricsState::default()));

        let sync_total = meter
            .u64_counter("leancd_sync_total")
            .with_description("Number of reconciliation passes")
            .build();
        let sync_errors = meter
            .u64_counter("leancd_sync_errors_total")
            .with_description("Number of failed reconciliations")
            .build();
        let hooks_total = meter
            .u64_counter("leancd_hooks_total")
            .with_description("Helm hooks executed, by phase and result")
            .build();

        let st = state.clone();
        let last_success = meter
            .i64_observable_gauge("leancd_sync_last_success_timestamp_seconds")
            .with_description("Unix timestamp of the last successful sync")
            .with_callback(move |o| {
                o.observe(st.lock().unwrap().last_success_epoch, &[]);
            })
            .build();

        let st = state.clone();
        let managed = meter
            .i64_observable_gauge("leancd_managed_resources")
            .with_description("Number of resources managed by leancd")
            .with_callback(move |o| {
                o.observe(st.lock().unwrap().managed_resources, &[]);
            })
            .build();

        let st = state.clone();
        let rss = meter
            .i64_observable_gauge("leancd_rss_bytes")
            .with_description("Resident set size of the leancd process (bytes)")
            .with_callback(move |o| {
                if let Some(rss) = current_rss_bytes() {
                    o.observe(rss as i64, &[]);
                }
            })
            .build();

        let drift = meter
            .i64_observable_gauge("leancd_drift_detected")
            .with_description("Number of drifted resources, broken down by GVK")
            .with_callback(move |o| {
                let s = st.lock().unwrap();
                for ((group, version, kind), n) in s.drift_per_gvk.iter() {
                    o.observe(
                        *n,
                        &[
                            KeyValue::new("group", group.clone()),
                            KeyValue::new("version", version.clone()),
                            KeyValue::new("kind", kind.clone()),
                        ],
                    );
                }
            })
            .build();

        Self {
            sync_total,
            sync_errors,
            hooks_total,
            state,
            _gauges: [last_success, managed, rss, drift],
        }
    }

    /// Record the Unix timestamp of the most recent successful sync.
    pub fn set_last_success_epoch(&self, v: i64) {
        self.state.lock().unwrap().last_success_epoch = v;
    }

    /// Record how many resources leancd currently manages.
    pub fn set_managed_resources(&self, v: i64) {
        self.state.lock().unwrap().managed_resources = v;
    }

    /// Record hook executions for one phase, split by outcome. Attributes
    /// distinguish the phase (`presync`, `postsync`, `predelete`, `postdelete`)
    /// and the result (`succeeded` / `failed`).
    pub fn record_hooks(&self, phase: &str, succeeded: u64, failed: u64) {
        if succeeded > 0 {
            self.hooks_total.add(
                succeeded,
                &[
                    KeyValue::new("phase", phase.to_string()),
                    KeyValue::new("result", "succeeded".to_string()),
                ],
            );
        }
        if failed > 0 {
            self.hooks_total.add(
                failed,
                &[
                    KeyValue::new("phase", phase.to_string()),
                    KeyValue::new("result", "failed".to_string()),
                ],
            );
        }
    }

    /// Clear all per-GVK drift counts (resolved drifts disappear next pass).
    pub fn reset_drift(&self) {
        self.state.lock().unwrap().drift_per_gvk.clear();
    }

    /// Set the drift count for a single GVK.
    pub fn set_drift(&self, group: &str, version: &str, kind: &str, n: i64) {
        self.state.lock().unwrap().drift_per_gvk.insert(
            (group.to_string(), version.to_string(), kind.to_string()),
            n,
        );
    }
}

/// Build a `MeterProvider` that exports metrics over OTLP/HTTP at fixed
/// intervals (default 60s, override with `OTEL_METRIC_EXPORT_INTERVAL`). The
/// endpoint, protocol, headers, and timeout are read from the standard
/// `OTEL_EXPORTER_OTLP_*` environment variables by the exporter.
pub fn init_meter_provider() -> Result<SdkMeterProvider> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .build()
        .map_err(|e| Error::Other(format!("otel metric exporter: {e}")))?;
    let provider = SdkMeterProvider::builder()
        .with_resource(Resource::builder().with_service_name("leancd").build())
        .with_periodic_exporter(exporter)
        .build();
    Ok(provider)
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
    use std::sync::{Arc, Weak};

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::metrics::data::{Gauge, ResourceMetrics};
    use opentelemetry_sdk::metrics::reader::MetricReader;
    use opentelemetry_sdk::metrics::{
        InstrumentKind, ManualReader, MetricResult, Pipeline, Temporality,
    };

    /// The headline guarantee: even the test process itself must sit well under
    /// the RSS budget. This is a sanity check for the measurement code;
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
                    "current RSS {rss} bytes >= the budget {budget}"
                );
            }
            None => {
                // /proc unavailable (non-Linux test host) — nothing to assert.
            }
        }
    }

    /// A `MetricReader` shared between the provider (which owns a clone) and
    /// the test (which calls `collect`). `ManualReader` is not cloneable, so we
    /// wrap `Arc<dyn MetricReader>` and delegate — the same pattern the OTel SDK
    /// benchmarks use.
    #[derive(Clone, Debug)]
    struct SharedReader(Arc<dyn MetricReader>);

    impl MetricReader for SharedReader {
        fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
            self.0.register_pipeline(pipeline)
        }
        fn collect(&self, rm: &mut ResourceMetrics) -> MetricResult<()> {
            self.0.collect(rm)
        }
        fn force_flush(&self) -> OTelSdkResult {
            self.0.force_flush()
        }
        fn shutdown(&self) -> OTelSdkResult {
            self.0.shutdown()
        }
        fn temporality(&self, kind: InstrumentKind) -> Temporality {
            self.0.temporality(kind)
        }
    }

    /// Build a provider on a shared in-memory reader, apply `f` to a fresh
    /// `Metrics`, then collect the exported data through the reader.
    fn collect_after(f: impl FnOnce(&Metrics)) -> ResourceMetrics {
        let reader = SharedReader(Arc::new(ManualReader::builder().build()));
        let provider = SdkMeterProvider::builder()
            .with_resource(Resource::builder_empty().build())
            .with_reader(reader.clone())
            .build();
        let meter = provider.meter("leancd-test");
        let metrics = Metrics::new(&meter);
        f(&metrics);
        drop(metrics);
        drop(meter);
        let mut rm = ResourceMetrics {
            resource: Resource::builder_empty().build(),
            scope_metrics: Vec::new(),
        };
        reader.collect(&mut rm).expect("collect");
        rm
    }

    /// Look up the most recent data-point value of a labelless i64 gauge.
    fn gauge_value(rm: &ResourceMetrics, name: &str) -> Option<i64> {
        for scope in &rm.scope_metrics {
            for metric in &scope.metrics {
                if metric.name.as_ref() == name {
                    let data = metric.data.as_any().downcast_ref::<Gauge<i64>>()?;
                    return data.data_points.last().map(|p| p.value);
                }
            }
        }
        None
    }

    #[test]
    fn managed_resources_is_reported() {
        let rm = collect_after(|m| m.set_managed_resources(42));
        assert_eq!(gauge_value(&rm, "leancd_managed_resources"), Some(42));
    }

    #[test]
    fn drift_detected_records_per_gvk() {
        let rm = collect_after(|m| {
            m.set_drift("apps", "v1", "Deployment", 2);
            m.set_drift("", "v1", "ConfigMap", 1);
        });

        let mut found = 0;
        for scope in &rm.scope_metrics {
            for metric in &scope.metrics {
                if metric.name.as_ref() == "leancd_drift_detected" {
                    let data = metric
                        .data
                        .as_any()
                        .downcast_ref::<Gauge<i64>>()
                        .expect("gauge data");
                    assert_eq!(data.data_points.len(), 2, "expected two GVK series");
                    for dp in &data.data_points {
                        let g = str_attr(&dp.attributes, "group");
                        let v = str_attr(&dp.attributes, "version");
                        let k = str_attr(&dp.attributes, "kind");
                        match (g.as_str(), v.as_str(), k.as_str()) {
                            ("apps", "v1", "Deployment") => assert_eq!(dp.value, 2),
                            ("", "v1", "ConfigMap") => assert_eq!(dp.value, 1),
                            other => panic!("unexpected GVK {other:?}"),
                        }
                        found += 1;
                    }
                }
            }
        }
        assert_eq!(found, 2, "drift metric not collected");
    }

    #[test]
    fn drift_detected_reset_clears_values() {
        let rm = collect_after(|m| {
            m.set_drift("apps", "v1", "Deployment", 2);
            m.reset_drift();
        });

        for scope in &rm.scope_metrics {
            for metric in &scope.metrics {
                if metric.name.as_ref() == "leancd_drift_detected" {
                    let data = metric
                        .data
                        .as_any()
                        .downcast_ref::<Gauge<i64>>()
                        .expect("gauge data");
                    assert!(data.data_points.is_empty(), "reset did not clear series");
                }
            }
        }
    }

    /// Read a string-valued attribute, falling back to "" (cluster-scoped kinds
    /// have an absent group, which OTel may omit rather than emitting an empty).
    fn str_attr(attrs: &[KeyValue], key: &str) -> String {
        attrs
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
            .unwrap_or_default()
    }
}
