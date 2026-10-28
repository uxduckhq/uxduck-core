//! The fake vehicle's airspeed sensor (S190), as `ArduPlane` 4.8's in SITL:
//! a pitot's pressure as `SIM_ARSPD_OFS` plus the true airspeed's, read as
//! `sqrt((pressure - ARSPD_OFFSET) * ARSPD_RATIO)`, so a sensor not zeroed
//! reads a few m/s at rest and raising `SIM_ARSPD_OFS` by 60 Pa reads
//! about 11 m/s, as blowing into the pitot does. `SIM_ARSPD_FAIL` above 0
//! sticks the reading there, as a blocked tube or a dead sensor would.
//!
//! Zeroing answers `MAV_CMD_PREFLIGHT_CALIBRATION` with its ground-pressure
//! parameter (p3 = 1), as `ArduPlane` does: "in progress" at once, its
//! barometer and airspeed messages, then 2 s later `ARSPD_OFFSET` taken
//! from the pressure now, "Airspeed 1 calibrated" and "accepted". Its
//! airspeed parameter (p6 = 2) is unsupported, as `ArduPlane` 4.8 answers.
//! Refused while armed.
//!
//! With `ARSPD_AUTOCAL` on and flying, once a second its estimate of the
//! ratio moves a fifth of the way to `SIM_ARSPD_RATIO`, kept as
//! `ARSPD_RATIO`, and sent as `AIRSPEED_AUTOCAL`; a change of more than 5%
//! since it last said is said as "Airspeed 0 ratio reset: …", as
//! `ArduPlane` does when it saves it.

/// `SIM_ARSPD_OFS`: the sensor's pressure at rest, Pa, as SITL's.
pub const SIM_OFFSET_PA: f32 = 2013.0;
/// `SIM_ARSPD_RATIO`: the ratio the sensor truly has, as SITL's.
pub const TRUE_RATIO: f32 = 1.99;
/// `ARSPD_RATIO` as listed: wrong, so the automatic calibration has
/// somewhere to go.
pub const LISTED_RATIO: f32 = 1.6;
/// `ARSPD_OFFSET` of a sensor not zeroed: 8 Pa off, reading 4 m/s at rest.
pub const UNZEROED_PA: f32 = 2005.0;
/// How long zeroing takes.
pub const ZERO_S: f64 = 2.0;
/// The automatic calibration's update, once a second.
const AUTOCAL_EVERY_S: f64 = 1.0;
/// Each update moves the estimate this part of the way.
const AUTOCAL_STEP: f32 = 0.2;
/// It says a new ratio when it moved more than this since it last said.
const SAID_CHANGE: f32 = 0.05;

/// What the sensor does at a tick, for the fake vehicle to send.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// A `STATUSTEXT`: severity and text.
    Say(u8, String),
    /// The zero's final `COMMAND_ACK` for command 241.
    Ack(u8),
    /// A parameter it changed itself, to broadcast as `PARAM_VALUE`.
    Changed(&'static str),
    /// `AIRSPEED_AUTOCAL` with this ratio.
    Autocal(f32),
}

/// The fake vehicle's airspeed sensor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FakeAirspeed {
    /// Its parameters are listed (`add_airspeed`): a plane with a sensor.
    pub listed: bool,
    zeroing_until_s: Option<f64>,
    autocal_at_s: Option<f64>,
    said_ratio: Option<f32>,
}

/// A parameter's value in the fake vehicle's table.
fn value(parameters: &[(String, f32, u8)], name: &str) -> Option<f32> {
    parameters.iter().find(|p| p.0 == name).map(|p| p.1)
}

fn set(parameters: &mut [(String, f32, u8)], name: &str, to: f32) {
    if let Some(p) = parameters.iter_mut().find(|p| p.0 == name) {
        p.1 = to;
    }
}

/// The pitot's pressure, Pa, at a true airspeed.
fn pressure(parameters: &[(String, f32, u8)], true_mps: f64) -> f64 {
    let ratio = f64::from(value(parameters, "SIM_ARSPD_RATIO").unwrap_or(TRUE_RATIO));
    let offset = f64::from(value(parameters, "SIM_ARSPD_OFS").unwrap_or(SIM_OFFSET_PA));
    true_mps * true_mps / ratio.max(0.1) + offset
}

impl FakeAirspeed {
    /// A sensor that is listed, as `add_airspeed` makes it.
    #[must_use]
    pub fn listed() -> Self {
        Self {
            listed: true,
            ..Self::default()
        }
    }

    /// What it reads at a true airspeed, m/s.
    #[must_use]
    pub fn reading(parameters: &[(String, f32, u8)], true_mps: f64) -> f64 {
        if let Some(stuck) = value(parameters, "SIM_ARSPD_FAIL").filter(|v| *v > 0.0) {
            return f64::from(stuck);
        }
        let zero = f64::from(value(parameters, "ARSPD_OFFSET").unwrap_or(SIM_OFFSET_PA));
        let ratio = f64::from(value(parameters, "ARSPD_RATIO").unwrap_or(2.0));
        ((pressure(parameters, true_mps) - zero).max(0.0) * ratio).sqrt()
    }

