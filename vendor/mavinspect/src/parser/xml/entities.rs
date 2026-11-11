pub mod deprecated {
    use crate::errors::Result;
    use crate::parser::errors::XmlParseError;
    use crate::protocol::{Deprecated, DeprecatedSince};

    #[derive(Debug, Clone)]
    pub(crate) struct XmlDeprecated {
        pub since: String,
        pub replaced_by: String,
    }

    impl XmlDeprecated {
        pub fn to_deprecated(&self) -> Result<Deprecated> {
            let mut since_parts = self.since.split('-');
            let year: i32 = since_parts
                .next()
                .ok_or(XmlParseError::DeprecatedSinceDateIncorrectFormat(
                    self.since.clone(),
                ))?
                .parse()
                .map_err(XmlParseError::DeprecatedSinceIncorrectYear)?;
            let month: u8 = since_parts
                .next()
                .ok_or(XmlParseError::DeprecatedSinceDateIncorrectFormat(
                    self.since.clone(),
                ))?
                .parse()
                .map_err(XmlParseError::DeprecatedSinceIncorrectMonth)?;

            Ok(Deprecated::new(
                DeprecatedSince::new(year, month),
                self.replaced_by.to_string(),
            ))
        }
    }

    #[test]
    fn correct_deprecated_since_date_parsing() {
        let deprecated = XmlDeprecated {
            since: "2023-10".to_string(),
            replaced_by: "something".to_string(),
        }
            .to_deprecated()
            .unwrap();

        assert_eq!(deprecated.since().year(), 2023);
        assert_eq!(deprecated.since().month(), 10);
    }
}

pub(crate) mod enums {
    use std::collections::HashMap;

    use crate::errors::{Error, Result};
    use crate::parser::errors::XmlParseError;
    use crate::protocol::{
        Enum, EnumEntry, EnumEntryMavCmdFlags, EnumEntryMavCmdParam, EnumEntryValue, MavType,
        Units, Value,
    };
    use crate::utils::Builder;

    use super::deprecated::XmlDeprecated;

    #[derive(Debug, Clone)]
    pub(crate) struct XmlEnumEntry {
        pub value: String,
        pub name: String,
        pub enum_name: String,
        pub description: String,
        pub cmd_flags: XmlEnumEntryMavCmdFlags,
        pub params: Vec<XmlEnumEntryMavCmdParam>,
        pub deprecated: Option<XmlDeprecated>,
        pub wip: bool,
        pub defined_in: Option<String>,
    }

    impl XmlEnumEntry {
        pub fn to_enum_entry(&self) -> Result<EnumEntry> {
            let mut params = Vec::new();

            for param in &self.params {
                let idx = param
                    .index
                    .parse::<u8>()
                    .map_err(XmlParseError::EnumEntryMavCmdParamInvalidIndex)?;

                match idx {
                    1 => params.push(param.to_enum_entry_mav_cmd_param(&MavType::Float)?),
                    2 => params.push(param.to_enum_entry_mav_cmd_param(&MavType::Float)?),
                    3 => params.push(param.to_enum_entry_mav_cmd_param(&MavType::Float)?),
                    4 => params.push(param.to_enum_entry_mav_cmd_param(&MavType::Float)?),
                    5 => params.push(param.to_enum_entry_mav_cmd_param(&MavType::Int32)?),
                    6 => params.push(param.to_enum_entry_mav_cmd_param(&MavType::Int32)?),
                    7 => params.push(param.to_enum_entry_mav_cmd_param(&MavType::Float)?),
                    _ => {}
                }
            }

            let value: EnumEntryValue = parse_int::parse(&self.value).map_err(|error| {
                XmlParseError::EnumEntryValueParseFailed {
                    enum_name: self.enum_name.to_owned(),
                    entry_name: self.name.to_owned(),
                    value: self.value.to_owned(),
                    error,
                }
            })?;

            let name_stripped = if let Some(s) = self
                .name
                .strip_prefix(format!("{}_", &self.enum_name).as_str())
            {
                s
            } else {
                self.name.as_str()
            };

            let flags = self.cmd_flags.to_enum_entry_mav_cmd_flags()?;

            let deprecated = match &self.deprecated {
                Some(deprecated) => Some(deprecated.to_deprecated()?),
                None => None,
            };

            Ok(EnumEntry::builder()
                .set_value(value)
                .set_name(self.name.clone())
                .set_name_stripped(name_stripped.to_string())
                .set_description(self.description.clone())
                .set_cmd_flags(flags)
                .set_params(params)
                .set_wip(self.wip)
                .set_deprecated(deprecated)
                .set_defined_in(self.defined_in.clone().unwrap_or_else(|| {
                    panic!(
                        "Enum entry should always be defined in some dialect: name = '{}'",
                        self.name
                    )
                }))
                .build())
        }
    }

