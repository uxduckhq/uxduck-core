//! Whether a camera's sound can be heard, and why not, in words for
//! people that name the vehicle and the camera (ADR-0067 M13): from what
//! UXDuck's sender on a companion computer says of the session's sound
//! (M9), else from the sound the camera's stream carries. A microphone
//! plugged in or pulled out while the camera plays raises a notice.

use serde::Deserialize;
use uxduck_core::domain::companion::CompanionComputer;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::domain::video::{
    VideoSound, VideoSoundCompanion, VideoSoundState, VideoSource, VideoState, url_host,
};
use uxduck_core::events::{Actor, Change};

use crate::Engine;
use crate::commands::Raise;

/// What UXDuck's sender says of one session's sound (ADR-0067 M9), as it
/// answers the session's PATCH and GET.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SenderSound {
    pub state: SenderSoundState,
    /// The session asked for its sound.
    #[serde(default)]
    pub on: bool,
    #[serde(default)]
    pub microphone: Option<SenderMicrophone>,
    /// Such as "Opus, 48 kHz, mono".
    #[serde(default)]
    pub codec: Option<String>,
    /// The Opus bit/s sent to this session now.
    #[serde(default)]
    pub bitrate: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The sound's state on the sender (ADR-0067 M9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SenderSoundState {
    /// Sent to this session now.
    Sending,
    /// A microphone is there and this session has not asked.
    Available,
    NoMicrophone,
    /// The device chosen is not plugged in.
    Unplugged,
    /// `microphone = "none"`.
    Off,
    /// `GStreamer` is missing or the device refused, with its reason.
    Failed,
}

/// A capture device on the sender's computer (ADR-0067 M2).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SenderMicrophone {
    pub id: String,
    /// Such as `USB PnP Sound Device`.
    pub name: String,
    #[serde(default)]
    pub card: String,
    #[serde(default)]
    pub usb: bool,
}

/// What a camera's worker saw of its sound lately, told once a second.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SoundSeen {
    /// What UXDuck's sender said of the session's sound, when it serves
    /// the camera.
    pub sender: Option<SenderSound>,
    /// Whether the camera has sound is not known: its server sends sound
    /// only when asked for it, and nobody has asked (ADR-0067 M10).
    pub unasked: bool,
    /// Sound is wanted and frames of it came in the last second.
    pub flowing: bool,
}

fn actor() -> Actor {
    Actor {
        client_id: "video".into(),
        operator_id: None,
        identity: None,
    }
}

/// "the Plane's camera computer", or "the camera computer" for a camera of
/// no vehicle.
fn camera_computer(vehicle: Option<&str>) -> String {
    vehicle.map_or_else(
        || "the camera computer".to_owned(),
        |name| format!("the {name}'s camera computer"),
    )
}

/// Opus is always 48 kHz, so its rate is left out: "Opus, mono".
fn short_codec(codec: &str) -> String {
    codec.replacen("Opus, 48 kHz, ", "Opus, ", 1)
}

/// The camera path an address names on its server: `front` for
/// `http://pi:8889/front/whep` or `rtsp://pi:8554/front`.
fn path_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.split_once('/')?.1;
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let path = path.trim_matches('/');
    let path = path.strip_suffix("/whep").unwrap_or(path);
    (!path.is_empty() && path != "whep").then(|| path.to_owned())
}

/// A camera's addresses, its own and its fallbacks.
fn addresses(source: &VideoSource) -> impl Iterator<Item = &str> {
    source
        .url
        .iter()
        .chain(&source.fallback_urls)
        .map(String::as_str)
}

