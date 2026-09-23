#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(double_negations)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::panicking_overflow_checks)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
#![cfg_attr(docsrs, feature(async_fn_in_trait))]

//! This crate implements a lightweight framework around
//! [`kube_runtime::Controller`] which provides a simpler interface for common
//! controller patterns. To use it, you define the data that your controller is
//! going to operate over, and implement the [`Context`] trait on that struct:
//!
//! ```no_run
//! # use std::collections::BTreeSet;
//! # use std::sync::{Arc, Mutex};
//! # use k8s_openapi::api::core::v1::Pod;
//! # use kube::{Client, Resource};
//! # use kube_runtime::controller::Action;
//! #[derive(Default, Clone)]
//! struct PodCounter {
//!     pods: Arc<Mutex<BTreeSet<String>>>,
//! }
//!
//! impl PodCounter {
//!     fn pod_count(&self) -> usize {
//!         let mut pods = self.pods.lock().unwrap();
//!         pods.len()
//!     }
//! }
//!
//! #[async_trait::async_trait]
//! impl k8s_controller::Context for PodCounter {
//!     type Resource = Pod;
//!     type Error = kube::Error;
//!
//!     const FINALIZER_NAME: Option<&'static str> = Some("example.com/pod-counter");
//!
//!     async fn apply(
//!         &self,
//!         client: Client,
//!         pod: &Self::Resource,
//!         _metadata: &mut k8s_controller::TraceMetadata,
//!     ) -> Result<Option<Action>, Self::Error> {
//!         let mut pods = self.pods.lock().unwrap();
//!         pods.insert(pod.meta().uid.as_ref().unwrap().clone());
//!         Ok(None)
//!     }
//!
//!     async fn cleanup(
//!         &self,
//!         client: Client,
//!         pod: &Self::Resource,
//!         _metadata: &mut k8s_controller::TraceMetadata,
//!     ) -> Result<Option<Action>, Self::Error> {
//!         let mut pods = self.pods.lock().unwrap();
//!         pods.remove(pod.meta().uid.as_ref().unwrap());
//!         Ok(None)
//!     }
//! }
//! ```
//!
//! Then you can run it against your Kubernetes cluster by creating a
//! [`Controller`]:
//!
//! ```no_run
//! # use std::collections::BTreeSet;
//! # use std::sync::{Arc, Mutex};
//! # use std::thread::sleep;
//! # use std::time::Duration;
//! # use k8s_openapi::api::core::v1::Pod;
//! # use kube::{Config, Client};
//! # use kube_runtime::controller::Action;
//! # use kube_runtime::watcher;
//! # use tokio::task;
//! # #[derive(Default, Clone)]
//! # struct PodCounter {
//! #     pods: Arc<Mutex<BTreeSet<String>>>,
//! # }
//! # impl PodCounter {
//! #     fn pod_count(&self) -> usize { todo!() }
//! # }
//! # #[async_trait::async_trait]
//! # impl k8s_controller::Context for PodCounter {
//! #     type Resource = Pod;
//! #     type Error = kube::Error;
//! #     const FINALIZER_NAME: Option<&'static str> = Some("example.com/pod-counter");
//! #     async fn apply(
//! #         &self,
//! #         client: Client,
//! #         pod: &Self::Resource,
//! #         _metadata: &mut k8s_controller::TraceMetadata,
//! #     ) -> Result<Option<Action>, Self::Error> { todo!() }
//! #     async fn cleanup(
//! #         &self,
//! #         client: Client,
//! #         pod: &Self::Resource,
//! #         _metadata: &mut k8s_controller::TraceMetadata,
//! #     ) -> Result<Option<Action>, Self::Error> { todo!() }
//! # }
//! # async fn foo() {
//! let kube_config = Config::infer().await.unwrap();
//! let kube_client = Client::try_from(kube_config).unwrap();
//! let context = PodCounter::default();
//! let controller = k8s_controller::Controller::namespaced_all(
//!     kube_client,
//!     context.clone(),
//!     watcher::Config::default(),
//! );
//! task::spawn(controller.run());
//!
//! loop {
//!     println!("{} pods running", context.pod_count());
//!     sleep(Duration::from_secs(1));
//! }
//! # }
//! ```
//!
//! If you run multiple replicas of your controller (for instance, to avoid
//! downtime of webhooks served by the same process during rollouts), you
//! can use [leader election](LeaderElection) to ensure that only one
//! replica reconciles at a time:
//!
//! ```no_run
//! # use std::collections::BTreeSet;
//! # use std::sync::{Arc, Mutex};
//! # use k8s_openapi::api::core::v1::Pod;
//! # use kube::{Config, Client};
//! # use kube_runtime::controller::Action;
//! # use kube_runtime::watcher;
//! # #[derive(Default, Clone)]
//! # struct PodCounter {
//! #     pods: Arc<Mutex<BTreeSet<String>>>,
//! # }
//! # #[async_trait::async_trait]
//! # impl k8s_controller::Context for PodCounter {
//! #     type Resource = Pod;
//! #     type Error = kube::Error;
//! #     const FINALIZER_NAME: Option<&'static str> = Some("example.com/pod-counter");
//! #     async fn apply(
//! #         &self,
//! #         client: Client,
//! #         pod: &Self::Resource,
//! #         _metadata: &mut k8s_controller::TraceMetadata,
//! #     ) -> Result<Option<Action>, Self::Error> { todo!() }
//! # }
//! # async fn foo() {
//! # let kube_config = Config::infer().await.unwrap();
//! # let kube_client = Client::try_from(kube_config).unwrap();
//! # let context = PodCounter::default();
//! let leader_election = k8s_controller::LeaderElection::new(
//!     kube_client.clone(),
//!     "my-namespace",
//!     "pod-counter",
//!     // must be unique per replica; the pod name is a good choice
//!     &std::env::var("HOSTNAME").unwrap(),
//! );
//! loop {
//!     let controller = k8s_controller::Controller::namespaced_all(
//!         kube_client.clone(),
//!         context.clone(),
//!         watcher::Config::default(),
//!     );
//!     leader_election.with_lease(controller.run()).await;
//!     // leadership was lost; the controller has been stopped, and we loop
//!     // to rejoin the election. Exiting the process (and letting
//!     // Kubernetes restart it) works too, and is preferable if your
//!     // reconcilers spawn tasks or do blocking work that stopping the
//!     // controller can't cancel.
//! }
//! # }
//! ```
//!
//! A process that runs several controllers should usually guard them all
//! with a single lease, rather than electing a separate leader per
//! controller (which could scatter the controllers across replicas). Use
//! [`LeaderElection::with_lease`] with a future that runs all of them:
//!
//! ```no_run
//! # use std::collections::BTreeSet;
//! # use std::sync::{Arc, Mutex};
//! # use k8s_openapi::api::core::v1::Pod;
//! # use kube::{Config, Client};
//! # use kube_runtime::controller::Action;
//! # use kube_runtime::watcher;
//! # #[derive(Default, Clone)]
//! # struct PodCounter {
//! #     pods: Arc<Mutex<BTreeSet<String>>>,
//! # }
//! # #[async_trait::async_trait]
//! # impl k8s_controller::Context for PodCounter {
//! #     type Resource = Pod;
//! #     type Error = kube::Error;
//! #     const FINALIZER_NAME: Option<&'static str> = Some("example.com/pod-counter");
//! #     async fn apply(
//! #         &self,
//! #         client: Client,
//! #         pod: &Self::Resource,
//! #         _metadata: &mut k8s_controller::TraceMetadata,
//! #     ) -> Result<Option<Action>, Self::Error> { todo!() }
//! # }
//! # async fn foo() {
//! # let kube_config = Config::infer().await.unwrap();
//! # let kube_client = Client::try_from(kube_config).unwrap();
//! # let leader_election = k8s_controller::LeaderElection::new(
//! #     kube_client.clone(),
//! #     "my-namespace",
//! #     "pod-counter",
//! #     &std::env::var("HOSTNAME").unwrap(),
//! # );
//! let controller_a = k8s_controller::Controller::namespaced(
//!     kube_client.clone(),
//!     PodCounter::default(),
//!     "namespace-a",
//!     watcher::Config::default(),
//! );
//! let controller_b = k8s_controller::Controller::namespaced(
//!     kube_client.clone(),
//!     PodCounter::default(),
//!     "namespace-b",
//!     watcher::Config::default(),
//! );
//! leader_election
//!     .with_lease(futures::future::join(controller_a.run(), controller_b.run()))
//!     .await;
//! // leadership was lost; both controllers have been stopped
//! std::process::exit(1);
//! # }
//! ```
//!
//! # Observability
//!
//! Every reconciliation pass is logged, as a `reconcile` [tracing]
//! span. Beyond that, a [`Controller`] can be configured to:
//!
//! * report each pass, and each [step](Step) a reconciler divides its work
//!   into, to a [`ReconcileObserver`], for metrics. With the `prometheus`
//!   feature, [`PrometheusMetrics`] exports these
//!   as Prometheus metrics. See the [`observe`] module.
//! * publish a Kubernetes event on the resource whenever reconciling it
//!   fails, through an [`EventRecorder`](events::EventRecorder), so that
//!   `kubectl describe` explains why it is not converging. Reconcilers can
//!   publish events of their own through the same recorder. See the
//!   [`events`] module.
//!
//! A single observer and event recorder are typically shared by every
//! controller in a process:
//!
//! ```no_run
//! # use std::collections::BTreeSet;
//! # use std::sync::{Arc, Mutex};
//! # use k8s_openapi::api::core::v1::Pod;
//! # use kube::{Config, Client};
//! # use kube_runtime::controller::Action;
//! # use kube_runtime::watcher;
//! # #[derive(Default, Clone)]
//! # struct PodCounter {
//! #     pods: Arc<Mutex<BTreeSet<String>>>,
//! # }
//! # #[async_trait::async_trait]
//! # impl k8s_controller::Context for PodCounter {
//! #     type Resource = Pod;
//! #     type Error = kube::Error;
//! #     async fn apply(
//! #         &self,
//! #         client: Client,
//! #         pod: &Self::Resource,
//! #         _metadata: &mut k8s_controller::TraceMetadata,
//! #     ) -> Result<Option<Action>, Self::Error> { todo!() }
//! # }
//! # struct MyObserver;
//! # impl k8s_controller::ReconcileObserver for MyObserver {}
//! # async fn foo() {
//! # let kube_client = Client::try_from(Config::infer().await.unwrap()).unwrap();
//! use k8s_controller::events::{EventRecorder, Reporter};
//!
//! let observer: Arc<dyn k8s_controller::ReconcileObserver> = Arc::new(MyObserver);
//! let events = Arc::new(EventRecorder::new(
//!     kube_client.clone(),
//!     Reporter {
//!         controller: "pod-counter.example.com".to_owned(),
//!         instance: std::env::var("HOSTNAME").ok(),
//!     },
//! ));
//! let controller = k8s_controller::Controller::namespaced_all(
//!     kube_client,
//!     PodCounter::default(),
//!     watcher::Config::default(),
//! )
//! .with_name("pod-counter")
//! .with_observer(Arc::clone(&observer))
//! .with_event_recorder(Arc::clone(&events));
//! controller.run().await;
//! # }
//! ```
//!
//! Within a reconciler, steps are started from the [`TraceMetadata`] passed
//! to [`Context::apply`] and [`Context::cleanup`]:
//!
//! ```no_run
//! # use kube::Client;
//! # use kube_runtime::controller::Action;
//! # use k8s_openapi::api::core::v1::ConfigMap;
//! # async fn sync_deployment() -> Result<(), kube::Error> { Ok(()) }
//! # async fn deployment_ready() -> Result<bool, kube::Error> { Ok(true) }
//! # struct Widgets;
//! # #[async_trait::async_trait]
//! # impl k8s_controller::Context for Widgets {
//! #     type Resource = ConfigMap;
//! #     type Error = kube::Error;
//! async fn apply(
//!     &self,
//!     client: Client,
//!     widget: &Self::Resource,
//!     metadata: &mut k8s_controller::TraceMetadata,
//! ) -> Result<Option<Action>, Self::Error> {
//!     let step = metadata.step("deployment");
//!     sync_deployment().await?;
//!     if !deployment_ready().await? {
//!         step.finish(k8s_controller::Outcome::Waiting);
//!         return Ok(Some(Action::requeue(std::time::Duration::from_secs(5))));
//!     }
//!     step.finish(k8s_controller::Outcome::Completed);
//!     Ok(None)
//! }
//! # }
//! ```

mod controller;
pub mod events;
mod leader_election;
pub mod observe;
#[cfg(feature = "prometheus")]
mod prometheus;
#[cfg(test)]
mod test_util;

pub use controller::{Context, Controller, Error};
pub use leader_election::LeaderElection;
pub use observe::{
    Outcome, Phase, ReconcileObserver, ReconcileRecord, Step, StepRecord, TraceMetadata,
};
#[cfg(feature = "prometheus")]
pub use prometheus::PrometheusMetrics;
