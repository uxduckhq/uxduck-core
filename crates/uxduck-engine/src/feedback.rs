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
    BundleVehicle, FeedbackAddParams, FeedbackAttachment, FeedbackBundle, FeedbackFileParams,
    FeedbackFiled, FeedbackKind, FeedbackProgress, FeedbackSendParams, FeedbackSendResult,
    FeedbackSuggestParams, FeedbackVoiceNote, SuggestionCard,
};
use uxduck_contracts::dto::trace::TraceFrame;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::ClientKind;
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
/// A kept frame still larger than this keeps only its outline: a window's
/// first state snapshot can be tens of megabytes of parameters.
const KEPT_FRAME: usize = 16 * 1024;
/// In an outline, an object with more entries than this is counted.
const OUTLINE_ENTRIES: usize = 50;
/// The most a bundle may be. The API takes 5 MB, and a smaller one goes out
/// sooner over a field connection.
pub const MAX_BUNDLE: usize = 2 * 1024 * 1024;
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
    /// Signed in, the name the person goes by here.
    pub reporter_name: Option<String>,
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
    /// Filed by the agent without the person reading it (S294): sent
    /// anonymously, whoever is signed in.
    pub agent_filed: bool,
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

/// More for a report the agent filed (S294): the root adds it with the
/// report's follow-up key, which it keeps.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedbackAddition {
    /// A UUID, so a retry adds nothing twice.
    pub id: String,
    pub report_id: String,
    pub text: String,
    /// Files the person added, base64.
    pub attachments: Vec<FeedbackAttachment>,
}

/// Where reports go; the root answers through [`Engine::feedback_done`].
pub trait FeedbackPort: Send {
    fn send(&mut self, order: FeedbackOrder);
    /// Adds to a report the agent filed; answered by the addition's id.
    fn add(&mut self, addition: FeedbackAddition);
    /// Whether reports go out from here. The browser worker's page sends
    /// its own, so the agent cannot file one there.
    fn sends(&self) -> bool {
        true
    }
}

/// No sender: every report is refused. For tests and the browser worker.
#[derive(Debug, Default)]
pub struct NoFeedback;

impl FeedbackPort for NoFeedback {
    fn send(&mut self, _: FeedbackOrder) {}
    fn add(&mut self, _: FeedbackAddition) {}
    fn sends(&self) -> bool {
        false
    }
}

