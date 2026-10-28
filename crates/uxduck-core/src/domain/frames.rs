//! Where a vehicle's motors are, for the motor test (ADR-0037 V9, S188):
//! each motor's place on the frame, its spin direction and its letter in
//! the order `ArduPilot` tests them, so the person checks order and
//! direction against a drawing.
//!
//! The multicopter tables are read from the motor diagrams in `ArduPilot`'s
//! documentation ("Connect ESCs and Motors", CC BY-SA 3.0,
//! <https://ardupilot.org/copter/docs/connect-escs-and-motors.html>) by
//! `tools/frames/from_diagrams.py`, which keeps a copy of each diagram
//! beside it; nothing comes from `ArduPilot`'s GPL source (the 2026-10-01
//! decision on GPL material, ADR-0021). Each motor is its output number
//! (`MOT_1` is index 0), its place with the nose up, its spin seen from
//! above (none for a motor that yaws without torque) and its testing order
//! (A is 1). The test goes clockwise from the front, or from the first
//! motor right of the front on an X frame; on coaxial frames the top motor
//! of each pair goes before the bottom. A rover tests its throttle and
//! steering, or its left and right throttles when it steers by skidding
//! (the Rover documentation's motor test). Frames with no diagram
//! (tricopter, helicopters, single, coaxial, bicopter, tailsitters, scripted
//! matrices, co-rotating octa quads) are named but not drawn.

use serde::{Deserialize, Serialize};

/// Which way a propeller turns, seen from above.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Spin {
    Clockwise,
    CounterClockwise,
}

/// One motor or output the motor test drives, where it is drawn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MotorPlace {
    /// Its place in the test's order, from 1: what `MAV_CMD_DO_MOTOR_TEST`
    /// is sent.
    pub order: u8,
    /// "A", "B"…; a rover's "Throttle", "Steering", "Left", "Right".
    pub label: String,
    /// The motor's own number, from 1 (`MOT_1`, an output set to Motor 1);
    /// a rover's test instance.
    pub motor: u8,
    /// Its place, the nose up: right of the middle and ahead of it, the
    /// farthest motor 1 from the middle.
    pub right: f64,
    pub forward: f64,
    /// None for a rover's outputs, and frames that yaw without torque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spin: Option<Spin>,
    /// How many motors before it, in the test's order, share its place:
    /// the other of a coaxial pair is 1.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stack: u8,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
const fn is_zero(value: &u8) -> bool {
    *value == 0
}

/// A vehicle's motors as the motor test draws them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MotorLayout {
    /// "Quad X", "Hexa +", "Y6B", "Rover", "Skid-steer rover", or "Frame
    /// class 7, type 1" for one not drawn.
    pub frame: String,
    /// In the test's order; empty for a frame not drawn.
    pub motors: Vec<MotorPlace>,
}

/// One motor of a table: its place (right of the middle, ahead of it, the
/// farthest motor 1 from the middle), its spin and its testing order.
#[derive(Clone, Copy, Debug)]
struct Def(f64, f64, Option<Spin>, u8);

const fn d(right: f64, forward: f64, spin: Option<Spin>, order: u8) -> Def {
    Def(right, forward, spin, order)
}

