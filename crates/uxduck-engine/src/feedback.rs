//! Feedback (ADR-0016 O8, O11): UXDuck's part of the diagnostics bundle,
//! scrubbed as ADR-0013 says, handed to the composition root to send or keep
//! while offline. The request is answered when the root reports back.
//!
//! Scrubbing is by field and by pattern, over the whole bundle: coordinates
//! are rounded to 0.01° (about 1 km), and secrets, addresses, device paths,
//! serial numbers and e-mail addresses are removed. The screenshot cannot be
//! scrubbed, which is why the person sees it and can remove it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use serde_json::{Map, Value};
use uxduck_contracts::dto::analytics::Surface;
use uxduck_contracts::dto::feedback::{
    BUNDLE_SCHEMA, BundleChatItem, BundleDaemon, BundleLink, BundleNotification, BundleReport,
    BundleVehicle, FeedbackAddParams, FeedbackAttachment, FeedbackBundle, FeedbackFileParams,
    FeedbackFiled, FeedbackKind, FeedbackLeftOut, FeedbackProgress, FeedbackSendParams,
    FeedbackSendResult, FeedbackSuggestParams, FeedbackVoiceNote, LeftOut, SuggestionCard,
};
use uxduck_contracts::dto::trace::TraceFrame;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::ClientKind;
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::link_failure::LinkFailure;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::thread::ItemKind;
use uxduck_core::domain::video::without_login;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// How long a client's frames are kept for its report, and at most how many:
/// a ring in memory for every connection, so that a report can say what its
/// window did just before. The form says "the last two minutes".
pub(crate) const RECENT_US: u64 = 120_000_000;
pub(crate) const RECENT_FRAMES: usize = 200;
/// Longer strings in a kept frame are cut and end in "…", such as a
/// screenshot being sent: the ring holds what a window did, not its files.
const KEPT_STRING: usize = 512;
/// A kept frame still larger than this keeps only its outline: a window's
/// first state snapshot can be tens of megabytes of parameters.
const KEPT_FRAME: usize = 16 * 1024;
/// In an outline, an object with more entries than this is counted.
const OUTLINE_ENTRIES: usize = 50;
/// The most UXDuck's servers take of a bundle (`LIMITS.bundle` in
/// `apps/api/src/features/feedback/report.ts`): they refuse the whole report
/// for a larger one, the person's words with it, so a bundle is fitted to
/// this and says what was left out. Until 2026-11-15 a bundle was fitted to
/// 2 MB, a number of UXDuck's own, and said nothing.
pub const MAX_BUNDLE: usize = 5 * 1024 * 1024;
/// Room a fitted bundle keeps for its record of what was left out.
const LEFT_OUT_ROOM: usize = 2048;
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
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a field of the report the API reads"
)]
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
    /// Filed by an agent outside UXDuck, such as Claude Code over MCP,
    /// from its connection instead of a chat.
    pub over_mcp: bool,
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
/// Endpoints leave it here, before it is kept: the links' and any the
/// frame itself names, such as a link it adds (FB-267).
pub(crate) fn keep(
    recent: &mut VecDeque<TraceFrame>,
    mut frame: TraceFrame,
    now_us: u64,
    endpoints: &Endpoints,
) {
    if matches!(
        frame.method.as_deref(),
        Some("telemetry/update" | "agent/itemDelta" | "trace/frame" | "feedback/progress")
    ) {
        return;
    }
    cut_strings(&mut frame.payload);
    let mut named = Endpoints::default();
    endpoints_in(&frame.payload, &mut named);
    redact_endpoints(&mut frame.payload, &[endpoints, &named]);
    if frame.kind == uxduck_contracts::dto::trace::FrameKind::Request {
        without_listener_ports(&mut frame.payload);
    }
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

/// A bundle's lists that give way when it is over [`MAX_BUNDLE`], in the
/// order they do: where each is, whether its newest comes first, and the
/// field that holds an item's time.
const FITTED: [(&str, bool, &str); 5] = [
    ("/trace", false, "tsUs"),
    ("/log", false, ""),
    ("/chat", false, ""),
    ("/errors", false, "atUs"),
    ("/daemon/notifications", true, "createdAtUs"),
];

/// Fits a whole bundle within [`MAX_BUNDLE`], so the report is never
/// refused for its details: the oldest trace frames go first, then the
/// oldest log lines, the chat's oldest items, the oldest page errors and
/// the oldest notifications. The newest of each stays, being nearest the
/// problem.
///
/// What went is written in the bundle's `leftOut`, how much of each and
/// from when to when, beside what `left_out` already holds: what the
/// composition root left out of the flight data. It is also the answer,
/// for the person sending the report: all of it `None` when nothing was
/// left out.
pub fn fit(bundle: &mut Value, mut left_out: FeedbackLeftOut) -> FeedbackLeftOut {
    let mut size = json_len(bundle);
    if size > MAX_BUNDLE {
        for (part, newest_first, stamp) in FITTED {
            let Some(items) = bundle.pointer_mut(part).and_then(Value::as_array_mut) else {
                continue;
            };
            let (mut gone, mut bytes) = (0, 0);
            while size > MAX_BUNDLE - LEFT_OUT_ROOM && gone < items.len() {
                let oldest = if newest_first {
                    items.len() - 1 - gone
                } else {
                    gone
                };
                let len = json_len(&items[oldest]) + 1;
                size = size.saturating_sub(len);
                bytes += len;
                gone += 1;
            }
            if gone == 0 {
                continue;
            }
            let went: Vec<Value> = if newest_first {
                items.split_off(items.len() - gone)
            } else {
                items.drain(..gone).collect()
            };
            let stamps = || went.iter().filter_map(|item| item.get(stamp)?.as_u64());
            let record = Some(LeftOut {
                count: gone as u64,
                bytes: bytes as u64,
                from_us: stamps().min(),
                to_us: stamps().max(),
                limit_bytes: MAX_BUNDLE as u64,
            });
            match part {
                "/trace" => left_out.trace = record,
                "/log" => left_out.log = record,
                "/chat" => left_out.chat = record,
                "/errors" => left_out.errors = record,
                _ => left_out.notifications = record,
            }
        }
    }
    if left_out != FeedbackLeftOut::default()
        && let Ok(record) = serde_json::to_value(&left_out)
    {
        bundle["leftOut"] = record;
    }
    left_out
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

/// Where links are configured to reach or listen, kept for leaving out of
/// reports: the scrub finds addresses by their shape, and a name such as
/// `drone.local` has none (FB-267). Each host is kept once, as it is
/// compared, with its ports, so a text is scanned once however many links
/// there are. This computer's own addresses say nothing about the person
/// and are not kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Endpoints(BTreeMap<String, BTreeSet<u16>>);

impl Endpoints {
    /// Keeps where a link opens, if that is another computer.
    pub(crate) fn note(&mut self, kind: &LinkKind) {
        let (LinkKind::UdpListen { host, port }
        | LinkKind::UdpForward { host, port }
        | LinkKind::TcpServer { host, port }
        | LinkKind::TcpClient { host, port }
        | LinkKind::UdpClient { host, port }) = kind
        else {
            return;
        };
        self.insert(host, *port);
    }

    fn insert(&mut self, host: &str, port: u16) {
        let host = normalized_host(host);
        if !is_local(&host) {
            self.0.entry(host).or_default().insert(port);
        }
    }
}

/// The endpoints of the links configured now, shared with whatever keeps
/// diagnostics for a report, such as the daemon's log lines. Each kept
/// trace frame and log line leaves them out as it is kept, so only the
/// links configured now are remembered, never every link of the run
/// (FB-267).
#[derive(Clone, Debug, Default)]
pub struct ActiveEndpoints(Arc<RwLock<Endpoints>>);

impl ActiveEndpoints {
    /// Becomes the endpoints of these link kinds.
    pub(crate) fn set<'a>(&self, kinds: impl IntoIterator<Item = &'a LinkKind>) {
        let mut endpoints = Endpoints::default();
        for kind in kinds {
            endpoints.note(kind);
        }
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = endpoints;
    }

    /// The endpoints now.
    pub fn read(&self) -> RwLockReadGuard<'_, Endpoints> {
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }
}

