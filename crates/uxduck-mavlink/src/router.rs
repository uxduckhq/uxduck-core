//! The router (ADR-0004 routing rules): which links a frame goes out on, and
//! loop detection. Byte-preserving and link-aware; logic only, so the caller
//! does the sending.
//!
//! - Vehicle-facing links (listeners, serial, TCP) feed forward links, through
//!   each forward's vehicle filter.
//! - A read-write forward's frames go to the vehicles it may reach, unicast to
//!   each vehicle's own peer on its own link. A read-only forward sends
//!   nothing back.
//! - A frame never goes out on the link it came in on.
//! - Every frame sent is fingerprinted with its original link and peer.
//!   Repetition from that source is ordinary traffic (sequence numbers wrap).
//!   A copy returning through another source is a loop; repeated loops disable
//!   the forward involved.

use std::collections::{BTreeMap, VecDeque};

use crate::framing::Header;

/// How a link takes part in routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// Vehicles are heard on it: a listener, serial device or TCP client.
    Vehicles,
    /// Sends vehicle traffic to another program or daemon.
    Forward {
        /// VIDs whose traffic it carries; empty for every vehicle.
        vehicles: Vec<String>,
        /// The one link whose traffic it carries, for a simulated vehicle's
        /// forward; `None` for every link.
        from_link: Option<String>,
        /// Whether what comes back may reach the vehicles.
        writable: bool,
    },
}

/// Where a vehicle is reached, for traffic coming back from a forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VehicleRoute {
    pub vehicle_id: String,
    pub link_id: String,
    pub peer: String,
}

/// One frame to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Send {
    pub link_id: String,
    /// The peer on that link, or `None` for the link's own destination (a forward).
    pub peer: Option<String>,
    pub bytes: Vec<u8>,
}

