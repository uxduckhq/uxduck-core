//! `uxduck marker` (S177, ADR-0035): named things on the map besides
//! vehicles, such as a launch site or a ground station.

use std::path::Path;

use clap::{Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use super::client::{CliError, LocalClient};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Look {
    Person,
    GroundStation,
    Boat,
    Car,
    Flag,
    Pin,
    Building,
}

impl Look {
    const fn wire(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::GroundStation => "ground-station",
            Self::Boat => "boat",
            Self::Car => "car",
            Self::Flag => "flag",
            Self::Pin => "pin",
            Self::Building => "building",
        }
    }
}

/// A building's size (ADR-0052): a rectangle, width across its front and
/// depth along its heading, or round.
#[derive(Debug, clap::Args)]
pub struct Size {
    /// Metres across its front; with `--depth`, a rectangular building.
    #[arg(long, requires = "depth", conflicts_with = "radius")]
    width: Option<f64>,
    /// Metres front to back.
    #[arg(long, requires = "width")]
    depth: Option<f64>,
    /// Metres: a round building, a tank or a tree.
    #[arg(long)]
    radius: Option<f64>,
    /// Metres above the ground.
    #[arg(long)]
    height: Option<f64>,
}

impl Size {
    fn set(self, params: &mut Value) {
        let outline = match (self.width, self.depth, self.radius) {
            (Some(width), Some(depth), _) => {
                Some(json!({ "type": "rectangle", "widthM": width, "depthM": depth }))
            }
            (_, _, Some(radius)) => Some(json!({ "type": "round", "radiusM": radius })),
            _ => None,
        };
        set(params, "outline", outline);
        set(params, "heightM", self.height.map(Value::from));
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Colour {
    Blue,
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Purple,
    Pink,
    Grey,
    Black,
}

impl Colour {
    const fn wire(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Red => "red",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Teal => "teal",
            Self::Purple => "purple",
            Self::Pink => "pink",
            Self::Grey => "grey",
            Self::Black => "black",
        }
    }
}

/// A line, an area or a circle and how it is drawn (ADR-0053).
#[derive(Debug, clap::Args)]
pub struct Drawing {
    /// A line through points in order: "53.07,-4.08 53.08,-4.07".
    #[arg(long, conflicts_with_all = ["area", "circle"], allow_hyphen_values = true)]
    line: Option<String>,
    /// An area with corners in order round it: "53.07,-4.08 53.08,-4.07 53.07,-4.06".
    #[arg(long, conflicts_with = "circle", allow_hyphen_values = true)]
    area: Option<String>,
    /// A circle of this many metres about the marker's point.
    #[arg(long)]
    circle: Option<f64>,
    #[arg(long, value_enum)]
    colour: Option<Colour>,
    /// Its line or outline, solid or dashed.
    #[arg(long, value_enum)]
    style: Option<Style>,
    /// An area or a circle filled faintly, or its outline only.
    #[arg(long, value_enum)]
    fill: Option<Fill>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Style {
    Solid,
    Dashed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Fill {
    Filled,
    Outline,
}

impl Drawing {
    fn set(self, params: &mut Value) -> Result<(), CliError> {
        let shape = match (self.line, self.area, self.circle) {
            (Some(line), _, _) => Some(json!({ "type": "line", "points": points(&line)? })),
            (_, Some(area), _) => Some(json!({ "type": "area", "points": points(&area)? })),
            (_, _, Some(radius)) => Some(json!({ "type": "circle", "radiusM": radius })),
            _ => None,
        };
        set(params, "shape", shape);
        set(params, "colour", self.colour.map(|c| Value::from(c.wire())));
        set(
            params,
            "dashed",
            self.style.map(|s| Value::from(s == Style::Dashed)),
        );
        set(
            params,
            "outlineOnly",
            self.fill.map(|f| Value::from(f == Fill::Outline)),
        );
        Ok(())
    }
}

/// "53.07,-4.08 53.08,-4.07" as points.
fn points(text: &str) -> Result<Vec<Value>, CliError> {
    text.split(|c: char| c.is_whitespace() || c == ';')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (lat, lon) = pair.split_once(',').ok_or_else(|| {
                CliError::Usage(format!("{pair} is not a latitude,longitude pair"))
            })?;
            let number = |n: &str| {
                n.trim().parse::<f64>().map_err(|_| {
                    CliError::Usage(format!("{pair} is not a latitude,longitude pair"))
                })
            };
            Ok(json!({ "latitudeDeg": number(lat)?, "longitudeDeg": number(lon)? }))
        })
        .collect()
}

#[derive(Debug, Subcommand)]
pub enum MarkerCommand {
    /// Every marker, by name.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Adds a marker: `uxduck marker add --lat 53.07 --lon -4.08 --look flag --name "Launch site"`,
    /// a building centred there: `… --name Barn --width 24 --depth 12 --height 8 --heading 30`,
    /// or a line or an area: `uxduck marker add --name Road --line "53.07,-4.08 53.08,-4.07" --colour orange`.
    Add {
        #[arg(long, allow_hyphen_values = true, required_unless_present_any = ["line", "area"], requires = "lon")]
        lat: Option<f64>,
        #[arg(long, allow_hyphen_values = true, requires = "lat")]
        lon: Option<f64>,
        /// Named after its look when not given.
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        look: Option<Look>,
        /// Degrees clockwise from north.
        #[arg(long)]
        heading: Option<f64>,
        #[command(flatten)]
        size: Size,
        #[command(flatten)]
        drawing: Drawing,
    },
    /// Changes a marker, by name or id: only what is given.
    Set {
        marker: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        look: Option<Look>,
        #[arg(long, allow_hyphen_values = true, requires = "lon")]
        lat: Option<f64>,
        #[arg(long, allow_hyphen_values = true, requires = "lat")]
        lon: Option<f64>,
        /// Degrees clockwise from north.
        #[arg(long, conflicts_with = "no_heading")]
        heading: Option<f64>,
        /// Takes its heading away.
        #[arg(long)]
        no_heading: bool,
        #[command(flatten)]
        size: Size,
        #[command(flatten)]
        drawing: Drawing,
    },
    /// Removes a marker, by name or id.
    Remove { marker: String },
}

pub async fn run(data_dir: &Path, command: MarkerCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        MarkerCommand::List { json } => {
            let list = client.call("marker/list", json!({})).await?;
            if json {
                println!("{list}");
            } else if list["markers"].as_array().is_none_or(Vec::is_empty) {
                println!("No markers yet.");
            } else {
                for marker in list["markers"].as_array().into_iter().flatten() {
                    println!("{}", line(marker));
                }
            }
        }
        MarkerCommand::Add {
            lat,
            lon,
            name,
            look,
            heading,
            size,
            drawing,
        } => {
            let mut params = json!({});
            set(&mut params, "latitudeDeg", lat.map(Value::from));
            set(&mut params, "longitudeDeg", lon.map(Value::from));
            drawing.set(&mut params)?;
            set(&mut params, "name", name.map(Value::from));
            set(&mut params, "look", look.map(|l| Value::from(l.wire())));
            set(&mut params, "headingDeg", heading.map(Value::from));
            size.set(&mut params);
            let marker = client.call("marker/add", params).await?;
            println!("{}", line(&marker));
        }
        MarkerCommand::Set {
            marker,
            name,
            look,
            lat,
            lon,
            heading,
            no_heading,
            size,
            drawing,
        } => {
            let current = find(&mut client, &marker).await?;
            let mut params = json!({ "id": current["id"], "expectedVersion": current["version"] });
            set(&mut params, "name", name.map(Value::from));
            set(&mut params, "look", look.map(|l| Value::from(l.wire())));
            set(&mut params, "latitudeDeg", lat.map(Value::from));
            set(&mut params, "longitudeDeg", lon.map(Value::from));
            if no_heading {
                set(&mut params, "headingDeg", Some(Value::Null));
            } else {
                set(&mut params, "headingDeg", heading.map(Value::from));
            }
            size.set(&mut params);
            drawing.set(&mut params)?;
            let marker = client.call("marker/update", params).await?;
            println!("{}", line(&marker));
        }
        MarkerCommand::Remove { marker } => {
            let current = find(&mut client, &marker).await?;
            client
                .call("marker/remove", json!({ "id": current["id"] }))
                .await?;
            println!("Removed {}.", current["name"].as_str().unwrap_or_default());
        }
    }
    Ok(())
}