impl FromIterator<(String, u16)> for Endpoints {
    fn from_iter<I: IntoIterator<Item = (String, u16)>>(pairs: I) -> Self {
        let mut endpoints = Self::default();
        for (host, port) in pairs {
            endpoints.insert(&host, port);
        }
        endpoints
    }
}

/// `host` as it is compared, the same for a configured host and one in
/// text: in lower case, Unicode letters included, and without brackets. An IPv6 address loses its
/// scope and is written the standard way. A DNS name loses the dot that
/// makes it absolute. Empty for a host that names nothing, such as "%",
/// "[]" or ".".
fn normalized_host(host: &str) -> String {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_lowercase();
    let address = host
        .split_once('%')
        .map_or(host.as_str(), |(address, _)| address);
    match address.parse::<std::net::Ipv6Addr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => host.trim_end_matches('.').to_owned(),
    }
}

/// Whether a normalized host is this computer, or names nothing.
fn is_local(host: &str) -> bool {
    host.is_empty()
        || host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// The endpoints `value` holds as an object's `host` and `port`.
fn endpoints_in(value: &Value, found: &mut Endpoints) {
    match value {
        Value::Object(map) => {
            if let Some((host, port)) = endpoint_of(map) {
                found.insert(host, port);
            }
            map.values().for_each(|v| endpoints_in(v, found));
        }
        Value::Array(items) => items.iter().for_each(|v| endpoints_in(v, found)),
        _ => {}
    }
}

/// A listener's port in a request, which a refused listener never turns
/// into a link (FB-267): "[removed]", as its host is when it is another
/// computer's.
fn without_listener_ports(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if matches!(
                map.get("type").and_then(Value::as_str),
                Some("udp-listen" | "tcp-server")
            ) && map.get("port").is_some_and(Value::is_u64)
            {
                map.insert("port".into(), Value::String("[removed]".into()));
            }
            map.values_mut().for_each(without_listener_ports);
        }
        Value::Array(items) => items.iter_mut().for_each(without_listener_ports),
        _ => {}
    }
}

/// An object's `host` and `port`, when the host is another computer.
fn endpoint_of(map: &serde_json::Map<String, Value>) -> Option<(&str, u16)> {
    let host = map.get("host")?.as_str()?;
    let port = u16::try_from(map.get("port")?.as_u64()?).ok()?;
    (!is_local(&normalized_host(host))).then_some((host, port))
}

/// Leaves endpoints out of a report (FB-267): an object's `host` with its
/// `port`, unless the host is this computer, each endpoint of `known`
/// where text names it, as [`without_endpoints`] says, and any endpoint a
/// link's name is written with. Other words and numbers stay, even one that
/// matches a host or a port.
pub fn redact_endpoints(value: &mut Value, known: &[&Endpoints]) {
    match value {
        Value::Object(map) => {
            if endpoint_of(map).is_some() {
                map.insert("host".into(), Value::String("[removed]".into()));
                map.insert("port".into(), Value::String("[removed]".into()));
            }
            if map.get("kind").and_then(|kind| kind.get("type")).is_some()
                && let Some(Value::String(name)) = map.get_mut("name")
            {
                *name = name_without_endpoints(name);
            }
            map.values_mut().for_each(|v| redact_endpoints(v, known));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| redact_endpoints(v, known)),
        Value::String(text) => *text = without_endpoints(text, known),
        _ => {}
    }
}

/// Text without the endpoints of `known` that UXDuck and the system write,
/// matched without regard to case, in one pass (FB-267):
/// `drone.local:5760`, `tcp:drone.local:5760`, `drone.local (port 5760)`,
/// an IPv6 address with or without brackets and with any scope, as
/// `fe80::1%lo:5760` or `[fe80::1%2]:5760`, and the host of a URL whatever
/// its port. A host or a port standing alone in other words stays. Run it
/// before the scrub, which would hide an address and leave its port.
#[must_use]
pub fn without_endpoints(text: &str, known: &[&Endpoints]) -> String {
    if known.iter().all(|endpoints| endpoints.0.is_empty()) {
        return text.to_owned();
    }
    without_endpoints_where(text, &|host, port| known_at(known, host, port))
}

/// A link's name without any endpoint it is written with, known or not: a
/// name can keep a host after the link is edited away from it (FB-267). A
/// host is a name with a letter or a dot, or an IPv6 address, so "2:30"
/// stays. The scrub hides a bracketed IPv6 address after this.
fn name_without_endpoints(name: &str) -> String {
    without_endpoints_where(name, &|host, _| {
        let host = normalized_host(host);
        host.chars()
            .any(|c| c.is_alphabetic() || c == '.' || c == ':')
    })
}