impl Engine {
    /// The companion computer serving a camera playing from `host`, and its
    /// vehicle: the one chosen for the camera (S358), else the one whose
    /// Raspberry Pi camera one of the camera's addresses names (ADR-0060
    /// C8), else the one at `host`.
    pub(crate) fn serving_computer(
        &self,
        source: &VideoSource,
        host: &str,
    ) -> Option<(&Vehicle, &CompanionComputer)> {
        let vehicle = self.vehicle(source.vehicle_id.as_deref()?)?;
        let bare = |host: &str| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase()
        };
        let computers = &vehicle.companions;
        let computer = match &source.served_by {
            Some(id) => computers.iter().find(|c| &c.id == id),
            None => computers
                .iter()
                .find(|c| {
                    c.check
                        .as_ref()
                        .and_then(|check| check.cameras.as_ref())
                        .and_then(|cameras| cameras.named_by(addresses(source)))
                        .is_some()
                })
                .or_else(|| computers.iter().find(|c| bare(&c.host) == bare(host))),
        }?;
        Some((vehicle, computer))
    }

    /// The companion computer camera a source plays, by the vehicle page's
    /// ids, and the name of the source's vehicle: for the speaker's
    /// Microphone row and the words that name them.
    fn sound_companion(
        &self,
        source: &VideoSource,
    ) -> (Option<String>, Option<VideoSoundCompanion>) {
        let vehicle_name = source
            .vehicle_id
            .as_deref()
            .and_then(|id| self.vehicle(id))
            .map(|v| v.name.clone());
        let Some(playing) = source.active_url.as_ref().or(source.url.as_ref()) else {
            return (vehicle_name, None);
        };
        let Some((vehicle, computer)) = self.serving_computer(source, url_host(playing)) else {
            return (vehicle_name, None);
        };
        let path = computer
            .check
            .as_ref()
            .and_then(|check| check.cameras.as_ref())
            .and_then(|cameras| cameras.named_by(addresses(source)))
            .map(|camera| camera.name.clone())
            .or_else(|| path_of(playing));
        let companion = path.map(|path| VideoSoundCompanion {
            vehicle_id: vehicle.id.clone(),
            computer_id: computer.id.clone(),
            path,
        });
        (Some(vehicle.name.clone()), companion)
    }

    /// Whether a camera's sound can be heard, and why not (ADR-0067 M13).
    /// None while it is not known: before its stream is, or while its
    /// server was not asked for sound.
    #[must_use]
    pub fn video_sound(&self, source: &VideoSource) -> Option<VideoSound> {
        let seen = self.video.sound_seen.get(&source.id);
        let (vehicle, companion) = self.sound_companion(source);
        let computer = camera_computer(vehicle.as_deref());
        let camera = companion
            .as_ref()
            .map_or_else(|| source.name.clone(), |c| c.path.clone());
        let flowing = seen.is_some_and(|s| s.flowing);
        let heard_codec = source
            .audio
            .as_ref()
            .filter(|a| a.recorded)
            .map(|a| short_codec(&a.name));
        let sound = |state, reason: Option<String>| VideoSound {
            state,
            microphone: None,
            codec: None,
            reason,
            companion: companion.clone(),
            flowing: false,
            bitrate: None,
        };
        if let Some(sender) = seen.and_then(|s| s.sender.as_ref()) {
            return Some(match sender.state {
                SenderSoundState::Sending | SenderSoundState::Available => VideoSound {
                    microphone: sender.microphone.as_ref().map(|m| match &vehicle {
                        Some(vehicle) => format!("{} on the {vehicle}", m.name),
                        None => m.name.clone(),
                    }),
                    codec: heard_codec.or_else(|| sender.codec.as_deref().map(short_codec)),
                    flowing,
                    bitrate: sender.bitrate,
                    ..sound(VideoSoundState::Available, None)
                },
                SenderSoundState::NoMicrophone => sound(
                    VideoSoundState::NoMicrophone,
                    Some(format!(
                        "No microphone on {computer}. Plug in a USB microphone."
                    )),
                ),
                SenderSoundState::Unplugged => sound(
                    VideoSoundState::Unplugged,
                    Some(format!(
                        "The microphone chosen for {camera} is not plugged in on {computer}."
                    )),
                ),
                SenderSoundState::Off => sound(
                    VideoSoundState::Off,
                    Some(format!("Sound is off for {camera} on {computer}.")),
                ),
                SenderSoundState::Failed => {
                    let why = sender.reason.as_deref().unwrap_or_default();
                    let reason = if why.contains("GStreamer") || why.contains("gst-launch") {
                        format!("Sound needs GStreamer on {computer}.")
                    } else if why.is_empty() {
                        format!("Sound for {camera} failed on {computer}.")
                    } else {
                        format!("Sound for {camera} failed on {computer}: {why}")
                    };
                    sound(VideoSoundState::Failed, Some(reason))
                }
            });
        }
        if let Some(audio) = &source.audio {
            return Some(if audio.recorded {
                VideoSound {
                    codec: heard_codec,
                    flowing,
                    ..sound(VideoSoundState::Available, None)
                }
            } else {
                VideoSound {
                    codec: Some(audio.name.clone()),
                    ..sound(
                        VideoSoundState::Unsupported,
                        Some(format!(
                            "This camera sends {} sound, which UXDuck can't play.",
                            audio.name
                        )),
                    )
                }
            });
        }
        if seen.is_some_and(|s| s.unasked) || source.codec.is_none() {
            return None;
        }
        Some(sound(
            VideoSoundState::NoSound,
            Some("This camera sends no sound.".into()),
        ))
    }

    /// What a camera's worker saw of its sound, told once a second: its
    /// sound is said again when that changes it.
    pub fn video_sound_seen(&mut self, source_id: &str, seen: SoundSeen) {
        if self.video.sound_seen.get(source_id) == Some(&seen) {
            return;
        }
        self.video.sound_seen.insert(source_id.to_owned(), seen);
        let Some(mut source) = self.state.video_sources.get(source_id).cloned() else {
            return;
        };
        let sound = self.video_sound(&source).map(Box::new);
        if sound == source.sound {
            return;
        }
        let before = std::mem::replace(&mut source.sound, sound);
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.notice_microphone(&source, before.as_deref());
    }

    /// A notice when a playing camera's microphone was plugged in or pulled
    /// out on its companion computer (ADR-0067 M13).
    pub(crate) fn notice_microphone(&mut self, source: &VideoSource, before: Option<&VideoSound>) {
        let (Some(before), Some(now)) = (before, source.sound.as_ref()) else {
            return;
        };
        let Some(companion) = now.companion.as_ref() else {
            return;
        };
        if source.state != VideoState::Receiving {
            return;
        }
        let heard = |sound: &VideoSound| match sound.state {
            VideoSoundState::Available => Some(sound.microphone.is_some()),
            VideoSoundState::NoMicrophone | VideoSoundState::Unplugged => Some(false),
            _ => None,
        };
        let (Some(was), Some(is)) = (heard(before), heard(now)) else {
            return;
        };
        if was == is {
            return;
        }
        let vehicle = self
            .vehicle(&companion.vehicle_id)
            .map_or_else(|| source.name.clone(), |v| v.name.clone());
        let path = &companion.path;
        let (severity, title, body) = if is {
            (
                Severity::Info,
                format!("{vehicle}, {path}: a microphone was plugged in."),
                None,
            )
        } else {
            (
                Severity::Warning,
                format!("{vehicle}, {path}: the microphone was unplugged."),
                Some("The video keeps playing.".to_owned()),
            )
        };
        self.raise(Raise {
            severity,
            kind: NotificationKind::Transient,
            subject: Subject::VideoSource {
                id: source.id.clone(),
            },
            title,
            body,
            actions: Vec::new(),
            condition: None,
            report: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_camera_path_is_read_from_its_address() {
        assert_eq!(
            path_of("http://pi:8889/front/whep").as_deref(),
            Some("front")
        );
        assert_eq!(path_of("rtsp://pi:8554/front").as_deref(), Some("front"));
        assert_eq!(
            path_of("rtsp://u:p@gw:8554/video/rear/?x=1").as_deref(),
            Some("video/rear")
        );
        assert_eq!(path_of("http://pi:8889/whep"), None);
        assert_eq!(path_of("rtsp://pi:8554"), None);
    }

    #[test]
    fn the_senders_sound_is_read_as_it_answers() {
        let said: SenderSound = serde_json::from_str(
            r#"{"state":"sending","on":true,"microphone":{"id":"usb:3-1.2","name":"USB PnP Sound Device","card":"Device","usb":true},"codec":"Opus, 48 kHz, mono","bitrate":24000,"reason":null}"#,
        )
        .unwrap();
        assert_eq!(said.state, SenderSoundState::Sending);
        assert_eq!(said.microphone.unwrap().id, "usb:3-1.2");
        assert_eq!(said.bitrate, Some(24_000));
        let none: SenderSound = serde_json::from_str(
            r#"{"state":"noMicrophone","on":false,"microphone":null,"codec":null,"bitrate":null,"reason":null}"#,
        )
        .unwrap();
        assert_eq!(none.state, SenderSoundState::NoMicrophone);
        assert_eq!(short_codec("Opus, 48 kHz, mono"), "Opus, mono");
        assert_eq!(short_codec("AAC, 48 kHz, stereo"), "AAC, 48 kHz, stereo");
    }
}
