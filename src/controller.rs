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

#[async_trait::async_trait]
pub trait Context {
    type Resource: Resource;
    type Error: std::error::Error;

    const FINALIZER_NAME: &'static str;

    async fn apply(
        &self,
        client: Client,
        resource: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error>;
    async fn cleanup(
        &self,
        client: Client,
        resource: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error>;

    async fn reconcile(
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
                        self.on_success(&resource, action)
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

    fn on_success(&self, _resource: &Self::Resource, action: Option<Action>) -> Action {
        action.unwrap_or_else(|| {
            Action::requeue(Duration::from_secs(thread_rng().gen_range(2400..3600)))
        })
    }
    fn on_error(
        self: Arc<Self>,
        _resource: Arc<Self::Resource>,
        _err: &kube_runtime::finalizer::Error<Self::Error>,
        consecutive_errors: u32,
    ) -> Action {
        let seconds = 2u64.pow(consecutive_errors.min(7) + 1);
        Action::requeue(Duration::from_millis(
            thread_rng().gen_range((seconds * 500)..(seconds * 1000)),
        ))
    }
}

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
    pub fn namespaced(namespace: &str, client: Client, lp: ListParams, context: Ctx) -> Self
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

    pub fn namespaced_all(client: Client, lp: ListParams, context: Ctx) -> Self
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

    pub fn cluster(client: Client, lp: ListParams, context: Ctx) -> Self
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
                        .reconcile(client.clone(), make_api(&resource), resource)
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
                    context.on_error(resource, err, consecutive_errors)
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
