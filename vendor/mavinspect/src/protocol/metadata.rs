//! Metadata for MAVLink dialects.
//!
//! Upon reading from a location that contains MAVlink XML definitions, [`Inspector`](crate::Inspector)
//! looks up for a file `.dialects-metadata.yml`. If it exists, it loads its contents into a
//! [`MavInspectMetadata`](crate::parser::MavInspectMetadata) and attaches [`DialectMetadata`]
//! to all top-level dialects loaded from this location (includes are ignored).

/// Metadata for MAVLink dialect.
///
/// Upon reading from a location that contains MAVlink XML definitions, [`Inspector`](crate::Inspector)
/// looks up for a file `.dialects-metadata.yml`. If it exists, it loads its contents into
/// [`MavInspectMetadata`](crate::parser::MavInspectMetadata) and attaches metadata to all
/// top-level dialects loaded from this location (includes are ignored).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DialectMetadata {
    pub(crate) tags: DialectTags,
}

/// Collection of tags used in [`DialectMetadata`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct DialectTags(Vec<String>);

impl DialectTags {
    /// Constructor.
    pub fn new(tags: &[impl AsRef<str>]) -> Self {
        let tags = tags
            .iter()
            .map(|t| t.as_ref().to_string())
            .collect::<Vec<_>>();
        Self(tags)
    }

    /// Returns all tags.
    pub fn tags(&self) -> &[String] {
        self.0.as_slice()
    }

    /// Returns `true` if there are no tags in collection.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns `true` if metadata contains a specified `tag`.
    pub fn contains(&self, tag: impl AsRef<str>) -> bool {
        self.0.iter().any(|t| t == tag.as_ref())
    }

    /// Returns `true` if one of tags are present.
    pub fn has_one_of(&self, tags: &Self) -> bool {
        for present in self.0.iter() {
            for requested in tags.0.iter() {
                if present == requested {
                    return true;
                }
            }
        }

        false
    }

    /// Extends tags with the provided list.
    pub fn extend(&mut self, tags: &DialectTags) {
        for tag in tags.0.iter() {
            if !self.0.contains(&tag) {
                self.0.push(tag.to_string());
            }
        }
    }
}

impl DialectMetadata {
    /// Returns all tags.
    #[inline(always)]
    pub fn tags(&self) -> &[String] {
        self.tags.tags()
    }

    /// Returns `true` if metadata contains a specified `tag`.
    #[inline(always)]
    pub fn has_tag(&self, tag: impl AsRef<str>) -> bool {
        self.tags.contains(tag)
    }

    /// Returns `true` if one of tags are present.
    #[inline(always)]
    pub fn has_one_of_tags(&self, tags: &DialectTags) -> bool {
        self.tags.has_one_of(tags)
    }
}
