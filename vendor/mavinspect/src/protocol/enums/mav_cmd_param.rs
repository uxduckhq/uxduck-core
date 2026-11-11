#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::protocol::{Description, Units, Value};
use crate::utils::{Buildable, Builder};

/// [`EnumEntry`](super::EnumEntry) `MAV_CMD` parameter.
///
/// Makes sense only in the context of MAVLink command enum (`MAV_CMD`).
///
/// See: MAVLink [command details](https://mavlink.io/en/guide/xml_schema.html#MAV_CMD) in XML
/// schema docs.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumEntryMavCmdParam {
    index: u8,
    description: Description,
    label: Option<String>,
    units: Option<Units>,
    r#enum: Option<String>,
    decimal_places: Option<u8>,
    increment: Option<Value>,
    min_value: Option<Value>,
    max_value: Option<Value>,
    reserved: bool,
    default: Option<Value>,
}

impl Buildable for EnumEntryMavCmdParam {
    type Builder = EnumEntryMavCmdParamBuilder;

    /// Creates [`EnumEntryMavCmdParamBuilder`] initialised with current enum entry.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::EnumEntryMavCmdParam;
    /// use mavinspect::utils::{Buildable, Builder};
    ///
    /// let original = EnumEntryMavCmdParam::builder()
    ///     .set_index(3)
    ///     .set_description("original".to_string())
    ///     .build();
    ///
    /// let updated = original.to_builder()
    ///     .set_description("updated".to_string())
    ///     .build();
    ///
    /// assert_eq!(updated.index(), 3);
    /// assert_eq!(updated.description(), "updated");
    /// ```
    fn to_builder(&self) -> EnumEntryMavCmdParamBuilder {
        EnumEntryMavCmdParamBuilder(self.clone())
    }
}

impl EnumEntryMavCmdParam {
    /// Initiates builder.
    ///
    /// Instead of constructor we use
    /// [builder](https://rust-unofficial.github.io/patterns/patterns/creational/builder.html)
    /// pattern. An instance of [`EnumEntryMavCmdParamBuilder`] returned by this function is initialized
    /// with default values. Once desired values are set, you can call [`EnumEntryMavCmdParamBuilder::build`] to
    /// obtain [`EnumEntryMavCmdParam`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::EnumEntryMavCmdParam;
    /// use mavinspect::utils::Builder;
    ///
    /// let param = EnumEntryMavCmdParam::builder()
    ///     .set_index(3)
    ///     .set_description("description".to_string())
    ///     .build();
    ///
    /// assert!(matches!(param, EnumEntryMavCmdParam { .. }));
    /// assert_eq!(param.index(), 3);
    /// assert_eq!(param.description(), "description");
    /// ```
    pub fn builder() -> EnumEntryMavCmdParamBuilder {
        EnumEntryMavCmdParamBuilder::new()
    }

    /// Parameter index: 1..7.
    pub fn index(&self) -> u8 {
        self.index
    }

    /// Description.
    pub fn description(&self) -> &str {
        self.description.as_str()
    }

    /// Display label.
    ///
    /// Display name to represent the parameter in a GCS or other UI. All words in label should be capitalised.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// Units of measurement.
    pub fn units(&self) -> Option<&Units> {
        self.units.as_ref()
    }

    /// Name of the enum containing possible values for the parameter (if applicable).
    pub fn r#enum(&self) -> Option<&str> {
        self.r#enum.as_deref()
    }

    /// Decimal places.
    ///
    /// Hint to a UI about how many decimal places to use if the parameter value is displayed.
    pub fn decimal_places(&self) -> Option<u8> {
        self.decimal_places
    }

    /// Allowed increments for the parameter value.
    pub fn increment(&self) -> Option<&Value> {
        self.increment.as_ref()
    }

    /// Minimum value for parameter.
    pub fn min_value(&self) -> Option<&Value> {
        self.min_value.as_ref()
    }

    /// Maximum value for parameter.
    pub fn max_value(&self) -> Option<&Value> {
        self.max_value.as_ref()
    }

    /// Reserved flag.
    ///
    /// Boolean indicating whether param is reserved for future use. Default is `false`.
    pub fn reserved(&self) -> bool {
        self.reserved
    }

    /// Default value.
    ///
    /// Default value for the param (primarily used for `reserved` params, where the value is `0`
    /// or `NaN`).
    pub fn default(&self) -> Option<&Value> {
        self.default.as_ref()
    }
}

/// Builder for [`EnumEntryMavCmdParam`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumEntryMavCmdParamBuilder(EnumEntryMavCmdParam);

impl Builder for EnumEntryMavCmdParamBuilder {
    type Buildable = EnumEntryMavCmdParam;

