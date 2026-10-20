//! Gamepads read by the page's Gamepad API, for UXDuck Web only: the page
//! posts every plugged-in pad's axes and buttons about 25 times a second
//! while it is visible, and these become the same input events the
//! installed daemon gets from gilrs. A pad stops counting when no page has
//! reported it for [`STALE_US`], so a hidden or frozen tab stops its input
//! rather than holding its last sticks. The composition root retains an
//! existing activation while that page lives, resuming only from fresh input.

use std::collections::BTreeMap;

use serde::Deserialize;
use uxduck_contracts::dto::analytics::ControllerInterruptionReason;
use uxduck_core::domain::controller::{GamepadAxis, GamepadButton};
use uxduck_engine::controller_input::InputEvent;

/// The notification a page sends with its pads.
pub const METHOD: &str = "browser/gamepads";
/// Diagnostic notification for the page whose input was interrupted.
pub const INTERRUPTED: &str = "browser/controllerInputInterrupted";

/// Input older than this cannot drive a vehicle.
pub const STALE_US: u64 = 500_000;

/// One page's report.
#[derive(Debug, Default, Deserialize)]
pub struct Frame {
    #[serde(default)]
    pub pads: Vec<Pad>,
    /// Source sampling time, not worker delivery time. Older pages omit it.
    #[serde(default, rename = "readAtMs")]
    pub read_at_ms: Option<u64>,
    /// Why this page stopped reporting pads. Older pages omit it.
    #[serde(default)]
    pub reason: ControllerInterruptionReason,
}

/// One pad as the page reads it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pad {
    pub id: String,
    pub name: String,
    /// The browser knows its layout (`mapping == "standard"`).
    #[serde(default)]
    pub standard: bool,
    #[serde(default)]
    pub axes: Vec<f64>,
    #[serde(default)]
    pub buttons: Vec<bool>,
}

#[derive(Debug)]
struct Known {
    axes: Vec<f64>,
    buttons: Vec<bool>,
    standard: bool,
    /// The pages reporting it, and when each last did.
    seen: BTreeMap<u32, u64>,
    last_page: u32,
    last_report_us: u64,
}

#[derive(Debug)]
pub struct Interruption {
    pub id: String,
    pub page: u32,
    pub reason: ControllerInterruptionReason,
    pub gap_ms: u32,
}

#[must_use]
pub fn bounded_ms(us: u64) -> u32 {
    u32::try_from((us / 1000).min(60_000)).unwrap_or(60_000)
}

/// Every pad pages report.
#[derive(Debug, Default)]
pub struct Pads {
    known: BTreeMap<String, Known>,
    interruptions: Vec<Interruption>,
}

impl Pads {
    /// A page's report: the events since the last one.
    pub fn frame(&mut self, page: u32, now_us: u64, frame: Frame) -> Vec<InputEvent> {
        let read_at_us = frame
            .read_at_ms
            .map_or(now_us, |at| at.saturating_mul(1000).min(now_us));
        let mut events = self.expire(now_us);
        // A worker backlog must not replay old input or refresh its lease.
        if now_us.saturating_sub(read_at_us) >= STALE_US {
            return events;
        }
        let reported: Vec<String> = frame.pads.iter().map(|pad| pad.id.clone()).collect();
        for pad in frame.pads {
            let Some(known) = self.known.get_mut(&pad.id) else {
                events.extend(connected(&pad));
                let mut seen = BTreeMap::new();
                seen.insert(page, read_at_us);
                self.known.insert(
                    pad.id,
                    Known {
                        axes: pad.axes,
                        buttons: pad.buttons,
                        standard: pad.standard,
                        seen,
                        last_page: page,
                        last_report_us: read_at_us,
                    },
                );
                continue;
            };
            known.seen.insert(page, read_at_us);
            known.last_page = page;
            known.last_report_us = read_at_us;
            for (index, &value) in pad.axes.iter().enumerate() {
                if known
                    .axes
                    .get(index)
                    .is_none_or(|&was| (was - value).abs() > 1e-3)
                {
                    events.push(axis(&pad.id, known.standard, index, value));
                }
            }
            for (index, &pressed) in pad.buttons.iter().enumerate() {
                if known.buttons.get(index) != Some(&pressed) {
                    events.push(button(&pad.id, known.standard, index, pressed));
                }
            }
            known.axes = pad.axes;
            known.buttons = pad.buttons;
        }
        // What this page no longer reports, it let go of.
        for (id, known) in &mut self.known {
            if !reported.contains(id) {
                known.seen.remove(&page);
            }
        }
        events.extend(self.unseen(now_us, frame.reason));
        events
    }

    /// A page closed: what only it reported is unplugged.
    pub fn forget(&mut self, page: u32, now_us: u64) -> Vec<InputEvent> {
        for known in self.known.values_mut() {
            known.seen.remove(&page);
        }
        self.unseen(now_us, ControllerInterruptionReason::PageClosed)
    }

