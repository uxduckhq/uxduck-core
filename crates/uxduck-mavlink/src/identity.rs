//! Vehicle ids (ADR-0004): a UUID v5 of the autopilot's UID, so every daemon
//! that sees the same aircraft agrees on its id without coordination.

use uuid::Uuid;

/// The namespace for `UXDuck` vehicle ids. Fixed forever: changing it would
/// give every vehicle a new id.
const NAMESPACE: Uuid = Uuid::from_bytes([
    0x6f, 0x2a, 0x0e, 0x53, 0x1c, 0x6b, 0x4d, 0x57, 0x9a, 0x1e, 0x3b, 0x55, 0x78, 0x4a, 0x0d, 0x21,
]);

/// The id from the `AUTOPILOT_VERSION` UIDs: `uid2` when the autopilot
/// fills it (the full 18-byte hardware id), otherwise `uid`. `None` when both
/// are zero, which some autopilots and simulators send.
#[must_use]
pub fn vid_from_uid(uid: u64, uid2: [u8; 18]) -> Option<String> {
    if uid2.iter().any(|b| *b != 0) {
        return Some(Uuid::new_v5(&NAMESPACE, &[b"uid2:".as_slice(), &uid2].concat()).to_string());
    }
    (uid != 0).then(|| Uuid::new_v5(&NAMESPACE, format!("uid:{uid:016x}").as_bytes()).to_string())
}

/// A simulated vehicle's id: its UID within its simulation, since every
/// simulator of a build reports the same UID. A new simulation is a new
/// vehicle; one started again on its link keeps its id.
#[must_use]
pub fn simulated_vid(link_id: &str, uid: u64, uid2: [u8; 18]) -> Option<String> {
    let uid = uid_text(uid, uid2)?;
    Some(Uuid::new_v5(&NAMESPACE, format!("simulation:{link_id}|{uid}").as_bytes()).to_string())
}

/// The UID as shown to people.
#[must_use]
pub fn uid_text(uid: u64, uid2: [u8; 18]) -> Option<String> {
    if uid2.iter().any(|b| *b != 0) {
        return Some(uid2.iter().fold(String::with_capacity(36), |mut out, b| {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
            out
        }));
    }
    (uid != 0).then(|| format!("{uid:016x}"))
}

/// A provisional id before the UID is known: stable for the same link, peer
/// and system id, so a restart does not mint a new one. Replaced, with an
/// alias, once the UID arrives.
#[must_use]
pub fn provisional_vid(link: &str, peer: &str, system_id: u8) -> String {
    Uuid::new_v5(
        &NAMESPACE,
        format!("provisional:{link}|{peer}|{system_id}").as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_uid_gives_the_same_id_everywhere() {
        let a = vid_from_uid(0x1234, [0; 18]);
        assert_eq!(a, vid_from_uid(0x1234, [0; 18]));
        assert_ne!(a, vid_from_uid(0x1235, [0; 18]));
        let mut uid2 = [0u8; 18];
        uid2[0] = 1;
        assert_ne!(vid_from_uid(0x1234, uid2), a, "uid2 wins when present");
        assert_eq!(vid_from_uid(0, [0; 18]), None);
        assert_eq!(
            uid_text(0x1234, [0; 18]).as_deref(),
            Some("0000000000001234")
        );
        assert_ne!(simulated_vid("sim-a", 0x1234, [0; 18]), a);
        assert_ne!(
            simulated_vid("sim-a", 0x1234, [0; 18]),
            simulated_vid("sim-b", 0x1234, [0; 18]),
            "two simulations of one build are two vehicles"
        );
        assert_eq!(simulated_vid("sim-a", 0, [0; 18]), None);
        assert_ne!(
            provisional_vid("l1", "1.2.3.4:5", 1),
            provisional_vid("l1", "1.2.3.4:5", 2)
        );
    }
}
