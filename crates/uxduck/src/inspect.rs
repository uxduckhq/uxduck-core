//! The MAVLink inspector's table for the installed daemon: one for the
//! process, which the data plane records every frame in with [`record`] and
//! `mavlink/inspect` reads with [`snapshot`]. The table itself is the
//! engine's [`Inspector`]. `mavlink/history` pages through what UXDuck
//! sent from the table ([`history`]) and what vehicles sent from their
//! links' tlogs ([`crate::tlog_index`]), so the table keeps only the first.

use std::path::Path;
use std::sync::{LazyLock, Mutex, PoisonError};

use uxduck_contracts::dto::inspect::{
    InspectDirection, MavlinkHistoryParams, MavlinkHistoryResult, MavlinkInspectResult, PastMessage,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_engine::mavlink_inspect::{self, Fallback, Inspector};
use uxduck_mavlink::framing::Header;

static TABLE: LazyLock<Mutex<Inspector>> =
    LazyLock::new(|| Mutex::new(Inspector::default().keep_history(false, true)));

/// One frame from a vehicle, or to it, as the data plane saw it.
pub fn record(
    vehicle_id: &str,
    direction: InspectDirection,
    header: Header,
    payload: &[u8],
    at_us: u64,
) {
    TABLE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(vehicle_id, direction, header, payload, at_us);
}

/// What the inspector shows for a vehicle known by these ids, at `now_us`:
/// received first, then sent, each by name.
pub fn snapshot(vehicle_ids: &[String], now_us: u64) -> MavlinkInspectResult {
    // Copied out under the lock; decoded after it.
    let seen = TABLE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .read(vehicle_ids, now_us);
    seen.decode(fallback())
}

/// Latest received heartbeat, read without decoding the inspector's table.
#[must_use]
pub fn heartbeat_us(vehicle_ids: &[String]) -> Option<u64> {
    TABLE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .heartbeat_us(vehicle_ids)
}

/// Earlier messages of one type for `mavlink/history`, newest first: what
/// the vehicle sent from its link's tlogs in `tlogs`, what UXDuck sent from
/// the table.
///
/// # Errors
/// When `params.before` is not a cursor of the kind the direction pages.
pub fn history(
    tlogs: &Path,
    vehicle_ids: &[String],
    link_id: &str,
    system_id: u8,
    params: &MavlinkHistoryParams,
) -> Result<MavlinkHistoryResult, ApiError> {
    let bad = || ApiError::new(ErrorKind::InvalidValue, "not a cursor from this history");
    let limit = mavlink_inspect::page_size(params.limit);
    let before = params.before.as_deref();
    match params.direction {
        InspectDirection::Sent => {
            let earlier = TABLE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .history(vehicle_ids, params.direction, params.id, before, limit)
                .map_err(|_| bad())?;
            Ok(earlier.decode(fallback()))
        }
        InspectDirection::Received => {
            let (found, older) =
                crate::tlog_index::page(tlogs, link_id, system_id, params.id, before, limit)
                    .map_err(|_| bad())?;
            let fallback = fallback();
            Ok(MavlinkHistoryResult {
                messages: found
                    .into_iter()
                    .map(|found| PastMessage {
                        at_us: found.at_us,
                        system_id: found.system_id,
                        component_id: found.component_id,
                        fields: mavlink_inspect::fields(params.id, &found.payload, fallback),
                        ..Default::default()
                    })
                    .collect(),
                older,
                from_log: true,
                coverage: None,
            })
        }
    }
}

/// How a message ArduPilot's dialect lacks is decoded: by a plugin's
/// dialect file, where plugins are built in.
#[allow(
    clippy::unnecessary_wraps,
    reason = "None where plugins are not built in"
)]
fn fallback() -> Option<Fallback<'static>> {
    #[cfg(feature = "plugins")]
    return Some(&vendor);
    #[cfg(not(feature = "plugins"))]
    None
}

/// Plugins' vendor messages, which ArduPilot's dialect table lacks, from
/// their dialect files.
#[cfg(feature = "plugins")]
fn vendor(id: u32, payload: &[u8]) -> Option<(String, serde_json::Map<String, serde_json::Value>)> {
    uxduck_mavlink::named::decode(id, payload).map(|decoded| (decoded.name, decoded.fields))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    const S: u64 = 1_000_000;

    #[test]
    fn a_heartbeat_is_decoded_on_read() {
        // HEARTBEAT: custom_mode 0, type 2 (quadrotor), autopilot 3
        // (ArduPilot), base_mode 81, system_status 3, mavlink_version 3.
        let payload = [0, 0, 0, 0, 2, 3, 81, 3, 3];
        let id = "vh-inspect-heartbeat";
        let header = Header {
            sequence: 0,
            system_id: 1,
            component_id: 1,
            message_id: 0,
        };
        record(id, InspectDirection::Received, header, &payload, 10 * S);
        let result = snapshot(&[id.to_owned()], 10 * S + S / 2);
        assert!(result.decoded);
        let message = &result.messages[0];
        assert_eq!(message.name, "HEARTBEAT");
        assert_eq!(message.dialect.as_deref(), Some("common"));
        assert_eq!(message.age_ms, 500);
        let fields = message.fields.as_ref().unwrap();
        assert_eq!(fields["type"], 2);
        assert_eq!(fields["autopilot"], 3);
    }

    #[cfg(feature = "plugins")]
    #[test]
    fn a_plugins_vendor_message_is_decoded_from_its_dialect_file() {
        let dialect = "<mavlink><messages><message id=\"47123\" name=\"INSPECT_VENDOR_TEST\">\
            <field type=\"uint16_t\" name=\"level\">x</field></message></messages></mavlink>";
        let messages = uxduck_mavlink::named::parse_dialect(dialect).unwrap();
        uxduck_mavlink::named::register("inspect-vendor-test", messages).unwrap();
        let id = "vh-inspect-vendor";
        let header = Header {
            sequence: 0,
            system_id: 1,
            component_id: 1,
            message_id: 47_123,
        };
        record(id, InspectDirection::Received, header, &[0x34, 0x12], S);
        let result = snapshot(&[id.to_owned()], S);
        uxduck_mavlink::named::unregister("inspect-vendor-test");
        let message = &result.messages[0];
        assert_eq!(message.name, "INSPECT_VENDOR_TEST");
        assert_eq!(message.dialect, None);
        assert_eq!(message.fields.as_ref().unwrap()["level"], 0x1234);
    }
}
