//! A plan's checks (ADR-0029 P9, S129): problems found in it, in the engine
//! so the Mission tab, the agent and the CLI see the same list. Errors stop
//! an upload; warnings do not. Heights over the ground come from the
//! terrain the vehicle would use (ADR-0026): what is not yet fetched is
//! asked for, and said to be missing until it comes.

use uxduck_core::domain::plan::{
    FenceCircle, FencePolygon, FenceSettings, LatLon, PlanAutopilot, PlanBody, PlanFrame,
    PlanVehicleType, command_has_position, distance_m, has_position,
};

/// How bad a check is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    /// Said, not counted: the plan does it on purpose (FB-273).
    Note,
}

/// One problem: what, where (an item's number from 1, when it is one), in words.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub severity: Severity,
    /// A stable name, for tests and the app: `below-ground`, `no-take-off`…
    pub code: &'static str,
    pub item: Option<u32>,
    pub message: String,
}

/// The ground's height above sea level at a point: `Some(Some(m))` known,
/// `Some(None)` known to have none (a void), `None` not fetched yet.
pub type Ground<'a> = dyn FnMut(LatLon) -> Option<Option<f64>> + 'a;

/// Over this, many countries ask for permission: said, not refused.
const HEIGHT_LIMIT_M: f64 = 120.0;
/// Samples along a leg, at most this far apart, however long the leg.
/// Until 2026-11-17 a leg had 200 samples at most, so past 10 km they
/// spread out and a narrow ridge between them went unchecked, unsaid.
const LEG_STEP_M: f64 = 50.0;

const TAKE_OFF: [u16; 2] = [22, 84];
const ENDS: [u16; 4] = [20, 21, 85, 189];
/// Loiter unlimited: the vehicle holds there until it is told otherwise.
const HOLDS: u16 = 17;
/// Loiter for turns or for a time: done circling, the mission runs out.
const LOITERS: [u16; 2] = [18, 19];
/// Repeat from item, Repeat from tag and the tag it repeats from.
const JUMP: u16 = 177;
const JUMP_TAG: u16 = 601;
const TAG: u16 = 600;
/// Set home: at the vehicle, when param1 is 1, else at the item's position.
const SET_HOME: u16 = 179;

/// Land, and a VTOL's land: the mission ends where it touches down.
const LANDS: [u16; 2] = [21, 85];

/// The navigation commands with a position, which ArduPilot flies through.
/// A landing sequence start, a region of interest (Look at) and the other
/// `DO_` and condition commands mark or face a place without going there.
fn flown_through(command: u16) -> bool {
    matches!(command, 16..=19 | 21 | 22 | 30 | 31 | 82 | 84 | 85 | 94)
}

/// Which commands a type cannot run (the app's `commands.ts`, its refusals).
fn unfit(command: u16, kind: PlanVehicleType) -> bool {
    use PlanVehicleType::{Boat, Multicopter, Plane, Rover, Submarine, Vtol};
    if kind == PlanVehicleType::Blimp {
        return !matches!(command, 16 | 177 | 183 | 206);
    }
    match command {
        84 | 85 | 3000 => kind != Vtol,
        30 | 188 | 189 | 191 | 212 | 223 => !matches!(kind, Plane | Vtol),
        83 | 210 => kind != Plane,
        82 | 92 | 94 | 216 | 222 | 42600 => kind != Multicopter,
        22 | 31 | 208 | 42703 => matches!(kind, Rover | Boat | Submarine),
        115 => matches!(kind, Plane | Vtol),
        194 | 213 => !matches!(kind, Rover | Boat),
        _ => false,
    }
}

/// Commands PX4 does not support in a mission (P14): ArduPilot's own, such
/// as continue and change altitude, spline waypoints and payload place, and
/// those PX4's mission leaves out. The app's `commands.ts` leaves them out
/// for PX4 too.
pub const PX4_UNFIT: [u16; 33] = [
    30, 82, 83, 92, 94, 112, 114, 115, 181, 182, 184, 188, 191, 193, 194, 202, 205, 207, 208, 210,
    212, 213, 215, 216, 217, 218, 222, 223, 534, 600, 601, 42702, 42703,
];

fn label(kind: PlanVehicleType) -> &'static str {
    match kind {
        PlanVehicleType::Multicopter => "a multicopter",
        PlanVehicleType::Plane => "a plane",
        PlanVehicleType::Vtol => "a VTOL",
        PlanVehicleType::Rover => "a rover",
        PlanVehicleType::Boat => "a boat",
        PlanVehicleType::Submarine => "a submarine",
        PlanVehicleType::Blimp => "a blimp",
    }
}