/// Keeps a frame for the connection's report: the safe side only (the tap
/// has redacted it), long strings cut, the high-rate streams left out.
pub(crate) fn keep(recent: &mut VecDeque<TraceFrame>, mut frame: TraceFrame, now_us: u64) {
    if matches!(
        frame.method.as_deref(),
        Some("telemetry/update" | "agent/itemDelta" | "trace/frame" | "feedback/progress")
    ) {
        return;
    }
    cut_strings(&mut frame.payload);
    if frame.bytes > KEPT_FRAME as u64 && json_len(&frame.payload) > KEPT_FRAME {
        outline(&mut frame.payload);
    }
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

/// What is left of a frame too large to keep: its fields, with each list,
/// and each object of many entries, replaced by how many it held, such as
/// `"[166447 items]"`.
fn outline(value: &mut Value) {
    match value {
        Value::Array(items) => *value = Value::String(format!("[{} items]", items.len())),
        Value::Object(map) if map.len() > OUTLINE_ENTRIES => {
            *value = Value::String(format!("{{{} entries}}", map.len()));
        }
        Value::Object(map) => map.values_mut().for_each(outline),
        _ => {}
    }
}

fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

/// Fits a whole bundle within [`MAX_BUNDLE`]: the oldest trace frames go
/// first, then the oldest log lines, then the chat's oldest items, so the
/// report is never refused for its details.
pub fn fit(bundle: &mut Value) {
    let mut size = json_len(bundle);
    for part in ["trace", "log", "chat"] {
        let Some(items) = bundle.get_mut(part).and_then(Value::as_array_mut) else {
            continue;
        };
        let mut gone = 0;
        while size > MAX_BUNDLE && gone < items.len() {
            size = size.saturating_sub(json_len(&items[gone]) + 1);
            gone += 1;
        }
        items.drain(..gone);
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
        reporter_name: params
            .reporter_name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned),
        surface: params.surface,
        context: engine.context(&params),
        screenshot: params.screenshot.clone(),
        bundle,
        flight,
        credit: params.credit,
        from_agent: params.from_agent,
        agent_filed: false,
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

/// How many reports the agent may file in one chat, and additions to them.
const MAX_FILED: usize = 3;
const MAX_ADDED: usize = 10;

/// A report the agent files by itself (S294): the title and text only,
/// scrubbed as a bundle's free text is, sent anonymously with a follow-up key
/// the root keeps so the agent can add to it from the same chat.
pub(crate) fn file(engine: &mut Engine, params: &FeedbackFileParams) -> Result<Value, ApiError> {
    let thread = agent_chat(engine, params.thread_id.as_deref())?;
    let title = scrub_text(params.title.trim());
    let text = scrub_text(params.text.trim());
    if title.is_empty() || title.chars().count() > MAX_TITLE {
        return Err(invalid("the title must be 1 to 120 characters"));
    }
    if text.is_empty() || text.chars().count() > MAX_SUGGESTION {
        return Err(invalid("the text must be 1 to 2,000 characters"));
    }
    let filed = calls_in(engine, &thread, "feedback/file")
        + engine
            .agent_filing
            .values()
            .filter(|t| **t == thread)
            .count();
    if filed >= MAX_FILED {
        return Err(ApiError::new(
            ErrorKind::Busy,
            "three reports were filed from this chat already: add to one with feedback/add",
        ));
    }
    let report_id = uuid(&engine.random.token());
    let order = FeedbackOrder {
        report_id: report_id.clone(),
        kind: params.kind,
        text: format!("{title}\n\n{text}"),
        reply_email: None,
        reporter_name: None,
        surface: engine.surface(),
        context: engine.agent_context(),
        screenshot: None,
        bundle: None,
        flight: None,
        credit: false,
        from_agent: true,
        agent_filed: true,
        attachments: Vec::new(),
        follow_up_key: Some(engine.random.token()),
        voice_notes: Vec::new(),
    };
    if let Some(waiting) = engine.replying_to.take() {
        engine.awaiting_feedback.insert(report_id.clone(), waiting);
    }
    engine.agent_filing.insert(report_id, thread);
    engine.feedback.send(order);
    Ok(Value::Null)
}

/// More for a report filed from the same chat (S294): the agent's words,
/// scrubbed, or files the person added from the chat's card.
pub(crate) fn add(
    engine: &mut Engine,
    params: &FeedbackAddParams,
    operator: bool,
) -> Result<Value, ApiError> {
    let thread = agent_chat(engine, params.thread_id.as_deref())?;
    if !filed_in(engine, &thread).contains(&params.report_id) {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            "no report with that id was filed from this chat",
        ));
    }
    if !operator && !params.attachments.is_empty() {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only the person can attach files to a report",
        ));
    }
    let text = if operator {
        params.text.trim().to_owned()
    } else {
        scrub_text(params.text.trim())
    };
    if (text.is_empty() && params.attachments.is_empty()) || text.chars().count() > MAX_SUGGESTION {
        return Err(invalid("add 1 to 2,000 characters, or a file"));
    }
    if params.attachments.len() > MAX_ATTACHMENTS
        || params
            .attachments
            .iter()
            .map(|a| a.data.len())
            .sum::<usize>()
            > MAX_ATTACHMENTS_B64
    {
        return Err(invalid("attach up to 10 files, 25 MB together"));
    }
    if calls_in(engine, &thread, "feedback/add") >= MAX_ADDED {
        return Err(ApiError::new(
            ErrorKind::Busy,
            "enough was added from this chat already",
        ));
    }
    let id = uuid(&engine.random.token());
    if let Some(waiting) = engine.replying_to.take() {
        engine.awaiting_feedback.insert(id.clone(), waiting);
    }
    engine.feedback.add(FeedbackAddition {
        id,
        report_id: params.report_id.clone(),
        text,
        attachments: params.attachments.clone(),
    });
    Ok(Value::Null)
}

/// The chat a report is filed from, where reports go out from here.
fn agent_chat(engine: &Engine, thread: Option<&str>) -> Result<String, ApiError> {
    if !engine.feedback.sends() {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Automatic reporting isn't available in this browser session. Use feedback_suggest \
             to prepare a draft for the person to review and send. Nothing has been sent.",
        ));
    }
    thread
        .filter(|id| engine.state.threads.contains_key(*id))
        .map(str::to_owned)
        .ok_or_else(|| invalid("reports are filed from a UXDuck chat"))
}

/// How many calls of a method a chat holds, whatever became of them.
fn calls_in(engine: &Engine, thread: &str, method: &str) -> usize {
    engine
        .state
        .items_of(thread)
        .into_iter()
        .filter(|item| item.tool_call.as_ref().is_some_and(|c| c.method == method))
        .count()
}

/// The reports the agent filed in a chat, from its calls' results.
fn filed_in(engine: &Engine, thread: &str) -> Vec<String> {
    engine
        .state
        .items_of(thread)
        .into_iter()
        .filter_map(|item| item.tool_call.as_ref())
        .filter(|call| call.method == "feedback/file")
        .filter_map(|call| serde_json::from_str::<FeedbackFiled>(call.result.as_deref()?).ok())
        .map(|filed| filed.report_id)
        .collect()
}

