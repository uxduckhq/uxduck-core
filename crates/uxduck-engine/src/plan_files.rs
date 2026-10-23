//! Plans in and out of files (ADR-0029 P2): QGroundControl's `.plan`,
//! the `QGC WPL 110` text Mission Planner and QGroundControl both write
//! (waypoints, fence and rally items), ArduPilot's older fence and rally
//! text, and KML polygons and lines. Formats are recognised by their
//! contents, not their names. Nothing that could not be read is dropped
//! silently: each is a line in [`Imported::skipped`].

use serde_json::{Value, json};
use uxduck_core::domain::plan::{
    Fence, FenceCircle, FencePolygon, LatLon, PatternShape, PlanAutopilot, PlanBody, PlanFrame,
    PlanItem, PlanPattern, PlanVehicleType, PlannedHome, RallyPoint, command_has_position,
};

/// One file as given: its name (for the plan's name and messages) and text.
#[derive(Clone, Debug)]
pub struct PlanFile {
    pub name: String,
    pub text: String,
}

/// Files read into one plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Imported {
    pub body: PlanBody,
    /// Whether the files said what the plan is for; if not, the caller's
    /// default (the selected vehicle's type) applies.
    pub vehicle_type_known: bool,
    /// What could not be read, one line each, naming the file.
    pub skipped: Vec<String>,
}

const MAV_CMD_NAV_FENCE_RETURN_POINT: u16 = 5000;
const MAV_CMD_NAV_FENCE_POLYGON_VERTEX_INCLUSION: u16 = 5001;
const MAV_CMD_NAV_FENCE_POLYGON_VERTEX_EXCLUSION: u16 = 5002;
const MAV_CMD_NAV_FENCE_CIRCLE_INCLUSION: u16 = 5003;
const MAV_CMD_NAV_FENCE_CIRCLE_EXCLUSION: u16 = 5004;
const MAV_CMD_NAV_RALLY_POINT: u16 = 5100;

/// Reads several files into one plan: a mission, a fence and rally points
/// may come in separate files (Mission Planner) or one (`.plan`).
///
/// # Errors
///
/// A file that is none of the formats above, or no files at all, with a
/// sentence naming the file.
pub fn import(files: &[PlanFile], default_type: PlanVehicleType) -> Result<Imported, String> {
    if files.is_empty() {
        return Err("No files to open.".into());
    }
    let mut parts = Parts::default();
    for file in files {
        read_file(file, &mut parts)?;
    }
    let vehicle_type_known = parts.vehicle_type.is_some();
    let vehicle_type = parts.vehicle_type.unwrap_or(default_type);
    let name = files
        .iter()
        .find(|file| !parts.items.is_empty() && file.name == parts.items_from)
        .unwrap_or(&files[0])
        .name
        .clone();
    let mut body = PlanBody::new(
        stem(&name),
        vehicle_type,
        parts.autopilot.unwrap_or_default(),
    );
    // The plan's frame is the one most positioned items use; if they mix,
    // each keeps its own (P4).
    if let Some(frame) = majority_frame(&parts.items) {
        body.frame = frame;
    }
    if let Some(altitude) = parts
        .items
        .iter()
        .rev()
        .find(|item| item.command == 16 && item.altitude_m > 0.0)
        .map(|item| item.altitude_m)
    {
        body.next_altitude_m = altitude;
    }
    // Lines from KML carry no usable height: they fly at the next height.
    let next = body.next_altitude_m;
    for item in &mut parts.items {
        if item.altitude_m.is_nan() {
            item.altitude_m = next;
        }
    }
    body.items = parts.items;
    body.patterns = parts.patterns;
    body.fence = parts.fence;
    body.rally = parts.rally;
    body.planned_home = parts.home;
    crate::plan_patterns::expand(&mut body);
    Ok(Imported {
        body,
        vehicle_type_known,
        skipped: parts.skipped,
    })
}

#[derive(Default)]
struct Parts {
    items: Vec<PlanItem>,
    items_from: String,
    fence: Fence,
    rally: Vec<RallyPoint>,
    home: Option<PlannedHome>,
    vehicle_type: Option<PlanVehicleType>,
    autopilot: Option<PlanAutopilot>,
    patterns: Vec<PlanPattern>,
    skipped: Vec<String>,
}

fn read_file(file: &PlanFile, parts: &mut Parts) -> Result<(), String> {
    let text = file.text.trim_start_matches('\u{feff}');
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| format!("{} is not a plan file: {error}.", file.name))?;
        if value.get("fileType").and_then(Value::as_str) != Some("Plan") {
            return Err(format!(
                "{} is JSON but not a QGroundControl plan.",
                file.name
            ));
        }
        read_qgc_plan(&file.name, &value, parts);
        Ok(())
    } else if trimmed.starts_with("QGC WPL") {
        read_wpl(&file.name, text, parts);
        Ok(())
    } else if trimmed.starts_with('<') {
        read_kml(&file.name, text, parts)
    } else if trimmed
        .lines()
        .any(|line| line.trim_start().to_ascii_uppercase().starts_with("RALLY"))
    {
        read_rally_text(&file.name, text, parts);
        Ok(())
    } else if trimmed
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .all(|line| parse_lat_lon(line).is_some())
        && !trimmed.is_empty()
    {
        read_fence_text(&file.name, text, parts)
    } else if file.name.to_ascii_lowercase().ends_with(".kmz") {
        Err(format!(
            "{} is a KMZ file: unzip it and open the .kml inside.",
            file.name
        ))
    } else {
        Err(format!(
            "{} is not a plan, waypoint, fence, rally or KML file UXDuck can read.",
            file.name
        ))
    }
}

