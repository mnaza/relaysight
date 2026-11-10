//! An OpenID Connect provider that authenticates nobody.
//!
//! A real provider needs an account somewhere and a browser with a human in
//! it, which is why most OIDC integrations are tested against a mock of the
//! code that talks to them rather than against the protocol. This is the
//! protocol: discovery, authorize, token and JWKS, with a real RS256
//! signature over a real ID token.
//!
//! It authenticates nobody — whoever asks is whoever they said they were.
//! That is the point of it, and the reason it must never be reachable from
//! anywhere that matters.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{Query, State},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
use serde::{Deserialize, Serialize};

/// Ways a provider can be wrong.
///
/// A client that has only ever met an honest provider has never had its
/// checks tested, and every one of these is a token somebody has been handed
/// in earnest by a provider that was misconfigured, or by somebody standing
/// where the provider should have been.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quirk {
    #[default]
    Honest,
    /// An ID token minted for somebody else's client.
    WrongAudience,
    /// An ID token claiming to come from another issuer.
    WrongIssuer,
    /// An ID token that expired an hour ago.
    Expired,
    /// Signed with a key that is not the one in the JWKS, under the kid that
    /// is: the shape of somebody forging a token.
    OtherKey,
    /// Honest, and saying so: `email_verified` is false. A provider that lets
    /// somebody type an address they do not own says this, and a client that
    /// ignores it lets them sign in as whoever owns that address.
    UnverifiedEmail,
}

/// What a running provider is, for whoever needs to point at it.
pub struct FakeProvider {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone)]
struct ProviderState {
    issuer: String,
    client_id: String,
    client_secret: String,
    encoding: Arc<EncodingKey>,
    modulus: String,
    exponent: String,
    key_id: String,
    /// Codes handed out, and who they stand for.
    codes: Arc<Mutex<HashMap<String, Issued>>>,
    /// The email every login is for, unless the request names another.
    default_email: String,
    quirk: Quirk,
}

#[derive(Clone)]
struct Issued {
    email: String,
    nonce: Option<String>,
}

#[derive(Serialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    response_types_supported: Vec<String>,
    subject_types_supported: Vec<String>,
    id_token_signing_alg_values_supported: Vec<String>,
}

#[derive(Deserialize)]
struct AuthorizeQuery {
    redirect_uri: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    /// Not part of OIDC: how a test says who is signing in.
    #[serde(default)]
    login_hint: Option<String>,
}

#[derive(Deserialize)]
struct TokenForm {
    code: String,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
}

#[derive(Serialize)]
struct Tokens {
    access_token: String,
    token_type: String,
    expires_in: u64,
    id_token: String,
}

#[derive(Serialize)]
struct Claims {
    iss: String,
    aud: String,
    sub: String,
    email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email_verified: Option<bool>,
    exp: u64,
    iat: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
}

/// Start an honest one on the given port. `0` takes whatever the OS gives.
pub async fn start(port: u16, default_email: &str) -> std::io::Result<FakeProvider> {
    start_as(port, default_email, Quirk::Honest).await
}

/// Start one that is wrong in a chosen way.
pub async fn start_as(
    port: u16,
    default_email: &str,
    quirk: Quirk,
) -> std::io::Result<FakeProvider> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let issuer = format!("http://{}", listener.local_addr()?);

    // A key per run: nothing here is worth keeping, and a committed private
    // key is a thing somebody eventually uses in earnest.
    let mut rng = rand_compat::thread_rng();
    let key = RsaPrivateKey::new(&mut rng, 2048).expect("generate a key");
    let public = key.to_public_key();
    // The key it signs with is the key it publishes, unless it is the sort of
    // provider that does not.
    let signing = if quirk == Quirk::OtherKey {
        RsaPrivateKey::new(&mut rng, 2048).expect("generate another key")
    } else {
        key
    };
    let der = signing.to_pkcs1_der().expect("serialise the key");
    let encoding = EncodingKey::from_rsa_der(der.as_bytes());

    let state = ProviderState {
        issuer: issuer.clone(),
        client_id: "relaysight-test".into(),
        client_secret: "relaysight-test-secret".into(),
        encoding: Arc::new(encoding),
        modulus: BASE64URL.encode(public.n().to_bytes_be()),
        exponent: BASE64URL.encode(public.e().to_bytes_be()),
        key_id: "fake-1".into(),
        codes: Arc::new(Mutex::new(HashMap::new())),
        default_email: default_email.to_owned(),
        quirk,
    };
    let provider = FakeProvider {
        issuer: issuer.clone(),
        client_id: state.client_id.clone(),
        client_secret: state.client_secret.clone(),
    };

    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/jwks", get(jwks))
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(provider)
}

