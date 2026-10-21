// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! An in-process, scripted HTTP origin for `test/compat`'s hand-written
//! TC-* integration tests (per the plan §3: run via `tokio::spawn`, not a
//! child process — only the fixture proxy itself needs to be a real OS
//! process for signals/restarts).

pub mod script;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use script::Scenario;

#[derive(Default, Clone)]
struct SharedState {
    scenarios: Arc<RwLock<HashMap<String, Scenario>>>,
    fetch_counts: Arc<RwLock<HashMap<String, usize>>>,
}

pub struct MockOrigin {
    pub addr: SocketAddr,
    state: SharedState,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockOrigin {
    /// Bind an ephemeral port and start serving in the background.
    pub async fn spawn() -> std::io::Result<Self> {
        let state = SharedState::default();
        // axum 0.7 wildcard syntax (`/*rest`) — 0.8 renamed this to
        // `/{*rest}`; keep this in sync if the axum pin is ever bumped.
        let app = Router::new()
            .route("/*path", any(handle))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock-origin server failed");
        });

        Ok(MockOrigin {
            addr,
            state,
            _handle: handle,
        })
    }

    /// Script the response for a given request path (leading slash
    /// included, e.g. `/tc-compat-unsized-01`).
    pub fn set_scenario(&self, path: impl Into<String>, scenario: Scenario) {
        self.state
            .scenarios
            .write()
            .unwrap()
            .insert(path.into(), scenario);
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// How many times this path has actually been fetched from the origin
    /// -- TC-COMPAT.STREAMING-01's own concern: a follower attached to an
    /// in-flight writer must never cause a second fetch.
    pub fn fetch_count(&self, path: impl Into<String>) -> usize {
        self.state
            .fetch_counts
            .read()
            .unwrap()
            .get(&path.into())
            .copied()
            .unwrap_or(0)
    }
}

async fn handle(
    State(state): State<SharedState>,
    AxumPath(path): AxumPath<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let full_path = format!("/{path}");
    let scenario = state.scenarios.read().unwrap().get(&full_path).cloned();

    let Some(scenario) = scenario else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(format!("no scenario scripted for {full_path}")))
            .unwrap();
    };

    *state
        .fetch_counts
        .write()
        .unwrap()
        .entry(full_path.clone())
        .or_insert(0) += 1;

    // A conditional request whose validator still matches is answered
    // `304`, which the cache merges into the stored response rather than
    // replacing it.
    if let Some(etag) = &scenario.etag {
        let matches = headers
            .get(axum::http::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|candidate| candidate.trim() == etag));
        if matches {
            // Only the 304's own header set: a cache merges these into the
            // stored response, so a test controls exactly what changes.
            let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
            for (name, value) in &scenario.revalidated_headers {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::try_from(name.as_str()),
                    HeaderValue::try_from(value.as_str()),
                ) {
                    builder = builder.header(name, value);
                }
            }
            return builder.body(Body::empty()).unwrap();
        }
    }

    let mut builder =
        Response::builder().status(StatusCode::from_u16(scenario.status).unwrap_or(StatusCode::OK));
    for (name, value) in &scenario.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            builder = builder.header(name, value);
        }
    }
    if scenario.declare_length {
        builder = builder.header("content-length", scenario.total_len().to_string());
    }

    let chunks = scenario.chunks.clone();
    let stream = async_stream::stream! {
        for chunk in chunks {
            if !chunk.delay_before.is_zero() {
                tokio::time::sleep(chunk.delay_before).await;
            }
            yield Ok::<_, std::io::Error>(chunk.data);
        }
    };

    builder.body(Body::from_stream(stream)).unwrap()
}