/// A QGroundControl survey or corridor scan as a pattern (P11): its area or
/// line, angle, turn-around and height; the lane spacing is the camera's
/// side footprint, which is the spacing itself for a manual camera.
fn qgc_pattern(item: &Value, patterns: &mut Vec<PlanPattern>) -> Option<()> {
    let transect = item.get("TransectStyleComplexItem")?;
    let camera = transect.get("CameraCalc");
    let spacing_m = camera
        .and_then(|c| c.get("AdjustedFootprintSide"))
        .and_then(Value::as_f64)
        .filter(|s| *s > 0.0)
        .unwrap_or(20.0);
    let altitude_m = camera
        .and_then(|c| c.get("DistanceToSurface"))
        .and_then(Value::as_f64)
        .unwrap_or(50.0);
    let turnaround_m = transect
        .get("TurnAroundDistance")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let points = |key: &str| -> Vec<LatLon> {
        item.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let p = p.as_array()?;
                Some(LatLon {
                    latitude_deg: number(p.first())?,
                    longitude_deg: number(p.get(1))?,
                })
            })
            .collect()
    };
    let (kind, shape) = match item.get("complexItemType").and_then(Value::as_str)? {
        "survey" => {
            let area = points("polygon");
            if area.len() < 3 {
                return None;
            }
            (
                "survey",
                PatternShape::Survey {
                    area,
                    spacing_m,
                    angle_deg: item.get("angle").and_then(Value::as_f64).unwrap_or(0.0),
                    turnaround_m,
                    altitude_m,
                    crosshatch: transect
                        .get("Refly90Degrees")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
            )
        }
        "CorridorScan" => {
            let line = points("polyline");
            if line.len() < 2 {
                return None;
            }
            (
                "corridor",
                PatternShape::Corridor {
                    line,
                    width_m: item
                        .get("CorridorWidth")
                        .and_then(Value::as_f64)
                        .unwrap_or(spacing_m),
                    spacing_m,
                    turnaround_m,
                    altitude_m,
                },
            )
        }
        _ => return None,
    };
    let id = (1..=patterns.len() + 1)
        .map(|n| format!("{kind}-{n}"))
        .find(|id| patterns.iter().all(|p| &p.id != id))?;
    patterns.push(PlanPattern { id, shape });
    Some(())
}

/// The frame a QGroundControl pattern's heights are in.
fn qgc_pattern_frame(item: &Value) -> u8 {
    let transect = item.get("TransectStyleComplexItem");
    if transect
        .and_then(|t| t.get("FollowTerrain"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return PlanFrame::AboveTerrain.mav_frame();
    }
    let relative = transect
        .and_then(|t| t.get("CameraCalc"))
        .and_then(|c| c.get("DistanceToSurfaceRelative"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if relative {
        PlanFrame::AboveHome.mav_frame()
    } else {
        PlanFrame::AboveSeaLevel.mav_frame()
    }
}

fn stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match base.rfind('.') {
        Some(dot) if dot > 0 => &base[..dot],
        _ => base,
    };
    if stem.trim().is_empty() {
        "Imported plan".into()
    } else {
        stem.trim().to_string()
    }
}

fn majority_frame(items: &[PlanItem]) -> Option<PlanFrame> {
    let mut counts = [0usize; 3];
    for item in items
        .iter()
        .filter(|item| command_has_position(item.command))
    {
        match PlanFrame::from_mav_frame(item.frame) {
            Some(PlanFrame::AboveSeaLevel) => counts[0] += 1,
            Some(PlanFrame::AboveHome) => counts[1] += 1,
            Some(PlanFrame::AboveTerrain) => counts[2] += 1,
            None => {}
        }
    }
    let best = counts.iter().copied().max()?;
    if best == 0 {
        return None;
    }
    // Ties go to above home, then above terrain: the common cases.
    [1usize, 2, 0]
        .into_iter()
        .find(|&i| counts[i] == best)
        .map(|i| match i {
            0 => PlanFrame::AboveSeaLevel,
            1 => PlanFrame::AboveHome,
            _ => PlanFrame::AboveTerrain,
        })
}

/// `MAV_TYPE` to a plan's vehicle type.
fn vehicle_type_from_mav_type(mav_type: i64) -> Option<PlanVehicleType> {
    Some(match mav_type {
        1 => PlanVehicleType::Plane,
        2 | 3 | 4 | 13 | 14 | 15 => PlanVehicleType::Multicopter,
        10 => PlanVehicleType::Rover,
        11 => PlanVehicleType::Boat,
        12 => PlanVehicleType::Submarine,
        7 => PlanVehicleType::Blimp,
        19..=22 => PlanVehicleType::Vtol,
        _ => return None,
    })
}

fn mav_type_for(vehicle_type: PlanVehicleType) -> i64 {
    match vehicle_type {
        PlanVehicleType::Multicopter => 2,
        PlanVehicleType::Plane => 1,
        PlanVehicleType::Vtol => 20,
        PlanVehicleType::Rover => 10,
        PlanVehicleType::Boat => 11,
        PlanVehicleType::Submarine => 12,
        PlanVehicleType::Blimp => 7,
    }
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

#[allow(
    clippy::too_many_lines,
    reason = "one format's three parts, read in order"
)]
fn read_qgc_plan(name: &str, plan: &Value, parts: &mut Parts) {
    if let Some(mission) = plan.get("mission") {
        if let Some(t) = mission.get("vehicleType").and_then(Value::as_i64) {
            parts.vehicle_type = parts.vehicle_type.or(vehicle_type_from_mav_type(t));
        }
        match mission.get("firmwareType").and_then(Value::as_i64) {
            Some(3) => parts.autopilot = Some(PlanAutopilot::Ardupilot),
            Some(12) => parts.autopilot = Some(PlanAutopilot::Px4),
            _ => {}
        }
        if let Some(home) = mission.get("plannedHomePosition").and_then(Value::as_array)
            && let (Some(lat), Some(lon)) = (number(home.first()), number(home.get(1)))
            && !(lat == 0.0 && lon == 0.0)
        {
            parts.home = Some(PlannedHome {
                latitude_deg: lat,
                longitude_deg: lon,
                altitude_m: number(home.get(2)),
            });
        }
        let items = mission
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut read = Vec::new();
        for (index, item) in items.iter().enumerate() {
            match item.get("type").and_then(Value::as_str) {
                Some("SimpleItem") => match qgc_simple_item(item) {
                    Some(item) => read.push(item),
                    None => parts
                        .skipped
                        .push(format!("{name}: item {} could not be read.", index + 1)),
                },
                Some("ComplexItem") if qgc_pattern(item, &mut parts.patterns).is_some() => {
                    // Its place among the items: made from the pattern on save.
                    let Some(pattern) = parts.patterns.last() else {
                        continue;
                    };
                    read.push(PlanItem {
                        command: 16,
                        frame: qgc_pattern_frame(item),
                        params: [None; 4],
                        latitude_deg: 0.0,
                        longitude_deg: 0.0,
                        altitude_m: 0.0,
                        autocontinue: true,
                        pattern: Some(pattern.id.clone()),
                    });
                }
                Some("ComplexItem") => {
                    let kind = item
                        .get("complexItemType")
                        .and_then(Value::as_str)
                        .unwrap_or("pattern");
                    let generated = item
                        .get("TransectStyleComplexItem")
                        .and_then(|t| t.get("Items"))
                        .and_then(Value::as_array);
                    match generated {
                        Some(list) => {
                            let simple: Vec<PlanItem> =
                                list.iter().filter_map(qgc_simple_item).collect();
                            parts.skipped.push(format!(
                                "{name}: the {kind} became its {} items. It cannot be changed as a {kind} yet.",
                                simple.len()
                            ));
                            read.extend(simple);
                        }
                        None => parts
                            .skipped
                            .push(format!("{name}: a {kind} could not be read.")),
                    }
                }
                _ => parts
                    .skipped
                    .push(format!("{name}: item {} could not be read.", index + 1)),
            }
        }
        if !read.is_empty() {
            parts.items_from = name.to_string();
            parts.items.extend(read);
        }
    }
    if let Some(fence) = plan.get("geoFence") {
        for polygon in fence
            .get("polygons")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let points: Vec<LatLon> = polygon
                .get("polygon")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some(LatLon {
                        latitude_deg: number(p.first())?,
                        longitude_deg: number(p.get(1))?,
                    })
                })
                .collect();
            if points.len() < 3 {
                parts.skipped.push(format!(
                    "{name}: a fence polygon with fewer than three corners."
                ));
                continue;
            }
            let inclusion = polygon
                .get("inclusion")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            parts.fence.polygons.push(FencePolygon {
                inclusion,
                points,
                name: None,
            });
        }
        for circle in fence
            .get("circles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let inner = circle.get("circle").unwrap_or(circle);
            let center = inner.get("center").and_then(Value::as_array);
            let radius = number(inner.get("radius"));
            match (center, radius) {
                (Some(center), Some(radius_m)) if radius_m > 0.0 => {
                    if let (Some(lat), Some(lon)) = (number(center.first()), number(center.get(1)))
                    {
                        let inclusion = circle
                            .get("inclusion")
                            .and_then(Value::as_bool)
                            .unwrap_or(true);
                        parts.fence.circles.push(FenceCircle {
                            inclusion,
                            center: LatLon {
                                latitude_deg: lat,
                                longitude_deg: lon,
                            },
                            radius_m,
                            name: None,
                        });
                    }
                }
                _ => parts
                    .skipped
                    .push(format!("{name}: a fence circle could not be read.")),
            }
        }
        if let Some(point) = fence.get("breachReturn").and_then(Value::as_array)
            && let (Some(lat), Some(lon)) = (number(point.first()), number(point.get(1)))
            && !(lat == 0.0 && lon == 0.0)
        {
            parts.fence.return_point = Some(LatLon {
                latitude_deg: lat,
                longitude_deg: lon,
            });
        }
    }
    if let Some(rally) = plan.get("rallyPoints") {
        for point in rally
            .get("points")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(p) = point.as_array() else { continue };
            match (number(p.first()), number(p.get(1))) {
                (Some(lat), Some(lon)) => parts.rally.push(RallyPoint {
                    latitude_deg: lat,
                    longitude_deg: lon,
                    altitude_m: number(p.get(2)).unwrap_or(0.0),
                    name: None,
                }),
                _ => parts
                    .skipped
                    .push(format!("{name}: a rally point could not be read.")),
            }
        }
    }
}

