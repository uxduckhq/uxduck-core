//! Cloud sync (ADR-0001 D9, ADR-0002, S16): whether this computer mirrors
//! its work to the account, how far it has got each way, and the account's
//! library of vehicles published by other computers.

use serde::{Deserialize, Serialize};

/// Which vehicles from the account this computer uses (ADR-0001 D9,
/// "activation per device").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Activation {
    /// Only the ones someone chooses here.
    #[default]
    Pick,
    /// Every one, as it arrives.
    MirrorAll,
}

/// What a person can choose to keep off the account (2026-10-24): sync is
/// everything until they leave something out. Flights are not here: each
/// goes only when someone saves it (C9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncCategory {
    /// Operators with their display settings and layouts, and plugins
    /// they published.
    Settings,
    /// Chats and their folders.
    Chats,
    Places,
    /// Saved vehicles, their history and snapshots, connections,
    /// cameras and parameter definition sets.
    Vehicles,
    Plans,
}

impl SyncCategory {
    /// The category an entity belongs to; none for one that is always the
    /// person's own choice, item by item.
    #[must_use]
    pub fn of(entity: &str) -> Option<Self> {
        Some(match entity {
            "operator" | "plugin" => Self::Settings,
            "thread" | "item" | "folder" => Self::Chats,
            "place" => Self::Places,
            "vehicle" | "history" | "snapshot" | "link" | "camera" | "own-set" | "dialect"
            | "firmware-profile" => Self::Vehicles,
            "plan" => Self::Plans,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    /// On unless the person turned it off (maintainer, 2026-11-06: "lets
    /// auto sync it - not get people to opt in"). A computer turned off has
    /// a `device_id`, so it stays off; one from before, off without one,
    /// is turned on once.
    pub enabled: bool,
    #[serde(default)]
    pub activation: Activation,
    /// What stays off the account; empty syncs everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SyncCategory>,
    /// This computer in the account; minted when sync is first turned on.
    #[serde(default)]
    pub device_id: String,
    /// The export cursor: this computer's events up to here are in the
    /// account, or were not the account's to have.
    #[serde(default)]
    pub exported_through: u64,
    /// Changes sent so far.
    #[serde(default)]
    pub exported: u64,
    /// The account's cursor: changes from other computers taken up to here.
    #[serde(default)]
    pub pulled_through: u64,
    /// The kinds of entity the account leaves out unless they are asked
    /// for, that this computer has asked for since its cursor began (FB-229).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub takes: Vec<String>,
    /// Changes taken so far.
    #[serde(default)]
    pub pulled: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at_us: Option<u64>,
    /// The account this browser's work belongs to, from its first sync
    /// (ADR-0033): another account signed in here does not receive it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    /// What the account would not keep for its size (2026-11-14): larger
    /// than it takes at once, or more than it has room for (2026-11-15). It
    /// stays on this computer, said to the person, until it fits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub too_large: Vec<TooLarge>,
    #[serde(default)]
    pub version: u64,
}

/// One entity whose latest change the account refused for its size: it is
/// larger than the account takes at once, or the account has no room for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TooLarge {
    /// `plan`, `operator`, `history` and the rest, as synced.
    pub entity: String,
    pub id: String,
    /// The change's size as the account measured it; as this computer
    /// measured it when the account server did not say.
    pub bytes: u64,
    /// The largest change the account takes at once, when its server said.
    /// Never this computer's own copy of the number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// Set when the account had no room for it (2026-11-15): the change is
    /// not too large to take, the account's allowance is used up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_room: Option<NoRoom>,
    /// Set when the account would not take it for what it holds, not its
    /// size: a camera's copy its server read as carrying a login (FB-229).
    /// The server's reason, kept for the log. It is not sent again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_taken: Option<String>,
    /// Which copy was refused, so that copy is sent again once the account
    /// can take it. None for one listed before 2026-11-15.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<RefusedCopy>,
}

/// Why the account had no room for a change, in its server's numbers. Never
/// this computer's own copy of any of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoRoom {
    /// What keeping the change would add to the account: its copy among
    /// the kept versions, and as the entity's copy what it is larger by
    /// than the one it replaces.
    pub adds_bytes: u64,
    /// What the account held then.
    pub account_bytes: u64,
    /// What the account's plan allows.
    pub allowance_bytes: u64,
}

/// The copy of an entity the account refused, as it was sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusedCopy {
    pub version: u64,
    pub digest: String,
    pub ts_us: u64,
}

