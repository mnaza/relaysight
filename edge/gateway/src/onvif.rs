use std::{collections::BTreeSet, time::Duration};

use anyhow::{Context, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{SecondsFormat, Utc};
use quick_xml::{Reader, events::Event};
use rand::RngCore;
use reqwest::Client;
use sha1::{Digest, Sha1};
use tokio::{
    net::UdpSocket,
    time::{Instant, timeout},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

const WS_DISCOVERY_ADDR: &str = "239.255.255.250:3702";
const SOAP_ENV: &str = "http://www.w3.org/2003/05/soap-envelope";
const DEVICE_WSDL: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA_WSDL: &str = "http://www.onvif.org/ver10/media/wsdl";

#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub endpoint_reference: Option<String>,
    pub xaddrs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceInformation {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    /// Parsed from the device response but not surfaced yet — the inventory/
    /// warranty view that consumes it is not built.
    #[allow(dead_code)]
    pub serial_number: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct OnvifProfile {
    pub token: String,
    pub name: Option<String>,
    pub encoding: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    pub bitrate_kbps: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CameraCandidate {
    pub camera_id: String,
    /// ONVIF device-service URL, kept for the PTZ/imaging calls that are not
    /// wired yet; discovery itself only needs the media profile.
    #[allow(dead_code)]
    pub device_service: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub profile: OnvifProfile,
    pub rtsp_uri: String,
    /// RTSP URI of `live_profile`. Equals `rtsp_uri` when they are the same profile.
    pub live_rtsp_uri: String,
    pub snapshot_uri: Option<String>,
}

async fn stream_uri(
    client: &reqwest::Client,
    media_service: &str,
    credentials: Option<&Credentials>,
    token: &str,
) -> anyhow::Result<String> {
    let body = format!(
        "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>TCP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
        xml_escape(token),
    );
    let xml = soap_post(
        client,
        media_service,
        credentials,
        MEDIA_WSDL,
        "GetStreamUri",
        &body,
    )
    .await?;
    parse_first_text(&xml, "Uri").ok_or_else(|| anyhow!("camera returned no RTSP Uri"))
}

/// Pixel ceiling for a live substream: 1280x720. Above this a profile is a second
/// main stream rather than a preview, and relaying it defeats the point.
const LIVE_PIXEL_CEILING: u64 = 1280 * 720;

fn pixels(p: &OnvifProfile) -> u64 {
    u64::from(p.width.unwrap_or(0)) * u64::from(p.height.unwrap_or(0))
}

/// True when the profile can travel the H.264 passthrough path. Cameras that omit
/// the encoding are treated as usable — excluding them would leave the substream
/// unused on hardware that supports it, and the RTSP session fails loudly anyway.
fn carries_h264(p: &OnvifProfile) -> bool {
    match &p.encoding {
        None => true,
        Some(e) => {
            let normalised: String = e
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_uppercase();
            normalised == "H264" || normalised == "AVC"
        }
    }
}

/// Choose the profile to record from and the profile to show live.
///
/// Recording wants detail, so it takes the highest resolution the camera offers.
/// Live view is relayed through TURN whenever the site cannot hold a direct path,
/// and that traffic is metered, so it takes the best substream that still saves
/// real bandwidth. See `docs/TURN-COSTS.md` — the difference is roughly 5.7x.
///
/// Both must be carriable by the H.264 passthrough path; neither the live nor the
/// archive writer transcodes. A camera offering nothing usable falls back to its
/// largest profile so the failure surfaces at RTSP rather than here.
fn select_profiles(mut profiles: Vec<OnvifProfile>) -> (OnvifProfile, OnvifProfile) {
    profiles.sort_by_key(|p| std::cmp::Reverse(pixels(p)));

    let main = profiles
        .iter()
        .find(|p| carries_h264(p))
        .cloned()
        .unwrap_or_else(|| profiles[0].clone());

    let candidates: Vec<&OnvifProfile> = profiles
        .iter()
        .filter(|p| p.token != main.token && carries_h264(p))
        .collect();

    // `candidates` is still ordered largest-first.
    let live = candidates
        .iter()
        .find(|p| pixels(p) <= LIVE_PIXEL_CEILING)
        .or_else(|| candidates.last())
        .map(|p| (*p).clone())
        .unwrap_or_else(|| main.clone());

    (main, live)
}

pub async fn discover(wait: Duration) -> anyhow::Result<Vec<DiscoveredDevice>> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("bind WS-Discovery UDP socket")?;
    socket.set_broadcast(true).context("enable UDP broadcast")?;

    let message_id = Uuid::new_v4();
    let probe = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"
 xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing"
 xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
 xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <e:Header>
    <w:MessageID>uuid:{message_id}</w:MessageID>
    <w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
    <w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
  </e:Header>
  <e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>
</e:Envelope>"#
    );

    socket
        .send_to(probe.as_bytes(), WS_DISCOVERY_ADDR)
        .await
        .context("send WS-Discovery probe")?;

    let deadline = Instant::now() + wait;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut devices = Vec::new();
    let mut seen = BTreeSet::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, socket.recv_from(&mut buffer)).await {
            Ok(Ok((size, source))) => {
                let xml = String::from_utf8_lossy(&buffer[..size]);
                match parse_probe_match(&xml) {
                    Ok(Some(device)) => {
                        let key = device
                            .endpoint_reference
                            .clone()
                            .or_else(|| device.xaddrs.first().cloned())
                            .unwrap_or_else(|| source.to_string());
                        if seen.insert(key) {
                            debug!(%source, xaddrs = ?device.xaddrs, "ONVIF device discovered");
                            devices.push(device);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => warn!(%source, %error, "failed to parse WS-Discovery reply"),
                }
            }
            Ok(Err(error)) => return Err(error).context("receive WS-Discovery reply"),
            Err(_) => break,
        }
    }
    Ok(devices)
}

pub async fn resolve_camera(
    client: &Client,
    device: &DiscoveredDevice,
    credentials: Option<&Credentials>,
) -> anyhow::Result<CameraCandidate> {
    let device_service = device
        .xaddrs
        .iter()
        .find(|url| url.starts_with("http://") || url.starts_with("https://"))
        .cloned()
        .ok_or_else(|| anyhow!("ONVIF device has no HTTP XAddr"))?;

    let info_xml = soap_post(
        client,
        &device_service,
        credentials,
        DEVICE_WSDL,
        "GetDeviceInformation",
        "<tds:GetDeviceInformation/>",
    )
    .await?;
    let info = parse_device_information(&info_xml)?;

    let capabilities_xml = soap_post(
        client,
        &device_service,
        credentials,
        DEVICE_WSDL,
        "GetCapabilities",
        "<tds:GetCapabilities><tds:Category>Media</tds:Category></tds:GetCapabilities>",
    )
    .await?;
    let media_service = parse_media_xaddr(&capabilities_xml)
        .or_else(|| infer_media_service(&device_service))
        .ok_or_else(|| anyhow!("camera did not expose ONVIF Media XAddr"))?;

    let profiles_xml = soap_post(
        client,
        &media_service,
        credentials,
        MEDIA_WSDL,
        "GetProfiles",
        "<trt:GetProfiles/>",
    )
    .await?;
    let profiles = parse_profiles(&profiles_xml)?;
    if profiles.is_empty() {
        return Err(anyhow!("camera returned no ONVIF media profiles"));
    }

    let (profile, live_profile) = select_profiles(profiles);
    if live_profile.token == profile.token {
        info!(
            profile = %profile.token,
            "no usable substream; live view will relay the main stream"
        );
    } else {
        info!(
            main = %profile.token,
            main_res = ?(profile.width, profile.height),
            live = %live_profile.token,
            live_res = ?(live_profile.width, live_profile.height),
            "live view will use the substream"
        );
    }
    let rtsp_uri = stream_uri(client, &media_service, credentials, &profile.token).await?;
    // Same token means the camera offers no usable substream; do not ask twice.
    let live_rtsp_uri = if live_profile.token == profile.token {
        rtsp_uri.clone()
    } else {
        match stream_uri(client, &media_service, credentials, &live_profile.token).await {
            Ok(uri) => uri,
            Err(error) => {
                // A camera that lists a substream but will not hand out its URI is
                // not a reason to have no live view at all.
                warn!(%error, token = %live_profile.token, "substream URI unavailable; live falls back to the main stream");
                rtsp_uri.clone()
            }
        }
    };

    let snapshot_body = format!(
        "<trt:GetSnapshotUri><trt:ProfileToken>{}</trt:ProfileToken></trt:GetSnapshotUri>",
        xml_escape(&profile.token),
    );
    let snapshot_uri = match soap_post(
        client,
        &media_service,
        credentials,
        MEDIA_WSDL,
        "GetSnapshotUri",
        &snapshot_body,
    )
    .await
    {
        Ok(xml) => parse_first_text(&xml, "Uri"),
        Err(error) => {
            debug!(%error, "camera did not provide an ONVIF snapshot URI");
            None
        }
    };

    let identity = device
        .endpoint_reference
        .as_deref()
        .unwrap_or(&device_service);
    let camera_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, identity.as_bytes()).to_string();

    Ok(CameraCandidate {
        camera_id,
        device_service,
        manufacturer: info.manufacturer,
        model: info.model,
        firmware: info.firmware,
        profile,
        rtsp_uri,
        live_rtsp_uri,
        snapshot_uri,
    })
}

async fn soap_post(
    client: &Client,
    endpoint: &str,
    credentials: Option<&Credentials>,
    namespace: &str,
    operation: &str,
    body: &str,
) -> anyhow::Result<String> {
    let security = credentials.map(wsse_header).unwrap_or_default();
    let envelope = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="{SOAP_ENV}" xmlns:tds="{DEVICE_WSDL}" xmlns:trt="{MEDIA_WSDL}" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">
  <s:Header>{security}</s:Header>
  <s:Body>{body}</s:Body>
</s:Envelope>"#
    );

    let response = client
        .post(endpoint)
        .header(
            "Content-Type",
            format!("application/soap+xml; charset=utf-8; action=\"{namespace}/{operation}\""),
        )
        .body(envelope)
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .context("send ONVIF SOAP request")?;
    let status = response.status();
    let text = response.text().await.context("read ONVIF SOAP response")?;
    if !status.is_success() {
        let detail =
            parse_first_text(&text, "Text").unwrap_or_else(|| text.chars().take(220).collect());
        return Err(anyhow!("ONVIF SOAP HTTP {status}: {detail}"));
    }
    Ok(text)
}

