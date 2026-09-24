use std::collections::BTreeMap;
use std::error::Error as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::FutureExt;
use futures::stream::StreamExt;
use kube::api::Api;
use kube::core::{ClusterResourceScope, NamespaceResourceScope};
use kube::{Client, Resource, ResourceExt};
use kube_runtime::controller::Action;
use kube_runtime::finalizer::{Event as FinalizerEvent, finalizer};
use kube_runtime::watcher;
use rand::{Rng, rng};
use tracing::field::Empty;
use tracing::{Instrument, Span, error, info, info_span, trace, warn};

use crate::events::{Event, EventRecorder, EventType};
use crate::observe::{Outcome, Pass, Phase, ReconcileObserver, TraceMetadata};

/// An error from a reconciliation pass, as passed to
/// [`Context::error_action`] and [`Context::failure_event`].
#[derive(Debug, thiserror::Error)]
pub enum Error<E: std::error::Error + 'static> {
    /// [`Context::apply`] returned an error, and the context has no
    /// finalizer.
    #[error("{0}")]
    ControllerError(#[source] E),
    /// The context has a finalizer, and either [`Context::apply`] or
    /// [`Context::cleanup`] returned an error (wrapped as
    /// [`ApplyFailed`](kube_runtime::finalizer::Error::ApplyFailed) or
    /// [`CleanupFailed`](kube_runtime::finalizer::Error::CleanupFailed)), or
    /// adding or removing the finalizer failed.
    #[error("{0}")]
    FinalizerError(#[source] kube_runtime::finalizer::Error<E>),
}

impl<E: std::error::Error + 'static> Error<E> {
    /// Renders this error followed by each of its causes, separated by
    /// `": "`, as used in the note of the default
    /// [failure event](Context::failure_event).
    pub fn display_chain(&self) -> String {
        let mut out = self.to_string();
        let mut source = self.source();
        while let Some(err) = source {
            let message = err.to_string();
            // Error types commonly end their own message with their source's
            // (as this one does), which a naive join would repeat.
            if !out.ends_with(&message) {
                out.push_str(": ");
                out.push_str(&message);
            }
            source = err.source();
        }
        out
    }
}

/// The observability configured on a [`Controller`], shared by its passes.
struct Instrumentation {
    name: Arc<str>,
    observer: Option<Arc<dyn ReconcileObserver>>,
    events: Option<Arc<EventRecorder>>,
}

/// The [`Controller`] watches a set of resources, calling methods on the
/// provided [`Context`] when events occur.
pub struct Controller<Ctx: Context>
where
    Ctx: Send + Sync + 'static,
    Ctx::Error: Send + Sync + 'static,
    Ctx::Resource: Send + Sync + 'static,
    Ctx::Resource: Clone + std::fmt::Debug + serde::Serialize,
    for<'de> Ctx::Resource: serde::Deserialize<'de>,
    <Ctx::Resource as Resource>::DynamicType:
        Eq + Clone + std::hash::Hash + std::default::Default + std::fmt::Debug + std::marker::Unpin,
{
    client: kube::Client,
    make_api: Box<dyn Fn(&Ctx::Resource) -> Api<Ctx::Resource> + Sync + Send + 'static>,
    controller: kube_runtime::controller::Controller<Ctx::Resource>,
    context: Ctx,
    name: Option<Arc<str>>,
    observer: Option<Arc<dyn ReconcileObserver>>,
    events: Option<Arc<EventRecorder>>,
}

