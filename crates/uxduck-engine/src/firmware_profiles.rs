//! A fork's firmware profiles (ADR-0044 F12, S254): one file its CI
//! publishes, `{ "uxduck": "firmware/1", "label", "firmware", "version"?, "commit"?, "parameters"?, "dialects"? }`,
//! naming its parameter descriptions and its MAVLink dialects by URLs
//! relative to its own. Added once; the person's set and the dialect it
//! brings are its own, bound to its commit, and fetched again daily and on
//! a refresh. A refresh that fails keeps what was read before and says why.

use serde::Deserialize;
use serde_json::{Value, json};
use uxduck_contracts::dto::firmware_profile::{self as dto, FirmwareListResult};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::dialect::Dialect;
use uxduck_core::domain::firmware_profile::FirmwareProfile;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::param_definitions::{Firmware, OwnSet};
use uxduck_core::events::{Actor, Change};
use uxduck_mavlink::custom;

use crate::Engine;
use crate::commands::Raise;
use crate::param_definitions::DefinitionsOrder;
use crate::rpc::handlers::to_value;

/// The most profiles people may add.
const MAX_PROFILES: usize = 16;
/// The most dialect files one profile names.
const MAX_DIALECT_FILES: usize = 8;
/// The most bytes of XML one profile's dialects hold together.
const MAX_DIALECT_BYTES: usize = 8 << 20;
const SCHEMA: &str = "firmware/1";

/// A profile being read, and why it last failed. Not journaled.
#[derive(Debug, Default)]
pub(crate) struct Run {
    problem: Option<String>,
    generation: u32,
    reading: Option<Reading>,
}

/// A profile's dialect files arriving, for the read numbered `generation`.
#[derive(Debug)]
struct Reading {
    generation: u32,
    read: Read,
    texts: Vec<Option<Result<String, String>>>,
}

/// What a profile says, its URLs resolved against its own.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Read {
    label: String,
    firmware: Firmware,
    version: Option<String>,
    commit: Option<String>,
    parameters: Option<String>,
    dialects: Vec<String>,
}

#[derive(Deserialize)]
struct File {
    uxduck: String,
    label: String,
    firmware: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    parameters: Option<String>,
    #[serde(default)]
    dialects: Vec<String>,
}

fn actor() -> Actor {
    Actor {
        client_id: "firmware-profiles".into(),
        operator_id: None,
        identity: None,
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn condition(id: &str) -> String {
    format!("firmware-profile:{id}")
}

/// Where UXDuck fetches from: `https://`, or a server on this computer.
fn fetchable(url: &str) -> bool {
    url.len() <= 2048
        && (url.starts_with("https://")
            || url.starts_with("http://127.0.0.1")
            || url.starts_with("http://localhost"))
}

/// A name a person gives a profile, trimmed; none when it is empty.
fn profile_name(name: &str) -> Result<Option<String>, ApiError> {
    let name = name.trim();
    if name.chars().count() > 64 {
        return Err(invalid("a name has at most 64 characters"));
    }
    Ok((!name.is_empty()).then(|| name.to_owned()))
}

/// `reference` read relative to the profile at `base`.
fn resolve(base: &str, reference: &str) -> Result<String, String> {
    let reference = reference.trim();
    let resolved = if reference.contains("://") {
        reference.to_owned()
    } else {
        let base = base.split(['?', '#']).next().unwrap_or(base);
        let (scheme, rest) = base
            .split_once("://")
            .ok_or_else(|| format!("{base} is not a URL"))?;
        let mut path: Vec<&str> = rest.split('/').collect();
        let host = path.remove(0);
        path.pop();
        if reference.starts_with('/') {
            path.clear();
        }
        for segment in reference.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    path.pop();
                }
                segment => path.push(segment),
            }
        }
        format!("{scheme}://{host}/{}", path.join("/"))
    };
    if fetchable(&resolved) {
        Ok(resolved)
    } else {
        Err(format!("{reference} is not an https:// address"))
    }
}

