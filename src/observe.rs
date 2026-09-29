//! Observing what reconciliation does, for metrics.
//!
//! A [`Controller`](crate::Controller) given a [`ReconcileObserver`] via
//! [`with_observer`](crate::Controller::with_observer) reports every
//! reconciliation pass to it as a [`ReconcileRecord`]. Within a pass,
//! reconcilers can additionally divide their work into named [`Step`]s,
//! started with [`TraceMetadata::step`], each reported as a [`StepRecord`].
//! Steps are what attribute a slow or failing pass to the part of the
//! reconciler responsible, and are the only way to report that a part of it
//! was [skipped](Outcome::Skipped).
//!
//! With the `prometheus` feature enabled,
//! [`PrometheusMetrics`](crate::PrometheusMetrics) implements
//! [`ReconcileObserver`] by exporting Prometheus metrics. Implement the trait
//! directly to export to another metrics system.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::ObjectReference;
use kube_runtime::controller::Action;
use tracing::warn;

use crate::events::{Event, EventRecorder};

/// What a reconciliation pass, or a step of one, concluded.
///
/// [`as_str`](Outcome::as_str) gives the value used as a metric label, which
/// is stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Outcome {
    /// Brought what it manages to the desired state, or found it already
    /// there.
    Completed,
    /// Made progress, but the desired state has not been reached yet, and
    /// asked to be run again to continue.
    Waiting,
    /// Had nothing to do, for instance because what it manages is disabled
    /// by configuration. Never inferred; reconcilers must report it
    /// explicitly.
    Skipped,
    /// Returned an error.
    Failed,
    /// Stopped without reaching a conclusion: either the reconciliation was
    /// cancelled (for instance because leadership was lost, or the process
    /// is shutting down), or, for a [`Step`], an error propagated out of it
    /// before it was finished.
    Abandoned,
}

impl Outcome {
    /// The label value for this outcome.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Waiting => "waiting",
            Outcome::Skipped => "skipped",
            Outcome::Failed => "failed",
            Outcome::Abandoned => "abandoned",
        }
    }

    /// Classifies a result in the form returned by
    /// [`Context::apply`](crate::Context::apply) and
    /// [`Context::cleanup`](crate::Context::cleanup):
    ///
    /// * `Ok(None)` and `Ok(Some(Action::await_change()))` are
    ///   [`Completed`](Outcome::Completed), since there is nothing more to
    ///   do until something changes.
    /// * Any other `Ok(Some(action))` is [`Waiting`](Outcome::Waiting),
    ///   since the reconciler asked to be run again.
    /// * `Err(_)` is [`Failed`](Outcome::Failed).
    ///
    /// A reconciler that returns a requeue action as a periodic resync after
    /// having fully converged will therefore be reported as waiting, and
    /// should override that with [`TraceMetadata::set_outcome`].
    pub fn of_result<E>(result: &Result<Option<Action>, E>) -> Self {
        match result {
            Ok(None) => Outcome::Completed,
            Ok(Some(action)) if *action == Action::await_change() => Outcome::Completed,
            Ok(Some(_)) => Outcome::Waiting,
            Err(_) => Outcome::Failed,
        }
    }
}

impl Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which part of the resource's lifecycle a reconciliation pass handled.
///
/// [`as_str`](Phase::as_str) gives the value used as a metric label, which is
/// stable, and matches the `event_type` field of the `reconcile` tracing
/// span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// The controller added its finalizer to the resource. Neither
    /// [`apply`](crate::Context::apply) nor
    /// [`cleanup`](crate::Context::cleanup) was called; the resource will be
    /// reconciled again once the finalizer is in place.
    Init,
    /// [`Context::apply`](crate::Context::apply) was called.
    Apply,
    /// [`Context::cleanup`](crate::Context::cleanup) was called.
    Cleanup,
    /// The resource is being deleted, and there was no cleanup to run:
    /// either the context has no finalizer, or the controller's finalizer
    /// had already been removed.
    Delete,
}

impl Phase {
    /// The label value for this phase.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Init => "init",
            Phase::Apply => "apply",
            Phase::Cleanup => "cleanup",
            Phase::Delete => "delete",
        }
    }
}

