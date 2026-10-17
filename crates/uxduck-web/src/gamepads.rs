//! Gamepads read by the page's Gamepad API, for UXDuck Web only: the page
//! posts every plugged-in pad's axes and buttons about 25 times a second
//! while it is visible, and these become the same input events the
//! installed daemon gets from gilrs. A pad stops counting when no page has
//! reported it for [`STALE_US`], so a hidden or frozen tab lets it go
//! rather than holding its last sticks.

use std::collections::BTreeMap;

use serde::Deserialize;
use uxduck_core::domain::controller::{GamepadAxis, GamepadButton};
use uxduck_engine::controller_input::InputEvent;

/// The notification a page sends with its pads.
pub const METHOD: &str = "browser/gamepads";

/// A pad no page has reported for this long is unplugged.
pub const STALE_US: u64 = 500_000;

/// One page's report.
#[derive(Debug, Default, Deserialize)]
pub struct Frame {
    #[serde(default)]
    pub pads: Vec<Pad>,
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
}

/// Every pad pages report.
#[derive(Debug, Default)]
pub struct Pads {
    known: BTreeMap<String, Known>,
}

impl Pads {
    /// A page's report: the events since the last one.
    pub fn frame(&mut self, page: u32, now_us: u64, frame: Frame) -> Vec<InputEvent> {
        let mut events = Vec::new();
        let reported: Vec<String> = frame.pads.iter().map(|pad| pad.id.clone()).collect();
        for pad in frame.pads {
            let Some(known) = self.known.get_mut(&pad.id) else {
                events.extend(connected(&pad));
                let mut seen = BTreeMap::new();
                seen.insert(page, now_us);
                self.known.insert(
                    pad.id,
                    Known {
                        axes: pad.axes,
                        buttons: pad.buttons,
                        standard: pad.standard,
                        seen,
                    },
                );
                continue;
            };
            known.seen.insert(page, now_us);
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
        events.extend(self.unseen());
        events
    }

    /// A page closed: what only it reported is unplugged.
    pub fn forget(&mut self, page: u32) -> Vec<InputEvent> {
        for known in self.known.values_mut() {
            known.seen.remove(&page);
        }
        self.unseen()
    }

    /// Pads no page has reported lately are unplugged.
    pub fn expire(&mut self, now_us: u64) -> Vec<InputEvent> {
        for known in self.known.values_mut() {
            known
                .seen
                .retain(|_, &mut at| now_us.saturating_sub(at) < STALE_US);
        }
        self.unseen()
    }

    /// Whether any page reports a pad, so sticks may be sent.
    #[must_use]
    pub fn any(&self) -> bool {
        !self.known.is_empty()
    }

    fn unseen(&mut self) -> Vec<InputEvent> {
        let gone: Vec<String> = self
            .known
            .iter()
            .filter(|(_, known)| known.seen.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        gone.into_iter()
            .map(|id| {
                self.known.remove(&id);
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
            },
        );
        let InputEvent::Present { axes, .. } = &events[1] else {
            panic!("{events:?}");
        };
        assert_eq!(axes, &[(0, None, 0.2), (1, None, -0.4)]);
    }

    #[test]
    fn a_pad_goes_when_no_page_reports_it() {
        let mut pads = Pads::default();
        pads.frame(1, 0, pad(vec![], vec![]));
        pads.frame(2, 0, pad(vec![], vec![]));
        assert!(pads.forget(1).is_empty(), "the other page still has it");
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
    }
}
