mod ddsketch;
mod label_filter;
mod recorder;
mod storage;

use std::sync::atomic::Ordering;

use chrono::Utc;
use metrics::Key;
use metrics_tracing_context::TracingContextLayer;
use metrics_util::layers::Layer;
use once_cell::sync::OnceCell;
use snafu::Snafu;

pub use self::ddsketch::{AgentDDSketch, BinMap, Config};
use self::{label_filter::VectorLabelFilter, recorder::Registry, recorder::VectorRecorder};
use crate::event::{Metric, MetricValue};

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Snafu)]
pub enum Error {
    #[snafu(display("Recorder already initialized."))]
    AlreadyInitialized,
    #[snafu(display("Metrics system was not initialized."))]
    NotInitialized,
}

static CONTROLLER: OnceCell<Controller> = OnceCell::new();

// Cardinality counter parameters, expose the internal metrics registry
// cardinality. Useful for the end users to help understand the characteristics
// of their environment and how vectors acts in it.
const CARDINALITY_KEY_NAME: &str = "internal_metrics_cardinality_total";
static CARDINALITY_KEY: Key = Key::from_static_name(CARDINALITY_KEY_NAME);

/// Controller allows capturing metric snapshots.
pub struct Controller {
    recorder: VectorRecorder,
}

fn metrics_enabled() -> bool {
    !matches!(std::env::var("DISABLE_INTERNAL_METRICS_CORE"), Ok(x) if x == "true")
}

fn tracing_context_layer_enabled() -> bool {
    !matches!(std::env::var("DISABLE_INTERNAL_METRICS_TRACING_INTEGRATION"), Ok(x) if x == "true")
}

fn init(recorder: VectorRecorder) -> Result<()> {
    // An escape hatch to allow disabing internal metrics core. May be used for
    // performance reasons. This is a hidden and undocumented functionality.
    if !metrics_enabled() {
        metrics::set_boxed_recorder(Box::new(metrics::NoopRecorder))
            .map_err(|_| Error::AlreadyInitialized)?;
        info!(message = "Internal metrics core is disabled.");
        return Ok(());
    }

    ////
    //// Prepare the controller
    ////

    // The `Controller` is a safe spot in memory for us to stash a clone of the
    // registry -- where metrics are actually kept -- so that our sub-systems
    // interested in these metrics can grab copies. See `capture_metrics` and
    // its callers for an example.
    let controller = Controller {
        recorder: recorder.clone(),
    };
    CONTROLLER
        .set(controller)
        .map_err(|_| Error::AlreadyInitialized)?;

    ////
    //// Initialize the recorder.
    ////

    // The recorder is the interface between metrics-rs and our registry. In our
    // case it doesn't _do_ much other than shepherd into the registry and
    // update the cardinality counter, see above, as needed.
    let recorder: Box<dyn metrics::Recorder> = if tracing_context_layer_enabled() {
        // Apply a layer to capture tracing span fields as labels.
        Box::new(TracingContextLayer::new(VectorLabelFilter).layer(recorder))
    } else {
        Box::new(recorder)
    };

    // This where we combine metrics-rs and our registry. We box it to avoid
    // having to fiddle with statics ourselves.
    metrics::set_boxed_recorder(recorder).map_err(|_| Error::AlreadyInitialized)
}

/// Initialize the default metrics sub-system
///
/// # Errors
///
/// This function will error if it is called multiple times.
pub fn init_global() -> Result<()> {
    init(VectorRecorder::new_global())
}

/// Initialize the thread-local metrics sub-system
///
/// # Errors
///
/// This function will error if it is called multiple times.
pub fn init_test() -> Result<()> {
    init(VectorRecorder::new_test())
}

impl Controller {
    /// Clear all metrics from the registry.
    pub fn reset(&self) {
        self.recorder.with_registry(Registry::clear);
    }