impl Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A completed (or [abandoned](Outcome::Abandoned)) reconciliation pass, as
/// reported to [`ReconcileObserver::reconciled`].
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct ReconcileRecord<'a> {
    /// The [name](crate::Controller::with_name) of the controller.
    pub controller: &'a str,
    /// The kind of the reconciled resource.
    pub kind: &'a str,
    /// The namespace of the reconciled resource, if it is namespaced.
    pub namespace: Option<&'a str>,
    /// The name of the reconciled resource.
    pub name: &'a str,
    pub phase: Phase,
    pub outcome: Outcome,
    /// Time spent in the pass, including the controller's finalizer
    /// bookkeeping but excluding publishing a failure event.
    pub duration: Duration,
}

/// A finished (or [abandoned](Outcome::Abandoned)) [`Step`], as reported to
/// [`ReconcileObserver::step_finished`].
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct StepRecord<'a> {
    /// The [name](crate::Controller::with_name) of the controller.
    pub controller: &'a str,
    /// The kind of the reconciled resource.
    pub kind: &'a str,
    /// The namespace of the reconciled resource, if it is namespaced.
    pub namespace: Option<&'a str>,
    /// The name of the reconciled resource.
    pub name: &'a str,
    /// The name the step was started with.
    pub step: &'static str,
    pub outcome: Outcome,
    pub duration: Duration,
}

/// Receives reports of what reconciliation did.
///
/// Methods are called synchronously from the reconciliation (including from
/// [`Step`]'s `Drop` implementation), so they should be cheap and must not
/// block.
///
/// Records identify the reconciled resource, but its namespace and name
/// usually make poor metric labels: they are unbounded, and a label set is
/// kept for as long as the process lives, even after the resource is
/// deleted.
pub trait ReconcileObserver: Send + Sync {
    /// Called when a reconciliation pass ends, including when it is
    /// cancelled.
    fn reconciled(&self, record: &ReconcileRecord<'_>) {
        let _record = record;
    }

    /// Called when a [`Step`] is finished or dropped.
    fn step_finished(&self, record: &StepRecord<'_>) {
        let _record = record;
    }
}

/// The context shared by everything recorded during one reconciliation pass.
pub(crate) struct Pass {
    pub(crate) controller: Arc<str>,
    pub(crate) reference: ObjectReference,
    pub(crate) observer: Option<Arc<dyn ReconcileObserver>>,
    pub(crate) events: Option<Arc<EventRecorder>>,
}

impl Pass {
    fn kind(&self) -> &str {
        self.reference.kind.as_deref().unwrap_or_default()
    }

    fn name(&self) -> &str {
        self.reference.name.as_deref().unwrap_or_default()
    }

    pub(crate) fn record(&self, phase: Phase, outcome: Outcome, duration: Duration) {
        if let Some(observer) = &self.observer {
            observer.reconciled(&ReconcileRecord {
                controller: &self.controller,
                kind: self.kind(),
                namespace: self.reference.namespace.as_deref(),
                name: self.name(),
                phase,
                outcome,
                duration,
            });
        }
    }
}

/// Times one named step of a reconciliation pass, reporting it to the
/// controller's [`ReconcileObserver`] when finished or dropped.
///
/// Created by [`TraceMetadata::step`]. A step dropped without being finished
/// is reported as [`Outcome::Abandoned`], so that `?` propagating an error
/// out of a step identifies the step the pass stopped in, without any
/// bookkeeping at the early return. The flip side is that every path out of
/// a step that does reach a conclusion must finish it explicitly:
///
/// ```no_run
/// # use k8s_controller::{Outcome, TraceMetadata};
/// # async fn create_certificate() -> Result<(), kube::Error> { Ok(()) }
/// # async fn f(metadata: &TraceMetadata, tls_enabled: bool) -> Result<(), kube::Error> {
/// let step = metadata.step("certificate");
/// if tls_enabled {
///     create_certificate().await?;
///     step.finish(Outcome::Completed);
/// } else {
///     step.finish(Outcome::Skipped);
/// }
/// # Ok(())
/// # }
/// ```
#[must_use = "a step that is never finished is reported as abandoned"]
pub struct Step {
    pass: Option<Arc<Pass>>,
    name: &'static str,
    start: Instant,
    outcome: Option<Outcome>,
}