impl<Ctx: Context> Controller<Ctx>
where
    Ctx: Send + Sync + 'static,
    Ctx::Error: Send + Sync + 'static,
    Ctx::Resource: Clone + std::fmt::Debug + serde::Serialize,
    for<'de> Ctx::Resource: serde::Deserialize<'de>,
    <Ctx::Resource as Resource>::DynamicType:
        Eq + Clone + std::hash::Hash + std::default::Default + std::fmt::Debug + std::marker::Unpin,
{
    /// Creates a new controller for a namespaced resource using the given
    /// `client`. The `context` given determines the type of resource
    /// to watch (via the [`Context::Resource`] type provided as part of
    /// the trait implementation). The resources to be watched will be
    /// limited to resources in the given `namespace`. A [`watcher::Config`]
    /// can be given to limit the resources watched (for instance,
    /// `watcher::Config::default().labels("app=myapp")`).
    pub fn namespaced(client: Client, context: Ctx, namespace: &str, wc: watcher::Config) -> Self
    where
        Ctx::Resource: Resource<Scope = NamespaceResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |resource: &Ctx::Resource| {
                Api::<Ctx::Resource>::namespaced(client.clone(), &resource.namespace().unwrap())
            })
        };
        let controller = kube_runtime::controller::Controller::new(
            Api::<Ctx::Resource>::namespaced(client.clone(), namespace),
            wc,
        );
        Self::new(client, make_api, controller, context)
    }

    /// Creates a new controller for a namespaced resource using the given
    /// `client`. The `context` given determines the type of resource to
    /// watch (via the [`Context::Resource`] type provided as part of the
    /// trait implementation). The resources to be watched will not be
    /// limited by namespace. A [`watcher::Config`] can be given to limit the
    /// resources watched (for instance,
    /// `watcher::Config::default().labels("app=myapp")`).
    pub fn namespaced_all(client: Client, context: Ctx, wc: watcher::Config) -> Self
    where
        Ctx::Resource: Resource<Scope = NamespaceResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |resource: &Ctx::Resource| {
                Api::<Ctx::Resource>::namespaced(client.clone(), &resource.namespace().unwrap())
            })
        };
        let controller = kube_runtime::controller::Controller::new(
            Api::<Ctx::Resource>::all(client.clone()),
            wc,
        );
        Self::new(client, make_api, controller, context)
    }

    /// Creates a new controller for a cluster-scoped resource using the
    /// given `client`. The `context` given determines the type of resource
    /// to watch (via the [`Context::Resource`] type provided as part of the
    /// trait implementation). A [`watcher::Config`] can be given to limit the
    /// resources watched (for instance,
    /// `watcher::Config::default().labels("app=myapp")`).
    pub fn cluster(client: Client, context: Ctx, wc: watcher::Config) -> Self
    where
        Ctx::Resource: Resource<Scope = ClusterResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |_: &Ctx::Resource| Api::<Ctx::Resource>::all(client.clone()))
        };
        let controller = kube_runtime::controller::Controller::new(
            Api::<Ctx::Resource>::all(client.clone()),
            wc,
        );
        Self::new(client, make_api, controller, context)
    }

    fn new(
        client: Client,
        make_api: Box<dyn Fn(&Ctx::Resource) -> Api<Ctx::Resource> + Sync + Send + 'static>,
        controller: kube_runtime::controller::Controller<Ctx::Resource>,
        context: Ctx,
    ) -> Self {
        Self {
            client,
            make_api,
            controller,
            context,
            name: None,
            observer: None,
            events: None,
        }
    }

    /// Sets the name identifying this controller in its metrics (the
    /// `controller` field of [`ReconcileRecord`](crate::ReconcileRecord) and
    /// [`StepRecord`](crate::StepRecord)), and in the `controller` field of
    /// its `reconcile` tracing span. Defaults to [`Context::FINALIZER_NAME`]
    /// if set, and otherwise to the kind of the resource being watched.
    ///
    /// Controllers sharing an [observer](Controller::with_observer) or
    /// [event recorder](Controller::with_event_recorder) must have distinct
    /// names, or their metrics will be merged, and one controller
    /// succeeding will reset the aggregation of the other's failure events
    /// for the same resource.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into().into());
        self
    }

    /// Reports every reconciliation pass, and every [step](crate::Step)
    /// within one, to `observer`. See the [`observe`](crate::observe)
    /// module.
    pub fn with_observer(mut self, observer: Arc<dyn ReconcileObserver>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Publishes a Kubernetes event on the resource whenever reconciling it
    /// fails, as determined by [`Context::failure_event`], and enables
    /// [`TraceMetadata::publish_event`] for the context's own events. See
    /// the [`events`](crate::events) module, including for the RBAC
    /// permissions this requires.
    pub fn with_event_recorder(mut self, events: Arc<EventRecorder>) -> Self {
        self.events = Some(events);
        self
    }

    /// Run the controller. This method will not return. The [`Context`]
    /// given to the constructor will have its [`apply`](Context::apply)
    /// method called when a resource is created or updated, and its
    /// [`cleanup`](Context::cleanup) method called when a resource is about
    /// to be deleted.
    ///
    /// To run multiple replicas of a controller with only one reconciling
    /// at a time, pass this method's future to
    /// [`LeaderElection::with_lease`](crate::LeaderElection::with_lease).
    pub async fn run(self) {
        let Self {
            client,
            make_api,
            controller,
            context,
            name,
            observer,
            events,
        } = self;
        let instrumentation = Arc::new(Instrumentation {
            name: name.unwrap_or_else(|| match Ctx::FINALIZER_NAME {
                Some(finalizer_name) => finalizer_name.into(),
                None => Ctx::Resource::kind(&Default::default()).into(),
            }),
            observer,
            events,
        });
        let instrumentation = &instrumentation;
        let backoffs = Arc::new(Mutex::new(BTreeMap::new()));
        let backoffs = &backoffs;
        controller
            .run(
                |resource, context| {
                    let uid = resource.uid().unwrap();
                    let backoffs = Arc::clone(backoffs);
                    reconcile(
                        context,
                        client.clone(),
                        make_api(&resource),
                        resource,
                        Arc::clone(instrumentation),
                    )
                    .inspect(move |result| {
                        if result.is_ok() {
                            backoffs.lock().unwrap().remove(&uid);
                        }
                    })
                },
                |resource, err, context| {
                    let consecutive_errors = {
                        let uid = resource.uid().unwrap();
                        let mut backoffs = backoffs.lock().unwrap();
                        let consecutive_errors: u32 =
                            backoffs.get(&uid).copied().unwrap_or_default();
                        backoffs.insert(uid, consecutive_errors.saturating_add(1));
                        consecutive_errors
                    };
                    context.error_action(resource, err, consecutive_errors)
                },
                Arc::new(context),
            )
            .for_each(|res| async {
                // ReconcilerFailed errors will already have been reported by
                // the _reconcile function
                if let Err(e) = res
                    && !matches!(e, kube_runtime::controller::Error::ReconcilerFailed(..))
                {
                    // warn instead of error because these kinds of errors
                    // are almost always recoverable
                    warn!(
                        error = %e,
                        source = e.source(),
                        "internal kube controller error",
                    );
                }
            })
            .await
    }

    /// Allow configuring the underlying [`kube_runtime::Controller`]. For
    /// example, you can use
    /// `controller.with_controller(|controller| controller.with_config(Config::default().concurrency(10)))`
    /// to limit the created controller to reconciling 10 resources at once.
    pub fn with_controller<F>(mut self, f: F) -> Self
    where
        F: FnOnce(
            kube_runtime::Controller<Ctx::Resource>,
        ) -> kube_runtime::Controller<Ctx::Resource>,
    {
        self.controller = f(self.controller);
        self
    }
}

