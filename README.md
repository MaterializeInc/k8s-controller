# kube-controller

This crate implements a lightweight framework around
[`kube_runtime::Controller`] which provides a simpler interface for common
controller patterns. To use it, you define the data that your controller is
going to operate over, and implement the `Context` trait on that struct:

```rust
#[derive(Default, Clone)]
struct PodCounter {
    pods: Arc<Mutex<BTreeSet<String>>>,
}

impl PodCounter {
    fn pod_count(&self) -> usize {
        let mut pods = self.pods.lock().unwrap();
        pods.len()
    }
}

#[async_trait::async_trait]
impl kube_controller::Context for PodCounter {
    type Resource = Pod;
    type Error = kube::Error;

    const FINALIZER_NAME: &'static str = "example.com/pod-counter";

    async fn apply(
        &self,
        client: Client,
        pod: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let mut pods = self.pods.lock().unwrap();
        pods.insert(pod.meta().uid.as_ref().unwrap().clone());
        Ok(None)
    }

    async fn cleanup(
        &self,
        client: Client,
        pod: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let mut pods = self.pods.lock().unwrap();
        pods.remove(pod.meta().uid.as_ref().unwrap());
        Ok(None)
    }
}
```

Then you can run it against your Kubernetes cluster by creating a
[`Controller`]:

```rust
let kube_config = Config::infer().await.unwrap();
let kube_client = Client::try_from(kube_config).unwrap();
let context = PodCounter::default();
let controller = kube_controller::Controller::namespaced_all(
    kube_client,
    context.clone(),
    ListParams::default(),
);
task::spawn(controller.run());

loop {
    println!("{} pods running", context.pod_count());
    sleep(Duration::from_secs(1));
}
```
