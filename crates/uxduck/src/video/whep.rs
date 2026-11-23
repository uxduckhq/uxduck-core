//! A WHEP camera (ADR-0064 W1, W2, W5, W10, W11, W14): one WebRTC session
//! per camera, on the daemon's own task and socket, however many windows
//! watch it. It posts an offer to the camera's WHEP address, keeps only the
//! answer's ICE candidates on the address's own host, receives H.264 with
//! `str0m`, and puts the pictures through a `Source` into the hub as an RTSP
//! camera's go. It asks again for lost packets even when the sender offers
//! no RTX, as `MediaMTX` does, and deletes the session when it closes.
//!
//! The pictures are dated as RTSP's are: by the serving companion
//! computer's TIMESYNC clock first (S357), then by the server's RTCP sender
//! reports against a round trip (S355). `MediaMTX` answers no RTCP extended
//! report, so RTCP gives a receiver no round trip, and the round trip is
//! ICE's consent check on the same pair of addresses instead.
//!
//! When the vehicle moves to another connection (#923), the session follows
//! it with an ICE restart (RFC 9725): a PATCH with new credentials and one
//! candidate on the new connection, under `a=mid:0` because `MediaMTX` reads a
//! PATCH's candidates only under a numeric mid, to the session's path at the
//! camera's address on that connection. The session keeps its DTLS, SRTP and
//! streams, so the hub keeps one source. The RTSP worker opens a new session
//! when the PATCH fails, then falls back to RTSP (`rtsp.rs`).
//!
//! An https:// address may pin its certificate's SHA-256 with `#sha256=`
//! (W10): the certificate must be that one, whoever signed it. Without a
//! pin, the system's certificates decide. A user and password in the
//! address go as HTTP Basic authentication, as `MediaMTX` reads them.

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use str0m::change::SdpAnswer;
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use uxduck_core::domain::video::{AddressReason, is_whep, redact};
use uxduck_media::latency::Arrival;
use uxduck_media::nal::Codec;
use uxduck_media::rtcp::SenderReport;
use uxduck_media::rtp::Picture;
use uxduck_media::source::Source;

use super::listener::{item, metadata_due, report};
use super::rtsp::{
    Camera, ConnectionLog, Moving, NO_PICTURES, Progress, REPORT_EVERY, SILENT_FOR, STALL_AFTER,
    START_WITHIN, decode, wall_us, wall_us_at,
};

const USER_AGENT: &str = concat!("UXDuck/", env!("CARGO_PKG_VERSION"));
/// Each WHEP request, so a server that took the connection and went
/// silent is given up on, as RTSP's set-up is.
const ASK_FOR: Duration = Duration::from_secs(10);
/// Seconds from 1900, NTP's epoch, to 1970.
const NTP_TO_UNIX_S: u64 = 2_208_988_800;

/// A WHEP address, taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    /// What requests go to: the address without user, password or pin.
    pub url: String,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    /// The SHA-256 of the certificate an https:// address must present.
    pub pin: Option<[u8; 32]>,
}