/// The [`Context`] trait should be implemented in order to provide callbacks
/// for events that happen to resources watched by a [`Controller`].
#[cfg_attr(not(docsrs), async_trait::async_trait)]
pub trait Context {
    /// The type of Kubernetes [resource](Resource) that will be watched by
    /// the [`Controller`] this context is passed to
    type Resource: Resource + Send + Sync + 'static;
    /// The error type which will be returned by the [`apply`](Self::apply)
    /// and [`cleanup`](Self::cleanup) methods
    type Error: std::error::Error;

    /// The name to use for the finalizer. This must be unique across
    /// controllers - if multiple controllers with the same finalizer name
    /// run against the same resource, unexpected behavior can occur.
    ///
    /// If this is None (the default), a finalizer will not be used, and
    /// cleanup events will not be reported.
    const FINALIZER_NAME: Option<&'static str> = None;

    /// This method is called when a watched resource is created or updated.
    /// The [`Client`] used by the controller is passed in to allow making
    /// additional API requests, as is the resource which triggered this
    /// event. If this method returns `Some(action)`, the given action will
    /// be performed, otherwise if `None` is returned,
    /// [`success_action`](Self::success_action) will be called to find the
    /// action to perform.
    ///
    /// `metadata` is state for this reconciliation pass, which can be used
    /// to annotate its tracing span, time [steps](TraceMetadata::step) of
    /// the work for metrics, and [publish events](TraceMetadata::publish_event)
    /// about the resource.
    ///
    /// What this returns determines the [`Outcome`] reported to the
    /// controller's [observer](Controller::with_observer), as described in
    /// [`Outcome::of_result`].
    async fn apply(
        &self,
        client: Client,
        resource: &Self::Resource,
        metadata: &mut TraceMetadata,
    ) -> Result<Option<Action>, Self::Error>;

