//! The API side of the gateway, for tests.
//!
//! The command loop is the gateway's whole reason to exist: the cloud never
//! dials in, so everything the product does arrives through this poll. Nothing
//! exercised it, because it needs an API to talk to. This is a small one — it
//! hands out queued commands, records completions, and can be told to reject a
//! poll so the loop's error handling is reachable.

use std::sync::Arc;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::RwLock,
};

#[derive(Default)]
pub struct Seen {
    /// Completion bodies, in the order they arrived.
    pub completions: Vec<serde_json::Value>,
    /// Authorization headers seen on any request.
    pub tokens: Vec<String>,
    pub polls: u32,
    /// Presigned upload requests, and the blob PUTs that followed them.
    pub uploads: u32,
    /// Recordings filed by the gateway itself, with nobody having asked —
    /// a schedule keeping a window.
    pub filed_recordings: Vec<serde_json::Value>,
    /// How many times a plugin was asked to look at a camera.
    pub analyses: u32,
    pub blobs: u32,
    /// Bearer tokens seen specifically on /storage/uploads requests — the
    /// API refuses uploads without one, so a test can pin that they go out.
    pub upload_tokens: Vec<String>,
    /// Enrollment attempts, and the enrollment tokens already claimed —
    /// the real API burns each token on first claim, so the fake does too.
    pub enrolls: u32,
}

pub struct FakeControlPlane {
    pub url: String,
    pub seen: Arc<RwLock<Seen>>,
    /// What `GET /api/v1/gateways/{id}/sources` answers.
    pub sources: Arc<RwLock<Vec<serde_json::Value>>>,
    /// What `GET /api/v1/gateways/{id}/recording-policies` answers.
    pub policies: Arc<RwLock<Vec<serde_json::Value>>>,
}

impl FakeControlPlane {
    /// `commands` are handed out one per poll, in order; afterwards every poll
    /// answers `null`. `reject_first_polls` answers 401 that many times before
    /// serving anything, so the loop's rejection path can be reached.
    pub async fn start(commands: Vec<serde_json::Value>, reject_first_polls: u32) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(RwLock::new(Seen::default()));
        let recorder = Arc::clone(&seen);
        let queue = Arc::new(RwLock::new(commands.into_iter().collect::<Vec<_>>()));
        let claimed = Arc::new(RwLock::new(std::collections::HashSet::<String>::new()));
        let sources = Arc::new(RwLock::new(Vec::<serde_json::Value>::new()));
        let served_sources = Arc::clone(&sources);
        let policies = Arc::new(RwLock::new(Vec::<serde_json::Value>::new()));
        let served_policies = Arc::clone(&policies);
        let self_url = url.clone();

