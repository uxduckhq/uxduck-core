//! A field that can be left out (no change), `null` (clear it) or a value
//! (set it): `Option<Option<T>>` on the wire. Use with
//! `#[serde(default, deserialize_with = "crate::nullable::deserialize", skip_serializing_if = "Option::is_none")]`.

use serde::{Deserialize, Deserializer};

/// A present field, even `null`, becomes `Some(..)`; `default` covers absence.
///
/// # Errors
/// When the value does not deserialize as `T`.
pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[allow(clippy::option_option, reason = "the three states under test")]
    struct Change {
        #[serde(
            default,
            deserialize_with = "super::deserialize",
            skip_serializing_if = "Option::is_none"
        )]
        folder_id: Option<Option<String>>,
    }

    #[test]
    fn absent_null_and_value_are_three_different_things() {
        let parse = |text: &str| {
            serde_json::from_str::<Change>(text)
                .map(|c| c.folder_id)
                .ok()
        };
        assert_eq!(parse("{}"), Some(None));
        assert_eq!(parse(r#"{"folder_id":null}"#), Some(Some(None)));
        assert_eq!(
            parse(r#"{"folder_id":"f1"}"#),
            Some(Some(Some("f1".into())))
        );
        let back = serde_json::to_string(&Change {
            folder_id: Some(None),
        })
        .ok();
        assert_eq!(back.as_deref(), Some(r#"{"folder_id":null}"#));
    }
}