    #[derive(Debug, Clone)]
    pub(crate) struct XmlEnum {
        pub name: String,
        pub bitmask: bool,
        pub description: String,
        pub entries: HashMap<String, XmlEnumEntry>,
        pub deprecated: Option<XmlDeprecated>,
        pub defined_in: Option<String>,
    }

    impl XmlEnum {
        pub fn to_enum(&self) -> Result<Enum> {
            let mut entries = Vec::new();
            for entry in self.entries.values() {
                entries.push(entry.to_enum_entry()?);
            }

            let deprecated = match &self.deprecated {
                None => None,
                Some(deprecated) => Some(deprecated.to_deprecated()?),
            };

            Ok(Enum::builder()
                .set_name(self.name.clone())
                .set_description(self.description.clone())
                .set_entries(entries.as_slice())
                .set_bitmask(self.bitmask)
                .set_deprecated(deprecated)
                .set_defined_in(self.defined_in.clone().unwrap_or_else(|| {
                    panic!(
                        "Enum should always be defined in some dialect: name = '{}'",
                        self.name
                    )
                }))
                .build())
        }
    }

    #[derive(Debug, Clone)]
    pub(crate) struct XmlEnumEntryMavCmdFlags {
        pub has_location: Option<bool>,
        pub is_destination: Option<bool>,
        pub mission_only: Option<bool>,
    }

    impl XmlEnumEntryMavCmdFlags {
        pub fn to_enum_entry_mav_cmd_flags(&self) -> Result<Option<EnumEntryMavCmdFlags>> {
            let has_location = match &self.has_location {
                Some(true) => Some(true),
                _ => None,
            };
            let is_destination = match &self.is_destination {
                Some(true) => Some(true),
                _ => None,
            };
            let mission_only = match &self.mission_only {
                Some(true) => Some(true),
                _ => None,
            };

            Ok(match (has_location, is_destination, mission_only) {
                (None, None, None) => None,
                _ => Some(
                    EnumEntryMavCmdFlags::builder()
                        .set_has_location(has_location)
                        .set_is_destination(is_destination)
                        .set_mission_only(mission_only)
                        .build(),
                ),
            })
        }
    }

    #[derive(Debug, Clone)]
    pub(crate) struct XmlEnumEntryMavCmdParam {
        pub index: String,
        pub description: String,
        pub label: Option<String>,
        pub units: Option<String>,
        pub r#enum: Option<String>,
        pub decimal_places: Option<String>,
        pub increment: Option<String>,
        pub min_value: Option<String>,
        pub max_value: Option<String>,
        pub reserved: Option<String>,
        pub default: Option<String>,
    }

    impl XmlEnumEntryMavCmdParam {
        pub fn to_enum_entry_mav_cmd_param(
            &self,
            r#type: &MavType,
        ) -> Result<EnumEntryMavCmdParam> {
            let index = self
                .index
                .parse::<u8>()
                .map_err(XmlParseError::EnumEntryMavCmdParamInvalidIndex)?;
            let units = match &self.units {
                None => None,
                Some(units) => Some(units.parse::<Units>().map_err(|err| match err {
                    crate::errors::Error::Units(err) => XmlParseError::IncorrectUnits(err),
                    _ => XmlParseError::Other(err.to_string()),
                })?),
            };
            let decimal_places = match &self.decimal_places {
                None => None,
                Some(dec) => Some(
                    dec.parse::<u8>()
                        .map_err(XmlParseError::EnumEntryMavCmdParamInvalidDecimalPlaces)?,
                ),
            };
            let increment = match &self.increment {
                None => None,
                Some(inc) => Some(Value::parse(inc, r#type).map_err(|err| match err {
                    Error::Value(err) => XmlParseError::EnumEntryMavCmdParamInvalidIncrement(err),
                    _ => XmlParseError::Other(err.to_string()),
                })?),
            };
            let min_value = match &self.min_value {
                None => None,
                Some(min) => Some(Value::parse(min, r#type).map_err(|err| match err {
                    Error::Value(err) => XmlParseError::EnumEntryMavCmdParamInvalidMinValue(err),
                    _ => XmlParseError::Other(err.to_string()),
                })?),
            };
            let max_value = match &self.max_value {
                None => None,
                Some(max) => Some(Value::parse(max, r#type).map_err(|err| match err {
                    Error::Value(err) => XmlParseError::EnumEntryMavCmdParamInvalidMaxValue(err),
                    _ => XmlParseError::Other(err.to_string()),
                })?),
            };
            let reserved = match &self.reserved {
                None => false,
                Some(reserved) => reserved
                    .parse::<bool>()
                    .map_err(XmlParseError::EnumEntryMavCmdParamInvalidReserved)?,
            };
            let default = match &self.default {
                None => None,
                Some(def) => Some(Value::parse(def, r#type).map_err(|err| match err {
                    Error::Value(err) => {
                        XmlParseError::EnumEntryMavCmdParamInvalidDefaultValue(err)
                    }
                    _ => XmlParseError::Other(err.to_string()),
                })?),
            };

            Ok(EnumEntryMavCmdParam::builder()
                .set_index(index)
                .set_description(self.description.clone())
                .set_label(self.label.clone())
                .set_units(units)
                .set_enum(self.r#enum.clone())
                .set_decimal_places(decimal_places)
                .set_increment(increment)
                .set_min_value(min_value)
                .set_max_value(max_value)
                .set_reserved(reserved)
                .set_default(default)
                .build())
        }
    }
}