    /// This method is called when a watched resource is marked for deletion.
    /// The [`Client`] used by the controller is passed in to allow making
    /// additional API requests, as is the resource which triggered this
    /// event. If this method returns `Some(action)`, the given action will
    /// be performed, otherwise if `None` is returned,
    /// [`success_action`](Self::success_action) will be called to find the
    /// action to perform.
    ///
    /// `metadata` and the return value are treated as for
    /// [`apply`](Self::apply).
    ///
    /// Note that this method will only be called if a finalizer is used.
    async fn cleanup(
        &self,
        client: Client,
        resource: &Self::Resource,
        metadata: &mut TraceMetadata,
    ) -> Result<Option<Action>, Self::Error> {
        // use a better name for the parameter name in the docs
        let _client = client;
        let _resource = resource;
        let _metadata = metadata;

        Ok(Some(Action::await_change()))
    }

    /// This method is called when a call to [`apply`](Self::apply) or
    /// [`cleanup`](Self::cleanup) returns `Ok(None)`. It should return the
    /// default [`Action`] to perform. The default implementation will
    /// requeue the event at a random time between 40 and 60 minutes in the
    /// future.
    fn success_action(&self, resource: &Self::Resource) -> Action {
        // use a better name for the parameter name in the docs
        let _resource = resource;

        Action::requeue(Duration::from_secs(rng().random_range(2400..3600)))
    }

    /// This method is called when a call to [`apply`](Self::apply) or
    /// [`cleanup`](Self::cleanup) returns `Err`. It should return the
    /// default [`Action`] to perform. The error returned will be passed in
    /// here, as well as a count of how many consecutive errors have happened
    /// for this resource, to allow for an exponential backoff strategy. The
    /// default implementation uses exponential backoff with a max of 256
    /// seconds and some added randomization to avoid thundering herds.
    fn error_action(
        self: Arc<Self>,
        resource: Arc<Self::Resource>,
        err: &Error<Self::Error>,
        consecutive_errors: u32,
    ) -> Action {
        // use a better name for the parameter name in the docs
        let _resource = resource;
        let _err = err;

        let seconds = 2u64.pow(consecutive_errors.min(7) + 1);
        Action::requeue(Duration::from_millis(
            rng().random_range((seconds * 500)..(seconds * 1000)),
        ))
    }