/// # Errors
/// An address that is not
/// `http[s]://[user[:password]@]host[:port]/path[#sha256=…]`.
pub fn address(url: &str) -> Result<Address, String> {
    let url = url.trim();
    let (scheme, rest) = url
        .split_once("://")
        .filter(|(scheme, _)| {
            ["http", "https"]
                .iter()
                .any(|s| scheme.eq_ignore_ascii_case(s))
        })
        .ok_or_else(|| "a WHEP address starts with http:// or https://".to_owned())?;
    let secure = scheme.eq_ignore_ascii_case("https");
    let (rest, fragment) = rest
        .split_once('#')
        .map_or((rest, None), |(bare, fragment)| (bare, Some(fragment)));
    let pin = fragment.map(pin).transpose()?;
    if pin.is_some() && !secure {
        return Err("a certificate fingerprint needs an https:// WHEP address".into());
    }
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (credentials, host_port) = match authority.rsplit_once('@') {
        Some((c, h)) => (Some(c), h),
        None => (None, authority),
    };
    let (user, password) = match credentials {
        Some(c) => match c.split_once(':') {
            Some((u, p)) => (Some(decode(u)), Some(decode(p))),
            None => (Some(decode(c)), None),
        },
        None => (None, None),
    };
    let (host, port) = match host_port.strip_prefix('[') {
        Some(bracketed) => bracketed
            .split_once(']')
            .map(|(host, rest)| (host, rest.strip_prefix(':')))
            .ok_or_else(|| "the WHEP address has no closing ]".to_owned())?,
        None => match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        },
    };
    let port = port.map_or(Ok(if secure { 443 } else { 80 }), |p| {
        p.parse().map_err(|_| format!("{p} is not a port"))
    })?;
    if host.is_empty() {
        return Err("the WHEP address has no host".into());
    }
    Ok(Address {
        url: format!(
            "{}://{host_port}/{path}",
            if secure { "https" } else { "http" }
        ),
        host: host.to_owned(),
        port,
        user,
        password,
        pin,
    })
}

/// `sha256=` and 64 hex digits, with or without colons between bytes.
fn pin(fragment: &str) -> Result<[u8; 32], String> {
    let wrong = || {
        format!("#{fragment} is not a certificate fingerprint, such as #sha256= and 64 hex digits")
    };
    let digits: Vec<u8> = fragment
        .strip_prefix("sha256=")
        .ok_or_else(wrong)?
        .bytes()
        .filter(|b| *b != b':')
        .collect();
    if digits.len() != 64 {
        return Err(wrong());
    }
    let mut pin = [0u8; 32];
    for (byte, pair) in pin.iter_mut().zip(digits.chunks(2)) {
        let pair = std::str::from_utf8(pair).map_err(|_| wrong())?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| wrong())?;
    }
    Ok(pin)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, b| {
        let _ = write!(text, "{b:02x}");
        text
    })
}

/// Accepts only the certificate whose SHA-256 is pinned, its handshake
/// signed as rustls checks any (W10). The name it was made for does not
/// matter: the pin says which computer it is.
#[derive(Debug)]
struct Pinned {
    sha256: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let presented: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if presented == self.sha256 {
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }
        Err(rustls::Error::General(format!(
            "the camera's certificate changed: its SHA-256 is {}, not the pinned {}",
            hex(&presented),
            hex(&self.sha256)
        )))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The HTTP client for one address: its pinned certificate or the
/// system's, and never a proxy, because a camera is on the vehicle's
/// network.
fn client(address: &Address) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(ASK_FOR)
        .user_agent(USER_AGENT);
    if let Some(sha256) = address.pin {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("UXDuck could not set up TLS for WHEP: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned { sha256, provider }))
            .with_no_client_auth();
        builder = builder.tls_backend_preconfigured(tls);
    }
    builder
        .build()
        .map_err(|e| format!("UXDuck could not set up its WHEP client: {e}"))
}

/// A request's error with what caused it, such as a changed certificate.
fn why(error: &reqwest::Error) -> String {
    let mut text = if error.is_timeout() {
        format!("no answer within {} s", ASK_FOR.as_secs())
    } else if error.is_connect() {
        "could not connect".to_owned()
    } else {
        error.to_string()
    };
    let mut cause = std::error::Error::source(error);
    while let Some(inner) = cause {
        let inner_text = inner.to_string();
        if !text.contains(&inner_text) {
            text = format!("{text}: {inner_text}");
        }
        cause = inner.source();
    }
    text
}

fn authorized(request: reqwest::RequestBuilder, address: &Address) -> reqwest::RequestBuilder {
    match &address.user {
        Some(user) => request.basic_auth(user, address.password.as_ref()),
        None => request,
    }
}

/// What a WHEP server said when it refused a request.
async fn refused(response: reqwest::Response, what: &str) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let body = body.trim();
    if body.is_empty() {
        format!("the WHEP server answered the {what} with {status}")
    } else {
        format!("the WHEP server answered the {what} with {status}: {body}")
    }
}