/// A random token's first 32 hex digits as a version 4 UUID.
fn uuid(token: &str) -> String {
    let hex = token
        .get(..32)
        .unwrap_or("00000000000000000000000000000000");
    format!(
        "{}-{}-4{}-8{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
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
        // Filed by the agent: answered with the report's id, to add to it.
        let filed = self.agent_filing.remove(report_id).is_some();
        let frame = match result.and_then(|r| {
            if filed {
                to_value(&FeedbackFiled {
                    report_id: report_id.to_owned(),
                    status: r.status,
                    issue: r.issue,
                })
            } else {
                to_value(&r)
            }
        }) {
            Ok(value) => serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": value }),
            Err(error) => crate::rpc::router::error(&request_id, &error),
        };
        self.send(connection, &frame);
    }

    /// How much of a report the root has sent so far: told to the page
    /// waiting on its `feedback/send`. Not for the agent's own reports.
    pub fn feedback_progress(&mut self, report_id: &str, sent: u64, total: u64) {
        if self.agent_filing.contains_key(report_id) {
            return;
        }
        let Some(&(connection, _)) = self.awaiting_feedback.get(report_id) else {
            return;
        };
        let frame = crate::rpc::router::notification(&ServerNotification::FeedbackProgress(
            FeedbackProgress {
                report_id: report_id.to_owned(),
                sent,
                total,
            },
        ));
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

    /// Where a report from the chat is said to come from: UXDuck Desktop
    /// when its shell is connected.
    fn surface(&self) -> Surface {
        let shell = self.connections.values().any(|c| {
            c.admitted
                .as_ref()
                .is_some_and(|a| a.kind == ClientKind::Shell)
        });
        if shell {
            Surface::Desktop
        } else {
            Surface::Browser
        }
    }

    /// "UXDuck 0.4.1 · filed from a chat by the agent · 1 vehicle
    /// (ArduPilot Copter, udp-listen)": nothing about the page or the person.
    fn agent_context(&self) -> String {
        [
            format!("UXDuck {}", self.config.daemon_version),
            "filed from a chat by the agent".to_owned(),
            self.vehicles_line(),
        ]
        .join(" · ")
    }

    /// "1 vehicle (ArduPilot Copter, udp-listen)": their autopilots, types
    /// and links, up to three.
    fn vehicles_line(&self) -> String {
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
        match self.state.vehicles.len() {
            0 => "no vehicle".to_owned(),
            1 => format!("1 vehicle ({})", vehicles.join(", ")),
            n => format!("{n} vehicles ({})", vehicles.join(", ")),
        }
    }

    /// UXDuck's part of the bundle, before scrubbing.
    #[allow(
        clippy::too_many_lines,
        reason = "the bundle's parts, read top to bottom"
    )]
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
                connections: v.connections.as_ref().map(Into::into),
            })
            .collect();
        let links = self
            .state
            .links
            .values()
            .map(|l| BundleLink {
                id: l.id.clone(),
                kind: link_kind(&l.kind).to_owned(),
                status: format!("{:?}", l.status).to_lowercase(),
                detail: l.status_detail.clone(),
                transport: self
                    .system
                    .transport_diagnostics(&l.id)
                    .as_ref()
                    .map(Into::into),
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
    /// 1 vehicle (ArduPilot Copter, udp-listen) · not signed in · map open".
    /// The root puts the platform first.
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
        parts.push(self.vehicles_line());
        parts.push("not signed in".to_owned());
        fitted(&parts, &params.ui.panes)
    }
}

