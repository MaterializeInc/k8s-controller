//! Integration tests for the finalizer logic in
//! [`k8s_controller::Controller`]. These exercise the cleanup-requeue
//! semantics against a real Kubernetes API server (a dedicated `kind`
//! cluster named `k8s-controller-test`).
//!
//! Requires the `kind` and `kubectl`-compatible network to be available.
//! The first run will provision the cluster (slow, ~30s); subsequent runs
//! reuse it. Tear down manually with:
//!
//! ```text
//! kind delete cluster --name k8s-controller-test
//! ```

use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use json_patch::jsonptr::PointerBuf;
use json_patch::{PatchOperation, ReplaceOperation};
use k8s_controller::{Context, Controller};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace};
use kube::api::{Api, DeleteParams, ObjectMeta, Patch, PatchParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config, Resource, ResourceExt};
use kube_runtime::controller::Action;
use kube_runtime::watcher;
use tokio::process::Command;
use tokio::time::sleep;

const CLUSTER_NAME: &str = "k8s-controller-test";
const FINALIZER: &str = "test.k8s-controller/finalizer";
/// A finalizer owned by some other controller. Used to exercise the
/// "finalizers list already non-empty" paths.
const FOREIGN_FINALIZER: &str = "other.example/keep";

// NB: a `kube::Client` spawns a background worker on the runtime it is built
// on. Each `#[tokio::test]` gets its own runtime, so the client must be built
// (and used) within the same test - sharing one client across tests via a
// `static` lets the builder's runtime shut down underneath the others,
// surfacing as `Service(Closed)`.
async fn build_client() -> Client {
    ensure_cluster().await;
    let out = Command::new("kind")
        .args(["get", "kubeconfig", "--name", CLUSTER_NAME])
        .output()
        .await
        .expect("invoke `kind get kubeconfig`");
    assert!(
        out.status.success(),
        "`kind get kubeconfig` failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let yaml = String::from_utf8(out.stdout).expect("kubeconfig is utf-8");
    let kc = Kubeconfig::from_yaml(&yaml).expect("parse kubeconfig");
    let cfg = Config::from_custom_kubeconfig(kc, &KubeConfigOptions::default())
        .await
        .expect("build kube Config");
    Client::try_from(cfg).expect("build kube Client")
}

async fn ensure_cluster() {
    // Tests run concurrently, each on its own runtime. Serialize the
    // check-then-create so two tests can't both observe a missing cluster and
    // race to `kind create cluster` (the loser fails with "already exists").
    // A `tokio::sync::Mutex` (not `std`) is required because the guard is held
    // across `.await`.
    static LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
    let _guard = LOCK.lock().await;

    let out = Command::new("kind")
        .args(["get", "clusters"])
        .output()
        .await
        .expect("invoke `kind get clusters`");
    let listed = String::from_utf8_lossy(&out.stdout);
    if listed.lines().any(|l| l.trim() == CLUSTER_NAME) {
        return;
    }
    let status = Command::new("kind")
        .args([
            "create",
            "cluster",
            "--name",
            CLUSTER_NAME,
            "--wait",
            "120s",
        ])
        .status()
        .await
        .expect("invoke `kind create cluster`");
    assert!(status.success(), "`kind create cluster` failed");
}

async fn create_namespace(client: &Client, name: &str) {
    let api: Api<Namespace> = Api::all(client.clone());
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(name.into()),
            ..Default::default()
        },
        ..Default::default()
    };
    api.create(&PostParams::default(), &ns)
        .await
        .expect("create namespace");
}

async fn create_configmap(client: &Client, namespace: &str, name: &str) {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.into()),
            ..Default::default()
        },
        ..Default::default()
    };
    api.create(&PostParams::default(), &cm)
        .await
        .expect("create configmap");
}

async fn create_configmap_with_finalizers(
    client: &Client,
    namespace: &str,
    name: &str,
    finalizers: &[&str],
) {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.into()),
            finalizers: Some(finalizers.iter().map(|f| f.to_string()).collect()),
            ..Default::default()
        },
        ..Default::default()
    };
    api.create(&PostParams::default(), &cm)
        .await
        .expect("create configmap");
}

