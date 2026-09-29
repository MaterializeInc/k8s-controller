//! A minimal in-process stand-in for the Kubernetes API server, for tests.

use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex};

use kube::Client;
use kube::client::Body;
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub body: Value,
    /// The body returned in response.
    pub response: Value,
}

impl RecordedRequest {
    pub fn created_name(&self) -> String {
        self.response["metadata"]["name"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[derive(Default)]
struct State {
    requests: Vec<RecordedRequest>,
    next_patch_status: Option<u16>,
    hang_next: bool,
    created: usize,
}

/// Accepts every request, recording it. Creates respond with the submitted
/// object, named from its `generateName`; patches respond with a stub object.
#[derive(Clone, Default)]
pub struct MockApiServer {
    state: Arc<Mutex<State>>,
}

impl MockApiServer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Makes the next PATCH request fail with the given HTTP status.
    pub fn fail_next_patch(&self, code: u16) {
        self.state.lock().unwrap().next_patch_status = Some(code);
    }

    /// Makes the next request never receive a response. It is not recorded.
    pub fn hang_next_request(&self) {
        self.state.lock().unwrap().hang_next = true;
    }

    pub fn client(&self) -> Client {
        let state = Arc::clone(&self.state);
        let service = tower::service_fn(move |req: http::Request<Body>| {
            let state = Arc::clone(&state);
            async move {
                if std::mem::take(&mut state.lock().unwrap().hang_next) {
                    std::future::pending::<()>().await;
                }
                let method = req.method().to_string();
                let path = req.uri().path().to_owned();
                let bytes = req.into_body().collect_bytes().await.unwrap();
                let body: Value = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };

                let mut state = state.lock().unwrap();
                let (status, response) = match method.as_str() {
                    "POST" => {
                        state.created += 1;
                        let mut response = body.clone();
                        let prefix = response["metadata"]["generateName"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        response["metadata"]["name"] =
                            Value::String(format!("{prefix}{}", state.created));
                        (201, response)
                    }
                    "PATCH" => match state.next_patch_status.take() {
                        Some(code) => (
                            code,
                            json!({
                                "apiVersion": "v1",
                                "kind": "Status",
                                "status": "Failure",
                                "message": "injected failure",
                                "reason": "Injected",
                                "code": code,
                            }),
                        ),
                        None => (
                            200,
                            json!({
                                "apiVersion": "events.k8s.io/v1",
                                "kind": "Event",
                                "metadata": {"name": path.rsplit('/').next().unwrap()},
                                "eventTime": null,
                            }),
                        ),
                    },
                    other => panic!("unexpected {other} request to {path}"),
                };
                state.requests.push(RecordedRequest {
                    method,
                    path,
                    body,
                    response: response.clone(),
                });

                let response = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&response).unwrap()))
                    .unwrap();
                Ok::<_, Infallible>(response)
            }
        });
        Client::new(service, "default")
    }
}

/// Runs `fut` to completion on a current-thread runtime, as the kube client
/// requires one to be running.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(fut)
}
