//! A camera's own settings in the session (ADR-0063 Y5, S411): its
//! definition file read over MAVLink FTP from the component that keeps it,
//! and the settings that file names read and written as extended
//! parameters, addressed to the camera's component.

use super::{CommandOrder, CommandOutcome, Effect, Session, files};
use crate::camera::Key;
use crate::camera_file::{self, READ_ID, Step};
use crate::camera_params::{Acked, Out, Outcome, Said, Told, Value};

type Address = (String, String, u8);

const NO_ADDRESS: &str = "This vehicle has no address in this session.";

/// Whether a command is one of a camera's own settings, which is written
/// as an extended parameter and not sent as a command.
pub(super) fn is_setting(command: &uxduck_core::domain::command::Command) -> bool {
    matches!(
        command,
        uxduck_core::domain::command::Command::Camera { action, .. } if action.setting().is_some()
    )
}

impl Session {
    /// Reads a camera's definition file over MAVLink FTP: `path` on the
    /// component `from`, or on the camera's own. Answered by one
    /// [`Effect::CameraDefinition`], with the file's bytes or why there
    /// are none.
    pub fn camera_definition(
        &mut self,
        vehicle_id: &str,
        component_id: u8,
        from: Option<u8>,
        path: &str,
        now_us: u64,
    ) -> Vec<Effect> {
        let failed = |why: &str| {
            vec![Effect::CameraDefinition {
                vehicle_id: vehicle_id.to_owned(),
                component_id,
                result: Err(why.into()),
            }]
        };
        let Some(key) = self.command_address(vehicle_id) else {
            return failed(NO_ADDRESS);
        };
        let Some(autopilot) = self.vehicles.get(&key).map(|v| v.autopilot_component) else {
            return failed(NO_ADDRESS);
        };
        let from = from.unwrap_or(component_id);
        let heard: Key = (key.0.clone(), key.1.clone(), key.2, from);
        if self.camera_files.reading(&heard) {
            return failed("Another definition file is being read from the same component.");
        }
        if from != autopilot {
            let output = self.camera_files.start(&heard, component_id, path, now_us);
            return self.camera_file_output(&heard, output, now_us);
        }
        // The autopilot's files are read one at a time, by its one client.
        self.camera_files.start_shared(&heard, component_id, now_us);
        let op = files::Op::Read {
            path: path.to_owned(),
            from: 0,
        };
        let output = self
            .vehicles
            .get_mut(&key)
            .map(|vehicle| vehicle.ftp.file(READ_ID, op, now_us))
            .unwrap_or_default();
        self.ftp_output(&key, output, now_us)
    }

    /// Reads a camera's own settings: every one it has, or those named.
    /// Answered by [`Effect::CameraSettings`], the last of them saying how
    /// the read ended.
    pub fn camera_settings(
        &mut self,
        vehicle_id: &str,
        component_id: u8,
        names: Option<Vec<String>>,
        now_us: u64,
    ) -> Vec<Effect> {
        let Some(key) = self.command_address(vehicle_id) else {
            return vec![Effect::CameraSettings {
                vehicle_id: vehicle_id.to_owned(),
                component_id,
                values: Vec::new(),
                read: Some(Err(NO_ADDRESS.into())),
            }];
        };
        let heard: Key = (key.0, key.1, key.2, component_id);
        let outs = match names {
            None => self.camera_params.list(&heard, now_us),
            Some(names) => self.camera_params.read(&heard, names, now_us),
        };
        outs.iter()
            .map(|out| self.camera_param_send(&heard, out))
            .collect()
    }