fn set(params: &mut Value, key: &str, value: Option<Value>) {
    if let (Some(value), Some(map)) = (value, params.as_object_mut()) {
        Map::insert(map, key.to_owned(), value);
    }
}

/// A marker by id, or by its name when only one has it.
async fn find(client: &mut LocalClient, marker: &str) -> Result<Value, CliError> {
    let list = client.call("marker/list", json!({})).await?;
    let markers = list["markers"].as_array().cloned().unwrap_or_default();
    if let Some(one) = markers.iter().find(|m| m["id"] == marker) {
        return Ok(one.clone());
    }
    let named: Vec<&Value> = markers
        .iter()
        .filter(|m| {
            m["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(marker))
        })
        .collect();
    match named.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(CliError::Usage(format!(
            "no marker named {marker}; see `uxduck marker list`"
        ))),
        _ => Err(CliError::Usage(format!(
            "more than one marker is named {marker}; use its id"
        ))),
    }
}

/// One marker in a line: "Launch site (mk1a2b…) · flag · 53.0700000, -4.0800000 · facing 270°".
fn line(marker: &Value) -> String {
    let mut parts = vec![
        format!(
            "{} ({})",
            marker["name"].as_str().unwrap_or_default(),
            marker["id"].as_str().unwrap_or_default()
        ),
        format!(
            "{:.7}, {:.7}",
            marker["latitudeDeg"].as_f64().unwrap_or_default(),
            marker["longitudeDeg"].as_f64().unwrap_or_default()
        ),
    ];
    if marker["shape"].is_null() {
        parts.insert(1, marker["look"].as_str().unwrap_or_default().to_string());
    }
    if let Some(heading) = marker["headingDeg"].as_f64() {
        parts.push(format!("facing {heading:.0}°"));
    }
    let outline = &marker["outline"];
    match outline["type"].as_str() {
        Some("rectangle") => parts.push(format!(
            "{} × {} m",
            metres(&outline["widthM"]),
            metres(&outline["depthM"])
        )),
        Some("round") => parts.push(format!(
            "{} m across",
            metres(&json!(
                outline["radiusM"].as_f64().unwrap_or_default() * 2.0
            ))
        )),
        Some("corners") => parts.push(format!(
            "{} corners",
            outline["cornersM"].as_array().map_or(0, Vec::len)
        )),
        _ => {}
    }
    if marker["heightM"].is_number() {
        parts.push(format!("{} m tall", metres(&marker["heightM"])));
    }
    let shape = &marker["shape"];
    let corners: Vec<[f64; 2]> = shape["points"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            [
                p["latitudeDeg"].as_f64().unwrap_or_default(),
                p["longitudeDeg"].as_f64().unwrap_or_default(),
            ]
        })
        .collect();
    match shape["type"].as_str() {
        Some("line") => parts.push(format!("line, {}", distance(length_m(&corners)))),
        Some("area") => parts.push(format!("area, {}", hectares(&corners))),
        Some("circle") => parts.push(format!(
            "circle, {}",
            distance(shape["radiusM"].as_f64().unwrap_or_default())
        )),
        _ => {}
    }
    if let Some(colour) = marker["colour"].as_str() {
        parts.push(colour.to_owned());
    }
    if marker["dashed"] == true {
        parts.push("dashed".to_owned());
    }
    if marker["outlineOnly"] == true {
        parts.push("outline only".to_owned());
    }
    parts.join(" · ")
}

