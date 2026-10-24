//! Feedback (ADR-0016 O8, O11): UXDuck's part of the diagnostics bundle,
//! scrubbed as ADR-0013 says, handed to the composition root to send or keep
//! while offline. The request is answered when the root reports back.
//!
//! Scrubbing is by field and by pattern, over the whole bundle: coordinates
//! are rounded to 0.01° (about 1 km), and secrets, addresses, device paths,
//! serial numbers and e-mail addresses are removed. The screenshot cannot be
//! scrubbed, which is why the person sees it and can remove it.

use std::collections::VecDeque;

use serde_json::{Map, Value};
use uxduck_contracts::dto::analytics::Surface;
use uxduck_contracts::dto::feedback::{
    BUNDLE_SCHEMA, BundleChatItem, BundleDaemon, BundleLink, BundleNotification, BundleReport,
    BundleVehicle, FeedbackAttachment, FeedbackBundle, FeedbackKind, FeedbackSendParams,
    FeedbackSendResult, FeedbackSuggestParams, FeedbackVoiceNote, SuggestionCard,
};
use uxduck_contracts::dto::trace::TraceFrame;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::thread::ItemKind;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// How long a client's frames are kept for its report, and at most how many.
pub(crate) const RECENT_US: u64 = 120_000_000;
pub(crate) const RECENT_FRAMES: usize = 200;
/// Longer strings in a kept frame are cut, such as a screenshot being sent.
const KEPT_STRING: usize = 512;
const MAX_TEXT: usize = 5000;
/// A 3 MB image in base64.
const MAX_SCREENSHOT_B64: usize = 4 * 1024 * 1024 + 4;
/// A 10 MB voice note in base64, how many a report takes, and the longest
/// transcript the app sends with one.
const MAX_VOICE_B64: usize = 10 * 1024 * 1024 / 3 * 4 + 4;
const MAX_VOICE_NOTES: usize = 5;
const MAX_TRANSCRIPT: usize = 5000;
/// Files the person attached: how many, and 25 MB together in base64.
const MAX_ATTACHMENTS: usize = 10;
const MAX_ATTACHMENTS_B64: usize = 25 * 1024 * 1024 / 3 * 4 + 4 * MAX_ATTACHMENTS;

/// What the composition root sends: the report, the scrubbed bundle, and
/// which vehicle's flight data to add from the tlogs.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedbackOrder {
    pub report_id: String,
    pub kind: FeedbackKind,
    pub text: String,
    pub reply_email: Option<String>,
    pub surface: Surface,
    /// One line for people, without the platform, which the root adds.
    pub context: String,
    /// Base64, as the page sent it.
    pub screenshot: Option<String>,
    pub bundle: Option<Value>,
    pub flight: Option<FlightRef>,
    /// Credit the signed-in account, or tell this install, when it ships (S73).
    pub credit: bool,
    pub from_agent: bool,
    /// Files the person attached, base64, as the page sent them.
    pub attachments: Vec<FeedbackAttachment>,
    /// The key the page keeps to add to the report later.
    pub follow_up_key: Option<String>,
    /// Recorded voice notes in the order made, base64, with what the app
    /// wrote down live.
    pub voice_notes: Vec<FeedbackVoiceNote>,
}

/// Where a vehicle's raw frames are: its link's tlogs, its system id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlightRef {
    pub link_id: String,
    pub system_id: u8,
}

/// Where reports go; the root answers through [`Engine::feedback_done`].
pub trait FeedbackPort: Send {
    fn send(&mut self, order: FeedbackOrder);
}

/// No sender: every report is refused. For tests and the browser worker.
#[derive(Debug, Default)]
pub struct NoFeedback;

impl FeedbackPort for NoFeedback {
    fn send(&mut self, _: FeedbackOrder) {}
}

/// Keeps a frame for the connection's report: the safe side only (the tap
/// has redacted it), long strings cut, the high-rate streams left out.
pub(crate) fn keep(recent: &mut VecDeque<TraceFrame>, mut frame: TraceFrame, now_us: u64) {
    if matches!(
        frame.method.as_deref(),
        Some("telemetry/update" | "agent/itemDelta" | "trace/frame")
    ) {
        return;
    }
    cut_strings(&mut frame.payload);
    recent.push_back(frame);
    while recent.len() > RECENT_FRAMES
        || recent
            .front()
            .is_some_and(|f| now_us.saturating_sub(f.ts_us) > RECENT_US)
    {
        recent.pop_front();
    }
}

