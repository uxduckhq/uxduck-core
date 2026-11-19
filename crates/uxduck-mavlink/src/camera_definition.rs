//! A camera's definition file (ADR-0063 Y5, S411): the XML a camera names in
//! `CAMERA_INFORMATION.cam_definition_uri`, which lists the camera's own
//! settings, the values each takes, and which values of one rule out
//! another.
//!
//! The file comes from the camera, or from an address the camera gave, so
//! it is read as hostile input. It may declare no document type and use no
//! entity beyond XML's own five. Its size, its depth and its counts are
//! limited, and a file that breaks a limit is refused whole. Nothing in it
//! is run, fetched or followed: what it says is kept as text and numbers.

use uxduck_core::domain::camera::CameraSettingKind;

/// The most a definition file may unpack to.
pub const MAX_UNPACKED: usize = 4 << 20;
/// How deep its elements may nest.
const MAX_DEPTH: usize = 16;
/// How many settings it may name.
const MAX_SETTINGS: usize = 512;
/// How many values, updates or exclusions one list may hold.
const MAX_IN_A_LIST: usize = 512;
/// How many values it may list in all.
const MAX_VALUES: usize = 20_000;
/// The longest name, value or condition, and where free text is cut. The
/// file is the camera's own or its maker's, fetched from wherever the
/// camera names, so what it says is bounded. Text that is cut ends in a
/// mark ([`cut`]), so that whoever reads it sees the file said more.
const MAX_TEXT: usize = 256;

/// What a camera's definition file says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Definition {
    /// The file's own version, as it says it.
    pub version: String,
    pub vendor: String,
    pub model: String,
    /// The settings, in the file's order.
    pub settings: Vec<Setting>,
}

impl Definition {
    /// How much of the file's free text was cut, as a sentence: how many
    /// of its names, units and descriptions were longer than
    /// [`MAX_TEXT`] characters. None when all of it was kept whole.
    #[must_use]
    pub fn text_cut(&self) -> Option<String> {
        // [`cut`] leaves a text it cut one character longer than is kept,
        // ending in its mark.
        let cut = [&self.version, &self.vendor, &self.model]
            .into_iter()
            .map(String::as_str)
            .chain(self.settings.iter().flat_map(|setting| {
                std::iter::once(setting.description.as_str())
                    .chain(setting.unit.as_deref())
                    .chain(setting.choices.iter().map(|choice| choice.name.as_str()))
            }))
            .filter(|text| text.ends_with('…') && text.chars().count() > MAX_TEXT)
            .count();
        match cut {
            0 => None,
            1 => Some(format!(
                "The camera's definition file has 1 text longer than {MAX_TEXT} characters. UXDuck shows its first {MAX_TEXT} characters, and … marks where it was cut."
            )),
            _ => Some(format!(
                "The camera's definition file has {} texts longer than {MAX_TEXT} characters. UXDuck shows the first {MAX_TEXT} characters of each, and … marks where each was cut.",
                uxduck_core::domain::camera::grouped(cut as u64)
            )),
        }
    }
}

/// One of the camera's own settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    /// Its name as an extended parameter.
    pub name: String,
    /// What the file calls it, for people.
    pub description: String,
    pub kind: CameraSettingKind,
    /// False when the file asks for no control of its own (`control="0"`).
    pub control: bool,
    pub read_only: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub unit: Option<String>,
    /// The values it takes. Empty for a number in a range, and for a
    /// `bool`.
    pub choices: Vec<Choice>,
    /// The settings to read again when this one changes.
    pub updates: Vec<String>,
}

/// One value a setting takes, and what choosing it does to the others.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub name: String,
    pub value: f64,
    /// The settings that do not apply while this is chosen.
    pub exclusions: Vec<String>,
    /// The settings whose values are limited while this is chosen.
    pub ranges: Vec<Range>,
}

/// The values another setting is limited to while a choice stands.
#[derive(Clone, Debug, PartialEq)]
pub struct Range {
    pub setting: String,
    /// When given, the limit holds only while this does.
    pub condition: Option<Condition>,
    pub choices: Vec<(String, f64)>,
}

/// A range's condition: tests of other settings' values, joined left to
/// right by AND and OR, as QGroundControl reads them.
#[derive(Clone, Debug, PartialEq)]
pub struct Condition(Vec<(Join, Option<Test>)>);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Join {
    And,
    Or,
}

#[derive(Clone, Debug, PartialEq)]
struct Test {
    setting: String,
    op: Op,
    value: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Equal,
    NotEqual,
    Greater,
    Smaller,
}

impl Condition {
    /// `CAM_EXPMODE=1 AND CAM_ISO>100`. A test that cannot be read never
    /// holds.
    fn of(text: &str) -> Self {
        let mut tests = Vec::new();
        let mut join = Join::And;
        let mut words = text.split_whitespace();
        while let Some(word) = words.next() {
            tests.push((join, Test::of(word)));
            join = match words.next() {
                Some(word) if word.eq_ignore_ascii_case("AND") => Join::And,
                Some(_) => Join::Or,
                None => break,
            };
        }
        Self(tests)
    }