    /// Creates [`EnumEntryMavCmdParam`] from builder.
    fn build(&self) -> EnumEntryMavCmdParam {
        // We need this to get error when `EnumEntry` changes
        #[allow(clippy::match_single_binding)]
        match self.0.clone() {
            EnumEntryMavCmdParam {
                index,
                description,
                label,
                units,
                r#enum,
                decimal_places,
                increment,
                min_value,
                max_value,
                reserved,
                default,
            } => EnumEntryMavCmdParam {
                index,
                description,
                label,
                units,
                r#enum,
                decimal_places,
                increment,
                min_value,
                max_value,
                reserved,
                default,
            },
        }
    }
}

impl EnumEntryMavCmdParamBuilder {
    /// Creates builder instance.
    ///
    /// Instantiates builder with default values for [`EnumEntryMavCmdParam`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets parameter index: 1..7.
    ///
    /// See: [`EnumEntryMavCmdParam::index`].
    pub fn set_index(&mut self, index: u8) -> &mut Self {
        self.0.index = index;
        self
    }

    /// Sets description.
    ///
    /// See: [`EnumEntryMavCmdParam::description`].
    pub fn set_description(&mut self, description: impl AsRef<str>) -> &mut Self {
        self.0.description = Description::new(description);
        self
    }

    /// Sets display label.
    ///
    /// See: [`EnumEntryMavCmdParam::label`].
    pub fn set_label(&mut self, label: Option<impl AsRef<str>>) -> &mut Self {
        self.0.label = label.map(|s| s.as_ref().to_string());
        self
    }

    /// Sets units of measurement.
    ///
    /// See: [`EnumEntryMavCmdParam::units`].
    pub fn set_units(&mut self, units: Option<Units>) -> &mut Self {
        self.0.units = units;
        self
    }

    /// Sets name of the enum containing possible values for the parameter (if applicable).
    ///
    /// See: [`EnumEntryMavCmdParam::enum`].
    pub fn set_enum(&mut self, r#enum: Option<impl AsRef<str>>) -> &mut Self {
        self.0.r#enum = r#enum.map(|s| s.as_ref().to_string());
        self
    }

    /// Sets decimal places.
    ///
    /// See: [`EnumEntryMavCmdParam::decimal_places`].
    pub fn set_decimal_places(&mut self, decimal_places: Option<u8>) -> &mut Self {
        self.0.decimal_places = decimal_places;
        self
    }

    /// Sets allowed increments for the parameter value.
    ///
    /// See: [`EnumEntryMavCmdParam::increment`].
    pub fn set_increment(&mut self, increment: Option<Value>) -> &mut Self {
        self.0.increment = increment;
        self
    }

    /// Sets minimum value for parameter.
    ///
    /// See: [`EnumEntryMavCmdParam::min_value`].
    pub fn set_min_value(&mut self, min_value: Option<Value>) -> &mut Self {
        self.0.min_value = min_value;
        self
    }

    /// Sets maximum value for parameter.
    ///
    /// See: [`EnumEntryMavCmdParam::max_value`].
    pub fn set_max_value(&mut self, max_value: Option<Value>) -> &mut Self {
        self.0.max_value = max_value;
        self
    }

    /// Sets reserved flag.
    ///
    /// See: [`EnumEntryMavCmdParam::reserved`].
    pub fn set_reserved(&mut self, reserved: bool) -> &mut Self {
        self.0.reserved = reserved;
        self
    }

    /// Sets default value.
    ///
    /// See: [`EnumEntryMavCmdParam::default`].
    pub fn set_default(&mut self, default: Option<Value>) -> &mut Self {
        self.0.default = default;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_entry_builder() {
        let param = EnumEntryMavCmdParam::builder()
            .set_index(3)
            .set_description("description")
            .set_label(Some("label".to_string()))
            .set_units(Some(Units::Ampere))
            .set_enum(Some("enum".to_string()))
            .set_decimal_places(Some(5))
            .set_increment(Some(Value::UInt16(10)))
            .set_min_value(Some(Value::UInt16(100)))
            .set_max_value(Some(Value::UInt16(u16::MAX)))
            .set_reserved(true)
            .set_default(Some(Value::UInt16(1000)))
            .build();

        assert!(matches!(param, EnumEntryMavCmdParam { .. }));
        assert_eq!(param.index(), 3);
        assert_eq!(param.description(), "description");
        assert_eq!(param.label().unwrap(), "label");
        assert!(matches!(param.units(), Some(Units::Ampere)));
        assert_eq!(param.r#enum().unwrap(), "enum");
        assert!(matches!(param.decimal_places(), Some(5)));
        assert!(matches!(param.increment(), Some(Value::UInt16(10))));
        assert!(matches!(param.min_value(), Some(Value::UInt16(100))));
        assert!(matches!(param.max_value(), Some(Value::UInt16(u16::MAX))));
        assert!(param.reserved());
        assert!(matches!(param.default(), Some(Value::UInt16(1000))));
    }
}
