//! A minimal in-process stand-in for the Kubernetes API server, for tests.

use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex};

use kube::Client;
use kube::client::Body;
use serde_json::{Value, json};

use crate::events::is_dns_subdomain;

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

/// Accepts every request with a valid name, recording it. Creates respond
/// with the submitted object, named from its `generateName` if it has no
/// name; patches respond with a stub object.
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
                    "POST" => match created_name(&body["metadata"], state.created + 1) {
                        Ok(name) => {
                            state.created += 1;
                            let mut response = body.clone();
                            response["metadata"]["name"] = Value::String(name);
                            (201, response)
                        }
                        Err(message) => (
                            422,
                            json!({
                                "apiVersion": "v1",
                                "kind": "Status",
                                "status": "Failure",
                                "message": message,
                                "reason": "Invalid",
                                "code": 422,
                            }),
                        ),
                    },
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

/// The name the API server gives an object created with `metadata`, or why
/// it rejects the object. Like the API server, this validates `generateName`
/// as a DNS subdomain except for a trailing `-`, and then appends `suffix`.
/// [`is_dns_subdomain`] is at least as strict as the API server, so a name
/// accepted here is accepted there too.
fn created_name(metadata: &Value, suffix: usize) -> Result<String, String> {
    if let Some(name) = metadata["name"].as_str() {
        return if is_dns_subdomain(name) {
            Ok(name.to_owned())
        } else {
            Err(format!("metadata.name: Invalid value: {name:?}"))
        };
    }
    let Some(prefix) = metadata["generateName"].as_str() else {
        return Err("metadata.name: Required value".to_owned());
    };
    let masked = match prefix.strip_suffix('-') {
        Some(rest) => format!("{rest}a"),
        None => prefix.to_owned(),
    };
    if !is_dns_subdomain(&masked) {
        return Err(format!("metadata.generateName: Invalid value: {prefix:?}"));
    }
    Ok(format!("{prefix}{suffix}"))
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