    /// Whether it holds for the values the camera has now. A test of a
    /// setting with no value does not hold.
    #[must_use]
    pub fn holds(&self, value: &dyn Fn(&str) -> Option<f64>) -> bool {
        self.0.iter().fold(true, |so_far, (join, test)| {
            let holds = test.as_ref().is_some_and(|test| {
                value(&test.setting).is_some_and(|now| match test.op {
                    Op::Equal => same(now, test.value),
                    Op::NotEqual => !same(now, test.value),
                    Op::Greater => now > test.value,
                    Op::Smaller => now < test.value,
                })
            });
            match join {
                Join::And => so_far && holds,
                Join::Or => so_far || holds,
            }
        })
    }
}

impl Test {
    fn of(word: &str) -> Option<Self> {
        let (at, op, len) = if let Some(at) = word.find("!=") {
            (at, Op::NotEqual, 2)
        } else if let Some(at) = word.find('=') {
            (at, Op::Equal, 1)
        } else if let Some(at) = word.find('>') {
            (at, Op::Greater, 1)
        } else {
            (word.find('<')?, Op::Smaller, 1)
        };
        Some(Self {
            setting: word[..at].to_owned(),
            op,
            value: number(&word[at + len..])?,
        })
    }
}

/// Whether two numbers are one value of a setting. A `float` setting's
/// value comes back as the nearest 32-bit number to what the file wrote.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "values are compared as the camera holds them"
)]
pub fn same(a: f64, b: f64) -> bool {
    a == b || (a as f32) == (b as f32)
}

impl Definition {
    #[must_use]
    pub fn setting(&self, name: &str) -> Option<&Setting> {
        self.settings.iter().find(|setting| setting.name == name)
    }

    /// The setting whose present value rules this one out, when one does:
    /// it does not apply as the camera is set now.
    #[must_use]
    pub fn excluded_by(&self, name: &str, value: &dyn Fn(&str) -> Option<f64>) -> Option<&str> {
        self.settings
            .iter()
            .filter(|other| other.name != name)
            .find(|other| {
                other
                    .chosen(value)
                    .is_some_and(|choice| choice.exclusions.iter().any(|out| out == name))
            })
            .map(|other| other.name.as_str())
    }

    /// The values a setting takes as the camera is set now: those another
    /// setting's present value limits it to, or else its own. The first
    /// limit in the file's order that holds is the one.
    #[must_use]
    pub fn choices(
        &self,
        setting: &Setting,
        value: &dyn Fn(&str) -> Option<f64>,
    ) -> Vec<(String, f64)> {
        self.settings
            .iter()
            .filter_map(|other| other.chosen(value))
            .flat_map(|choice| &choice.ranges)
            .find(|range| {
                range.setting == setting.name
                    && range.condition.as_ref().is_none_or(|c| c.holds(value))
            })
            .map_or_else(
                || {
                    setting
                        .choices
                        .iter()
                        .map(|choice| (choice.name.clone(), choice.value))
                        .collect()
                },
                |range| range.choices.clone(),
            )
    }
}

impl Setting {
    /// The choice that stands now.
    fn chosen(&self, value: &dyn Fn(&str) -> Option<f64>) -> Option<&Choice> {
        let now = value(&self.name)?;
        self.choices.iter().find(|choice| same(choice.value, now))
    }
}

