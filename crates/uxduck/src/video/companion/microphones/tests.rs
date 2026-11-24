//! Discovery on the shared fake `/proc` and `/sys` trees, which
//! `camera.py` reads too, a microphone plugged in and pulled out, and what
//! each choice gives.

#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::{Path, PathBuf};

use super::*;

#[cfg(unix)]
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/microphones")
}

/// A case copied into `to`, with its `/sys` built from its `devices.json`:
/// each card's device folder under `sys/devices` and the
/// `sys/class/sound/<card>/device` link to it. The repository keeps no
/// `sys` tree, because a USB device's folder has a colon in its name, which
/// Windows cannot check out.
#[cfg(unix)]
pub(crate) fn copy_case(case: &str, to: &Path) {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
    let from = fixtures().join(case);
    copy(&from, to);
    let devices: BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(from.join("devices.json")).unwrap()).unwrap();
    for (card, device) in devices {
        std::fs::create_dir_all(to.join("sys/devices").join(&device)).unwrap();
        let class = to.join("sys/class/sound").join(card);
        std::fs::create_dir_all(&class).unwrap();
        std::os::unix::fs::symlink(
            Path::new("../../../devices").join(device),
            class.join("device"),
        )
        .unwrap();
    }
}

/// A case built in a folder of its own, removed when it is dropped.
#[cfg(unix)]
pub(crate) fn case(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    copy_case(name, dir.path());
    dir
}

#[cfg(unix)]
#[test]
fn every_case_finds_the_devices_and_the_automatic_choice_its_expected_json_says() {
    let mut cases = 0;
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let entry = entry.unwrap();
        if !entry.path().is_dir() {
            continue;
        }
        cases += 1;
        let built = case(entry.file_name().to_str().unwrap());
        let dir = built.path();
        let expected: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("expected.json")).unwrap()).unwrap();
        let found = find(dir);
        let as_json: Vec<serde_json::Value> = found
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "name": m.name,
                    "card": m.card,
                    "usb": m.usb,
                    "alsa": m.alsa,
                })
            })
            .collect();
        assert_eq!(
            serde_json::Value::from(as_json),
            expected["microphones"],
            "{}",
            dir.display()
        );
        assert_eq!(
            automatic(&found).map(|m| m.id.as_str()),
            expected["automatic"].as_str(),
            "{}",
            dir.display()
        );
    }
    assert_eq!(cases, 4, "none, one-usb, several and loopback-only");
}

#[cfg(unix)]
#[test]
fn a_microphone_plugged_in_and_pulled_out_is_seen_at_the_next_read() {
    let root = tempfile::tempdir().unwrap();
    copy_case("none", root.path());
    assert!(find(root.path()).is_empty(), "HDMI plays only");

    // A USB microphone appears as card 2.
    let asound = root.path().join("proc/asound");
    let mut cards = std::fs::read_to_string(asound.join("cards")).unwrap();
    cards.push_str(" 2 [Device         ]: USB-Audio - USB PnP Sound Device\n                      C-Media Electronics Inc. USB PnP Sound Device at usb-xhci-hcd.1-1.2, full speed\n");
    std::fs::write(asound.join("cards"), &cards).unwrap();
    std::fs::create_dir_all(asound.join("card2/pcm0c")).unwrap();
    let interface = root.path().join("sys/devices/usb1/1-1/1-1.2/1-1.2:1.0");
    std::fs::create_dir_all(&interface).unwrap();
    std::fs::create_dir_all(root.path().join("sys/class/sound/card2")).unwrap();
    std::os::unix::fs::symlink(
        "../../../devices/usb1/1-1/1-1.2/1-1.2:1.0",
        root.path().join("sys/class/sound/card2/device"),
    )
    .unwrap();
    let found = find(root.path());
    assert_eq!(
        found,
        [Microphone {
            id: "usb:1-1.2".into(),
            name: "USB PnP Sound Device".into(),
            card: "Device".into(),
            usb: true,
            alsa: "hw:CARD=Device,DEV=0".into(),
        }]
    );
    assert_eq!(
        Choice::Auto.resolve(&found),
        Resolved::Device(&found[0]),
        "automatic takes the only one"
    );

    // Pulled out: ALSA drops its card.
    let without = cards.lines().take(4).collect::<Vec<_>>().join("\n") + "\n";
    std::fs::write(asound.join("cards"), without).unwrap();
    std::fs::remove_dir_all(asound.join("card2")).unwrap();
    assert!(find(root.path()).is_empty());
    assert_eq!(
        Choice::Device("usb:1-1.2".into()).resolve(&find(root.path())),
        Resolved::Unplugged("usb:1-1.2".into())
    );
}

#[test]
fn no_alsa_at_all_is_no_microphone() {
    let root = tempfile::tempdir().unwrap();
    assert!(find(root.path()).is_empty());
    assert_eq!(Choice::Auto.resolve(&[]), Resolved::NoMicrophone);
}

fn microphone(id: &str, usb: bool) -> Microphone {
    Microphone {
        id: id.into(),
        name: id.into(),
        card: id.into(),
        usb,
        alsa: String::new(),
    }
}

#[test]
fn a_choice_gives_its_own_device_or_says_why_there_is_none() {
    let loopback = microphone("card:Loopback", false);
    let usb = microphone("usb:3-1.2", true);
    let snowball = microphone("usb:1-2", true);
    let several = [loopback.clone(), usb.clone(), snowball.clone()];
    assert_eq!(
        Choice::Auto.resolve(&several),
        Resolved::Device(&several[1]),
        "the first USB one in card order"
    );
    assert_eq!(
        Choice::Auto.resolve(std::slice::from_ref(&loopback)),
        Resolved::Device(&loopback),
        "the only one, USB or not"
    );
    assert_eq!(
        Choice::Auto.resolve(&[loopback.clone(), microphone("card:Other", false)]),
        Resolved::NoMicrophone,
        "several and none of them USB"
    );
    assert_eq!(
        Choice::Device("usb:1-2".into()).resolve(&several),
        Resolved::Device(&several[2])
    );
    assert_eq!(
        Choice::Device("usb:9-9".into()).resolve(&several),
        Resolved::Unplugged("usb:9-9".into()),
        "never another device"
    );
    assert_eq!(Choice::None.resolve(&several), Resolved::Off);
}

#[test]
fn a_choice_reads_auto_none_or_an_id_and_says_what_else_it_takes() {
    for (text, choice) in [
        ("auto", Choice::Auto),
        ("none", Choice::None),
        ("usb:3-1.2", Choice::Device("usb:3-1.2".into())),
        ("card:Loopback", Choice::Device("card:Loopback".into())),
    ] {
        assert_eq!(Choice::try_from(text.to_owned()).unwrap(), choice);
        assert_eq!(choice.to_string(), text);
    }
    for wrong in ["Auto", "hw:1,0", "usb:", "card: ", "2", ""] {
        let error = Choice::try_from(wrong.to_owned()).unwrap_err();
        assert!(error.contains("is not \"auto\", \"none\""), "{error}");
    }
}
