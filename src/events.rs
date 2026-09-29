//! Publishing Kubernetes [events](https://kubernetes.io/docs/reference/kubernetes-api/cluster-resources/event-v1/)
//! about the resources a controller reconciles.
//!
//! Events are what `kubectl describe` shows beneath an object, which makes
//! them the place to explain to someone without access to the controller's
//! logs why an object is not converging.
//!
//! An [`EventRecorder`] can be given to a [`Controller`](crate::Controller)
//! via [`with_event_recorder`](crate::Controller::with_event_recorder), in
//! which case every failed reconciliation is published as an event on the
//! resource being reconciled (see [`Context::failure_event`](crate::Context::failure_event)).
//! Reconcilers can publish events of their own through
//! [`TraceMetadata::publish_event`](crate::TraceMetadata::publish_event), or
//! through [`EventRecorder::publish`] directly, which also works outside of
//! reconciliation (for instance, from a background task).
//!
//! Each controller should have its own recorder, whose [`Reporter`] names
//! that controller, even when several controllers in a process reconcile
//! the same kind of resource. The reporter is the only thing in an event
//! that identifies which controller published it.
//!
//! # Aggregation
//!
//! Publishing an event identical to the one last published for the same
//! object, reason, and action, within [`SERIES_WINDOW`], does not create a
//! new event object. Instead it increments the `series.count` of the existing
//! one, so that a reconciliation retrying on a backoff does not bury its
//! resource in identical events. Any difference, including in the note,
//! starts a new event, so that an event always describes the most recent
//! occurrence accurately.
//!
//! Failure events published by a controller additionally start afresh after
//! the resource next reconciles successfully, so a failure that recurs after
//! a recovery is reported as a new event rather than as a continuation of
//! the old one.
//!
//! # Timeouts
//!
//! Each publish is bounded by the recorder's [timeout](EventRecorder::with_timeout),
//! so that an unresponsive API server delays the reconciliation publishing
//! an event by at most that long.
//!
//! # RBAC
//!
//! Publishing requires `create` and `patch` permissions on `events` in the
//! `events.k8s.io` API group, in every namespace the controller reconciles
//! resources in. Events about cluster-scoped resources are created in the
//! `default` namespace.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::ObjectReference;
use k8s_openapi::api::events::v1::Event as KubeEvent;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams};
use kube::{Client, Resource, ResourceExt};

pub use kube_runtime::events::{EventType, Reporter};

/// How long after an event was last published that publishing it again
/// still aggregates into it, rather than creating a new event.
///
/// This matches the aggregation window of client-go's event correlator.
pub const SERIES_WINDOW: Duration = Duration::from_secs(10 * 60);

/// The Kubernetes API server rejects events whose note is longer than this
/// many bytes.
pub const MAX_NOTE_BYTES: usize = 1024;

/// The default [timeout](EventRecorder::with_timeout) for publishing an
/// event.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Namespace used for events about cluster-scoped resources, which have no
/// namespace of their own.
const CLUSTER_EVENT_NAMESPACE: &str = "default";

/// An event to publish about a resource.
///
/// `reason` and `action` must each be at most 128 characters, or the API
/// server will reject the event.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Whether this is an ordinary event or a problem. Shown as `Type` by
    /// `kubectl describe`.
    pub type_: EventType,
    /// Why the event happened, as a machine-readable `PascalCase` word (for
    /// instance `ReconcileFailed`). Shown as `Reason` by `kubectl describe`,
    /// and what tools and dashboards group events by, so it should be
    /// treated as a stable interface.
    pub reason: String,
    /// What was being done when the event happened, as a machine-readable
    /// `PascalCase` word (for instance `Reconcile`). Not shown by
    /// `kubectl describe`.
    pub action: String,
    /// A human-readable description of what happened. Shown as `Message` by
    /// `kubectl describe`. Truncated to [`MAX_NOTE_BYTES`] if longer.
    pub note: Option<String>,
    /// Another object involved in the event, if any.
    pub related: Option<ObjectReference>,
}