/// Replace the object's finalizers with an empty list so the API server can
/// finish deleting it. Used to unblock teardown of objects that carry a
/// finalizer we don't otherwise remove (e.g. [`FOREIGN_FINALIZER`]).
async fn clear_finalizers(api: &Api<ConfigMap>, name: &str) {
    let patch = json_patch::Patch(vec![PatchOperation::Replace(ReplaceOperation {
        path: PointerBuf::from_str("/metadata/finalizers").expect("valid pointer"),
        value: Vec::<String>::new().into(),
    })]);
    let _ = api
        .patch::<ConfigMap>(name, &PatchParams::default(), &Patch::Json(patch))
        .await;
}

#[derive(Clone)]
struct TestContext {
    cleanup_calls: Arc<AtomicU32>,
    /// Number of cleanup invocations that should return `Ok(Some(Action))`
    /// before the next one returns `Ok(None)`. With `0`, the first call
    /// returns `None` and the finalizer is removed immediately.
    requeue_count: u32,
}

#[async_trait]
impl Context for TestContext {
    type Resource = ConfigMap;
    type Error = kube::Error;

    const FINALIZER_NAME: Option<&'static str> = Some(FINALIZER);

    async fn apply(
        &self,
        _client: Client,
        _resource: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        Ok(None)
    }

    async fn cleanup(
        &self,
        _client: Client,
        _resource: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let prev = self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
        if prev >= self.requeue_count {
            Ok(None)
        } else {
            Ok(Some(Action::requeue(Duration::from_millis(500))))
        }
    }
}

async fn wait_for<F, Fut>(desc: &str, timeout: Duration, mut poll: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let started = Instant::now();
    while started.elapsed() < timeout {
        if poll().await {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {desc}");
}

async fn is_not_found(api: &Api<ConfigMap>, name: &str) -> bool {
    matches!(
        api.get(name).await,
        Err(kube::Error::Api(ref e)) if e.code == 404,
    )
}

fn unique_namespace(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos:x}")
}

/// `cleanup` returns `Ok(None)` on the first call: the finalizer should be
/// removed and the ConfigMap should be fully deleted by the API server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_none_removes_finalizer_and_deletes() {
    let client = build_client().await;
    let ns = unique_namespace("ftest-none");
    create_namespace(&client, &ns).await;

    let ctx = TestContext {
        cleanup_calls: Arc::new(AtomicU32::new(0)),
        requeue_count: 0,
    };
    let controller =
        Controller::namespaced(client.clone(), ctx.clone(), &ns, watcher::Config::default());
    let task = tokio::spawn(controller.run());

    let cm_name = "cm";
    create_configmap(&client, &ns, cm_name).await;

    let api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    wait_for("finalizer to be added", Duration::from_secs(20), || async {
        match api.get(cm_name).await {
            Ok(o) => o.finalizers().iter().any(|f| f == FINALIZER),
            Err(_) => false,
        }
    })
    .await;

    api.delete(cm_name, &DeleteParams::default())
        .await
        .expect("delete cm");

    wait_for(
        "configmap to be deleted",
        Duration::from_secs(20),
        || async { is_not_found(&api, cm_name).await },
    )
    .await;

    assert_eq!(
        ctx.cleanup_calls.load(Ordering::SeqCst),
        1,
        "cleanup should run exactly once when it returns Ok(None) immediately",
    );

    task.abort();
    let ns_api: Api<Namespace> = Api::all(client.clone());
    let _ = ns_api.delete(&ns, &DeleteParams::default()).await;
}