fn qgc_simple_item(item: &Value) -> Option<PlanItem> {
    let command = u16::try_from(item.get("command")?.as_u64()?).ok()?;
    let frame = u8::try_from(item.get("frame").and_then(Value::as_u64).unwrap_or(3)).ok()?;
    let params = item.get("params")?.as_array()?;
    let param = |i: usize| number(params.get(i));
    Some(PlanItem {
        command,
        frame,
        params: [param(0), param(1), param(2), param(3)],
        latitude_deg: param(4).unwrap_or(0.0),
        longitude_deg: param(5).unwrap_or(0.0),
        altitude_m: param(6).unwrap_or(0.0),
        autocontinue: item
            .get("autoContinue")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        pattern: None,
    })
}

/// A `QGC WPL 110` row.
struct WplRow {
    index: usize,
    item: PlanItem,
}

fn parse_wpl_rows(name: &str, text: &str, skipped: &mut Vec<String>) -> Vec<WplRow> {
    let mut rows = Vec::new();
    for (line_number, line) in text.lines().enumerate().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
        let fields = if fields.len() >= 12 {
            fields
        } else {
            line.split_whitespace().collect()
        };
        let parsed = (|| {
            if fields.len() < 12 {
                return None;
            }
            let float = |i: usize| fields[i].parse::<f64>().ok();
            let param = |i: usize| float(i).filter(|n| n.is_finite());
            Some(WplRow {
                index: fields[0].parse().ok()?,
                item: PlanItem {
                    command: fields[3].parse().ok()?,
                    frame: fields[2].parse().ok()?,
                    params: [param(4), param(5), param(6), param(7)],
                    latitude_deg: float(8)?,
                    longitude_deg: float(9)?,
                    altitude_m: float(10)?,
                    autocontinue: fields[11] != "0",
                    pattern: None,
                },
            })
        })();
        match parsed {
            Some(row) => rows.push(row),
            None => skipped.push(format!(
                "{name}: line {} could not be read.",
                line_number + 1
            )),
        }
    }
    rows
}