    /// This method is called when a reconciliation pass fails, if the
    /// controller has an [event recorder](Controller::with_event_recorder),
    /// to determine the Kubernetes event to publish on the resource. Return
    /// `None` to publish nothing, for instance for errors that are an
    /// expected part of waiting on something else.
    ///
    /// The default implementation publishes a `Warning` event with reason
    /// `ReconcileFailed` (or `CleanupFailed`, if the resource is being
    /// deleted), and the error and its causes as the note. Repeats of an
    /// identical event are aggregated, as described in the
    /// [`events`](crate::events) module; this means that errors which
    /// include something that varies from one attempt to the next (such as a
    /// timestamp or request ID) create a new event per attempt, so should be
    /// rephrased here.
    fn failure_event(
        &self,
        resource: &Self::Resource,
        phase: Phase,
        err: &Error<Self::Error>,
    ) -> Option<Event> {
        // use a better name for the parameter name in the docs
        let _resource = resource;

        let (reason, action) = match phase {
            Phase::Cleanup | Phase::Delete => ("CleanupFailed", "Cleanup"),
            _ => ("ReconcileFailed", "Reconcile"),
        };
        Some(Event {
            type_: EventType::Warning,
            reason: reason.to_owned(),
            action: action.to_owned(),
            note: Some(err.display_chain()),
            related: None,
        })
    }
}

/// Records a reconciliation pass as [abandoned](Outcome::Abandoned) if it is
/// dropped before being finished.
struct PassGuard {
    pass: Arc<Pass>,
    start: Instant,
    phase: Option<Phase>,
    fallback_phase: Phase,
    finished: bool,
}

impl PassGuard {
    fn phase(&self) -> Phase {
        self.phase.unwrap_or(self.fallback_phase)
    }

    fn finish(&mut self, outcome: Outcome) {
        self.finished = true;
        self.pass
            .record(self.phase(), outcome, self.start.elapsed());
    }
}

impl Drop for PassGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.pass
                .record(self.phase(), Outcome::Abandoned, self.start.elapsed());
        }
    }
}