fn wsse_header(credentials: &Credentials) -> String {
    let mut nonce = [0_u8; 20];
    rand::thread_rng().fill_bytes(&mut nonce);
    let created = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(credentials.password.as_bytes());
    let digest = BASE64.encode(hasher.finalize());
    let nonce_b64 = BASE64.encode(nonce);

    format!(
        r#"<wsse:Security s:mustUnderstand="1"><wsse:UsernameToken><wsse:Username>{}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</wsse:Nonce><wsu:Created>{}</wsu:Created></wsse:UsernameToken></wsse:Security>"#,
        xml_escape(&credentials.username),
        digest,
        nonce_b64,
        created
    )
}

fn infer_media_service(device_service: &str) -> Option<String> {
    if device_service.contains("device_service") {
        Some(device_service.replace("device_service", "Media"))
    } else {
        None
    }
}

fn parse_probe_match(xml: &str) -> anyhow::Result<Option<DiscoveredDevice>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_xaddrs = false;
    let mut in_address = false;
    let mut xaddrs = Vec::new();
    let mut endpoint_reference = None;

    loop {
        match reader.read_event()? {
            Event::Start(event) => match event.local_name().as_ref() {
                b"XAddrs" => in_xaddrs = true,
                b"Address" => in_address = true,
                _ => {}
            },
            Event::Text(text) if in_xaddrs => {
                let value = text.decode()?.into_owned();
                xaddrs.extend(value.split_whitespace().map(ToOwned::to_owned));
            }
            Event::Text(text) if in_address => {
                endpoint_reference = Some(text.decode()?.into_owned())
            }
            Event::End(event) => match event.local_name().as_ref() {
                b"XAddrs" => in_xaddrs = false,
                b"Address" => in_address = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    if xaddrs.is_empty() && endpoint_reference.is_none() {
        return Ok(None);
    }
    xaddrs.sort();
    xaddrs.dedup();
    Ok(Some(DiscoveredDevice {
        endpoint_reference,
        xaddrs,
    }))
}

fn parse_device_information(xml: &str) -> anyhow::Result<DeviceInformation> {
    Ok(DeviceInformation {
        manufacturer: parse_first_text(xml, "Manufacturer"),
        model: parse_first_text(xml, "Model"),
        firmware: parse_first_text(xml, "FirmwareVersion"),
        serial_number: parse_first_text(xml, "SerialNumber"),
    })
}

fn parse_media_xaddr(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut media_depth = 0_u32;
    let mut in_xaddr = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => match event.local_name().as_ref() {
                b"Media" => media_depth += 1,
                b"XAddr" if media_depth > 0 => in_xaddr = true,
                _ => {}
            },
            Ok(Event::Text(text)) if in_xaddr => return text.decode().ok().map(|v| v.into_owned()),
            Ok(Event::End(event)) => match event.local_name().as_ref() {
                b"XAddr" => in_xaddr = false,
                b"Media" if media_depth > 0 => media_depth -= 1,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

fn parse_profiles(xml: &str) -> anyhow::Result<Vec<OnvifProfile>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut profiles = Vec::new();
    let mut current: Option<OnvifProfile> = None;
    let mut in_video = false;
    let mut field: Option<Vec<u8>> = None;

    loop {
        match reader.read_event()? {
            Event::Start(event) => {
                let local = event.local_name().as_ref().to_vec();
                if local == b"Profiles" {
                    let token = event
                        .attributes()
                        .filter_map(Result::ok)
                        .find(|attr| attr.key.as_ref() == b"token")
                        .and_then(|attr| attr.decode_and_unescape_value(reader.decoder()).ok())
                        .map(|v| v.into_owned())
                        .unwrap_or_default();
                    current = Some(OnvifProfile {
                        token,
                        ..Default::default()
                    });
                } else if current.is_some() && local == b"VideoEncoderConfiguration" {
                    in_video = true;
                } else if current.is_some()
                    && (local == b"Name"
                        || (in_video
                            && (local == b"Encoding"
                                || local == b"Width"
                                || local == b"Height"
                                || local == b"FrameRateLimit"
                                || local == b"BitrateLimit")))
                {
                    field = Some(local);
                }
            }
            Event::Text(text) => {
                let value = text.decode()?.into_owned();
                if let (Some(profile), Some(field_name)) = (current.as_mut(), field.as_deref()) {
                    match field_name {
                        b"Name" if profile.name.is_none() => profile.name = Some(value),
                        b"Encoding" => profile.encoding = Some(value),
                        b"Width" => profile.width = value.parse().ok(),
                        b"Height" => profile.height = value.parse().ok(),
                        b"FrameRateLimit" => profile.fps = value.parse().ok(),
                        b"BitrateLimit" => profile.bitrate_kbps = value.parse().ok(),
                        _ => {}
                    }
                }
            }
            Event::End(event) => {
                let local = event.local_name();
                if local.as_ref() == b"Profiles" {
                    if let Some(profile) = current.take()
                        && !profile.token.is_empty()
                    {
                        profiles.push(profile);
                    }
                } else if local.as_ref() == b"VideoEncoderConfiguration" {
                    in_video = false;
                }
                field = None;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(profiles)
}

fn parse_first_text(xml: &str, target: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut inside = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) if event.local_name().as_ref() == target.as_bytes() => {
                inside = true
            }
            Ok(Event::Text(text)) if inside => return text.decode().ok().map(|v| v.into_owned()),
            Ok(Event::End(event)) if event.local_name().as_ref() == target.as_bytes() => {
                inside = false
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::{
        OnvifProfile, parse_media_xaddr, parse_probe_match, parse_profiles, select_profiles,
    };

    fn profile(token: &str, w: u32, h: u32, encoding: Option<&str>) -> OnvifProfile {
        OnvifProfile {
            token: token.into(),
            width: Some(w),
            height: Some(h),
            encoding: encoding.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn live_takes_the_substream_and_main_takes_the_highest() {
        let (main, live) = select_profiles(vec![
            profile("sub", 704, 576, Some("H264")),
            profile("main", 1920, 1080, Some("H264")),
        ]);
        assert_eq!(main.token, "main");
        assert_eq!(live.token, "sub");
    }

    #[test]
    fn live_takes_the_best_substream_under_the_ceiling() {
        // 1280x720 is the best quality that still saves real bandwidth; the
        // 1600x1200 profile is a second main stream, not a substream.
        let (_, live) = select_profiles(vec![
            profile("main", 2592, 1944, Some("H264")),
            profile("second-main", 1600, 1200, Some("H264")),
            profile("sub-720", 1280, 720, Some("H264")),
            profile("sub-cif", 352, 288, Some("H264")),
        ]);
        assert_eq!(live.token, "sub-720");
    }

    #[test]
    fn live_never_selects_a_codec_the_passthrough_path_cannot_carry() {
        // The live path is H.264 passthrough. An MJPEG or H.265 substream would
        // negotiate and then produce nothing.
        let (main, live) = select_profiles(vec![
            profile("main", 1920, 1080, Some("H264")),
            profile("mjpeg", 640, 480, Some("JPEG")),
            profile("h265", 704, 576, Some("H265")),
        ]);
        assert_eq!(main.token, "main");
        assert_eq!(
            live.token, "main",
            "no usable substream means fall back to the main stream"
        );
    }

    #[test]
    fn a_single_profile_camera_uses_it_for_both() {
        let (main, live) = select_profiles(vec![profile("only", 1280, 720, Some("H264"))]);
        assert_eq!(main.token, "only");
        assert_eq!(live.token, "only");
    }

    #[test]
    fn unknown_encoding_is_not_excluded() {
        // Some cameras omit the encoding in GetProfiles. Excluding them would
        // leave the substream unused on hardware that supports it.
        let (main, live) = select_profiles(vec![
            profile("main", 1920, 1080, None),
            profile("sub", 640, 480, None),
        ]);
        assert_eq!(main.token, "main");
        assert_eq!(live.token, "sub");
    }

    #[test]
    fn falls_back_to_the_smallest_when_every_other_profile_is_large() {
        let (_, live) = select_profiles(vec![
            profile("main", 3840, 2160, Some("H264")),
            profile("big", 2560, 1440, Some("H264")),
        ]);
        assert_eq!(live.token, "big", "still better than relaying 4K");
    }

    #[test]
    fn parses_xaddrs_and_endpoint() {
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"><s:Body><d:ProbeMatches><d:ProbeMatch><a:EndpointReference><a:Address>urn:uuid:camera-1</a:Address></a:EndpointReference><d:XAddrs>http://192.168.1.21/onvif/device_service http://[fe80::1]/onvif/device_service</d:XAddrs></d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>"#;
        let parsed = parse_probe_match(xml).unwrap().unwrap();
        assert_eq!(
            parsed.endpoint_reference.as_deref(),
            Some("urn:uuid:camera-1")
        );
        assert_eq!(parsed.xaddrs.len(), 2);
    }

    #[test]
    fn parses_media_capability() {
        let xml = r#"<Capabilities><Media><XAddr>http://10.0.0.2/onvif/Media</XAddr></Media></Capabilities>"#;
        assert_eq!(
            parse_media_xaddr(xml).as_deref(),
            Some("http://10.0.0.2/onvif/Media")
        );
    }

    #[test]
    fn parses_profiles_and_video_encoder() {
        let xml = r#"<GetProfilesResponse><Profiles token="main"><Name>MainStream</Name><VideoEncoderConfiguration><Encoding>H264</Encoding><Resolution><Width>1920</Width><Height>1080</Height></Resolution><RateControl><FrameRateLimit>25</FrameRateLimit><BitrateLimit>2048</BitrateLimit></RateControl></VideoEncoderConfiguration></Profiles></GetProfilesResponse>"#;
        let profiles = parse_profiles(xml).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].token, "main");
        assert_eq!(profiles[0].width, Some(1920));
        assert_eq!(profiles[0].fps, Some(25.0));
    }
}