/// Every check on a plan, errors first.
#[allow(clippy::too_many_lines)]
pub fn check(body: &PlanBody, ground: &mut Ground<'_>) -> Vec<Check> {
    let mut checks = Vec::new();
    let error = |code, item, message: String| Check {
        severity: Severity::Error,
        code,
        item,
        message,
    };
    let warning = |code, item, message: String| Check {
        severity: Severity::Warning,
        code,
        item,
        message,
    };
    let number = |i: usize| u32::try_from(i + 1).ok();

    // What one mission carries, counted in 16 bits (`MOST_ITEMS`): more
    // could not be sent whole. ArduPilot keeps home as item 0 of them.
    let home = usize::from(body.autopilot == PlanAutopilot::Ardupilot);
    // A PX4 is sent a command more ahead of each Point gimbal step
    // (`plans::px4_gimbal_handovers`), which its mission carries too.
    let handovers = if body.autopilot == PlanAutopilot::Px4 {
        crate::plans::px4_gimbal_handover_count(body.items.iter().map(|item| item.command))
    } else {
        0
    };
    if body.items.len() + home + handovers > crate::plan_patterns::MOST_ITEMS {
        #[allow(clippy::cast_precision_loss, reason = "counts said in words")]
        let said = |count: usize| crate::plan_patterns::grouped(count as f64);
        let more = if handovers == 0 {
            String::new()
        } else {
            format!(
                " and PX4 is sent {} more, a Set gimbal control command ahead of each Point gimbal step",
                said(handovers)
            )
        };
        checks.push(error(
            "too-many-items",
            None,
            format!(
                "it has {} items{more}: one mission carries {} at most{}, as MAVLink counts them in 16 bits",
                said(body.items.len()),
                said(crate::plan_patterns::MOST_ITEMS - home),
                if home == 1 { " besides home" } else { "" }
            ),
        ));
    }

    // A plan last saved by an earlier UXDuck can hold part of what a
    // pattern makes: it stopped a survey at 2,000 items and the survey
    // looked complete. Said, and the upload is the person's to decide.
    for pattern in &body.patterns {
        let held = body
            .items
            .iter()
            .filter(|item| item.pattern.as_deref() == Some(pattern.id.as_str()))
            .count();
        let Some(makes) = crate::plan_patterns::makes_now(pattern, body, held) else {
            continue;
        };
        #[allow(clippy::cast_precision_loss, reason = "counts said in words")]
        let said = |count: usize| crate::plan_patterns::grouped(count as f64);
        let message = match crate::plan_patterns::refusal(pattern, body) {
            Some(why) => format!(
                "{} holds {} items made by an earlier UXDuck and cannot be made again as it is set: {why}",
                pattern.id,
                said(held)
            ),
            None => format!(
                "{} holds {} items and its settings make {}. An earlier UXDuck stopped a pattern at 2,000 items and flew a spacing under 1 m at 1 m, so it may cover part of its area. Change the plan and its items are made again",
                pattern.id,
                said(held),
                said(makes)
            ),
        };
        checks.push(warning("pattern-part", None, message));
    }

    if body.vehicle_type == PlanVehicleType::Blimp {
        checks.push(warning(
            "blimp-auto",
            None,
            "Blimp AUTO is experimental: climb manually before AUTO. It holds the last target when finished. LAND stops actuators, not a controlled descent. Go to is not implemented.".into(),
        ));
        if body.autopilot != PlanAutopilot::Ardupilot {
            checks.push(error(
                "blimp-autopilot",
                None,
                "Blimp plans require ArduPilot Blimp firmware with AUTO support.".into(),
            ));
        }
        if !body.fence.polygons.is_empty()
            || !body.fence.circles.is_empty()
            || body.fence.settings != FenceSettings::default()
            || !body.rally.is_empty()
        {
            checks.push(error("blimp-parts", None, "Blimp uploads support waypoints, not fence or rally transfers. Remove those parts before uploading.".into()));
        }
        for (index, item) in body.items.iter().enumerate() {
            if item.command == 16
                && item
                    .params
                    .iter()
                    .flatten()
                    .any(|value| value.is_finite() && *value != 0.0)
            {
                checks.push(error("blimp-waypoint-options", number(index), "Blimp ignores waypoint hold, acceptance/pass radius and yaw parameters. Clear them and use WP_RADIUS and WP_VEL in Parameters.".into()));
            }
            if matches!(item.command, 183 | 206) {
                checks.push(warning("blimp-payload", number(index), "Payload commands require the matching firmware feature and configured hardware. Mission acceptance does not verify the payload action.".into()));
            }
        }
    }

    for (i, item) in body.items.iter().enumerate() {
        if unfit(item.command, body.vehicle_type) {
            checks.push(error(
                "command-unfit",
                number(i),
                if body.vehicle_type == PlanVehicleType::Blimp {
                    format!(
                        "item {} is outside UXDuck's supported Blimp mission commands",
                        i + 1
                    )
                } else {
                    format!(
                        "item {} is a command {} cannot run",
                        i + 1,
                        label(body.vehicle_type)
                    )
                },
            ));
        } else if body.autopilot == PlanAutopilot::Px4 && PX4_UNFIT.contains(&item.command) {
            checks.push(error(
                "command-unfit",
                number(i),
                format!("item {} is a command PX4 does not support", i + 1),
            ));
        }
        if item.command == SET_HOME
            && item.params[0] != Some(1.0)
            && item.latitude_deg == 0.0
            && item.longitude_deg == 0.0
        {
            checks.push(error(
                "home-nowhere",
                number(i),
                format!(
                    "item {} would set home at 0°, 0°: set its Use current to 1",
                    i + 1
                ),
            ));
        }
    }

    // The fence: whole shapes, and home and the points inside it.
    for (p, polygon) in body.fence.polygons.iter().enumerate() {
        if polygon.points.len() < 3 {
            checks.push(error(
                "fence-shape",
                None,
                format!("fence area {} has fewer than three corners", p + 1),
            ));
        } else if crosses_itself(&polygon.points) {
            checks.push(error(
                "fence-shape",
                None,
                format!("fence area {} crosses itself", p + 1),
            ));
        }
    }
    let fenced = !body.fence.polygons.is_empty() || !body.fence.circles.is_empty();
    if fenced {
        if let Some(home) = body.planned_home {
            let at = LatLon {
                latitude_deg: home.latitude_deg,
                longitude_deg: home.longitude_deg,
            };
            if !allowed(&body.fence.polygons, &body.fence.circles, at) {
                checks.push(error(
                    "home-outside-fence",
                    None,
                    "the planned home is outside the fence: the vehicle would not arm there".into(),
                ));
            }
        }
        for (i, item) in body.items.iter().enumerate() {
            if !has_position(item) {
                continue;
            }
            let at = LatLon {
                latitude_deg: item.latitude_deg,
                longitude_deg: item.longitude_deg,
            };
            if !allowed(&body.fence.polygons, &body.fence.circles, at) {
                checks.push(error(
                    "outside-fence",
                    number(i),
                    format!("item {} is outside the fence", i + 1),
                ));
            }
        }
        for (r, rally) in body.rally.iter().enumerate() {
            let at = LatLon {
                latitude_deg: rally.latitude_deg,
                longitude_deg: rally.longitude_deg,
            };
            if !allowed(&body.fence.polygons, &body.fence.circles, at) {
                checks.push(warning(
                    "rally-outside-fence",
                    None,
                    format!("rally point {} is outside the fence", r + 1),
                ));
            }
        }
    }

    // Above the fence's ceiling (S132): the fence acts when it gets there.
    if let Some(ceiling) = body.fence.settings.ceiling_m {
        for (i, item) in body.items.iter().enumerate() {
            let over_home_or_ground = matches!(item.frame, 3 | 6 | 10 | 11);
            if has_position(item) && over_home_or_ground && item.altitude_m > ceiling {
                checks.push(warning(
                    "above-ceiling",
                    number(i),
                    format!(
                        "item {} is at {} m, above the fence's {} m ceiling: the fence acts when it gets there",
                        i + 1,
                        item.altitude_m.round(),
                        ceiling.round()
                    ),
                ));
            }
        }
    }

    let flies = body.vehicle_type.flies();
    if flies && !body.items.is_empty() {
        let first = body.items[0].command;
        if matches!(
            body.vehicle_type,
            PlanVehicleType::Multicopter | PlanVehicleType::Vtol
        ) && !TAKE_OFF.contains(&first)
        {
            checks.push(warning(
                "no-take-off",
                Some(1),
                "it does not start with a take-off: from the ground it needs one".into(),
            ));
        }
        if body.vehicle_type != PlanVehicleType::Blimp {
            checks.extend(end(body));
        }
        // A landing before the end: what follows is never reached, unless it
        // takes off again.
        if let Some(at) = body.items.iter().position(|i| LANDS.contains(&i.command)) {
            let rest = &body.items[at + 1..];
            let again = rest
                .iter()
                .find(|i| command_has_position(i.command) || TAKE_OFF.contains(&i.command));
            if again.is_some_and(|i| !TAKE_OFF.contains(&i.command)) {
                checks.push(warning(
                    "lands-early",
                    number(at),
                    format!(
                        "item {} lands the vehicle: items {} to {} after it are never reached",
                        at + 1,
                        at + 2,
                        body.items.len()
                    ),
                ));
            }
        }
        let lands = body.items.iter().any(|i| i.command == 21);
        let sequence = body.items.iter().any(|i| i.command == 189);
        if body.vehicle_type == PlanVehicleType::Plane && lands && !sequence {
            checks.push(warning(
                "no-landing-sequence",
                None,
                "it lands but has no landing sequence start: a return to launch will not use it"
                    .into(),
            ));
        }
        clearance(body, ground, &mut checks);
    }
    checks.sort_by_key(|c| match c.severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Note => 2,
    });
    checks
}

/// The navigation commands (`AP_Mission::is_nav_cmd`): flown one after
/// another. The others run beside the one before them.
fn navigation(command: u16) -> bool {
    command <= 95 || matches!(command, 213 | 42702 | 42703)
}

/// Whether a mission has a Repeat from item or from tag.
fn repeats(body: &PlanBody) -> bool {
    body.items
        .iter()
        .any(|i| matches!(i.command, JUMP | JUMP_TAG))
}