/// Whether the camera's WHEP server answers at all, for the RTSP worker's
/// probes of a preferred address (ADR-0057 D10). An OPTIONS request makes
/// no session, so the probe carries no media.
///
/// # Errors
/// What did not answer.
pub(super) async fn probe(url: &str) -> Result<(), String> {
    let address = address(url)?;
    let response = client(&address)?
        .request(reqwest::Method::OPTIONS, &address.url)
        .send()
        .await
        .map_err(|e| why(&e))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(refused(response, "probe").await)
    }
}

/// The address's host's IP addresses: the camera's on one connection.
async fn hosts(address: &Address) -> Result<Vec<IpAddr>, String> {
    let mut found: Vec<IpAddr> = tokio::net::lookup_host((address.host.as_str(), address.port))
        .await
        .map_err(|e| format!("can't find {}: {e}", address.host))?
        .map(|a| a.ip())
        .collect();
    found.dedup();
    if found.is_empty() {
        return Err(format!("{} has no address", address.host));
    }
    Ok(found)
}

/// The daemon's end of one connection: a socket on the local address that
/// routes to the camera's host, its only ICE candidate.
struct Link {
    socket: UdpSocket,
    local: SocketAddr,
    hosts: Vec<IpAddr>,
}

impl Link {
    async fn towards(hosts: Vec<IpAddr>) -> Result<Self, String> {
        let remote = hosts[0];
        let any: SocketAddr = if remote.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        // Connecting a UDP socket sends nothing: it asks the system which
        // of this computer's addresses reaches the host.
        let route = std::net::UdpSocket::bind(any)
            .and_then(|s| s.connect((remote, 9)).map(|()| s))
            .and_then(|s| s.local_addr())
            .map_err(|e| format!("no route to {remote}: {e}"))?;
        let socket = UdpSocket::bind((route.ip(), 0))
            .await
            .map_err(|e| format!("can't open a socket for WebRTC: {e}"))?;
        let local = socket
            .local_addr()
            .map_err(|e| format!("can't open a socket for WebRTC: {e}"))?;
        Ok(Self {
            socket,
            local,
            hosts,
        })
    }

    fn candidate(&self) -> Result<Candidate, String> {
        Candidate::host(self.local, "udp")
            .map_err(|e| format!("{} can't be a WebRTC address: {e}", self.local))
    }
}

/// The ICE candidates of an SDP answer, or of a PATCH's answer, kept only
/// when they are UDP on one of `hosts`, so the video takes the connection
/// UXDuck chose for the camera, never one ICE prefers (W5). The text is
/// otherwise as it was, with how many were kept.
fn on_host(sdp: &str, hosts: &[IpAddr]) -> (String, usize) {
    let mut kept = 0;
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.lines() {
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            let fields: Vec<&str> = candidate.split_whitespace().collect();
            let udp = fields.get(2).is_some_and(|t| t.eq_ignore_ascii_case("udp"));
            let here = fields
                .get(4)
                .and_then(|ip| ip.parse::<IpAddr>().ok())
                .is_some_and(|ip| hosts.contains(&ip));
            if !(udp && here) {
                continue;
            }
            kept += 1;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    (out, kept)
}

/// The session on the server, deleted when it closes (W1).
struct Session {
    client: reqwest::Client,
    address: Address,
    /// Where the session is: its path at the address on the connection now.
    url: String,
}

impl Drop for Session {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let request = authorized(self.client.delete(&self.url), &self.address);
        let url = redact(&self.url);
        runtime.spawn(async move {
            if let Err(e) = request.send().await {
                tracing::debug!(%url, error = %why(&e), "a WHEP session's DELETE failed");
            }
        });
    }
}

