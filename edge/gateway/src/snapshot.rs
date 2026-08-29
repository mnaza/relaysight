use std::time::Duration;

use anyhow::{Context, anyhow};
use digest_auth::AuthContext;
use reqwest::{Client, StatusCode, header::WWW_AUTHENTICATE};
use url::Url;

pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

pub async fn fetch(
    client: &Client,
    raw_url: &str,
    configured_username: Option<&str>,
    configured_password: Option<&str>,
) -> anyhow::Result<Snapshot> {
    let mut url = Url::parse(raw_url).context("parse ONVIF snapshot URL")?;
    let embedded_username = (!url.username().is_empty()).then(|| url.username().to_owned());
    let embedded_password = url.password().map(ToOwned::to_owned);
    if embedded_username.is_some() {
        url.set_username("")
            .map_err(|_| anyhow!("clear snapshot username"))?;
    }
    if embedded_password.is_some() {
        url.set_password(None)
            .map_err(|_| anyhow!("clear snapshot password"))?;
    }

    let username = configured_username
        .map(ToOwned::to_owned)
        .or(embedded_username);
    let password = configured_password
        .map(ToOwned::to_owned)
        .or(embedded_password)
        .unwrap_or_default();
    let credentials = username
        .filter(|value| !value.is_empty())
        .map(|value| (value, password));

    let send_once = |authorization: Option<String>| {
        let mut request = client.get(url.clone()).timeout(Duration::from_secs(8));
        if let Some(value) = authorization {
            request = request.header(reqwest::header::AUTHORIZATION, value);
        } else if let Some((username, password)) = credentials.as_ref() {
            request = request.basic_auth(username, Some(password));
        }
        request
    };

    let mut response = send_once(None)
        .send()
        .await
        .context("request camera snapshot")?;
    if response.status() == StatusCode::UNAUTHORIZED
        && let (Some((username, password)), Some(challenge)) = (
            credentials.as_ref(),
            response
                .headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
        )
        && challenge
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("digest ")
    {
        let mut prompt = digest_auth::parse(challenge).context("parse camera Digest challenge")?;
        let mut request_uri = url.path().to_owned();
        if let Some(query) = url.query() {
            request_uri.push('?');
            request_uri.push_str(query);
        }
        let context = AuthContext::new(username, password, &request_uri);
        let authorization = prompt
            .respond(&context)
            .context("answer camera Digest challenge")?
            .to_string();
        response = send_once(Some(authorization))
            .send()
            .await
            .context("request camera snapshot with Digest auth")?;
    }

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("camera snapshot returned HTTP {status}");
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("image/jpeg")
        .split(';')
        .next()
        .unwrap_or("image/jpeg")
        .trim()
        .to_owned();
    if !content_type.starts_with("image/") {
        anyhow::bail!("camera snapshot returned unexpected content type {content_type}");
    }
    let bytes = response.bytes().await.context("read camera snapshot")?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        anyhow::bail!("camera snapshot exceeds {} bytes", MAX_SNAPSHOT_BYTES);
    }
    if bytes.is_empty() {
        anyhow::bail!("camera snapshot is empty");
    }
    Ok(Snapshot {
        bytes: bytes.to_vec(),
        content_type,
    })
}