/// `cleanup` returns `Ok(Some(Action))` several times before returning
/// `Ok(None)`. The finalizer must remain in place across all `Some`
/// returns - this is the bug the fix targets - and only be removed once
/// `cleanup` finally returns `Ok(None)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_some_keeps_finalizer_until_none() {
    let client = build_client().await;
    let ns = unique_namespace("ftest-some");
    create_namespace(&client, &ns).await;

    let requeue_count = 4;
    let ctx = TestContext {
        cleanup_calls: Arc::new(AtomicU32::new(0)),
        requeue_count,
    };
    let controller =
        Controller::namespaced(client.clone(), ctx.clone(), &ns, watcher::Config::default());
    let task = tokio::spawn(controller.run());

    let cm_name = "cm";
    create_configmap(&client, &ns, cm_name).await;

    let api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    wait_for("finalizer to be added", Duration::from_secs(20), || async {
        match api.get(cm_name).await {
            Ok(o) => o.finalizers().iter().any(|f| f == FINALIZER),
            Err(_) => false,
        }
    })
    .await;

    api.delete(cm_name, &DeleteParams::default())
        .await
        .expect("delete cm");

    // Wait for cleanup to be entered at least once so we know the
    // controller has observed the deletion timestamp.
    wait_for(
        "cleanup to run at least once",
        Duration::from_secs(15),
        || async { ctx.cleanup_calls.load(Ordering::SeqCst) >= 1 },
    )
    .await;

    // While cleanup is still requeuing, the object should remain present
    // with both deletionTimestamp and our finalizer. Poll the assertion
    // for a short window to make sure we observe the "still pending" state
    // and not an intermediate gap.
    let observed_pending_until = Instant::now() + Duration::from_millis(800);
    while Instant::now() < observed_pending_until {
        let obj = api
            .get(cm_name)
            .await
            .expect("configmap must persist while cleanup is requeuing");
        assert!(
            obj.meta().deletion_timestamp.is_some(),
            "deletionTimestamp should be set while cleanup is in progress",
        );
        assert!(
            obj.finalizers().iter().any(|f| f == FINALIZER),
            "finalizer must remain while cleanup returns Ok(Some(_)); \
             current finalizers: {:?}, calls so far: {}",
            obj.finalizers(),
            ctx.cleanup_calls.load(Ordering::SeqCst),
        );
        sleep(Duration::from_millis(100)).await;
    }

    // Eventually cleanup returns Ok(None) and the object is removed.
    wait_for(
        "configmap to finally be deleted",
        Duration::from_secs(30),
        || async { is_not_found(&api, cm_name).await },
    )
    .await;

    let final_calls = ctx.cleanup_calls.load(Ordering::SeqCst);
    assert!(
        final_calls > requeue_count,
        "expected more than {requeue_count} cleanup calls (one returning \
         Ok(None) on top of {requeue_count} requeues), got {final_calls}",
    );

    task.abort();
    let ns_api: Api<Namespace> = Api::all(client.clone());
    let _ = ns_api.delete(&ns, &DeleteParams::default()).await;
}

/// The object already carries another controller's finalizer when ours is
/// added. This exercises the "finalizers list non-empty" branch of the add
/// path: our finalizer must be appended without disturbing the existing one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_finalizer_appends_to_existing_list() {
    let client = build_client().await;
    let ns = unique_namespace("ftest-append");
    create_namespace(&client, &ns).await;

    let ctx = TestContext {
        cleanup_calls: Arc::new(AtomicU32::new(0)),
        requeue_count: 0,
    };
    let controller =
        Controller::namespaced(client.clone(), ctx.clone(), &ns, watcher::Config::default());
    let task = tokio::spawn(controller.run());

    let cm_name = "cm";
    create_configmap_with_finalizers(&client, &ns, cm_name, &[FOREIGN_FINALIZER]).await;

    let api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    wait_for(
        "our finalizer to be added",
        Duration::from_secs(20),
        || async {
            match api.get(cm_name).await {
                Ok(o) => o.finalizers().iter().any(|f| f == FINALIZER),
                Err(_) => false,
            }
        },
    )
    .await;

    let obj = api.get(cm_name).await.expect("get cm");
    assert!(
        obj.finalizers().iter().any(|f| f == FOREIGN_FINALIZER),
        "pre-existing finalizer must be preserved; got {:?}",
        obj.finalizers(),
    );
    assert_eq!(
        obj.finalizers().first().map(String::as_str),
        Some(FOREIGN_FINALIZER),
        "ours should be appended after the existing finalizer; got {:?}",
        obj.finalizers(),
    );
    assert_eq!(
        ctx.cleanup_calls.load(Ordering::SeqCst),
        0,
        "cleanup must not run while the object is not being deleted",
    );

    task.abort();
    clear_finalizers(&api, cm_name).await;
    let _ = api.delete(cm_name, &DeleteParams::default()).await;
    let ns_api: Api<Namespace> = Api::all(client.clone());
    let _ = ns_api.delete(&ns, &DeleteParams::default()).await;
}