/// A file's name: the last part of its URL.
fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// A profile's text, read and checked.
fn read(text: &str, base: &str) -> Result<Read, String> {
    let file: File =
        serde_json::from_str(text).map_err(|e| format!("this is not a firmware profile: {e}"))?;
    if file.uxduck != SCHEMA {
        return Err(format!(
            "the profile is {}. This UXDuck reads {SCHEMA}",
            file.uxduck
        ));
    }
    let label = file.label.trim().to_owned();
    if label.is_empty() || label.chars().count() > 64 {
        return Err("a profile's label has 1 to 64 characters".into());
    }
    let firmware = Firmware::parse(file.firmware.trim()).ok_or_else(|| {
        format!(
            "{} is not an ArduPilot firmware, such as ArduCopter",
            file.firmware
        )
    })?;
    let commit = match file
        .commit
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty())
    {
        Some(c) if (7..=40).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Some(c)
        }
        Some(_) => return Err("a commit is 7 to 40 hex characters".into()),
        None => None,
    };
    if file.parameters.is_none() && file.dialects.is_empty() {
        return Err("the profile names no parameters and no dialects".into());
    }
    if file.dialects.len() > MAX_DIALECT_FILES {
        return Err(format!(
            "a profile names at most {MAX_DIALECT_FILES} dialect files"
        ));
    }
    Ok(Read {
        label,
        firmware,
        version: file
            .version
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty()),
        commit,
        parameters: file
            .parameters
            .as_deref()
            .map(|p| resolve(base, p))
            .transpose()?,
        dialects: file
            .dialects
            .iter()
            .map(|d| resolve(base, d))
            .collect::<Result<_, _>>()?,
    })
}

impl Engine {
    /// Fetches a profile again, such as one from another computer.
    pub(crate) fn fetch_profile(&mut self, id: &str) {
        if let Some(url) = self.state.firmware_profiles.get(id).map(|p| p.url.clone()) {
            self.ask_definitions(DefinitionsOrder::FetchProfile {
                id: id.to_owned(),
                url,
            });
        }
    }

    /// Fetches every profile again: daily, and on a refresh.
    pub(crate) fn refresh_profiles(&mut self) {
        let ids: Vec<String> = self.state.firmware_profiles.keys().cloned().collect();
        for id in ids {
            self.fetch_profile(&id);
        }
    }

    fn add_profile(&mut self, url: &str, name: Option<&str>) -> Result<FirmwareProfile, ApiError> {
        let url = url.trim();
        if !fetchable(url) {
            return Err(invalid("a firmware profile's URL starts with https://"));
        }
        let name = name.map(profile_name).transpose()?.flatten();
        let name = name.as_deref();
        if self.state.firmware_profiles.values().any(|p| p.url == url) {
            return Err(invalid("this firmware profile is already added"));
        }
        if self.state.firmware_profiles.len() >= MAX_PROFILES {
            return Err(invalid(format!(
                "UXDuck keeps at most {MAX_PROFILES} firmware profiles. Remove one first"
            )));
        }
        let now = self.clock.now_us();
        let profile = FirmwareProfile {
            id: format!("firmware-{}", &self.random.token()[..12]),
            url: url.to_owned(),
            label: name
                .unwrap_or_else(|| url.split('/').nth(2).unwrap_or(url))
                .to_owned(),
            name: name.map(str::to_owned),
            firmware: None,
            version: None,
            commit: None,
            own_set_id: None,
            dialect_id: None,
            added_at_us: now,
            fetched_at_us: None,
            updated_at_us: now,
        };
        if !self.ask_definitions(DefinitionsOrder::FetchProfile {
            id: profile.id.clone(),
            url: profile.url.clone(),
        }) {
            return Err(invalid("this UXDuck cannot fetch firmware profiles"));
        }
        self.commit_as(
            actor(),
            Change::FirmwareProfileUpserted {
                profile: profile.clone(),
            },
        );
        Ok(profile)
    }