pub(crate) mod messages {
    use super::deprecated::XmlDeprecated;
    use crate::errors::{Error, Result};
    use crate::parser::errors::XmlParseError;
    use crate::protocol::{
        MavType, Message, MessageField, MessageFieldInvalidValue, MessageId, Units, Value,
    };
    use crate::utils::Builder;

    #[derive(Debug, Clone)]
    pub(crate) struct XmlMessageField {
        pub field_type: String,
        pub name: String,
        pub description: String,
        pub r#enum: Option<String>,
        pub units: Option<String>,
        pub bitmask: bool,
        pub print_format: Option<String>,
        pub default: Option<String>,
        pub invalid: Option<String>,
        pub instance: bool,
        pub extension: bool,
    }

    impl XmlMessageField {
        pub fn to_message_field(&self) -> Result<MessageField> {
            let r#type = self
                .field_type
                .parse::<MavType>()
                .map_err(|err| match err {
                    Error::Type(err) => XmlParseError::IncorrectMessageFieldType(err),
                    _ => XmlParseError::Other(err.to_string()),
                })?;
            let units = match &self.units {
                None => None,
                Some(units) => Some(units.parse::<Units>().map_err(|err| match err {
                    Error::Units(err) => XmlParseError::IncorrectUnits(err),
                    _ => XmlParseError::Other(err.to_string()),
                })?),
            };
            let default = match &self.default {
                None => None,
                Some(default) => Some(Value::parse(default, &r#type).map_err(|err| match err {
                    Error::Value(err) => {
                        XmlParseError::EnumEntryMavCmdParamInvalidDefaultValue(err)
                    }
                    _ => XmlParseError::Other(err.to_string()),
                })?),
            };
            let invalid = match self.invalid.clone() {
                Some(invalid) => Some(
                    MessageFieldInvalidValue::parse(invalid.as_str(), &r#type).map_err(|err| {
                        match err {
                            Error::InvalidValue(err) => XmlParseError::IncorrectInvalidValue(err),
                            _ => XmlParseError::Other(err.to_string()),
                        }
                    })?,
                ),
                None => None,
            };

            Ok(MessageField::builder()
                .set_name(self.name.clone())
                .set_description(self.description.clone())
                .set_type(r#type)
                .set_enum(self.r#enum.clone())
                .set_units(units)
                .set_bitmask(self.bitmask)
                .set_print_format(self.print_format.clone())
                .set_default(default)
                .set_invalid(invalid)
                .set_instance(self.instance)
                .set_extension(self.extension)
                .build())
        }
    }

    #[derive(Debug, Clone)]
    pub(crate) struct XmlMessage {
        pub id: String,
        pub name: String,
        pub description: String,
        pub fields: Vec<XmlMessageField>,
        pub wip: bool,
        pub deprecated: Option<XmlDeprecated>,
        pub defined_in: Option<String>,
    }

    impl XmlMessage {
        pub fn to_message(&self) -> Result<Message> {
            let message_id = self
                .id
                .parse::<MessageId>()
                .map_err(XmlParseError::IncorrectMessageId)?;

            let mut fields: Vec<MessageField> = Vec::new();

            for xml_fld in &self.fields {
                fields.push(xml_fld.to_message_field()?);
            }

            let deprecated = match &self.deprecated {
                Some(deprecated) => Some(deprecated.to_deprecated()?),
                None => None,
            };

            Ok(Message::builder()
                .set_id(message_id)
                .set_name(self.name.clone())
                .set_description(self.description.clone())
                .set_fields(fields)
                .set_wip(self.wip)
                .set_deprecated(deprecated)
                .set_defined_in(self.defined_in.clone().unwrap_or_else(|| {
                    panic!(
                        "Message should always be defined in some dialect: name = '{}'",
                        self.name
                    )
                }))
                .build())
        }
    }
}