/// An error publishing an event.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// The API server rejected the event, or could not be reached.
    #[error(transparent)]
    Kube(#[from] kube::Error),
    /// Publishing did not complete within the recorder's
    /// [timeout](EventRecorder::with_timeout).
    #[error("timed out after {0:?} publishing event")]
    Timeout(Duration),
}

/// Publishes Kubernetes events on resources on behalf of one controller,
/// aggregating repeats as described in the [module documentation](self).
///
/// Share a recorder (via [`Arc`]) between a [`Controller`](crate::Controller)
/// and whatever else publishes events on that controller's behalf, but not
/// between controllers.
pub struct EventRecorder {
    client: Client,
    reporter: Reporter,
    timeout: Duration,
    series: Mutex<HashMap<SeriesKey, Slot>>,
}

/// The series published under one [`SeriesKey`], locked for the whole of a
/// publish so that concurrent publishes of the same event cannot both
/// create it, or both patch it to the same count.
type Slot = Arc<tokio::sync::Mutex<Option<Series>>>;

/// Identifies the events that can aggregate into one another.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SeriesKey {
    /// Whether these are the failure events the controller publishes when
    /// reconciling the resource fails, as opposed to events published
    /// through [`EventRecorder::publish`] or
    /// [`TraceMetadata::publish_event`](crate::TraceMetadata::publish_event).
    failure: bool,
    uid: String,
    reason: String,
    action: String,
}

/// The last event published under a [`SeriesKey`].
struct Series {
    event: Event,
    namespace: String,
    name: String,
    count: i32,
    last_published: Instant,
}

