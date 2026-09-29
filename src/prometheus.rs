use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, Opts};

use crate::observe::{ReconcileObserver, ReconcileRecord, StepRecord};

/// A [`ReconcileObserver`] that exports Prometheus metrics. Requires the
/// `prometheus` feature.
///
/// The metrics, each prefixed with the namespace given to
/// [`new`](PrometheusMetrics::new), are:
///
/// * `<namespace>_reconciliations_total{controller, phase, outcome}`: count
///   of reconciliation passes. An `outcome` of `failed` is the signal to
///   alert on. `waiting` is normal while a resource converges.
/// * `<namespace>_reconciliation_duration_seconds{controller, phase}`:
///   histogram of time spent in each pass. Reconcilers that wait by
///   requeueing, rather than by blocking, return promptly, so this measures
///   work done rather than time to converge.
/// * `<namespace>_reconciliation_steps_total{controller, step, outcome}`:
///   count of [steps](crate::Step). Since a step that an error propagates
///   out of is reported as `abandoned`, as is one in a pass that was
///   cancelled, `abandoned` locates where passes stop, but is not itself a
///   failure signal.
/// * `<namespace>_reconciliation_step_duration_seconds{controller, step}`:
///   histogram of time spent in each step.
///
/// Label values are the [controller name](crate::Controller::with_name),
/// [`Phase::as_str`](crate::Phase::as_str),
/// [`Outcome::as_str`](crate::Outcome::as_str), and the step name.
///
/// This is a [`Collector`], so it must be registered with a registry to be
/// exported, and then shared by every controller that reports to it:
///
/// ```no_run
/// # use std::sync::Arc;
/// # fn f() -> Result<(), prometheus::Error> {
/// let metrics = k8s_controller::PrometheusMetrics::new("my_operator")?;
/// let registry = prometheus::Registry::new();
/// registry.register(Box::new(metrics.clone()))?;
/// let observer: Arc<dyn k8s_controller::ReconcileObserver> = Arc::new(metrics);
/// // pass `Arc::clone(&observer)` to each `Controller::with_observer`
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct PrometheusMetrics {
    reconciliations: IntCounterVec,
    reconciliation_duration: HistogramVec,
    steps: IntCounterVec,
    step_duration: HistogramVec,
}

impl PrometheusMetrics {
    /// The default histogram buckets, in seconds.
    pub const DEFAULT_BUCKETS: &[f64] = &[0.01, 0.05, 0.25, 1.0, 5.0, 30.0];

    /// Creates the metrics, with names prefixed by `namespace` (which may be
    /// empty, for no prefix) and histograms using
    /// [`DEFAULT_BUCKETS`](Self::DEFAULT_BUCKETS).
    pub fn new(namespace: &str) -> Result<Self, prometheus::Error> {
        Self::with_buckets(namespace, Self::DEFAULT_BUCKETS.to_vec())
    }

    /// Creates the metrics, with names prefixed by `namespace` (which may be
    /// empty, for no prefix) and histograms using the given `buckets`, in
    /// seconds.
    pub fn with_buckets(namespace: &str, buckets: Vec<f64>) -> Result<Self, prometheus::Error> {
        Ok(Self {
            reconciliations: IntCounterVec::new(
                Opts::new(
                    "reconciliations_total",
                    "Count of reconciliation passes, by controller, by the phase of the resource's \
                     lifecycle handled, and by what the pass concluded. An outcome of `failed` \
                     means the reconciler returned an error.",
                )
                .namespace(namespace),
                &["controller", "phase", "outcome"],
            )?,
            reconciliation_duration: HistogramVec::new(
                HistogramOpts::new(
                    "reconciliation_duration_seconds",
                    "Time spent in one reconciliation pass.",
                )
                .namespace(namespace)
                .buckets(buckets.clone()),
                &["controller", "phase"],
            )?,
            steps: IntCounterVec::new(
                Opts::new(
                    "reconciliation_steps_total",
                    "Count of reconciliation steps, by controller, by step, and by what the step \
                     concluded. An outcome of `abandoned` means the step did not conclude, either \
                     because an error propagated out of it or because the pass was cancelled.",
                )
                .namespace(namespace),
                &["controller", "step", "outcome"],
            )?,
            step_duration: HistogramVec::new(
                HistogramOpts::new(
                    "reconciliation_step_duration_seconds",
                    "Time spent in one reconciliation step.",
                )
                .namespace(namespace)
                .buckets(buckets),
                &["controller", "step"],
            )?,
        })
    }
}

impl ReconcileObserver for PrometheusMetrics {
    fn reconciled(&self, record: &ReconcileRecord<'_>) {
        let phase = record.phase.as_str();
        self.reconciliations
            .with_label_values(&[record.controller, phase, record.outcome.as_str()])
            .inc();
        self.reconciliation_duration
            .with_label_values(&[record.controller, phase])
            .observe(record.duration.as_secs_f64());
    }

    fn step_finished(&self, record: &StepRecord<'_>) {
        self.steps
            .with_label_values(&[record.controller, record.step, record.outcome.as_str()])
            .inc();
        self.step_duration
            .with_label_values(&[record.controller, record.step])
            .observe(record.duration.as_secs_f64());
    }
}

impl PrometheusMetrics {
    fn collectors(&self) -> [&dyn Collector; 4] {
        let Self {
            reconciliations,
            reconciliation_duration,
            steps,
            step_duration,
        } = self;
        [
            reconciliations,
            reconciliation_duration,
            steps,
            step_duration,
        ]
    }
}

impl Collector for PrometheusMetrics {
    fn desc(&self) -> Vec<&Desc> {
        self.collectors()
            .into_iter()
            .flat_map(Collector::desc)
            .collect()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        self.collectors()
            .into_iter()
            .flat_map(Collector::collect)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{Outcome, Phase};

    #[test]
    fn exports_records() {
        let metrics = PrometheusMetrics::new("test").unwrap();
        let registry = prometheus::Registry::new();
        registry.register(Box::new(metrics.clone())).unwrap();

        metrics.reconciled(&ReconcileRecord {
            controller: "widgets",
            kind: "Widget",
            namespace: Some("ns"),
            name: "w",
            phase: Phase::Apply,
            outcome: Outcome::Failed,
            duration: Duration::from_millis(20),
        });
        metrics.step_finished(&StepRecord {
            controller: "widgets",
            kind: "Widget",
            namespace: Some("ns"),
            name: "w",
            step: "deployment",
            outcome: Outcome::Abandoned,
            duration: Duration::from_millis(10),
        });

        let families = registry.gather();
        let names: Vec<_> = families.iter().map(|f| f.name()).collect();
        assert_eq!(
            names,
            [
                "test_reconciliation_duration_seconds",
                "test_reconciliation_step_duration_seconds",
                "test_reconciliation_steps_total",
                "test_reconciliations_total",
            ]
        );
        let labels = |name: &str| -> Vec<(String, String)> {
            let family = families.iter().find(|f| f.name() == name).unwrap();
            family.get_metric()[0]
                .get_label()
                .iter()
                .map(|l| (l.name().to_owned(), l.value().to_owned()))
                .collect()
        };
        let owned = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            labels("test_reconciliations_total"),
            owned(&[
                ("controller", "widgets"),
                ("outcome", "failed"),
                ("phase", "apply")
            ])
        );
        assert_eq!(
            labels("test_reconciliation_steps_total"),
            owned(&[
                ("controller", "widgets"),
                ("outcome", "abandoned"),
                ("step", "deployment")
            ])
        );
    }
}
