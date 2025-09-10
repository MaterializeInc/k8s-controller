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
//!     const FINALIZER_NAME: &'static str = "example.com/pod-counter";
//!
//!     async fn apply(
//!         &self,
//!         client: Client,
//!         pod: &Self::Resource,
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
//! #     const FINALIZER_NAME: &'static str = "example.com/pod-counter";
//! #     async fn apply(
//! #         &self,
//! #         client: Client,
//! #         pod: &Self::Resource,
//! #     ) -> Result<Option<Action>, Self::Error> { todo!() }
//! #     async fn cleanup(
//! #         &self,
//! #         client: Client,
//! #         pod: &Self::Resource,
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

mod controller;

pub use controller::{Context, Controller};