async fn discovery(State(state): State<ProviderState>) -> Json<Discovery> {
    Json(Discovery {
        issuer: state.issuer.clone(),
        authorization_endpoint: format!("{}/authorize", state.issuer),
        token_endpoint: format!("{}/token", state.issuer),
        jwks_uri: format!("{}/jwks", state.issuer),
        response_types_supported: vec!["code".into()],
        subject_types_supported: vec!["public".into()],
        id_token_signing_alg_values_supported: vec!["RS256".into()],
    })
}

/// Authenticate nobody and send them back with a code.
async fn authorize(
    State(state): State<ProviderState>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let code = format!("code-{}", uuid_like());
    state.codes.lock().expect("codes").insert(
        code.clone(),
        Issued {
            email: query
                .login_hint
                .unwrap_or_else(|| state.default_email.clone()),
            nonce: query.nonce,
        },
    );
    let joiner = if query.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut back = format!("{}{joiner}code={code}", query.redirect_uri);
    if let Some(returned) = query.state {
        back.push_str(&format!("&state={returned}"));
    }
    Redirect::to(&back).into_response()
}

async fn token(
    State(state): State<ProviderState>,
    axum::extract::Form(form): axum::extract::Form<TokenForm>,
) -> Response {
    if form.client_id.as_deref() != Some(state.client_id.as_str())
        || form.client_secret.as_deref() != Some(state.client_secret.as_str())
    {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let Some(issued) = state.codes.lock().expect("codes").remove(&form.code) else {
        // A code is good once. Reusing one is either a mistake or somebody
        // replaying somebody else's.
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_grant"})),
        )
            .into_response();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock")
        .as_secs();
    let claims = Claims {
        iss: match state.quirk {
            Quirk::WrongIssuer => "https://somebody-else.example".into(),
            _ => state.issuer.clone(),
        },
        aud: match state.quirk {
            Quirk::WrongAudience => "another-application".into(),
            _ => state.client_id.clone(),
        },
        sub: format!("sub-{}", issued.email),
        email: issued.email,
        // Absent unless there is something to say: a provider that does not
        // send it at all is the common case, and this is the one that does.
        email_verified: (state.quirk == Quirk::UnverifiedEmail).then_some(false),
        exp: match state.quirk {
            Quirk::Expired => now.saturating_sub(3_600),
            _ => now + 300,
        },
        iat: now,
        nonce: issued.nonce,
    };
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(state.key_id.clone());
    let id_token =
        jsonwebtoken::encode(&header, &claims, &state.encoding).expect("sign the id token");
    Json(Tokens {
        access_token: "fake-access-token".into(),
        token_type: "Bearer".into(),
        expires_in: 300,
        id_token,
    })
    .into_response()
}

async fn jwks(State(state): State<ProviderState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": state.key_id,
            "n": state.modulus,
            "e": state.exponent,
        }]
    }))
}

/// Enough uniqueness for a code that lives for one round trip.
fn uuid_like() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock")
        .subsec_nanos();
    format!("{nanos:09}{:04}", std::process::id() % 10_000)
}

/// `rsa` wants an `OsRng` from the `rand` version it was built against, and
/// pulling that version in directly is the one dependency this crate would
/// otherwise need.
mod rand_compat {
    pub fn thread_rng() -> rsa::rand_core::OsRng {
        rsa::rand_core::OsRng
    }
}
