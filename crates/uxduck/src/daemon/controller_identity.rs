use std::collections::BTreeMap;
use std::fmt::Write;

use sha2::{Digest, Sha256};
use uxduck_core::domain::controller::CONTROLLER_UNIT_ID_PREFIX;

pub(super) struct Device {
    pub entry: u64,
    pub vendor: u16,
    pub product: u16,
    pub serial: Option<String>,
    pub physical: bool,
}

pub(super) fn persistent_id(
    entry: u64,
    vendor: u16,
    product: u16,
    admitted: &[u64],
    devices: &[Device],
) -> Option<String> {
    if admitted.iter().filter(|&&id| id == entry).count() != 1 {
        return None;
    }
    let mut matching = devices.iter().filter(|device| device.entry == entry);
    let device = matching.next()?;
    if matching.next().is_some()
        || !device.physical
        || device.vendor != vendor
        || device.product != product
    {
        return None;
    }
    let serial = serial(device)?;
    if devices
        .iter()
        .filter(|candidate| {
            candidate.vendor == vendor
                && candidate.product == product
                && self::serial(candidate).as_deref() == Some(&serial)
        })
        .count()
        != 1
    {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(b"uxduck/controller-unit/1\0");
    hash.update(vendor.to_le_bytes());
    hash.update(product.to_le_bytes());
    hash.update(device.serial.as_deref()?.as_bytes());
    Some(hash.finalize()[..28].iter().fold(
        CONTROLLER_UNIT_ID_PREFIX.to_owned(),
        |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        },
    ))
}

fn serial(device: &Device) -> Option<String> {
    let value = device.serial.as_deref()?.trim().to_ascii_lowercase();
    (!value.is_empty()
        && !value.chars().all(|c| c == '0')
        && !matches!(value.as_str(), "unknown" | "none" | "n/a"))
    .then_some(value)
}

pub(super) struct Attachments {
    session: String,
    next: u64,
    ids: BTreeMap<usize, (u64, String)>,
}

impl Attachments {
    pub fn new() -> Result<Self, getrandom::Error> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce)?;
        let session = nonce.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        });
        Ok(Self {
            session,
            next: 0,
            ids: BTreeMap::new(),
        })
    }

    pub fn connect(&mut self, slot: usize, entry: u64, persistent: Option<String>) -> String {
        self.next += 1;
        let id = persistent.unwrap_or_else(|| format!("gp-session-{}-{}", self.session, self.next));
        self.ids.insert(slot, (entry, id.clone()));
        id
    }

    pub fn get(&self, slot: usize, entry: Option<u64>) -> Option<String> {
        let (admitted, id) = self.ids.get(&slot)?;
        (Some(*admitted) == entry).then(|| id.clone())
    }

    pub fn remove(&mut self, slot: usize) -> Option<String> {
        self.ids.remove(&slot).map(|(_, id)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(entry: u64, serial: Option<&str>) -> Device {
        Device {
            entry,
            vendor: 0x1234,
            product: 0x5678,
            serial: serial.map(str::to_owned),
            physical: true,
        }
    }

    fn identity(entry: u64, devices: &[Device]) -> Option<String> {
        persistent_id(entry, 0x1234, 0x5678, &[entry], devices)
    }

    #[test]
    fn identical_models_join_exact_attachments_and_have_distinct_unit_ids() {
        let devices = [
            device(11, Some("test-unit-a")),
            device(22, Some("test-unit-b")),
        ];
        let first = identity(11, &devices).unwrap();
        let second = identity(22, &devices).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(CONTROLLER_UNIT_ID_PREFIX) && first.len() <= 64);
        assert!(!first.contains("test-unit"));
    }

    #[test]
    fn missing_placeholder_or_duplicate_serials_cannot_persist() {
        for serial in [
            None,
            Some(""),
            Some("  "),
            Some("00000000"),
            Some("unknown"),
        ] {
            assert!(identity(11, &[device(11, serial)]).is_none());
        }
        let devices = [
            device(11, Some("test-unit-a")),
            device(22, Some(" TEST-UNIT-A ")),
        ];
        assert!(identity(11, &devices).is_none());
        assert!(identity(22, &devices).is_none());
    }

    #[test]
    fn separately_attached_serials_with_different_case_do_not_alias() {
        let lower = identity(11, &[device(11, Some("test-unit-a"))]).unwrap();
        let upper = identity(22, &[device(22, Some("TEST-UNIT-A"))]).unwrap();
        assert_ne!(lower, upper);
    }

    #[test]
    fn separately_attached_serials_with_different_padding_do_not_alias() {
        let exact = identity(11, &[device(11, Some("test-unit-a"))]).unwrap();
        let padded = identity(22, &[device(22, Some(" test-unit-a "))]).unwrap();
        assert_ne!(exact, padded);
    }

    #[test]
    fn wrappers_ambiguous_entries_and_model_mismatches_fail_closed() {
        let mut wrapper = device(11, Some("synthetic-unit"));
        wrapper.physical = false;
        assert!(identity(11, &[wrapper]).is_none());
        let devices = [
            device(11, Some("test-unit-a")),
            device(11, Some("test-unit-b")),
        ];
        assert!(identity(11, &devices).is_none());
        assert!(persistent_id(11, 1, 2, &[11], &[device(11, Some("test-unit-a"))]).is_none());
        assert!(persistent_id(11, 0x1234, 0x5678, &[11, 11], &devices[..1]).is_none());
        assert!(persistent_id(11, 0x1234, 0x5678, &[], &devices[..1]).is_none());
    }

    #[test]
    fn queued_disconnect_cannot_join_the_replacement_with_the_same_model() {
        let replacement = [device(22, Some("test-unit-b"))];
        assert!(identity(11, &replacement).is_none());
        assert!(identity(11, &[]).is_none());
        assert!(identity(22, &replacement).is_some());
    }

    #[test]
    fn serial_identity_survives_restart_reenumeration_and_candidate_order() {
        let first = identity(11, &[device(11, Some("test-unit-a"))]);
        let restarted = identity(
            99,
            &[
                device(88, Some("test-unit-b")),
                device(99, Some("test-unit-a")),
            ],
        );
        assert_eq!(first, restarted);
        assert_ne!(first, identity(99, &[device(99, Some("test-unit-b"))]));
    }

    #[test]
    fn removal_and_slot_reuse_invalidate_associations_without_restoring_authority() {
        let mut ids = Attachments::new().unwrap();
        let stable = identity(11, &[device(11, Some("test-unit-a"))]);
        let first = ids.connect(3, 11, stable.clone());
        assert_eq!(ids.get(3, Some(11)), Some(first.clone()));
        assert!(ids.get(3, None).is_none());
        assert!(ids.get(3, Some(22)).is_none());
        assert_eq!(ids.remove(3), Some(first.clone()));
        assert!(ids.get(3, Some(11)).is_none());
        let replacement = ids.connect(3, 22, None);
        assert_ne!(replacement, first);
        assert!(replacement.starts_with("gp-session-"));
        ids.remove(3);
        assert_eq!(ids.connect(4, 99, stable), first);
    }

    #[test]
    fn session_only_ids_do_not_survive_replug_or_restart() {
        let mut ids = Attachments::new().unwrap();
        let first = ids.connect(3, 11, None);
        ids.remove(3);
        assert_ne!(first, ids.connect(3, 22, None));
        assert_ne!(first, Attachments::new().unwrap().connect(3, 11, None));
    }
}