/// The context line in 250 characters, with what is open last: listed while
/// it fits, else counted, so the vehicles are never what is cut off.
fn fitted(parts: &[String], panes: &[String]) -> String {
    const MAX: usize = 250;
    let mut line = parts.join(" · ");
    if !panes.is_empty() {
        let listed = format!(" · {} open", panes.join(", "));
        let tail = if line.chars().count() + listed.chars().count() <= MAX {
            listed
        } else {
            format!(" · {} tabs open", panes.len())
        };
        line.push_str(&tail);
    }
    line.chars().take(MAX).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_long_tab_list_is_counted_and_never_cuts_off_the_vehicles() {
        let parts = [
            "UXDuck 0.7.2".to_owned(),
            "Chrome 156 tab on UXDuck Desktop".to_owned(),
            "1 vehicle (Ardupilotmega Quadrotor, udp-listen)".to_owned(),
            "not signed in".to_owned(),
        ];
        let few = ["map".to_owned(), "control".to_owned()];
        assert_eq!(
            fitted(&parts, &few),
            "UXDuck 0.7.2 · Chrome 156 tab on UXDuck Desktop · 1 vehicle \
             (Ardupilotmega Quadrotor, udp-listen) · not signed in · map, control open"
        );
        let many: Vec<String> = (0..40).map(|i| format!("pane-{i}")).collect();
        let line = fitted(&parts, &many);
        assert!(
            line.contains("1 vehicle (Ardupilotmega Quadrotor, udp-listen)"),
            "{line}"
        );
        assert!(line.ends_with(" · 40 tabs open"), "{line}");
        assert!(line.chars().count() <= 250);
        assert_eq!(fitted(&parts, &[]), parts.join(" · "));
    }

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

    /// A frame as it came, its size on the wire its payload's.
    fn frame(ts_us: u64, method: &str, payload: Value) -> TraceFrame {
        TraceFrame {
            ts_us,
            direction: uxduck_contracts::dto::trace::TraceDirection::In,
            kind: uxduck_contracts::dto::trace::FrameKind::Request,
            client_id: "c1".into(),
            transport: uxduck_contracts::dto::trace::Transport::WebSocket,
            method: Some(method.into()),
            id: None,
            bytes: payload.to_string().len() as u64,
            latency_us: None,
            payload,
        }
    }

    #[test]
    fn kept_frames_are_recent_short_and_not_telemetry() {
        let mut recent = VecDeque::new();
        keep(&mut recent, frame(1, "telemetry/update", json!({})), 1);
        keep(&mut recent, frame(1, "feedback/progress", json!({})), 1);
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

    /// A report sent just after the window connected carried its state
    /// snapshot, 32 MB of parameters, and the API refused the whole report.
    #[test]
    fn a_frame_too_large_to_keep_keeps_its_outline_and_size() {
        let parameters: Vec<Value> = (0..5000)
            .map(|n| json!({ "vehicleId": "v1", "name": format!("PARAM_{n}"), "value": n }))
            .collect();
        let snapshot = frame(
            1,
            "state/subscribe",
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "result": { "seq": 42, "snapshot": { "parameters": parameters, "threads": [] } },
            }),
        );
        let bytes = snapshot.bytes;
        let mut recent = VecDeque::new();
        keep(&mut recent, snapshot, 1);
        let kept = &recent[0];
        assert_eq!(kept.bytes, bytes, "the bundle still says how large it was");
        assert_eq!(
            kept.payload,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "result": {
                    "seq": 42,
                    "snapshot": { "parameters": "[5000 items]", "threads": "[0 items]" },
                },
            })
        );

        // Large only for a long string: cut, and kept whole.
        keep(
            &mut recent,
            frame(
                1,
                "feedback/send",
                json!({ "screenshot": "A".repeat(100_000), "attachments": [{ "name": "a.pdf" }] }),
            ),
            1,
        );
        assert_eq!(recent[1].payload["attachments"][0]["name"], "a.pdf");
    }

    #[test]
    fn a_bundle_is_fitted_within_its_budget_oldest_first() {
        let frames: Vec<Value> = (0..400)
            .map(|n| json!({ "tsUs": n, "payload": "y".repeat(10_000) }))
            .collect();
        let line = |n: usize| format!("{n:05} {}", "x".repeat(400));
        let mut bundle = json!({
            "schema": BUNDLE_SCHEMA,
            "trace": frames,
            "log": (0..500).map(line).collect::<Vec<_>>(),
            "chat": [{ "kind": "user-message", "text": "Why did it climb?" }],
        });
        fit(&mut bundle);
        assert!(json_len(&bundle) <= MAX_BUNDLE);
        let trace = bundle["trace"].as_array().unwrap();
        assert!(!trace.is_empty() && trace.len() < 400, "{}", trace.len());
        assert_eq!(trace.last().unwrap()["tsUs"], 399, "the newest frames stay");
        assert_eq!(
            bundle["log"].as_array().unwrap().len(),
            500,
            "the log is whole while dropping frames makes room"
        );
        assert_eq!(bundle["chat"].as_array().unwrap().len(), 1);

        let mut logged = json!({
            "schema": BUNDLE_SCHEMA,
            "trace": [],
            "log": (0..10_000).map(line).collect::<Vec<_>>(),
        });
        fit(&mut logged);
        assert!(json_len(&logged) <= MAX_BUNDLE);
        let log = logged["log"].as_array().unwrap();
        assert!(log.len() < 10_000);
        assert_eq!(log.last().unwrap().as_str(), Some(line(9999).as_str()));

        let mut small =
            json!({ "schema": BUNDLE_SCHEMA, "trace": [{ "tsUs": 1 }], "log": ["one"] });
        let before = small.clone();
        fit(&mut small);
        assert_eq!(small, before, "a bundle that fits is left as it is");
    }
}