impl Default for SyncState {
    /// On from the start; its device id is minted when it is first needed.
    fn default() -> Self {
        Self {
            enabled: true,
            activation: Activation::default(),
            skipped: Vec::new(),
            device_id: String::new(),
            exported_through: 0,
            exported: 0,
            pulled_through: 0,
            takes: Vec::new(),
            pulled: 0,
            last_synced_at_us: None,
            account_id: String::new(),
            too_large: Vec::new(),
            version: 0,
        }
    }
}

impl SyncState {
    /// Whether changes to this entity go to the account and come from it.
    #[must_use]
    pub fn syncs(&self, entity: &str) -> bool {
        SyncCategory::of(entity).is_none_or(|category| !self.skipped.contains(&category))
    }

    /// Whether something stays here for want of room in the account. Room
    /// appears there without anything changing here, so a computer that
    /// has nothing to send asks the account what it holds.
    #[must_use]
    pub fn waits_for_room(&self) -> bool {
        self.too_large.iter().any(|one| one.no_room.is_some())
    }
}

/// A vehicle another computer published to the account: its profile, not
/// where it was last heard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryVehicle {
    pub id: String,
    pub name: String,
    pub autopilot: String,
    pub vehicle_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// Its size on the map in centimetres, as the person gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
    /// Its colour, as given or chosen (ADR-0029 P19), so it is the same on
    /// every computer of the account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<crate::domain::vehicle::VehicleColour>,
    /// Which parameter descriptions it uses, as a person chose (ADR-0044
    /// F10).
    #[serde(
        default,
        skip_serializing_if = "crate::domain::param_definitions::DefinitionsChoice::is_automatic"
    )]
    pub definitions: crate::domain::param_definitions::DefinitionsChoice,
    /// Which MAVLink dialects it uses, as a person chose (F13).
    #[serde(
        default,
        skip_serializing_if = "crate::domain::dialect::DialectChoice::is_automatic"
    )]
    pub dialects: crate::domain::dialect::DialectChoice,
    /// The computer that published this copy.
    pub device: String,
    /// When that computer changed it.
    pub updated_at_us: u64,
}

/// A connection another computer published to the account (C2): how to
/// reach its vehicles, not whether it is up. Used on a computer only once
/// someone there chooses it, or "Use them all" for a network link; a
/// serial link names its USB device. Signing keys never travel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryLink {
    pub id: String,
    pub name: String,
    pub kind: crate::domain::link::LinkKind,
    pub policy: crate::domain::link::LinkPolicy,
    pub topology: crate::domain::link::Topology,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vehicle_filter: Vec<String>,
    #[serde(default)]
    pub carries: crate::domain::link::LinkCarries,
    /// The computer that published this copy.
    pub device: String,
    /// When that computer changed it.
    pub updated_at_us: u64,
}

/// A camera someone added, as it goes to the account (FB-229): a recipe to
/// add it again, never its login. Used on another computer only once
/// someone there chooses it, where any login it needs is typed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedCamera {
    /// Its id here, and for a camera on a UDP port `udp-<port>`, the
    /// same on every computer.
    pub id: String,
    pub name: String,
    /// Someone named it; otherwise its name is its address.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named_by_hand: bool,
    /// The vehicle someone chose for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Its RTSP or WHEP addresses, preferred first, each without its login.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// Which of `urls` had a login here, by place, so another computer
    /// asks for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logins: Vec<usize>,
    /// For a camera that sends to UXDuck, the UDP port it listens on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_port: Option<u16>,
    /// "Record with the others" is off (S153).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_alone: bool,
    /// Its vehicle's own camera takes a photo and records along with it,
    /// as someone set it (S145).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_camera: Option<bool>,
    /// Recorded on the vehicle only (ADR-0066 R6).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vehicle_only: bool,
}

impl SharedCamera {
    /// It asks for a user name and password typed here: an address had one
    /// there, and that address needs nothing else edited.
    #[must_use]
    pub fn needs_login(&self) -> bool {
        self.logins.iter().any(|&place| {
            self.urls
                .get(place)
                .is_some_and(|url| !crate::domain::video::needs_its_address(url))
        })
    }

    /// An address had a token, a key or a signature there, which no user
    /// name or password stands for, so its address is edited here.
    #[must_use]
    pub fn needs_its_address(&self) -> bool {
        self.urls
            .iter()
            .any(|url| crate::domain::video::needs_its_address(url))
    }
}

/// A camera another computer published to the account (FB-229).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryCamera {
    #[serde(flatten)]
    pub camera: SharedCamera,
    /// The computer that published this copy.
    pub device: String,
    /// The copy's version in the account, which orders it against a removal.
    pub version: u64,
    /// When that computer changed it.
    pub updated_at_us: u64,
}