/// A move of the playing session to another of the camera's addresses, and
/// where its outcome goes: an ICE restart there (W14).
pub(super) struct Follow {
    url: String,
    reason: AddressReason,
    decided_us: u64,
    done: oneshot::Sender<Result<(), String>>,
}

/// Asks the WHEP session playing `from` to follow the vehicle to `to` with
/// an ICE restart, for `reason`. None when either is not a WHEP address, or
/// nothing plays.
pub(super) fn follow(
    sessions: &mpsc::UnboundedSender<Follow>,
    from: &str,
    to: &str,
    reason: AddressReason,
) -> Option<oneshot::Receiver<Result<(), String>>> {
    if !(is_whep(from) && is_whep(to)) {
        return None;
    }
    let (done, outcome) = oneshot::channel();
    sessions
        .send(Follow {
            url: to.to_owned(),
            reason,
            decided_us: wall_us(),
            done,
        })
        .ok()?;
    Some(outcome)
}

fn rtc() -> Rtc {
    RtcConfig::new()
        .clear_codecs()
        .enable_h264(true)
        .set_stats_interval(Some(Duration::from_secs(1)))
        .build(Instant::now())
}

/// When an instant this computer measured was, by its wall clock.
fn arrival(at: Instant) -> Arrival {
    let now = Arrival::now();
    let ago = u64::try_from(now.at.saturating_duration_since(at).as_micros()).unwrap_or(u64::MAX);
    Arrival {
        at,
        wall_us: now.wall_us.saturating_sub(ago),
    }
}

/// A sender report's time as RTCP carries it: NTP's 64 bits.
fn ntp(time: SystemTime) -> u64 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs() + NTP_TO_UNIX_S;
    let fraction = (u64::from(since.subsec_nanos()) << 32) / 1_000_000_000;
    (seconds << 32) | fraction
}

struct Opened {
    rtc: Rtc,
    link: Link,
    session: Session,
    answer: String,
    server: Option<String>,
}

/// The offer, the answer, and the answer's candidates on the address's host.
async fn open(address: &Address) -> Result<Opened, String> {
    let client = client(address)?;
    let link = Link::towards(hosts(address).await?).await?;
    let mut rtc = rtc();
    rtc.add_local_candidate(link.candidate()?);
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    let (offer, pending) = change
        .apply()
        .ok_or_else(|| "UXDuck made no WebRTC offer".to_owned())?;
    let response = authorized(client.post(&address.url), address)
        .header(reqwest::header::CONTENT_TYPE, "application/sdp")
        .body(offer.to_sdp_string())
        .send()
        .await
        .map_err(|e| why(&e))?;
    if !response.status().is_success() {
        return Err(refused(response, "offer").await);
    }
    let server = response
        .headers()
        .get(reqwest::header::SERVER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "the WHEP server gave the session no address (Location)".to_owned())?;
    let url = response
        .url()
        .join(location)
        .map_err(|e| format!("the WHEP session's address {location} is not one: {e}"))?
        .to_string();
    let session = Session {
        client,
        address: address.clone(),
        url,
    };
    let answer = response.text().await.map_err(|e| why(&e))?;
    let (answer, kept) = on_host(&answer, &link.hosts);
    if kept == 0 {
        return Err(format!(
            "the WHEP server offered no ICE candidate on {}",
            address.host
        ));
    }
    let parsed = SdpAnswer::from_sdp_string(&answer)
        .map_err(|e| format!("the WHEP answer is not SDP UXDuck reads: {e}"))?;
    rtc.sdp_api()
        .accept_answer(pending, parsed)
        .map_err(|e| format!("the WHEP answer was refused: {e}"))?;
    Ok(Opened {
        rtc,
        link,
        session,
        answer,
        server,
    })
}