/// `FRAME_CLASS`, the `FRAME_TYPE`s a table is for (none: any not listed
/// for that class), the frame's name, and its motors in output order.
type Table = (u8, &'static [u8], &'static str, &'static [Def]);

// Written by tools/frames/from_diagrams.py from the diagrams in
// tools/frames/diagrams/: run it again when ArduPilot's documentation
// changes. Do not edit by hand.
const TABLES: &[Table] = &[
    // Quad +: m_01_00_quad_plus.svg
    (
        1,
        &[0],
        "Quad +",
        &[
            d(1.000, 0.000, Some(Spin::CounterClockwise), 2),
            d(-1.000, 0.000, Some(Spin::CounterClockwise), 4),
            d(0.000, 1.000, Some(Spin::Clockwise), 1),
            d(0.000, -1.000, Some(Spin::Clockwise), 3),
        ],
    ),
    // Quad X: m_01_01_quad_x.svg
    (
        1,
        &[1],
        "Quad X",
        &[
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 3),
            d(-0.707, 0.707, Some(Spin::Clockwise), 4),
            d(0.707, -0.707, Some(Spin::Clockwise), 2),
        ],
    ),
    // Quad V: m_01_02_quad_v.svg
    (
        1,
        &[2],
        "Quad V",
        &[
            d(0.740, 0.673, Some(Spin::CounterClockwise), 1),
            d(-0.471, -0.673, Some(Spin::CounterClockwise), 3),
            d(-0.740, 0.673, Some(Spin::Clockwise), 4),
            d(0.471, -0.673, Some(Spin::Clockwise), 2),
        ],
    ),
    // Quad H: m_01_03_quad_h.svg
    (
        1,
        &[3],
        "Quad H",
        &[
            d(0.707, 0.707, Some(Spin::Clockwise), 1),
            d(-0.707, -0.707, Some(Spin::Clockwise), 3),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 4),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 2),
        ],
    ),
    // Quad V-tail: m_01_04_quad_v_tail.svg
    (
        1,
        &[4],
        "Quad V-tail",
        &[
            d(0.707, 0.376, None, 1),
            d(-0.707, -0.707, Some(Spin::Clockwise), 3),
            d(-0.707, 0.376, None, 4),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 2),
        ],
    ),
    // Quad A-tail: m_01_05_quad_a_tail.svg
    (
        1,
        &[5],
        "Quad A-tail",
        &[
            d(0.857, 0.456, None, 1),
            d(-0.514, -0.857, Some(Spin::CounterClockwise), 3),
            d(-0.857, 0.456, None, 4),
            d(0.514, -0.857, Some(Spin::Clockwise), 2),
        ],
    ),
    // Quad + reversed: m_01_06_quad_plus_rev.svg
    (
        1,
        &[6],
        "Quad + reversed",
        &[
            d(1.000, 0.000, Some(Spin::Clockwise), 2),
            d(-1.000, 0.000, Some(Spin::Clockwise), 4),
            d(0.000, 1.000, Some(Spin::CounterClockwise), 1),
            d(0.000, -1.000, Some(Spin::CounterClockwise), 3),
        ],
    ),
    // Quad X, Betaflight order: m_01_12_quad_x_bf.svg
    (
        1,
        &[12],
        "Quad X, Betaflight order",
        &[
            d(0.707, -0.707, Some(Spin::Clockwise), 2),
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 3),
            d(-0.707, 0.707, Some(Spin::Clockwise), 4),
        ],
    ),
    // Quad X, DJI order: m_01_13_quad_x_dji.svg
    (
        1,
        &[13],
        "Quad X, DJI order",
        &[
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(-0.707, 0.707, Some(Spin::Clockwise), 4),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 3),
            d(0.707, -0.707, Some(Spin::Clockwise), 2),
        ],
    ),
    // Quad X, clockwise order: m_01_14_quad_x_cw.svg
    (
        1,
        &[14],
        "Quad X, clockwise order",
        &[
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(0.707, -0.707, Some(Spin::Clockwise), 2),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 3),
            d(-0.707, 0.707, Some(Spin::Clockwise), 4),
        ],
    ),
    // Quad +, no yaw torque: m_01_16_quad_plus_nyt.svg
    (
        1,
        &[16],
        "Quad +, no yaw torque",
        &[
            d(1.000, 0.000, None, 2),
            d(-1.000, 0.000, None, 4),
            d(0.000, 1.000, None, 1),
            d(0.000, -1.000, None, 3),
        ],
    ),
    // Quad X, no yaw torque: m_01_17_quad_x_nyt.svg
    (
        1,
        &[17],
        "Quad X, no yaw torque",
        &[
            d(0.707, 0.707, None, 1),
            d(-0.707, -0.707, None, 3),
            d(-0.707, 0.707, None, 4),
            d(0.707, -0.707, None, 2),
        ],
    ),
    // Quad X reversed, Betaflight order: m_01_18_quad_x_bf_rev.svg
    (
        1,
        &[18],
        "Quad X reversed, Betaflight order",
        &[
            d(0.707, -0.707, Some(Spin::CounterClockwise), 2),
            d(0.707, 0.707, Some(Spin::Clockwise), 1),
            d(-0.707, -0.707, Some(Spin::Clockwise), 3),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 4),
        ],
    ),
    // Quad Y4: m_01_19_quad_y4a.svg
    (
        1,
        &[19],
        "Quad Y4",
        &[
            d(0.682, 0.403, Some(Spin::CounterClockwise), 1),
            d(0.000, -1.000, Some(Spin::Clockwise), 2),
            d(0.000, -1.000, Some(Spin::CounterClockwise), 3),
            d(-0.682, 0.403, Some(Spin::Clockwise), 4),
        ],
    ),
    // Hexa +: m_02_00_hexa_plus.svg
    (
        2,
        &[0],
        "Hexa +",
        &[
            d(0.000, 0.894, Some(Spin::Clockwise), 1),
            d(0.000, -0.894, Some(Spin::CounterClockwise), 4),
            d(-0.894, -0.447, Some(Spin::Clockwise), 5),
            d(0.894, 0.447, Some(Spin::CounterClockwise), 2),
            d(-0.894, 0.447, Some(Spin::CounterClockwise), 6),
            d(0.894, -0.447, Some(Spin::Clockwise), 3),
        ],
    ),
    // Hexa X: m_02_01_hexa_x.svg
    (
        2,
        &[1],
        "Hexa X",
        &[
            d(0.894, 0.000, Some(Spin::Clockwise), 2),
            d(-0.894, 0.000, Some(Spin::CounterClockwise), 5),
            d(-0.447, 0.894, Some(Spin::Clockwise), 6),
            d(0.447, -0.894, Some(Spin::CounterClockwise), 3),
            d(0.447, 0.894, Some(Spin::CounterClockwise), 1),
            d(-0.447, -0.894, Some(Spin::Clockwise), 4),
        ],
    ),
    // Hexa H: m_02_03_hexa_h.svg
    (
        2,
        &[3],
        "Hexa H",
        &[
            d(0.707, 0.000, Some(Spin::Clockwise), 2),
            d(-0.707, 0.000, Some(Spin::CounterClockwise), 5),
            d(-0.707, 0.707, Some(Spin::Clockwise), 6),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 3),
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(-0.707, -0.707, Some(Spin::Clockwise), 4),
        ],
    ),
    // Hexa X, DJI order: m_02_13_hexa_x_dji.svg
    (
        2,
        &[13],
        "Hexa X, DJI order",
        &[
            d(0.447, 0.894, Some(Spin::CounterClockwise), 1),
            d(-0.447, 0.894, Some(Spin::Clockwise), 6),
            d(-0.894, 0.000, Some(Spin::CounterClockwise), 5),
            d(-0.447, -0.894, Some(Spin::Clockwise), 4),
            d(0.447, -0.894, Some(Spin::CounterClockwise), 3),
            d(0.894, 0.000, Some(Spin::Clockwise), 2),
        ],
    ),
    // Hexa X, clockwise order: m_02_14_hexa_x_cw.svg
    (
        2,
        &[14],
        "Hexa X, clockwise order",
        &[
            d(0.447, 0.894, Some(Spin::CounterClockwise), 1),
            d(0.894, 0.000, Some(Spin::Clockwise), 2),
            d(0.447, -0.894, Some(Spin::CounterClockwise), 3),
            d(-0.447, -0.894, Some(Spin::Clockwise), 4),
            d(-0.894, 0.000, Some(Spin::CounterClockwise), 5),
            d(-0.447, 0.894, Some(Spin::Clockwise), 6),
        ],
    ),
    // Octa +: m_03_00_octo_plus.svg
    (
        3,
        &[0],
        "Octa +",
        &[
            d(0.000, 1.000, Some(Spin::Clockwise), 1),
            d(0.000, -1.000, Some(Spin::Clockwise), 5),
            d(0.707, 0.707, Some(Spin::CounterClockwise), 2),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 4),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 8),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 6),
            d(-1.000, 0.000, Some(Spin::Clockwise), 7),
            d(1.000, 0.000, Some(Spin::Clockwise), 3),
        ],
    ),
    // Octa X: m_03_01_octo_x.svg
    (
        3,
        &[1],
        "Octa X",
        &[
            d(0.383, 0.924, Some(Spin::Clockwise), 1),
            d(-0.383, -0.924, Some(Spin::Clockwise), 5),
            d(0.924, 0.383, Some(Spin::CounterClockwise), 2),
            d(0.383, -0.924, Some(Spin::CounterClockwise), 4),
            d(-0.383, 0.924, Some(Spin::CounterClockwise), 8),
            d(-0.924, -0.383, Some(Spin::CounterClockwise), 6),
            d(-0.924, 0.383, Some(Spin::Clockwise), 7),
            d(0.924, -0.383, Some(Spin::Clockwise), 3),
        ],
    ),
    // Octa V: m_03_02_octo_v.svg
    (
        3,
        &[2],
        "Octa V",
        &[
            d(-0.587, 0.240, Some(Spin::Clockwise), 7),
            d(0.474, -0.226, Some(Spin::Clockwise), 3),
            d(-0.474, -0.226, Some(Spin::CounterClockwise), 6),
            d(0.354, -0.707, Some(Spin::CounterClockwise), 4),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 8),
            d(0.587, 0.240, Some(Spin::CounterClockwise), 2),
            d(0.707, 0.707, Some(Spin::Clockwise), 1),
            d(-0.354, -0.707, Some(Spin::Clockwise), 5),
        ],
    ),
    // Octa H: m_03_03_octo_h.svg
    (
        3,
        &[3],
        "Octa H",
        &[
            d(0.707, 0.707, Some(Spin::Clockwise), 1),
            d(-0.707, -0.707, Some(Spin::Clockwise), 5),
            d(0.707, 0.235, Some(Spin::CounterClockwise), 2),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 4),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 8),
            d(-0.707, -0.235, Some(Spin::CounterClockwise), 6),
            d(-0.707, 0.235, Some(Spin::Clockwise), 7),
            d(0.707, -0.235, Some(Spin::Clockwise), 3),
        ],
    ),
    // Octa X, DJI order: m_03_13_octo_x_dji.svg
    (
        3,
        &[13],
        "Octa X, DJI order",
        &[
            d(0.383, 0.924, Some(Spin::CounterClockwise), 1),
            d(-0.383, 0.924, Some(Spin::Clockwise), 8),
            d(-0.924, 0.383, Some(Spin::CounterClockwise), 7),
            d(-0.924, -0.383, Some(Spin::Clockwise), 6),
            d(-0.383, -0.924, Some(Spin::CounterClockwise), 5),
            d(0.383, -0.924, Some(Spin::Clockwise), 4),
            d(0.924, -0.383, Some(Spin::CounterClockwise), 3),
            d(0.924, 0.383, Some(Spin::Clockwise), 2),
        ],
    ),
    // Octa X, clockwise order: m_03_14_octo_x_cw.svg
    (
        3,
        &[14],
        "Octa X, clockwise order",
        &[
            d(0.383, 0.924, Some(Spin::CounterClockwise), 1),
            d(0.924, 0.383, Some(Spin::Clockwise), 2),
            d(0.924, -0.383, Some(Spin::CounterClockwise), 3),
            d(0.383, -0.924, Some(Spin::Clockwise), 4),
            d(-0.383, -0.924, Some(Spin::CounterClockwise), 5),
            d(-0.924, -0.383, Some(Spin::Clockwise), 6),
            d(-0.924, 0.383, Some(Spin::CounterClockwise), 7),
            d(-0.383, 0.924, Some(Spin::Clockwise), 8),
        ],
    ),
    // Octa I: m_03_15_octo_i.svg
    (
        3,
        &[15],
        "Octa I",
        &[
            d(-0.235, -0.707, Some(Spin::Clockwise), 5),
            d(0.235, 0.707, Some(Spin::Clockwise), 1),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 6),
            d(-0.235, 0.707, Some(Spin::CounterClockwise), 8),
            d(0.235, -0.707, Some(Spin::CounterClockwise), 4),
            d(0.707, 0.707, Some(Spin::CounterClockwise), 2),
            d(0.707, -0.707, Some(Spin::Clockwise), 3),
            d(-0.707, 0.707, Some(Spin::Clockwise), 7),
        ],
    ),
    // Octa quad +: m_04_00_octo_quad_plus.svg
    (
        4,
        &[0],
        "Octa quad +",
        &[
            d(0.000, 1.000, Some(Spin::CounterClockwise), 1),
            d(-0.846, 0.000, Some(Spin::Clockwise), 7),
            d(0.000, -1.000, Some(Spin::CounterClockwise), 5),
            d(0.846, 0.000, Some(Spin::Clockwise), 3),
            d(-0.846, 0.000, Some(Spin::CounterClockwise), 8),
            d(0.000, 1.000, Some(Spin::Clockwise), 2),
            d(0.846, 0.000, Some(Spin::CounterClockwise), 4),
            d(0.000, -1.000, Some(Spin::Clockwise), 6),
        ],
    ),
    // Octa quad X: m_04_01_octo_quad_x.svg
    (
        4,
        &[1],
        "Octa quad X",
        &[
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(-0.707, 0.707, Some(Spin::Clockwise), 7),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 5),
            d(0.707, -0.707, Some(Spin::Clockwise), 3),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 8),
            d(0.707, 0.707, Some(Spin::Clockwise), 2),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 4),
            d(-0.707, -0.707, Some(Spin::Clockwise), 6),
        ],
    ),
    // Octa quad V: m_04_02_octo_quad_v.svg
    (
        4,
        &[2],
        "Octa quad V",
        &[
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(-0.707, 0.707, Some(Spin::Clockwise), 7),
            d(-0.495, -0.707, Some(Spin::CounterClockwise), 5),
            d(0.495, -0.707, Some(Spin::Clockwise), 3),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 8),
            d(0.707, 0.707, Some(Spin::Clockwise), 2),
            d(0.495, -0.707, Some(Spin::CounterClockwise), 4),
            d(-0.495, -0.707, Some(Spin::Clockwise), 6),
        ],
    ),
    // Octa quad H: m_04_03_octo_quad_h.svg
    (
        4,
        &[3],
        "Octa quad H",
        &[
            d(0.707, 0.707, Some(Spin::Clockwise), 1),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 7),
            d(-0.707, -0.707, Some(Spin::Clockwise), 5),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 3),
            d(-0.707, 0.707, Some(Spin::Clockwise), 8),
            d(0.707, 0.707, Some(Spin::CounterClockwise), 2),
            d(0.707, -0.707, Some(Spin::Clockwise), 4),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 6),
        ],
    ),
    // Octa quad X, Betaflight order: m_04_12_octo_quad_x_bf.svg
    (
        4,
        &[12],
        "Octa quad X, Betaflight order",
        &[
            d(0.707, -0.707, Some(Spin::Clockwise), 3),
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 5),
            d(-0.707, 0.707, Some(Spin::Clockwise), 7),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 4),
            d(0.707, 0.707, Some(Spin::Clockwise), 2),
            d(-0.707, -0.707, Some(Spin::Clockwise), 6),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 8),
        ],
    ),
    // Octa quad X, clockwise order: m_04_14_octo_quad_x_cw.svg
    (
        4,
        &[14],
        "Octa quad X, clockwise order",
        &[
            d(0.707, 0.707, Some(Spin::CounterClockwise), 1),
            d(0.707, 0.707, Some(Spin::Clockwise), 2),
            d(0.707, -0.707, Some(Spin::Clockwise), 3),
            d(0.707, -0.707, Some(Spin::CounterClockwise), 4),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 5),
            d(-0.707, -0.707, Some(Spin::Clockwise), 6),
            d(-0.707, 0.707, Some(Spin::Clockwise), 7),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 8),
        ],
    ),
    // Octa quad X reversed, Betaflight order: m_04_18_octo_quad_x_bf_rev.svg
    (
        4,
        &[18],
        "Octa quad X reversed, Betaflight order",
        &[
            d(0.707, -0.707, Some(Spin::CounterClockwise), 3),
            d(0.707, 0.707, Some(Spin::Clockwise), 1),
            d(-0.707, -0.707, Some(Spin::Clockwise), 5),
            d(-0.707, 0.707, Some(Spin::CounterClockwise), 7),
            d(0.707, -0.707, Some(Spin::Clockwise), 4),
            d(0.707, 0.707, Some(Spin::CounterClockwise), 2),
            d(-0.707, -0.707, Some(Spin::CounterClockwise), 6),
            d(-0.707, 0.707, Some(Spin::Clockwise), 8),
        ],
    ),
    // Y6A: m_05_00_y6_a.svg
    (
        5,
        &[],
        "Y6A",
        &[
            d(0.895, 0.447, Some(Spin::CounterClockwise), 2),
            d(-0.895, 0.447, Some(Spin::Clockwise), 5),
            d(-0.895, 0.447, Some(Spin::CounterClockwise), 6),
            d(0.000, -0.895, Some(Spin::Clockwise), 4),
            d(0.895, 0.447, Some(Spin::Clockwise), 1),
            d(0.000, -0.895, Some(Spin::CounterClockwise), 3),
        ],
    ),
    // Y6B: m_05_10_y6_b.svg
    (
        5,
        &[10],
        "Y6B",
        &[
            d(0.894, 0.447, Some(Spin::Clockwise), 1),
            d(0.894, 0.447, Some(Spin::CounterClockwise), 2),
            d(0.000, -0.894, Some(Spin::Clockwise), 3),
            d(0.000, -0.894, Some(Spin::CounterClockwise), 4),
            d(-0.894, 0.447, Some(Spin::Clockwise), 5),
            d(-0.894, 0.447, Some(Spin::CounterClockwise), 6),
        ],
    ),
    // Y6F: m_05_11_y6_f.svg
    (
        5,
        &[11],
        "Y6F",
        &[
            d(0.000, -0.894, Some(Spin::CounterClockwise), 3),
            d(0.894, 0.447, Some(Spin::CounterClockwise), 1),
            d(-0.894, 0.447, Some(Spin::CounterClockwise), 5),
            d(0.000, -0.894, Some(Spin::Clockwise), 4),
            d(0.894, 0.447, Some(Spin::Clockwise), 2),
            d(-0.894, 0.447, Some(Spin::Clockwise), 6),
        ],
    ),
    // Dodeca hexa +: m_12_00_dodecahexa_plus.svg
    (
        12,
        &[0],
        "Dodeca hexa +",
        &[
            d(0.000, 0.894, Some(Spin::CounterClockwise), 1),
            d(0.000, 0.894, Some(Spin::Clockwise), 2),
            d(0.894, 0.447, Some(Spin::Clockwise), 3),
            d(0.894, 0.447, Some(Spin::CounterClockwise), 4),
            d(0.894, -0.447, Some(Spin::CounterClockwise), 5),
            d(0.894, -0.447, Some(Spin::Clockwise), 6),
            d(0.000, -0.894, Some(Spin::Clockwise), 7),
            d(0.000, -0.894, Some(Spin::CounterClockwise), 8),
            d(-0.894, -0.447, Some(Spin::CounterClockwise), 9),
            d(-0.894, -0.447, Some(Spin::Clockwise), 10),
            d(-0.894, 0.447, Some(Spin::Clockwise), 11),
            d(-0.894, 0.447, Some(Spin::CounterClockwise), 12),
        ],
    ),
    // Dodeca hexa X: m_12_01_dodecahexa_x.svg
    (
        12,
        &[1],
        "Dodeca hexa X",
        &[
            d(0.447, 0.894, Some(Spin::CounterClockwise), 1),
            d(0.447, 0.894, Some(Spin::Clockwise), 2),
            d(0.894, 0.000, Some(Spin::Clockwise), 3),
            d(0.894, 0.000, Some(Spin::CounterClockwise), 4),
            d(0.447, -0.894, Some(Spin::CounterClockwise), 5),
            d(0.447, -0.894, Some(Spin::Clockwise), 6),
            d(-0.447, -0.894, Some(Spin::Clockwise), 7),
            d(-0.447, -0.894, Some(Spin::CounterClockwise), 8),
            d(-0.894, 0.000, Some(Spin::CounterClockwise), 9),
            d(-0.894, 0.000, Some(Spin::Clockwise), 10),
            d(-0.447, 0.894, Some(Spin::Clockwise), 11),
            d(-0.447, 0.894, Some(Spin::CounterClockwise), 12),
        ],
    ),
    // Deca +: m_14_00_deca_plus.svg
    (
        14,
        &[0],
        "Deca +",
        &[
            d(0.000, 0.955, Some(Spin::CounterClockwise), 1),
            d(0.590, 0.773, Some(Spin::Clockwise), 2),
            d(0.955, 0.295, Some(Spin::CounterClockwise), 3),
            d(0.955, -0.295, Some(Spin::Clockwise), 4),
            d(0.590, -0.773, Some(Spin::CounterClockwise), 5),
            d(0.000, -0.955, Some(Spin::Clockwise), 6),
            d(-0.590, -0.773, Some(Spin::CounterClockwise), 7),
            d(-0.955, -0.295, Some(Spin::Clockwise), 8),
            d(-0.955, 0.295, Some(Spin::CounterClockwise), 9),
            d(-0.590, 0.773, Some(Spin::Clockwise), 10),
        ],
    ),
    // Deca X: m_14_01_deca_x_and_cw_x.svg
    (
        14,
        &[1, 14],
        "Deca X",
        &[
            d(0.295, 0.955, Some(Spin::CounterClockwise), 1),
            d(0.773, 0.590, Some(Spin::Clockwise), 2),
            d(0.955, 0.000, Some(Spin::CounterClockwise), 3),
            d(0.773, -0.590, Some(Spin::Clockwise), 4),
            d(0.295, -0.955, Some(Spin::CounterClockwise), 5),
            d(-0.295, -0.955, Some(Spin::Clockwise), 6),
            d(-0.773, -0.590, Some(Spin::CounterClockwise), 7),
            d(-0.955, 0.000, Some(Spin::Clockwise), 8),
            d(-0.773, 0.590, Some(Spin::CounterClockwise), 9),
            d(-0.295, 0.955, Some(Spin::Clockwise), 10),
        ],
    ),
];