    /// Its answer to `MAV_CMD_PREFLIGHT_CALIBRATION` with these parameters
    /// (p3, p6): a result and what it says first; `None` for one that is
    /// not its own.
    pub fn zero(
        &mut self,
        t_s: f64,
        armed: bool,
        (ground, airspeed): (f32, f32),
    ) -> Option<(u8, Vec<String>)> {
        if !self.listed {
            return None;
        }
        if (airspeed - 2.0).abs() < 0.5 {
            // MAV_RESULT_UNSUPPORTED, as ArduPlane 4.8 answers.
            return Some((3, Vec::new()));
        }
        if (ground - 1.0).abs() >= 0.5 {
            return None;
        }
        if armed {
            return Some((4, Vec::new()));
        }
        if self.zeroing_until_s.is_some() {
            return Some((5, Vec::new()));
        }
        self.zeroing_until_s = Some(t_s + ZERO_S);
        Some((
            5,
            vec![
                "Updating barometer calibration".into(),
                "Barometer calibration complete".into(),
                "Airspeed 1 calibration started".into(),
            ],
        ))
    }

    /// What it does by `t_s`, flying or not at a true airspeed.
    pub fn tick(
        &mut self,
        t_s: f64,
        parameters: &mut [(String, f32, u8)],
        flying: bool,
        true_mps: f64,
    ) -> Vec<Out> {
        let mut out = Vec::new();
        if self.zeroing_until_s.is_some_and(|until| t_s >= until) {
            self.zeroing_until_s = None;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "a pressure in pascals, kept as a float parameter"
            )]
            let now = pressure(parameters, true_mps) as f32;
            set(parameters, "ARSPD_OFFSET", now);
            out.push(Out::Changed("ARSPD_OFFSET"));
            out.push(Out::Say(6, "Airspeed 1 calibrated".into()));
            out.push(Out::Ack(0));
        }
        let on = value(parameters, "ARSPD_AUTOCAL").is_some_and(|v| v != 0.0);
        if !(on && flying) {
            self.autocal_at_s = None;
            return out;
        }
        if self
            .autocal_at_s
            .is_some_and(|at| t_s - at < AUTOCAL_EVERY_S)
        {
            return out;
        }
        self.autocal_at_s = Some(t_s);
        let truth = value(parameters, "SIM_ARSPD_RATIO").unwrap_or(TRUE_RATIO);
        let ratio = value(parameters, "ARSPD_RATIO").unwrap_or(2.0);
        let next = ratio + (truth - ratio) * AUTOCAL_STEP;
        set(parameters, "ARSPD_RATIO", next);
        out.push(Out::Autocal(next));
        let said = *self.said_ratio.get_or_insert(ratio);
        if (next - said).abs() > said.abs() * SAID_CHANGE {
            self.said_ratio = Some(next);
            out.push(Out::Changed("ARSPD_RATIO"));
            out.push(Out::Say(6, format!("Airspeed 0 ratio reset: {next:.6}")));
        }
        out
    }

    /// It restarted: a zero running stops.
    pub const fn restarted(&mut self) {
        self.zeroing_until_s = None;
        self.autocal_at_s = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(offset: f32) -> Vec<(String, f32, u8)> {
        [
            ("ARSPD_TYPE", 1.0),
            ("ARSPD_OFFSET", offset),
            ("ARSPD_RATIO", LISTED_RATIO),
            ("ARSPD_AUTOCAL", 0.0),
            ("SIM_ARSPD_OFS", SIM_OFFSET_PA),
            ("SIM_ARSPD_RATIO", TRUE_RATIO),
            ("SIM_ARSPD_FAIL", 0.0),
        ]
        .iter()
        .map(|(n, v)| ((*n).to_owned(), *v, 9))
        .collect()
    }

    #[test]
    fn zeroing_takes_the_pressure_now_and_a_blow_raises_the_reading() {
        let mut p = listed(UNZEROED_PA);
        let mut sensor = FakeAirspeed::listed();
        assert!((FakeAirspeed::reading(&p, 0.0) - 3.58).abs() < 0.01);
        assert_eq!(sensor.zero(0.0, true, (1.0, 0.0)).map(|a| a.0), Some(4));
        assert_eq!(sensor.zero(0.0, false, (0.0, 2.0)).map(|a| a.0), Some(3));
        assert_eq!(sensor.zero(0.0, false, (0.0, 0.0)), None);
        let (result, said) = sensor.zero(0.0, false, (1.0, 0.0)).unwrap();
        assert_eq!(result, 5);
        assert_eq!(said[2], "Airspeed 1 calibration started");
        assert!(sensor.tick(1.0, &mut p, false, 0.0).is_empty());
        let done = sensor.tick(2.0, &mut p, false, 0.0);
        assert_eq!(done.last(), Some(&Out::Ack(0)));
        assert!(FakeAirspeed::reading(&p, 0.0) < 0.01);
        set(&mut p, "SIM_ARSPD_OFS", SIM_OFFSET_PA + 60.0);
        assert!((FakeAirspeed::reading(&p, 0.0) - 9.8).abs() < 0.1);
        // Stuck: it reads the same whatever happens.
        set(&mut p, "SIM_ARSPD_FAIL", 1.4);
        assert!((FakeAirspeed::reading(&p, 20.0) - 1.4).abs() < 1e-6);
    }

    #[test]
    fn the_automatic_calibration_converges_in_flight_only() {
        let mut p = listed(SIM_OFFSET_PA);
        let mut sensor = FakeAirspeed::listed();
        set(&mut p, "ARSPD_AUTOCAL", 1.0);
        assert!(sensor.tick(0.0, &mut p, false, 0.0).is_empty());
        let mut said = 0;
        for t in 0..30 {
            for out in sensor.tick(f64::from(t), &mut p, true, 20.0) {
                if matches!(out, Out::Say(..)) {
                    said += 1;
                }
            }
        }
        let ratio = value(&p, "ARSPD_RATIO").unwrap();
        assert!((ratio - TRUE_RATIO).abs() < 0.01, "{ratio}");
        assert!((2..6).contains(&said), "{said}");
    }
}
