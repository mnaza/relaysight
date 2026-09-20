//! One bridge: a loopback UDP socket webrtc-rs uses as its TURN server, carried
//! to the real relay over a stream.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use rtc::stun::error_code::{CODE_SERVER_ERROR, ErrorCodeAttribute};
use rtc::stun::message::{
    CLASS_ERROR_RESPONSE, CLASS_REQUEST, Message, MessageType, Setter, is_stun_message,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tracing::warn;

use super::framing::StreamSplitter;

/// Datagrams held for one source while its connection to the relay opens.
const QUEUE_WHILE_CONNECTING: usize = 16;
/// A relay that has not accepted the connection by then counts as unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection to the relay: TLS or plain TCP.
pub(crate) trait RelayStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> RelayStream for T {}

/// Opens a fresh connection to the relay.
pub(crate) type Connector =
    Arc<dyn Fn() -> BoxFuture<'static, io::Result<Box<dyn RelayStream>>> + Send + Sync>;

/// What a bridge has carried.
#[derive(Debug, Default)]
pub(crate) struct BridgeStats {
    up: AtomicU64,
    down: AtomicU64,
}

impl BridgeStats {
    /// Bytes carried from webrtc-rs to the relay.
    pub(crate) fn bytes_up(&self) -> u64 {
        self.up.load(Ordering::Relaxed)
    }

    /// Bytes carried from the relay back to webrtc-rs.
    pub(crate) fn bytes_down(&self) -> u64 {
        self.down.load(Ordering::Relaxed)
    }
}

/// A running bridge. Dropping it stops the bridge and closes its connections.
pub(crate) struct Bridge {
    local_addr: SocketAddr,
    stats: Arc<BridgeStats>,
    task: JoinHandle<()>,
}

impl Bridge {
    pub(crate) async fn spawn(connector: Connector) -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let local_addr = socket.local_addr()?;
        let stats = Arc::new(BridgeStats::default());
        let task = tokio::spawn(run(socket, connector, Arc::clone(&stats)));
        Ok(Self {
            local_addr,
            stats,
            task,
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub(crate) fn stats(&self) -> Arc<BridgeStats> {
        Arc::clone(&self.stats)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A bridge carries one peer: webrtc-rs gives it a single UDP socket. The
/// first source to speak is that peer, and anything else on this machine that
/// finds the loopback port is ignored rather than given a relay connection of
/// its own.
async fn run(socket: Arc<UdpSocket>, connector: Connector, stats: Arc<BridgeStats>) {
    // Owned by this task, so aborting it aborts the session with it.
    let mut sessions = JoinSet::new();
    let mut peer: Option<(SocketAddr, mpsc::Sender<Bytes>)> = None;
    let mut warned_about_strangers = false;
    let mut buf = vec![0u8; 65_536];
    loop {
        let (n, source) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
                ) =>
            {
                continue;
            }
            Err(err) => {
                warn!(error = %err, "TURN bridge socket failed");
                return;
            }
        };
        match &peer {
            Some((addr, _)) if *addr != source => {
                if !warned_about_strangers {
                    warned_about_strangers = true;
                    warn!(%source, "TURN bridge ignoring datagrams from another local socket");
                }
                continue;
            }
            None => {
                let (tx, rx) = mpsc::channel(QUEUE_WHILE_CONNECTING);
                sessions.spawn(session(
                    Arc::clone(&socket),
                    source,
                    Arc::clone(&connector),
                    rx,
                    Arc::clone(&stats),
                ));
                peer = Some((source, tx));
            }
            Some(_) => {}
        }
        // A full queue means the connection is still opening; webrtc-rs retransmits,
        // so dropping here is cheaper than buffering without bound. The session
        // never ends on its own, so a closed queue means the bridge is going away.
        if let Some((_, queue)) = &peer {
            let _ = queue.try_send(Bytes::copy_from_slice(&buf[..n]));
        }
    }
}

async fn session(
    socket: Arc<UdpSocket>,
    source: SocketAddr,
    connector: Connector,
    mut queue: mpsc::Receiver<Bytes>,
    stats: Arc<BridgeStats>,
) {
    let failure = match tokio::time::timeout(CONNECT_TIMEOUT, connector()).await {
        Ok(Ok(stream)) => pump(stream, &socket, source, &mut queue, &stats).await,
        Ok(Err(err)) => err.to_string(),
        Err(_) => format!("no connection within {CONNECT_TIMEOUT:?}"),
    };
    warn!(%source, reason = %failure, "TURN bridge cannot reach the relay; failing its requests");
    // Answer every request from now on, so webrtc-rs gives up on this relay at
    // once instead of retransmitting for seconds.
    while let Some(datagram) = queue.recv().await {
        if let Some(reply) = error_response(&datagram) {
            let _ = socket.send_to(&reply, source).await;
        }
    }
}

/// Carry messages both ways until the connection ends; returns why it ended.
async fn pump(
    stream: Box<dyn RelayStream>,
    socket: &UdpSocket,
    source: SocketAddr,
    queue: &mut mpsc::Receiver<Bytes>,
    stats: &BridgeStats,
) -> String {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut splitter = StreamSplitter::default();
    let mut buf = vec![0u8; 65_536];
    loop {
        tokio::select! {
            datagram = queue.recv() => {
                let Some(datagram) = datagram else {
                    return "the bridge stopped".into();
                };
                if let Err(err) = writer.write_all(&datagram).await {
                    return err.to_string();
                }
                stats.up.fetch_add(datagram.len() as u64, Ordering::Relaxed);
            }
            read = reader.read(&mut buf) => {
                let n = match read {
                    Ok(0) => return "the relay closed the connection".into(),
                    Ok(n) => n,
                    Err(err) => return err.to_string(),
                };
                splitter.push(&buf[..n]);
                loop {
                    match splitter.next_message() {
                        Ok(Some(message)) => {
                            stats.down.fetch_add(message.len() as u64, Ordering::Relaxed);
                            let _ = socket.send_to(&message, source).await;
                        }
                        Ok(None) => break,
                        Err(err) => return err.to_string(),
                    }
                }
            }
        }
    }
}

/// A STUN error response (500) to `datagram` when it is a STUN request.
pub(crate) fn error_response(datagram: &[u8]) -> Option<Vec<u8>> {
    if !is_stun_message(datagram) {
        return None;
    }
    let mut request = Message::new();
    request.raw = datagram.to_vec();
    request.decode().ok()?;
    if request.typ.class != CLASS_REQUEST {
        return None;
    }
    let mut response = Message::new();
    let setters: [Box<dyn Setter>; 3] = [
        Box::new(request.transaction_id),
        Box::new(MessageType::new(request.typ.method, CLASS_ERROR_RESPONSE)),
        Box::new(ErrorCodeAttribute {
            code: CODE_SERVER_ERROR,
            reason: b"the gateway cannot reach the relay".to_vec(),
        }),
    ];
    response.build(&setters).ok()?;
    Some(response.raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::stun::message::{Getter, METHOD_ALLOCATE, TransactionId};
    use tokio::net::{TcpListener, TcpStream};

    fn tcp_connector(addr: SocketAddr) -> Connector {
        Arc::new(
            move || -> BoxFuture<'static, io::Result<Box<dyn RelayStream>>> {
                Box::pin(async move {
                    let stream = TcpStream::connect(addr).await?;
                    Ok(Box::new(stream) as Box<dyn RelayStream>)
                })
            },
        )
    }

    fn allocate_request() -> Message {
        let mut request = Message::new();
        let setters: [Box<dyn Setter>; 2] = [
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)),
        ];
        request.build(&setters).unwrap();
        request
    }

    async fn next_datagram(socket: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(6), socket.recv_from(&mut buf))
            .await
            .expect("a datagram back from the bridge")
            .unwrap();
        buf[..n].to_vec()
    }

    #[tokio::test]
    async fn datagrams_cross_the_bridge_both_ways_whatever_the_read_boundaries() {
        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = Bridge::spawn(tcp_connector(relay.local_addr().unwrap()))
            .await
            .unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let request = allocate_request().raw;
        let channel_data = vec![0x40, 0x00, 0x00, 0x05, 1, 2, 3, 4, 5, 0, 0, 0];
        client.send_to(&request, bridge.local_addr()).await.unwrap();
        client
            .send_to(&channel_data, bridge.local_addr())
            .await
            .unwrap();

        let (mut upstream, _) = relay.accept().await.unwrap();
        let both = [request.clone(), channel_data.clone()].concat();
        let mut arrived = vec![0u8; both.len()];
        upstream.read_exact(&mut arrived).await.unwrap();
        assert_eq!(
            arrived, both,
            "datagrams must reach the relay byte for byte, in order"
        );

        // The relay sends both back, cut in the middle of the first message.
        upstream.write_all(&both[..7]).await.unwrap();
        upstream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        upstream.write_all(&both[7..]).await.unwrap();

        assert_eq!(
            next_datagram(&client).await,
            request,
            "the first message must come back whole"
        );
        assert_eq!(
            next_datagram(&client).await,
            channel_data,
            "ChannelData must come back whole, padding included"
        );
        assert_eq!(bridge.stats().bytes_up(), both.len() as u64);
        assert_eq!(bridge.stats().bytes_down(), both.len() as u64);
    }

    #[tokio::test]
    async fn a_relay_that_refuses_the_connection_turns_requests_into_errors_at_once() {
        let closed = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let bridge = Bridge::spawn(tcp_connector(closed)).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let request = allocate_request();
        client
            .send_to(&request.raw, bridge.local_addr())
            .await
            .unwrap();

        let mut reply = Message::new();
        reply.raw = next_datagram(&client).await;
        reply.decode().unwrap();
        assert_eq!(reply.transaction_id, request.transaction_id);
        assert_eq!(
            reply.typ,
            MessageType::new(METHOD_ALLOCATE, CLASS_ERROR_RESPONSE)
        );
        let mut code = ErrorCodeAttribute::default();
        code.get_from(&reply).unwrap();
        assert_eq!(
            code.code.0, 500,
            "the bridge answers a failed relay with 500"
        );

        // ChannelData has nothing to answer, so it is dropped rather than echoed.
        client
            .send_to(&[0x40, 0x00, 0x00, 0x00], bridge.local_addr())
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        assert!(
            tokio::time::timeout(Duration::from_millis(300), client.recv_from(&mut buf))
                .await
                .is_err(),
            "no reply to ChannelData"
        );
    }

    #[tokio::test]
    async fn a_second_source_gets_no_connection_of_its_own() {
        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = Bridge::spawn(tcp_connector(relay.local_addr().unwrap()))
            .await
            .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let request = allocate_request().raw;
        peer.send_to(&request, bridge.local_addr()).await.unwrap();
        let (mut upstream, _) = relay.accept().await.unwrap();
        let mut arrived = vec![0u8; request.len()];
        upstream.read_exact(&mut arrived).await.unwrap();

        // webrtc-rs gives a bridge exactly one socket. Anything else on this
        // machine spraying at the loopback port is not it.
        stranger
            .send_to(&allocate_request().raw, bridge.local_addr())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), relay.accept())
                .await
                .is_err(),
            "a second source must not open a second connection to the relay"
        );
        let mut buf = [0u8; 64];
        assert!(
            tokio::time::timeout(Duration::from_millis(300), upstream.read(&mut buf))
                .await
                .is_err(),
            "a second source's datagrams must not be carried"
        );
        assert_eq!(
            bridge.stats().bytes_up(),
            request.len() as u64,
            "only the peer's bytes count"
        );
    }

    #[tokio::test]
    async fn dropping_the_bridge_closes_its_connection_to_the_relay() {
        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge = Bridge::spawn(tcp_connector(relay.local_addr().unwrap()))
            .await
            .unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(&allocate_request().raw, bridge.local_addr())
            .await
            .unwrap();
        let (mut upstream, _) = relay.accept().await.unwrap();
        let mut request = [0u8; 20];
        upstream.read_exact(&mut request).await.unwrap();

        drop(bridge);
        let mut rest = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(2), upstream.read(&mut rest))
            .await
            .expect("the connection must close when the bridge is dropped")
            .unwrap();
        assert_eq!(n, 0);
    }
}
