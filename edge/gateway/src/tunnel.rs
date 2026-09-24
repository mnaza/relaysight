//! Letting somebody reach one device's own web page, through this gateway.
//!
//! An installer standing at a site can open the NVR's settings page. Nobody
//! outside can, short of forwarding a port or running a VPN — which is how
//! sites end up with an NVR on the public internet.
//!
//! This is the smallest thing that helps: the control plane hands over one
//! request at a time, the gateway performs it on the camera network, and the
//! answer goes back. It is not a VPN, it is not a video path, and it is off
//! unless this gateway was told to allow it. A site that says no cannot be
//! tunnelled into even by a control plane somebody else has taken over.
//! See docs/TUNNEL.md.

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tracing::{debug, info, warn};

use crate::Config;

/// A device page, its stylesheet and its images. Anything larger is not a
/// settings page, and this is not how video leaves a site.
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

/// How long one request to a device on the local network may take.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether this site allows it at all.
pub fn enabled() -> bool {
    std::env::var("TUNNEL_ENABLED").is_ok_and(|value| value == "true")
}

/// Hold a long poll, perform what comes back, answer, repeat.
pub async fn serve(config: Config, client: reqwest::Client) {
    info!("device tunnelling is enabled on this gateway");
    let next = format!(
        "{}/api/v1/gateways/{}/tunnel/next",
        config.api_url.trim_end_matches('/'),
        config.gateway_id
    );
    let answer_to = format!(
        "{}/api/v1/gateways/{}/tunnel/answer",
        config.api_url.trim_end_matches('/'),
        config.gateway_id
    );
    loop {
        let call = match client
            .get(&next)
            .bearer_auth(&config.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                response.json::<Option<vms_domain::TunnelCall>>().await.ok()
            }
            Ok(response) => {
                warn!(status = %response.status(), "the tunnel poll was refused");
                None
            }
            Err(error) => {
                debug!(%error, "the tunnel poll did not complete");
                None
            }
        };
        let Some(Some(call)) = call else {
            // Nothing to do, or nothing this gateway could understand. Pause
            // rather than spin: an idle tunnel should cost nothing.
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };

        let answer = perform(&client, &call).await;
        if let Err(error) = client
            .post(&answer_to)
            .bearer_auth(&config.token)
            .json(&answer)
            .send()
            .await
        {
            warn!(%error, "the tunnel answer did not reach the control plane");
        }
    }
}

/// Fetch one thing from the device, and say what happened either way.
async fn perform(
    client: &reqwest::Client,
    call: &vms_domain::TunnelCall,
) -> vms_domain::TunnelAnswer {
    let url = format!("http://{}:{}{}", call.host, call.port, call.path);
    info!(url = %url, "tunnelling a request to a device");
    let failed = |error: String| vms_domain::TunnelAnswer {
        id: call.id.clone(),
        status: 0,
        content_type: None,
        body_base64: String::new(),
        error: Some(error),
    };

    let response = match client.get(&url).timeout(DEVICE_TIMEOUT).send().await {
        Ok(response) => response,
        Err(error) => {
            return failed(format!(
                "{}:{} did not answer: {error}",
                call.host, call.port
            ));
        }
    };
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => return failed(format!("the device stopped mid-answer: {error}")),
    };
    if body.len() > MAX_ANSWER_BYTES {
        // Truncating would hand back a broken page and call it a page.
        return failed(format!(
            "that answer is {} bytes; the tunnel carries pages, not video",
            body.len()
        ));
    }
    vms_domain::TunnelAnswer {
        id: call.id.clone(),
        status,
        content_type,
        body_base64: BASE64.encode(&body),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(host: &str, port: u16, path: &str) -> vms_domain::TunnelCall {
        vms_domain::TunnelCall {
            id: "call-1".into(),
            session_id: "session-1".into(),
            host: host.into(),
            port,
            method: "GET".into(),
            path: path.into(),
        }
    }

    /// A device with a web page, and a big file it would rather not send.
    async fn fake_device() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = vec![0_u8; 4096];
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let (content_type, body) = if request.contains("/huge") {
                        ("application/octet-stream", vec![0_u8; MAX_ANSWER_BYTES + 1])
                    } else {
                        ("text/html; charset=utf-8", b"<h1>NVR</h1>".to_vec())
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn a_device_page_comes_back_as_the_device_wrote_it() {
        let port = fake_device().await;
        let answer = perform(&reqwest::Client::new(), &call("127.0.0.1", port, "/")).await;
        assert_eq!(answer.status, 200);
        assert_eq!(
            answer.content_type.as_deref(),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            String::from_utf8(BASE64.decode(answer.body_base64).unwrap()).unwrap(),
            "<h1>NVR</h1>"
        );
        assert!(answer.error.is_none());
    }

    #[tokio::test]
    async fn something_too_big_is_refused_rather_than_truncated() {
        // Half a page is a broken page presented as a page. And this is not
        // how video leaves a site.
        let port = fake_device().await;
        let answer = perform(&reqwest::Client::new(), &call("127.0.0.1", port, "/huge")).await;
        let error = answer.error.expect("a refusal");
        assert!(error.contains("pages, not video"), "{error}");
        assert!(answer.body_base64.is_empty());
    }

    #[tokio::test]
    async fn a_device_that_is_not_there_says_so_rather_than_hanging() {
        let answer = perform(&reqwest::Client::new(), &call("127.0.0.1", 9, "/")).await;
        let error = answer.error.expect("a refusal");
        assert!(error.contains("did not answer"), "{error}");
    }

    #[test]
    fn tunnelling_is_off_unless_the_site_said_otherwise() {
        // The safety valve: a site that says nothing cannot be tunnelled into
        // by anybody, including a control plane somebody else has taken over.
        unsafe { std::env::remove_var("TUNNEL_ENABLED") };
        assert!(!enabled());
        unsafe { std::env::set_var("TUNNEL_ENABLED", "yes") };
        assert!(!enabled(), "only the word this gateway documents counts");
        unsafe { std::env::set_var("TUNNEL_ENABLED", "true") };
        assert!(enabled());
        unsafe { std::env::remove_var("TUNNEL_ENABLED") };
    }
}
