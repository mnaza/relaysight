//! Run the fake provider as a process, for `make check-sso`.
//!
//! It refuses to start without being told a port, because the one thing worse
//! than an identity provider that authenticates nobody is one that does so on
//! a port somebody else was using for something real.

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let Some(port) = std::env::args()
        .nth(1)
        .and_then(|raw| raw.parse::<u16>().ok())
    else {
        eprintln!("usage: vms-fake-oidc <port> [default-email]");
        eprintln!("this provider authenticates nobody; never run it anywhere that matters");
        std::process::exit(2);
    };
    let email = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "admin@localhost".into());
    let provider = vms_fake_oidc::start(port, &email).await?;
    println!("issuer={}", provider.issuer);
    println!("client_id={}", provider.client_id);
    println!("client_secret={}", provider.client_secret);
    println!("every login here is {email}");
    std::future::pending::<()>().await;
    Ok(())
}