impl std::fmt::Debug for EventRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventRecorder")
            .field("reporter", &self.reporter)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl EventRecorder {
    /// Creates a recorder that publishes events as `reporter`, with the
    /// [default timeout](DEFAULT_TIMEOUT).
    ///
    /// [`Reporter::controller`] becomes each event's `reportingController`,
    /// shown by `kubectl describe` as where the event came from. It should
    /// name the individual controller, and must be a qualified name (for
    /// instance `"my-operator.example.com/widgets"`), or the API server
    /// rejects the event. [`Reporter::instance`] becomes each event's
    /// `reportingInstance`, and should identify the replica, for which the
    /// pod name is a good choice; if it is `None`, the controller name is
    /// used instead.
    pub fn new(client: Client, reporter: Reporter) -> Self {
        Self {
            client,
            reporter,
            timeout: DEFAULT_TIMEOUT,
            series: Mutex::new(HashMap::new()),
        }
    }

    /// Sets how long publishing an event may take, including any wait for
    /// a concurrent publish of the same event, before giving up with
    /// [`PublishError::Timeout`].
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The reporter this recorder publishes events as.
    pub fn reporter(&self) -> &Reporter {
        &self.reporter
    }

    /// Publishes `event` about `resource`.
    ///
    /// A note longer than [`MAX_NOTE_BYTES`] is truncated to fit rather than
    /// having the whole event rejected.
    ///
    /// Events are informational, so callers should usually log a failure to
    /// publish one rather than let it change what they do next.
    pub async fn publish<K>(&self, resource: &K, event: &Event) -> Result<(), PublishError>
    where
        K: Resource,
        K::DynamicType: Default,
    {
        let reference = resource.object_ref(&Default::default());
        self.publish_to(&reference, event, false).await
    }

    /// Forgets every event published about `resource`, so that the next
    /// event published about it creates a new event object even if it is
    /// identical to the last one.
    ///
    /// This is useful when an event reports a state that the resource can
    /// leave and later re-enter, and each entry should be reported as a
    /// distinct occurrence.
    pub fn forget<K: Resource>(&self, resource: &K) {
        if let Some(uid) = resource.meta().uid.as_deref() {
            self.series.lock().unwrap().retain(|key, _| key.uid != uid);
        }
    }

    /// Forgets the failure events published about the object with the given
    /// `uid`.
    pub(crate) fn forget_failures(&self, uid: &str) {
        self.series
            .lock()
            .unwrap()
            .retain(|key, _| key.uid != uid || !key.failure);
    }

    /// Publishes `event` about the object `reference` refers to, as a
    /// failure event if `failure` is set.
    pub(crate) async fn publish_to(
        &self,
        reference: &ObjectReference,
        event: &Event,
        failure: bool,
    ) -> Result<(), PublishError> {
        tokio::time::timeout(
            self.timeout,
            self.publish_unbounded(reference, event, failure),
        )
        .await
        .map_err(|_| PublishError::Timeout(self.timeout))?
        .map_err(PublishError::Kube)
    }

    async fn publish_unbounded(
        &self,
        reference: &ObjectReference,
        event: &Event,
        failure: bool,
    ) -> Result<(), kube::Error> {
        let event = Event {
            note: event.note.as_deref().map(truncate_note),
            ..(*event).clone()
        };
        let namespace = reference
            .namespace
            .clone()
            .unwrap_or_else(|| CLUSTER_EVENT_NAMESPACE.to_owned());
        let Some(uid) = reference.uid.clone() else {
            self.create(&namespace, reference, &event).await?;
            return Ok(());
        };
        let slot = self.slot(SeriesKey {
            failure,
            uid,
            reason: event.reason.clone(),
            action: event.action.clone(),
        });
        let mut series = slot.lock().await;

        if let Some(series) = series
            .as_mut()
            .filter(|s| s.event == event && s.last_published.elapsed() < SERIES_WINDOW)
        {
            let count = series.count.saturating_add(1);
            match self
                .patch_series(&series.namespace, &series.name, count)
                .await
            {
                Ok(()) => {
                    series.count = count;
                    series.last_published = Instant::now();
                    return Ok(());
                }
                // The API server deletes events some time after their last
                // update (an hour, by default), which a long-running series
                // can outlive. Start a new one in that case.
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => return Err(e),
            }
        }

        let created = self.create(&namespace, reference, &event).await?;
        *series = Some(Series {
            event,
            namespace,
            name: created.name_any(),
            count: 1,
            last_published: Instant::now(),
        });
        Ok(())
    }

    /// Returns the slot for `key`, evicting the slots of series that can no
    /// longer be aggregated into.
    fn slot(&self, key: SeriesKey) -> Slot {
        let mut slots = self.series.lock().unwrap();
        let now = Instant::now();
        slots.retain(|k, slot| {
            *k == key
                || slot.try_lock().map_or(true, |series| {
                    series
                        .as_ref()
                        .is_some_and(|s| now.duration_since(s.last_published) < SERIES_WINDOW)
                })
        });
        Arc::clone(slots.entry(key).or_default())
    }

    async fn create(
        &self,
        namespace: &str,
        reference: &ObjectReference,
        event: &Event,
    ) -> Result<KubeEvent, kube::Error> {
        let api: Api<KubeEvent> = Api::namespaced(self.client.clone(), namespace);
        let event = KubeEvent {
            metadata: ObjectMeta {
                generate_name: Some(format!(
                    "{}.",
                    reference
                        .name
                        .as_deref()
                        .unwrap_or(&self.reporter.controller)
                )),
                namespace: Some(namespace.to_owned()),
                ..Default::default()
            },
            action: Some(event.action.clone()),
            reason: Some(event.reason.clone()),
            note: event.note.clone(),
            type_: Some(
                match event.type_ {
                    EventType::Normal => "Normal",
                    EventType::Warning => "Warning",
                }
                .to_owned(),
            ),
            event_time: Some(MicroTime(Timestamp::now())),
            regarding: Some(reference.clone()),
            related: event.related.clone(),
            reporting_controller: Some(self.reporter.controller.clone()),
            reporting_instance: Some(
                self.reporter
                    .instance
                    .clone()
                    .unwrap_or_else(|| self.reporter.controller.clone()),
            ),
            ..Default::default()
        };
        api.create(&PostParams::default(), &event).await
    }

    async fn patch_series(
        &self,
        namespace: &str,
        name: &str,
        count: i32,
    ) -> Result<(), kube::Error> {
        let api: Api<KubeEvent> = Api::namespaced(self.client.clone(), namespace);
        let patch = serde_json::json!({
            "series": {
                "count": count,
                "lastObservedTime": MicroTime(Timestamp::now()),
            },
        });
        api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }
}