fn read_wpl(name: &str, text: &str, parts: &mut Parts) {
    let rows = parse_wpl_rows(name, text, &mut parts.skipped);
    let is_fence =
        |c: u16| (MAV_CMD_NAV_FENCE_RETURN_POINT..=MAV_CMD_NAV_FENCE_CIRCLE_EXCLUSION).contains(&c);
    let mut mission = Vec::new();
    let mut fence_rows = Vec::new();
    for row in rows {
        if is_fence(row.item.command) {
            fence_rows.push(row.item);
        } else if row.item.command == MAV_CMD_NAV_RALLY_POINT {
            parts.rally.push(RallyPoint {
                latitude_deg: row.item.latitude_deg,
                longitude_deg: row.item.longitude_deg,
                altitude_m: row.item.altitude_m,
                name: None,
            });
        } else if row.index == 0 && row.item.command == 16 {
            // Mission Planner and QGroundControl write home as item 0.
            if !(row.item.latitude_deg == 0.0 && row.item.longitude_deg == 0.0) {
                parts.home = Some(PlannedHome {
                    latitude_deg: row.item.latitude_deg,
                    longitude_deg: row.item.longitude_deg,
                    altitude_m: Some(row.item.altitude_m),
                });
            }
        } else {
            mission.push(row.item);
        }
    }
    read_fence_items(name, &fence_rows, &mut parts.fence, &mut parts.skipped);
    if !mission.is_empty() {
        parts.items_from = name.to_string();
        parts.items.extend(mission);
    }
}

/// Fence items as MAVLink carries them: a polygon is `param1` vertices of
/// the same kind in a row; circles carry their radius in `param1`.
fn read_fence_items(name: &str, items: &[PlanItem], fence: &mut Fence, skipped: &mut Vec<String>) {
    let mut i = 0;
    while i < items.len() {
        let item = &items[i];
        let at = LatLon {
            latitude_deg: item.latitude_deg,
            longitude_deg: item.longitude_deg,
        };
        match item.command {
            MAV_CMD_NAV_FENCE_RETURN_POINT => {
                fence.return_point = Some(at);
                i += 1;
            }
            MAV_CMD_NAV_FENCE_CIRCLE_INCLUSION | MAV_CMD_NAV_FENCE_CIRCLE_EXCLUSION => {
                match item.params[0].filter(|r| *r > 0.0) {
                    Some(radius_m) => fence.circles.push(FenceCircle {
                        inclusion: item.command == MAV_CMD_NAV_FENCE_CIRCLE_INCLUSION,
                        center: at,
                        radius_m,
                        name: None,
                    }),
                    None => skipped.push(format!("{name}: a fence circle with no radius.")),
                }
                i += 1;
            }
            command => {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a count of corners, at least 0"
                )]
                let count = item.params[0].map_or(0, |n| n.clamp(0.0, 1e6) as usize);
                let vertices: Vec<&PlanItem> = items[i..]
                    .iter()
                    .take(count.max(1))
                    .take_while(|v| v.command == command)
                    .collect();
                if count < 3 || vertices.len() != count {
                    skipped.push(format!(
                        "{name}: a fence polygon whose corners do not add up."
                    ));
                    i += vertices.len().max(1);
                    continue;
                }
                fence.polygons.push(FencePolygon {
                    inclusion: command == MAV_CMD_NAV_FENCE_POLYGON_VERTEX_INCLUSION,
                    points: vertices
                        .iter()
                        .map(|v| LatLon {
                            latitude_deg: v.latitude_deg,
                            longitude_deg: v.longitude_deg,
                        })
                        .collect(),
                    name: None,
                });
                i += count;
            }
        }
    }
}

fn parse_lat_lon(line: &str) -> Option<(f64, f64)> {
    let mut fields = line.split_whitespace();
    let lat = fields.next()?.parse::<f64>().ok()?;
    let lon = fields.next()?.parse::<f64>().ok()?;
    (fields.next().is_none() && lat.abs() <= 90.0 && lon.abs() <= 180.0).then_some((lat, lon))
}

/// ArduPilot's older fence file: the return point, then the polygon, then
/// the first corner again to close it.
fn read_fence_text(name: &str, text: &str, parts: &mut Parts) -> Result<(), String> {
    let points: Vec<LatLon> = text
        .lines()
        .filter_map(parse_lat_lon)
        .map(|(lat, lon)| LatLon {
            latitude_deg: lat,
            longitude_deg: lon,
        })
        .collect();
    if points.len() < 4 {
        return Err(format!("{name} has too few points for a fence."));
    }
    let mut polygon = points[1..].to_vec();
    if polygon.len() > 3 && polygon.first() == polygon.last() {
        polygon.pop();
    }
    parts.fence.return_point = Some(points[0]);
    parts.fence.polygons.push(FencePolygon {
        inclusion: true,
        points: polygon,
        name: None,
    });
    Ok(())
}

/// Mission Planner's older rally file: `RALLY lat lon alt …` lines.
fn read_rally_text(name: &str, text: &str, parts: &mut Parts) {
    for (line_number, line) in text.lines().enumerate() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields
            .first()
            .is_none_or(|f| !f.eq_ignore_ascii_case("RALLY"))
        {
            continue;
        }
        let float = |i: usize| fields.get(i).and_then(|f| f.parse::<f64>().ok());
        match (float(1), float(2), float(3)) {
            (Some(lat), Some(lon), Some(alt)) => {
                parts.rally.push(RallyPoint {
                    latitude_deg: lat,
                    longitude_deg: lon,
                    altitude_m: alt,
                    name: None,
                });
            }
            _ => parts.skipped.push(format!(
                "{name}: line {} could not be read.",
                line_number + 1
            )),
        }
    }
}