    /// Renames a profile, with the set and dialect it brought.
    fn rename_profile(&mut self, id: &str, name: &str) -> Result<FirmwareProfile, ApiError> {
        let profile = self
            .state
            .firmware_profiles
            .get(id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(ErrorKind::NotFound, format!("no firmware profile {id}"))
            })?;
        let name = profile_name(name)?.ok_or_else(|| invalid("a firmware profile needs a name"))?;
        if let Some(mut own) = profile
            .own_set_id
            .as_ref()
            .and_then(|own| self.state.own_sets.get(own))
            .cloned()
            .filter(|own| own.label != name)
        {
            own.label.clone_from(&name);
            self.commit_as(actor(), Change::OwnSetUpserted { set: own });
        }
        let now = self.clock.now_us();
        if let Some(mut dialect) = profile
            .dialect_id
            .as_ref()
            .and_then(|dialect| self.state.dialects.get(dialect))
            .cloned()
            .filter(|dialect| dialect.label != name)
        {
            dialect.label.clone_from(&name);
            dialect.updated_at_us = now.max(dialect.updated_at_us + 1);
            self.commit_as(actor(), Change::DialectUpserted { dialect });
        }
        if profile.name.as_deref() == Some(name.as_str()) && profile.label == name {
            return Ok(profile);
        }
        let renamed = FirmwareProfile {
            label: name.clone(),
            name: Some(name),
            updated_at_us: now.max(profile.updated_at_us + 1),
            ..profile
        };
        self.commit_as(
            actor(),
            Change::FirmwareProfileUpserted {
                profile: renamed.clone(),
            },
        );
        Ok(renamed)
    }

    /// Removes a profile with the set and dialect it brought.
    fn remove_profile(&mut self, id: &str) -> Result<(), ApiError> {
        let profile = self
            .state
            .firmware_profiles
            .get(id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(ErrorKind::NotFound, format!("no firmware profile {id}"))
            })?;
        if let Some(own) = profile
            .own_set_id
            .filter(|own| self.state.own_sets.contains_key(own))
        {
            self.remove_own_set(&own, None);
        }
        if let Some(dialect) = profile
            .dialect_id
            .filter(|dialect| self.state.dialects.contains_key(dialect))
        {
            self.commit_as(actor(), Change::DialectRemoved { id: dialect });
        }
        self.definitions.profiles.remove(id);
        self.resolve(&condition(id));
        self.commit_as(
            actor(),
            Change::FirmwareProfileRemoved { id: id.to_owned() },
        );
        Ok(())
    }

    /// An answer for a profile, or for one of the files it names (their
    /// ids are `<profile>#<read>#<file>`): true when it was one of these.
    pub(crate) fn profile_fetched(&mut self, id: &str, result: &Result<String, String>) -> bool {
        if self.state.firmware_profiles.contains_key(id) {
            self.profile_read(id, result.clone());
            return true;
        }
        let Some((profile, rest)) = id.split_once('#') else {
            return false;
        };
        if let Some((generation, index)) = rest
            .split_once('#')
            .and_then(|(g, i)| Some((g.parse().ok()?, i.parse().ok()?)))
        {
            self.dialect_file_fetched(profile, generation, index, result.clone());
        }
        true
    }

    /// The profile itself arrived: its parameter set brought up to date and
    /// fetched, and its dialect files asked for.
    fn profile_read(&mut self, id: &str, result: Result<String, String>) {
        let Some(profile) = self.state.firmware_profiles.get(id).cloned() else {
            return;
        };
        let read = match result.and_then(|text| read(&text, &profile.url)) {
            Ok(read) => Read {
                label: profile.name.clone().unwrap_or(read.label),
                ..read
            },
            Err(why) => return self.profile_failed(&profile, &why),
        };
        let own_set_id = if let Some(url) = &read.parameters {
            Some(self.profile_parameters(&profile, &read, url))
        } else {
            if let Some(own) = profile
                .own_set_id
                .clone()
                .filter(|own| self.state.own_sets.contains_key(own))
            {
                self.remove_own_set(&own, None);
            }
            None
        };
        let mut dialect_id = profile.dialect_id.clone();
        let run = self.definitions.profiles.entry(id.to_owned()).or_default();
        run.problem = None;
        let mut ask = Vec::new();
        if read.dialects.is_empty() {
            run.reading = None;
        } else {
            run.generation += 1;
            let generation = run.generation;
            ask = read
                .dialects
                .iter()
                .enumerate()
                .map(|(index, url)| DefinitionsOrder::FetchProfile {
                    id: format!("{id}#{generation}#{index}"),
                    url: url.clone(),
                })
                .collect();
            run.reading = Some(Reading {
                generation,
                read: read.clone(),
                texts: vec![None; read.dialects.len()],
            });
        }
        if read.dialects.is_empty()
            && let Some(dialect) = dialect_id
                .take()
                .filter(|dialect| self.state.dialects.contains_key(dialect))
        {
            self.commit_as(actor(), Change::DialectRemoved { id: dialect });
        }
        let now = self.clock.now_us();
        let updated = FirmwareProfile {
            label: read.label,
            firmware: Some(read.firmware),
            version: read.version,
            commit: read.commit,
            own_set_id,
            dialect_id,
            fetched_at_us: Some(now),
            updated_at_us: now.max(profile.updated_at_us + 1),
            ..profile
        };
        self.commit_as(
            actor(),
            Change::FirmwareProfileUpserted { profile: updated },
        );
        self.resolve(&condition(id));
        for order in ask {
            self.ask_definitions(order);
        }
    }

    /// The person's set holding a profile's parameter descriptions, kept
    /// in step with the profile and fetched from `url`; its id.
    fn profile_parameters(&mut self, profile: &FirmwareProfile, read: &Read, url: &str) -> String {
        let id = profile
            .own_set_id
            .clone()
            .unwrap_or_else(|| format!("{}-parameters", profile.id));
        let existing = self.state.own_sets.get(&id).cloned();
        let own = OwnSet {
            id: id.clone(),
            label: read.label.clone(),
            firmware: read.firmware,
            version: read.version.clone(),
            commit: read.commit.clone(),
            vehicle_id: None,
            parameter_count: existing.as_ref().map_or(0, |e| e.parameter_count),
            added_at_us: existing
                .as_ref()
                .map_or_else(|| self.clock.now_us(), |e| e.added_at_us),
            url: Some(url.to_owned()),
            fetched_at_us: existing.as_ref().and_then(|e| e.fetched_at_us),
            profile_id: Some(profile.id.clone()),
        };
        if existing.as_ref() != Some(&own) {
            self.commit_as(actor(), Change::OwnSetUpserted { set: own });
        }
        self.ask_definitions(DefinitionsOrder::FetchOwn {
            id: id.clone(),
            url: url.to_owned(),
        });
        id
    }

    /// One of a profile's dialect files arrived; with the last, the dialect
    /// is read again.
    fn dialect_file_fetched(
        &mut self,
        profile_id: &str,
        generation: u32,
        index: usize,
        result: Result<String, String>,
    ) {
        let Some(run) = self.definitions.profiles.get_mut(profile_id) else {
            return;
        };
        let Some(reading) = run
            .reading
            .as_mut()
            .filter(|reading| reading.generation == generation)
        else {
            return;
        };
        let Some(slot) = reading.texts.get_mut(index) else {
            return;
        };
        *slot = Some(result);
        if reading.texts.iter().any(Option::is_none) {
            return;
        }
        let Some(reading) = run.reading.take() else {
            return;
        };
        let Some(profile) = self.state.firmware_profiles.get(profile_id).cloned() else {
            return;
        };
        match self.profile_dialect(&profile, reading) {
            Ok(()) => self.resolve(&condition(profile_id)),
            Err(why) => self.profile_failed(&profile, &why),
        }
    }

    /// A profile's dialect read from its files and kept, bound to its
    /// commit, when it changed.
    fn profile_dialect(
        &mut self,
        profile: &FirmwareProfile,
        reading: Reading,
    ) -> Result<(), String> {
        let mut files: Vec<(String, String)> = Vec::new();
        for (url, text) in reading.read.dialects.iter().zip(reading.texts) {
            let text = text
                .unwrap_or_else(|| Err("not fetched".into()))
                .map_err(|why| format!("{}: {why}", file_name(url)))?;
            files.push((file_name(url), text));
        }
        if files.iter().map(|(_, text)| text.len()).sum::<usize>() > MAX_DIALECT_BYTES {
            return Err("the dialect files are larger than 8 MB together".into());
        }
        let pairs: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();
        let read = custom::read(&pairs)?;
        let id = profile
            .dialect_id
            .clone()
            .unwrap_or_else(|| format!("{}-dialect", profile.id));
        let existing = self.state.dialects.get(&id).cloned();
        let now = self.clock.now_us();
        let mut dialect = Dialect {
            id: id.clone(),
            label: reading.read.label.clone(),
            files: files.into_iter().map(|(name, _)| name).collect(),
            commits: reading.read.commit.clone().into_iter().collect(),
            every_vehicle: reading.read.commit.is_none(),
            messages: read.messages,
            enums: read.enums,
            commands: read.commands,
            unchanged: read.unchanged,
            added_at_us: existing.as_ref().map_or(now, |e| e.added_at_us),
            updated_at_us: existing.as_ref().map_or(now, |e| e.updated_at_us),
            profile_id: Some(profile.id.clone()),
            places_read: true,
        };
        dialect.commits = Self::reach(&dialect, &dialect.commits, dialect.every_vehicle)
            .map_err(|e| e.message)?;
        if existing.as_ref() != Some(&dialect) {
            dialect.updated_at_us = existing
                .as_ref()
                .map_or(now, |e| now.max(e.updated_at_us + 1));
            self.commit_as(actor(), Change::DialectUpserted { dialect });
        }
        if profile.dialect_id.as_deref() != Some(id.as_str()) {
            let mut updated = profile.clone();
            updated.dialect_id = Some(id);
            updated.updated_at_us = now.max(profile.updated_at_us + 1);
            self.commit_as(
                actor(),
                Change::FirmwareProfileUpserted { profile: updated },
            );
        }
        Ok(())
    }

    /// A profile, or a file it names, could not be read: what it brought
    /// before stays, and a notice says why.
    fn profile_failed(&mut self, profile: &FirmwareProfile, why: &str) {
        self.definitions
            .profiles
            .entry(profile.id.clone())
            .or_default()
            .problem = Some(why.to_owned());
        let (title, body) = if profile.fetched_at_us.is_some() {
            (
                format!("Couldn't refresh the firmware profile {}", profile.label),
                format!("{why}. UXDuck keeps what it read before."),
            )
        } else {
            (
                format!("Couldn't read the firmware profile from {}", profile.label),
                format!("{why}."),
            )
        };
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::System,
            title,
            body: Some(body.replace("..", ".")),
            actions: Vec::new(),
            condition: Some(condition(&profile.id)),
            report: None,
        });
    }

    fn profile_summary(&self, profile: &FirmwareProfile) -> dto::FirmwareProfile {
        dto::FirmwareProfile {
            problem: self
                .definitions
                .profiles
                .get(&profile.id)
                .and_then(|run| run.problem.clone()),
            ..profile.into()
        }
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::FirmwareList(params) => {
            require(Capability::Read)?;
            if params.refresh {
                engine.refresh_profiles();
            }
            let mut profiles: Vec<&FirmwareProfile> =
                engine.state.firmware_profiles.values().collect();
            profiles.sort_by_key(|p| std::cmp::Reverse(p.added_at_us));
            to_value(&FirmwareListResult {
                profiles: profiles
                    .into_iter()
                    .map(|p| engine.profile_summary(p))
                    .collect(),
            })
        }
        ClientRequest::FirmwareAdd(params) => {
            require(Capability::Config)?;
            let profile = engine.add_profile(&params.url, params.name.as_deref())?;
            to_value(&engine.profile_summary(&profile))
        }
        ClientRequest::FirmwareRename(params) => {
            require(Capability::Config)?;
            let profile = engine.rename_profile(&params.id, &params.name)?;
            to_value(&engine.profile_summary(&profile))
        }
        ClientRequest::FirmwareRemove(params) => {
            require(Capability::Config)?;
            engine.remove_profile(&params.id)?;
            Ok(json!({}))
        }
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not a firmware profile method",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_found_beside_the_profile() {
        let base = "https://ci.acme.example/builds/r12/profile.json?token=1";
        assert_eq!(
            resolve(base, "apm.pdef.xml").unwrap(),
            "https://ci.acme.example/builds/r12/apm.pdef.xml"
        );
        assert_eq!(
            resolve(base, "../mavlink/./acme.xml").unwrap(),
            "https://ci.acme.example/builds/mavlink/acme.xml"
        );
        assert_eq!(
            resolve(base, "/shared/common.xml").unwrap(),
            "https://ci.acme.example/shared/common.xml"
        );
        assert_eq!(
            resolve(base, "https://cdn.acme.example/acme.xml").unwrap(),
            "https://cdn.acme.example/acme.xml"
        );
        assert!(resolve(base, "http://acme.example/acme.xml").is_err());
        assert_eq!(
            resolve("http://127.0.0.1:8000/profile.json", "acme.xml").unwrap(),
            "http://127.0.0.1:8000/acme.xml"
        );
        assert_eq!(file_name("https://a.example/x/acme.xml?v=2"), "acme.xml");
    }

    #[test]
    fn a_profile_is_read_and_checked() {
        let base = "https://ci.acme.example/r12/profile.json";
        let ok = read(
            r#"{ "uxduck": "firmware/1", "label": "Acme r12", "firmware": "Copter",
                 "version": "4.5.7-acme12", "commit": "2A3DC4B7BF9E",
                 "parameters": "apm.pdef.xml", "dialects": ["acme.xml"] }"#,
            base,
        )
        .unwrap();
        assert_eq!(ok.firmware, Firmware::ArduCopter);
        assert_eq!(ok.commit.as_deref(), Some("2a3dc4b7bf9e"));
        assert_eq!(
            ok.parameters.as_deref(),
            Some("https://ci.acme.example/r12/apm.pdef.xml")
        );
        let refused = |text: &str| read(text, base).unwrap_err();
        assert!(refused(r#"{ "uxduck": "firmware/2", "label": "A", "firmware": "ArduCopter", "dialects": ["a.xml"] }"#).contains("firmware/1"));
        assert!(
            refused(r#"{ "uxduck": "firmware/1", "label": "A", "firmware": "Copter" }"#)
                .contains("no parameters")
        );
        assert!(refused(r#"{ "uxduck": "firmware/1", "label": "A", "firmware": "PX4", "dialects": ["a.xml"] }"#).contains("PX4"));
        assert!(refused(r#"{ "uxduck": "firmware/1", "label": "A", "firmware": "Copter", "commit": "xyz", "dialects": ["a.xml"] }"#).contains("hex"));
        assert!(refused("<html>").contains("not a firmware profile"));
    }
}
