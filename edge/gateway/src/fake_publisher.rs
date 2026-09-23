//! An RTMP publisher, in this process, for tests.
//!
//! `ffmpeg` and OBS are what will really push to the gateway, and neither
//! belongs in a test suite. This drives the real client half of `rml_rtmp`
//! over a real socket, so the handshake, the chunk stream and the AMF commands
//! are the ones a publisher sends.

use anyhow::{Context, anyhow};
use bytes::Bytes;
use rml_rtmp::{
    handshake::{Handshake, HandshakeProcessResult, PeerType},
    sessions::{
        ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult,
        PublishRequestType, StreamMetadata,
    },
    time::RtmpTimestamp,
};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// The `code` of an AMF `_error` response, if that is what this is.
fn error_code(values: &[rml_rtmp::rml_amf0::Amf0Value]) -> Option<String> {
    use rml_rtmp::rml_amf0::Amf0Value;
    values.iter().find_map(|value| {
        let Amf0Value::Object(fields) = value else {
            return None;
        };
        match fields.get("level") {
            Some(Amf0Value::Utf8String(level)) if level == "_error" => {}
            _ => return None,
        }
        match fields.get("code") {
            Some(Amf0Value::Utf8String(code)) => Some(code.clone()),
            _ => Some("unspecified".to_string()),
        }
    })
}

pub struct FakePublisher {
    socket: TcpStream,
    session: ClientSession,
    buffer: Vec<u8>,
}

impl FakePublisher {
    /// Connect and publish on `stream_key`, returning once the gateway has
    /// accepted — or an error saying it refused.
    pub async fn publish_to(address: SocketAddr, stream_key: &str) -> anyhow::Result<Self> {
        let mut socket = TcpStream::connect(address)
            .await
            .context("connect to the RTMP listener")?;
        let mut handshake = Handshake::new(PeerType::Client);
        let start = handshake
            .generate_outbound_p0_and_p1()
            .map_err(|error| anyhow!("handshake start: {error:?}"))?;
        socket.write_all(&start).await?;

        let mut buffer = vec![0_u8; 16 * 1024];
        let leftover = loop {
            let read = socket.read(&mut buffer).await?;
            if read == 0 {
                return Err(anyhow!("the gateway closed during the handshake"));
            }
            match handshake
                .process_bytes(&buffer[..read])
                .map_err(|error| anyhow!("handshake: {error:?}"))?
            {
                HandshakeProcessResult::InProgress { response_bytes } => {
                    socket.write_all(&response_bytes).await?;
                }
                HandshakeProcessResult::Completed {
                    response_bytes,
                    remaining_bytes,
                } => {
                    socket.write_all(&response_bytes).await?;
                    break remaining_bytes;
                }
            }
        };

        let config = ClientSessionConfig::new();
        let (session, results) =
            ClientSession::new(config).map_err(|error| anyhow!("client session: {error:?}"))?;
        let mut publisher = Self {
            socket,
            session,
            buffer,
        };
        publisher.send(results).await?;
        publisher.feed(&leftover).await?;

        let connect = publisher
            .session
            .request_connection("live".to_string())
            .map_err(|error| anyhow!("connect request: {error:?}"))?;
        publisher.send(vec![connect]).await?;
        publisher
            .wait_for(|event| matches!(event, ClientSessionEvent::ConnectionRequestAccepted))
            .await
            .context("the gateway did not accept the connection")?;

        let publish = publisher
            .session
            .request_publishing(stream_key.to_string(), PublishRequestType::Live)
            .map_err(|error| anyhow!("publish request: {error:?}"))?;
        publisher.send(vec![publish]).await?;
        publisher
            .wait_for(|event| matches!(event, ClientSessionEvent::PublishRequestAccepted))
            .await
            .context("the gateway did not accept the publisher")?;
        Ok(publisher)
    }

    pub async fn send_metadata(&mut self, width: u32, height: u32, fps: f32) -> anyhow::Result<()> {
        let mut metadata = StreamMetadata::new();
        metadata.video_width = Some(width);
        metadata.video_height = Some(height);
        metadata.video_frame_rate = Some(fps);
        metadata.video_codec_id = Some(7);
        let result = self
            .session
            .publish_metadata(&metadata)
            .map_err(|error| anyhow!("metadata: {error:?}"))?;
        self.send(vec![result]).await
    }

    /// One FLV video tag, as an encoder writes it.
    pub async fn send_video(&mut self, tag: Bytes, timestamp_ms: u32) -> anyhow::Result<()> {
        let result = self
            .session
            .publish_video_data(tag, RtmpTimestamp::new(timestamp_ms), false)
            .map_err(|error| anyhow!("video data: {error:?}"))?;
        self.send(vec![result]).await
    }

    pub async fn stop(mut self) -> anyhow::Result<()> {
        let results = self
            .session
            .stop_publishing()
            .map_err(|error| anyhow!("stop publishing: {error:?}"))?;
        self.send(results).await?;
        self.socket.shutdown().await?;
        Ok(())
    }

    async fn send(&mut self, results: Vec<ClientSessionResult>) -> anyhow::Result<()> {
        for result in results {
            if let ClientSessionResult::OutboundResponse(packet) = result {
                self.socket.write_all(&packet.bytes).await?;
            }
        }
        Ok(())
    }

    /// Feed bytes from the gateway into the session, returning the events.
    async fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<Vec<ClientSessionEvent>> {
        let results = self
            .session
            .handle_input(bytes)
            .map_err(|error| anyhow!("gateway response rejected: {error:?}"))?;
        let mut events = Vec::new();
        let mut outbound = Vec::new();
        for result in results {
            match result {
                ClientSessionResult::OutboundResponse(packet) => {
                    outbound.push(ClientSessionResult::OutboundResponse(packet))
                }
                ClientSessionResult::RaisedEvent(event) => events.push(event),
                ClientSessionResult::UnhandleableMessageReceived(_) => {}
            }
        }
        self.send(outbound).await?;
        Ok(events)
    }

    async fn wait_for(
        &mut self,
        mut matches: impl FnMut(&ClientSessionEvent) -> bool,
    ) -> anyhow::Result<()> {
        loop {
            let mut buffer = std::mem::take(&mut self.buffer);
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.socket.read(&mut buffer),
            )
            .await
            .context("the gateway said nothing")??;
            if read == 0 {
                self.buffer = buffer;
                return Err(anyhow!("the gateway closed the connection"));
            }
            let events = self.feed(&buffer[..read]).await?;
            self.buffer = buffer;
            for event in &events {
                match event {
                    ClientSessionEvent::ConnectionRequestRejected { description } => {
                        return Err(anyhow!("the gateway refused: {description}"));
                    }
                    // How a refused publish comes back: the client half has no
                    // state for an `_error` on a transaction it is waiting on,
                    // so it hands the AMF object straight over.
                    ClientSessionEvent::UnhandleableOnStatusCode { code } => {
                        return Err(anyhow!("the gateway refused: {code}"));
                    }
                    ClientSessionEvent::UnknownTransactionResultReceived {
                        additional_values,
                        ..
                    } => {
                        if let Some(code) = error_code(additional_values) {
                            return Err(anyhow!("the gateway refused: {code}"));
                        }
                    }
                    _ => {}
                }
                if matches(event) {
                    return Ok(());
                }
            }
        }
    }
}