/// An ICE restart over WHEP PATCH to `to`, the session's path at that
/// address: new credentials and the candidate on the new connection, which
/// becomes the only one, and the answer's credentials and candidates on
/// that host taken in. The new link and the answer as it now stands (W14).
async fn restart(
    rtc: &mut Rtc,
    session: &mut Session,
    answer: &str,
    to: &Address,
) -> Result<(Link, String), String> {
    let link = Link::towards(hosts(to).await?).await?;
    let candidate = link.candidate()?;
    let at = reqwest::Url::parse(&session.url)
        .map_err(|e| format!("the WHEP session's address is not one: {e}"))?;
    let mut target = reqwest::Url::parse(&to.url)
        .map_err(|e| format!("the WHEP address {} is not one: {e}", redact(&to.url)))?;
    target.set_path(at.path());
    target.set_query(at.query());
    let target = target.to_string();
    let mut change = rtc.sdp_api();
    let credentials = change.ice_restart(false);
    let (_, pending) = change
        .apply()
        .ok_or_else(|| "UXDuck made no ICE restart".to_owned())?;
    let fragment = format!(
        "a=ice-ufrag:{}\r\na=ice-pwd:{}\r\nm=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\na={}\r\n",
        credentials.ufrag,
        credentials.pass,
        candidate.to_sdp_string()
    );
    let client = client(to)?;
    let response = authorized(client.patch(&target), to)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/trickle-ice-sdpfrag",
        )
        .header(reqwest::header::IF_MATCH, "\"*\"")
        .body(fragment)
        .send()
        .await
        .map_err(|e| why(&e))?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Err("the WHEP server took the candidate without restarting ICE".into());
    }
    if !response.status().is_success() {
        return Err(refused(response, "ICE restart").await);
    }
    let body = response.text().await.map_err(|e| why(&e))?;
    let value = |key: &str| {
        body.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(str::trim)
            .ok_or_else(|| format!("the WHEP server's ICE restart has no {key}"))
    };
    let (ufrag, pwd) = (value("a=ice-ufrag:")?, value("a=ice-pwd:")?);
    let (theirs, kept) = on_host(&body, &link.hosts);
    if kept == 0 {
        return Err(format!(
            "the WHEP server's ICE restart offered no ICE candidate on {}",
            to.host
        ));
    }
    let candidates: Vec<&str> = theirs
        .lines()
        .filter(|l| l.starts_with("a=candidate:"))
        .collect();
    let mut restarted = String::with_capacity(answer.len());
    let mut placed = false;
    for line in answer.lines() {
        if line.starts_with("a=ice-ufrag:") {
            let _ = write!(restarted, "a=ice-ufrag:{ufrag}\r\n");
        } else if line.starts_with("a=ice-pwd:") {
            let _ = write!(restarted, "a=ice-pwd:{pwd}\r\n");
        } else if line.starts_with("a=candidate:") {
            if !placed {
                for c in &candidates {
                    let _ = write!(restarted, "{c}\r\n");
                }
                placed = true;
            }
        } else {
            let _ = write!(restarted, "{line}\r\n");
        }
    }
    let parsed = SdpAnswer::from_sdp_string(&restarted)
        .map_err(|e| format!("the ICE restart's answer is not SDP UXDuck reads: {e}"))?;
    rtc.sdp_api()
        .accept_answer(pending, parsed)
        .map_err(|e| format!("the ICE restart's answer was refused: {e}"))?;
    rtc.add_local_candidate(candidate);
    session.client = client;
    session.address = to.clone();
    session.url = target;
    Ok((link, restarted))
}