    /// Writes one of a camera's own settings (`PARAM_EXT_SET`), for a
    /// command that is one. The camera's `PARAM_EXT_ACK` ends it, from
    /// the component it was sent to alone.
    pub(super) fn write_camera_setting(&mut self, order: CommandOrder, now_us: u64) -> Vec<Effect> {
        let not_sent = |order: CommandOrder, reason: &str| {
            vec![Effect::CommandDone {
                receipt_id: order.receipt_id,
                outcome: CommandOutcome::NotSent(reason.into()),
                attempts: 0,
            }]
        };
        let Some(key) = self
            .command_address(&order.vehicle_id)
            .filter(|key| self.vehicles.contains_key(key))
        else {
            return not_sent(order, "this vehicle has no address in this session");
        };
        let key = &key;
        let uxduck_core::domain::command::Command::Camera {
            component_id,
            action,
            ..
        } = &order.command
        else {
            return not_sent(order, "this is not a camera's setting");
        };
        if let Some(why) = action.invalid() {
            return not_sent(order, why);
        }
        let Some((name, number, kind)) = action.setting() else {
            return not_sent(order, "this is not a camera's setting");
        };
        let Some(kind) = kind else {
            return not_sent(
                order,
                "the camera's definition file, which says this setting's type, is not read",
            );
        };
        let Some(value) = Value::of(kind, number) else {
            return not_sent(
                order,
                kind.refuses(number)
                    .unwrap_or("the value does not fit this setting"),
            );
        };
        let Some(autopilot) = self.vehicles.get(key).map(|v| v.autopilot_component) else {
            return not_sent(order, "this vehicle has no address in this session");
        };
        let heard: Key = (
            key.0.clone(),
            key.1.clone(),
            key.2,
            component_id.unwrap_or(autopilot),
        );
        let name = name.to_owned();
        match self
            .camera_params
            .set(&heard, order.receipt_id.clone(), &name, value, now_us)
        {
            Ok(outs) => outs
                .iter()
                .map(|out| self.camera_param_send(&heard, out))
                .collect(),
            Err(why) => not_sent(order, why),
        }
    }

    /// A camera gave one of its settings (`PARAM_EXT_VALUE`).
    pub(super) fn camera_setting_said(
        &mut self,
        key: &Address,
        component: u8,
        said: Said,
        now_us: u64,
    ) {
        let heard: Key = (key.0.clone(), key.1.clone(), key.2, component);
        self.camera_params.value(&heard, said, now_us);
    }

    /// A camera answered a setting written (`PARAM_EXT_ACK`).
    pub(super) fn camera_setting_acked(
        &mut self,
        key: &Address,
        component: u8,
        acked: Acked,
        now_us: u64,
    ) -> Vec<Effect> {
        let heard: Key = (key.0.clone(), key.1.clone(), key.2, component);
        let told = self.camera_params.ack(&heard, acked, now_us);
        self.camera_told(told)
    }

    /// An FTP message from a component a definition file is read from with
    /// a client of its own. None for any other component, whose FTP is the
    /// vehicle's.
    pub(super) fn camera_file_answer(
        &mut self,
        key: &Address,
        component: u8,
        target_system: u8,
        payload: &[u8; 251],
        (received_us, now_us): (u64, u64),
    ) -> Option<Vec<Effect>> {
        if !self.camera_files.any() {
            return None;
        }
        let heard: Key = (key.0.clone(), key.1.clone(), key.2, component);
        if !self.camera_files.own(&heard) {
            return None;
        }
        // Its answer to another ground station is not an answer to this one.
        if target_system != self.config.gcs_system_id {
            return Some(Vec::new());
        }
        let answer = crate::transfers::ftp::read(payload);
        let output = self
            .camera_files
            .answer(&heard, &answer, received_us, now_us);
        Some(self.camera_file_output(&heard, output, now_us))
    }

    /// What a definition file's own FTP client decided: requests for the
    /// component it is read from, and what they came to.
    fn camera_file_output(
        &mut self,
        heard: &Key,
        output: files::Output,
        now_us: u64,
    ) -> Vec<Effect> {
        let gcs = self.config.gcs_system_id;
        let mut effects = Vec::new();
        for request in output.send {
            let sequence = self.next_sequence();
            effects.push(Effect::Send {
                link_id: heard.0.clone(),
                peer: heard.1.clone(),
                bytes: crate::sim::ftp_frame(
                    (gcs, crate::GCS_COMPONENT_ID),
                    (heard.2, heard.3),
                    sequence,
                    crate::transfers::ftp::payload(&request),
                ),
            });
        }
        for event in output.events {
            effects.extend(self.camera_file_event(heard, event, now_us));
        }
        effects
    }

