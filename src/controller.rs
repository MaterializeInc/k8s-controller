use std::collections::BTreeMap;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::FutureExt;
use futures::stream::StreamExt;
use kube::api::{Api, ListParams};
use kube::core::{ClusterResourceScope, NamespaceResourceScope};
use kube::{Client, Resource, ResourceExt};
use kube_runtime::controller::Action;
use kube_runtime::finalizer::{finalizer, Event};
use rand::{thread_rng, Rng};
use tracing::{event, Level};

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
}

impl<Ctx: Context> Controller<Ctx>
where
    Ctx: Send + Sync + 'static,
    Ctx::Error: Send + Sync + 'static,
    Ctx::Resource: Send + Sync + 'static,
    Ctx::Resource: Clone + std::fmt::Debug + serde::Serialize,
    for<'de> Ctx::Resource: serde::Deserialize<'de>,
    <Ctx::Resource as Resource>::DynamicType:
        Eq + Clone + std::hash::Hash + std::default::Default + std::fmt::Debug + std::marker::Unpin,
{
    /// Creates a new controller for a namespaced resource using the given
    /// `client`. The `context` given determines the type of resource
    /// to watch (via the [`Context::Resource`] type provided as part of
    /// the trait implementation). The resources to be watched will be
    /// limited to resources in the given `namespace`. A [`ListParams`]
    /// can be given to limit the resources watched (for instance,
    /// `ListParams::default().labels("app=myapp")`).
    pub fn namespaced(client: Client, context: Ctx, namespace: &str, lp: ListParams) -> Self
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
            lp,
        );
        Self {
            client,
            make_api,
            controller,
            context,
        }
    }

    /// Creates a new controller for a namespaced resource using the given
    /// `client`. The `context` given determines the type of resource to
    /// watch (via the [`Context::Resource`] type provided as part of the
    /// trait implementation). The resources to be watched will not be
    /// limited by namespace. A [`ListParams`] can be given to limit the
    /// resources watched (for instance,
    /// `ListParams::default().labels("app=myapp")`).
    pub fn namespaced_all(client: Client, context: Ctx, lp: ListParams) -> Self
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
            lp,
        );
        Self {
            client,
            make_api,
            controller,
            context,
        }
    }

    /// Creates a new controller for a cluster-scoped resource using the
    /// given `client`. The `context` given determines the type of resource
    /// to watch (via the [`Context::Resource`] type provided as part of the
    /// trait implementation). A [`ListParams`] can be given to limit the
    /// resources watched (for instance,
    /// `ListParams::default().labels("app=myapp")`).
    pub fn cluster(client: Client, context: Ctx, lp: ListParams) -> Self
    where
        Ctx::Resource: Resource<Scope = ClusterResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |_: &Ctx::Resource| Api::<Ctx::Resource>::all(client.clone()))
        };
        let controller = kube_runtime::controller::Controller::new(
            Api::<Ctx::Resource>::all(client.clone()),
            lp,
        );
        Self {
            client,
            make_api,
            controller,
            context,
        }
    }

    /// Run the controller. This method will not return. The [`Context`]
    /// given to the constructor will have its [`apply`](Context::apply)
    /// method called when a resource is created or updated, and its
    /// [`cleanup`](Context::cleanup) method called when a resource is about
    /// to be deleted.
    pub async fn run(self) {
        let Self {
            client,
            make_api,
            controller,
            context,
        } = self;
        let backoffs = Arc::new(Mutex::new(BTreeMap::new()));
        let backoffs = &backoffs;
        controller
            .run(
                |resource, context| {
                    let uid = resource.uid().unwrap();
                    let backoffs = Arc::clone(backoffs);
                    context
                        ._reconcile(client.clone(), make_api(&resource), resource)
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
            .for_each(|reconciliation_result| async move {
                let dynamic_type = Default::default();
                let kind = Ctx::Resource::kind(&dynamic_type).into_owned();
                match reconciliation_result {
                    Ok(resource) => {
                        event!(
                            Level::INFO,
                            resource_name = %resource.0.name,
                            controller = Ctx::FINALIZER_NAME,
                            "{} reconciliation successful.",
                            kind
                        );
                    }
                    Err(err) => event!(
                        Level::ERROR,
                        err = %err,
                        source = err.source(),
                        controller = Ctx::FINALIZER_NAME,
                        "{} reconciliation error.",
                        kind
                    ),
                }
            })
            .await
    }
}

/// The [`Context`] trait should be implemented in order to provide callbacks
/// for events that happen to resources watched by a [`Controller`].
#[cfg_attr(docs_rs, feature(async_fn_in_trait))]
#[cfg_attr(not(docs_rs), async_trait::async_trait)]
pub trait Context {
    /// The type of Kubernetes [resource](Resource) that will be watched by
    /// the [`Controller`] this context is passed to
    type Resource: Resource;
    /// The error type which will be returned by the [`apply`](Self::apply)
    /// and [`cleanup`](Self::apply) methods
    type Error: std::error::Error;

    /// The name to use for the finalizer. This must be unique across
    /// controllers - if multiple controllers with the same finalizer name
    /// run against the same resource, unexpected behavior can occur.
    const FINALIZER_NAME: &'static str;

    /// This method is called when a watched resource is created or updated.
    /// The [`Client`] used by the controller is passed in to allow making
    /// additional API requests, as is the resource which triggered this
    /// event. If this method returns `Some(action)`, the given action will
    /// be performed, otherwise if `None` is returned,
    /// [`success_action`](Self::success_action) will be called to find the
    /// action to perform.
    async fn apply(
        &self,
        client: Client,
        resource: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error>;

    /// This method is called when a watched resource is marked for deletion.
    /// The [`Client`] used by the controller is passed in to allow making
    /// additional API requests, as is the resource which triggered this
    /// event. If this method returns `Some(action)`, the given action will
    /// be performed, otherwise if `None` is returned,
    /// [`success_action`](Self::success_action) will be called to find the
    /// action to perform.
    async fn cleanup(
        &self,
        client: Client,
        resource: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error>;

    /// This method is called when a call to [`apply`](Self::apply) or
    /// [`cleanup`](Self::cleanup) returns `Ok(None)`. It should return the
    /// default [`Action`] to perform. The default implementation will
    /// requeue the event at a random time between 40 and 60 minutes in the
    /// future.
    fn success_action(&self, resource: &Self::Resource) -> Action {
        // use a better name for the parameter name in the docs
        let _resource = resource;

        Action::requeue(Duration::from_secs(thread_rng().gen_range(2400..3600)))
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
        err: &kube_runtime::finalizer::Error<Self::Error>,
        consecutive_errors: u32,
    ) -> Action {
        // use a better name for the parameter name in the docs
        let _resource = resource;
        let _err = err;

        let seconds = 2u64.pow(consecutive_errors.min(7) + 1);
        Action::requeue(Duration::from_millis(
            thread_rng().gen_range((seconds * 500)..(seconds * 1000)),
        ))
    }

    #[doc(hidden)]
    async fn _reconcile(
        self: Arc<Self>,
        client: Client,
        api: Api<Self::Resource>,
        resource: Arc<Self::Resource>,
    ) -> Result<Action, kube_runtime::finalizer::Error<Self::Error>>
    where
        Self: Send + Sync + 'static,
        Self::Error: Send + Sync + 'static,
        Self::Resource: Send + Sync + 'static,
        Self::Resource: Clone + std::fmt::Debug + serde::Serialize,
        for<'de> Self::Resource: serde::Deserialize<'de>,
        <Self::Resource as Resource>::DynamicType: Eq
            + Clone
            + std::hash::Hash
            + std::default::Default
            + std::fmt::Debug
            + std::marker::Unpin,
    {
        let dynamic_type = Default::default();
        let kind = Self::Resource::kind(&dynamic_type).into_owned();
        let mut ran = false;
        let res = finalizer(
            &api,
            Self::FINALIZER_NAME,
            Arc::clone(&resource),
            |event| async {
                ran = true;
                event!(
                    Level::INFO,
                    resource_name = %resource.name_unchecked().as_str(),
                    controller = Self::FINALIZER_NAME,
                    "Reconciling {} ({}).",
                    kind,
                    match event {
                        Event::Apply(_) => "apply",
                        Event::Cleanup(_) => "cleanup",
                    }
                );
                let action = match event {
                    Event::Apply(resource) => {
                        let action = self.apply(client, &resource).await?;
                        if let Some(action) = action {
                            action
                        } else {
                            self.success_action(&resource)
                        }
                    }
                    Event::Cleanup(resource) => self
                        .cleanup(client, &resource)
                        .await?
                        .unwrap_or_else(Action::await_change),
                };
                Ok(action)
            },
        )
        .await;
        if !ran {
            event!(
                Level::INFO,
                resource_name = %resource.name_unchecked().as_str(),
                controller = Self::FINALIZER_NAME,
                "Reconciling {} ({}).",
                kind,
                if resource.meta().deletion_timestamp.is_some() {
                    "delete"
                } else {
                    "init"
                }
            );
        }
        res
    }
}
