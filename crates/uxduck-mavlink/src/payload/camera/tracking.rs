//! What the simulated camera tracks (ADR-0063 Y5): a place on the ground,
//! taken where a point or the middle of a box in its picture falls, and
//! found again in every picture after as the vehicle moves and the camera
//! turns.
//!
//! The ground is flat and at home's height, as `CAMERA_FOV_STATUS` has it
//! here too, and the lens is half a metre above the vehicle's own height,
//! as the picture is drawn. Where the true ground is higher or lower than
//! home, the place taken is off by that height over the tangent of how
//! steeply the camera looks down.

use super::View;

const METRES_PER_DEGREE: f64 = 111_320.0;
/// The picture's height over its width, as the stream is sent.
const DOWN_OVER_ACROSS: f64 = 720.0 / 1280.0;
/// The lens above the vehicle's own height, in metres: where the picture
/// is drawn from, so that a vehicle on the ground sees over it.
pub(in crate::payload) const LENS_ABOVE_M: f64 = 0.5;
/// A ray this near level, or above it, meets no ground worth the name.
const LEVEL: f64 = 0.01;

/// How large the target was drawn, in metres across the line of sight
/// where it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Shape {
    Point { radius_m: f64 },
    Rectangle { across_m: f64, down_m: f64 },
}

/// A place on the ground the camera follows: degrees, and metres above
/// sea level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::payload) struct Target {
    pub(in crate::payload) lat: f64,
    pub(in crate::payload) lon: f64,
    pub(in crate::payload) alt_m: f64,
    shape: Shape,
}

/// Where the target is in a picture: across and down it from 0 to 1, how
/// far away in metres, and how many metres the picture is across there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Seen {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) range_m: f64,
    width_m: f64,
}

/// A target as `CAMERA_TRACKING_IMAGE_STATUS` gives it: the point and its
/// radius, or the box's left, top, right and bottom. NaN is not known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Drawn {
    pub(super) point: [f32; 3],
    pub(super) rectangle: [f32; 4],
}

impl Target {
    /// The ground under a place in the picture, with a radius or a box's
    /// width and height as shares of the picture's own. None where the
    /// camera does not know where it is, or looks over the ground there.
    pub(super) fn under(
        view: &View,
        at: (f32, f32),
        radius: f32,
        size: Option<(f32, f32)>,
    ) -> Option<Self> {
        let (across, down) = tangents(view);
        let [forward, right, below] = axes(view.attitude);
        let a = (f64::from(at.0) - 0.5) * 2.0 * across;
        let b = (f64::from(at.1) - 0.5) * 2.0 * down;
        let ray = [0, 1, 2].map(|i| forward[i] + right[i] * a + below[i] * b);
        let length = ray.iter().map(|part| part * part).sum::<f64>().sqrt();
        let height = height(view);
        if ray[2] < LEVEL * length {
            return None;
        }
        let reach = height / ray[2];
        let range_m = reach * length;
        let width_m = 2.0 * across * range_m;
        let latitude = f64::from(view.place.lat) / 1e7;
        Some(Self {
            lat: latitude + ray[0] * reach / METRES_PER_DEGREE,
            lon: f64::from(view.place.lon) / 1e7
                + ray[1] * reach / (METRES_PER_DEGREE * latitude.to_radians().cos()),
            alt_m: f64::from(view.place.alt_mm - view.place.relative_alt_mm) / 1000.0,
            shape: match size {
                Some((wide, high)) => Shape::Rectangle {
                    across_m: f64::from(wide) * width_m,
                    down_m: f64::from(high) * width_m * DOWN_OVER_ACROSS,
                },
                None => Shape::Point {
                    radius_m: f64::from(radius) * width_m,
                },
            },
        })
    }

    /// Whether it was taken as a box and not as a point.
    pub(super) fn boxed(&self) -> bool {
        matches!(self.shape, Shape::Rectangle { .. })
    }

    /// Where it is in the picture now. None when it is outside the picture
    /// or behind the camera.
    pub(super) fn seen(&self, view: &View) -> Option<Seen> {
        let (across, down) = tangents(view);
        let [forward, right, below] = axes(view.attitude);
        let latitude = f64::from(view.place.lat) / 1e7;
        let to = [
            (self.lat - latitude) * METRES_PER_DEGREE,
            (self.lon - f64::from(view.place.lon) / 1e7)
                * METRES_PER_DEGREE
                * latitude.to_radians().cos(),
            height(view),
        ];
        let along = |axis: [f64; 3]| axis[0] * to[0] + axis[1] * to[1] + axis[2] * to[2];
        let ahead = along(forward);
        if ahead <= 0.0 {
            return None;
        }
        let x = 0.5 + along(right) / ahead / (2.0 * across);
        let y = 0.5 + along(below) / ahead / (2.0 * down);
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
            return None;
        }
        let range_m = to.iter().map(|part| part * part).sum::<f64>().sqrt();
        Some(Seen {
            x,
            y,
            range_m,
            width_m: 2.0 * across * range_m,
        })
    }

    /// The target as the image status draws it, cut to the picture.
    #[allow(clippy::cast_possible_truncation, reason = "shares of a picture")]
    pub(super) fn drawn(&self, seen: &Seen) -> Drawn {
        let within = |share: f64| share.clamp(0.0, 1.0) as f32;
        match self.shape {
            Shape::Point { radius_m } => Drawn {
                point: [
                    within(seen.x),
                    within(seen.y),
                    within(radius_m / seen.width_m),
                ],
                rectangle: [f32::NAN; 4],
            },
            Shape::Rectangle { across_m, down_m } => {
                let half_wide = across_m / seen.width_m / 2.0;
                let half_high = down_m / (seen.width_m * DOWN_OVER_ACROSS) / 2.0;
                Drawn {
                    point: [f32::NAN; 3],
                    rectangle: [
                        within(seen.x - half_wide),
                        within(seen.y - half_high),
                        within(seen.x + half_wide),
                        within(seen.y + half_high),
                    ],
                }
            }
        }
    }
}

/// The lens above the ground at home's height, in metres.
fn height(view: &View) -> f64 {
    (f64::from(view.place.relative_alt_mm) / 1000.0).max(0.0) + LENS_ABOVE_M
}

/// The tangents of half the picture's angle across and down.
fn tangents(view: &View) -> (f64, f64) {
    let across = (f64::from(view.hfov_deg).to_radians() / 2.0).tan();
    (across, across * DOWN_OVER_ACROSS)
}

/// The camera's forward, right and down, each as north, east and down,
/// from its attitude: a quaternion (w, x, y, z) from north, east and down.
/// The lens looks along its x axis, and its picture's top is against its
/// z.
fn axes(attitude: [f32; 4]) -> [[f64; 3]; 3] {
    let [w, x, y, z] = attitude.map(f64::from);
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y + w * z),
            2.0 * (x * z - w * y),
        ],
        [
            2.0 * (x * y - w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + w * x),
        ],
        [
            2.0 * (x * z + w * y),
            2.0 * (y * z - w * x),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}