/// What routing one frame decided.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Routed {
    /// It is a copy of a frame this router sent: drop it.
    pub looped: bool,
    pub sends: Vec<Send>,
    /// Forward links to disable, with the reason for people.
    pub disable: Vec<LoopDetected>,
    pub matched: Option<LoopMatch>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopMatch {
    pub sent_at_us: u64,
    pub to_link: String,
    pub from: Option<(String, String)>,
    pub forward_link_id: Option<String>,
    pub threshold: usize,
    pub window_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopDetected {
    pub forward_link_id: String,
    pub received_link_id: String,
    pub received_peer: String,
}

#[derive(Debug)]
struct SentFrame {
    at_us: u64,
    to_link: String,
    from: Option<(String, String)>,
    fingerprint: Fingerprint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Fingerprint {
    hash: u64,
    system_id: u8,
    sequence: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopConfig {
    /// How long a sent frame is remembered.
    pub window_us: u64,
    /// Loops within the window that disable a forward.
    pub threshold: usize,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            window_us: 2_000_000,
            threshold: 5,
        }
    }
}

#[derive(Debug, Default)]
pub struct Router {
    config: LoopConfig,
    links: BTreeMap<String, Role>,
    /// Sent frames: when, by which link, what.
    sent: VecDeque<SentFrame>,
    /// Loop sightings per sending link, and the link each came back on.
    loops: BTreeMap<String, VecDeque<(u64, String)>>,
}

/// Remembered sent frames at most (a 2 s window of heavy traffic).
const MAX_SENT: usize = 8192;

/// FNV-1a over the frame's bytes.
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// An exact-wire-byte fingerprint for diagnostic comparisons, not authentication.
#[must_use]
pub fn fingerprint(bytes: &[u8]) -> String {
    format!("fnv1a64:{:016x}", hash(bytes))
}

impl Router {
    #[must_use]
    pub fn new(config: LoopConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub fn set_link(&mut self, link_id: &str, role: Role) {
        self.links.insert(link_id.to_owned(), role);
    }

    pub fn remove_link(&mut self, link_id: &str) {
        self.links.remove(link_id);
        self.loops.remove(link_id);
        self.sent.retain(|sent| {
            sent.to_link != link_id && sent.from.as_ref().is_none_or(|(from, _)| from != link_id)
        });
    }

    #[must_use]
    pub fn role(&self, link_id: &str) -> Option<&Role> {
        self.links.get(link_id)
    }

    /// Routes one whole frame that arrived on `link_id`: [`Router::check_loop`],
    /// then, unless it looped, [`Router::forward`].
    pub fn route(
        &mut self,
        (link_id, peer): (&str, &str),
        frame: &[u8],
        header: Header,
        vehicle: Option<&str>,
        vehicles: &[VehicleRoute],
        now_us: u64,
    ) -> Routed {
        let checked = self.check_loop((link_id, peer), frame, header, now_us);
        if checked.looped {
            return checked;
        }
        self.forward((link_id, peer), frame, header, vehicle, vehicles, now_us)
    }

    /// Whether a frame that just arrived is a copy of one this router sent.
    /// Ignore repetitions from the original source: unchanged payloads can
    /// have the same sequence again after only 256 frames. Check before the
    /// session sees a returned copy, which would look like a second vehicle.
    pub fn check_loop(
        &mut self,
        (link_id, peer): (&str, &str),
        frame: &[u8],
        header: Header,
        now_us: u64,
    ) -> Routed {
        self.expire(now_us);
        let mut routed = Routed::default();
        let fingerprint = Fingerprint {
            hash: hash(frame),
            system_id: header.system_id,
            sequence: header.sequence,
        };
        // Several scoped forwards may hear the same ground station peer.
        // Its repeated heartbeat on each pipe is still original source traffic.
        let matches = |sent: &&SentFrame| {
            sent.fingerprint == fingerprint
                && !sent.from.as_ref().is_some_and(|(source, source_peer)| {
                    source_peer == peer
                        && (source == link_id
                            || (matches!(self.links.get(source), Some(Role::Forward { .. }))
                                && matches!(self.links.get(link_id), Some(Role::Forward { .. }))))
                })
        };
        // When several destinations received a copy, a return on one of
        // those forwards identifies the sender; do not blame the first one.
        let direct = if matches!(self.links.get(link_id), Some(Role::Forward { .. })) {
            self.sent
                .iter()
                .find(|sent| sent.to_link == link_id && matches(sent))
        } else {
            None
        };
        let Some(sent) = direct.or_else(|| self.sent.iter().find(matches)) else {
            return routed;
        };
        routed.looped = true;
        // A reply echo may return from a vehicle after a forward sent it
        // there. Disable the forward, never the vehicle connection.
        let sender = if matches!(self.links.get(&sent.to_link), Some(Role::Forward { .. })) {
            Some(sent.to_link.clone())
        } else {
            sent.from.as_ref().and_then(|(source, _)| {
                matches!(self.links.get(source), Some(Role::Forward { .. })).then(|| source.clone())
            })
        };
        routed.matched = Some(LoopMatch {
            sent_at_us: sent.at_us,
            to_link: sent.to_link.clone(),
            from: sent.from.clone(),
            forward_link_id: sender.clone(),
            threshold: self.config.threshold,
            window_us: self.config.window_us,
        });
        let Some(sender) = sender else {
            return routed;
        };
        let sightings = self.loops.entry(sender.clone()).or_default();
        sightings.push_back((now_us, link_id.to_owned()));
        if sightings.len() >= self.config.threshold {
            sightings.clear();
            routed.disable.push(LoopDetected {
                forward_link_id: sender,
                received_link_id: link_id.to_owned(),
                received_peer: peer.to_owned(),
            });
        }
        routed
    }

    /// Where a frame that is not a loop goes. `vehicle` is the VID the
    /// session knows its sender by; `vehicles` is where each vehicle is
    /// reached now.
    pub fn forward(
        &mut self,
        (link_id, peer): (&str, &str),
        frame: &[u8],
        header: Header,
        vehicle: Option<&str>,
        vehicles: &[VehicleRoute],
        now_us: u64,
    ) -> Routed {
        let mut routed = Routed::default();
        let fingerprint = Fingerprint {
            hash: hash(frame),
            system_id: header.system_id,
            sequence: header.sequence,
        };
        match self.links.get(link_id).cloned() {
            Some(Role::Vehicles) => {
                for (id, role) in &self.links {
                    if let Role::Forward {
                        vehicles: filter,
                        from_link,
                        ..
                    } = role
                        && id != link_id
                        && from_link.as_deref().is_none_or(|from| from == link_id)
                        && (filter.is_empty()
                            || vehicle.is_some_and(|v| filter.iter().any(|f| f == v)))
                    {
                        routed.sends.push(Send {
                            link_id: id.clone(),
                            peer: None,
                            bytes: frame.to_vec(),
                        });
                    }
                }
            }
            Some(Role::Forward {
                vehicles: filter,
                from_link,
                writable: true,
            }) => {
                for target in vehicles {
                    let allowed = (filter.is_empty() || filter.contains(&target.vehicle_id))
                        && from_link
                            .as_deref()
                            .is_none_or(|from| from == target.link_id);
                    let vehicle_link =
                        matches!(self.links.get(&target.link_id), Some(Role::Vehicles));
                    if allowed && vehicle_link && target.link_id != link_id {
                        routed.sends.push(Send {
                            link_id: target.link_id.clone(),
                            peer: Some(target.peer.clone()),
                            bytes: frame.to_vec(),
                        });
                    }
                }
            }
            Some(Role::Forward {
                writable: false, ..
            })
            | None => {}
        }
        routed.sends.dedup();
        for send in &routed.sends {
            self.sent.push_back(SentFrame {
                at_us: now_us,
                to_link: send.link_id.clone(),
                from: Some((link_id.to_owned(), peer.to_owned())),
                fingerprint,
            });
        }
        while self.sent.len() > MAX_SENT {
            self.sent.pop_front();
        }
        routed
    }

    /// Remembers a frame the daemon itself sent, such as a command, so a
    /// copy coming back is recognised.
    pub fn sent_by_daemon(&mut self, link_id: &str, frame: &[u8], header: Header, now_us: u64) {
        self.sent.push_back(SentFrame {
            at_us: now_us,
            to_link: link_id.to_owned(),
            from: None,
            fingerprint: Fingerprint {
                hash: hash(frame),
                system_id: header.system_id,
                sequence: header.sequence,
            },
        });
    }

    fn expire(&mut self, now_us: u64) {
        let window = self.config.window_us;
        while self
            .sent
            .front()
            .is_some_and(|sent| now_us.saturating_sub(sent.at_us) > window)
        {
            self.sent.pop_front();
        }
        for sightings in self.loops.values_mut() {
            while sightings
                .front()
                .is_some_and(|(at, _)| now_us.saturating_sub(*at) > window)
            {
                sightings.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::header;
    use crate::sim::{gcs_heartbeat_frame, heartbeat_frame};

    fn route(
        router: &mut Router,
        link: &str,
        frame: &[u8],
        vehicle: Option<&str>,
        t: u64,
    ) -> Routed {
        let vehicles = [
            VehicleRoute {
                vehicle_id: "a".into(),
                link_id: "radio".into(),
                peer: "10.0.0.1:14550".into(),
            },
            VehicleRoute {
                vehicle_id: "b".into(),
                link_id: "radio".into(),
                peer: "10.0.0.2:14550".into(),
            },
        ];
        router.route(
            (link, "vehicle-peer"),
            frame,
            header(frame).unwrap(),
            vehicle,
            &vehicles,
            t,
        )
    }

    fn router() -> Router {
        let mut router = Router::new(LoopConfig::default());
        router.set_link("radio", Role::Vehicles);
        router.set_link(
            "to-mp",
            Role::Forward {
                vehicles: vec!["a".into()],
                from_link: None,
                writable: true,
            },
        );
        router.set_link(
            "watch-only",
            Role::Forward {
                vehicles: vec![],
                from_link: None,
                writable: false,
            },
        );
        router
    }

    #[test]
    fn vehicle_traffic_goes_to_the_forwards_its_filter_allows() {
        let mut router = router();
        let from_a = route(
            &mut router,
            "radio",
            &heartbeat_frame(1, 1, 0),
            Some("a"),
            0,
        );
        let links: Vec<&str> = from_a.sends.iter().map(|s| s.link_id.as_str()).collect();
        assert_eq!(links, ["to-mp", "watch-only"]);
        let from_b = route(
            &mut router,
            "radio",
            &heartbeat_frame(2, 1, 0),
            Some("b"),
            1,
        );
        let links: Vec<&str> = from_b.sends.iter().map(|s| s.link_id.as_str()).collect();
        assert_eq!(links, ["watch-only"], "b is not in to-mp's filter");
    }

    #[test]
    fn a_forward_reaches_only_its_vehicles_by_their_peers_and_read_only_sends_nothing() {
        let mut router = router();
        let back = route(
            &mut router,
            "to-mp",
            &gcs_heartbeat_frame(255, 190, 0),
            None,
            0,
        );
        assert_eq!(
            back.sends,
            [Send {
                link_id: "radio".into(),
                peer: Some("10.0.0.1:14550".into()),
                bytes: gcs_heartbeat_frame(255, 190, 0),
            }]
        );
        let none = route(
            &mut router,
            "watch-only",
            &gcs_heartbeat_frame(255, 190, 1),
            None,
            1,
        );
        assert!(none.sends.is_empty());
    }

    #[test]
    fn a_forward_from_one_link_carries_only_that_links_traffic_both_ways() {
        let mut router = Router::new(LoopConfig::default());
        router.set_link("radio", Role::Vehicles);
        router.set_link("sim", Role::Vehicles);
        router.set_link(
            "sim-to-qgc",
            Role::Forward {
                vehicles: vec![],
                from_link: Some("sim".into()),
                writable: true,
            },
        );
        let vehicles = [
            VehicleRoute {
                vehicle_id: "a".into(),
                link_id: "radio".into(),
                peer: "10.0.0.1:14550".into(),
            },
            VehicleRoute {
                vehicle_id: "s".into(),
                link_id: "sim".into(),
                peer: "127.0.0.1:5760".into(),
            },
        ];
        let frame = heartbeat_frame(1, 1, 0);
        let out = router.route(
            ("radio", "vehicle-peer"),
            &frame,
            header(&frame).unwrap(),
            Some("a"),
            &vehicles,
            0,
        );
        assert!(out.sends.is_empty(), "the radio's vehicle is not forwarded");
        let frame = heartbeat_frame(2, 1, 0);
        let out = router.route(
            ("sim", "vehicle-peer"),
            &frame,
            header(&frame).unwrap(),
            Some("s"),
            &vehicles,
            1,
        );
        let links: Vec<&str> = out.sends.iter().map(|s| s.link_id.as_str()).collect();
        assert_eq!(links, ["sim-to-qgc"]);
        let back = gcs_heartbeat_frame(255, 190, 0);
        let out = router.route(
            ("sim-to-qgc", "vehicle-peer"),
            &back,
            header(&back).unwrap(),
            None,
            &vehicles,
            2,
        );
        assert_eq!(
            out.sends,
            [Send {
                link_id: "sim".into(),
                peer: Some("127.0.0.1:5760".into()),
                bytes: back,
            }],
            "what comes back reaches the simulated vehicle only"
        );
    }

    #[test]
    fn a_frame_is_never_echoed_to_its_own_link() {
        let mut router = Router::new(LoopConfig::default());
        router.set_link(
            "both",
            Role::Forward {
                vehicles: vec![],
                from_link: None,
                writable: true,
            },
        );
        router.set_link("radio", Role::Vehicles);
        let result = route(
            &mut router,
            "radio",
            &heartbeat_frame(1, 1, 0),
            Some("a"),
            0,
        );
        assert!(result.sends.iter().all(|s| s.link_id != "radio"));
    }

    #[test]
    fn frames_that_come_back_are_loops_and_a_few_disable_the_forward() {
        let mut router = router();
        let mut disabled = Vec::new();
        for sequence in 0..5u8 {
            let frame = heartbeat_frame(1, 1, sequence);
            let t = u64::from(sequence) * 1000;
            assert!(!route(&mut router, "radio", &frame, Some("a"), t).looped);
            // An external tool relays it back into the radio listener.
            let back = router.route(
                ("radio", "relay-peer"),
                &frame,
                header(&frame).unwrap(),
                Some("a"),
                &[],
                t + 500,
            );
            assert!(back.looped, "a copy of a sent frame is dropped");
            disabled.extend(back.disable);
        }
        // Both forwards sent every frame; the first loop sighting is charged
        // to the first sender found, to-mp, which reaches the threshold.
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].forward_link_id, "to-mp");
        assert_eq!(disabled[0].received_link_id, "radio");
        assert_eq!(disabled[0].received_peer, "relay-peer");
    }

    #[test]
    fn unchanged_source_packets_after_sequence_wrap_are_not_loops() {
        let mut router = router();
        // A static SIMSTATE has identical bytes after the 8-bit sequence
        // wraps. SITL at 9x real time can wrap in under 300 ms.
        let frame = heartbeat_frame(1, 1, 144);
        for cycle in 0..10 {
            let result = route(&mut router, "radio", &frame, Some("a"), cycle * 280_000);
            assert!(!result.looped, "repetition from the same peer is valid");
            assert_eq!(result.sends.len(), 2);
            assert!(result.disable.is_empty());
        }
    }

    #[test]
    fn an_echo_through_the_forward_is_still_a_loop() {
        let mut router = router();
        let mut disabled = Vec::new();
        for sequence in 0..5 {
            let frame = heartbeat_frame(1, 1, sequence);
            route(
                &mut router,
                "radio",
                &frame,
                Some("a"),
                u64::from(sequence) * 1000,
            );
            let returned = route(
                &mut router,
                "to-mp",
                &frame,
                None,
                u64::from(sequence) * 1000 + 500,
            );
            assert!(returned.looped);
            disabled.extend(returned.disable);
        }
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].forward_link_id, "to-mp");
        assert_eq!(disabled[0].received_link_id, "to-mp");
    }

    #[test]
    fn an_echo_of_a_ground_station_message_disables_its_forward_not_the_vehicle() {
        let mut router = router();
        let mut disabled = Vec::new();
        for sequence in 0..5 {
            let frame = gcs_heartbeat_frame(255, 190, sequence);
            route(
                &mut router,
                "to-mp",
                &frame,
                None,
                u64::from(sequence) * 1000,
            );
            let returned = route(
                &mut router,
                "radio",
                &frame,
                Some("a"),
                u64::from(sequence) * 1000 + 500,
            );
            assert!(returned.looped);
            disabled.extend(returned.disable);
        }
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].forward_link_id, "to-mp");
    }

    #[test]
    fn an_echo_on_one_of_several_destinations_disables_that_destination() {
        let mut router = router();
        let mut disabled = Vec::new();
        for sequence in 0..5 {
            let frame = heartbeat_frame(1, 1, sequence);
            route(
                &mut router,
                "radio",
                &frame,
                Some("a"),
                u64::from(sequence) * 1000,
            );
            let returned = route(
                &mut router,
                "watch-only",
                &frame,
                None,
                u64::from(sequence) * 1000 + 500,
            );
            disabled.extend(returned.disable);
        }
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].forward_link_id, "watch-only");
    }

    #[test]
    fn removing_and_reopening_a_link_clears_its_packet_history() {
        let mut router = router();
        let frame = heartbeat_frame(1, 1, 0);
        route(&mut router, "radio", &frame, Some("a"), 0);
        router.remove_link("radio");
        router.set_link("radio", Role::Vehicles);
        assert!(
            !router
                .check_loop(
                    ("radio", "another-peer"),
                    &frame,
                    header(&frame).unwrap(),
                    500
                )
                .looped
        );
    }

    #[test]
    fn old_frames_are_forgotten() {
        let mut router = router();
        let frame = heartbeat_frame(1, 1, 9);
        route(&mut router, "radio", &frame, Some("a"), 0);
        assert!(!route(&mut router, "radio", &frame, Some("a"), 3_000_000).looped);
    }
}
