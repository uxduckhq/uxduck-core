//! A payload on the one link a ground station has to a simulated vehicle
//! (ADR-0063 Y2): the connection to the autopilot's SERIAL0, which S400
//! found is enough for ArduPilot to find and drive a camera and a gimbal.
//!
//! The payload has no port of its own, so the link is a wire the three
//! share. Whoever holds it passes every frame through here, each way, and
//! does what [`Heard`] says. The ground station hears everything the
//! autopilot and the payload say. The autopilot hears everything the
//! payload says, and everything the ground station says but what names the
//! camera or the gimbal.

use super::{AUTOPILOT, Faults, ManagedBy, Payload, Profile, View};

const HEARTBEAT: u32 = 0;

/// What came of a frame on the link once the payload heard it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Heard {
    /// The frame is the payload's alone and must not go on to the
    /// autopilot. ArduPilot does not pass such a frame back down the link
    /// it came by: it answers it itself, with the wrong result (S400).
    /// Only a frame from the ground station is ever kept.
    pub kept: bool,
    /// What the payload says, for the autopilot and the ground station
    /// both. Each of its components counts its frames, so a ground station
    /// shown only some of them would count the rest as lost.
    pub said: Vec<Vec<u8>>,
}

/// A simulated vehicle's payload, with the autopilot as its gimbal's manager.
#[derive(Clone, Debug)]
pub struct OnLink {
    payload: Payload,
    camera: bool,
    gimbal: bool,
    /// What kind of camera and gimbal they are (ADR-0063 Y16).
    profile: Profile,
    /// Where the camera's picture is sent as RTP over UDP (S408), if
    /// whoever holds the link sends one.
    stream: Option<String>,
    /// The room left on the camera's card, when not a whole empty card.
    card_left_mib: Option<f32>,
    /// What the camera says of its definition file, when not its own.
    defined: Option<Defined>,
}

/// What a simulated camera says of its definition file in place of its
/// own (S411): a development seam, to see a file at another address, one
/// UXDuck refuses, or a camera that names none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defined {
    /// The address it names in place of its own. `Some(None)` names none.
    pub uri: Option<Option<String>>,
    /// The file it serves over MAVLink FTP in place of its own.
    pub file: Option<Vec<u8>>,
}

impl Defined {
    /// Makes a camera say this of its definition file.
    pub fn apply(&self, camera: &mut super::camera::Camera) {
        if let Some(uri) = &self.uri {
            camera.definition_uri.clone_from(uri);
        }
        if let Some(file) = &self.file {
            camera.serve_definition(file);
        }
    }
}

impl OnLink {
    /// The payload of the vehicle with this system id, or none when the
    /// simulation carries neither a camera nor a gimbal.
    #[must_use]
    pub fn new(system: u8, camera: bool, gimbal: bool) -> Option<Self> {
        (camera || gimbal).then(|| Self {
            payload: build(system, camera, gimbal, Profile::Standard, None),
            camera,
            gimbal,
            profile: Profile::Standard,
            stream: None,
            card_left_mib: None,
            defined: None,
        })
    }

    /// As this kind of camera and gimbal (ADR-0063 Y16), in place of the
    /// camera and the gimbal device apart from it that it is otherwise.
    #[must_use]
    pub fn fitted(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self.start_over(self.payload.system);
        self
    }

    /// Switches faults on and off ([`Payload::set_faults`]). They stay on
    /// when the payload starts over as another system's.
    pub fn set_faults(&mut self, faults: Faults) {
        self.payload.set_faults(faults);
    }

    /// The faults that are on now ([`Payload::faults`]).
    #[must_use]
    pub fn faults(&self) -> Faults {
        self.payload.faults()
    }

    fn start_over(&mut self, system: u8) {
        let faults = self.payload.faults();
        self.payload = build(
            system,
            self.camera,
            self.gimbal,
            self.profile,
            self.stream.as_deref(),
        );
        if let Some(mib) = self.card_left_mib {
            self.leave_room(mib);
        }
        if let Some(defined) = self.defined.take() {
            self.define(defined);
        }
        self.payload.set_faults(faults);
    }