/// How the mission ends (FB-273). Where it holds or ends loitering is
/// meant: a note. Stopping at a waypoint is a warning. A Repeat from item
/// or tag anywhere takes it round as each firmware counts and stops it, so
/// its end is never sure, a landing after it included: its last item warns
/// that, promising nothing, and the warning can be kept as is. Without one,
/// the first Loiter unlimited holds for good, else it ends at the last
/// navigation command, past the actions after it.
fn end(body: &PlanBody) -> Option<Check> {
    let items = &body.items;
    let last = items.len().checked_sub(1)?;
    if repeats(body) {
        return Some(Check {
            severity: Severity::Warning,
            code: "no-end",
            item: u32::try_from(last + 1).ok(),
            message: "it repeats items, so where it ends is not certain, and no landing or return is sure to be reached".into(),
        });
    }
    let at = items
        .iter()
        .position(|i| i.command == HOLDS)
        .or_else(|| items.iter().rposition(|i| navigation(i.command)))
        .unwrap_or(last);
    let command = body.items[at].command;
    if ENDS.contains(&command) || items[last].command == 189 {
        return None;
    }
    let item = u32::try_from(at + 1).ok();
    let ardupilot = body.autopilot == PlanAutopilot::Ardupilot;
    // ArduPlane, a VTOL's too, switches to RTL when its mission runs out
    // (`Plane::exit_mission_callback`), or a QuadPlane to QRTL when set to.
    // ArduCopter holds there, and lands if it cannot (`ModeAuto::exit_mission`).
    let plane = ardupilot
        && matches!(
            body.vehicle_type,
            PlanVehicleType::Plane | PlanVehicleType::Vtol
        );
    let copter = ardupilot && body.vehicle_type == PlanVehicleType::Multicopter;
    let note = |message: String| Check {
        severity: Severity::Note,
        code: "ends-loitering",
        item,
        message,
    };
    // PX4 goes on by itself only from an item set to: one before the end
    // with "Go on to the next item by itself" unticked waits there, so the
    // loiter is not reached by itself. Nor does PX4 fly Loiter turns.
    let waits = body.autopilot == PlanAutopilot::Px4 && items[..at].iter().any(|i| !i.autocontinue);
    let loiters = !waits && LOITERS.contains(&command) && (ardupilot || command != 18);
    Some(if !waits && command == HOLDS {
        note(format!(
            "it ends loitering at item {} until you take over",
            at + 1
        ))
    } else if loiters {
        let then = if plane {
            "then ArduPilot normally switches to Return to launch"
        } else if copter {
            "then ArduPilot holds there until you take over, or lands if it cannot hold"
        } else {
            "then the vehicle waits there until you take over"
        };
        note(format!("it ends loitering at item {}, {then}", at + 1))
    } else {
        Check {
            severity: Severity::Warning,
            code: "no-end",
            item,
            message: if plane {
                "it ends without a landing or a return of its own: ArduPilot then normally switches to Return to launch"
            } else if copter {
                "it ends without a landing or a return: ArduPilot holds at the last point, or lands if it cannot hold"
            } else {
                "it ends without a landing or a return: the vehicle waits at the last point"
            }
            .into(),
        }
    })
}

/// What a warning is about, to tell whether one kept as is (FB-273) is
/// still the same: its code and words, the vehicle type and autopilot it
/// was checked for, and its subject where it stands in the plan. Inserting,
/// removing or moving items before it, or changing it, makes another. A
/// hex SHA-256, as the subject can be the whole plan.
#[must_use]
pub fn about(body: &PlanBody, check: &Check) -> String {
    use sha2::Digest as _;
    let at = |i: usize| serde_json::json!({ "at": i + 1, "item": body.items.get(i) });
    let index = check
        .item
        .and_then(|n| usize::try_from(n).ok()?.checked_sub(1));
    let subject = match (check.code, index) {
        // A mission with a repeat can end anywhere in it: all of it, in order.
        ("no-end", Some(_)) if repeats(body) => {
            serde_json::to_value(&body.items).unwrap_or_default()
        }
        // How it ends: the end, everything after it, and every repeat.
        ("no-end", Some(end)) => body
            .items
            .iter()
            .enumerate()
            .filter(|(i, item)| *i >= end || matches!(item.command, JUMP | JUMP_TAG | TAG))
            .map(|(i, _)| at(i))
            .collect(),
        (_, Some(i)) => at(i),
        ("rally-outside-fence", None) => {
            serde_json::json!({ "rally": body.rally, "fence": body.fence })
        }
        ("blimp-auto", None) => serde_json::Value::Null,
        (_, None) => serde_json::to_value(body).unwrap_or_default(),
    };
    let fingerprint = serde_json::json!({
        "code": check.code,
        "message": check.message,
        "vehicleType": body.vehicle_type,
        "autopilot": body.autopilot,
        "subject": subject,
    });
    sha2::Sha256::digest(fingerprint.to_string().as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// A point the vehicle flies through, not one it looks at or marks.
struct Point {
    /// The item's number, from 1.
    number: usize,
    at: LatLon,
    /// In its own frame.
    height: f64,
    frame: PlanFrame,
    #[allow(clippy::option_option, reason = "what `Ground` answers there")]
    under: Option<Option<f64>>,
}

/// Heights over the ground at each point and along each leg.
#[allow(clippy::too_many_lines)]
fn clearance(body: &PlanBody, ground: &mut Ground<'_>, checks: &mut Vec<Check>) {
    let home_ground = body.planned_home.and_then(|home| {
        ground(LatLon {
            latitude_deg: home.latitude_deg,
            longitude_deg: home.longitude_deg,
        })
        .flatten()
        .or(home.altitude_m)
    });
    let points: Vec<Point> = body
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| has_position(item) && flown_through(item.command))
        .filter_map(|(i, item)| {
            let at = LatLon {
                latitude_deg: item.latitude_deg,
                longitude_deg: item.longitude_deg,
            };
            Some(Point {
                number: i + 1,
                at,
                height: item.altitude_m,
                frame: PlanFrame::from_mav_frame(item.frame)?,
                under: ground(at),
            })
        })
        .collect();
    let above_sea = |height: f64, frame: PlanFrame, ground_m: Option<f64>| match frame {
        PlanFrame::AboveSeaLevel => Some(height),
        PlanFrame::AboveTerrain => ground_m.map(|g| g + height),
        PlanFrame::AboveHome => home_ground.map(|home| home + height),
    };
    let mut missing = false;
    let mut worst_below: Option<(f64, Option<usize>)> = None;
    let mut worst_low: Option<(f64, Option<usize>)> = None;
    let mut too_high: Option<usize> = None;
    let mut measure = |over: f64, item: Option<usize>, minimum: f64| {
        if over < 0.0 && worst_below.is_none_or(|(w, _)| over < w) {
            worst_below = Some((over, item));
        } else if over >= 0.0 && over < minimum && worst_low.is_none_or(|(w, _)| over < w) {
            worst_low = Some((over, item));
        }
        if over > HEIGHT_LIMIT_M && too_high.is_none() {
            too_high = item.or(Some(0));
        }
    };
    for (index, point) in points.iter().enumerate() {
        let &Point {
            number,
            at,
            height,
            frame,
            under,
        } = point;
        let landing = LANDS.contains(&body.items[number - 1].command);
        match under {
            None => missing = true,
            Some(None) => {}
            Some(Some(ground_m)) => match above_sea(height, frame, Some(ground_m)) {
                Some(amsl) => measure(
                    amsl - ground_m,
                    Some(number),
                    if landing { 0.0 } else { body.min_clearance_m },
                ),
                None => missing = true,
            },
        }
        let Some(&Point {
            number: next_number,
            at: next,
            height: next_height,
            frame: next_frame,
            under: next_under,
        }) = points.get(index + 1)
        else {
            continue;
        };
        // Along the leg, at the heights the autopilot flies between them.
        // ArduPilot flies a leg to a point above terrain at heights over the
        // ground, starting from its height over the ground where it is.
        // Every other leg, and every leg on PX4 (which gets points above
        // terrain as heights above sea level), is a straight line above sea
        // level. `None` when an end's ground or home is not known.
        let follows_ground =
            body.autopilot == PlanAutopilot::Ardupilot && next_frame == PlanFrame::AboveTerrain;
        let ends = if follows_ground {
            let from = if frame == PlanFrame::AboveTerrain {
                Some(height)
            } else {
                under
                    .flatten()
                    .and_then(|g| above_sea(height, frame, Some(g)).map(|amsl| amsl - g))
            };
            from.map(|from| (from, next_height))
        } else {
            above_sea(height, frame, under.flatten()).zip(above_sea(
                next_height,
                next_frame,
                next_under.flatten(),
            ))
        };
        let length = distance_m(at, next);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a count of samples along a leg"
        )]
        let pieces = (length / LEG_STEP_M).ceil() as u32;
        for k in 1..pieces {
            let f = f64::from(k) / f64::from(pieces);
            let spot = LatLon {
                latitude_deg: at.latitude_deg + (next.latitude_deg - at.latitude_deg) * f,
                longitude_deg: at.longitude_deg + (next.longitude_deg - at.longitude_deg) * f,
            };
            // A normal approach descends to touchdown. Its clearance margin
            // tapers to zero; terrain penetration is still always an error.
            let minimum = if LANDS.contains(&body.items[next_number - 1].command) {
                body.min_clearance_m * (1.0 - f)
            } else {
                body.min_clearance_m
            };
            let Some(known) = ground(spot) else {
                missing = true;
                continue;
            };
            let Some(ground_m) = known else {
                continue;
            };
            let Some((from, to)) = ends else {
                missing = true;
                continue;
            };
            let planned = from + (to - from) * f;
            measure(
                if follows_ground {
                    planned
                } else {
                    planned - ground_m
                },
                None,
                minimum,
            );
        }
    }
    let item_of = |item: Option<usize>| item.and_then(|n| u32::try_from(n).ok());
    let place = |item: Option<usize>| {
        item.map_or_else(|| "along a leg".to_owned(), |n| format!("at item {n}"))
    };
    if let Some((over, item)) = worst_below {
        checks.push(Check {
            severity: Severity::Error,
            code: "below-ground",
            item: item_of(item),
            message: format!("{} m below the ground {}", (-over).round(), place(item)),
        });
    }
    if let Some((over, item)) = worst_low {
        checks.push(Check {
            severity: Severity::Warning,
            code: "low-clearance",
            item: item_of(item),
            message: format!(
                "only {} m over the ground {} (the plan asks for {} m)",
                over.round(),
                place(item),
                body.min_clearance_m.round()
            ),
        });
    }
    if let Some(item) = too_high {
        let item = (item > 0).then_some(item);
        checks.push(Check {
            severity: Severity::Warning,
            code: "high",
            item: item_of(item),
            message: format!(
                "more than {HEIGHT_LIMIT_M} m over the ground {}: many countries ask for permission",
                place(item)
            ),
        });
    }
    if missing {
        let severity = if body.frame == PlanFrame::AboveTerrain {
            Severity::Error
        } else {
            Severity::Warning
        };
        checks.push(Check {
            severity,
            code: "terrain-missing",
            item: None,
            message: "the ground is not known everywhere along it yet: fetching it".into(),
        });
    }
}