fn cut_strings(value: &mut Value) {
    match value {
        Value::String(text) if text.len() > KEPT_STRING => {
            let mut end = KEPT_STRING;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push('…');
        }
        Value::Array(items) => items.iter_mut().for_each(cut_strings),
        Value::Object(map) => map.values_mut().for_each(cut_strings),
        _ => {}
    }
}

/// Field names whose values are secrets.
const SECRET_KEYS: [&str; 13] = [
    "token",
    "followupkey",
    "bootstrap",
    "session",
    "password",
    "secret",
    "apikey",
    "authorization",
    "cookie",
    "privatekey",
    "key",
    "signingkey",
    "credentials",
];
/// Field names whose values identify a place, a machine or a device.
const IDENTIFYING_KEYS: [&str; 14] = [
    "shellpath",
    "peer",
    "host",
    "address",
    "device",
    "ip",
    "uid",
    "uid2",
    "serial",
    "serialnumber",
    "usb",
    "email",
    "replyemail",
    "executable",
];
/// Field names holding a latitude or longitude.
const COORDINATE_KEYS: [&str; 8] = [
    "latitude",
    "longitude",
    "latitudedeg",
    "longitudedeg",
    "lat",
    "lon",
    "lng",
    "latdeg",
];

/// Scrubs a bundle in place (ADR-0013): see the module comment.
pub fn scrub(value: &mut Value) {
    scrub_under(value, "");
}

fn scrub_under(value: &mut Value, key: &str) {
    let name = key.to_ascii_lowercase().replace(['_', '-'], "");
    match value {
        Value::Object(map) => {
            for (child, inner) in map.iter_mut() {
                scrub_under(inner, child);
            }
        }
        Value::Array(items) => {
            for item in items {
                scrub_under(item, key);
            }
        }
        Value::Null | Value::Bool(_) => {}
        _ if SECRET_KEYS.contains(&name.as_str()) => *value = Value::String("[redacted]".into()),
        _ if IDENTIFYING_KEYS.contains(&name.as_str()) => {
            *value = Value::String("[removed]".into());
        }
        Value::Number(number) if COORDINATE_KEYS.contains(&name.as_str()) => {
            if let Some(degrees) = number.as_f64() {
                *value = serde_json::json!((degrees * 100.0).round() / 100.0);
            }
        }
        // Git build identities are public diagnostics, including full CI SHAs.
        // Secret-named fields above still win even when their value is hex.
        Value::String(text)
            if name == "commit"
                && (4..=40).contains(&text.len())
                && text.bytes().all(|byte| byte.is_ascii_hexdigit()) => {}
        Value::String(text) => *text = scrub_text(text),
        Value::Number(_) => {}
    }
}

