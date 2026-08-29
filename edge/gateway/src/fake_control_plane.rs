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
    pub blobs: u32,
}

pub struct FakeControlPlane {
    pub url: String,
    pub seen: Arc<RwLock<Seen>>,
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
        let self_url = url.clone();

        tokio::spawn(async move {
            let mut rejected = 0;
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

                let response = if first.starts_with("GET") && first.contains("/commands/next") {
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
                } else if first.starts_with("POST") && first.contains("/storage/uploads") {
                    // Point the presigned PUT back at this server so the upload
                    // completes without a second fake. Without this the record
                    // command always fails at the first object and the test
                    // would never reach the manifest it is meant to check.
                    let object_ref = format!("obj-{}", uuid::Uuid::new_v4());
                    recorder.write().await.uploads += 1;
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

        Self { url, seen }
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
