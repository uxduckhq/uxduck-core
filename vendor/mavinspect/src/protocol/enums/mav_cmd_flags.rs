#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::utils::{Buildable, Builder};

/// [`EnumEntry`](super::EnumEntry) `MAV_CMD` flags.
///
/// Contains flags like `hasLocation` or `isDestination`.
///
/// Makes sense only in the context of MAVLink command enum (`MAV_CMD`).
///
/// See: MAVLink [command details](https://mavlink.io/en/guide/xml_schema.html#MAV_CMD) in XML
/// schema docs.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct EnumEntryMavCmdFlags {
    has_location: Option<bool>,
    is_destination: Option<bool>,
    mission_only: Option<bool>,
}

impl Buildable for EnumEntryMavCmdFlags {
    type Builder = EnumEntryMavCmdFlagsBuilder;

    /// Creates [`EnumEntryMavCmdFlagsBuilder`] initialised with current enum entry.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::EnumEntryMavCmdFlags;
    /// use mavinspect::utils::{Buildable, Builder};
    ///
    /// let original = EnumEntryMavCmdFlags::builder()
    ///     .set_has_location(Some(true))
    ///     .set_is_destination(Some(true))
    ///     .build();
    ///
    /// let updated = original.to_builder()
    ///     .set_is_destination(Some(false))
    ///     .build();
    ///
    /// assert!(matches!(updated.has_location(), Some(true)));
    /// assert!(matches!(updated.is_destination(), Some(false)));
    /// ```
    fn to_builder(&self) -> EnumEntryMavCmdFlagsBuilder {
        EnumEntryMavCmdFlagsBuilder {
            flags: self.clone(),
        }
    }
}

impl EnumEntryMavCmdFlags {
    /// Initiates builder.
    ///
    /// Instead of constructor we use
    /// [builder](https://rust-unofficial.github.io/patterns/patterns/creational/builder.html)
    /// pattern. An instance of [`EnumEntryMavCmdFlagsBuilder`] returned by this function is initialized
    /// with default values. Once desired values are set, you can call [`EnumEntryMavCmdFlagsBuilder::build`] to
    /// obtain [`EnumEntryMavCmdFlags`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::EnumEntryMavCmdFlags;
    /// use mavinspect::utils::Builder;
    ///
    /// let flags = EnumEntryMavCmdFlags::builder()
    ///     .set_has_location(Some(true))
    ///     .set_is_destination(Some(true))
    ///     .build();
    ///
    /// assert!(matches!(flags, EnumEntryMavCmdFlags { .. }));
    /// assert!(matches!(flags.has_location(), Some(true)));
    /// assert!(matches!(flags.is_destination(), Some(true)));
    /// ```
    pub fn builder() -> EnumEntryMavCmdFlagsBuilder {
        EnumEntryMavCmdFlagsBuilder::new()
    }

    /// Has location.
    ///
    /// A boolean (default `true`) that provides a hint to a GCS that the entry should be displayed as a "standalone"
    /// location - rather than as a destination on the flight path. Apply for MAV_CMDs that contain lat/lon/alt location
    /// information in param 5, 6, and 7 values but which are not on the vehicle path.
    ///
    /// See: MAVLink [command details](https://mavlink.io/en/guide/xml_schema.html#MAV_CMD) in XML schema docs.
    pub fn has_location(&self) -> Option<bool> {
        self.has_location
    }

    /// Is destination.
    ///
    /// A boolean (default `true`) that provides a hint to a GCS that the entry is a location that should be displayed as
    /// a point on the flight path. Apply for MAV_CMD that contain lat/lon/alt location information in their param 5, 6,
    /// and 7 values, and that set a path/destination.
    ///
    /// See: MAVLink [command details](https://mavlink.io/en/guide/xml_schema.html#MAV_CMD) in XML schema docs.
    pub fn is_destination(&self) -> Option<bool> {
        self.is_destination
    }

    /// Mission only.
    ///
    /// Apply with value `true` if the MAV_COMMAND only "makes sense" in a mission. For example, the fence mission
    /// commands could not possibly be useful in a command.
    ///
    /// See: MAVLink [command details](https://mavlink.io/en/guide/xml_schema.html#MAV_CMD) in XML schema docs.
    pub fn mission_only(&self) -> Option<bool> {
        self.mission_only
    }
}

/// Builder for [`EnumEntryMavCmdFlags`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumEntryMavCmdFlagsBuilder {
    flags: EnumEntryMavCmdFlags,
}

impl Builder for EnumEntryMavCmdFlagsBuilder {
    type Buildable = EnumEntryMavCmdFlags;

    /// Creates [`EnumEntryMavCmdFlags`] from builder.
    fn build(&self) -> EnumEntryMavCmdFlags {
        // We need this to get error when `EnumEntry` changes
        #[allow(clippy::match_single_binding)]
        match self.flags.clone() {
            EnumEntryMavCmdFlags {
                has_location,
                is_destination,
                mission_only,
            } => EnumEntryMavCmdFlags {
                has_location,
                is_destination,
                mission_only,
            },
        }
    }
}

impl EnumEntryMavCmdFlagsBuilder {
    /// Creates builder instance.
    ///
    /// Instantiates builder with default values for [`EnumEntryMavCmdFlags`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets has location flag.
    ///
    /// See: [`EnumEntryMavCmdFlags::has_location`].
    pub fn set_has_location(&mut self, has_location: Option<bool>) -> &mut Self {
        self.flags.has_location = has_location;
        self
    }

    /// Sets is destination flag.
    ///
    /// See: [`EnumEntryMavCmdFlags::is_destination`].
    pub fn set_is_destination(&mut self, is_destination: Option<bool>) -> &mut Self {
        self.flags.is_destination = is_destination;
        self
    }

    /// Sets mission only flag.
    ///
    /// See: [`EnumEntryMavCmdFlags::mission_only`].
    pub fn set_mission_only(&mut self, mission_only: Option<bool>) -> &mut Self {
        self.flags.mission_only = mission_only;
        self
    }
}
