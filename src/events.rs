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
//! through [`EventRecorder::publish`] directly.
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

/// Publishes Kubernetes events on resources, aggregating repeats as described
/// in the [module documentation](self).
///
/// A single recorder is typically shared (via [`Arc`]) by every controller in
/// a process, and by the reconcilers that publish events of their own.
pub struct EventRecorder {
    client: Client,
    reporter: Reporter,
    series: Mutex<HashMap<SeriesKey, Series>>,
}

/// Identifies the events that can aggregate into one another.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SeriesKey {
    /// The controller whose failure events these are, or `None` for events
    /// published by reconcilers or other callers.
    origin: Option<Arc<str>>,
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
            .finish_non_exhaustive()
    }
}

impl EventRecorder {
    /// Creates a recorder that publishes events as `reporter`.
    ///
    /// [`Reporter::controller`] becomes each event's `reportingController`,
    /// and should name the controller (for instance
    /// `"my-operator.example.com"`). [`Reporter::instance`] becomes each
    /// event's `reportingInstance`, and should identify the replica, for
    /// which the pod name is a good choice; if it is `None`, the controller
    /// name is used instead.
    pub fn new(client: Client, reporter: Reporter) -> Self {
        Self {
            client,
            reporter,
            series: Mutex::new(HashMap::new()),
        }
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
    pub async fn publish<K>(&self, resource: &K, event: &Event) -> Result<(), kube::Error>
    where
        K: Resource,
        K::DynamicType: Default,
    {
        let reference = resource.object_ref(&Default::default());
        self.publish_as(None, &reference, event).await
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

    /// Forgets the failure events that `origin` published about the object
    /// with the given `uid`.
    pub(crate) fn forget_failures(&self, origin: &Arc<str>, uid: &str) {
        self.series
            .lock()
            .unwrap()
            .retain(|key, _| key.uid != uid || key.origin.as_ref() != Some(origin));
    }

    /// Publishes `event` about the object `reference` refers to, aggregating
    /// it only with other events from the same `origin`.
    pub(crate) async fn publish_as(
        &self,
        origin: Option<&Arc<str>>,
        reference: &ObjectReference,
        event: &Event,
    ) -> Result<(), kube::Error> {
        let event = Event {
            note: event.note.as_deref().map(truncate_note),
            ..(*event).clone()
        };
        let key = reference.uid.clone().map(|uid| SeriesKey {
            origin: origin.cloned(),
            uid,
            reason: event.reason.clone(),
            action: event.action.clone(),
        });

        if let Some(key) = &key
            && let Some((namespace, name, count)) = self.repeat_of(key, &event)
        {
            match self.patch_series(&namespace, &name, count).await {
                Ok(()) => {
                    if let Some(series) = self.series.lock().unwrap().get_mut(key) {
                        series.count = count;
                        series.last_published = Instant::now();
                    }
                    return Ok(());
                }
                // The API server deletes events some time after their last
                // update (an hour, by default), which a long-running series
                // can outlive. Start a new one in that case.
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => return Err(e),
            }
        }

        let namespace = reference
            .namespace
            .clone()
            .unwrap_or_else(|| CLUSTER_EVENT_NAMESPACE.to_owned());
        let created = self.create(&namespace, reference, &event).await?;
        if let Some(key) = key {
            self.series.lock().unwrap().insert(
                key,
                Series {
                    event,
                    namespace,
                    name: created.name_any(),
                    count: 1,
                    last_published: Instant::now(),
                },
            );
        }
        Ok(())
    }

    /// If `event` should aggregate into the series last published under
    /// `key`, returns that series' event's namespace, name, and new count.
    fn repeat_of(&self, key: &SeriesKey, event: &Event) -> Option<(String, String, i32)> {
        let mut series = self.series.lock().unwrap();
        let now = Instant::now();
        series.retain(|_, s| now.duration_since(s.last_published) < SERIES_WINDOW);
        series.get(key).filter(|s| s.event == *event).map(|s| {
            (
                s.namespace.clone(),
                s.name.clone(),
                s.count.saturating_add(1),
            )
        })
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
    fn forget_failures_only_affects_its_origin() {
        block_on(async {
            let server = MockApiServer::new();
            let recorder = recorder(&server);
            let cm = config_map("uid-1");
            let reference = cm.object_ref(&());
            let a: Arc<str> = "a".into();
            let b: Arc<str> = "b".into();
            recorder
                .publish_as(Some(&a), &reference, &event("it broke"))
                .await
                .unwrap();
            recorder
                .publish_as(Some(&b), &reference, &event("it broke"))
                .await
                .unwrap();
            recorder.publish(&cm, &event("it broke")).await.unwrap();
            recorder.forget_failures(&a, "uid-1");
            recorder
                .publish_as(Some(&a), &reference, &event("it broke"))
                .await
                .unwrap();
            recorder
                .publish_as(Some(&b), &reference, &event("it broke"))
                .await
                .unwrap();
            recorder.publish(&cm, &event("it broke")).await.unwrap();

            let methods: Vec<_> = server.requests().into_iter().map(|r| r.method).collect();
            assert_eq!(methods, ["POST", "POST", "POST", "POST", "PATCH", "PATCH"]);
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