/// Removes what looks like an address, a device path, an e-mail address, a
/// key or a token from free text such as a log line or an error message.
#[must_use]
pub fn scrub_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            out.push_str(&scrub_word(word));
            word.clear();
        }
    };
    for c in text.chars() {
        if c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | ',' | '(' | ')' | '[' | ']' | '{' | '}' | '=' | '<' | '>'
            )
        {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

fn scrub_word(word: &str) -> String {
    let bare = word.trim_end_matches(['.', ';', ':']);
    let tail = &word[bare.len()..];
    let lower = bare.to_ascii_lowercase();
    let replaced = if lower.starts_with("/dev/") || is_com_port(&lower) {
        Some("[device]")
    } else if [
        "/users/",
        "/home/",
        "c:\\users\\",
        "/var/folders/",
        "/private/",
    ]
    .iter()
    .any(|home| lower.starts_with(home))
    {
        Some("[path]")
    } else if bare.contains('@') && bare.contains('.') && !bare.starts_with('@') {
        Some("[email]")
    } else if is_ipv4(bare) || is_ipv6(bare) {
        Some("[address]")
    } else if looks_like_secret(bare) {
        Some("[redacted]")
    } else {
        None
    };
    replaced.map_or_else(|| word.to_owned(), |r| format!("{r}{tail}"))
}

fn is_com_port(word: &str) -> bool {
    word.strip_prefix("com")
        .is_some_and(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `192.168.1.20` or `192.168.1.20:14550`, optionally with a scheme.
fn is_ipv4(word: &str) -> bool {
    let word = word.split("//").last().unwrap_or(word);
    let host = word.split(['/', '?']).next().unwrap_or(word);
    let host = host.rsplit_once(':').map_or(host, |(h, port)| {
        if port.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
}

fn is_ipv6(word: &str) -> bool {
    let colons = word.matches(':').count();
    colons >= 2
        && word.bytes().all(|b| {
            b.is_ascii_hexdigit() || b == b':' || b == b'[' || b == b']' || b == b'%' || b == b'.'
        })
        && word.bytes().any(|b| b.is_ascii_hexdigit())
}

/// Long runs of letters and digits that are not ordinary words: API keys
/// (`sk-…`, `lin_api_…`), tokens and hashes. UUIDs are kept: they are the
/// ids a developer needs to match the bundle's parts.
fn looks_like_secret(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    if ["sk-", "lin_api_", "ghp_", "gho_", "xox", "aapt", "bearer"]
        .iter()
        .any(|p| lower.starts_with(p))
        && word.len() > 12
    {
        return true;
    }
    if is_uuid(word) {
        return false;
    }
    let token_chars = word
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'+' | b'/' | b'='));
    let digits = word.bytes().filter(u8::is_ascii_digit).count();
    token_chars && word.len() >= 32 && digits >= 4
}

fn is_uuid(word: &str) -> bool {
    word.len() == 36
        && word.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn invalid(why: &str) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, why.to_owned())
}

fn link_kind(kind: &LinkKind) -> &'static str {
    match kind {
        LinkKind::UdpListen { .. } => "udp-listen",
        LinkKind::Serial { .. } => "serial",
        LinkKind::TcpClient { .. } => "tcp-client",
        LinkKind::TcpServer { .. } => "tcp-server",
        LinkKind::UdpClient { .. } => "udp-client",
        LinkKind::UdpForward { .. } => "udp-forward",
        LinkKind::Simulator { .. } => "simulator",
        LinkKind::UsbReceiver {} => "usb-receiver",
    }
}

/// The longest suggestion title and text.
const MAX_TITLE: usize = 120;
const MAX_SUGGESTION: usize = 2000;

/// A suggestion card (S73): checked and given back to show. Nothing is sent;
/// only an operator's `feedback/send` sends.
pub(crate) fn suggest(params: &FeedbackSuggestParams) -> Result<Value, ApiError> {
    let title = params.title.trim();
    let text = params.text.trim();
    if title.is_empty() || title.chars().count() > MAX_TITLE {
        return Err(invalid("the title must be 1 to 120 characters"));
    }
    if text.is_empty() || text.chars().count() > MAX_SUGGESTION {
        return Err(invalid("the suggestion must be 1 to 2,000 characters"));
    }
    to_value(&SuggestionCard {
        title: title.to_owned(),
        text: text.to_owned(),
        sent: false,
    })
}

/// The files sent with a report, and its key, within what the API takes.
fn check_files(params: &FeedbackSendParams) -> Result<(), ApiError> {
    if params.voice_notes.len() > MAX_VOICE_NOTES {
        return Err(invalid("record up to 5 voice notes"));
    }
    if params
        .voice_notes
        .iter()
        .any(|v| v.data.len() > MAX_VOICE_B64)
    {
        return Err(invalid("a voice note is longer than 5 minutes"));
    }
    if params.voice_notes.iter().any(|v| {
        v.transcript
            .as_ref()
            .is_some_and(|t| t.chars().count() > MAX_TRANSCRIPT)
    }) {
        return Err(invalid(
            "a voice note's transcript is longer than 5,000 characters",
        ));
    }
    if params
        .screenshot
        .as_ref()
        .is_some_and(|s| s.len() > MAX_SCREENSHOT_B64)
    {
        return Err(invalid("the screenshot is larger than 3 MB"));
    }
    if params.attachments.len() > MAX_ATTACHMENTS {
        return Err(invalid("attach up to 10 files at a time"));
    }
    if params
        .attachments
        .iter()
        .map(|a| a.data.len())
        .sum::<usize>()
        > MAX_ATTACHMENTS_B64
    {
        return Err(invalid(
            "the attached files come to more than 25 MB. Remove some and send again",
        ));
    }
    if params.follow_up_key.as_ref().is_some_and(|key| {
        !(32..=128).contains(&key.len())
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }) {
        return Err(invalid("the follow-up key is not one UXDuck made"));
    }
    Ok(())
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    operator: bool,
) -> Result<Value, ApiError> {
    let ClientRequest::FeedbackSend(params) = request else {
        return Err(ApiError::new(ErrorKind::Internal, "not a feedback method"));
    };
    if !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may send feedback",
        ));
    }
    let text = params.text.trim().to_owned();
    // A voice note may say it all.
    if (text.is_empty() && params.voice_notes.is_empty()) || text.chars().count() > MAX_TEXT {
        return Err(invalid(
            "write up to 5,000 characters, or record a voice note",
        ));
    }
    if !is_uuid(&params.report_id) {
        return Err(invalid("the report id must be a UUID"));
    }
    check_files(&params)?;
    if params
        .reply_email
        .as_ref()
        .is_some_and(|e| e.len() > 254 || !e.contains('@'))
    {
        return Err(invalid("the reply address is not an e-mail address"));
    }
    let flight = match params.flight_data_vehicle_id.as_deref() {
        None => None,
        Some(id) => {
            let vehicle = engine
                .vehicle(id)
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))?;
            Some(FlightRef {
                link_id: vehicle.link_id.clone(),
                system_id: vehicle.system_id,
            })
        }
    };
    let bundle = if params.include_details {
        let mut value = serde_json::to_value(engine.bundle(connection, &params))
            .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?;
        scrub(&mut value);
        Some(value)
    } else {
        None
    };
    let order = FeedbackOrder {
        report_id: params.report_id.clone(),
        kind: params.kind,
        text,
        reply_email: params.reply_email.clone(),
        surface: params.surface,
        context: engine.context(&params),
        screenshot: params.screenshot.clone(),
        bundle,
        flight,
        credit: params.credit,
        from_agent: params.from_agent,
        attachments: params.attachments.clone(),
        follow_up_key: params.follow_up_key.clone(),
        voice_notes: params.voice_notes.clone(),
    };
    // Answered when the root reports back (`feedback_done`).
    if let Some(waiting) = engine.replying_to.take() {
        engine
            .awaiting_feedback
            .insert(params.report_id.clone(), waiting);
    }
    engine.feedback.send(order);
    Ok(Value::Null)
}