    /// One step of a definition file's read, by either client.
    pub(super) fn camera_file_event(
        &mut self,
        heard: &Key,
        event: files::Event,
        now_us: u64,
    ) -> Vec<Effect> {
        let Some(camera) = self.camera_files.camera(heard) else {
            return Vec::new();
        };
        match self.camera_files.event(heard, event) {
            Step::Going => Vec::new(),
            Step::Stop(why) => self.camera_file_stopped(heard, camera, why, now_us),
            Step::Done(result) => self.camera_definition_said(heard, camera, result),
        }
    }

    /// Stops a definition file's read and says why: the file open on the
    /// component is closed.
    fn camera_file_stopped(
        &mut self,
        heard: &Key,
        camera: u8,
        why: String,
        now_us: u64,
    ) -> Vec<Effect> {
        let own = self.camera_files.own(heard);
        let closing = self.camera_files.stop(heard, now_us);
        let mut effects = if own {
            let closing = files::Output {
                send: closing.send,
                events: Vec::new(),
            };
            self.camera_file_output(heard, closing, now_us)
        } else {
            let key: Address = (heard.0.clone(), heard.1.clone(), heard.2);
            let output = self
                .vehicles
                .get_mut(&key)
                .map(|vehicle| vehicle.ftp.cancel(READ_ID, now_us))
                .unwrap_or_default();
            self.ftp_output(&key, output, now_us)
        };
        effects.extend(self.camera_definition_said(heard, camera, Err(why)));
        effects
    }

    fn camera_definition_said(
        &self,
        heard: &Key,
        camera: u8,
        result: Result<Vec<u8>, String>,
    ) -> Vec<Effect> {
        let key: Address = (heard.0.clone(), heard.1.clone(), heard.2);
        let Some(vehicle) = self.vehicles.get(&key) else {
            return Vec::new();
        };
        vec![Effect::CameraDefinition {
            vehicle_id: vehicle.observation.id.clone(),
            component_id: camera,
            result,
        }]
    }

    /// Each tick: what is sent again, the files that took too long, and
    /// the settings and writes that went unanswered.
    pub(super) fn tick_camera_settings(&mut self, now_us: u64) -> Vec<Effect> {
        let timing = self.config.transfers;
        let mut effects = Vec::new();
        let (outputs, late) = self.camera_files.tick(now_us, &timing);
        for (heard, output) in outputs {
            effects.extend(self.camera_file_output(&heard, output, now_us));
        }
        for heard in late {
            if let Some(camera) = self.camera_files.camera(&heard) {
                effects.extend(self.camera_file_stopped(
                    &heard,
                    camera,
                    camera_file::too_slow(),
                    now_us,
                ));
            }
        }
        let (outs, told) = self.camera_params.tick(now_us);
        for (heard, out) in outs {
            effects.push(self.camera_param_send(&heard, &out));
        }
        effects.extend(self.camera_told(told));
        effects.extend(self.camera_settings_said());
        effects
    }

    /// The settings cameras gave and the reads that ended since this was
    /// last told (`Effect::CameraSettings`).
    pub(super) fn camera_settings_said(&mut self) -> Vec<Effect> {
        let told = self.camera_params.take();
        self.camera_told(told)
    }

    /// Forgets what is read from and written to the addresses that match.
    /// A write still waiting there ends unanswered, told at the next tick.
    pub(super) fn forget_camera_settings(&mut self, gone: impl Fn(&Key) -> bool) {
        self.camera_files.forget(&gone);
        let told = self.camera_params.forget(&gone);
        let ended = self.camera_told(told);
        self.deferred_effects.extend(ended);
    }