    /// Get a handle to the globally registered controller, if it's initialized.
    ///
    /// # Errors
    ///
    /// This function will fail if the metrics subsystem has not been correctly
    /// initialized.
    pub fn get() -> Result<&'static Self> {
        CONTROLLER.get().ok_or(Error::NotInitialized)
    }

    /// Take a snapshot of all gathered metrics and expose them as metric
    /// [`Event`](crate::event::Event)s.
    #[allow(clippy::cast_precision_loss)]
    pub fn capture_metrics(&self) -> Vec<Metric> {
        let timestamp = Utc::now();
        let mut metrics = Vec::<Metric>::new();

        self.recorder.with_registry(|registry| {
            registry.visit_counters(|key, counter| {
                // NOTE this will truncate if the value is greater than 2**52.
                let value = counter.load(Ordering::Relaxed) as f64;
                let value = MetricValue::Counter { value };
                metrics.push(Metric::from_metric_kv(key, value, timestamp));
            });
            registry.visit_gauges(|key, gauge| {
                let value = gauge.load(Ordering::Relaxed);
                let value = MetricValue::Gauge { value };
                metrics.push(Metric::from_metric_kv(key, value, timestamp));
            });
            registry.visit_histograms(|key, histogram| {
                let value = histogram.make_metric();
                metrics.push(Metric::from_metric_kv(key, value, timestamp));
            });
        });

        // Add aliases for deprecated metrics
        for i in 0..metrics.len() {
            let metric = &metrics[i];
            match metric.name() {
                "component_sent_events_total" => {
                    let alias = metric.clone().with_name("processed_events_total");
                    metrics.push(alias);
                }
                "component_sent_bytes_total" if metric.tag_matches("component_kind", "sink") => {
                    let alias = metric.clone().with_name("processed_bytes_total");
                    metrics.push(alias);
                }
                "component_received_bytes_total"
                    if metric.tag_matches("component_kind", "source") =>
                {
                    let alias = metric.clone().with_name("processed_bytes_total");
                    metrics.push(alias);
                }
                _ => {}
            }
        }

        let cardinality = MetricValue::Counter {
            value: (metrics.len() + 1) as f64,
        };
        metrics.push(Metric::from_metric_kv(
            &CARDINALITY_KEY,
            cardinality,
            timestamp,
        ));

        metrics
    }
}

#[macro_export]
/// This macro is used to emit metrics as a `counter` while simultaneously
/// converting from absolute values to incremental values.
///
/// Values that do not arrive in strictly monotonically increasing order are
/// ignored and will not be emitted.
macro_rules! update_counter {
    ($label:literal, $value:expr) => {{
        use ::std::sync::atomic::{AtomicU64, Ordering};

        static PREVIOUS_VALUE: AtomicU64 = AtomicU64::new(0);

        let new_value = $value;
        let mut previous_value = PREVIOUS_VALUE.load(Ordering::Relaxed);

        loop {
            // Either a new greater value has been emitted before this thread updated the counter
            // or values were provided that are not in strictly monotonically increasing order.
            // Ignore.
            if new_value <= previous_value {
                break;
            }

            match PREVIOUS_VALUE.compare_exchange_weak(
                previous_value,
                new_value,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                // Another thread has written a new value before us. Re-enter loop.
                Err(value) => previous_value = value,
                // Calculate delta to last emitted value and emit it.
                Ok(_) => {
                    let delta = new_value - previous_value;
                    // Albeit very unlikely, note that this sequence of deltas might be emitted in
                    // a different order than they were calculated.
                    counter!($label, delta);
                    break;
                }
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare_metrics(cardinality: usize) -> &'static Controller {
        let _ = init_test();
        let controller = Controller::get().unwrap();
        controller.reset();

        for idx in 0..cardinality {
            metrics::counter!("test", 1, "idx" => idx.to_string());
        }

        assert_eq!(controller.capture_metrics().len(), cardinality + 1);

        controller
    }

    #[test]
    fn cardinality_matches() {
        for cardinality in &[0, 1, 10, 100, 1000, 10000] {
            let controller = prepare_metrics(*cardinality);
            let list = controller.capture_metrics();
            assert_eq!(list.len(), cardinality + 1);
        }
    }
}