/// KML: polygons become inclusion fences, lines become waypoints at the
/// plan's next height. Coordinates are `lon,lat[,alt]`.
fn read_kml(name: &str, text: &str, parts: &mut Parts) -> Result<(), String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(text);
    let mut stack: Vec<String> = Vec::new();
    let mut found = false;
    let mut content = String::new();
    // The placemark's own name, for the fence area it holds.
    let mut placemark: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => {
                let tag = tag.local_name().as_ref().to_owned();
                if tag == "Placemark" {
                    placemark = None;
                }
                if tag == "name" || tag == "coordinates" {
                    content.clear();
                }
                stack.push(tag);
            }
            Ok(Event::Text(raw)) => content.push_str(&raw.xml10_content()),
            Ok(Event::CData(raw)) => content.push_str(&raw.xml10_content()),
            Ok(Event::GeneralRef(raw)) => {
                let reference = format!("&{};", raw.as_ref());
                content.push_str(
                    &quick_xml::escape::unescape(&reference)
                        .map_err(|error| format!("{name} is not valid KML: {error}."))?,
                );
            }
            Ok(Event::End(_))
                if stack.last().is_some_and(|t| t == "name")
                    && stack.iter().rev().nth(1).is_some_and(|t| t == "Placemark") =>
            {
                let text = content.trim();
                placemark = (!text.is_empty()).then(|| text.to_string());
                content.clear();
                stack.pop();
            }
            Ok(Event::End(_)) if stack.last().is_some_and(|t| t == "coordinates") => {
                let raw = &content;
                let points = kml_points(raw);
                if stack.iter().any(|t| t == "innerBoundaryIs") {
                    parts
                        .skipped
                        .push(format!("{name}: a hole in a polygon was left out."));
                } else if stack.iter().any(|t| t == "Polygon") {
                    let mut points = points;
                    if points.len() > 3 && points.first() == points.last() {
                        points.pop();
                    }
                    if points.len() >= 3 {
                        parts.fence.polygons.push(FencePolygon {
                            inclusion: true,
                            points,
                            name: placemark.clone(),
                        });
                        found = true;
                    }
                } else if stack.iter().any(|t| t == "LineString") && points.len() >= 2 {
                    parts.items_from = name.to_string();
                    parts.items.extend(points.into_iter().map(|p| PlanItem {
                        command: 16,
                        frame: PlanFrame::AboveTerrain.mav_frame(),
                        params: [Some(0.0), Some(0.0), Some(0.0), None],
                        latitude_deg: p.latitude_deg,
                        longitude_deg: p.longitude_deg,
                        altitude_m: f64::NAN,
                        autocontinue: true,
                        pattern: None,
                    }));
                    found = true;
                } else if stack.iter().any(|t| t == "Point") {
                    parts
                        .skipped
                        .push(format!("{name}: a single point was left out."));
                }
                content.clear();
                stack.pop();
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(format!("{name} is not valid KML: {error}.")),
            _ => {}
        }
    }
    if !found {
        return Err(format!("{name} has no polygon or line UXDuck can use."));
    }
    Ok(())
}

fn kml_points(raw: &str) -> Vec<LatLon> {
    raw.split_whitespace()
        .filter_map(|tuple| {
            let mut fields = tuple.split(',');
            let lon = fields.next()?.parse::<f64>().ok()?;
            let lat = fields.next()?.parse::<f64>().ok()?;
            Some(LatLon {
                latitude_deg: lat,
                longitude_deg: lon,
            })
        })
        .collect()
}

/// A file to save.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportFile {
    pub name: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    /// One `.plan` with all three parts.
    Qgc,
    /// Mission Planner: `.waypoints`, and `.fen.waypoints`, `.rally.waypoints` when there is a fence or rally points.
    MissionPlanner,
}

fn file_safe(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "plan".into()
    } else {
        cleaned
    }
}

fn param_json(param: Option<f64>) -> Value {
    param.map_or(Value::Null, |p| json!(p))
}

#[must_use]
pub fn export(body: &PlanBody, format: ExportFormat) -> Vec<ExportFile> {
    let base = file_safe(&body.name);
    match format {
        ExportFormat::Qgc => vec![ExportFile {
            name: format!("{base}.plan"),
            text: qgc_plan(body),
        }],
        ExportFormat::MissionPlanner => {
            let mut files = vec![ExportFile {
                name: format!("{base}.waypoints"),
                text: wpl_mission(body),
            }];
            if !body.fence.is_empty() {
                files.push(ExportFile {
                    name: format!("{base}.fen.waypoints"),
                    text: wpl_fence(&body.fence),
                });
            }
            if !body.rally.is_empty() {
                files.push(ExportFile {
                    name: format!("{base}.rally.waypoints"),
                    text: wpl_rally(body),
                });
            }
            files
        }
    }
}

fn qgc_plan(body: &PlanBody) -> String {
    let home = body.planned_home.map_or_else(
        || {
            let first = body.points().first().copied().unwrap_or(LatLon {
                latitude_deg: 0.0,
                longitude_deg: 0.0,
            });
            json!([first.latitude_deg, first.longitude_deg, 0])
        },
        |h| json!([h.latitude_deg, h.longitude_deg, h.altitude_m.unwrap_or(0.0)]),
    );
    let items: Vec<Value> = body
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            json!({
                "type": "SimpleItem",
                "autoContinue": item.autocontinue,
                "command": item.command,
                "doJumpId": index + 1,
                "frame": item.frame,
                "params": [
                    param_json(item.params[0]), param_json(item.params[1]),
                    param_json(item.params[2]), param_json(item.params[3]),
                    item.latitude_deg, item.longitude_deg, item.altitude_m,
                ],
            })
        })
        .collect();
    let polygons: Vec<Value> = body
        .fence
        .polygons
        .iter()
        .map(|p| {
            json!({
                "inclusion": p.inclusion,
                "polygon": p.points.iter().map(|q| json!([q.latitude_deg, q.longitude_deg])).collect::<Vec<_>>(),
                "version": 1,
            })
        })
        .collect();
    let circles: Vec<Value> = body
        .fence
        .circles
        .iter()
        .map(|c| {
            json!({
                "circle": { "center": [c.center.latitude_deg, c.center.longitude_deg], "radius": c.radius_m },
                "inclusion": c.inclusion,
                "version": 1,
            })
        })
        .collect();
    let mut geo_fence = json!({ "circles": circles, "polygons": polygons, "version": 2 });
    if let Some(point) = body.fence.return_point {
        geo_fence["breachReturn"] = json!([point.latitude_deg, point.longitude_deg, 0]);
    }
    let plan = json!({
        "fileType": "Plan",
        "groundStation": "UXDuck",
        "version": 1,
        "mission": {
            "firmwareType": match body.autopilot { PlanAutopilot::Ardupilot => 3, PlanAutopilot::Px4 => 12 },
            "vehicleType": mav_type_for(body.vehicle_type),
            "globalPlanAltitudeMode": 1,
            "plannedHomePosition": home,
            "items": items,
            "version": 2,
        },
        "geoFence": geo_fence,
        "rallyPoints": {
            "points": body.rally.iter().map(|r| json!([r.latitude_deg, r.longitude_deg, r.altitude_m])).collect::<Vec<_>>(),
            "version": 2,
        },
    });
    serde_json::to_string_pretty(&plan).unwrap_or_default() + "\n"
}