/// Once an object is deleting and our finalizer is no longer present, the
/// `(finalizer absent, is_deleting)` arm must do nothing: never re-add our
/// finalizer, never run cleanup again.
///
/// Rather than racing to observe the controller act (or fail to act) on an
/// object it never touches - a silent no-op has no signal to wait on - we
/// drive the object *into* that arm through the controller's own actions and
/// watch the observable `cleanup_calls` counter. A foreign finalizer keeps the
/// object alive after we remove ours, so it lingers in exactly the deleting,
/// our-finalizer-absent state the arm handles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_without_our_finalizer_is_noop() {
    let client = build_client().await;
    let ns = unique_namespace("ftest-noop");
    create_namespace(&client, &ns).await;

    let cm_name = "cm";
    // Start with a foreign finalizer in place; the controller will append ours
    // alongside it. cleanup returns Ok(None) immediately, so the first cleanup
    // removes our finalizer.
    let ctx = TestContext {
        cleanup_calls: Arc::new(AtomicU32::new(0)),
        requeue_count: 0,
    };
    let controller =
        Controller::namespaced(client.clone(), ctx.clone(), &ns, watcher::Config::default());
    let task = tokio::spawn(controller.run());

    create_configmap_with_finalizers(&client, &ns, cm_name, &[FOREIGN_FINALIZER]).await;
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);

    // Wait for the controller to append our finalizer. This proves it is live
    // and reconciling this exact object.
    wait_for(
        "our finalizer to be added",
        Duration::from_secs(20),
        || async {
            match api.get(cm_name).await {
                Ok(o) => o.finalizers().iter().any(|f| f == FINALIZER),
                Err(_) => false,
            }
        },
    )
    .await;

    // Delete it. cleanup runs once, returns Ok(None), and the controller
    // removes our finalizer - but the foreign finalizer keeps the object
    // alive, deleting and without our finalizer: the arm under test.
    api.delete(cm_name, &DeleteParams::default())
        .await
        .expect("delete cm");
    wait_for(
        "our finalizer to be removed",
        Duration::from_secs(20),
        || async {
            match api.get(cm_name).await {
                Ok(o) => !o.finalizers().iter().any(|f| f == FINALIZER),
                Err(_) => false,
            }
        },
    )
    .await;
    assert_eq!(
        ctx.cleanup_calls.load(Ordering::SeqCst),
        1,
        "cleanup should have run exactly once before our finalizer was removed",
    );

    // Now the controller keeps observing a deleting object without our
    // finalizer. Hold the assertion across a window: it must not re-add our
    // finalizer and must not run cleanup again.
    let observe_until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < observe_until {
        let obj = api.get(cm_name).await.expect("cm should still exist");
        assert!(
            obj.meta().deletion_timestamp.is_some(),
            "object should still be mid-deletion (held by the foreign finalizer)",
        );
        assert!(
            !obj.finalizers().iter().any(|f| f == FINALIZER),
            "controller must not re-add our finalizer to a deleting object; got {:?}",
            obj.finalizers(),
        );
        assert_eq!(
            ctx.cleanup_calls.load(Ordering::SeqCst),
            1,
            "cleanup must not run again once our finalizer is gone",
        );
        sleep(Duration::from_millis(100)).await;
    }

    task.abort();
    clear_finalizers(&api, cm_name).await;
    let ns_api: Api<Namespace> = Api::all(client.clone());
    let _ = ns_api.delete(&ns, &DeleteParams::default()).await;
}