async fn reconcile<Ctx>(
    ctx: Arc<Ctx>,
    client: Client,
    api: Api<Ctx::Resource>,
    resource: Arc<Ctx::Resource>,
    instrumentation: Arc<Instrumentation>,
) -> Result<Action, Error<Ctx::Error>>
where
    Ctx: Context + Send + Sync + 'static,
    Ctx::Error: Send + Sync + 'static,
    Ctx::Resource: Send + Sync + 'static,
    Ctx::Resource: Clone + std::fmt::Debug + serde::Serialize,
    for<'de> Ctx::Resource: serde::Deserialize<'de>,
    <Ctx::Resource as Resource>::DynamicType:
        Eq + Clone + std::hash::Hash + std::default::Default + std::fmt::Debug + std::marker::Unpin,
{
    let span = info_span!(
        "reconcile",
        resource_type = Ctx::Resource::kind(&Default::default()).as_ref(),
        resource_name = resource.name_unchecked().as_str(),
        controller = &*instrumentation.name,
        event_type = Empty,
        outcome = Empty,
        success = Empty,
        duration_seconds = Empty,
        metadata = Empty,
    );
    async {
        trace!("beginning reconciliation");

        let pass = Arc::new(Pass {
            controller: Arc::clone(&instrumentation.name),
            reference: resource.object_ref(&Default::default()),
            observer: instrumentation.observer.clone(),
            events: instrumentation.events.clone(),
        });
        let mut metadata = TraceMetadata::for_pass(Arc::clone(&pass));
        let mut guard = PassGuard {
            pass: Arc::clone(&pass),
            start: Instant::now(),
            phase: None,
            fallback_phase: if resource.meta().deletion_timestamp.is_some() {
                Phase::Delete
            } else {
                Phase::Init
            },
            finished: false,
        };
        let mut reconciler_outcome = None;

        let res = if let Some(finalizer_name) = Ctx::FINALIZER_NAME {
            finalizer(&api, finalizer_name, Arc::clone(&resource), |event| async {
                match event {
                    FinalizerEvent::Apply(resource) => {
                        guard.phase = Some(Phase::Apply);
                        let res = ctx.apply(client, &resource, &mut metadata).await;
                        reconciler_outcome = Some(Outcome::of_result(&res));
                        res.map(|action| action.unwrap_or_else(|| ctx.success_action(&resource)))
                    }
                    FinalizerEvent::Cleanup(resource) => {
                        guard.phase = Some(Phase::Cleanup);
                        let res = ctx.cleanup(client, &resource, &mut metadata).await;
                        reconciler_outcome = Some(Outcome::of_result(&res));
                        res.map(|action| action.unwrap_or_else(Action::await_change))
                    }
                }
            })
            .await
            .map_err(Error::FinalizerError)
        } else if resource.meta().deletion_timestamp.is_none() {
            guard.phase = Some(Phase::Apply);
            let res = ctx.apply(client, &resource, &mut metadata).await;
            reconciler_outcome = Some(Outcome::of_result(&res));
            res.map(|action| action.unwrap_or_else(|| ctx.success_action(&resource)))
                .map_err(Error::ControllerError)
        } else {
            Ok(Action::await_change())
        };

        let outcome = match &res {
            Err(_) => Outcome::Failed,
            Ok(_) => metadata
                .outcome
                .or(reconciler_outcome)
                .unwrap_or(Outcome::Completed),
        };
        let phase = guard.phase();
        let duration = guard.start.elapsed();
        guard.finish(outcome);

        let span = Span::current();
        span.record("event_type", phase.as_str());
        span.record("outcome", outcome.as_str());
        span.record("duration_seconds", duration.as_secs_f64());

        if !metadata.annotations.is_empty()
            && let Ok(s) = serde_json::to_string(&metadata.annotations)
        {
            span.record("metadata", s);
        }

        if let Err(e) = &res {
            span.record("success", false);
            error!(error = %e, source = e.source(), "reconcile");
        } else {
            span.record("success", true);
            info!("reconcile");
        }

        if let Some(events) = &instrumentation.events {
            match &res {
                Err(e) => {
                    if let Some(event) = ctx.failure_event(&resource, phase, e)
                        && let Err(publish_err) = events
                            .publish_as(Some(&instrumentation.name), &pass.reference, &event)
                            .await
                    {
                        warn!(
                            error = %publish_err,
                            reason = %event.reason,
                            "failed to publish reconciliation failure event",
                        );
                    }
                }
                Ok(_) => {
                    if let Some(uid) = &pass.reference.uid {
                        events.forget_failures(&instrumentation.name, uid);
                    }
                }
            }
        }

        res
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::Mutex;

    use futures::FutureExt;
    use k8s_openapi::api::core::v1::ConfigMap;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
    use k8s_openapi::jiff::Timestamp;

    use super::*;
    use crate::events::Reporter;
    use crate::observe::{ReconcileRecord, StepRecord};
    use crate::test_util::{MockApiServer, block_on};

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error("reconciling failed: {0}")]
        Wrapped(#[source] std::io::Error),
    }

    #[derive(Clone, Copy)]
    enum Behavior {
        Done,
        Requeue,
        Skip,
        Fail,
        FailQuietly,
        Hang,
    }

    struct TestContext {
        behavior: Mutex<Behavior>,
    }

    impl TestContext {
        fn new(behavior: Behavior) -> Arc<Self> {
            Arc::new(Self {
                behavior: Mutex::new(behavior),
            })
        }

        fn set(&self, behavior: Behavior) {
            *self.behavior.lock().unwrap() = behavior;
        }
    }

    #[async_trait::async_trait]
    impl Context for TestContext {
        type Resource = ConfigMap;
        type Error = TestError;

        async fn apply(
            &self,
            _client: Client,
            _resource: &ConfigMap,
            metadata: &mut TraceMetadata,
        ) -> Result<Option<Action>, TestError> {
            let behavior = *self.behavior.lock().unwrap();
            let step = metadata.step("work");
            match behavior {
                Behavior::Done => {
                    step.finish(Outcome::Completed);
                    Ok(None)
                }
                Behavior::Requeue => {
                    step.finish(Outcome::Waiting);
                    Ok(Some(Action::requeue(Duration::from_secs(1))))
                }
                Behavior::Skip => {
                    step.finish(Outcome::Skipped);
                    metadata.set_outcome(Outcome::Skipped);
                    Ok(None)
                }
                Behavior::Fail | Behavior::FailQuietly => {
                    Err(TestError::Wrapped(std::io::Error::other("disk on fire")))
                }
                Behavior::Hang => pending().await,
            }
        }

        fn failure_event(
            &self,
            _resource: &ConfigMap,
            _phase: Phase,
            err: &Error<TestError>,
        ) -> Option<Event> {
            match *self.behavior.lock().unwrap() {
                Behavior::FailQuietly => None,
                _ => Some(Event {
                    type_: EventType::Warning,
                    reason: "ReconcileFailed".to_owned(),
                    action: "Reconcile".to_owned(),
                    note: Some(err.display_chain()),
                    related: None,
                }),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Recorded {
        Pass(String, Phase, Outcome),
        Step(String, &'static str, Outcome),
    }

    #[derive(Default)]
    struct TestObserver(Mutex<Vec<Recorded>>);

    impl TestObserver {
        fn take(&self) -> Vec<Recorded> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    impl ReconcileObserver for TestObserver {
        fn reconciled(&self, record: &ReconcileRecord<'_>) {
            assert_eq!(record.kind, "ConfigMap");
            assert_eq!(record.namespace, Some("ns"));
            assert_eq!(record.name, "cm");
            self.0.lock().unwrap().push(Recorded::Pass(
                record.controller.to_owned(),
                record.phase,
                record.outcome,
            ));
        }

        fn step_finished(&self, record: &StepRecord<'_>) {
            self.0.lock().unwrap().push(Recorded::Step(
                record.controller.to_owned(),
                record.step,
                record.outcome,
            ));
        }
    }

    struct Harness {
        server: MockApiServer,
        observer: Arc<TestObserver>,
        instrumentation: Arc<Instrumentation>,
    }

    impl Harness {
        fn new() -> Self {
            let server = MockApiServer::new();
            let observer = Arc::new(TestObserver::default());
            let instrumentation = Arc::new(Instrumentation {
                name: "test".into(),
                observer: Some(Arc::<TestObserver>::clone(&observer)),
                events: Some(Arc::new(EventRecorder::new(
                    server.client(),
                    Reporter {
                        controller: "test.example.com".to_owned(),
                        instance: None,
                    },
                ))),
            });
            Self {
                server,
                observer,
                instrumentation,
            }
        }

        fn reconcile(
            &self,
            ctx: &Arc<TestContext>,
            resource: ConfigMap,
        ) -> impl Future<Output = Result<Action, Error<TestError>>> + use<> {
            let client = self.server.client();
            reconcile(
                Arc::clone(ctx),
                client.clone(),
                Api::namespaced(client, "ns"),
                Arc::new(resource),
                Arc::clone(&self.instrumentation),
            )
        }

        fn methods(&self) -> Vec<String> {
            self.server
                .requests()
                .into_iter()
                .map(|r| r.method)
                .collect()
        }
    }

    fn config_map() -> ConfigMap {
        ConfigMap {
            metadata: ObjectMeta {
                name: Some("cm".to_owned()),
                namespace: Some("ns".to_owned()),
                uid: Some("uid-1".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn pass(phase: Phase, outcome: Outcome) -> Recorded {
        Recorded::Pass("test".to_owned(), phase, outcome)
    }

    fn step(outcome: Outcome) -> Recorded {
        Recorded::Step("test".to_owned(), "work", outcome)
    }

    #[test]
    fn classifies_successful_passes() {
        block_on(async {
            let harness = Harness::new();
            let ctx = TestContext::new(Behavior::Done);
            harness.reconcile(&ctx, config_map()).await.unwrap();
            assert_eq!(
                harness.observer.take(),
                [
                    step(Outcome::Completed),
                    pass(Phase::Apply, Outcome::Completed)
                ]
            );

            ctx.set(Behavior::Requeue);
            harness.reconcile(&ctx, config_map()).await.unwrap();
            assert_eq!(
                harness.observer.take(),
                [step(Outcome::Waiting), pass(Phase::Apply, Outcome::Waiting)]
            );

            ctx.set(Behavior::Skip);
            harness.reconcile(&ctx, config_map()).await.unwrap();
            assert_eq!(
                harness.observer.take(),
                [step(Outcome::Skipped), pass(Phase::Apply, Outcome::Skipped)]
            );

            assert!(harness.server.requests().is_empty());
        });
    }

    #[test]
    fn publishes_and_aggregates_failure_events() {
        block_on(async {
            let harness = Harness::new();
            let ctx = TestContext::new(Behavior::Fail);
            harness.reconcile(&ctx, config_map()).await.unwrap_err();
            assert_eq!(
                harness.observer.take(),
                [
                    step(Outcome::Abandoned),
                    pass(Phase::Apply, Outcome::Failed)
                ]
            );
            let requests = harness.server.requests();
            assert_eq!(requests[0].body["reason"], "ReconcileFailed");
            assert_eq!(requests[0].body["note"], "reconciling failed: disk on fire");

            harness.reconcile(&ctx, config_map()).await.unwrap_err();
            assert_eq!(harness.methods(), ["POST", "PATCH"]);

            // a success ends the series, so the next failure is a new event
            ctx.set(Behavior::Done);
            harness.reconcile(&ctx, config_map()).await.unwrap();
            ctx.set(Behavior::Fail);
            harness.reconcile(&ctx, config_map()).await.unwrap_err();
            assert_eq!(harness.methods(), ["POST", "PATCH", "POST"]);
        });
    }

    #[test]
    fn failure_event_can_be_suppressed() {
        block_on(async {
            let harness = Harness::new();
            let ctx = TestContext::new(Behavior::FailQuietly);
            harness.reconcile(&ctx, config_map()).await.unwrap_err();
            assert_eq!(
                harness.observer.take(),
                [
                    step(Outcome::Abandoned),
                    pass(Phase::Apply, Outcome::Failed)
                ]
            );
            assert!(harness.server.requests().is_empty());
        });
    }

    #[test]
    fn deleted_resource_without_finalizer() {
        block_on(async {
            let harness = Harness::new();
            let ctx = TestContext::new(Behavior::Fail);
            let mut resource = config_map();
            resource.metadata.deletion_timestamp = Some(Time(Timestamp::now()));
            harness.reconcile(&ctx, resource).await.unwrap();
            assert_eq!(
                harness.observer.take(),
                [pass(Phase::Delete, Outcome::Completed)]
            );
        });
    }

    #[test]
    fn cancelled_pass_is_abandoned() {
        block_on(async {
            let harness = Harness::new();
            let ctx = TestContext::new(Behavior::Hang);
            let mut fut = Box::pin(harness.reconcile(&ctx, config_map()));
            assert!((&mut fut).now_or_never().is_none());
            assert!(harness.observer.take().is_empty());
            drop(fut);
            assert_eq!(
                harness.observer.take(),
                [
                    step(Outcome::Abandoned),
                    pass(Phase::Apply, Outcome::Abandoned)
                ]
            );
        });
    }

    #[test]
    fn display_chain_does_not_repeat_messages() {
        let err: Error<TestError> =
            Error::FinalizerError(kube_runtime::finalizer::Error::ApplyFailed(
                TestError::Wrapped(std::io::Error::other("disk on fire")),
            ));
        assert_eq!(
            err.display_chain(),
            "failed to apply object: reconciling failed: disk on fire"
        );

        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] std::io::Error);
        let err: Error<Outer> = Error::ControllerError(Outer(std::io::Error::other("inner")));
        assert_eq!(err.display_chain(), "outer: inner");

        #[derive(Debug, thiserror::Error)]
        #[error("reading lock: timed out waiting for lock")]
        struct Mentions(#[source] std::io::Error);
        let err: Error<Mentions> =
            Error::ControllerError(Mentions(std::io::Error::other("timed out")));
        assert_eq!(
            err.display_chain(),
            "reading lock: timed out waiting for lock: timed out"
        );
    }
}