/// Connects, plays and relays until the session fails, following the
/// vehicle to another address when `follows` asks. `silent` says when
/// nothing has come for `STALL_AFTER`, and `progress` times a move to the
/// address, or an ICE restart to another, until its first keyframe
/// (ADR-0057 D13 and D14).
#[allow(
    clippy::too_many_lines,
    reason = "one loop: str0m's output, the socket, a move, and the report"
)]
pub(super) async fn play(
    camera: Camera<'_>,
    url: &str,
    connection_log: &mut ConnectionLog,
    silent: &tokio::sync::watch::Sender<bool>,
    progress: &mut Progress,
    follows: &mut mpsc::UnboundedReceiver<Follow>,
) -> Result<(), String> {
    let source_id = camera.id;
    let mut address = address(url)?;
    let mut playing_url = url.to_owned();
    // The offer asks for the pictures, as PLAY does over RTSP.
    let offered_us = wall_us();
    let Opened {
        mut rtc,
        mut link,
        mut session,
        mut answer,
        server,
    } = tokio::time::timeout(START_WITHIN, open(&address))
        .await
        .map_err(|_| "the camera did not start within 15 s".to_owned())??;
    if let Some(moving) = progress.moving.as_mut() {
        moving.play_sent_us = Some(offered_us);
    }
    // `MediaMTX`'s WebRTC output sends gortsplib's sender reports, which date
    // pictures as its RTSP server's do (S355).
    let mut source = Source::rtp(Codec::H264, &[]).over_rtsp(server.as_deref().map(|s| {
        if s.eq_ignore_ascii_case("mediamtx") {
            "gortsplib"
        } else {
            s
        }
    }));
    let mut peer = SocketAddr::new(link.hosts[0], address.port);
    let mut nacking = false;
    let mut next_sequence: Option<u64> = None;
    let mut answered = 0u64;
    let mut last_picture = tokio::time::Instant::now();
    // When anything at all last came, for a stall.
    let mut last_heard = last_picture;
    let mut reported = (Instant::now(), 0u64, 0u64);
    let mut klv_posted = None;
    let mut buf = vec![0u8; 2048];
    loop {
        let deadline = loop {
            let output = rtc
                .poll_output()
                .map_err(|e| format!("WebRTC failed: {e}"))?;
            match output {
                Output::Timeout(at) => break at,
                Output::Transmit(t) => {
                    if t.source == link.local {
                        // A datagram the network drops is a loss WebRTC sees.
                        let _ = link.socket.send_to(&t.contents, t.destination).await;
                    }
                }
                Output::Event(Event::MediaData(data)) => {
                    // `MediaMTX` offers no RTX, which str0m would wait for
                    // before asking again for lost packets.
                    if !nacking
                        && let Some(stream) = rtc.direct_api().stream_rx_by_mid(data.mid, None)
                    {
                        stream.suppress_nack(false);
                        nacking = true;
                    }
                    let (start, end) = (**data.seq_range.start(), **data.seq_range.end());
                    let missing = next_sequence.map_or(0, |next| start.saturating_sub(next));
                    next_sequence = Some(end + 1);
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "RTP's 32-bit timestamp, as the packets had it"
                    )]
                    let picture = Picture {
                        annex_b: data.data.to_vec(),
                        timestamp: data.time.numer() as u32,
                        damaged: !data.contiguous,
                    };
                    let unit = source.push_picture(
                        &picture,
                        end.saturating_sub(start) + 1,
                        data.data.len() as u64,
                        missing,
                        arrival(data.network_time),
                    );
                    if let Some(output) = unit {
                        last_picture = tokio::time::Instant::now();
                        connection_log.receiving(source_id, Instant::now());
                        if let uxduck_media::source::Output::Unit(unit) = &output {
                            progress.picture(camera, &playing_url, unit.key);
                        }
                        if let Some(metadata) = metadata_due(&output, &mut klv_posted) {
                            camera.post(move |e, id| e.video_klv(id, &metadata));
                        }
                        camera.hub.publish(source_id, &item(output));
                    }
                }
                Output::Event(Event::SenderFeedback(feedback)) => {
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "RTP's 32-bit timestamp, as the report had it"
                    )]
                    let report = SenderReport {
                        ssrc: *feedback.sender_info.ssrc,
                        ntp: ntp(feedback.sender_info.ntp_time),
                        rtp: feedback.sender_info.rtp_time.numer() as u32,
                    };
                    source.server_report(report, arrival(feedback.received_at));
                }
                // ICE's consent check, a round trip on the pair the video
                // takes, times the server's clock (S355).
                Output::Event(Event::PeerStats(stats)) => {
                    if let Some(pair) = stats.selected_candidate_pair {
                        peer = pair.remote.addr;
                        if pair.responses_received != answered
                            && let Some(rtt) = pair.current_round_trip_time
                        {
                            answered = pair.responses_received;
                            let now = Instant::now();
                            source.round_trip(now.checked_sub(rtt).unwrap_or(now), now);
                        }
                    }
                }
                Output::Event(Event::IceConnectionStateChange(state)) => {
                    tracing::debug!(source = %source_id, ?state, "a WHEP camera's ICE");
                }
                Output::Event(Event::Closed) => {
                    return Err("the WHEP server closed the session".into());
                }
                Output::Event(_) => {}
            }
        };
        if reported.0.elapsed() >= REPORT_EVERY {
            let (id, host) = (source_id.to_owned(), address.host.clone());
            source.companion(
                camera
                    .engine
                    .run(move |e| e.camera_companion(&id, &host))
                    .await
                    .ok()
                    .flatten(),
            );
            let status = source.status();
            let elapsed = reported.0.elapsed().as_secs_f64();
            let mut observed = report(
                address.port,
                &peer,
                &status,
                elapsed,
                reported.1,
                reported.2,
            );
            observed.id = Some(source_id.to_owned());
            observed.counters.hold_us = camera.hub.hold_us(source_id);
            reported = (
                Instant::now(),
                status.counters.bytes,
                status.counters.pictures,
            );
            camera.post(move |e, _| e.rtsp_observed(observed));
        }
        tokio::select! {
            biased;
            Some(Follow { url, reason, decided_us, done }) = follows.recv() => {
                let last_picture_us = progress.last_picture.map(wall_us_at);
                let patched_us = wall_us();
                let moved = match self::address(&url) {
                    Ok(to) => restart(&mut rtc, &mut session, &answer, &to)
                        .await
                        .map(|moved| (to, moved)),
                    Err(e) => Err(e),
                };
                match moved {
                    Ok((to, (moved, restarted))) => {
                        tracing::info!(
                            source = %source_id,
                            to = %redact(&url),
                            "a WHEP camera restarted ICE to follow its vehicle"
                        );
                        address = to;
                        link = moved;
                        answer = restarted;
                        peer = SocketAddr::new(link.hosts[0], address.port);
                        answered = 0;
                        // The pictures may stop from now: the old connection
                        // is the one the vehicle left.
                        last_picture = tokio::time::Instant::now();
                        last_heard = last_picture;
                        // Timed as a move, with the PATCH as its PLAY, to the
                        // first keyframe on either connection.
                        progress.moving = Some(Moving {
                            reason,
                            last_picture_us,
                            decided_us,
                            pre_opened: false,
                            play_sent_us: Some(patched_us),
                            first_picture_us: None,
                        });
                        playing_url = url;
                        let _ = done.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = done.send(Err(e));
                    }
                }
            }
            received = link.socket.recv_from(&mut buf) => {
                let (n, from) = received.map_err(|e| format!("WebRTC's socket failed: {e}"))?;
                last_heard = tokio::time::Instant::now();
                silent.send_if_modified(|was| std::mem::replace(was, false));
                let Ok(contents) = buf[..n].try_into() else {
                    continue;
                };
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source: from,
                        destination: link.local,
                        contents,
                    },
                );
                rtc.handle_input(input).map_err(|e| format!("WebRTC failed: {e}"))?;
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                rtc.handle_input(Input::Timeout(Instant::now()))
                    .map_err(|e| format!("WebRTC failed: {e}"))?;
            }
            () = tokio::time::sleep_until(last_picture + SILENT_FOR) => {
                return Err(NO_PICTURES.to_owned());
            }
            () = tokio::time::sleep_until(last_heard + STALL_AFTER), if !*silent.borrow() => {
                silent.send_replace(true);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