/// Reads a definition file as it was fetched, unpacking one that is
/// xz-compressed.
///
/// # Errors
/// Why UXDuck refuses the file, as a sentence for people. It says nothing
/// of what the file holds beyond the names of its settings.
pub fn parse(bytes: &[u8]) -> Result<Definition, String> {
    use quick_xml::events::Event;
    let text = unpack(bytes)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().expand_empty_elements = true;
    let mut open = Open::default();
    loop {
        let event = reader.read_event().map_err(|_| {
            format!(
                "It is not well-formed XML, near byte {}.",
                reader.error_position()
            )
        })?;
        match event {
            Event::DocType(_) => {
                return Err("It declares a document type, which UXDuck does not read.".into());
            }
            Event::Start(tag) => open.start(&tag)?,
            Event::End(_) => open.end()?,
            Event::Text(text) => open.text(&text.xml10_content()),
            Event::CData(text) => open.text(&text.xml10_content()),
            Event::GeneralRef(raw) => {
                let reference = format!("&{};", raw.as_ref());
                let resolved = quick_xml::escape::unescape(&reference).map_err(|_| {
                    "It uses an entity that is not one of XML's own, which UXDuck does not read."
                        .to_owned()
                })?;
                open.text(&resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    open.finish()
}

fn not_text() -> String {
    "It is not text UXDuck can read.".to_owned()
}

/// A file as text, unpacked when it is xz-compressed. A packed file that
/// says it unpacks to more than [`MAX_UNPACKED`] is refused before any of it
/// is unpacked.
fn unpack(bytes: &[u8]) -> Result<String, String> {
    let text = if crate::xz::is_xz(bytes) {
        crate::xz::unpack(bytes, MAX_UNPACKED).map_err(|refused| match refused {
            crate::xz::Refused::TooLarge => {
                "It unpacks to more than 4 MB, the most UXDuck reads for a camera's definition."
                    .to_owned()
            }
            crate::xz::Refused::Unreadable(_) => {
                "It is xz-compressed and could not be unpacked.".to_owned()
            }
        })?
    } else if bytes.len() > MAX_UNPACKED {
        return Err(
            "It is larger than 4 MB, the most UXDuck reads for a camera's definition.".into(),
        );
    } else {
        bytes.to_vec()
    };
    String::from_utf8(text).map_err(|_| not_text())
}

/// The elements that are read. Any other, and all inside it, is skipped.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tag {
    Root,
    Definition,
    Model,
    Vendor,
    Parameters,
    Parameter,
    Description,
    Updates,
    Update,
    Options,
    Choice,
    Exclusions,
    Exclude,
    Ranges,
    Range,
    RangeChoice,
}

fn child(parent: Option<Tag>, name: &str) -> Option<Tag> {
    Some(match (parent, name) {
        (None, "mavlinkcamera") => Tag::Root,
        (Some(Tag::Root), "definition") => Tag::Definition,
        (Some(Tag::Definition), "model") => Tag::Model,
        (Some(Tag::Definition), "vendor") => Tag::Vendor,
        (Some(Tag::Root), "parameters") => Tag::Parameters,
        (Some(Tag::Parameters), "parameter") => Tag::Parameter,
        (Some(Tag::Parameter), "description") => Tag::Description,
        (Some(Tag::Parameter), "updates") => Tag::Updates,
        (Some(Tag::Updates), "update") => Tag::Update,
        (Some(Tag::Parameter), "options") => Tag::Options,
        (Some(Tag::Options), "option" | "roption") => Tag::Choice,
        (Some(Tag::Choice), "exclusions") => Tag::Exclusions,
        (Some(Tag::Exclusions), "exclude") => Tag::Exclude,
        (Some(Tag::Choice), "parameterranges") => Tag::Ranges,
        (Some(Tag::Ranges), "parameterrange") => Tag::Range,
        (Some(Tag::Range), "roption" | "option") => Tag::RangeChoice,
        _ => return None,
    })
}

/// The file as it is read: what is open, and what is built so far.
#[derive(Default)]
struct Open {
    path: Vec<Tag>,
    /// How deep inside an element that is not read.
    skipped: usize,
    /// The outer element has closed.
    closed: bool,
    text: String,
    definition: Definition,
    setting: Option<Setting>,
    choice: Option<Choice>,
    range: Option<Range>,
    values: usize,
}

impl Open {
    fn start(&mut self, tag: &quick_xml::events::BytesStart) -> Result<(), String> {
        if self.path.len() + self.skipped >= MAX_DEPTH {
            return Err(format!("Its elements nest more than {MAX_DEPTH} deep."));
        }
        self.text.clear();
        if self.skipped > 0 {
            self.skipped += 1;
            return Ok(());
        }
        let parent = self.path.last().copied();
        let Some(kind) = child(parent, tag.name().as_ref()) else {
            if parent.is_none() {
                return Err(
                    "It is not a camera's definition: its outer element is not mavlinkcamera."
                        .into(),
                );
            }
            self.skipped = 1;
            return Ok(());
        };
        if self.closed {
            return Err("It has more than one outer element.".into());
        }
        match kind {
            Tag::Definition => {
                self.definition.version = cut(&attribute(tag, "version")?.unwrap_or_default());
            }
            Tag::Parameter => self.parameter(tag)?,
            Tag::Choice => {
                let Some(setting) = &self.setting else {
                    return Ok(());
                };
                if setting.choices.len() >= MAX_IN_A_LIST {
                    return Err(too_many(&setting.name, "values"));
                }
                let (name, value) = self.value(tag)?;
                self.choice = Some(Choice {
                    name,
                    value,
                    exclusions: Vec::new(),
                    ranges: Vec::new(),
                });
            }
            Tag::Range => {
                let within = self.within();
                let setting = attribute(tag, "parameter")?
                    .filter(|name| named(name))
                    .ok_or_else(|| {
                        format!("It limits a setting it does not name, under {within}.")
                    })?;
                let condition = attribute(tag, "condition")?;
                if condition.as_ref().is_some_and(|c| c.len() > MAX_TEXT) {
                    return Err(format!(
                        "A condition under {within} is longer than {MAX_TEXT} characters."
                    ));
                }
                self.range = Some(Range {
                    setting,
                    condition: condition
                        .filter(|c| !c.trim().is_empty())
                        .map(|c| Condition::of(&c)),
                    choices: Vec::new(),
                });
            }
            Tag::RangeChoice => {
                let within = self.within();
                if self
                    .range
                    .as_ref()
                    .is_some_and(|range| range.choices.len() >= MAX_IN_A_LIST)
                {
                    return Err(too_many(&within, "values in one limit"));
                }
                let choice = self.value(tag)?;
                if let Some(range) = &mut self.range {
                    range.choices.push(choice);
                }
            }
            _ => {}
        }
        self.path.push(kind);
        Ok(())
    }

    /// The setting being read, for an error to name.
    fn within(&self) -> String {
        self.setting
            .as_ref()
            .map_or_else(|| "a setting".to_owned(), |setting| setting.name.clone())
    }

    /// A value's name and number, counted against the file's limit.
    fn value(&mut self, tag: &quick_xml::events::BytesStart) -> Result<(String, f64), String> {
        let within = self.within();
        self.values += 1;
        if self.values > MAX_VALUES {
            return Err(format!("It lists more than {MAX_VALUES} values in all."));
        }
        let text = attribute(tag, "value")?.unwrap_or_default();
        let value = number(&text)
            .ok_or_else(|| format!("It gives {within} a value that is not a number."))?;
        let name = attribute(tag, "name")?
            .map(|name| cut(&name))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| cut(&text));
        Ok((name, value))
    }

    fn parameter(&mut self, tag: &quick_xml::events::BytesStart) -> Result<(), String> {
        if self.definition.settings.len() >= MAX_SETTINGS {
            return Err(format!("It names more than {MAX_SETTINGS} settings."));
        }
        let name = attribute(tag, "name")?.unwrap_or_default();
        if !named(&name) {
            return Err(
                "It names a setting with something other than 1 to 16 plain characters.".into(),
            );
        }
        if self.definition.setting(&name).is_some() {
            return Err(format!("It names {name} twice."));
        }
        let kind = attribute(tag, "type")?
            .as_deref()
            .and_then(CameraSettingKind::of)
            .ok_or_else(|| {
                format!("It gives {name} a type that is not one a camera's setting has.")
            })?;
        let flag = |key: &str| -> Result<bool, String> {
            Ok(attribute(tag, key)?.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true")))
        };
        let limit = |key: &str| -> Result<Option<f64>, String> {
            attribute(tag, key)?
                .map(|text| {
                    number(&text)
                        .ok_or_else(|| format!("It gives {name} a {key} that is not a number."))
                })
                .transpose()
        };
        let (min, max) = (limit("min")?, limit("max")?);
        if let (Some(min), Some(max)) = (min, max)
            && min > max
        {
            return Err(format!("It gives {name} a least value above its most."));
        }
        self.setting = Some(Setting {
            description: String::new(),
            kind,
            control: attribute(tag, "control")?.is_none_or(|v| v != "0"),
            read_only: flag("readonly")?,
            min,
            max,
            step: limit("step")?.filter(|step| *step > 0.0),
            unit: attribute(tag, "unit")?
                .map(|unit| cut(&unit))
                .filter(|unit| !unit.is_empty()),
            choices: Vec::new(),
            updates: Vec::new(),
            name,
        });
        Ok(())
    }

    fn text(&mut self, more: &str) {
        let read = matches!(
            self.path.last(),
            Some(Tag::Model | Tag::Vendor | Tag::Description | Tag::Update | Tag::Exclude)
        );
        // Kept a little past the limit, so that a name too long is seen.
        if read && self.skipped == 0 && self.text.len() <= 4 * MAX_TEXT {
            self.text.push_str(more);
        }
    }

    fn end(&mut self) -> Result<(), String> {
        if self.skipped > 0 {
            self.skipped -= 1;
            return Ok(());
        }
        let Some(kind) = self.path.pop() else {
            return Ok(());
        };
        let text = std::mem::take(&mut self.text);
        let text = text.trim();
        match kind {
            Tag::Root => self.closed = true,
            Tag::Model => self.definition.model = cut(text),
            Tag::Vendor => self.definition.vendor = cut(text),
            Tag::Description => {
                if let Some(setting) = &mut self.setting
                    && setting.description.is_empty()
                {
                    setting.description = cut(text);
                }
            }
            Tag::Update | Tag::Exclude => {
                let within = self.within();
                if !named(text) {
                    return Err(format!(
                        "Under {within} it names a setting with something other than 1 to 16 plain characters."
                    ));
                }
                let list = if kind == Tag::Update {
                    self.setting.as_mut().map(|setting| &mut setting.updates)
                } else {
                    self.choice.as_mut().map(|choice| &mut choice.exclusions)
                };
                if let Some(list) = list {
                    if list.len() >= MAX_IN_A_LIST {
                        return Err(too_many(&within, "settings in one list"));
                    }
                    list.push(text.to_owned());
                }
            }
            Tag::Range => {
                if let (Some(choice), Some(range)) = (&mut self.choice, self.range.take()) {
                    if choice.ranges.len() >= MAX_IN_A_LIST {
                        return Err(too_many(&self.within(), "limits under one value"));
                    }
                    choice.ranges.push(range);
                }
            }
            Tag::Choice => {
                if let (Some(setting), Some(choice)) = (&mut self.setting, self.choice.take()) {
                    setting.choices.push(choice);
                }
            }
            Tag::Parameter => {
                if let Some(mut setting) = self.setting.take() {
                    if setting.description.is_empty() {
                        setting.description.clone_from(&setting.name);
                    }
                    // On or off, whatever else the file lists for it.
                    if setting.kind == CameraSettingKind::Bool {
                        setting.choices.clear();
                    }
                    self.definition.settings.push(setting);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The file, once it has ended: without what it says of settings it
    /// does not name.
    fn finish(mut self) -> Result<Definition, String> {
        if !self.closed || !self.path.is_empty() || self.skipped > 0 {
            return Err(
                if self.closed || !self.path.is_empty() || self.skipped > 0 {
                    "It ends before its elements close.".into()
                } else {
                    "It is not a camera's definition: its outer element is not mavlinkcamera."
                        .to_owned()
                },
            );
        }
        let names: std::collections::BTreeSet<String> = self
            .definition
            .settings
            .iter()
            .map(|setting| setting.name.clone())
            .collect();
        for setting in &mut self.definition.settings {
            setting.updates.retain(|name| names.contains(name));
            for choice in &mut setting.choices {
                choice.exclusions.retain(|name| names.contains(name));
                choice.ranges.retain(|range| names.contains(&range.setting));
            }
        }
        Ok(self.definition)
    }
}

fn too_many(within: &str, what: &str) -> String {
    format!("It gives {within} more than {MAX_IN_A_LIST} {what}.")
}

/// An attribute's value, with XML's own entities resolved.
fn attribute(tag: &quick_xml::events::BytesStart, key: &str) -> Result<Option<String>, String> {
    for attribute in tag.attributes() {
        let attribute = attribute
            .map_err(|_| "It is not well-formed XML: an attribute is broken.".to_owned())?;
        if attribute.key.as_ref() != key {
            continue;
        }
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|_| {
                "It uses an entity that is not one of XML's own, which UXDuck does not read."
                    .to_owned()
            })?;
        return Ok(Some(value.trim().to_owned()));
    }
    Ok(None)
}

/// Whether this can be a setting's name: an extended parameter's, 1 to 16
/// plain characters, without those a condition is written with.
fn named(name: &str) -> bool {
    (1..=16).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'=' | b'!' | b'<' | b'>'))
}

fn number(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.len() > MAX_TEXT {
        return None;
    }
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// Free text without control characters. Text longer than [`MAX_TEXT`]
/// characters is cut there and ends in "…", the mark that it was cut.
fn cut(text: &str) -> String {
    let mut all = text.trim().chars().filter(|c| !c.is_control());
    let mut kept: String = all.by_ref().take(MAX_TEXT).collect();
    if all.next().is_some() {
        kept.push('…');
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIM: &str = include_str!("payload/camera/definition.xml");

    fn xz(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut text.as_bytes(), &mut out).unwrap();
        out
    }

    fn wrap(parameters: &str) -> String {
        format!("<mavlinkcamera><parameters>{parameters}</parameters></mavlinkcamera>")
    }

    fn refused(text: &str) -> String {
        parse(text.as_bytes()).unwrap_err()
    }

    #[test]
    fn the_simulated_cameras_file_is_read() {
        let file = parse(SIM.as_bytes()).unwrap();
        assert_eq!(
            (
                file.vendor.as_str(),
                file.model.as_str(),
                file.version.as_str()
            ),
            ("UXDuck", "SimCamera", "1")
        );
        let names: Vec<&str> = file.settings.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "CAM_MODE",
                "CAM_EXPMODE",
                "CAM_EV",
                "CAM_ISO",
                "CAM_SHUTTERSPD",
                "CAM_VIDRES",
                "CAM_VIDFPS",
                "CAM_PHOTOFMT",
                "CAM_SHUTTERCNT"
            ]
        );
        let mode = file.setting("CAM_MODE").unwrap();
        assert!(!mode.control, "the mode has its own control already");
        let ev = file.setting("CAM_EV").unwrap();
        assert_eq!(
            (ev.kind, ev.min, ev.max, ev.step, ev.unit.as_deref()),
            (
                CameraSettingKind::Float,
                Some(-3.0),
                Some(3.0),
                Some(0.5),
                Some("EV")
            )
        );
        assert!(ev.choices.is_empty());
        let iso = file.setting("CAM_ISO").unwrap();
        assert_eq!(iso.description, "ISO");
        assert_eq!(iso.choices.len(), 6);
        assert_eq!(
            file.setting("CAM_EXPMODE").unwrap().updates,
            ["CAM_ISO", "CAM_SHUTTERSPD"]
        );
        assert!(file.setting("CAM_SHUTTERCNT").unwrap().read_only);
    }

    #[test]
    fn an_xz_packed_file_is_unpacked() {
        assert_eq!(parse(&xz(SIM)).unwrap(), parse(SIM.as_bytes()).unwrap());
    }

    #[test]
    fn a_choice_rules_out_the_settings_it_excludes() {
        let file = parse(SIM.as_bytes()).unwrap();
        let auto = |name: &str| match name {
            "CAM_MODE" | "CAM_EXPMODE" => Some(0.0),
            _ => None,
        };
        assert_eq!(file.excluded_by("CAM_ISO", &auto), Some("CAM_EXPMODE"));
        assert_eq!(file.excluded_by("CAM_VIDRES", &auto), Some("CAM_MODE"));
        assert_eq!(file.excluded_by("CAM_EV", &auto), None);
        let manual = |name: &str| (name == "CAM_EXPMODE").then_some(1.0);
        assert_eq!(file.excluded_by("CAM_ISO", &manual), None);
        assert_eq!(file.excluded_by("CAM_EV", &manual), Some("CAM_EXPMODE"));
        let unknown = |_: &str| None;
        assert_eq!(
            file.excluded_by("CAM_ISO", &unknown),
            None,
            "nothing is ruled out before the camera has said its values"
        );
    }

    #[test]
    fn a_choice_limits_another_settings_values() {
        let file = parse(SIM.as_bytes()).unwrap();
        let rate = file.setting("CAM_VIDFPS").unwrap();
        let values = |at: f64| {
            let now = move |name: &str| (name == "CAM_VIDRES").then_some(at);
            file.choices(rate, &now)
                .into_iter()
                .map(|(_, value)| value)
                .collect::<Vec<_>>()
        };
        assert_eq!(values(1.0), [24.0, 30.0, 60.0]);
        assert_eq!(values(0.0), [24.0, 30.0], "the largest size has no 60");
    }

    #[test]
    fn a_limit_with_a_condition_holds_only_while_it_does() {
        let file = parse(
            wrap(
                r#"<parameter name="CAM_MODE" type="uint32">
                    <options>
                        <option name="Photo" value="0" />
                        <option name="Video" value="1">
                            <parameterranges>
                                <parameterrange parameter="CAM_ISO" condition="CAM_EXPMODE=1 AND CAM_EV&lt;2">
                                    <roption name="100" value="100" />
                                </parameterrange>
                                <parameterrange parameter="CAM_GONE">
                                    <roption name="1" value="1" />
                                </parameterrange>
                            </parameterranges>
                        </option>
                    </options>
                </parameter>
                <parameter name="CAM_EXPMODE" type="uint32" />
                <parameter name="CAM_EV" type="float" />
                <parameter name="CAM_ISO" type="uint32">
                    <options>
                        <option name="100" value="100" />
                        <option name="200" value="200" />
                    </options>
                </parameter>"#,
            )
            .as_bytes(),
        )
        .unwrap();
        let iso = file.setting("CAM_ISO").unwrap();
        let count = |mode: f64, exposure: f64, ev: f64| {
            let now = move |name: &str| match name {
                "CAM_MODE" => Some(mode),
                "CAM_EXPMODE" => Some(exposure),
                "CAM_EV" => Some(ev),
                _ => None,
            };
            file.choices(iso, &now).len()
        };
        assert_eq!(count(1.0, 1.0, 0.0), 1);
        assert_eq!(count(1.0, 0.0, 0.0), 2, "the exposure is not manual");
        assert_eq!(count(1.0, 1.0, 2.5), 2, "the second test does not hold");
        assert_eq!(count(0.0, 1.0, 0.0), 2, "in photo mode");
        let video = &file.setting("CAM_MODE").unwrap().choices[1];
        assert_eq!(
            video.ranges.len(),
            1,
            "a limit on a setting the file does not name is dropped"
        );
    }

    #[test]
    fn conditions_join_left_to_right() {
        let now = |name: &str| match name {
            "A" => Some(1.0),
            "B" => Some(5.0),
            _ => None,
        };
        let holds = |text: &str| Condition::of(text).holds(&now);
        assert!(holds("A=1"));
        assert!(!holds("A!=1"));
        assert!(holds("B>4 AND A=1"));
        assert!(!holds("B<4 AND A=1"));
        assert!(holds("B<4 OR A=1"));
        assert!(holds("B<4 and A=2 OR B=5"));
        assert!(!holds("C=1"), "a setting with no value");
        assert!(!holds("A"), "not a test");
        assert!(!holds("A=one"), "not a number");
    }

    #[test]
    fn a_float_value_matches_as_the_camera_holds_it() {
        #[allow(clippy::cast_possible_truncation, reason = "the point of the test")]
        let held = f64::from(0.016_666_f64 as f32);
        assert!(same(0.016_666, held));
        assert!(!same(0.016_666, 0.033_333));
        assert!(same(3.0, 3.0));
    }

    #[test]
    fn a_document_type_is_refused() {
        assert_eq!(
            refused(
                "<!DOCTYPE mavlinkcamera [<!ENTITY a SYSTEM \"file:///etc/passwd\">]><mavlinkcamera>&a;</mavlinkcamera>"
            ),
            "It declares a document type, which UXDuck does not read."
        );
    }

    #[test]
    fn an_entity_that_is_not_xmls_own_is_refused() {
        assert_eq!(
            refused(&wrap(
                r#"<parameter name="A" type="bool"><description>&lol;</description></parameter>"#
            )),
            "It uses an entity that is not one of XML's own, which UXDuck does not read."
        );
        assert_eq!(
            refused(&wrap(r#"<parameter name="A" type="bool" unit="&lol;" />"#)),
            "It uses an entity that is not one of XML's own, which UXDuck does not read."
        );
        let file = parse(
            wrap(r#"<parameter name="A" type="bool"><description>Sun &amp; shade &#65;</description></parameter>"#)
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(file.settings[0].description, "Sun & shade A");
    }

    #[test]
    fn a_file_that_is_not_a_cameras_definition_is_refused() {
        assert_eq!(
            refused("<html><body>Sign in</body></html>"),
            "It is not a camera's definition: its outer element is not mavlinkcamera."
        );
        assert_eq!(
            refused("not xml at all"),
            "It is not a camera's definition: its outer element is not mavlinkcamera."
        );
        assert_eq!(refused(""), refused("  "));
        assert!(refused("<mavlinkcamera><parameters>").starts_with("It "));
        assert_eq!(
            parse(&[0xff, 0xfe, 0x00]).unwrap_err(),
            "It is not text UXDuck can read."
        );
        assert_eq!(
            refused("<mavlinkcamera></mavlinkcamera><mavlinkcamera></mavlinkcamera>"),
            "It has more than one outer element."
        );
    }

    #[test]
    fn broken_xml_is_refused_without_quoting_it() {
        let why = refused("<mavlinkcamera><parameters></secret-name></mavlinkcamera>");
        assert!(
            why.starts_with("It is not well-formed XML, near byte "),
            "{why}"
        );
        assert!(!why.contains("secret"), "{why}");
    }

    #[test]
    fn limits_refuse_the_whole_file() {
        let deep = format!(
            "<mavlinkcamera>{}{}</mavlinkcamera>",
            "<a>".repeat(16),
            "</a>".repeat(16)
        );
        assert_eq!(refused(&deep), "Its elements nest more than 16 deep.");
        let shallow = format!(
            "<mavlinkcamera>{}{}</mavlinkcamera>",
            "<a>".repeat(15),
            "</a>".repeat(15)
        );
        assert!(parse(shallow.as_bytes()).is_ok());

        let each = |count: usize, one: &dyn Fn(usize) -> String| {
            let mut all = String::new();
            for n in 0..count {
                all.push_str(&one(n));
            }
            all
        };
        let many = each(513, &|n| {
            format!(r#"<parameter name="P{n}" type="bool" />"#)
        });
        assert_eq!(refused(&wrap(&many)), "It names more than 512 settings.");

        let values = each(513, &|n| format!(r#"<option name="{n}" value="{n}" />"#));
        assert_eq!(
            refused(&wrap(&format!(
                r#"<parameter name="A" type="uint32"><options>{values}</options></parameter>"#
            ))),
            "It gives A more than 512 values."
        );

        let some = each(500, &|n| format!(r#"<option name="{n}" value="{n}" />"#));
        let all = each(41, &|n| {
            format!(r#"<parameter name="P{n}" type="uint32"><options>{some}</options></parameter>"#)
        });
        assert_eq!(
            refused(&wrap(&all)),
            "It lists more than 20000 values in all."
        );
    }

    #[test]
    fn a_setting_the_protocol_cannot_name_or_type_is_refused() {
        let one = |attributes: &str| refused(&wrap(&format!("<parameter {attributes} />")));
        let name = "It names a setting with something other than 1 to 16 plain characters.";
        assert_eq!(one(r#"type="bool""#), name);
        assert_eq!(one(r#"name="" type="bool""#), name);
        assert_eq!(one(r#"name="SEVENTEEN_LETTERS" type="bool""#), name);
        assert_eq!(one(r#"name="CAM ISO" type="bool""#), name);
        assert_eq!(one(r#"name="CAM=ISO" type="bool""#), name);
        assert_eq!(one(r#"name="CAM_ISÖ" type="bool""#), name);
        assert_eq!(
            one(r#"name="A" type="string""#),
            "It gives A a type that is not one a camera's setting has."
        );
        assert_eq!(
            one(r#"name="A""#),
            "It gives A a type that is not one a camera's setting has."
        );
        assert_eq!(
            one(r#"name="A" type="float" min="low""#),
            "It gives A a min that is not a number."
        );
        assert_eq!(
            one(r#"name="A" type="float" max="inf""#),
            "It gives A a max that is not a number."
        );
        assert_eq!(
            one(r#"name="A" type="float" min="2" max="1""#),
            "It gives A a least value above its most."
        );
        assert_eq!(
            refused(&wrap(
                r#"<parameter name="A" type="bool" /><parameter name="A" type="bool" />"#
            )),
            "It names A twice."
        );
        assert_eq!(
            refused(&wrap(
                r#"<parameter name="A" type="uint8"><options><option name="x" value="x" /></options></parameter>"#
            )),
            "It gives A a value that is not a number."
        );
        assert_eq!(
            refused(&wrap(
                r#"<parameter name="A" type="uint8"><updates><update>NOT A NAME</update></updates></parameter>"#
            )),
            "Under A it names a setting with something other than 1 to 16 plain characters."
        );
    }

    // Text as long as is kept is whole, and is not marked as cut.
    #[test]
    fn free_text_that_fits_is_not_marked() {
        let fits = "y".repeat(256);
        let file = parse(
            format!(
                r#"<mavlinkcamera><definition version="1"><model>M</model><vendor>V</vendor></definition>
                <parameters><parameter name="A" type="bool"><description>  {fits}  </description></parameter></parameters>
                </mavlinkcamera>"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(file.settings[0].description, fits);
        assert_eq!(file.text_cut(), None);
    }

    #[test]
    fn what_is_not_read_is_skipped_and_free_text_is_cut() {
        let long = "x".repeat(1000);
        let file = parse(
            format!(
                r#"<?xml version="1.0" encoding="UTF-8" ?>
                <mavlinkcamera>
                    <definition version="7"><model>M</model><vendor>V</vendor><extra><model>No</model></extra></definition>
                    <parameters>
                        <parameter name="A" type="uint8" default="1">
                            <description>{long}</description>
                            <script>alert(1)</script>
                            <options><option name="&lt;b&gt;One&lt;/b&gt;" value="1"><exclusions><exclude>B</exclude><exclude>GONE</exclude></exclusions></option></options>
                            <updates><update>B</update><update>GONE</update></updates>
                        </parameter>
                        <parameter name="B" type="bool"><options><option name="On" value="1" /></options></parameter>
                        <group><parameter name="C" type="bool" /></group>
                    </parameters>
                    <localization><locale name="de_DE"><strings original="One" translated="Eins" /></locale></localization>
                </mavlinkcamera>"#
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!((file.version.as_str(), file.model.as_str()), ("7", "M"));
        assert_eq!(
            file.settings.len(),
            2,
            "a setting inside an unknown element is not one"
        );
        let a = &file.settings[0];
        assert_eq!(
            a.description,
            format!("{}…", "x".repeat(256)),
            "256 characters, and a mark that the file said more"
        );
        assert_eq!(
            file.text_cut().as_deref(),
            Some(
                "The camera's definition file has 1 text longer than 256 characters. UXDuck shows its first 256 characters, and … marks where it was cut."
            )
        );
        assert_eq!(a.choices[0].name, "<b>One</b>", "kept as text");
        assert_eq!(a.choices[0].exclusions, ["B"]);
        assert_eq!(a.updates, ["B"]);
        let b = &file.settings[1];
        assert_eq!(b.description, "B", "its name, when the file says no more");
        assert!(b.choices.is_empty(), "on or off");
    }

    #[test]
    fn a_packed_file_that_says_it_unpacks_too_large_is_refused_unread() {
        // 64 MiB of zeros in under 10 KB, and the same with its body
        // damaged: the size is the answer for both, so neither was unpacked.
        let (bomb, damaged) = crate::xz::bomb();
        for file in [bomb, damaged.as_slice()] {
            assert_eq!(
                parse(file).unwrap_err(),
                "It unpacks to more than 4 MB, the most UXDuck reads for a camera's definition."
            );
        }
    }

    #[test]
    fn a_file_that_unpacks_too_large_is_refused() {
        let big = format!(
            "<mavlinkcamera><!--{}--></mavlinkcamera>",
            "x".repeat(MAX_UNPACKED)
        );
        assert_eq!(
            parse(&xz(&big)).unwrap_err(),
            "It unpacks to more than 4 MB, the most UXDuck reads for a camera's definition."
        );
        assert_eq!(
            parse(big.as_bytes()).unwrap_err(),
            "It is larger than 4 MB, the most UXDuck reads for a camera's definition."
        );
        let mut broken = xz(SIM);
        let middle = broken.len() / 2;
        broken.truncate(middle);
        assert_eq!(
            parse(&broken).unwrap_err(),
            "It is xz-compressed and could not be unpacked."
        );
    }
}