impl Step {
    /// Finishes the step with the given outcome.
    pub fn finish(mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
    }

    /// Finishes the step with the outcome [classified](Outcome::of_result)
    /// from a reconciler-style result.
    ///
    /// Prefer this to propagating an error out of the step with `?` when the
    /// result is in hand, since that reports the step as
    /// [abandoned](Outcome::Abandoned) rather than [failed](Outcome::Failed).
    pub fn finish_with<E>(self, result: &Result<Option<Action>, E>) {
        self.finish(Outcome::of_result(result));
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        let Some(pass) = &self.pass else { return };
        let Some(observer) = &pass.observer else {
            return;
        };
        observer.step_finished(&StepRecord {
            controller: &pass.controller,
            kind: pass.kind(),
            namespace: pass.reference.namespace.as_deref(),
            name: pass.name(),
            step: self.name,
            outcome: self.outcome.unwrap_or(Outcome::Abandoned),
            duration: self.start.elapsed(),
        });
    }
}

/// Per-pass state handed to [`Context::apply`](crate::Context::apply) and
/// [`Context::cleanup`](crate::Context::cleanup).
///
/// It collects annotations for the pass's tracing span, times
/// [steps](TraceMetadata::step), overrides the pass's reported
/// [outcome](TraceMetadata::set_outcome), and
/// [publishes events](TraceMetadata::publish_event) about the resource being
/// reconciled.
///
/// A `TraceMetadata` created with [`Default`] (for instance, to call a
/// reconciler from a unit test) accepts all of these, but reports steps and
/// publishes events nowhere.
#[derive(Default)]
pub struct TraceMetadata {
    pub(crate) annotations: BTreeMap<String, String>,
    pub(crate) outcome: Option<Outcome>,
    pass: Option<Arc<Pass>>,
}

impl std::fmt::Debug for TraceMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceMetadata")
            .field("annotations", &self.annotations)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

impl TraceMetadata {
    pub(crate) fn for_pass(pass: Arc<Pass>) -> Self {
        Self {
            pass: Some(pass),
            ..Default::default()
        }
    }

    /// Adds a key/value pair to the `metadata` field of the pass's
    /// `reconcile` tracing span, replacing any earlier value for `key`.
    pub fn annotate<K: Display, V: Display>(&mut self, key: K, val: V) {
        self.annotations.insert(key.to_string(), val.to_string());
    }

    /// Starts timing the step named `name`. See [`Step`].
    ///
    /// Step names become metric labels, so they should come from a small
    /// fixed set.
    pub fn step(&self, name: &'static str) -> Step {
        Step {
            pass: self.pass.clone(),
            name,
            start: Instant::now(),
            outcome: None,
        }
    }

    /// Overrides the outcome reported for this pass, if the reconciler
    /// returns `Ok`. By default the outcome is
    /// [classified](Outcome::of_result) from what the reconciler returns;
    /// an `Err` is always reported as [`Outcome::Failed`].
    pub fn set_outcome(&mut self, outcome: Outcome) {
        self.outcome = Some(outcome);
    }

    /// Publishes `event` about the resource being reconciled, using the
    /// controller's [event recorder](crate::Controller::with_event_recorder).
    /// Does nothing if the controller has none.
    ///
    /// Failure to publish is logged rather than returned, since an event
    /// only reports on reconciliation and should not change its course.
    /// Use [`EventRecorder::publish`] directly to handle errors.
    pub async fn publish_event(&self, event: Event) {
        let Some(pass) = &self.pass else { return };
        let Some(events) = &pass.events else { return };
        if let Err(e) = events.publish_to(&pass.reference, &event, false).await {
            warn!(
                error = %e,
                reason = %event.reason,
                controller = %pass.controller,
                "failed to publish event",
            );
        }
    }
}