    /// With the camera's picture sent as RTP over UDP to `uri` (S408),
    /// such as `udp://127.0.0.1:15600`: the camera names it as its video
    /// stream. Without a camera there is none to name.
    #[must_use]
    pub fn streaming(mut self, uri: &str) -> Self {
        if self.camera {
            self.stream = Some(uri.to_owned());
            if let Some(camera) = &mut self.payload.camera {
                camera.stream.clone_from(&self.stream);
                camera.stream_rtp = true;
            }
        }
        self
    }

    /// The payload itself, for a test to look inside it.
    #[cfg(test)]
    pub(super) fn payload(&self) -> &Payload {
        &self.payload
    }

    /// What the camera's video shows now: the simulation's truth, for
    /// whoever draws its picture. None with no camera, with the camera's
    /// power off, and until the autopilot has said where the vehicle is.
    #[must_use]
    pub fn view(&self) -> Option<View> {
        self.payload.view()
    }

    /// Leaves this much room on the camera's card, in mebibytes, now and
    /// each time the payload starts over: a development seam, to see what
    /// a card nearly or wholly full does without recording for hours
    /// (S405).
    pub fn leave_room(&mut self, mib: f32) {
        self.card_left_mib = Some(mib);
        if let Some(camera) = &mut self.payload.camera {
            camera.leave_room(mib);
        }
    }

    /// Makes the camera say this of its definition file, now and each
    /// time the payload starts over (S411).
    pub fn define(&mut self, defined: Defined) {
        if let Some(camera) = &mut self.payload.camera {
            defined.apply(camera);
        }
        self.defined = Some(defined);
    }

    /// Heartbeats on the host's clock: as the link comes up, and about
    /// once a second after ([`Payload::announce`]).
    pub fn announce(&mut self) -> Vec<Vec<u8>> {
        self.payload.announce()
    }

    /// One whole frame the autopilot sent, and what the payload says to it.
    /// The frame stays the ground station's to hear too: the autopilot
    /// numbers its frames, and one held back would count as lost.
    pub fn from_autopilot(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        // The vehicle was given another system id (`MAV_SYSID`): the
        // payload is the same vehicle's, and starts again as its own.
        if let Some(header) = crate::framing::header(frame)
            && header.component_id == AUTOPILOT
            && header.message_id == HEARTBEAT
            && header.system_id != self.payload.system
        {
            self.start_over(header.system_id);
        }
        self.payload.receive(frame)
    }

    /// One whole frame a ground station sends to the vehicle.
    pub fn from_ground(&mut self, frame: &[u8]) -> Heard {
        Heard {
            kept: self.payload.takes(frame),
            said: self.payload.receive(frame),
        }
    }
}

fn build(
    system: u8,
    camera: bool,
    gimbal: bool,
    profile: Profile,
    stream: Option<&str>,
) -> Payload {
    let mut payload = Payload::new(system);
    let stream = stream.map(str::to_owned);
    let rtp = stream.is_some();
    if camera && gimbal && profile.unit() {
        payload = payload.with_unit(stream);
    } else {
        if camera {
            payload = payload.with_camera(stream);
        }
        if gimbal {
            payload = payload.with_gimbal(ManagedBy::Autopilot);
        }
    }
    if let Some(camera) = &mut payload.camera {
        camera.stream_rtp = rtp;
        camera.video_only = profile == Profile::StreamOnly;
        camera.mode_bound = profile == Profile::PhotoVideo;
        // A camera that tracks keeps its own gimbal on what it tracks, so
        // it is the unit that has both.
        camera.thermal_sensor = profile == Profile::ThermalTracking && gimbal;
        camera.tracks = profile == Profile::ThermalTracking && gimbal;
    }
    payload
}
