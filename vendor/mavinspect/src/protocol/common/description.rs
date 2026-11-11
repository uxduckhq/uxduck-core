#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::cmp::{max, min};
use std::fmt::{Display, Formatter};

const TAB_SPACES: &str = "    ";

/// Description of a MAVLink entity.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "specta", specta(rename = "MavInspectDescription"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Description(String);

impl Description {
    /// Constructs a description.
    pub fn new(description: impl AsRef<str>) -> Self {
        Self(Self::normalize(description))
    }

    /// Returns description as a string slice.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Returns description as a reference to [`String`].
    pub fn as_string_ref(&self) -> &String {
        &self.0
    }

    /// Normalizes description.
    pub fn normalize(value: impl AsRef<str>) -> String {
        let parts = value.as_ref().split("\n").collect::<Vec<_>>();

        let first_line_ident_len = if let Some(&first_line) = parts.first() {
            if first_line.trim().is_empty() {
                0
            } else if let Some(first_line_prefix) = first_line.strip_suffix(first_line.trim_start()) {
                let first_line_prefix = first_line_prefix.replace("\t", TAB_SPACES);
                first_line_prefix.chars().count()
            } else {
                0
            }
        } else {
            return value.as_ref().trim().to_string();
        };

        let min_ident = if first_line_ident_len > 0 {
            first_line_ident_len
        } else if parts.len() > 1 {
            let mut min_non_zero_ident = 0;
            for &line in parts[1..].iter() {
                if line.trim().is_empty() {
                    continue;
                }

                if let Some(line_empty_prefix) = line.strip_suffix(line.trim_start()) {
                    let line_empty_prefix = line_empty_prefix.replace("\t", TAB_SPACES);
                    let line_empty_prefix_len = line_empty_prefix.chars().count();
                    if min_non_zero_ident == 0 && !line_empty_prefix.is_empty() {
                        min_non_zero_ident = line_empty_prefix_len;
                    } else {
                        min_non_zero_ident = min(min_non_zero_ident, line_empty_prefix_len);
                    }
                }
            }

            min_non_zero_ident
        } else {
            0
        };

        let description = if min_ident > 0 {
            let with_first_line_padded = format!("{}{}", " ".repeat(min_ident), value.as_ref().trim_start());

            let lines = with_first_line_padded.split('\n').map(|line| {
                if line.trim().is_empty() {
                    line.trim().to_string()
                } else if let Some(line_prefix) = line.strip_suffix(line.trim_start()) {
                    let line_suffix = line_prefix.replace("\t", TAB_SPACES);
                    let line_suffix_len = line_suffix.chars().count();
                    let line_ident_len = max(line_suffix_len as i32 - min_ident as i32, 0) as usize;

                    let line = format!("{}{}", " ".repeat(line_ident_len), line.trim_start());
                    line
                } else {
                    line.to_string()
                }
            }).collect::<Vec<_>>();
            let indented = lines.join("\n");

            textwrap::dedent(indented.as_str())
        } else {
            textwrap::dedent(value.as_ref())
        };

        description
    }
}

impl Display for Description {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.0))
    }
}

impl From<&str> for Description {
    fn from(value: &str) -> Self {
        Description::new(value)
    }
}

impl From<String> for Description {
    fn from(value: String) -> Self {
        Description::new(value)
    }
}

#[cfg(test)]
mod description_tests {
    use super::*;

    #[test]
    fn test_normalize() {
        assert_eq!(Description::normalize(""), "");
        assert_eq!(Description::normalize("\n"), "\n");
        assert_eq!(Description::normalize("foobar"), "foobar");

        let normalized = Description::normalize("foobar\n  bar");
        assert_eq!(normalized.as_str(), "foobar\nbar");
        let normalized = Description::normalize("foo\n  bar\n    foobar");
        assert_eq!(normalized.as_str(), "foo\nbar\n  foobar");
        let normalized = Description::normalize("foo\n  bar\n\n    foobar");
        assert_eq!(normalized.as_str(), "foo\nbar\n\n  foobar");
        let normalized = Description::normalize("foo\n\tbar\n\t  foobar");
        assert_eq!(normalized.as_str(), "foo\nbar\n  foobar");
    }
}