    fn camera_param_send(&mut self, heard: &Key, out: &Out) -> Effect {
        let sequence = self.next_sequence();
        Effect::Send {
            link_id: heard.0.clone(),
            peer: heard.1.clone(),
            bytes: crate::sim::param_ext_frame(
                (self.config.gcs_system_id, crate::GCS_COMPONENT_ID),
                (heard.2, heard.3),
                sequence,
                out,
            ),
        }
    }

    fn camera_told(&self, told: Vec<Told>) -> Vec<Effect> {
        told.into_iter()
            .filter_map(|told| {
                Some(match told {
                    Told::Values { key, values, read } => {
                        let vehicle = self.vehicles.get(&(key.0, key.1, key.2))?;
                        Effect::CameraSettings {
                            vehicle_id: vehicle.observation.id.clone(),
                            component_id: key.3,
                            values,
                            read,
                        }
                    }
                    Told::Done {
                        receipt_id,
                        outcome,
                        attempts,
                    } => Effect::CommandDone {
                        receipt_id,
                        outcome: match outcome {
                            Outcome::Accepted => CommandOutcome::Accepted,
                            Outcome::Refused(why) => CommandOutcome::Rejected(why),
                            Outcome::Timeout => CommandOutcome::Timeout,
                        },
                        attempts,
                    },
                    Told::Working { receipt_id } => Effect::CommandProgress {
                        receipt_id,
                        progress: None,
                    },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use mavio::dialects::ardupilotmega::Ardupilotmega;
    use uxduck_core::domain::camera::{CameraAction, CameraSettingKind};
    use uxduck_core::domain::command::Command;

    use super::*;
    use crate::payload::Payload;
    use crate::payload::camera::settings::{DEFINITION, NAMES};
    use crate::session::Config;
    use crate::sim::FakeVehicle;

    const PEER: &str = "127.0.0.1:5760";
    const SECOND: u64 = 1_000_000;

    /// The settings told, and how a read ended if one did.
    type Told = (Vec<(String, Value)>, Option<Result<(), String>>);

    /// A session and a fake vehicle that carries the simulated camera.
    struct Rig {
        session: Session,
        vehicle: FakeVehicle,
        now_us: u64,
        /// The component each file request and each request about settings
        /// was sent to, with the request.
        sent: Vec<(u8, Ardupilotmega)>,
        made: Vec<Effect>,
    }

    impl Rig {
        /// The camera identified, three seconds in.
        fn new() -> Self {
            let mut vehicle = FakeVehicle::new(1, 0x1234);
            vehicle.payload = Some(Payload::new(1).with_camera(None));
            let mut rig = Self {
                session: Session::new(Config::default()),
                vehicle,
                now_us: 0,
                sent: Vec::new(),
                made: Vec::new(),
            };
            rig.seconds(3);
            rig
        }

        fn id(&self) -> String {
            self.session
                .vehicle_at("l1", PEER, 1)
                .expect("the vehicle")
                .to_owned()
        }

        fn camera(&mut self) -> &mut crate::payload::Camera {
            let payload = self.vehicle.payload.as_mut().expect("a payload");
            payload.camera.as_mut().expect("a camera")
        }

        /// The camera loses its power: it says and answers nothing more.
        fn silence(&mut self) {
            self.vehicle.camera_silent_s = Some((0.0, f64::MAX));
            self.vehicle.payload.as_mut().expect("a payload").camera_off = true;
        }

        /// Hands the session's frames to the vehicle and its answers back,
        /// until neither says more.
        fn converse(&mut self, effects: Vec<Effect>) {
            let mut pending = effects;
            for _ in 0..100_000 {
                if pending.is_empty() {
                    break;
                }
                let mut next = Vec::new();
                for effect in pending {
                    let Effect::Send { bytes, .. } = effect else {
                        self.made.push(effect);
                        continue;
                    };
                    if let Some((_, message)) = crate::payload::wire::typed(&bytes) {
                        let to = match &message {
                            Ardupilotmega::FileTransferProtocol(m) => Some(m.target_component),
                            Ardupilotmega::ParamExtRequestList(m) => Some(m.target_component),
                            Ardupilotmega::ParamExtRequestRead(m) => Some(m.target_component),
                            Ardupilotmega::ParamExtSet(m) => Some(m.target_component),
                            _ => None,
                        };
                        if let Some(to) = to {
                            self.sent.push((to, message));
                        }
                    }
                    for reply in self.vehicle.answer(&bytes) {
                        next.extend(self.session.ingest("l1", PEER, &reply, self.now_us));
                    }
                }
                pending = next;
            }
        }

        /// A tenth of a second: the payload's clock moves, what it sends
        /// arrives, and time is told.
        fn step(&mut self) {
            self.now_us += SECOND / 10;
            #[allow(clippy::cast_precision_loss, reason = "a test's few seconds")]
            let t_s = self.now_us as f64 / 1e6;
            let mut frames = Vec::new();
            if self.now_us.is_multiple_of(SECOND) {
                frames.push(self.vehicle.heartbeat());
            }
            frames.extend(self.vehicle.payload_step(t_s));
            let mut effects: Vec<Effect> = frames
                .iter()
                .flat_map(|frame| self.session.ingest("l1", PEER, frame, self.now_us))
                .collect();
            effects.extend(self.session.tick(self.now_us));
            self.converse(effects);
        }

        fn seconds(&mut self, seconds: u64) {
            for _ in 0..seconds * 10 {
                self.step();
            }
        }

        /// Reads a definition file as the engine orders it.
        fn definition(&mut self, from: Option<u8>, path: &str) {
            let effects = self
                .session
                .camera_definition(&self.id(), 100, from, path, self.now_us);
            self.converse(effects);
        }

        /// What the read of a definition file came to, once.
        fn read(&mut self) -> Option<Result<Vec<u8>, String>> {
            let at = self
                .made
                .iter()
                .position(|effect| matches!(effect, Effect::CameraDefinition { .. }))?;
            match self.made.remove(at) {
                Effect::CameraDefinition {
                    vehicle_id,
                    component_id,
                    result,
                } => {
                    assert_eq!((vehicle_id, component_id), (self.id(), 100));
                    Some(result)
                }
                _ => None,
            }
        }

        /// Writes a setting as the engine orders it.
        fn write(
            &mut self,
            receipt: &str,
            name: &str,
            value: f64,
            kind: Option<CameraSettingKind>,
        ) {
            let order = CommandOrder {
                receipt_id: receipt.into(),
                vehicle_id: self.id(),
                command: Command::Camera {
                    component_id: Some(100),
                    device_id: None,
                    action: CameraAction::SetSetting {
                        name: name.into(),
                        value,
                        kind,
                    },
                },
                force: false,
            };
            let effects = self.session.command(order, self.now_us);
            self.converse(effects);
        }

        fn done(&self, receipt: &str) -> Option<(CommandOutcome, u8)> {
            self.made.iter().find_map(|effect| match effect {
                Effect::CommandDone {
                    receipt_id,
                    outcome,
                    attempts,
                } if receipt_id == receipt => Some((outcome.clone(), *attempts)),
                _ => None,
            })
        }

        /// The settings told since this was last asked, and how a read
        /// ended if one did.
        fn told(&mut self) -> Told {
            let mut all = Vec::new();
            let mut ended = None;
            self.made.retain(|effect| {
                let Effect::CameraSettings {
                    component_id,
                    values,
                    read,
                    ..
                } = effect
                else {
                    return true;
                };
                assert_eq!(*component_id, 100);
                all.extend(values.iter().cloned());
                ended = ended.take().or_else(|| read.clone());
                false
            });
            (all, ended)
        }

        /// The writes sent so far: the component, the name and the type.
        fn writes(&self) -> Vec<(u8, String, u8)> {
            self.sent
                .iter()
                .filter_map(|(to, message)| match message {
                    Ardupilotmega::ParamExtSet(m) => Some((
                        *to,
                        crate::decode::param_name(&m.param_id),
                        m.param_type as u8,
                    )),
                    _ => None,
                })
                .collect()
        }
    }

    // MAVLink FTP, "mftp:///camera.xml": "FTP resource 'camera.xml' from
    // current component". The camera answers for its own files, beside the
    // autopilot's.
    #[test]
    fn a_definition_file_is_read_from_the_camera_itself() {
        let mut rig = Rig::new();
        // The vehicle's own files are read meanwhile, by its own client.
        let id = rig.id();
        let listing = rig.session.file(
            &id,
            "list",
            files::Op::List {
                path: "/APM/LOGS".into(),
            },
            rig.now_us,
        );
        rig.definition(None, "/camera.xml");
        rig.converse(listing);
        assert_eq!(rig.read(), Some(Ok(DEFINITION.as_bytes().to_vec())));
        assert!(rig.read().is_none(), "told once");
        let (to_camera, to_autopilot): (Vec<_>, Vec<_>) = rig
            .sent
            .iter()
            .filter(|(_, message)| matches!(message, Ardupilotmega::FileTransferProtocol(_)))
            .partition(|(to, _)| *to == 100);
        assert!(!to_camera.is_empty() && !to_autopilot.is_empty());
        let listed = rig.made.iter().any(|effect| {
            matches!(
                effect,
                Effect::File {
                    event: files::Event::Listed { id, entries },
                    ..
                } if id == "list" && !entries.is_empty()
            )
        });
        assert!(listed, "the vehicle's own listing is whole");
        // Nothing of the camera's read is told as a file of the vehicle's.
        assert!(!rig.made.iter().any(|effect| matches!(
            effect,
            Effect::File { event, .. } if camera_file::ours(event)
        )));
        assert!(!rig.session.camera_files.any());
    }

    // "mftp://comp=1:@SYS/uarts.txt": a file its autopilot keeps is read by
    // the vehicle's one client, in its turn.
    #[test]
    fn a_definition_file_the_autopilot_keeps_is_read_through_the_vehicles_own_client() {
        let mut rig = Rig::new();
        rig.definition(Some(1), "@SYS/uarts.txt");
        let read = rig.read().expect("told").expect("read");
        assert!(read.starts_with(b"UARTV1"));
        assert!(rig.sent.iter().all(|(to, message)| !matches!(
            message,
            Ardupilotmega::FileTransferProtocol(_)
        ) || *to == 1));
        assert!(
            !rig.made
                .iter()
                .any(|effect| matches!(effect, Effect::File { .. }))
        );
        // One that is not there says so.
        rig.definition(Some(1), "/APM/none.xml");
        let why = rig.read().expect("told").expect_err("not there");
        assert!(
            why.ends_with('.') && why.chars().next().is_some_and(char::is_uppercase),
            "{why}"
        );
    }

    #[test]
    fn a_file_that_is_not_there_is_too_large_or_never_comes_says_why() {
        let mut rig = Rig::new();
        rig.definition(None, "/none.xml");
        let why = rig.read().expect("told").expect_err("not there");
        assert!(why.contains("no such file"), "{why}");

        // The camera's file is not trusted to be small.
        rig.camera()
            .serve_definition(&vec![b' '; camera_file::MAX_BYTES + 1]);
        rig.definition(None, "/camera.xml");
        let why = rig.read().expect("told").expect_err("too large");
        assert!(why.contains("larger than 1 MB"), "{why}");
        assert!(!rig.session.camera_files.any());
        // Nothing more is asked of the camera for it.
        let asked = rig.sent.len();
        rig.seconds(2);
        assert_eq!(rig.sent.len(), asked);

        // A read while one is going is refused, and a camera that stops
        // answering is given up on.
        rig.silence();
        rig.definition(None, "/camera.xml");
        assert!(rig.read().is_none());
        rig.definition(None, "/camera.xml");
        let why = rig.read().expect("told").expect_err("already reading");
        assert!(why.contains("Another definition file"), "{why}");
        rig.seconds(70);
        assert_eq!(
            rig.read(),
            Some(Err("The camera did not answer.".to_owned()))
        );
        assert!(!rig.session.needs_tick() || !rig.session.camera_files.any());
    }

    // Camera definition file, "Protocol Definition": "it will request all
    // parameters from the camera using the PARAM_EXT_REQUEST_LIST message".
    #[test]
    fn a_cameras_settings_are_read_and_the_read_is_told_when_all_are_in() {
        let mut rig = Rig::new();
        let id = rig.id();
        let effects = rig.session.camera_settings(&id, 100, None, rig.now_us);
        rig.converse(effects);
        rig.step();
        let (values, ended) = rig.told();
        assert_eq!(ended, Some(Ok(())));
        // Each as it came, in the order of the camera's list.
        let names: Vec<&str> = values.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, NAMES.to_vec());

        // Some of them, by name.
        let names = vec!["CAM_ISO".to_owned(), "CAM_SHUTTERSPD".to_owned()];
        let effects = rig
            .session
            .camera_settings(&id, 100, Some(names), rig.now_us);
        rig.converse(effects);
        rig.step();
        assert_eq!(
            rig.told(),
            (
                vec![
                    ("CAM_ISO".into(), Value::Uint32(400)),
                    ("CAM_SHUTTERSPD".into(), Value::Real32(0.002)),
                ],
                Some(Ok(()))
            )
        );

        // A camera that does not answer: asked three times, then said so.
        rig.silence();
        let effects = rig.session.camera_settings(&id, 100, None, rig.now_us);
        rig.converse(effects);
        rig.seconds(6);
        let (values, ended) = rig.told();
        assert!(values.is_empty());
        assert_eq!(ended, Some(Err("The camera did not answer.".to_owned())));
        let lists = rig
            .sent
            .iter()
            .filter(|(_, message)| matches!(message, Ardupilotmega::ParamExtRequestList(_)))
            .count();
        assert_eq!(lists, 1 + 3);
    }

    // Extended parameter protocol, "Write Parameters": PARAM_EXT_SET to the
    // camera's component, ended by its PARAM_EXT_ACK.
    #[test]
    fn a_setting_is_written_to_the_camera_and_its_answer_ends_the_command() {
        let mut rig = Rig::new();
        let uint32 = Some(CameraSettingKind::Uint32);
        rig.write("r1", "CAM_PHOTOFMT", 1.0, uint32);
        assert_eq!(rig.done("r1"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(rig.writes(), vec![(100, "CAM_PHOTOFMT".to_owned(), 5)]);
        rig.step();
        // The value the camera now has, from its answer.
        assert_eq!(
            rig.told(),
            (vec![("CAM_PHOTOFMT".into(), Value::Uint32(1))], None)
        );

        // What the camera refuses is told in its words, with the value it
        // keeps.
        rig.write("r2", "CAM_ISO", 800.0, uint32);
        assert_eq!(
            rig.done("r2"),
            Some((
                CommandOutcome::Rejected("The camera said: Failed.".into()),
                1
            ))
        );
        rig.step();
        assert_eq!(rig.told().0, vec![("CAM_ISO".into(), Value::Uint32(400))]);
        rig.write("r3", "CAM_PHOTOFMT", 9.0, uint32);
        let Some((CommandOutcome::Rejected(why), 1)) = rig.done("r3") else {
            panic!("refused");
        };
        assert!(
            why.ends_with("The camera said: Value unsupported."),
            "{why}"
        );

        // What cannot be a value of the setting's type, or has no type, is
        // not sent.
        let sent = rig.writes().len();
        rig.write("r4", "CAM_PHOTOFMT", 1.5, uint32);
        rig.write("r5", "CAM_PHOTOFMT", 1.0, None);
        rig.write("r6", "CAM_PHOTOFMT_NAME_TOO_LONG", 1.0, uint32);
        for receipt in ["r4", "r5", "r6"] {
            assert!(
                matches!(rig.done(receipt), Some((CommandOutcome::NotSent(_), 0))),
                "{receipt}"
            );
        }
        assert_eq!(rig.writes().len(), sent);
    }

    // "For long-running write operations drone may initially respond with
    // PARAM_ACK_IN_PROGRESS".
    #[test]
    fn a_setting_the_camera_is_working_on_is_told_and_ends_when_it_has_changed() {
        let mut rig = Rig::new();
        let uint32 = Some(CameraSettingKind::Uint32);
        rig.write("mode", "CAM_MODE", 1.0, uint32);
        rig.write("size", "CAM_VIDRES", 0.0, uint32);
        assert!(rig.done("size").is_none());
        assert!(rig.made.iter().any(|effect| matches!(
            effect,
            Effect::CommandProgress { receipt_id, progress: None } if receipt_id == "size"
        )));
        // The same setting again meanwhile is in the way.
        rig.write("again", "CAM_VIDRES", 2.0, uint32);
        assert!(matches!(
            rig.done("again"),
            Some((CommandOutcome::NotSent(_), 0))
        ));
        rig.seconds(2);
        assert_eq!(rig.done("size"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(
            rig.writes().iter().filter(|w| w.1 == "CAM_VIDRES").count(),
            1
        );
    }

    #[test]
    fn a_write_the_camera_never_answers_is_sent_three_times_and_times_out() {
        let mut rig = Rig::new();
        rig.silence();
        rig.write("r1", "CAM_PHOTOFMT", 1.0, Some(CameraSettingKind::Uint32));
        rig.seconds(6);
        assert_eq!(rig.done("r1"), Some((CommandOutcome::Timeout, 3)));
        assert_eq!(rig.writes().len(), 3);
        assert!(!rig.session.camera_params.waiting());
    }

    // The answer ends a write only from the component it was sent to.
    #[test]
    fn another_components_acknowledgement_is_not_the_cameras() {
        use mavio::dialects::ardupilotmega::enums::ParamAck;
        use mavio::dialects::ardupilotmega::messages::ParamExtAck;

        let mut rig = Rig::new();
        rig.silence();
        rig.write("r1", "CAM_PHOTOFMT", 1.0, Some(CameraSettingKind::Uint32));
        let value = Value::Uint32(1);
        let ack = ParamExtAck {
            param_id: crate::payload::wire::text("CAM_PHOTOFMT"),
            param_value: value.bytes(),
            param_type: crate::sim::ext_type(value.kind()),
            param_result: ParamAck::Accepted,
        };
        for component in [1, 154] {
            let frame = crate::sim::frame(1, component, 0, &ack);
            let effects = rig.session.ingest("l1", PEER, &frame, rig.now_us);
            rig.converse(effects);
        }
        assert!(rig.done("r1").is_none());
        let frame = crate::sim::frame(1, 100, 0, &ack);
        let effects = rig.session.ingest("l1", PEER, &frame, rig.now_us);
        rig.converse(effects);
        assert_eq!(rig.done("r1"), Some((CommandOutcome::Accepted, 1)));
    }

    // A link that is removed takes its cameras' reads and writes with it.
    #[test]
    fn a_removed_links_write_ends_unanswered() {
        let mut rig = Rig::new();
        rig.silence();
        rig.write("r1", "CAM_PHOTOFMT", 1.0, Some(CameraSettingKind::Uint32));
        rig.definition(None, "/camera.xml");
        rig.session.remove_link("l1");
        assert!(!rig.session.camera_files.any());
        assert!(!rig.session.camera_params.waiting());
        let effects = rig.session.tick(rig.now_us + SECOND);
        rig.converse(effects);
        assert_eq!(rig.done("r1"), Some((CommandOutcome::Timeout, 1)));
    }
}