/// Text without each endpoint, as [`without_endpoints`] reads them, that
/// `wanted` takes, given its host as written and its port if it has one.
fn without_endpoints_where(text: &str, wanted: &dyn Fn(&str, Option<u16>) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        if !host_char(c) {
            at += c.len_utf8();
            continue;
        }
        let start = at;
        at = text[at..]
            .find(|c: char| !host_char(c))
            .map_or(text.len(), |n| at + n);
        let found = if text[..at].ends_with(':') && text[at..].starts_with("//") {
            let (span, host, end) = url_authority(text, at + 2);
            at = end;
            wanted(&host.replace("%25", "%"), None).then_some(span)
        } else {
            mention(text, start, at, wanted)
        };
        if let Some(span) = found {
            out.push_str(&text[copied..span.start]);
            out.push_str("[endpoint]");
            copied = span.end;
            at = at.max(span.end);
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// A character that can be part of a host and its port as text writes
/// them, a letter of any script included, as in `café.local`.
fn host_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%')
}

/// Whether `host`, as text writes it, is one of `known` at `port`, or
/// at any port when there is none.
fn known_at(known: &[&Endpoints], host: &str, port: Option<u16>) -> bool {
    let host = normalized_host(host);
    known.iter().any(|endpoints| {
        endpoints
            .0
            .get(&host)
            .is_some_and(|ports| port.is_none_or(|port| ports.contains(&port)))
    })
}

/// A port as text writes it: digits only.
fn port_of(digits: &str) -> Option<u16> {
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// Where the run of host characters at `start..end` names an endpoint
/// `wanted` takes: `[host]:port`, `host:port` after a prefix such as
/// `tcp:` or not, or `host (port N)`.
fn mention<'a>(
    text: &'a str,
    start: usize,
    end: usize,
    wanted: &dyn Fn(&str, Option<u16>) -> bool,
) -> Option<Range<usize>> {
    let run = text[start..end].trim_end_matches('.');
    let run_end = start + run.len();
    if run_end == end
        && start > 0
        && text.as_bytes()[start - 1] == b'['
        && text[end..].starts_with(']')
    {
        let digits = text[end + 1..].strip_prefix(':').map_or("", |after| {
            &after[..after.bytes().take_while(u8::is_ascii_digit).count()]
        });
        if let Some(port) = port_of(digits)
            && wanted(run, Some(port))
        {
            return Some(start - 1..end + 2 + digits.len());
        }
    }
    let hosts = |host: &'a str| [Some(host), host.rsplit_once(':').map(|(_, last)| last)];
    if let Some((host, digits)) = run.rsplit_once(':')
        && let Some(port) = port_of(digits)
        && let Some(host) = hosts(host)
            .into_iter()
            .flatten()
            .find(|host| wanted(host, Some(port)))
    {
        return Some(run_end - digits.len() - 1 - host.len()..run_end);
    }
    let named = text[end..].strip_prefix(" (port ")?;
    let digits = &named[..named.bytes().take_while(u8::is_ascii_digit).count()];
    let port = port_of(digits)?;
    if !named[digits.len()..].starts_with(')') {
        return None;
    }
    let host = hosts(run)
        .into_iter()
        .flatten()
        .find(|host| wanted(host, Some(port)))?;
    Some(run_end - host.len()..end + " (port )".len() + digits.len())
}

/// A URL's authority from just after `://`: the span of its host with its
/// port, the host alone, and where the authority ends. A dot that ends a
/// sentence is not part of it, nor is a `]` around the whole URL, while
/// the brackets around an IPv6 address are.
fn url_authority(text: &str, from: usize) -> (Range<usize>, &str, usize) {
    let mut end = text.len();
    let mut literal = false;
    for (n, c) in text[from..].char_indices() {
        match c {
            '[' => literal = true,
            ']' if literal => literal = false,
            ']' | '/' | '?' | '#' | '"' | '\'' | '<' | '>' | ',' | ')' => {
                end = from + n;
                break;
            }
            c if c.is_whitespace() => {
                end = from + n;
                break;
            }
            _ => {}
        }
    }
    let start = text[from..end].rfind('@').map_or(from, |n| from + n + 1);
    let end = start + text[start..end].trim_end_matches('.').len();
    let host_port = &text[start..end];
    let host = match host_port.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(host, _)| host),
        None => host_port
            .rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
            .map_or(host_port, |(host, _)| host),
    };
    (start..end, host, end)
}

/// Removes what looks like an address, a device path, an e-mail address, a
/// key or a token from free text such as a log line or an error message.
#[must_use]
pub fn scrub_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            out.push_str(&scrub_spaceless(&text[start..i]));
            out.push(c);
            start = i + c.len_utf8();
        }
    }
    out.push_str(&scrub_spaceless(&text[start..]));
    out
}

/// Text without whitespace, scrubbed word by word. A camera's address also
/// loses its login, as it does on its way to the account (FB-229), and a
/// word the scrub hid in the address as written stays hidden.
fn scrub_spaceless(text: &str) -> String {
    let whole = without_bracketed_ipv6(text);
    let text = whole.as_str();
    let (words, separators) = words_of(text);
    let scrubbed = joined(words.iter().map(|word| scrub_word(word)), &separators);
    let Some(at) = text.find("://") else {
        return scrubbed;
    };
    if let Some((bare, _)) = without_login(text) {
        if bare == text {
            return scrubbed;
        }
        let (bare_words, bare_separators) = words_of(&bare);
        if bare_separators == separators {
            return joined(
                words.iter().zip(&bare_words).map(|(word, bare)| {
                    let hidden = scrub_word(word);
                    if hidden == *word {
                        scrub_word(bare)
                    } else {
                        hidden
                    }
                }),
                &separators,
            );
        }
    }
    let kept_scheme = words
        .iter()
        .find(|word| word.contains("://"))
        .is_some_and(|word| scrub_word(word) == *word);
    if kept_scheme {
        format!("{}://[redacted]", scrub_spaceless(&text[..at]))
    } else {
        "[redacted]".to_owned()
    }
}

/// Text with each bracketed IPv6 address, as in `[2001:db8::1]:45680` or
/// `[fe80::1%lo]`, replaced with its port as one address: the word
/// scrub splits at brackets and would leave the port (FB-267). It walks the
/// brackets once, pairing each `]` with the last `[` before it.
fn without_bracketed_ipv6(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut open = None;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'[' => open = Some(at),
            b']' => {
                if let Some(start) = open.take()
                    && bracketed_ipv6(&text[start + 1..at])
                {
                    let mut end = at + 1;
                    if text[end..].starts_with(':') {
                        let digits = text[end + 1..]
                            .bytes()
                            .take_while(u8::is_ascii_digit)
                            .count();
                        if digits > 0 {
                            end += 1 + digits;
                        }
                    }
                    out.push_str(&text[copied..start]);
                    out.push_str("[address]");
                    copied = end;
                    at = end;
                    continue;
                }
            }
            _ => {}
        }
        at += 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// An IPv6 address as written between brackets, with or without its scope,
/// `::` included.
fn bracketed_ipv6(inner: &str) -> bool {
    let (address, scope) = inner.split_once('%').unwrap_or((inner, ""));
    address.parse::<std::net::Ipv6Addr>().is_ok()
        && scope
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// The words of text without whitespace, split at quotes, brackets, commas,
/// `=`, `<` and `>`, and the separators between them. A word may be empty.
fn words_of(text: &str) -> (Vec<&str>, Vec<char>) {
    let mut words = Vec::new();
    let mut separators = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if matches!(
            c,
            '"' | '\'' | ',' | '(' | ')' | '[' | ']' | '{' | '}' | '=' | '<' | '>'
        ) {
            words.push(&text[start..i]);
            separators.push(c);
            start = i + c.len_utf8();
        }
    }
    words.push(&text[start..]);
    (words, separators)
}