    /// Pads no page has reported lately are unplugged.
    pub fn expire(&mut self, now_us: u64) -> Vec<InputEvent> {
        for known in self.known.values_mut() {
            known
                .seen
                .retain(|_, &mut at| now_us.saturating_sub(at) < STALE_US);
        }
        self.unseen(now_us, ControllerInterruptionReason::ReportTimeout)
    }

    /// Drained before the engine pauses output or releases a closing page's claims.
    pub fn take_interruptions(&mut self) -> Vec<Interruption> {
        std::mem::take(&mut self.interruptions)
    }

    /// Whether any page reports a pad, so sticks may be sent.
    #[must_use]
    pub fn any(&self) -> bool {
        !self.known.is_empty()
    }

    fn unseen(&mut self, now_us: u64, reason: ControllerInterruptionReason) -> Vec<InputEvent> {
        let gone: Vec<String> = self
            .known
            .iter()
            .filter(|(_, known)| known.seen.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        gone.into_iter()
            .map(|id| {
                if let Some(known) = self.known.remove(&id) {
                    self.interruptions.push(Interruption {
                        id: id.clone(),
                        page: known.last_page,
                        reason,
                        gap_ms: bounded_ms(now_us.saturating_sub(known.last_report_us)),
                    });
                }
                InputEvent::Disconnected { id }
            })
            .collect()
    }
}

/// A new pad: its name, then every input it has, so each shows and drives
/// before it moves.
fn connected(pad: &Pad) -> [InputEvent; 2] {
    [
        InputEvent::Connected {
            id: pad.id.clone(),
            name: pad.name.clone(),
            layout_known: pad.standard,
        },
        InputEvent::Present {
            id: pad.id.clone(),
            axes: pad
                .axes
                .iter()
                .enumerate()
                .map(|(index, &value)| {
                    let (axis, value) = standard_axis(pad.standard, index, value);
                    (code(index), axis, value)
                })
                .collect(),
            buttons: pad
                .buttons
                .iter()
                .enumerate()
                .map(|(index, &pressed)| {
                    (code(index), standard_button(pad.standard, index), pressed)
                })
                .collect(),
        },
    ]
}

#[allow(clippy::cast_possible_truncation, reason = "a pad has few inputs")]
fn code(index: usize) -> u32 {
    index as u32
}

/// An axis by its index. On the standard layout the first four are the
/// sticks, and the browser's Y is down-positive where gilrs's (and so the
/// mappings') is up-positive; any other device's values are as it sent them.
fn axis(id: &str, standard: bool, index: usize, value: f64) -> InputEvent {
    let (axis, value) = standard_axis(standard, index, value);
    InputEvent::Axis {
        id: id.to_owned(),
        axis,
        code: Some(code(index)),
        value,
    }
}

fn standard_axis(standard: bool, index: usize, value: f64) -> (Option<GamepadAxis>, f64) {
    match (standard, index) {
        (true, 0) => (Some(GamepadAxis::LeftStickX), value),
        (true, 1) => (Some(GamepadAxis::LeftStickY), -value),
        (true, 2) => (Some(GamepadAxis::RightStickX), value),
        (true, 3) => (Some(GamepadAxis::RightStickY), -value),
        _ => (None, value),
    }
}

fn button(id: &str, standard: bool, index: usize, pressed: bool) -> InputEvent {
    InputEvent::Button {
        id: id.to_owned(),
        button: standard_button(standard, index),
        code: Some(code(index)),
        pressed,
    }
}

/// The standard layout's buttons, in the Gamepad API's order.
fn standard_button(standard: bool, index: usize) -> Option<GamepadButton> {
    use GamepadButton::{
        DpadDown, DpadLeft, DpadRight, DpadUp, East, LeftThumb, LeftTrigger, LeftTrigger2, Mode,
        North, RightThumb, RightTrigger, RightTrigger2, Select, South, Start, West,
    };
    if !standard {
        return None;
    }
    [
        South,
        East,
        West,
        North,
        LeftTrigger,
        RightTrigger,
        LeftTrigger2,
        RightTrigger2,
        Select,
        Start,
        LeftThumb,
        RightThumb,
        DpadUp,
        DpadDown,
        DpadLeft,
        DpadRight,
        Mode,
    ]
    .get(index)
    .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(axes: Vec<f64>, buttons: Vec<bool>) -> Frame {
        Frame {
            pads: vec![Pad {
                id: "web-045e-0b13".into(),
                name: "Xbox Wireless Controller".into(),
                standard: true,
                axes,
                buttons,
            }],
            ..Frame::default()
        }
    }

    #[test]
    fn a_new_pad_is_connected_with_every_input_and_up_is_positive() {
        let mut pads = Pads::default();
        let events = pads.frame(1, 0, pad(vec![0.0, -0.5, 0.0, 0.0], vec![false, true]));
        assert!(matches!(
            &events[0],
            InputEvent::Connected { layout_known: true, name, .. } if name == "Xbox Wireless Controller"
        ));
        let InputEvent::Present { axes, buttons, .. } = &events[1] else {
            panic!("{events:?}");
        };
        assert_eq!(axes[1], (1, Some(GamepadAxis::LeftStickY), 0.5));
        assert_eq!(buttons[1], (1, Some(GamepadButton::East), true));
    }

    #[test]
    fn only_changes_are_events() {
        let mut pads = Pads::default();
        pads.frame(1, 0, pad(vec![0.0, 0.0], vec![false]));
        assert!(
            pads.frame(1, 40_000, pad(vec![0.0, 0.0], vec![false]))
                .is_empty()
        );
        let events = pads.frame(1, 80_000, pad(vec![0.3, 0.0], vec![true]));
        assert_eq!(
            events,
            [
                InputEvent::Axis {
                    id: "web-045e-0b13".into(),
                    axis: Some(GamepadAxis::LeftStickX),
                    code: Some(0),
                    value: 0.3,
                },
                InputEvent::Button {
                    id: "web-045e-0b13".into(),
                    button: Some(GamepadButton::South),
                    code: Some(0),
                    pressed: true,
                },
            ]
        );
    }

    #[test]
    fn a_radio_is_as_it_sent_it_by_index() {
        let mut pads = Pads::default();
        let events = pads.frame(
            1,
            0,
            Frame {
                pads: vec![Pad {
                    id: "web-1209-4f54".into(),
                    name: "TBS Tango 2".into(),
                    standard: false,
                    axes: vec![0.2, -0.4],
                    buttons: vec![],
                }],
                ..Frame::default()
            },
        );
        let InputEvent::Present { axes, .. } = &events[1] else {
            panic!("{events:?}");
        };
        assert_eq!(axes, &[(0, None, 0.2), (1, None, -0.4)]);
    }

    #[test]
    fn old_queued_samples_cannot_connect_or_extend_the_input_deadline() {
        let mut pads = Pads::default();
        let mut frame = pad(vec![0.0; 4], vec![false]);
        frame.read_at_ms = Some(0);
        assert!(pads.frame(1, STALE_US, frame).is_empty());
        assert!(!pads.any(), "an already stale snapshot never connects");
        let mut frame = pad(vec![0.0; 4], vec![false]);
        frame.read_at_ms = Some(1_000);
        pads.frame(1, 1_100_000, frame);
        assert!(pads.expire(1_499_999).is_empty());
        assert_eq!(
            pads.expire(1_500_000).len(),
            1,
            "cutoff starts at sampling, not delivery"
        );
    }

    #[test]
    fn a_pad_goes_when_no_page_reports_it() {
        let mut pads = Pads::default();
        pads.frame(1, 0, pad(vec![], vec![]));
        pads.frame(2, 0, pad(vec![], vec![]));
        assert!(
            pads.forget(1, 40_000).is_empty(),
            "the other page still has it"
        );
        let gone = pads.frame(2, 10_000, Frame::default());
        assert_eq!(
            gone,
            [InputEvent::Disconnected {
                id: "web-045e-0b13".into()
            }]
        );
        assert!(!pads.any());
    }

    #[test]
    fn a_silent_page_lets_its_pads_go() {
        let mut pads = Pads::default();
        pads.frame(1, 0, pad(vec![], vec![]));
        assert!(pads.expire(STALE_US - 1).is_empty());
        assert_eq!(pads.expire(STALE_US).len(), 1);
        let interruptions = pads.take_interruptions();
        assert_eq!(interruptions.len(), 1);
        assert_eq!(
            interruptions[0].reason,
            ControllerInterruptionReason::ReportTimeout
        );
        assert_eq!(interruptions[0].gap_ms, 500);
        assert!(pads.take_interruptions().is_empty());
    }

    #[test]
    fn explicit_losses_keep_their_reason_and_do_not_interrupt_another_reporting_page() {
        for reason in [
            ControllerInterruptionReason::PageHidden,
            ControllerInterruptionReason::ReadError,
            ControllerInterruptionReason::DeviceMissing,
            ControllerInterruptionReason::ReaderStopped,
        ] {
            let mut pads = Pads::default();
            pads.frame(1, 0, pad(vec![], vec![]));
            pads.frame(2, 40_000, pad(vec![], vec![]));
            assert!(
                pads.frame(
                    1,
                    80_000,
                    Frame {
                        reason,
                        ..Frame::default()
                    }
                )
                .is_empty()
            );
            assert!(pads.take_interruptions().is_empty());
            assert_eq!(
                pads.frame(
                    2,
                    120_000,
                    Frame {
                        reason,
                        ..Frame::default()
                    }
                )
                .len(),
                1
            );
            let interruptions = pads.take_interruptions();
            assert_eq!(interruptions.len(), 1);
            assert_eq!(interruptions[0].reason, reason);
            assert_eq!(interruptions[0].page, 2);
            assert_eq!(interruptions[0].gap_ms, 80);
        }
    }
}
