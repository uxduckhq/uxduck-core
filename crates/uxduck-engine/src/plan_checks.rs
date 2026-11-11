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
/// Samples along a leg, at most this far apart.
const LEG_STEP_M: f64 = 50.0;

const TAKE_OFF: [u16; 2] = [22, 84];
const ENDS: [u16; 4] = [20, 21, 85, 189];
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
        if let Some(last) = body.items.last()
            && body.vehicle_type != PlanVehicleType::Blimp
            && !ENDS.contains(&last.command)
        {
            checks.push(warning(
                "no-end",
                number(body.items.len() - 1),
                "it ends without a landing or a return: the vehicle waits at the last point".into(),
            ));
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
    checks.sort_by_key(|c| c.severity == Severity::Warning);
    checks
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
        let pieces = (length / LEG_STEP_M).ceil().min(200.0) as u32;
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
