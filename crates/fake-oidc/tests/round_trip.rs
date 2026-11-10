//! The provider answers the four questions a client asks it.

#[tokio::test]
async fn discovery_names_endpoints_that_answer() {
    let provider = vms_fake_oidc::start(0, "someone@example.test")
        .await
        .expect("start a provider");
    let http = reqwest::Client::new();
    let discovery: serde_json::Value = http
        .get(format!(
            "{}/.well-known/openid-configuration",
            provider.issuer
        ))
        .send()
        .await
        .expect("ask for discovery")
        .json()
        .await
        .expect("read discovery");
    assert_eq!(discovery["issuer"].as_str(), Some(provider.issuer.as_str()));
    let jwks: serde_json::Value = http
        .get(discovery["jwks_uri"].as_str().expect("a jwks uri"))
        .send()
        .await
        .expect("ask for the keys")
        .json()
        .await
        .expect("read the keys");
    assert_eq!(jwks["keys"][0]["kty"].as_str(), Some("RSA"));
}