/// The letter of a motor's place in the test's order: 1 is A.
#[must_use]
pub fn letter(order: u8) -> String {
    match order {
        1..=26 => char::from(b'A' + order - 1).to_string(),
        _ => order.to_string(),
    }
}

/// A multicopter's, or a `QuadPlane`'s VTOL, motors from its frame class
/// and type. A frame not drawn has no motors and is named by its numbers.
#[must_use]
pub fn multicopter(class: u8, frame_type: u8) -> MotorLayout {
    let table = TABLES
        .iter()
        .find(|(c, types, ..)| *c == class && types.contains(&frame_type))
        .or_else(|| {
            TABLES
                .iter()
                .find(|(c, types, ..)| *c == class && types.is_empty())
        });
    let Some((_, _, name, defs)) = table else {
        return MotorLayout {
            frame: format!("Frame class {class}, type {frame_type}"),
            motors: Vec::new(),
        };
    };
    let mut motors: Vec<MotorPlace> = defs
        .iter()
        .enumerate()
        .map(|(index, Def(right, forward, spin, order))| MotorPlace {
            order: *order,
            label: letter(*order),
            motor: u8::try_from(index + 1).unwrap_or(u8::MAX),
            right: *right,
            forward: *forward,
            spin: *spin,
            stack: 0,
        })
        .collect();
    motors.sort_by_key(|m| m.order);
    for i in 0..motors.len() {
        let below = motors[..i]
            .iter()
            .filter(|o| {
                (o.right - motors[i].right).abs() < 1e-3
                    && (o.forward - motors[i].forward).abs() < 1e-3
            })
            .count();
        motors[i].stack = u8::try_from(below).unwrap_or(u8::MAX);
    }
    MotorLayout {
        frame: (*name).to_owned(),
        motors,
    }
}