fn wpl_row(index: usize, item: &PlanItem) -> String {
    let p = |i: usize| item.params[i].map_or_else(|| "nan".to_string(), |v| format!("{v:.8}"));
    format!(
        "{index}\t0\t{}\t{}\t{}\t{}\t{}\t{}\t{:.8}\t{:.8}\t{:.6}\t{}\n",
        item.frame,
        item.command,
        p(0),
        p(1),
        p(2),
        p(3),
        item.latitude_deg,
        item.longitude_deg,
        item.altitude_m,
        u8::from(item.autocontinue)
    )
}

fn wpl_mission(body: &PlanBody) -> String {
    let mut text = String::from("QGC WPL 110\n");
    let home = body.planned_home.map_or_else(
        || {
            body.points().first().copied().unwrap_or(LatLon {
                latitude_deg: 0.0,
                longitude_deg: 0.0,
            })
        },
        |h| LatLon {
            latitude_deg: h.latitude_deg,
            longitude_deg: h.longitude_deg,
        },
    );
    let home_alt = body.planned_home.and_then(|h| h.altitude_m).unwrap_or(0.0);
    let home_row = format!(
        "0\t1\t0\t16\t0\t0\t0\t0\t{:.8}\t{:.8}\t{home_alt:.6}\t1\n",
        home.latitude_deg, home.longitude_deg
    );
    text.push_str(&home_row);
    for (index, item) in body.items.iter().enumerate() {
        text += &wpl_row(index + 1, item);
    }
    text
}

/// A fence item: `MAV_FRAME_GLOBAL`, as QGroundControl sends them and
/// ArduPilot reads them back; a fence vertex has no height.
fn point_item(command: u16, at: LatLon, param1: f64) -> PlanItem {
    PlanItem {
        command,
        frame: 0,
        params: [Some(param1), Some(0.0), Some(0.0), Some(0.0)],
        latitude_deg: at.latitude_deg,
        longitude_deg: at.longitude_deg,
        altitude_m: 0.0,
        autocontinue: false,
        pattern: None,
    }
}

/// The fence as MAVLink fence items.
#[must_use]
pub fn fence_items(fence: &Fence) -> Vec<PlanItem> {
    let mut items = Vec::new();
    if let Some(point) = fence.return_point {
        items.push(point_item(MAV_CMD_NAV_FENCE_RETURN_POINT, point, 0.0));
    }
    for polygon in &fence.polygons {
        let command = if polygon.inclusion {
            MAV_CMD_NAV_FENCE_POLYGON_VERTEX_INCLUSION
        } else {
            MAV_CMD_NAV_FENCE_POLYGON_VERTEX_EXCLUSION
        };
        items.extend(polygon.points.iter().map(|p| {
            point_item(
                command,
                *p,
                f64::from(u32::try_from(polygon.points.len()).unwrap_or(u32::MAX)),
            )
        }));
    }
    for circle in &fence.circles {
        let command = if circle.inclusion {
            MAV_CMD_NAV_FENCE_CIRCLE_INCLUSION
        } else {
            MAV_CMD_NAV_FENCE_CIRCLE_EXCLUSION
        };
        items.push(point_item(command, circle.center, circle.radius_m));
    }
    items
}

fn wpl_fence(fence: &Fence) -> String {
    let mut text = String::from("QGC WPL 110\n");
    for (index, item) in fence_items(fence).iter().enumerate() {
        text += &wpl_row(index, item);
    }
    text
}

fn wpl_rally(body: &PlanBody) -> String {
    let mut text = String::from("QGC WPL 110\n");
    for (index, rally) in body.rally.iter().enumerate() {
        let item = PlanItem {
            command: MAV_CMD_NAV_RALLY_POINT,
            frame: body.frame.mav_frame(),
            params: [Some(0.0); 4],
            latitude_deg: rally.latitude_deg,
            longitude_deg: rally.longitude_deg,
            altitude_m: rally.altitude_m,
            autocontinue: false,
            pattern: None,
        };
        text += &wpl_row(index, &item);
    }
    text
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    clippy::unwrap_used,
    clippy::unreadable_literal,
    reason = "exact values from files in tests"
)]
mod tests {
    use super::*;

    fn file(name: &str, text: &str) -> PlanFile {
        PlanFile {
            name: name.into(),
            text: text.into(),
        }
    }

    const WPL: &str = "QGC WPL 110\n\
0\t1\t0\t16\t0\t0\t0\t0\t-35.363262\t149.165237\t584.000000\t1\n\
1\t0\t3\t22\t0.000000\t0.000000\t0.000000\t0.000000\t0.000000\t0.000000\t25.000000\t1\n\
2\t0\t10\t16\t0.000000\t0.000000\t0.000000\t0.000000\t-35.361\t149.163\t25.000000\t1\n\
3\t0\t10\t16\t0.000000\t0.000000\t0.000000\t0.000000\t-35.360\t149.164\t35.000000\t1\n\
4\t0\t3\t20\t0.000000\t0.000000\t0.000000\t0.000000\t0.000000\t0.000000\t0.000000\t1\n";

    #[test]
    fn mission_planner_waypoints_with_home() {
        let imported = import(&[file("Hill.waypoints", WPL)], PlanVehicleType::Plane).unwrap();
        let body = &imported.body;
        assert_eq!(body.name, "Hill");
        assert!(!imported.vehicle_type_known);
        assert_eq!(body.vehicle_type, PlanVehicleType::Plane);
        assert_eq!(body.items.len(), 4);
        assert_eq!(body.planned_home.unwrap().altitude_m, Some(584.0));
        assert_eq!(body.frame, PlanFrame::AboveTerrain);
        assert_eq!(body.next_altitude_m, 35.0);
        assert!(imported.skipped.is_empty());
    }