fn joined(words: impl Iterator<Item = String>, separators: &[char]) -> String {
    let mut out = String::new();
    for (i, word) in words.enumerate() {
        if let Some(separator) = i.checked_sub(1).map(|i| separators[i]) {
            out.push(separator);
        }
        out.push_str(&word);
    }
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
        redact_endpoints(&mut value, &[&engine.link_endpoints.read()]);
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
        over_mcp: false,
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
/// An agent outside UXDuck has the same for one connection.
const MAX_FILED: usize = 3;
const MAX_ADDED: usize = 10;

/// A report the agent files by itself (S294): the title and text only,
/// scrubbed as a bundle's free text is, sent anonymously with a follow-up key
/// the root keeps so the agent can add to it from the same chat, or the same
/// connection for an agent outside UXDuck.
pub(crate) fn file(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &FeedbackFileParams,
) -> Result<Value, ApiError> {
    let chat = filed_from(engine, connection, params.thread_id.as_deref())?;
    let title = scrub_text(params.title.trim());
    let text = scrub_text(params.text.trim());
    if title.is_empty() || title.chars().count() > MAX_TITLE {
        return Err(invalid("the title must be 1 to 120 characters"));
    }
    if text.is_empty() || text.chars().count() > MAX_SUGGESTION {
        return Err(invalid("the text must be 1 to 2,000 characters"));
    }
    let filed = match &chat {
        Some(thread) => {
            calls_in(engine, thread, "feedback/file")
                + engine
                    .agent_filing
                    .values()
                    .filter(|t| t.as_ref() == Some(thread))
                    .count()
        }
        None => outside(engine, connection).map_or(0, |agent| agent.filed.len()),
    };
    if filed >= MAX_FILED {
        return Err(ApiError::new(
            ErrorKind::Busy,
            if chat.is_some() {
                "three reports were filed from this chat already: add to one with feedback/add"
            } else {
                "three reports were filed over this connection already: add to one with feedback/add"
            },
        ));
    }
    let report_id = uuid(&engine.random.token());
    let mcp_client = match &chat {
        Some(_) => None,
        None => outside(engine, connection).map(|agent| agent.client.clone()),
    };
    let order = FeedbackOrder {
        report_id: report_id.clone(),
        kind: params.kind,
        text: format!("{title}\n\n{text}"),
        reply_email: None,
        reporter_name: None,
        surface: engine.surface(),
        context: engine.agent_context(mcp_client.as_deref()),
        screenshot: None,
        bundle: None,
        flight: None,
        credit: false,
        from_agent: true,
        agent_filed: true,
        over_mcp: chat.is_none(),
        attachments: Vec::new(),
        follow_up_key: Some(engine.random.token()),
        voice_notes: Vec::new(),
    };
    if let Some(waiting) = engine.replying_to.take() {
        engine.awaiting_feedback.insert(report_id.clone(), waiting);
    }
    if chat.is_none()
        && let Some(agent) = outside_mut(engine, connection)
    {
        agent.filed.push(report_id.clone());
    }
    engine.agent_filing.insert(report_id, chat);
    engine.feedback.send(order);
    Ok(Value::Null)
}

/// More for a report filed from the same chat (S294): the agent's words,
/// scrubbed, or files the person added from the chat's card. An agent
/// outside UXDuck adds to what its own connection filed.
pub(crate) fn add(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &FeedbackAddParams,
    operator: bool,
) -> Result<Value, ApiError> {
    let chat = filed_from(engine, connection, params.thread_id.as_deref())?;
    let (known, added) = match &chat {
        Some(thread) => (
            filed_in(engine, thread).contains(&params.report_id),
            calls_in(engine, thread, "feedback/add"),
        ),
        None => outside(engine, connection).map_or((false, 0), |agent| {
            (agent.filed.contains(&params.report_id), agent.added)
        }),
    };
    if !known {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            if chat.is_some() {
                "no report with that id was filed from this chat"
            } else {
                "no report with that id was filed over this connection"
            },
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
    if added >= MAX_ADDED {
        return Err(ApiError::new(
            ErrorKind::Busy,
            if chat.is_some() {
                "enough was added from this chat already"
            } else {
                "enough was added over this connection already"
            },
        ));
    }
    if chat.is_none()
        && let Some(agent) = outside_mut(engine, connection)
    {
        agent.added += 1;
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

/// The chat a report is filed from, where reports go out from here. None
/// for an agent outside UXDuck, such as an MCP client, which names no chat:
/// the report is its connection's.
fn filed_from(
    engine: &Engine,
    connection: ConnectionId,
    thread: Option<&str>,
) -> Result<Option<String>, ApiError> {
    if !engine.feedback.sends() {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Automatic reporting isn't available in this browser session. Use feedback_suggest \
             to prepare a draft for the person to review and send. Nothing has been sent.",
        ));
    }
    match thread {
        None if outside(engine, connection).is_some() => Ok(None),
        Some(id) if engine.state.threads.contains_key(id) => Ok(Some(id.to_owned())),
        _ => Err(invalid("reports are filed from a UXDuck chat")),
    }
}

fn outside(engine: &Engine, connection: ConnectionId) -> Option<&crate::OutsideAgent> {
    engine.connections.get(&connection)?.outside.as_ref()
}

fn outside_mut(engine: &mut Engine, connection: ConnectionId) -> Option<&mut crate::OutsideAgent> {
    engine.connections.get_mut(&connection)?.outside.as_mut()
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
    /// An agent outside UXDuck is named instead, as "filed over MCP by
    /// claude-code".
    fn agent_context(&self, mcp_client: Option<&str>) -> String {
        [
            format!("UXDuck {}", self.config.daemon_version),
            mcp_client.map_or_else(
                || "filed from a chat by the agent".to_owned(),
                |client| format!("filed over MCP by {client}"),
            ),
            self.vehicles_line(),
        ]
        .join(" · ")
    }

    /// "1 vehicle (ArduPilot Copter, udp-listen)": their autopilots, types
    /// and links. The line is one of an issue's first, so three are named
    /// and the rest counted, as "5 vehicles (…, and 2 more)". The bundle
    /// has every one.
    fn vehicles_line(&self) -> String {
        const NAMED: usize = 3;
        let mut vehicles: Vec<String> = self
            .state
            .vehicles
            .values()
            .take(NAMED)
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
            n => {
                if n > NAMED {
                    vehicles.push(format!("and {} more", n - NAMED));
                }
                format!("{n} vehicles ({})", vehicles.join(", "))
            }
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
                detail: l
                    .status_error
                    .as_ref()
                    .map(LinkFailure::summary)
                    .or_else(|| l.status_detail.clone()),
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
        // Every one, newest first. Until 2026-11-15 only the newest 20
        // went, and nothing said so. `fit` leaves the oldest out when the
        // bundle is over what the servers take, and says how many.
        notifications.sort_by_key(|n| std::cmp::Reverse(n.created_at_us));
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
            // As the page sent them: it keeps its last 20, oldest first.
            // Until 2026-11-15 the first 20 of a longer list went, which
            // are the oldest, and nothing said so.
            errors: params.errors.clone(),
            log: Vec::new(),
            chat,
            left_out: None,
        }
    }

    /// "UXDuck 0.4.1 · Chrome 141 tab on UXDuck Desktop · window 1512×945 ·
    /// 1 vehicle (ArduPilot Copter, udp-listen) · not signed in · map open".
    /// The root puts the platform first.
    fn context(&self, params: &FeedbackSendParams) -> String {
        // The page's own words, which nothing checks: one longer than 40
        // characters is cut for this line, and ends in "…" to say so.
        let short = |s: &str| {
            if s.chars().count() > 40 {
                format!("{}…", s.chars().take(39).collect::<String>())
            } else {
                s.to_owned()
            }
        };
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
        parts.push(
            if self.account_identity.signed_in {
                "signed in"
            } else {
                "not signed in"
            }
            .to_owned(),
        );
        fitted(&parts, &params.ui.panes)
    }
}

/// The context line in 250 characters, with what is open last: listed while
/// it fits, else counted, so the vehicles are never what is cut off. The
/// servers take 300 of a line that the root puts the platform before
/// (`context` in `apps/api/src/features/feedback/report.ts`). A line still
/// longer ends in "…": the bundle has all of it.
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
    if line.chars().count() > MAX {
        line = format!("{}…", line.chars().take(MAX - 1).collect::<String>());
    }
    line
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

    /// FB-267: a link's host name has no address's shape, so its endpoint
    /// goes where UXDuck names it, and nowhere else: a host that is also a
    /// word, such as `desktop`, leaves the word.
    #[test]
    fn a_link_endpoint_leaves_the_bundle_and_words_stay() {
        let endpoints = Endpoints::from_iter([
            ("drone.local".to_owned(), 5760),
            ("review-test.invalid".to_owned(), 14550),
            ("desktop".to_owned(), 14551),
            ("fe80::1".to_owned(), 5760),
        ]);
        let mut bundle = json!({
            "links": [{ "detail": "Nothing answered at drone.local:5760. Check that the device is on." }],
            "trace": [
                { "payload": "UXDuck couldn't find Review-Test.invalid (port 14550). Check the name." },
                { "payload": { "kind": { "type": "tcp-client", "host": "elsewhere.invalid", "port": 5760 } } },
                { "payload": { "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } } },
            ],
            "surface": "desktop",
            "log": [
                "desktop:14551 refused the connection. [fe80::1]:5760 closed the connection.",
                "mydrone.local:5760, drone.local:57600 and drone.local port 5760 stay",
            ],
        });
        redact_endpoints(&mut bundle, &[&endpoints]);
        assert_eq!(
            bundle,
            json!({
                "links": [{ "detail": "Nothing answered at [endpoint]. Check that the device is on." }],
                "trace": [
                    { "payload": "UXDuck couldn't find [endpoint]. Check the name." },
                    { "payload": { "kind": { "type": "tcp-client", "host": "[removed]", "port": "[removed]" } } },
                    { "payload": { "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } } },
                ],
                "surface": "desktop",
                "log": [
                    "[endpoint] refused the connection. [endpoint] closed the connection.",
                    "mydrone.local:5760, drone.local:57600 and drone.local port 5760 stay",
                ],
            })
        );
    }

    /// FB-267: IPv6 endpoints go however they are written, bracketed or not
    /// and with any scope, and a URL loses a link's host whatever its port.
    #[test]
    fn ipv6_endpoints_and_url_hosts_leave_the_text() {
        let endpoints = Endpoints::from_iter([
            ("fe80::cafe%lo".to_owned(), 45676),
            ("2001:db8::beef".to_owned(), 45671),
            ("request".to_owned(), 14552),
        ]);
        let said = without_endpoints(
            "UDP fe80::cafe%lo:45676, peer [FE80::CAFE%2]:45676, fe80::cafe:45676 \
             and [2001:db8::beef]:45671 at=2001:db8::beef:45671. \
             See https://request/path and http://user@request:8080/x?q=1, \
             a request to fe80::cafe:1:45676 and 2001:db8::beef:456710 stays.",
            &[&endpoints],
        );
        assert_eq!(
            said,
            "UDP [endpoint], peer [endpoint], [endpoint] \
             and [endpoint] at=[endpoint]. \
             See https://[endpoint]/path and http://user@[endpoint]/x?q=1, \
             a request to fe80::cafe:1:45676 and 2001:db8::beef:456710 stays."
        );
        assert_eq!(
            scrub_text(&without_endpoints(
                "link ready at=[2001:db8::beef]:45671",
                &[&endpoints]
            )),
            "link ready at=[endpoint]"
        );
    }

    /// A small, seeded generator: the same cases on every run.
    struct Cases(u64);

    impl Cases {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % n as u64).unwrap()
        }

        fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
            from[self.below(from.len())]
        }

        fn port(&mut self) -> u16 {
            u16::try_from(1 + self.below(65_535)).unwrap()
        }

        fn cased(&mut self, text: &str) -> String {
            text.chars()
                .map(|c| {
                    if self.below(2) == 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect()
        }

        /// A host a link can be configured with, as written there.
        fn host(&mut self) -> String {
            match self.below(4) {
                0 => format!(
                    "{}{}.{}{}",
                    self.pick(&["drone", "relay", "gcs", "request", "desktop", "link"]),
                    self.below(100),
                    self.pick(&["local", "example", "lan"]),
                    self.pick(&["", "."])
                ),
                1 => format!(
                    "10.{}.{}.{}",
                    self.below(256),
                    self.below(256),
                    self.below(256)
                ),
                2 => format!("2001:db8::{:x}", 1 + self.below(0xfffe)),
                _ => format!(
                    "fe80::{:x}{}",
                    1 + self.below(0xfffe),
                    self.pick(&["", "%lo", "%2", "%eth0"])
                ),
            }
        }

        /// A host that names nothing, or nearly.
        fn garbage(&mut self) -> String {
            let n = self.below(6);
            (0..n)
                .map(|_| self.pick(&["%", "[", "]", ".", ":", "a", "0", "f", "-", "_", " "]))
                .collect()
        }

        /// Words between mentions: never a digit, so a port left behind shows.
        fn filler(&mut self) -> String {
            let n = self.below(4);
            (0..n)
                .map(|_| self.pick(&["link", "ready", "at", "to", "UDP", "port", "the", "is"]))
                .collect::<Vec<_>>()
                .join(" ")
        }

        /// `host` at `port` as UXDuck or the system writes it.
        fn mention(&mut self, host: &str, port: u16) -> String {
            let written = self.cased(host);
            let (address, scope) = written.split_once('%').unwrap_or((&written, ""));
            let v6 = address.contains(':');
            let scope = if v6 && self.below(2) == 0 {
                format!("%{}", self.pick(&["lo", "2", "eth0"]))
            } else if scope.is_empty() {
                String::new()
            } else {
                format!("%{scope}")
            };
            let dns = address.trim_end_matches('.');
            let dotted = if !v6 && self.below(2) == 0 {
                format!("{dns}.")
            } else {
                dns.to_owned()
            };
            let host = format!("{}{scope}", if v6 { address } else { &dotted });
            let path = self.pick(&["", "/status", "/a/b?q=x"]);
            match self.below(if v6 { 5 } else { 4 }) {
                0 => uxduck_core::domain::link_failure::endpoint(&host, port),
                1 => format!("{host} (port {port})"),
                2 if v6 => format!("https://[{}]{path}", host.replace('%', "%25")),
                2 => format!("https://{host}{path}"),
                3 if v6 => format!("http://user@[{host}]:{port}{path}"),
                3 => format!("http://user@{host}:{port}{path}"),
                _ => format!("{host}:{port}"),
            }
        }
    }

    /// FB-267: over random hosts and text the endpoint scan always ends, and
    /// it never leaves a configured host or its port where an endpoint is
    /// written, whatever its case, brackets, scope or trailing dot.
    #[test]
    fn the_endpoint_scan_ends_and_leaves_no_configured_endpoint() {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut cases = Cases(0x2545_f491_4f6c_dd1d);
            for _ in 0..20_000 {
                let endpoints: Endpoints = (0..=cases.below(3))
                    .map(|_| (cases.garbage(), cases.port()))
                    .collect();
                let text: String = (0..cases.below(12))
                    .map(|_| match cases.below(3) {
                        0 => cases.garbage(),
                        1 => cases.filler(),
                        _ => cases
                            .pick(&["://", "[", "]:", "%", " (port ", ":1", "."])
                            .to_owned(),
                    })
                    .collect();
                let _ = without_endpoints(&text, &[&endpoints]);
                let _ = scrub_text(&text);
            }
            for _ in 0..20_000 {
                let mut configured = Vec::new();
                let mut text = cases.filler();
                for _ in 0..=cases.below(3) {
                    let host = cases.host();
                    let port = cases.port();
                    let mention = cases.mention(&host, port);
                    text.push_str(cases.pick(&[" ", "=", "(", "\"", ", "]));
                    text.push_str(&mention);
                    text.push_str(cases.pick(&[" ", ". ", ", ", ") ", "\" "]));
                    text.push_str(&cases.filler());
                    configured.push((host, port));
                }
                let endpoints = configured.iter().cloned().collect::<Endpoints>();
                let kept = without_endpoints(&text, &[&endpoints]).to_ascii_lowercase();
                for (host, port) in &configured {
                    let bare = normalized_host(host);
                    assert!(
                        !kept.contains(&bare),
                        "{bare} left in {kept:?} from {text:?}"
                    );
                    assert!(
                        !kept.contains(&port.to_string()),
                        "port {port} left in {kept:?} from {text:?}"
                    );
                }
            }
            done.send(()).unwrap();
        });
        finished
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the scan ends for every case");
    }

    /// FB-267: hosts that name nothing never stop the scan.
    #[test]
    fn a_host_that_names_nothing_is_skipped() {
        let text = "UDP %:5760 at [].:5760 and .:5760 or :5760";
        for host in ["[]", ".", "", " ", "...", "[.]"] {
            let endpoints = Endpoints::from_iter([(host.to_owned(), 5760)]);
            assert_eq!(endpoints, Endpoints::default(), "{host:?}");
            assert_eq!(without_endpoints(text, &[&endpoints]), text, "{host:?}");
        }
        let endpoints = Endpoints::from_iter([("%".to_owned(), 5760), ("%%".to_owned(), 5760)]);
        assert_eq!(
            without_endpoints(text, &[&endpoints]),
            "UDP [endpoint] at [].:5760 and .:5760 or :5760"
        );
    }

    /// FB-267: only an IPv6 address loses its scope, so a name with a `%`
    /// in it never stands for a shorter name.
    #[test]
    fn a_scope_goes_only_from_an_ipv6_address() {
        let endpoints = Endpoints::from_iter([
            ("relay%wifi".to_owned(), 5760),
            ("FE80::0:CAFE%eth0".to_owned(), 14550),
        ]);
        assert_eq!(
            without_endpoints(
                "relay%wifi:5760, relay%wifi:note, https://relay/status and fe80::cafe%2:14550",
                &[&endpoints]
            ),
            "[endpoint], relay%wifi:note, https://relay/status and [endpoint]"
        );
    }

    /// FB-267: the scan reads a text once however many links there are, so
    /// a megabyte of log with a thousand links and nested brackets is quick.
    #[test]
    fn the_endpoint_scan_grows_with_the_text_and_not_the_links() {
        let endpoints: Endpoints = (0..1_000u16)
            .map(|i| (format!("drone{i}.example"), 5_000 + i))
            .collect();
        let chunk = "[[[[link ready at=drone7.example:5007]] [2001:db8::1]:45680]] \
         see https://drone999.example/status, fe80::1%lo:14550 (port 5) [x]:5 ";
        let megabyte = 1 << 20;
        let texts = [
            chunk.repeat(megabyte / chunk.len() + 1),
            format!("{}{}", "[".repeat(megabyte / 2), "]".repeat(megabyte / 2)),
            "a:".repeat(megabyte / 2),
            "[::1".repeat(megabyte / 4),
        ];
        let started = std::time::Instant::now();
        for text in &texts {
            let kept = scrub_text(&without_endpoints(text, &[&endpoints]));
            assert!(!kept.contains("drone7.example") && !kept.contains("drone999"));
        }
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(20), "took {took:?}");
    }

    /// FB-267: a host in any script leaves the text however it is written,
    /// in a plain endpoint, a lookup error and a URL.
    #[test]
    fn a_unicode_host_leaves_the_text() {
        let endpoints = Endpoints::from_iter([
            ("café.invalid".to_owned(), 45688),
            ("Ünï.example".to_owned(), 14550),
        ]);
        assert_eq!(
            without_endpoints(
                "UXDuck couldn't find CAFÉ.invalid (port 45688). Check the name. \
                 link ready at=café.invalid:45688 and https://ünï.example/x, \
                 café.invalid alone stays",
                &[&endpoints]
            ),
            "UXDuck couldn't find [endpoint]. Check the name. \
             link ready at=[endpoint] and https://[endpoint]/x, \
             café.invalid alone stays"
        );
    }

    /// FB-267: a bracket around a whole URL or endpoint is punctuation, and
    /// the brackets around an IPv6 address are part of it.
    #[test]
    fn a_bracket_around_an_endpoint_is_punctuation() {
        let endpoints = Endpoints::from_iter([
            ("review-synthetic.invalid".to_owned(), 45684),
            ("2001:db8::7".to_owned(), 5760),
        ]);
        assert_eq!(
            without_endpoints(
                "See [https://review-synthetic.invalid:45684]. And [http://[2001:db8::7]:5760/x]. \
                 And [review-synthetic.invalid:45684].",
                &[&endpoints]
            ),
            "See [https://[endpoint]]. And [http://[endpoint]/x]. And [[endpoint]]."
        );
    }

    /// FB-267: a link's name loses any endpoint it is written with, known
    /// or not, and other names stay as they are.
    #[test]
    fn a_link_name_loses_any_endpoint_it_names() {
        let mut value = json!({
            "link": {
                "name": "Old drone.local:5760, https://old.example/x and Desk:14550 at 2:30",
                "kind": { "type": "udp-client", "host": "127.0.0.1", "port": 14550 }
            },
            "vehicle": { "name": "Plane at drone.local:5760" },
        });
        redact_endpoints(&mut value, &[&Endpoints::default()]);
        assert_eq!(
            value,
            json!({
                "link": {
                    "name": "Old [endpoint], https://[endpoint]/x and [endpoint] at 2:30",
                    "kind": { "type": "udp-client", "host": "127.0.0.1", "port": 14550 }
                },
                "vehicle": { "name": "Plane at drone.local:5760" },
            })
        );
    }

    /// FB-267: the index holds the links configured now, never one removed.
    #[test]
    fn the_active_endpoints_are_the_links_configured_now() {
        let active = ActiveEndpoints::default();
        let drone = LinkKind::TcpClient {
            host: "drone.local".into(),
            port: 5760,
        };
        let relay = LinkKind::UdpClient {
            host: "relay.example".into(),
            port: 14550,
        };
        active.set([&drone, &relay]);
        assert_eq!(
            *active.read(),
            Endpoints::from_iter([
                ("drone.local".to_owned(), 5760),
                ("relay.example".to_owned(), 14550)
            ])
        );
        active.set([&relay]);
        assert_eq!(
            *active.read(),
            Endpoints::from_iter([("relay.example".to_owned(), 14550)])
        );
    }

    /// FB-267: the scrub hides a bracketed IPv6 address with its port, which
    /// a refused listener names before any endpoint was saved.
    #[test]
    fn a_bracketed_ipv6_address_goes_with_its_port() {
        assert_eq!(
            scrub_text(
                "UXDuck can't listen on [2001:db8::1]:45680. Or [fe80::1%lo]. Or [x]:5. \
                 UDP [::]:50481 is in use."
            ),
            "UXDuck can't listen on [address]. Or [address]. Or [x]:5. UDP [address] is in use."
        );
    }

    /// FB-267: a frame that adds a link leaves out its endpoint before the
    /// link is saved, wherever else the frame names it.
    #[test]
    fn a_kept_frame_leaves_out_the_endpoint_it_names() {
        let mut recent = VecDeque::new();
        let frame = |payload| TraceFrame {
            ts_us: 1,
            direction: uxduck_contracts::dto::trace::TraceDirection::In,
            kind: uxduck_contracts::dto::trace::FrameKind::Request,
            client_id: "c".into(),
            transport: uxduck_contracts::dto::trace::Transport::WebSocket,
            method: Some("link/add".into()),
            id: None,
            bytes: 1,
            latency_us: None,
            payload,
        };
        keep(
            &mut recent,
            frame(
                json!({ "params": { "kind": { "type": "udp-client", "host": "link", "port": 14552 } }, "note": "link:14552 and link/add" }),
            ),
            1,
            &Endpoints::default(),
        );
        assert_eq!(
            recent[0].payload,
            json!({ "params": { "kind": { "type": "udp-client", "host": "[removed]", "port": "[removed]" } }, "note": "[endpoint] and link/add" })
        );
        // A listener's port goes from a request, which may be refused before
        // it is a link, and stays in what UXDuck says about a link.
        let listener = json!({ "kind": { "type": "udp-listen", "host": "::", "port": 50481 } });
        keep(
            &mut recent,
            frame(listener.clone()),
            1,
            &Endpoints::default(),
        );
        assert_eq!(
            recent[1].payload,
            json!({ "kind": { "type": "udp-listen", "host": "::", "port": "[removed]" } })
        );
        let mut answer = frame(listener.clone());
        answer.kind = uxduck_contracts::dto::trace::FrameKind::Response;
        keep(&mut recent, answer, 1, &Endpoints::default());
        assert_eq!(recent[2].payload, listener);
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

    /// The samples of a corpus beside this file: blocks between blank lines,
    /// without comment lines.
    fn samples(corpus: &str) -> Vec<String> {
        corpus
            .split("\n\n")
            .map(|block| {
                block
                    .lines()
                    .filter(|line| !line.starts_with('#'))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|sample| !sample.trim().is_empty())
            .collect()
    }

    /// FB-229: a camera's address pasted into feedback loses its login as it
    /// does on its way to the account.
    #[test]
    fn a_cameras_login_never_goes_with_feedback() {
        let hide = samples(include_str!("scrub-must-hide.txt"));
        let leaks: Vec<(&String, String)> = hide
            .iter()
            .map(|sample| (sample, scrub_text(sample)))
            .filter(|(_, scrubbed)| scrubbed.contains("hunter2"))
            .collect();
        assert!(
            leaks.is_empty(),
            "{} of {} leak: {leaks:#?}",
            leaks.len(),
            hide.len()
        );
        let keep = samples(include_str!("scrub-must-keep.txt"));
        let changed: Vec<(&String, String)> = keep
            .iter()
            .map(|sample| (sample, scrub_text(sample)))
            .filter(|(sample, scrubbed)| *sample != scrubbed)
            .collect();
        assert!(
            changed.is_empty(),
            "{} of {} change: {changed:#?}",
            changed.len(),
            keep.len()
        );
        assert_eq!(
            scrub_text("opening rtsp://admin:hunter2@cam/main failed"),
            "opening rtsp://cam/main failed"
        );
        assert_eq!(
            scrub_text("rtsp://admin:12/34@cam/main"),
            "rtsp://[redacted]"
        );
    }

    /// Main's scrub before FB-229, kept as it was to compare against.
    fn scrub_text_before_cameras(text: &str) -> String {
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

    /// FB-229: taking a camera's login out of feedback text shows nothing
    /// the scrub hid before, and leaves no login.
    #[test]
    fn a_cameras_login_goes_and_nothing_hidden_before_comes_back() {
        let hex = "3f9a0c6b1d2e4f5a6b7c8d9e0f1a2b3c4d5e6f7a";
        let inputs = [
            "https://u:p4ss@api.example.com/v1/sk-abcdef1234567890xyz".to_owned(),
            format!("https://ana:pw@files.example.com/s/{hex}/x"),
            "rtsp://admin:hunter2@cam.local:554/live/ghp_abcdefghijklmnop1234".to_owned(),
            "rtsp://admin:hunter2@cam.corp.example.com/main".to_owned(),
            "wss://ana@example.com@gw.example.com/socket?token=hunter2".to_owned(),
            "sent to 10.0.0.5:14551 from COM4 and /dev/ttyUSB0, mail ana@example.com".to_owned(),
            format!("key sk-abc123def456ghi789, session {hex}, sessionid={hex}"),
            "data_dir=/Users/ana/Library and /home/ana/.config".to_owned(),
            "peer [fe80::1]:14550 and udp://10.0.0.5:14550".to_owned(),
            "rtsp://admin:hunter2@10.0.0.5:554/main".to_owned(),
            "rtsp://admin:hunter2@[fd7a::1]:8554/front".to_owned(),
            "rtsp://admin:hunter2@cam/main?sessionid=hunter2".to_owned(),
            "http://gw.example.com/front/whep?token=hunter2".to_owned(),
            "http://10.0.0.5:8889/whep?token=hunter2&stream=1".to_owned(),
            "rtsp://10.0.0.5:554/user=admin&password=hunter2&channel=1".to_owned(),
            "rtsp://cam.example.com/live?user=admin&password=hunter2".to_owned(),
            format!("rtsp://admin:hunter2@cam.example.com/main?password=hunter2&s={hex}"),
            "http://[fd7a::5]/cgi?loginuse=admin&loginpas=hunter2".to_owned(),
            "\"url\": \"rtsp://admin:hunter2@cam/main\", (rtsp://admin:hunter2@cam.lan/x)"
                .to_owned(),
            "rtsp://admin:12/hunter2@cam/main and rtsp://admin:hunter2@cam/a@b.example".to_owned(),
            "url=rtsp://admin:hunter2@ana@example.com/main".to_owned(),
        ];
        let fragments = |text: &str| -> Vec<String> {
            text.split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let markers = ["redacted", "address", "email", "path", "device"];
        for input in &inputs {
            let before = scrub_text_before_cameras(input);
            let now = scrub_text(input);
            let shown = fragments(&before);
            for part in fragments(&now) {
                assert!(
                    shown.contains(&part) || markers.contains(&part.as_str()),
                    "{input}: {now} shows {part}, which {before} hid"
                );
            }
            for login in ["hunter2", "p4ss", ":pw@", "admin"] {
                assert!(!now.contains(login), "{input}: {now} keeps {login}");
            }
            if !input.contains("://") {
                assert_eq!(now, before, "{input}");
            }
        }
    }

    /// FB-229: what was hidden before camera addresses were is hidden still.
    #[test]
    fn what_was_hidden_before_cameras_still_is() {
        for (given, kept) in [
            (
                "sent to 10.0.0.5:14551 from COM4, mail ana@example.com, key sk-abc123def456ghi789",
                "sent to [address] from [device], mail [email], key [redacted]",
            ),
            (
                "session 3f9a0c6b1d2e4f5a6b7c8d9e0f1a2b3c4d5e6f7a",
                "session [redacted]",
            ),
            ("token ghp_abcdefghijklmnop1234", "token [redacted]"),
            ("link opened on /dev/ttyUSB0.", "link opened on [device]."),
            (
                "data_dir=/Users/ana/Library/Application Support",
                "data_dir=[path] Support",
            ),
            ("peer [fe80::1]:14550", "peer [address]"),
        ] {
            assert_eq!(scrub_text(given), kept, "{given}");
        }
        let mut bundle = json!({ "password": "hunter2", "apiKey": "hunter2", "note": "rtsp://admin:hunter2@cam/main" });
        scrub(&mut bundle);
        assert!(!bundle.to_string().contains("hunter2"), "{bundle}");
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
        assert_eq!(scrub_text("peer [fe80::1]:14550"), "peer [address]");
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
        keep(
            &mut recent,
            frame(1, "telemetry/update", json!({})),
            1,
            &Endpoints::default(),
        );
        keep(
            &mut recent,
            frame(1, "feedback/progress", json!({})),
            1,
            &Endpoints::default(),
        );
        assert!(recent.is_empty());
        keep(
            &mut recent,
            frame(
                1,
                "feedback/send",
                json!({ "screenshot": "A".repeat(10_000) }),
            ),
            1,
            &Endpoints::default(),
        );
        let kept = recent[0].payload["screenshot"].as_str().unwrap();
        assert!(kept.len() < 600 && kept.ends_with('…'));
        keep(
            &mut recent,
            frame(RECENT_US + 10, "command/send", json!({})),
            RECENT_US + 10,
            &Endpoints::default(),
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
        keep(&mut recent, snapshot, 1, &Endpoints::default());
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
            &Endpoints::default(),
        );
        assert_eq!(recent[1].payload["attachments"][0]["name"], "a.pdf");
    }

    /// A bundle over what the servers take keeps the newest of each part,
    /// and says in itself and in its answer what was left out. Until
    /// 2026-11-15 it was cut to 2 MB and said nothing.
    #[test]
    fn a_bundle_over_what_the_servers_take_keeps_its_newest_and_says_what_went() {
        let frame = |n: u64| json!({ "tsUs": 1000 + n, "payload": "y".repeat(10_000) });
        let line = |n: usize| format!("{n:05} {}", "x".repeat(400));
        // 9 MB of trace, the page's errors and the notifications newest first.
        let mut bundle = json!({
            "schema": BUNDLE_SCHEMA,
            "trace": (0..900).map(frame).collect::<Vec<_>>(),
            "log": (0..500).map(line).collect::<Vec<_>>(),
            "chat": [{ "kind": "user-message", "text": "Why did it climb?" }],
            "errors": [{ "code": "map-unavailable", "atUs": 7 }],
            "daemon": { "notifications": [
                { "severity": "info", "title": "newer", "createdAtUs": 9 },
                { "severity": "info", "title": "older", "createdAtUs": 8 },
            ] },
        });
        let left_out = fit(&mut bundle, FeedbackLeftOut::default());
        assert!(json_len(&bundle) <= MAX_BUNDLE);
        let trace = bundle["trace"].as_array().unwrap();
        assert_eq!(
            trace.last().unwrap()["tsUs"],
            1899,
            "the newest frames stay"
        );
        let went = left_out.trace.clone().unwrap();
        assert_eq!(went.count + trace.len() as u64, 900, "none unaccounted for");
        assert!(went.count > 350, "{went:?}");
        assert_eq!(went.from_us, Some(1000));
        assert_eq!(went.to_us, Some(1000 + went.count - 1));
        assert_eq!(trace[0]["tsUs"], 1000 + went.count);
        assert!(went.bytes > went.count * 10_000, "{went:?}");
        assert_eq!(went.limit_bytes, MAX_BUNDLE as u64);
        assert_eq!(
            left_out,
            FeedbackLeftOut {
                trace: Some(went.clone()),
                ..FeedbackLeftOut::default()
            },
            "the log, the chat, the errors and the notifications are whole"
        );
        assert_eq!(bundle["log"].as_array().unwrap().len(), 500);
        assert_eq!(
            bundle["leftOut"],
            json!({ "trace": {
                "count": went.count,
                "bytes": went.bytes,
                "fromUs": 1000,
                "toUs": 1000 + went.count - 1,
                "limitBytes": MAX_BUNDLE,
            } }),
            "the bundle says it too"
        );

        // Under what the servers take, over the 2 MB a bundle was once cut
        // to: whole, and nothing to say.
        let mut whole = json!({
            "schema": BUNDLE_SCHEMA,
            "trace": (0..400).map(frame).collect::<Vec<_>>(),
            "log": ["one"],
        });
        let before = whole.clone();
        assert!(json_len(&whole) > 4_000_000);
        assert_eq!(
            fit(&mut whole, FeedbackLeftOut::default()),
            FeedbackLeftOut::default()
        );
        assert_eq!(whole, before, "a bundle that fits is left as it is");
    }

    /// Each part of a bundle gives way in turn, its oldest first, and what
    /// the root left out of the flight data is said beside them.
    #[test]
    fn a_bundles_parts_give_way_in_turn_and_the_flight_data_is_said_beside_them() {
        let frame = |n: u64| json!({ "tsUs": 1000 + n, "payload": "y".repeat(10_000) });
        let line = |n: usize| format!("{n:05} {}", "x".repeat(400));
        // The notifications come newest first, so their oldest are the last.
        let mut wordy = json!({
            "schema": BUNDLE_SCHEMA,
            "trace": [frame(0)],
            "log": (0..2_000).map(line).collect::<Vec<_>>(),
            "chat": (0..3).map(|n| json!({ "kind": "tool-call", "text": format!("{n}{}", "c".repeat(900_000)) })).collect::<Vec<_>>(),
            "errors": (0..3u64).map(|n| json!({ "code": "x", "stack": "s".repeat(900_000), "atUs": 20 + n })).collect::<Vec<_>>(),
            "daemon": { "notifications": (0..4u64).map(|n| json!({ "severity": "info", "title": "t".repeat(700_000), "createdAtUs": 40 - n })).collect::<Vec<_>>() },
        });
        let left_out = fit(&mut wordy, FeedbackLeftOut::default());
        assert!(json_len(&wordy) <= MAX_BUNDLE);
        assert_eq!(left_out.trace.as_ref().map(|l| l.count), Some(1));
        assert_eq!(
            left_out.log.as_ref().map(|l| (l.count, l.from_us)),
            Some((2_000, None))
        );
        assert_eq!(left_out.chat.as_ref().map(|l| l.count), Some(3));
        assert_eq!(
            left_out
                .errors
                .as_ref()
                .map(|l| (l.count, l.from_us, l.to_us)),
            Some((1, Some(20), Some(20))),
            "the oldest error"
        );
        assert_eq!(wordy["errors"][0]["atUs"], 21);
        assert_eq!(left_out.notifications, None);
        assert_eq!(
            wordy["daemon"]["notifications"].as_array().unwrap().len(),
            4
        );

        // What the root left out of the flight data is said beside it.
        let flight = LeftOut {
            count: 12,
            bytes: 3_456,
            from_us: Some(5),
            to_us: Some(6),
            limit_bytes: 20 * 1024 * 1024,
        };
        let mut small = json!({ "schema": BUNDLE_SCHEMA, "trace": [], "log": ["one"] });
        let said = FeedbackLeftOut {
            flight: Some(flight),
            ..FeedbackLeftOut::default()
        };
        assert_eq!(fit(&mut small, said.clone()), said);
        assert_eq!(small["leftOut"]["flight"]["count"], 12);
        assert_eq!(small["log"], json!(["one"]));
    }

    #[test]
    fn the_context_line_counts_the_vehicles_it_does_not_name_and_marks_what_it_cuts() {
        let parts = ["p".repeat(260)];
        let line = fitted(&parts, &[]);
        assert_eq!(line.chars().count(), 250);
        assert!(line.ends_with('…'), "{line}");
    }
}