/// Shortens `note` to at most [`MAX_NOTE_BYTES`], on a character boundary.
fn truncate_note(note: &str) -> String {
    if note.len() <= MAX_NOTE_BYTES {
        return note.to_owned();
    }
    const ELLIPSIS: &str = "...";
    let mut end = MAX_NOTE_BYTES - ELLIPSIS.len();
    while !note.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &note[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{MockApiServer, block_on};

    use k8s_openapi::api::core::v1::ConfigMap;
    use serde_json::json;

    fn config_map(uid: &str) -> ConfigMap {
        ConfigMap {
            metadata: ObjectMeta {
                name: Some("cm".to_owned()),
                namespace: Some("ns".to_owned()),
                uid: Some(uid.to_owned()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn event(note: &str) -> Event {
        Event {
            type_: EventType::Warning,
            reason: "Broken".to_owned(),
            action: "Reconcile".to_owned(),
            note: Some(note.to_owned()),
            related: None,
        }
    }

    fn recorder(server: &MockApiServer) -> EventRecorder {
        EventRecorder::new(
            server.client(),
            Reporter {
                controller: "test.example.com".to_owned(),
                instance: Some("pod-0".to_owned()),
            },
        )
    }

    #[test]
    fn test_truncate_note() {
        assert_eq!(truncate_note("short"), "short");

        let exact = "a".repeat(MAX_NOTE_BYTES);
        assert_eq!(truncate_note(&exact), exact);

        let long = "a".repeat(MAX_NOTE_BYTES + 1);
        let truncated = truncate_note(&long);
        assert_eq!(truncated.len(), MAX_NOTE_BYTES);
        assert!(truncated.ends_with("..."));

        let multibyte = "é".repeat(MAX_NOTE_BYTES);
        let truncated = truncate_note(&multibyte);
        assert!(truncated.len() <= MAX_NOTE_BYTES);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn creates_event_with_reporter_and_reference() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            recorder
                .publish(&config_map("uid-1"), &event("it broke"))
                .await
                .unwrap();

            let requests = server.requests();
            assert_eq!(requests.len(), 1);
            let req = &requests[0];
            assert_eq!(req.method, "POST");
            assert_eq!(req.path, "/apis/events.k8s.io/v1/namespaces/ns/events");
            assert_eq!(req.body["type"], "Warning");
            assert_eq!(req.body["reason"], "Broken");
            assert_eq!(req.body["action"], "Reconcile");
            assert_eq!(req.body["note"], "it broke");
            assert_eq!(req.body["reportingController"], "test.example.com");
            assert_eq!(req.body["reportingInstance"], "pod-0");
            assert_eq!(req.body["regarding"]["kind"], "ConfigMap");
            assert_eq!(req.body["regarding"]["uid"], "uid-1");
            assert_eq!(req.body["metadata"]["generateName"], "cm.");
        });
    }

    #[test]
    fn aggregates_identical_events() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            let cm = config_map("uid-1");
            for _ in 0..3 {
                recorder.publish(&cm, &event("it broke")).await.unwrap();
            }

            let requests = server.requests();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].method, "POST");
            let name = requests[0].created_name();
            for (req, count) in requests[1..].iter().zip([2, 3]) {
                assert_eq!(req.method, "PATCH");
                assert_eq!(
                    req.path,
                    format!("/apis/events.k8s.io/v1/namespaces/ns/events/{name}")
                );
                assert_eq!(req.body["series"]["count"], json!(count));
                assert!(req.body["series"]["lastObservedTime"].is_string());
            }
        });
    }

    #[test]
    fn concurrent_identical_events_aggregate() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            let cm = config_map("uid-1");
            let ev = event("it broke");
            let results =
                futures::future::join_all((0..3).map(|_| recorder.publish(&cm, &ev))).await;
            assert!(results.iter().all(Result::is_ok));

            let requests = server.requests();
            let methods: Vec<_> = requests.iter().map(|r| r.method.as_str()).collect();
            assert_eq!(methods, ["POST", "PATCH", "PATCH"]);
            assert_eq!(requests[1].body["series"]["count"], json!(2));
            assert_eq!(requests[2].body["series"]["count"], json!(3));
        });
    }

    #[test]
    fn changed_note_starts_new_event() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            let cm = config_map("uid-1");
            recorder.publish(&cm, &event("first cause")).await.unwrap();
            recorder.publish(&cm, &event("second cause")).await.unwrap();
            recorder.publish(&cm, &event("second cause")).await.unwrap();

            let methods: Vec<_> = server.requests().into_iter().map(|r| r.method).collect();
            assert_eq!(methods, ["POST", "POST", "PATCH"]);
        });
    }

    #[test]
    fn forget_starts_new_event() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            let cm = config_map("uid-1");
            recorder.publish(&cm, &event("it broke")).await.unwrap();
            recorder.forget(&cm);
            recorder.publish(&cm, &event("it broke")).await.unwrap();

            let methods: Vec<_> = server.requests().into_iter().map(|r| r.method).collect();
            assert_eq!(methods, ["POST", "POST"]);
        });
    }

    #[test]
    fn forget_failures_keeps_other_events() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            let cm = config_map("uid-1");
            let reference = cm.object_ref(&());
            recorder
                .publish_to(&reference, &event("it broke"), true)
                .await
                .unwrap();
            recorder.publish(&cm, &event("it broke")).await.unwrap();
            recorder.forget_failures("uid-1");
            recorder
                .publish_to(&reference, &event("it broke"), true)
                .await
                .unwrap();
            recorder.publish(&cm, &event("it broke")).await.unwrap();

            let methods: Vec<_> = server.requests().into_iter().map(|r| r.method).collect();
            assert_eq!(methods, ["POST", "POST", "POST", "PATCH"]);
        });
    }

    #[test]
    fn times_out_and_recovers() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server).with_timeout(Duration::from_millis(50));
            let cm = config_map("uid-1");
            server.hang_next_request();
            let err = recorder.publish(&cm, &event("it broke")).await.unwrap_err();
            assert!(matches!(err, PublishError::Timeout(_)), "{err:?}");

            recorder.publish(&cm, &event("it broke")).await.unwrap();
            let methods: Vec<_> = server.requests().into_iter().map(|r| r.method).collect();
            assert_eq!(methods, ["POST"]);
        });
    }

    #[test]
    fn expired_event_is_recreated() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            let cm = config_map("uid-1");
            recorder.publish(&cm, &event("it broke")).await.unwrap();
            server.fail_next_patch(404);
            recorder.publish(&cm, &event("it broke")).await.unwrap();
            recorder.publish(&cm, &event("it broke")).await.unwrap();

            let requests = server.requests();
            let methods: Vec<_> = requests.iter().map(|r| r.method.clone()).collect();
            assert_eq!(methods, ["POST", "PATCH", "POST", "PATCH"]);
            assert_eq!(requests[3].body["series"]["count"], json!(2));
        });
    }

    #[test]
    fn cluster_scoped_events_go_to_default_namespace() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            let ns = k8s_openapi::api::core::v1::Namespace {
                metadata: ObjectMeta {
                    name: Some("some-namespace".to_owned()),
                    uid: Some("uid-1".to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            };
            recorder.publish(&ns, &event("it broke")).await.unwrap();

            let requests = server.requests();
            assert_eq!(
                requests[0].path,
                "/apis/events.k8s.io/v1/namespaces/default/events"
            );
        });
    }
}