const M_PER_DEG: f64 = 111_320.0;

/// East and north of the first point, in metres.
fn local(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let Some(&[lat0, lon0]) = points.first() else {
        return Vec::new();
    };
    let cos = lat0.to_radians().cos();
    points
        .iter()
        .map(|[lat, lon]| [(lon - lon0) * M_PER_DEG * cos, (lat - lat0) * M_PER_DEG])
        .collect()
}

fn length_m(points: &[[f64; 2]]) -> f64 {
    local(points)
        .windows(2)
        .map(|pair| (pair[1][0] - pair[0][0]).hypot(pair[1][1] - pair[0][1]))
        .sum()
}

/// "850 m", "1.2 km".
fn distance(metres: f64) -> String {
    if metres < 1000.0 {
        format!("{metres:.0} m")
    } else {
        format!("{:.1} km", metres / 1000.0)
    }
}

/// An area's size: "3.4 ha", or square metres under a hectare.
fn hectares(points: &[[f64; 2]]) -> String {
    let ring = local(points);
    let twice: f64 = (0..ring.len())
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum();
    let square_m = twice.abs() / 2.0;
    if square_m < 10_000.0 {
        format!("{square_m:.0} m²")
    } else {
        format!("{:.1} ha", square_m / 10_000.0)
    }
}

/// Metres as people say them: "24", "9.5".
fn metres(value: &Value) -> String {
    let metres = value.as_f64().unwrap_or_default();
    let text = format!("{metres:.1}");
    text.strip_suffix(".0").map_or(text.clone(), str::to_owned)
}
