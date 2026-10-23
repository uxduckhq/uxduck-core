use serde_json::{Map, Value};

/// UI sessions belong to an installation, across its windows. Passive telemetry,
/// performance reports and update checks never create or extend a session.
#[derive(Default)]
pub(super) struct Session {
    id: Option<String>,
    started_us: u64,
    active_us: u64,
}

impl Session {
    pub(super) fn observe(
        &mut self,
        name: &str,
        properties: &Map<String, Value>,
        surface: &str,
        now: u64,
    ) -> Option<&str> {
        let surface = properties
            .get("surface")
            .and_then(Value::as_str)
            .unwrap_or(surface);
        let foreground_event = matches!(surface, "browser" | "desktop")
            && matches!(
                name,
                "app_opened"
                    | "engagement_started"
                    | "app_active_time"
                    | "feature_used"
                    | "download_clicked"
                    | "setting_changed"
                    | "feedback_opened"
                    | "feedback_submitted"
                    | "telemetry_layout_changed"
            );
        let expired = self.id.is_none()
            || now < self.active_us
            || now.saturating_sub(self.active_us) >= 30 * 60 * 1_000_000
            || now.saturating_sub(self.started_us) >= 24 * 60 * 60 * 1_000_000;
        if expired {
            self.id = None;
            if foreground_event {
                self.id = Some(session_uuid(now));
                self.started_us = now;
            }
        }
        if foreground_event {
            self.active_us = now;
        }
        // Background install/update activity has no session, even during a visit.
        if matches!(
            name,
            "install_completed" | "update_checked" | "$create_alias"
        ) {
            return None;
        }
        self.id.as_deref()
    }
}

/// `PostHog` sessions require `UUIDv7`: capture-time milliseconds then random bits.
fn session_uuid(now_us: u64) -> String {
    let mut id = super::uuid_v4();
    let millis = (now_us / 1000).min(0xffff_ffff_ffff);
    id.replace_range(..8, &format!("{:08x}", millis >> 16));
    id.replace_range(9..13, &format!("{:04x}", millis & 0xffff));
    id.replace_range(14..15, "7");
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sessions_rotate_without_activity_and_background_events_never_extend_them() {
        let minute = 60 * 1_000_000;
        let mut session = Session::default();
        let properties = Map::new();
        assert!(
            session
                .observe("vehicle_flight_time", &properties, "daemon", 0)
                .is_none()
        );
        let first = session
            .observe("app_opened", &properties, "desktop", 0)
            .unwrap()
            .to_owned();
        assert_eq!(
            session.observe("app_opened", &properties, "browser", minute),
            Some(first.as_str())
        );
        assert_eq!(
            session.observe("task_completed", &properties, "daemon", 2 * minute),
            Some(first.as_str())
        );
        assert!(
            session
                .observe("update_checked", &properties, "daemon", 20 * minute)
                .is_none()
        );
        assert_eq!(
            session.observe("app_performance", &properties, "desktop", 30 * minute),
            Some(first.as_str())
        );
        assert!(
            session
                .observe("vehicle_flight_time", &properties, "daemon", 31 * minute)
                .is_none()
        );
        let second = session
            .observe("app_active_time", &properties, "browser", 32 * minute)
            .unwrap()
            .to_owned();
        assert_ne!(first, second);
        for minute in 33..(32 + 24 * 60) {
            assert_eq!(
                session.observe(
                    "app_active_time",
                    &properties,
                    "browser",
                    minute * 60 * 1_000_000
                ),
                Some(second.as_str())
            );
        }
        assert_ne!(
            session.observe(
                "app_active_time",
                &properties,
                "browser",
                (32 + 24 * 60) * minute
            ),
            Some(second.as_str())
        );
        assert_ne!(
            session.observe("app_opened", &properties, "browser", 0),
            Some(second.as_str())
        );
    }

    #[test]
    fn session_uuids_are_version_7_and_encode_the_capture_timestamp() {
        let now = 1_790_753_905_962_742;
        let id = session_uuid(now);
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "7");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        let millis = u64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).unwrap();
        assert_eq!(millis, now / 1000);
        assert_ne!(id, session_uuid(now));
    }
}