/// A rover's or boat's outputs the motor test drives: its left and right
/// throttles when it steers by skidding, else its throttle and steering.
#[must_use]
pub fn rover(skid_steer: bool) -> MotorLayout {
    let place = |order: u8, label: &str, right: f64, forward: f64| MotorPlace {
        order,
        label: label.to_owned(),
        motor: order,
        right,
        forward,
        spin: None,
        stack: 0,
    };
    if skid_steer {
        MotorLayout {
            frame: "Skid-steer rover".into(),
            motors: vec![place(3, "Left", -0.7, 0.0), place(4, "Right", 0.7, 0.0)],
        }
    } else {
        MotorLayout {
            frame: "Rover".into(),
            motors: vec![
                place(1, "Throttle", 0.0, -0.7),
                place(2, "Steering", 0.0, 0.7),
            ],
        }
    }
}

/// `SERVOn_FUNCTION` values that make a rover steer by skidding: its left
/// and right throttles.
pub const SKID_STEER_FUNCTIONS: [f64; 2] = [73.0, 74.0];

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(layout: &MotorLayout) -> Vec<(String, u8, &'static str, Option<Spin>)> {
        layout
            .motors
            .iter()
            .map(|m| {
                let side = match (
                    m.forward > 0.1,
                    m.forward < -0.1,
                    m.right > 0.1,
                    m.right < -0.1,
                ) {
                    (true, _, true, _) => "front right",
                    (true, _, _, true) => "front left",
                    (true, ..) => "front",
                    (_, true, true, _) => "back right",
                    (_, true, _, true) => "back left",
                    (_, true, ..) => "back",
                    (.., true, _) => "right",
                    (.., true) => "left",
                    _ => "middle",
                };
                (m.label.clone(), m.motor, side, m.spin)
            })
            .collect()
    }

    use Spin::{Clockwise as Cw, CounterClockwise as Ccw};

    #[test]
    fn quad_x_is_tested_clockwise_from_the_front_right() {
        let layout = multicopter(1, 1);
        assert_eq!(layout.frame, "Quad X");
        // ArduPilot's wiki: motor 1 front right counter-clockwise, 3 front
        // left clockwise; the test goes A to D clockwise from front right.
        assert_eq!(
            summary(&layout),
            vec![
                ("A".into(), 1, "front right", Some(Ccw)),
                ("B".into(), 4, "back right", Some(Cw)),
                ("C".into(), 2, "back left", Some(Ccw)),
                ("D".into(), 3, "front left", Some(Cw)),
            ]
        );
    }

    #[test]
    fn quad_plus_starts_at_the_nose() {
        let layout = multicopter(1, 0);
        assert_eq!(layout.frame, "Quad +");
        assert_eq!(
            summary(&layout),
            vec![
                ("A".into(), 3, "front", Some(Cw)),
                ("B".into(), 1, "right", Some(Ccw)),
                ("C".into(), 4, "back", Some(Cw)),
                ("D".into(), 2, "left", Some(Ccw)),
            ]
        );
    }

    #[test]
    fn h_spins_the_other_way_to_x() {
        let x = multicopter(1, 1);
        let h = multicopter(1, 3);
        for (x, h) in x.motors.iter().zip(&h.motors) {
            assert_eq!((x.order, x.motor), (h.order, h.motor));
            assert_ne!(x.spin, h.spin);
        }
    }

    #[test]
    fn hexa_and_octa_go_round_clockwise_alternating() {
        for (class, frame_type, count) in [(2, 0, 6), (2, 1, 6), (3, 0, 8), (3, 1, 8), (14, 1, 10)]
        {
            let layout = multicopter(class, frame_type);
            assert_eq!(layout.motors.len(), count, "{}", layout.frame);
            let bearing = |m: &MotorPlace| m.right.atan2(m.forward).to_degrees().rem_euclid(360.0);
            // Each motor in the test's order is further round clockwise.
            for pair in layout.motors.windows(2) {
                assert!(bearing(&pair[1]) > bearing(&pair[0]), "{}", layout.frame);
                assert_ne!(pair[0].spin, pair[1].spin, "{}", layout.frame);
            }
        }
        assert_eq!(
            summary(&multicopter(2, 1))[..2],
            [
                ("A".into(), 5, "front right", Some(Ccw)),
                ("B".into(), 1, "right", Some(Cw)),
            ]
        );
    }

    #[test]
    fn coaxial_pairs_are_stacked() {
        let y6 = multicopter(5, 10);
        assert_eq!(y6.frame, "Y6B");
        assert_eq!(
            y6.motors.iter().map(|m| m.stack).collect::<Vec<_>>(),
            [0, 1, 0, 1, 0, 1]
        );
        // Y6B: A and B front right, C and D at the back, E and F front left.
        assert_eq!(summary(&y6)[0].2, "front right");
        assert_eq!(summary(&y6)[2].2, "back");
        assert_eq!(summary(&y6)[4].2, "front left");
        assert_eq!(multicopter(5, 0).frame, "Y6A");
        assert_eq!(
            multicopter(4, 1)
                .motors
                .iter()
                .filter(|m| m.stack == 1)
                .count(),
            4
        );
    }

    #[test]
    fn every_table_has_each_order_once() {
        for (class, types, _, defs) in TABLES {
            let mut orders: Vec<u8> = defs.iter().map(|Def(.., o)| *o).collect();
            orders.sort_unstable();
            let expected: Vec<u8> = (1..=u8::try_from(defs.len()).unwrap_or(0)).collect();
            assert_eq!(orders, expected, "class {class} types {types:?}");
        }
    }

    #[test]
    fn frames_not_drawn_are_named() {
        let tri = multicopter(7, 0);
        assert_eq!(tri.frame, "Frame class 7, type 0");
        assert!(tri.motors.is_empty());
        // A quad's V-tail is drawn; its front motors yaw without torque.
        let v_tail = multicopter(1, 4);
        assert_eq!(v_tail.frame, "Quad V-tail");
        assert_eq!(summary(&v_tail)[0], ("A".into(), 1, "front right", None));
        assert_eq!(
            summary(&v_tail)[1],
            ("B".into(), 4, "back right", Some(Ccw))
        );
    }

    #[test]
    fn the_tables_are_what_the_diagrams_say() {
        // The documentation's diagrams: one motor per `<use>`, numbered and
        // lettered beside it. The generator keeps the tables in step; this
        // checks the copies in tools/frames/diagrams/ against them.
        // The generator and its diagrams are not in the public Core export;
        // there, `tools/frames/from_diagrams.py --check` in CI is the guard.
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/frames/diagrams");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("no {}: the generator's check covers it", dir.display());
            return;
        };
        assert_eq!(
            entries.count(),
            TABLES.len(),
            "a diagram per table in {}",
            dir.display()
        );
    }

    #[test]
    fn rovers_test_throttle_and_steering_or_each_side() {
        let rover = rover(false);
        assert_eq!(
            rover
                .motors
                .iter()
                .map(|m| (m.order, m.label.as_str()))
                .collect::<Vec<_>>(),
            [(1, "Throttle"), (2, "Steering")]
        );
        let skid = super::rover(true);
        assert_eq!(
            skid.motors
                .iter()
                .map(|m| (m.order, m.label.as_str()))
                .collect::<Vec<_>>(),
            [(3, "Left"), (4, "Right")]
        );
    }

    #[test]
    fn letters_follow_the_order() {
        assert_eq!(letter(1), "A");
        assert_eq!(letter(12), "L");
    }
}
