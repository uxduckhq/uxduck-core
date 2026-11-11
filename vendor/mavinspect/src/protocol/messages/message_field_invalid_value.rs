use regex::Regex;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::errors::Result;
use crate::protocol::errors::{InvalidValueParseError, ValueError};
use crate::protocol::{MavType, Value};

const VALUE_AS_INVALID_REGEX: &str = r"^(((-?\d+)(\.\d+)?)|NAN|NaN|[A-Z0-9]+_MAX)$";
const ENUM_ENTRY_VALUE_AS_INVALID_REGEX: &str = r"^[A-Z]+[A-Z_]+[A-Z]+$";

/// Defines how to specify invalid value for [`MessageField`](crate::protocol::MessageField).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum MessageFieldInvalidValue {
    /// Field has a specific value.
    Value(Value),
    /// All array items are set to a specific value.
    AllItems(Value),
    /// First array item is set to a specific value.
    FirstItem(Value),
    /// Value from enum entry specified by name
    EnumEntryValue(String),
}

impl MessageFieldInvalidValue {
    /// Parses from string specification
    pub fn parse(s: &str, r#type: &MavType) -> Result<MessageFieldInvalidValue> {
        let normalized = s.trim();
        let value_re = Regex::new(VALUE_AS_INVALID_REGEX).unwrap();
        let enum_entry_name_re = Regex::new(ENUM_ENTRY_VALUE_AS_INVALID_REGEX).unwrap();

        fn map_incorrect_value_str(err: crate::errors::Error, s: &str) -> InvalidValueParseError {
            match err {
                crate::errors::Error::Value(err) => {
                    InvalidValueParseError::IncorrectValue(s.to_string(), err)
                }
                _ => InvalidValueParseError::Other(err.to_string()),
            }
        }

        Ok(match normalized {
            // First array item is set to a particular value
            _ if normalized.ends_with(":]") => {
                let scalar = normalized.trim_start_matches('[').trim_end_matches(":]");
                MessageFieldInvalidValue::FirstItem(
                    Value::parse(scalar, r#type).map_err(|err| map_incorrect_value_str(err, s))?,
                )
            }
            // All array items are set to particular value
            _ if normalized.ends_with(']') => {
                let scalar = normalized.trim_start_matches('[').trim_end_matches(']');
                MessageFieldInvalidValue::AllItems(
                    Value::parse(scalar, r#type).map_err(|err| map_incorrect_value_str(err, s))?,
                )
            }
            // Maximum value of a type
            _ if normalized.ends_with("_MAX") => {
                MessageFieldInvalidValue::Value(Value::Max(r#type.clone()))
            }
            // Hexadecimal value
            _ if normalized.starts_with("0x") => {
                let radix: u32 = (u32::try_from(normalized.chars().count()).unwrap() - 2) * 4;
                let int_value = i128::from_str_radix(normalized.trim_start_matches("0x"), radix)
                    .map_err(|err| {
                        InvalidValueParseError::IncorrectValue(
                            s.to_string(),
                            ValueError::ParseIntError(err),
                        )
                    })?;
                MessageFieldInvalidValue::Value(
                    Value::parse(int_value.to_string().as_str(), r#type)
                        .map_err(|err| map_incorrect_value_str(err, s))?,
                )
            }
            // Particular value marks scalar data as invalid
            _ if value_re.is_match(normalized) => MessageFieldInvalidValue::Value(
                Value::parse(normalized, r#type).map_err(|err| map_incorrect_value_str(err, s))?,
            ),
            // Enum entry name as invalid value
            _ if enum_entry_name_re.is_match(normalized) => {
                MessageFieldInvalidValue::EnumEntryValue(normalized.to_string())
            }
            // Return error if nothing works
            &_ => {
                return Err(
                    InvalidValueParseError::IncorrectSpecification(normalized.to_string()).into(),
                )
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_field_invalid_value_parser_is_correct() {
        // Scalar: maximum value
        assert!(matches!(
            MessageFieldInvalidValue::parse("INT16_MAX", &MavType::Int16).unwrap(),
            MessageFieldInvalidValue::Value(Value::Max(MavType::Int16)),
        ));

        // Scalar: integer value of i32
        assert!(matches!(
            MessageFieldInvalidValue::parse("-1234", &MavType::Int32).unwrap(),
            MessageFieldInvalidValue::Value(Value::Int32(-1234)),
        ));

        // Array: all items have integer value of i64
        assert!(matches!(
            MessageFieldInvalidValue::parse("[-1234]", &MavType::Int64).unwrap(),
            MessageFieldInvalidValue::AllItems(Value::Int64(-1234)),
        ));

        // Array: all items are NaNs of single precision floating point value
        {
            let parsed = MessageFieldInvalidValue::parse("[NaN]", &MavType::Float).unwrap();
            if let MessageFieldInvalidValue::AllItems(Value::Float(value)) = parsed {
                assert!(value.is_nan());
            } else {
                panic!("Invalid parsing result: {:?}", parsed);
            }
        }

        // Array: all items have double precision floating point value
        {
            let parsed = MessageFieldInvalidValue::parse("[-12.34]", &MavType::Double).unwrap();
            if let MessageFieldInvalidValue::AllItems(Value::Double(value)) = parsed {
                assert_eq!(value, -12.34);
            } else {
                panic!("Invalid parsing result: {:?}", parsed);
            }
        }
    }
}