        tokio::spawn(async move {
            let mut rejected = 0;
            let claimed = Arc::clone(&claimed);
            let sources = served_sources;
            let policies = served_policies;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 65536];
                let Ok(read) = socket.read(&mut buf).await else {
                    continue;
                };
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let first = request.lines().next().unwrap_or("").to_owned();

                if let Some(token) = request.lines().find_map(|l| {
                    l.strip_prefix("authorization: Bearer ")
                        .or_else(|| l.strip_prefix("Authorization: Bearer "))
                }) {
                    recorder.write().await.tokens.push(token.trim().to_owned());
                }

                let response = if first.starts_with("GET") && first.contains("/recording-policies")
                {
                    json_response(
                        &serde_json::Value::Array(policies.read().await.clone()).to_string(),
                    )
                } else if first.starts_with("GET") && first.contains("/sources") {
                    json_response(
                        &serde_json::Value::Array(sources.read().await.clone()).to_string(),
                    )
                } else if first.starts_with("GET") && first.contains("/commands/next") {
                    recorder.write().await.polls += 1;
                    if rejected < reject_first_polls {
                        rejected += 1;
                        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                    } else {
                        let next = queue.write().await.pop();
                        let body = next.map(|v| v.to_string()).unwrap_or_else(|| "null".into());
                        json_response(&body)
                    }
                } else if first.starts_with("POST") && first.contains("/complete") {
                    if let Some(body) = request.split_once("\r\n\r\n").map(|(_, b)| b)
                        && let Ok(value) = serde_json::from_str::<serde_json::Value>(body)
                    {
                        recorder.write().await.completions.push(value);
                    }
                    json_response("{}")
                } else if first.starts_with("POST") && first.contains("/gateways/enroll") {
                    recorder.write().await.enrolls += 1;
                    let token = request
                        .split_once("\r\n\r\n")
                        .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
                        .and_then(|v| v["enrollment_token"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    let mut claimed_tokens = claimed.write().await;
                    if token.is_empty() || claimed_tokens.contains(&token) {
                        // The real API answers Gone for a burned token.
                        "HTTP/1.1 410 Gone\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_owned()
                    } else {
                        claimed_tokens.insert(token.clone());
                        json_response(
                            &serde_json::json!({
                                "gateway_token": format!("enrolled-{}", claimed_tokens.len()),
                                "entitlement": {
                                    "edition": "community", "plan": "community",
                                    "self_hosted": true, "managed": false,
                                    "camera_limit": null, "capabilities": [],
                                },
                                "customer_id": "cust-1", "customer_name": "Customer",
                                "site_id": "site-1", "site_name": "Site", "city": "Barcelona",
                            })
                            .to_string(),
                        )
                    }
                } else if first.starts_with("POST") && first.contains("/ai/analyze") {
                    recorder.write().await.analyses += 1;
                    json_response(
                        &serde_json::json!({
                            "plugin_id": "ai-demo",
                            "model": "fake-1",
                            "detections": [
                                {"label": "person", "confidence": 0.95, "bbox": null,
                                 "attributes": {}},
                                {"label": "cat", "confidence": 0.20, "bbox": null,
                                 "attributes": {}},
                            ],
                            "metadata": {},
                        })
                        .to_string(),
                    )
                } else if first.starts_with("POST") && first.contains("/recordings") {
                    if let Some(body) = request.split("\r\n\r\n").nth(1)
                        && let Ok(manifest) = serde_json::from_str::<serde_json::Value>(body.trim())
                    {
                        recorder.write().await.filed_recordings.push(manifest);
                    }
                    "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_owned()
                } else if first.starts_with("POST") && first.contains("/storage/uploads") {
                    // Point the presigned PUT back at this server so the upload
                    // completes without a second fake. Without this the record
                    // command always fails at the first object and the test
                    // would never reach the manifest it is meant to check.
                    let object_ref = format!("obj-{}", uuid::Uuid::new_v4());
                    {
                        let mut seen = recorder.write().await;
                        seen.uploads += 1;
                        if let Some(token) = request.lines().find_map(|l| {
                            l.strip_prefix("authorization: Bearer ")
                                .or_else(|| l.strip_prefix("Authorization: Bearer "))
                        }) {
                            seen.upload_tokens.push(token.trim().to_owned());
                        }
                    }
                    json_response(
                        &serde_json::json!({
                            "method": "PUT",
                            "url": format!("{self_url}/blob/{object_ref}"),
                            "headers": {},
                            "object_ref": object_ref,
                            "expires_at": chrono::Utc::now() + chrono::Duration::minutes(15),
                        })
                        .to_string(),
                    )
                } else if first.starts_with("PUT") {
                    recorder.write().await.blobs += 1;
                    "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                } else {
                    json_response("{}")
                };
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        Self {
            url,
            seen,
            sources,
            policies,
        }
    }

    /// Wait until at least `count` completions have been recorded.
    pub async fn wait_for_completions(
        &self,
        count: usize,
        timeout: std::time::Duration,
    ) -> Vec<serde_json::Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                let seen = self.seen.read().await;
                if seen.completions.len() >= count {
                    return seen.completions.clone();
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "only {} completions arrived, expected {count}",
                self.seen.read().await.completions.len()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}
