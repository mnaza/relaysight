//! TLS to the relay: which certificates to trust, and how to connect.

use std::io;
use std::path::Path;
use std::sync::Arc;

use futures::future::BoxFuture;
use rtc::ice::url::{SchemeType, Url};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use super::bridge::{Connector, RelayStream};

/// The public roots, plus the certificates in `extra_ca_file` when one is given.
/// A file that cannot be used comes back as a problem to log, and the public
/// roots are used alone.
pub(crate) fn root_store(extra_ca_file: Option<&Path>) -> (RootCertStore, Option<String>) {
    let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let Some(path) = extra_ca_file else {
        return (roots, None);
    };
    let certificates: Vec<CertificateDer<'static>> = match CertificateDer::pem_file_iter(path) {
        Ok(iter) => match iter.collect::<Result<Vec<_>, _>>() {
            Ok(certificates) => certificates,
            Err(err) => {
                return (
                    roots,
                    Some(format!("cannot parse {}: {err}", path.display())),
                );
            }
        },
        Err(err) => {
            return (
                roots,
                Some(format!("cannot read {}: {err}", path.display())),
            );
        }
    };
    if certificates.is_empty() {
        return (
            roots,
            Some(format!("{} holds no certificates", path.display())),
        );
    }
    let mut extra = RootCertStore::empty();
    for certificate in certificates {
        if let Err(err) = extra.add(certificate) {
            return (
                roots,
                Some(format!(
                    "{} holds an unusable certificate: {err}",
                    path.display()
                )),
            );
        }
    }
    roots.extend(extra.roots);
    (roots, None)
}

/// A client configuration on the `ring` provider trusting `roots`.
pub(crate) fn client_config(roots: RootCertStore) -> Arc<ClientConfig> {
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("the ring provider supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    Arc::new(config)
}

/// Connections for a stream URL: TLS for `turns:`, plain TCP for `turn:…?transport=tcp`.
/// The TLS server name is the URL's host; there is no way to skip verification.
pub(crate) fn connector(url: &Url, config: Arc<ClientConfig>) -> Connector {
    let host = url.host.clone();
    let port = url.port;
    let tls = (url.scheme == SchemeType::Turns).then(|| TlsConnector::from(config));
    Arc::new(
        move || -> BoxFuture<'static, io::Result<Box<dyn RelayStream>>> {
            let host = host.clone();
            let tls = tls.clone();
            Box::pin(async move {
                let tcp = TcpStream::connect((host.as_str(), port)).await?;
                let Some(tls) = tls else {
                    return Ok(Box::new(tcp) as Box<dyn RelayStream>);
                };
                let name = ServerName::try_from(host)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
                let stream = tls.connect(name, tcp).await?;
                Ok(Box::new(stream) as Box<dyn RelayStream>)
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A throwaway self-signed CA certificate (no key anywhere), valid until 2126.
    const TEST_CA: &str = "-----BEGIN CERTIFICATE-----
MIIBoDCCAUegAwIBAgIUAp0CUGzMvRlLe7SWcI6QKM6jCzowCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwScmVsYXlzaWdodC10ZXN0LWNhMCAXDTI2MDkxNDA1NTg1MloY
DzIxMjYwODIxMDU1ODUyWjAdMRswGQYDVQQDDBJyZWxheXNpZ2h0LXRlc3QtY2Ew
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASxW/d+8eoofuyOxvy7Es6UgM+XLnFW
hPWQxPcty1oO3i9/VNvwQxxHXTZABrxmY/5sU7EypmLJGZNE7rTGbVUpo2MwYTAd
BgNVHQ4EFgQUUko059P0ORUiPvwAWBtuTbxRtOkwHwYDVR0jBBgwFoAUUko059P0
ORUiPvwAWBtuTbxRtOkwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAgQw
CgYIKoZIzj0EAwIDRwAwRAIgE+DTSNoj1ICrf7+2MxddDpE6zZXXiEfEW7Uz48mJ
RSoCIGUqpC9lwzfdqANSbGctm8IfiHvJnTP8MHgwDUVG9WeY
-----END CERTIFICATE-----
";

    fn public_roots() -> usize {
        webpki_roots::TLS_SERVER_ROOTS.len()
    }

    #[test]
    fn without_a_ca_file_only_the_public_roots_are_trusted() {
        let (roots, problem) = root_store(None);
        assert_eq!(roots.len(), public_roots());
        assert_eq!(problem, None);
    }

    #[test]
    fn a_ca_file_adds_its_certificate() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), TEST_CA).unwrap();
        let (roots, problem) = root_store(Some(file.path()));
        assert_eq!(problem, None);
        assert_eq!(roots.len(), public_roots() + 1);
    }

    #[test]
    fn a_missing_ca_file_is_reported_and_the_public_roots_still_apply() {
        let (roots, problem) = root_store(Some(Path::new("/nonexistent/relaysight-turn-ca.pem")));
        assert!(
            problem
                .expect("a missing file is a problem")
                .contains("cannot read")
        );
        assert_eq!(roots.len(), public_roots());
    }

    #[test]
    fn a_ca_file_without_certificates_is_reported() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "not a certificate\n").unwrap();
        let (roots, problem) = root_store(Some(file.path()));
        assert!(
            problem
                .expect("an empty file is a problem")
                .contains("holds no certificates")
        );
        assert_eq!(roots.len(), public_roots());
    }

    #[tokio::test]
    async fn a_tcp_url_connects_without_tls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = Url::parse_url(&format!("turn:127.0.0.1:{port}?transport=tcp")).unwrap();
        let connect = connector(&url, client_config(root_store(None).0));

        let (accepted, stream) = tokio::join!(listener.accept(), connect());
        let (mut server, _) = accepted.unwrap();
        let mut stream = stream.unwrap();
        stream.write_all(b"turn").await.unwrap();
        let mut arrived = [0u8; 4];
        server.read_exact(&mut arrived).await.unwrap();
        assert_eq!(&arrived, b"turn", "plain TCP must carry bytes unchanged");
    }
}