impl Engine {
    /// Where reports go; nothing is sent until the composition root sets it.
    pub fn set_feedback(&mut self, port: Box<dyn FeedbackPort>) {
        self.feedback = port;
    }

    /// The root finished with a report: answers the request that sent it.
    pub fn feedback_done(&mut self, report_id: &str, result: Result<FeedbackSendResult, ApiError>) {
        let Some((connection, request_id)) = self.awaiting_feedback.remove(report_id) else {
            return;
        };
        let frame = match result.and_then(|r| to_value(&r)) {
            Ok(value) => serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": value }),
            Err(error) => crate::rpc::router::error(&request_id, &error),
        };
        self.send(connection, &frame);
    }

    /// A suggestion this install sent shipped in this version (S73): said
    /// once, to everyone here.
    pub fn suggestion_shipped(&mut self, issue: &str, title: &str, version: &str) {
        self.raise(crate::commands::Raise {
            severity: Severity::Info,
            kind: NotificationKind::Persistent,
            subject: Subject::System,
            title: format!("Your suggestion shipped in UXDuck {version}"),
            body: Some(format!("{title} ({issue})")),
            actions: Vec::new(),
            condition: Some(format!("shipped:{issue}")),
            report: None,
        });
    }

    /// UXDuck's part of the bundle, before scrubbing.
    fn bundle(&self, connection: ConnectionId, params: &FeedbackSendParams) -> FeedbackBundle {
        let now = self.clock.now_us();
        let vehicles = self
            .state
            .vehicles
            .values()
            .map(|v| BundleVehicle {
                id: v.id.clone(),
                autopilot: v.autopilot.clone(),
                vehicle_type: v.vehicle_type.clone(),
                firmware: v.firmware.clone(),
                presence: format!("{:?}", v.presence).to_lowercase(),
                armed: v.armed,
                mode: v.mode.clone(),
                link: self
                    .state
                    .links
                    .get(&v.link_id)
                    .map_or("gone", |l| link_kind(&l.kind))
                    .to_owned(),
            })
            .collect();
        let links = self
            .state
            .links
            .values()
            .map(|l| BundleLink {
                kind: link_kind(&l.kind).to_owned(),
                status: format!("{:?}", l.status).to_lowercase(),
                detail: l.status_detail.clone(),
            })
            .collect();
        let mut notifications: Vec<BundleNotification> = self
            .state
            .notifications
            .values()
            .map(|n| BundleNotification {
                severity: format!("{:?}", n.severity).to_lowercase(),
                title: n.title.clone(),
                created_at_us: n.created_at_us,
            })
            .collect();
        notifications.sort_by_key(|n| std::cmp::Reverse(n.created_at_us));
        notifications.truncate(20);
        let settings = serde_json::to_value(self.settings_dto(&self.state.settings))
            .unwrap_or(Value::Object(Map::new()));
        let trace = self
            .connections
            .get(&connection)
            .map(|c| {
                c.recent
                    .iter()
                    .filter(|f| now.saturating_sub(f.ts_us) <= RECENT_US)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let chat = params.chat_thread_id.as_deref().map(|thread| {
            self.state
                .items_of(thread)
                .into_iter()
                .map(|item| BundleChatItem {
                    kind: match item.kind {
                        ItemKind::UserMessage => "user-message",
                        ItemKind::AgentMessage => "agent-message",
                        ItemKind::ToolCall => "tool-call",
                        ItemKind::Error => "error",
                    }
                    .to_owned(),
                    text: item.tool_call.as_ref().map_or_else(
                        || item.text.clone(),
                        |call| format!("{} {}", call.method, call.params),
                    ),
                })
                .collect()
        });
        FeedbackBundle {
            schema: BUNDLE_SCHEMA.to_owned(),
            report: BundleReport {
                id: params.report_id.clone(),
                kind: params.kind,
                time_us: now,
                surface: params.surface,
                daemon_version: Some(self.config.daemon_version.clone()),
                platform: None,
                arch: None,
            },
            ui: params.ui.clone(),
            daemon: Some(BundleDaemon {
                seq: self.state.seq,
                uptime_s: now.saturating_sub(self.started_us) / 1_000_000,
                vehicles,
                links,
                notifications,
                settings,
            }),
            trace,
            errors: params.errors.iter().take(20).cloned().collect(),
            log: Vec::new(),
            chat,
        }
    }

    /// "UXDuck 0.4.1 · Chrome 141 tab on UXDuck Desktop · window 1512×945 ·
    /// map open · 1 vehicle (ArduPilot Copter, udp-listen)". The root puts
    /// the platform first.
    fn context(&self, params: &FeedbackSendParams) -> String {
        // The page's own words: kept short, they are not checked.
        let short = |s: &str| s.chars().take(40).collect::<String>();
        let ui = &params.ui;
        let surface = match (params.surface, &ui.shell, &ui.browser) {
            (Surface::Desktop, Some(shell), _) => {
                format!("UXDuck Desktop {} window", short(&shell.version))
            }
            (Surface::Desktop, None, _) => "UXDuck Desktop window".to_owned(),
            (Surface::Browser, _, Some(browser)) => {
                format!("{} tab on UXDuck Desktop", short(browser))
            }
            (Surface::Browser, _, None) => "browser tab on UXDuck Desktop".to_owned(),
        };
        let mut parts = vec![format!("UXDuck {}", ui.app_version), surface];
        if let Some(window) = &ui.window {
            parts.push(format!("window {}", short(window)));
        }
        if !params.ui.panes.is_empty() {
            parts.push(format!("{} open", params.ui.panes.join(", ")));
        }
        let vehicles: Vec<String> = self
            .state
            .vehicles
            .values()
            .take(3)
            .map(|v| {
                let link = self
                    .state
                    .links
                    .get(&v.link_id)
                    .map_or("gone", |l| link_kind(&l.kind));
                format!("{} {}, {link}", v.autopilot, v.vehicle_type)
            })
            .collect();
        let count = self.state.vehicles.len();
        parts.push(match count {
            0 => "no vehicle".to_owned(),
            1 => format!("1 vehicle ({})", vehicles.join(", ")),
            n => format!("{n} vehicles ({})", vehicles.join(", ")),
        });
        parts.push("not signed in".to_owned());
        let line = parts.join(" · ");
        line.chars().take(250).collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn scrubbing_rounds_places_and_removes_secrets_addresses_and_devices() {
        let mut bundle = json!({
            "position": { "latitudeDeg": -35.363_261, "longitudeDeg": 149.165_23 },
            "lat": 51.5074,
            "link": { "host": "192.168.1.20", "device": "/dev/cu.usbmodem1101", "port": 14550 },
            "vehicle": { "uid": "000000001234abcd", "id": "5d0f7a3e-8c1b-5a2e-9f40-2b1c3d4e5f60" },
            "apiKey": "sk-proj-abcdef0123456789",
            "note": "sent to 10.0.0.5:14551 from COM4, mail ana@example.com, key sk-abc123def456ghi789",
            "token": 12345,
            "armed": true,
        });
        scrub(&mut bundle);
        assert_eq!(bundle["position"]["latitudeDeg"], json!(-35.36));
        assert_eq!(bundle["position"]["longitudeDeg"], json!(149.17));
        assert_eq!(bundle["lat"], json!(51.51));
        assert_eq!(bundle["link"]["host"], "[removed]");
        assert_eq!(bundle["link"]["device"], "[removed]");
        assert_eq!(bundle["link"]["port"], json!(14550));
        assert_eq!(bundle["vehicle"]["uid"], "[removed]");
        assert_eq!(
            bundle["vehicle"]["id"], "5d0f7a3e-8c1b-5a2e-9f40-2b1c3d4e5f60",
            "ids stay so the parts can be matched"
        );
        assert_eq!(bundle["apiKey"], "[redacted]");
        assert_eq!(bundle["token"], "[redacted]");
        assert_eq!(bundle["armed"], json!(true));
        assert_eq!(
            bundle["note"],
            "sent to [address] from [device], mail [email], key [redacted]"
        );
    }

    #[test]
    fn scrubbing_keeps_what_the_maps_say_about_themselves() {
        let failed = "failed basemapstyles-api.arcgis.com/arcgis/rest/services/styles/v2/styles/arcgis/imagery";
        let tile = "401 ibasemaps-api.arcgis.com/arcgis/rest/services/World_Imagery/MapServer/tile/{n}/{n}/{n}";
        let renderer =
            "ANGLE (Intel, Intel(R) UHD Graphics 620 (0x00003EA0) Direct3D11 vs_5_0 ps_5_0, D3D11)";
        let mut bundle = json!({
            "ui": {
                "maps": [{
                    "engine": "maplibre",
                    "variant": "map",
                    "size": "822×165",
                    "basemap": "esri-failed",
                    "renderer": renderer,
                    "problems": [failed, tile],
                }],
                "mapKeys": [{ "name": "arcgis", "state": "fetched" }, { "name": "google", "state": "saved", "failure": "unreachable" }],
            },
        });
        let before = bundle.clone();
        scrub(&mut bundle);
        assert_eq!(bundle, before);
    }

    #[test]
    fn free_text_keeps_ordinary_words_and_ids() {
        assert_eq!(
            scrub_text("INFO uxduck::daemon: listening port=45000 socket=/tmp/x.sock"),
            "INFO uxduck::daemon: listening port=45000 socket=/tmp/x.sock"
        );
        assert_eq!(
            scrub_text("link opened on /dev/ttyUSB0."),
            "link opened on [device]."
        );
        assert_eq!(scrub_text("peer [fe80::1]:14550"), "peer [[address]]:14550");
        assert_eq!(
            scrub_text("session 3f9a0c6b1d2e4f5a6b7c8d9e0f1a2b3c4d5e6f7a"),
            "session [redacted]"
        );
        assert_eq!(
            scrub_text("data_dir=/Users/ana/Library/Application Support"),
            "data_dir=[path] Support"
        );
    }

    #[test]
    fn kept_frames_are_recent_short_and_not_telemetry() {
        let frame = |ts_us: u64, method: &str, payload: Value| TraceFrame {
            ts_us,
            direction: uxduck_contracts::dto::trace::TraceDirection::In,
            kind: uxduck_contracts::dto::trace::FrameKind::Request,
            client_id: "c1".into(),
            transport: uxduck_contracts::dto::trace::Transport::WebSocket,
            method: Some(method.into()),
            id: None,
            bytes: 1,
            latency_us: None,
            payload,
        };
        let mut recent = VecDeque::new();
        keep(&mut recent, frame(1, "telemetry/update", json!({})), 1);
        assert!(recent.is_empty());
        keep(
            &mut recent,
            frame(
                1,
                "feedback/send",
                json!({ "screenshot": "A".repeat(10_000) }),
            ),
            1,
        );
        let kept = recent[0].payload["screenshot"].as_str().unwrap();
        assert!(kept.len() < 600 && kept.ends_with('…'));
        keep(
            &mut recent,
            frame(RECENT_US + 10, "command/send", json!({})),
            RECENT_US + 10,
        );
        assert_eq!(recent.len(), 1, "older than two minutes goes");
    }
}
