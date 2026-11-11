use std::collections::HashMap;

use crate::protocol::{DialectMetadata, DialectTags};

/// MAVInspect metadata for dialects.
///
/// Upon reading from a location that contains MAVlink XML definitions, [`Inspector`](crate::Inspector)
/// looks up for a file `.dialects-metadata.yml`. If it exists, it loads its contents into
/// this structure and then attaches [`DialectMetadata`] to all top-level dialects loaded from this
/// location (includes are ignored).
///
/// # Example
///
/// Put `.dialects-metadata.yml` with the following contents into a directory with MAVLink XML
/// definitions.
///
/// ```yaml
/// # Tags that will be added to all dialects
/// tags:
///   - extra
///   - test
/// dialects:
///   MyDialect:
///     # Tags that will be added only to `MyDialect` dialect
///     tags:
///       - foo
/// ```
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct MavInspectMetadata {
    tags: DialectTags,
    dialects: Option<HashMap<String, DialectMetadata>>,
}

impl MavInspectMetadata {
    /// Returns all tags.
    pub fn tags(&self) -> &[String] {
        self.tags.tags()
    }

    /// Returns metadata for a dialect with specified name.
    ///
    /// <section class="warning">
    /// Note, that we use a "plain" dialect name (from the XML definition filename), not the
    /// canonical one.
    /// </section>
    pub fn metadata_for_dialect(&self, dialect_name: impl AsRef<str>) -> DialectMetadata {
        let dialect_name = dialect_name.as_ref();
        let mut metadata = DialectMetadata {
            tags: self.tags.clone(),
        };

        if let Some(dialects_metadata) = &self.dialects {
            if let Some(dialect_metadata) = dialects_metadata.get(dialect_name) {
                metadata.tags.extend(&dialect_metadata.tags);
            }
        }

        metadata
    }
}
