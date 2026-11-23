//! The camera computer's microphones (ADR-0067 M2, M3): the sound capture
//! devices ALSA has, read from `/proc/asound` and `/sys/class/sound`, and
//! the one each camera's `microphone` choice gives.
//!
//! A card is a capture device when its `/proc/asound/card<N>` folder has a
//! `pcm<N>c` folder. One with none plays only, such as the Pi's HDMI. Its
//! name is the text after ` - ` on the card's first line in
//! `/proc/asound/cards`, and its id survives reboots and replugging into
//! the same port: `usb:<bus path>` when `/sys/class/sound/card<N>/device`
//! leads to a USB interface such as `3-1.2:1.0`, else `card:<ALSA id>`.
//! Never the card number, which changes with the order devices appear.
//!
//! `camera.py` reads the same files by the same rules for the vehicle
//! page, and both are tested on `tests/fixtures/microphones`.

use std::fmt;
use std::path::Path;

use serde::Serialize;

/// A sound capture device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Microphone {
    /// `usb:<bus path>` or `card:<ALSA id>`.
    pub id: String,
    /// The card's long name, such as `USB PnP Sound Device`.
    pub name: String,
    /// The ALSA card id, such as `Device`.
    pub card: String,
    pub usb: bool,
    /// What `alsasrc` opens, such as `hw:CARD=Device,DEV=0`.
    #[serde(skip)]
    pub alsa: String,
}

/// The capture devices under `root` (`/` on the computer), in card order.
/// A computer with no sound cards, or no ALSA, has none.
#[must_use]
pub fn find(root: &Path) -> Vec<Microphone> {
    let asound = root.join("proc/asound");
    let Ok(cards) = std::fs::read_to_string(asound.join("cards")) else {
        return Vec::new();
    };
    cards
        .lines()
        .filter_map(card_line)
        .filter_map(|(number, card, name)| {
            let device = capture_device(&asound.join(format!("card{number}")))?;
            let usb = usb_path(&root.join(format!("sys/class/sound/card{number}/device")));
            Some(Microphone {
                id: usb
                    .as_ref()
                    .map_or_else(|| format!("card:{card}"), |bus| format!("usb:{bus}")),
                name,
                alsa: format!("hw:CARD={card},DEV={device}"),
                card,
                usb: usb.is_some(),
            })
        })
        .collect()
}

/// A card's first line in `/proc/asound/cards`: ` 2 [Device         ]:
/// USB-Audio - USB PnP Sound Device`. Its number, ALSA id and name.
fn card_line(line: &str) -> Option<(u32, String, String)> {
    let (number, rest) = line.trim_start().split_once(' ')?;
    let number = number.parse().ok()?;
    let rest = rest.trim_start().strip_prefix('[')?;
    let (card, rest) = rest.split_once(']')?;
    let (_, name) = rest.split_once(" - ")?;
    Some((number, card.trim().to_owned(), name.trim().to_owned()))
}

/// The lowest capture device number of a card, from its `pcm<N>c` folders.
fn capture_device(card: &Path) -> Option<u32> {
    std::fs::read_dir(card)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            name.to_str()?
                .strip_prefix("pcm")?
                .strip_suffix('c')?
                .parse::<u32>()
                .ok()
        })
        .min()
}

/// The USB bus path a card's device leads to, such as `3-1.2` for the
/// interface `3-1.2:1.0`. None for a card that is not USB.
fn usb_path(device: &Path) -> Option<String> {
    let resolved = std::fs::canonicalize(device).ok()?;
    let interface = resolved.file_name()?.to_str()?;
    let (bus, configuration) = interface.split_once(':')?;
    let (root_hub, ports) = bus.split_once('-')?;
    let made_of = |text: &str, besides: &str| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_digit() || besides.contains(c))
    };
    (made_of(root_hub, "") && made_of(ports, ".") && made_of(configuration, "."))
        .then(|| bus.to_owned())
}

/// Which microphone a camera's sound comes from: `microphone` in its
/// `[cameras.<path>]` table, `auto` when it has none.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "String")]
pub enum Choice {
    /// The only capture device when there is exactly one, else the first
    /// USB one in card order, else none.
    #[default]
    Auto,
    None,
    /// A device by its id, `usb:…` or `card:…`, and no other.
    Device(String),
}

impl TryFrom<String> for Choice {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        match text.as_str() {
            "auto" => Ok(Self::Auto),
            "none" => Ok(Self::None),
            id if ["usb:", "card:"].iter().any(|kind| {
                id.strip_prefix(kind)
                    .is_some_and(|rest| !rest.trim().is_empty())
            }) =>
            {
                Ok(Self::Device(text))
            }
            _ => Err(format!(
                "microphone = {text:?} is not \"auto\", \"none\" or a device's id such as \"usb:1-1.2\" or \"card:Loopback\""
            )),
        }
    }
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::None => f.write_str("none"),
            Self::Device(id) => f.write_str(id),
        }
    }
}

/// What a choice gives among the devices found now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved<'a> {
    Device(&'a Microphone),
    /// `auto` with no capture device.
    NoMicrophone,
    /// The chosen device is not plugged in.
    Unplugged(String),
    /// `none`.
    Off,
}

impl Choice {
    #[must_use]
    pub fn resolve<'a>(&self, found: &'a [Microphone]) -> Resolved<'a> {
        match self {
            Self::None => Resolved::Off,
            Self::Auto => automatic(found).map_or(Resolved::NoMicrophone, Resolved::Device),
            Self::Device(id) => found
                .iter()
                .find(|m| &m.id == id)
                .map_or_else(|| Resolved::Unplugged(id.clone()), Resolved::Device),
        }
    }
}

/// The device Automatic uses: the only one, else the first USB one.
#[must_use]
pub fn automatic(found: &[Microphone]) -> Option<&Microphone> {
    match found {
        [only] => Some(only),
        _ => found.iter().find(|m| m.usb),
    }
}

#[cfg(test)]
pub(in crate::video) mod tests;