    const FENCE_WPL: &str = "QGC WPL 110\n\
0\t0\t3\t5000\t0\t0\t0\t0\t-35.36\t149.16\t0\t0\n\
1\t0\t3\t5001\t3\t0\t0\t0\t-35.35\t149.15\t0\t0\n\
2\t0\t3\t5001\t3\t0\t0\t0\t-35.37\t149.15\t0\t0\n\
3\t0\t3\t5001\t3\t0\t0\t0\t-35.37\t149.17\t0\t0\n\
4\t0\t3\t5004\t25\t0\t0\t0\t-35.36\t149.16\t0\t0\n";

    const RALLY_WPL: &str = "QGC WPL 110\n0\t0\t3\t5100\t0\t0\t0\t0\t-35.365\t149.161\t60\t0\n";

    #[test]
    fn three_mission_planner_files_become_one_plan() {
        let imported = import(
            &[
                file("Field.fen.waypoints", FENCE_WPL),
                file("Field.waypoints", WPL),
                file("Field.rally.waypoints", RALLY_WPL),
            ],
            PlanVehicleType::Multicopter,
        )
        .unwrap();
        let body = &imported.body;
        assert_eq!(body.name, "Field");
        assert_eq!(body.items.len(), 4);
        assert_eq!(body.fence.polygons.len(), 1);
        assert_eq!(body.fence.polygons[0].points.len(), 3);
        assert_eq!(body.fence.circles.len(), 1);
        assert!(!body.fence.circles[0].inclusion);
        assert!(body.fence.return_point.is_some());
        assert_eq!(body.rally.len(), 1);
    }

    #[test]
    fn ardupilot_fence_text_closes_its_polygon() {
        let text = "-35.363720\t149.163651\n-35.358738\t149.165070\n-35.359295\t149.154434\n-35.372292\t149.157135\n-35.368290\t149.166809\n-35.358738\t149.165070\n";
        let imported = import(&[file("CMAC-fence.txt", text)], PlanVehicleType::Plane).unwrap();
        assert_eq!(imported.body.fence.polygons[0].points.len(), 4);
        assert_eq!(
            imported.body.fence.return_point.unwrap().latitude_deg,
            -35.363720
        );
    }