/// Inside every inclusion shape and outside every exclusion one.
fn allowed(polygons: &[FencePolygon], circles: &[FenceCircle], at: LatLon) -> bool {
    let polygons_ok = polygons
        .iter()
        .filter(|p| p.points.len() >= 3)
        .all(|p| inside(&p.points, at) == p.inclusion);
    let circles_ok = circles
        .iter()
        .all(|c| (distance_m(c.center, at) <= c.radius_m) == c.inclusion);
    polygons_ok && circles_ok
}

/// Ray casting, on latitude and longitude: fences are small enough.
fn inside(points: &[LatLon], at: LatLon) -> bool {
    let mut inside = false;
    let mut j = points.len() - 1;
    for i in 0..points.len() {
        let (a, b) = (points[i], points[j]);
        if (a.latitude_deg > at.latitude_deg) != (b.latitude_deg > at.latitude_deg)
            && at.longitude_deg
                < (b.longitude_deg - a.longitude_deg) * (at.latitude_deg - a.latitude_deg)
                    / (b.latitude_deg - a.latitude_deg)
                    + a.longitude_deg
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Whether two edges that do not share a corner cross.
fn crosses_itself(points: &[LatLon]) -> bool {
    let n = points.len();
    let edge = |i: usize| (points[i], points[(i + 1) % n]);
    let cross = |o: LatLon, a: LatLon, b: LatLon| {
        (a.longitude_deg - o.longitude_deg) * (b.latitude_deg - o.latitude_deg)
            - (a.latitude_deg - o.latitude_deg) * (b.longitude_deg - o.longitude_deg)
    };
    for i in 0..n {
        for j in (i + 1)..n {
            if j == i + 1 || (i == 0 && j == n - 1) {
                continue;
            }
            let (p1, p2) = edge(i);
            let (q1, q2) = edge(j);
            let d1 = cross(q1, q2, p1);
            let d2 = cross(q1, q2, p2);
            let d3 = cross(p1, p2, q1);
            let d4 = cross(p1, p2, q2);
            if d1 * d2 < 0.0 && d3 * d4 < 0.0 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use uxduck_core::domain::plan::{PlanAutopilot, PlanItem, PlannedHome};

    use super::*;

    #[test]
    fn blimp_waypoints_work_without_takeoff_or_landing_but_skipped_commands_block() {
        let mut plan = body(vec![item(16, 0.0001, 0.001, 30.0, 3)]);
        plan.vehicle_type = PlanVehicleType::Blimp;
        let clean = check(&plan, &mut hill);
        assert!(!clean.iter().any(|check| check.severity == Severity::Error));
        assert!(codes(&clean).contains(&"blimp-auto"));
        assert!(!codes(&clean).contains(&"no-end"));
        for command in [17, 18, 19, 20, 21, 22, 82, 93, 178, 201, 31000] {
            plan.items[0].command = command;
            assert!(
                codes(&check(&plan, &mut hill)).contains(&"command-unfit"),
                "{command}"
            );
        }
        plan.items[0].command = 16;
        plan.items[0].params[0] = Some(5.0);
        assert!(codes(&check(&plan, &mut hill)).contains(&"blimp-waypoint-options"));
        plan.items[0].params = [None; 4];
        plan.autopilot = PlanAutopilot::Px4;
        assert!(codes(&check(&plan, &mut hill)).contains(&"blimp-autopilot"));
    }

    #[test]
    fn a_survey_an_earlier_uxduck_cut_short_is_said_until_its_items_are_made_again() {
        use uxduck_core::domain::plan::{PatternShape, PlanPattern};
        let at = |lat: f64, lon: f64| LatLon {
            latitude_deg: lat,
            longitude_deg: lon,
        };
        // A field 3 km across with lanes 2 m apart: 1,502 lanes.
        let survey = |spacing_m: f64, east: f64| PlanPattern {
            id: "survey-1".into(),
            shape: PatternShape::Survey {
                area: vec![
                    at(0.001, 0.0),
                    at(0.001, east),
                    at(0.002, east),
                    at(0.002, 0.0),
                ],
                spacing_m,
                angle_deg: 0.0,
                turnaround_m: 0.0,
                altitude_m: 90.0,
                crosshatch: false,
                margin_m: 0.0,
                start: None,
                shortest: false,
            },
        };
        let part = |plan: &PlanBody| {
            check(plan, &mut hill)
                .into_iter()
                .find(|c| c.code == "pattern-part")
        };
        let mut plan = body(vec![
            item(22, 0.0, 0.0, 90.0, 3),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        plan.patterns = vec![survey(2.0, 0.027)];
        crate::plan_patterns::expand(&mut plan);
        // As it is saved now: every lane, and nothing to say.
        assert_eq!(plan.items.len(), 2 + 3004);
        assert_eq!(part(&plan), None);
        // As an earlier UXDuck saved it: stopped at 2,000 items.
        let mut cut = plan.clone();
        cut.items.truncate(2 + 2000);
        let said = part(&cut).unwrap();
        assert_eq!(
            said.severity,
            Severity::Warning,
            "the upload is theirs to decide"
        );
        assert_eq!(
            said.message,
            "survey-1 holds 2,000 items and its settings make 3,004. An earlier UXDuck stopped a pattern at 2,000 items and flew a spacing under 1 m at 1 m, so it may cover part of its area. Change the plan and its items are made again"
        );
        // Lanes half a metre apart that it flew a metre apart.
        let mut close = body(vec![
            item(22, 0.0, 0.0, 90.0, 3),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        close.patterns = vec![survey(0.5, 0.0009)];
        crate::plan_patterns::expand(&mut close);
        assert_eq!(part(&close), None);
        let whole = close.items.len() - 2;
        close.items.truncate(2 + whole / 2);
        assert!(part(&close).unwrap().message.starts_with(&format!(
            "survey-1 holds {} items and its settings make {whole}.",
            whole / 2
        )),);
        // One it would refuse now says why, where it once flew 1 m.
        close.patterns = vec![survey(0.0, 0.0009)];
        assert_eq!(
            part(&close).unwrap().message,
            format!(
                "survey-1 holds {} items made by an earlier UXDuck and cannot be made again as it is set: survey-1's spacing is 0 m. Lanes need a spacing above 0 m",
                whole / 2
            )
        );
    }

    #[test]
    fn a_plan_of_more_items_than_one_mission_carries_cannot_go_up() {
        // Take-off, then waypoints 1 m apart, then a return: ArduPilot's
        // mission has home before them, so 65,534 is the most.
        let mut items = vec![item(22, 0.0, 0.0, 30.0, 3)];
        items.extend((0..65_532).map(|i| item(16, 0.0001, f64::from(i) * 1e-5, 30.0, 3)));
        items.push(item(20, 0.0, 0.0, 0.0, 3));
        let mut plan = body(items);
        assert_eq!(plan.items.len(), 65_534);
        assert!(!codes(&check(&plan, &mut hill)).contains(&"too-many-items"));
        plan.items.insert(1, item(16, 0.0001, 0.0, 30.0, 3));
        let checks = check(&plan, &mut hill);
        let too_many = checks.iter().find(|c| c.code == "too-many-items").unwrap();
        assert_eq!(too_many.severity, Severity::Error);
        assert_eq!(
            too_many.message,
            "it has 65,535 items: one mission carries 65,534 at most besides home, as MAVLink counts them in 16 bits"
        );
        // PX4 has no home among them.
        plan.autopilot = PlanAutopilot::Px4;
        assert!(!codes(&check(&plan, &mut hill)).contains(&"too-many-items"));
    }

    #[test]
    fn a_px4_plans_point_gimbal_steps_count_with_the_command_sent_ahead_of_each() {
        // 65,534 items go to a PX4 as they are, which has no home among
        // them. A Point gimbal step is sent with a command ahead of it.
        let mut items = vec![item(22, 0.0, 0.0, 30.0, 3)];
        items.extend((0..65_533).map(|i| item(16, 0.0001, f64::from(i) * 1e-5, 30.0, 3)));
        let mut plan = body(items);
        plan.autopilot = PlanAutopilot::Px4;
        assert_eq!(plan.items.len(), 65_534);
        assert!(!codes(&check(&plan, &mut hill)).contains(&"too-many-items"));
        plan.items[2] = item(1000, 0.0, 0.0, 0.0, 2);
        assert!(!codes(&check(&plan, &mut hill)).contains(&"too-many-items"));
        plan.items[4] = item(1000, 0.0, 0.0, 0.0, 2);
        let checks = check(&plan, &mut hill);
        let too_many = checks.iter().find(|c| c.code == "too-many-items").unwrap();
        assert_eq!(too_many.severity, Severity::Error);
        assert_eq!(
            too_many.message,
            "it has 65,534 items and PX4 is sent 2 more, a Set gimbal control command ahead of each Point gimbal step: one mission carries 65,535 at most, as MAVLink counts them in 16 bits"
        );
        // A step with the command ahead of it already is sent no second one.
        plan.items[3] = item(1001, 0.0, 0.0, 0.0, 2);
        assert!(!codes(&check(&plan, &mut hill)).contains(&"too-many-items"));
        // ArduPilot is sent the plan as it is, after home.
        plan.items[3] = item(16, 0.0001, 0.0, 30.0, 3);
        plan.autopilot = PlanAutopilot::Ardupilot;
        assert!(!codes(&check(&plan, &mut hill)).contains(&"too-many-items"));
    }

    #[test]
    fn type_and_autopilot_specific_commands_block_elsewhere() {
        let mut plan = body(vec![item(16, 0.0001, 0.001, 30.0, 3)]);
        for (command, fits, misfits) in [
            (30, PlanVehicleType::Plane, PlanVehicleType::Multicopter),
            (83, PlanVehicleType::Plane, PlanVehicleType::Vtol),
            (94, PlanVehicleType::Multicopter, PlanVehicleType::Plane),
            (42600, PlanVehicleType::Multicopter, PlanVehicleType::Rover),
            (115, PlanVehicleType::Rover, PlanVehicleType::Plane),
            (208, PlanVehicleType::Vtol, PlanVehicleType::Boat),
            (213, PlanVehicleType::Boat, PlanVehicleType::Multicopter),
            (222, PlanVehicleType::Multicopter, PlanVehicleType::Plane),
            (212, PlanVehicleType::Plane, PlanVehicleType::Multicopter),
            (42703, PlanVehicleType::Multicopter, PlanVehicleType::Rover),
            (223, PlanVehicleType::Vtol, PlanVehicleType::Multicopter),
        ] {
            plan.items[0].command = command;
            plan.vehicle_type = fits;
            assert!(!unfit(command, fits), "{command} on {fits:?}");
            plan.vehicle_type = misfits;
            assert!(
                codes(&check(&plan, &mut hill)).contains(&"command-unfit"),
                "{command} on {misfits:?}"
            );
        }
        plan.vehicle_type = PlanVehicleType::Multicopter;
        plan.autopilot = PlanAutopilot::Px4;
        for command in [2000, 1000, 197] {
            plan.items[0].command = command;
            assert!(
                !codes(&check(&plan, &mut hill)).contains(&"command-unfit"),
                "{command}"
            );
        }
        plan.items[0].command = 112;
        assert!(codes(&check(&plan, &mut hill)).contains(&"command-unfit"));
    }

    #[test]
    fn set_home_at_nowhere_is_an_error() {
        let mut plan = body(vec![item(179, 0.0, 0.0, 0.0, 3)]);
        assert!(codes(&check(&plan, &mut hill)).contains(&"home-nowhere"));
        plan.items[0].params[0] = Some(1.0);
        assert!(!codes(&check(&plan, &mut hill)).contains(&"home-nowhere"));
        plan.items[0].params[0] = Some(0.0);
        plan.items[0].latitude_deg = -35.36;
        plan.items[0].longitude_deg = 149.16;
        assert!(!codes(&check(&plan, &mut hill)).contains(&"home-nowhere"));
    }

    fn item(command: u16, lat: f64, lon: f64, alt: f64, frame: u8) -> PlanItem {
        PlanItem {
            command,
            frame,
            params: [None; 4],
            latitude_deg: lat,
            longitude_deg: lon,
            altitude_m: alt,
            autocontinue: true,
            pattern: None,
        }
    }

    fn body(items: Vec<PlanItem>) -> PlanBody {
        let mut body = PlanBody::new(
            "Valley".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        body.frame = PlanFrame::AboveHome;
        body.planned_home = Some(PlannedHome {
            latitude_deg: 0.0001,
            longitude_deg: 0.0,
            altitude_m: None,
        });
        body.items = items;
        body
    }

    /// Flat at 100 m, with a hill rising to 180 m east of longitude 0.004.
    #[allow(clippy::option_option, clippy::unnecessary_wraps)]
    fn hill(at: LatLon) -> Option<Option<f64>> {
        Some(Some(if at.longitude_deg > 0.004 {
            180.0
        } else {
            100.0
        }))
    }

    fn codes(checks: &[Check]) -> Vec<&'static str> {
        checks.iter().map(|c| c.code).collect()
    }

    /// How the plan's end is said: its severity, code, item and words.
    fn end(plan: &PlanBody) -> Vec<(Severity, &'static str, Option<u32>, String)> {
        check(plan, &mut hill)
            .into_iter()
            .filter(|c| matches!(c.code, "no-end" | "ends-loitering"))
            .map(|c| (c.severity, c.code, c.item, c.message))
            .collect()
    }

    fn said(
        severity: Severity,
        code: &'static str,
        item: u32,
        message: &str,
    ) -> Vec<(Severity, &'static str, Option<u32>, String)> {
        vec![(severity, code, Some(item), message.to_owned())]
    }

    fn with(command: u16, params: [Option<f64>; 4]) -> PlanItem {
        PlanItem {
            params,
            ..item(command, 0.0, 0.0, 0.0, 3)
        }
    }

    const STOPS_PLANE: &str = "it ends without a landing or a return of its own: ArduPilot then normally switches to Return to launch";
    const STOPS_COPTER: &str = "it ends without a landing or a return: ArduPilot holds at the last point, or lands if it cannot hold";
    const STOPS: &str =
        "it ends without a landing or a return: the vehicle waits at the last point";
    const UNSURE: &str = "it repeats items, so where it ends is not certain, and no landing or return is sure to be reached";

    #[test]
    fn a_mission_that_ends_loitering_is_a_note_and_one_that_stops_at_a_waypoint_warns() {
        // FB-273: a plane's mission ending on Loiter unlimited, waiting to
        // be told, warned that it ends without a landing or a return.
        let mut plan = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 40.0, 3),
            item(17, 0.0002, 0.002, 40.0, 3),
        ]);
        plan.vehicle_type = PlanVehicleType::Plane;
        let holds = said(
            Severity::Note,
            "ends-loitering",
            3,
            "it ends loitering at item 3 until you take over",
        );
        assert_eq!(end(&plan), holds);
        plan.items[2].autocontinue = false;
        assert_eq!(end(&plan), holds);
        // Said after every warning, and never stopping an upload.
        let all = check(&plan, &mut hill);
        assert_eq!(all.last().map(|c| c.severity), Some(Severity::Note));
        assert!(
            !all.iter().any(|c| c.severity == Severity::Error),
            "{all:?}"
        );

        // Loiter turns or time with nothing after: ArduPlane normally
        // switches to RTL (a QuadPlane can take QRTL), ArduCopter holds and
        // lands if it cannot, PX4 waits there.
        for command in LOITERS {
            plan.items[2].command = command;
            plan.vehicle_type = PlanVehicleType::Plane;
            plan.autopilot = PlanAutopilot::Ardupilot;
            let returns = said(
                Severity::Note,
                "ends-loitering",
                3,
                "it ends loitering at item 3, then ArduPilot normally switches to Return to launch",
            );
            assert_eq!(end(&plan), returns, "{command}");
            plan.vehicle_type = PlanVehicleType::Vtol;
            assert_eq!(end(&plan), returns, "{command}");
            plan.vehicle_type = PlanVehicleType::Multicopter;
            assert_eq!(
                end(&plan)[0].3,
                "it ends loitering at item 3, then ArduPilot holds there until you take over, or lands if it cannot hold",
                "{command}"
            );
            plan.autopilot = PlanAutopilot::Px4;
            for kind in [PlanVehicleType::Multicopter, PlanVehicleType::Plane] {
                plan.vehicle_type = kind;
                if command == 18 {
                    // PX4 does not fly Loiter turns: nothing reassuring.
                    assert_eq!(end(&plan), said(Severity::Warning, "no-end", 3, STOPS));
                } else {
                    assert_eq!(
                        end(&plan)[0].3,
                        "it ends loitering at item 3, then the vehicle waits there until you take over"
                    );
                }
            }
            plan.autopilot = PlanAutopilot::Ardupilot;
        }

        // A plain waypoint last still warns.
        plan.items[2].command = 16;
        plan.vehicle_type = PlanVehicleType::Multicopter;
        assert_eq!(
            end(&plan),
            said(Severity::Warning, "no-end", 3, STOPS_COPTER)
        );
        plan.autopilot = PlanAutopilot::Px4;
        assert_eq!(end(&plan), said(Severity::Warning, "no-end", 3, STOPS));
        plan.autopilot = PlanAutopilot::Ardupilot;
        plan.vehicle_type = PlanVehicleType::Plane;
        assert_eq!(
            end(&plan),
            said(Severity::Warning, "no-end", 3, STOPS_PLANE)
        );
        // Loiter unlimited before the end holds there for good.
        plan.items[1].command = 17;
        assert_eq!(
            end(&plan),
            said(
                Severity::Note,
                "ends-loitering",
                2,
                "it ends loitering at item 2 until you take over"
            )
        );
        plan.items[1].command = 16;

        // A return to launch or a landing says nothing about the end.
        for command in [20, 21] {
            plan.items[2].command = command;
            assert!(end(&plan).is_empty(), "{command}");
        }

        // Nor does a rover's or a blimp's: the end is a flying check.
        plan.items[2].command = 16;
        for kind in [
            PlanVehicleType::Rover,
            PlanVehicleType::Boat,
            PlanVehicleType::Blimp,
        ] {
            plan.vehicle_type = kind;
            assert!(end(&plan).is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn the_end_is_the_last_navigation_command_flown_and_any_repeat_keeps_the_warning() {
        let speed = with(178, [Some(0.0), Some(18.0), Some(-1.0), None]);
        let mut plan = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 40.0, 3),
            item(17, 0.0002, 0.002, 40.0, 3),
            speed.clone(),
        ]);
        plan.vehicle_type = PlanVehicleType::Plane;
        // An action after Loiter unlimited runs as it starts: the loiter
        // never ends, so the plane does not return.
        let holds = said(
            Severity::Note,
            "ends-loitering",
            3,
            "it ends loitering at item 3 until you take over",
        );
        assert_eq!(end(&plan), holds);
        // A waypoint after it is never reached.
        plan.items[3] = item(16, 0.0003, 0.003, 40.0, 3);
        assert_eq!(end(&plan), holds);
        // After a waypoint, it warns about the waypoint.
        plan.items[2].command = 16;
        plan.items[3] = speed;
        assert_eq!(
            end(&plan),
            said(Severity::Warning, "no-end", 3, STOPS_PLANE)
        );

        // A Repeat from item anywhere: whether and where it stops is the
        // firmware's to count, so the last item warns that, never a note,
        // promising neither a hold nor a return. Forever
        // (-1), a self-repeat ArduPilot's scan stops, -2, a number of times,
        // or to an item that is not there.
        plan.items[2].command = 17;
        for params in [
            [Some(2.0), Some(-1.0), None, None],
            [Some(4.0), Some(-1.0), None, None],
            [Some(2.0), Some(-2.0), None, None],
            [Some(2.0), Some(3.0), None, None],
            [Some(9.0), Some(-1.0), None, None],
        ] {
            plan.items[3] = with(JUMP, params);
            assert_eq!(
                end(&plan),
                said(Severity::Warning, "no-end", 4, UNSURE),
                "{params:?}"
            );
        }
        // Forward then back again, past a timed loiter: the same.
        let mut round = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            with(JUMP, [Some(4.0), Some(1.0), None, None]),
            item(19, 0.0001, 0.001, 40.0, 3),
            item(16, 0.0002, 0.002, 40.0, 3),
            with(JUMP, [Some(3.0), Some(1.0), None, None]),
        ]);
        round.vehicle_type = PlanVehicleType::Plane;
        assert_eq!(end(&round), said(Severity::Warning, "no-end", 5, UNSURE));
        // Repeat from tag, and PX4's repeats, the same.
        round.items[4] = with(JUMP_TAG, [Some(7.0), Some(-1.0), None, None]);
        round.items[1] = with(TAG, [Some(7.0), None, None, None]);
        assert_eq!(end(&round), said(Severity::Warning, "no-end", 5, UNSURE));
        round.autopilot = PlanAutopilot::Px4;
        round.items[4] = with(JUMP, [Some(2.0), Some(-1.0), None, None]);
        assert_eq!(end(&round), said(Severity::Warning, "no-end", 5, UNSURE));
        // A landing last may never be reached: it warns the same.
        round.autopilot = PlanAutopilot::Ardupilot;
        round.items.push(item(21, 0.0003, 0.003, 0.0, 3));
        assert_eq!(end(&round), said(Severity::Warning, "no-end", 6, UNSURE));

        // PX4 goes on by itself only from an item set to. One before the
        // loiter that waits to be told means the loiter is not reached by
        // itself: no note, but the warning as for any other end.
        let mut waits = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 40.0, 3),
            item(17, 0.0002, 0.002, 40.0, 3),
        ]);
        waits.autopilot = PlanAutopilot::Px4;
        assert_eq!(end(&waits)[0].1, "ends-loitering");
        waits.items[1].autocontinue = false;
        assert_eq!(end(&waits), said(Severity::Warning, "no-end", 3, STOPS));
        waits.items[2].command = 19;
        assert_eq!(end(&waits), said(Severity::Warning, "no-end", 3, STOPS));
        // ArduPilot goes on whatever the item says.
        waits.autopilot = PlanAutopilot::Ardupilot;
        assert_eq!(end(&waits)[0].1, "ends-loitering");
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one case after another")]
    fn what_a_warning_is_about_changes_with_its_subject_its_place_and_the_vehicle() {
        let mut plan = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 40.0, 3),
            item(16, 0.0002, 0.002, 40.0, 3),
        ]);
        plan.vehicle_type = PlanVehicleType::Plane;
        let about_end = |plan: &PlanBody| {
            let checks = check(plan, &mut hill);
            about(plan, checks.iter().find(|c| c.code == "no-end").unwrap())
        };
        let first = about_end(&plan);
        assert_eq!(first.len(), 64);
        plan.items[1].altitude_m = 45.0;
        assert_eq!(about_end(&plan), first, "another item changed");
        plan.items[1].altitude_m = 40.0;
        plan.items[2].altitude_m = 45.0;
        assert_ne!(about_end(&plan), first, "its item changed");
        plan.items[2].altitude_m = 40.0;
        assert_eq!(about_end(&plan), first);
        // Replaced by an identical item: the same.
        plan.items[2] = item(16, 0.0002, 0.002, 40.0, 3);
        assert_eq!(about_end(&plan), first);
        // Another vehicle type, or autopilot.
        plan.vehicle_type = PlanVehicleType::Vtol;
        assert_ne!(about_end(&plan), first, "a VTOL");
        plan.vehicle_type = PlanVehicleType::Plane;
        plan.autopilot = PlanAutopilot::Px4;
        assert_ne!(about_end(&plan), first, "PX4");
        plan.autopilot = PlanAutopilot::Ardupilot;
        assert_eq!(about_end(&plan), first);
        // An item inserted before it, or one before it removed.
        let mut inserted = plan.clone();
        inserted.items.insert(1, item(16, 0.00005, 0.0005, 40.0, 3));
        assert_ne!(about_end(&inserted), first, "inserted");
        let mut removed = plan.clone();
        removed.items.remove(1);
        assert_ne!(about_end(&removed), first, "removed");
        // An action after it, or a repeat anywhere.
        let mut after = plan.clone();
        after
            .items
            .push(with(178, [Some(0.0), Some(18.0), Some(-1.0), None]));
        assert_ne!(about_end(&after), first, "an action after it");
        let mut repeat = plan.clone();
        repeat
            .items
            .insert(2, with(JUMP, [Some(2.0), Some(1.0), None, None]));
        assert_ne!(about_end(&repeat), first, "a repeat");
        // With a repeat, the end can be any item: changing one before an
        // action at the end, or the order, makes it another.
        repeat
            .items
            .push(with(178, [Some(0.0), Some(18.0), Some(-1.0), None]));
        let repeated = about_end(&repeat);
        let mut changed = repeat.clone();
        changed.items[3].command = 19;
        assert_ne!(about_end(&changed), repeated, "an item before the action");
        let mut swapped = repeat.clone();
        swapped.items.swap(1, 3);
        assert_ne!(about_end(&swapped), repeated, "the order");

        // An item's warning moves with its item.
        let high = |plan: &PlanBody, n: u32| {
            about(
                plan,
                &Check {
                    severity: Severity::Warning,
                    code: "high",
                    item: Some(n),
                    message: format!("more than 120 m over the ground at item {n}"),
                },
            )
        };
        let mut three = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 130.0, 3),
            item(16, 0.0002, 0.002, 40.0, 3),
        ]);
        let was = high(&three, 2);
        three.items[2].altitude_m = 50.0;
        assert_eq!(high(&three, 2), was, "another item changed");
        // Reordered: another item is item 2.
        three.items.swap(1, 2);
        assert_ne!(high(&three, 2), was, "reordered");
        three.items.swap(1, 2);
        assert_eq!(high(&three, 2), was);
        three.items[1] = item(16, 0.0001, 0.001, 130.0, 3);
        assert_eq!(high(&three, 2), was, "an identical item");

        // A rally point's warning, with no item: the rally points and fence.
        let rally = |plan: &PlanBody| {
            about(
                plan,
                &Check {
                    severity: Severity::Warning,
                    code: "rally-outside-fence",
                    item: None,
                    message: "rally point 1 is outside the fence".into(),
                },
            )
        };
        let was = rally(&three);
        three.items[1].altitude_m = 60.0;
        assert_eq!(rally(&three), was, "a mission item changed");
        three.rally.push(uxduck_core::domain::plan::RallyPoint {
            latitude_deg: 0.1,
            longitude_deg: 0.1,
            altitude_m: 30.0,
            name: None,
        });
        assert_ne!(rally(&three), was, "a rally point changed");
    }

    #[test]
    fn a_leg_into_a_hill_above_home_is_below_the_ground_and_above_terrain_is_not() {
        let mut plan = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 40.0, 3),
            item(16, 0.0001, 0.008, 40.0, 3),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        let checks = check(&plan, &mut hill);
        assert_eq!(codes(&checks), ["below-ground"], "{checks:?}");
        // First found where the leg meets the hill, before item 3.
        assert_eq!(checks[0].message, "40 m below the ground along a leg");
        for item in &mut plan.items {
            item.frame = 10;
        }
        plan.frame = PlanFrame::AboveTerrain;
        assert!(check(&plan, &mut hill).is_empty());
    }

    /// A 20 km leg is sampled every 50 m too, so a ridge 20 m wide on it is
    /// found. Until 2026-11-17 its samples were 100 m apart and missed it.
    #[test]
    fn a_narrow_ridge_on_a_long_leg_is_found() {
        #[allow(clippy::option_option, clippy::unnecessary_wraps)]
        fn ridge(at: LatLon) -> Option<Option<f64>> {
            Some(Some(if (0.00136..0.00154).contains(&at.longitude_deg) {
                300.0
            } else {
                100.0
            }))
        }
        let plan = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 40.0, 3),
            item(16, 0.0001, 0.181, 40.0, 3),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        let checks = check(&plan, &mut ridge);
        assert_eq!(codes(&checks), ["below-ground"], "{checks:?}");
    }

    #[test]
    fn a_look_at_between_waypoints_is_not_a_leg() {
        for look_at in [195, 201] {
            let plan = body(vec![
                item(22, 0.0, 0.0, 40.0, 3),
                item(16, 0.0001, 0.001, 150.0, 0),
                item(look_at, 0.0001, 0.002, 0.0, 3),
                item(16, 0.0001, 0.003, 40.0, 10),
                item(20, 0.0, 0.0, 0.0, 3),
            ]);
            let checks = check(&plan, &mut hill);
            assert!(checks.is_empty(), "{look_at}: {checks:?}");
        }
    }

    #[test]
    fn a_leg_between_frames_is_flown_above_sea_level_into_the_hill() {
        // 140 m above sea level to 20 m over the 180 m hill: the straight
        // line meets the hill at 166 m, in either order of frames.
        for (from, to) in [
            (
                item(16, 0.0001, 0.001, 40.0, 3),
                item(16, 0.0001, 0.008, 200.0, 0),
            ),
            (
                item(16, 0.0001, 0.001, 40.0, 10),
                item(16, 0.0001, 0.008, 100.0, 3),
            ),
            (
                item(16, 0.0001, 0.001, 140.0, 0),
                item(16, 0.0001, 0.008, 100.0, 3),
            ),
        ] {
            let plan = body(vec![
                item(22, 0.0, 0.0, 40.0, 3),
                from.clone(),
                to.clone(),
                item(20, 0.0, 0.0, 0.0, 3),
            ]);
            let checks = check(&plan, &mut hill);
            assert_eq!(
                checks[0].message, "14 m below the ground along a leg",
                "{} to {}: {checks:?}",
                from.frame, to.frame
            );
            assert_eq!(checks[0].severity, Severity::Error);
        }
        // ArduPilot follows the ground on a leg to a point above terrain. PX4
        // gets that point above sea level and flies the straight line.
        let mut plan = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 40.0, 3),
            item(16, 0.0001, 0.008, 40.0, 10),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        assert!(check(&plan, &mut hill).is_empty());
        plan.autopilot = PlanAutopilot::Px4;
        assert_eq!(codes(&check(&plan, &mut hill))[0], "below-ground");
    }

    #[test]
    fn a_leg_between_above_home_and_above_sea_level_at_the_same_height_is_level() {
        // Home's ground is 100 m: 40 m above home is 140 m above sea level.
        let level = [
            (
                item(16, 0.0001, 0.001, 40.0, 3),
                item(16, 0.0001, 0.003, 140.0, 0),
            ),
            (
                item(16, 0.0001, 0.001, 140.0, 0),
                item(16, 0.0001, 0.003, 40.0, 3),
            ),
        ];
        for (from, to) in level {
            let plan = body(vec![
                item(22, 0.0, 0.0, 40.0, 3),
                from,
                to,
                item(20, 0.0, 0.0, 0.0, 3),
            ]);
            let checks = check(&plan, &mut hill);
            assert!(checks.is_empty(), "{checks:?}");
        }
    }

    #[test]
    fn heights_without_home_or_ground_stay_missing() {
        let mut plan = body(vec![
            item(22, 0.0, 0.0, 40.0, 3),
            item(16, 0.0001, 0.001, 40.0, 3),
            item(16, 0.0001, 0.003, 40.0, 3),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        plan.planned_home = None;
        assert_eq!(codes(&check(&plan, &mut hill)), ["terrain-missing"]);
        // Between frames, the end above home is just as unknown.
        plan.items[1] = item(16, 0.0001, 0.001, 140.0, 0);
        let checks = check(&plan, &mut hill);
        assert_eq!(codes(&checks), ["terrain-missing"], "{checks:?}");
        assert_eq!(checks[0].severity, Severity::Warning);
        plan.frame = PlanFrame::AboveTerrain;
        assert_eq!(check(&plan, &mut hill)[0].severity, Severity::Error);
        let mut nothing = |_: LatLon| None;
        assert_eq!(codes(&check(&plan, &mut nothing)), ["terrain-missing"]);
    }

    #[test]
    fn low_legs_warn_and_unknown_ground_is_missing() {
        let plan = body(vec![
            item(22, 0.0, 0.0, 5.0, 3),
            item(16, 0.0001, 0.001, 5.0, 3),
            item(21, 0.0001, 0.002, 0.0, 3),
        ]);
        let checks = check(&plan, &mut hill);
        assert_eq!(codes(&checks), ["low-clearance"]);
        let mut nothing = |_: LatLon| None;
        assert_eq!(codes(&check(&plan, &mut nothing)), ["terrain-missing"]);
    }

    #[test]
    fn normal_landings_do_not_warn_but_approach_terrain_and_buried_touchdown_still_fail() {
        for command in [21, 85] {
            let mut plan = body(vec![
                item(22, 0.0, 0.0, 40.0, 3),
                item(16, 0.0001, 0.001, 40.0, 3),
                item(command, 0.0001, 0.003, 0.0, 3),
            ]);
            if command == 85 {
                plan.vehicle_type = PlanVehicleType::Vtol;
                plan.items[0].command = 84;
            }
            let mut flat = |_: LatLon| Some(Some(100.0));
            assert!(check(&plan, &mut flat).is_empty(), "{command}");
            // A hill under the approach cannot hide behind the landing exception.
            let mut approach_hill = |at: LatLon| {
                Some(Some(
                    if at.longitude_deg > 0.0015 && at.longitude_deg < 0.0025 {
                        180.0
                    } else {
                        100.0
                    },
                ))
            };
            assert!(codes(&check(&plan, &mut approach_hill)).contains(&"below-ground"));
            plan.items[2].altitude_m = -5.0;
            assert!(codes(&check(&plan, &mut flat)).contains(&"below-ground"));
        }
    }

    #[test]
    fn a_landing_before_the_rest_warns_unless_it_takes_off_again() {
        let early = body(vec![
            item(22, 0.0, 0.0, 30.0, 3),
            item(21, 0.0001, 0.001, 0.0, 3),
            item(16, 0.0001, 0.002, 30.0, 3),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        let checks = check(&early, &mut |_| Some(Some(100.0)));
        assert_eq!(codes(&checks)[0], "lands-early", "{checks:?}");
        assert_eq!(
            checks[0].message,
            "item 2 lands the vehicle: items 3 to 4 after it are never reached"
        );
        assert_eq!(checks[0].item, Some(2));
        let again = body(vec![
            item(22, 0.0, 0.0, 30.0, 3),
            item(21, 0.0001, 0.001, 0.0, 3),
            item(22, 0.0, 0.0, 30.0, 3),
            item(16, 0.0001, 0.002, 30.0, 3),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        assert!(!codes(&check(&again, &mut |_| Some(Some(100.0)))).contains(&"lands-early"));
    }

    #[test]
    fn the_fence_holds_home_and_every_point() {
        let mut plan = body(vec![
            item(22, 0.0, 0.0, 30.0, 3),
            item(16, 0.0001, 0.001, 30.0, 3),
            item(16, 0.0001, 0.02, 30.0, 3),
            item(20, 0.0, 0.0, 0.0, 3),
        ]);
        let square = |d: f64| {
            vec![
                LatLon {
                    latitude_deg: -d,
                    longitude_deg: -d,
                },
                LatLon {
                    latitude_deg: -d,
                    longitude_deg: d,
                },
                LatLon {
                    latitude_deg: d,
                    longitude_deg: d,
                },
                LatLon {
                    latitude_deg: d,
                    longitude_deg: -d,
                },
            ]
        };
        plan.fence.polygons.push(FencePolygon {
            inclusion: true,
            points: square(0.01),
            name: None,
        });
        let mut flat = |_: LatLon| Some(Some(100.0));
        let checks = check(&plan, &mut flat);
        assert_eq!(codes(&checks), ["outside-fence"]);
        assert_eq!(checks[0].item, Some(3));
        // Crossed corners make no fence.
        plan.fence.polygons[0].points.swap(1, 2);
        assert!(codes(&check(&plan, &mut flat)).contains(&"fence-shape"));
        // On PX4, a spline waypoint is a command it does not support.
        let mut px4 = plan.clone();
        px4.autopilot = PlanAutopilot::Px4;
        px4.items[1].command = 82;
        let px4_checks = check(&px4, &mut flat);
        let unfit: Vec<&Check> = px4_checks
            .iter()
            .filter(|c| c.code == "command-unfit")
            .collect();
        assert_eq!(unfit.len(), 1);
        assert_eq!(unfit[0].message, "item 2 is a command PX4 does not support");
        // A 25 m ceiling under the 30 m waypoints: warned, not refused.
        plan.fence.polygons[0].points.swap(1, 2);
        plan.fence.settings.ceiling_m = Some(25.0);
        let checks = check(&plan, &mut flat);
        let above: Vec<&Check> = checks
            .iter()
            .filter(|c| c.code == "above-ceiling")
            .collect();
        assert_eq!(above.len(), 2, "{checks:?}");
        assert_eq!(above[0].severity, Severity::Warning);
        assert_eq!(
            above[0].message,
            "item 2 is at 30 m, above the fence's 25 m ceiling: the fence acts when it gets there"
        );
    }

    #[test]
    fn a_multicopter_wants_a_take_off_and_an_end_and_cannot_fly_a_plane_s_landing() {
        let plan = body(vec![
            item(16, 0.0001, 0.001, 30.0, 3),
            item(189, 0.0001, 0.002, 30.0, 3),
        ]);
        let mut flat = |_: LatLon| Some(Some(100.0));
        let checks = check(&plan, &mut flat);
        assert_eq!(codes(&checks), ["command-unfit", "no-take-off"]);
        assert_eq!(checks[0].severity, Severity::Error);
    }
}