    const QGC: &str = r#"{
  "fileType": "Plan", "groundStation": "QGroundControl", "version": 1,
  "geoFence": {"circles": [{"circle": {"center": [47.39, 8.54], "radius": 120}, "inclusion": false, "version": 1}],
               "polygons": [{"inclusion": true, "polygon": [[47.40, 8.54], [47.39, 8.55], [47.38, 8.54]], "version": 1}], "version": 2},
  "mission": {"cruiseSpeed": 15, "firmwareType": 12, "hoverSpeed": 5, "vehicleType": 20, "version": 2,
              "plannedHomePosition": [47.3977, 8.5456, 488],
              "items": [
                {"type": "SimpleItem", "autoContinue": true, "command": 84, "doJumpId": 1, "frame": 3, "params": [0, 0, 0, null, 47.398, 8.546, 50]},
                {"type": "ComplexItem", "complexItemType": "survey", "TransectStyleComplexItem": {"Items": [
                  {"type": "SimpleItem", "autoContinue": true, "command": 16, "doJumpId": 2, "frame": 3, "params": [0, 0, 0, null, 47.399, 8.547, 50]},
                  {"type": "SimpleItem", "autoContinue": true, "command": 16, "doJumpId": 3, "frame": 3, "params": [0, 0, 0, null, 47.400, 8.548, 50]}
                ]}},
                {"type": "ComplexItem", "complexItemType": "StructureScan"}
              ]},
  "rallyPoints": {"points": [[47.39, 8.545, 30]], "version": 2}
}"#;

    #[test]
    fn qgroundcontrol_plan_with_a_survey() {
        let imported = import(&[file("Zurich.plan", QGC)], PlanVehicleType::Multicopter).unwrap();
        let body = &imported.body;
        assert!(imported.vehicle_type_known);
        assert_eq!(body.vehicle_type, PlanVehicleType::Vtol);
        assert_eq!(body.autopilot, PlanAutopilot::Px4);
        assert_eq!(body.items.len(), 3);
        assert_eq!(body.items[0].params[3], None);
        assert_eq!(body.fence.polygons.len(), 1);
        assert_eq!(body.fence.circles[0].radius_m, 120.0);
        assert_eq!(body.rally.len(), 1);
        assert_eq!(imported.skipped.len(), 2, "{:?}", imported.skipped);
        assert!(imported.skipped[0].contains("survey"));
        assert!(imported.skipped[1].contains("StructureScan"));
    }

    #[test]
    fn qgc_surveys_and_corridors_become_patterns() {
        let plan = r#"{"fileType": "Plan", "version": 1, "groundStation": "QGroundControl",
          "mission": {"version": 2, "firmwareType": 3, "vehicleType": 2, "cruiseSpeed": 15, "hoverSpeed": 5,
            "plannedHomePosition": [-35.3632, 149.1652, 584],
            "items": [
              {"type": "SimpleItem", "command": 22, "frame": 3, "params": [0,0,0,null,0,0,30], "autoContinue": true, "doJumpId": 1, "AMSLAltAboveTerrain": null, "Altitude": 30, "AltitudeMode": 1},
              {"type": "ComplexItem", "complexItemType": "survey", "version": 5, "angle": 0, "entryLocation": 0,
               "flyAlternateTransects": false, "splitConcavePolygons": false,
               "polygon": [[-35.3600, 149.1600], [-35.3600, 149.1622], [-35.3618, 149.1622], [-35.3618, 149.1600]],
               "TransectStyleComplexItem": {"version": 1, "TurnAroundDistance": 10, "Refly90Degrees": false,
                 "CameraTriggerInTurnAround": true, "FollowTerrain": false, "HoverAndCapture": false,
                 "CameraCalc": {"version": 1, "CameraName": "Manual (no camera specs)", "AdjustedFootprintSide": 25,
                   "AdjustedFootprintFrontal": 25, "DistanceToSurface": 60, "DistanceToSurfaceRelative": true},
                 "Items": [], "VisualTransectPoints": []}},
              {"type": "ComplexItem", "complexItemType": "CorridorScan", "version": 2, "CorridorWidth": 50, "EntryPoint": 0,
               "polyline": [[-35.3650, 149.1600], [-35.3650, 149.1650]],
               "TransectStyleComplexItem": {"version": 1, "TurnAroundDistance": 0, "Refly90Degrees": false,
                 "CameraCalc": {"version": 1, "CameraName": "Manual (no camera specs)", "AdjustedFootprintSide": 25,
                   "DistanceToSurface": 40, "DistanceToSurfaceRelative": true},
                 "Items": [], "VisualTransectPoints": []}},
              {"type": "SimpleItem", "command": 20, "frame": 2, "params": [0,0,0,0,0,0,0], "autoContinue": true, "doJumpId": 9}
            ]},
          "geoFence": {"version": 2, "polygons": [], "circles": []},
          "rallyPoints": {"version": 2, "points": []}}"#;
        let imported = import(&[file("Field.plan", plan)], PlanVehicleType::Multicopter).unwrap();
        let body = imported.body;
        assert!(imported.skipped.is_empty(), "{:?}", imported.skipped);
        assert_eq!(body.patterns.len(), 2);
        let PatternShape::Survey {
            spacing_m,
            turnaround_m,
            altitude_m,
            ref area,
            ..
        } = body.patterns[0].shape
        else {
            panic!("{:?}", body.patterns[0]);
        };
        assert_eq!(
            (spacing_m, turnaround_m, altitude_m, area.len()),
            (25.0, 10.0, 60.0, 4)
        );
        assert_eq!(body.patterns[1].id, "corridor-1");
        // Take-off, the survey's lanes, the corridor's two lanes, return.
        assert_eq!(body.items[0].command, 22);
        assert_eq!(body.items.last().unwrap().command, 20);
        let survey = body
            .items
            .iter()
            .filter(|i| i.pattern.as_deref() == Some("survey-1"))
            .count();
        let corridor = body
            .items
            .iter()
            .filter(|i| i.pattern.as_deref() == Some("corridor-1"))
            .count();
        assert_eq!((survey, corridor), (16, 4));
        assert!(
            body.items
                .iter()
                .all(|i| i.latitude_deg != 0.0 || i.command == 22 || i.command == 20)
        );
    }

    #[test]
    fn kml_polygon_and_line() {
        let kml = r#"<?xml version="1.0"?><kml xmlns="http://www.opengis.net/kml/2.2"><Document>
          <Placemark><name> North &amp; south &#937; <![CDATA[<field>]]> </name><Polygon><outerBoundaryIs><LinearRing><coordinates>
            8.54,47.40,0 8.55,47.39,0 8.54,47.38,0 8.54,47.40,0
          </coordinates></LinearRing></outerBoundaryIs></Polygon></Placemark>
          <Placemark><LineString><coordinates>8.50,47.30 8.51,47.31</coordinates></LineString></Placemark>
        </Document></kml>"#;
        let imported = import(&[file("Area.kml", kml)], PlanVehicleType::Multicopter).unwrap();
        assert_eq!(imported.body.fence.polygons[0].points.len(), 3);
        assert_eq!(
            imported.body.fence.polygons[0].name.as_deref(),
            Some("North & south Ω <field>")
        );
        assert_eq!(imported.body.items.len(), 2);
        assert_eq!(imported.body.items[1].latitude_deg, 47.31);
        assert_eq!(imported.body.items[1].altitude_m, 40.0);
    }

    #[test]
    fn unreadable_files_say_so() {
        assert!(
            import(
                &[file("notes.txt", "hello world")],
                PlanVehicleType::Multicopter
            )
            .is_err()
        );
        assert!(
            import(
                &[file("x.json", "{\"a\": 1}")],
                PlanVehicleType::Multicopter
            )
            .is_err()
        );
        assert!(import(&[], PlanVehicleType::Multicopter).is_err());
    }

    #[test]
    fn exports_read_back_the_same() {
        let imported = import(
            &[
                file("Field.fen.waypoints", FENCE_WPL),
                file("Field.waypoints", WPL),
                file("Field.rally.waypoints", RALLY_WPL),
            ],
            PlanVehicleType::Multicopter,
        )
        .unwrap();
        for format in [ExportFormat::Qgc, ExportFormat::MissionPlanner] {
            let files: Vec<PlanFile> = export(&imported.body, format)
                .into_iter()
                .map(|f| PlanFile {
                    name: f.name,
                    text: f.text,
                })
                .collect();
            let again = import(&files, PlanVehicleType::Multicopter).unwrap();
            assert_eq!(again.body.items, imported.body.items, "{format:?}");
            assert_eq!(
                again.body.fence.polygons, imported.body.fence.polygons,
                "{format:?}"
            );
            assert_eq!(
                again.body.fence.circles, imported.body.fence.circles,
                "{format:?}"
            );
            assert_eq!(again.body.rally, imported.body.rally, "{format:?}");
            assert_eq!(again.body.name, "Field", "{format:?}");
        }
    }

    /// Reads every file in `UXDUCK_PLAN_SAMPLES` (files saved by
    /// QGroundControl, Mission Planner and ArduPilot's autotest, not kept in
    /// the repository): `UXDUCK_PLAN_SAMPLES=dir cargo test -p uxduck-engine
    /// plan_files -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs sample files on disk"]
    fn sample_files() {
        let dir = std::env::var("UXDUCK_PLAN_SAMPLES").expect("UXDUCK_PLAN_SAMPLES");
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        paths.sort();
        for path in paths {
            let text = std::fs::read_to_string(&path).unwrap();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            match import(
                &[PlanFile {
                    name: name.clone(),
                    text,
                }],
                PlanVehicleType::Multicopter,
            ) {
                Ok(i) => println!(
                    "{name}: {:?} {:?} {:?} items={} polygons={} circles={} rally={} home={} skipped={:?}",
                    i.body.vehicle_type,
                    i.body.autopilot,
                    i.body.frame,
                    i.body.items.len(),
                    i.body.fence.polygons.len(),
                    i.body.fence.circles.len(),
                    i.body.rally.len(),
                    i.body.planned_home.is_some(),
                    i.skipped
                ),
                Err(error) => println!("{name}: ERROR {error}"),
            }
        }
    }
}
