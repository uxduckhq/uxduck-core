use crc_any::{CRCu16, CRCu64};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::protocol::{Deprecated, Description, Dialect, Fingerprint, MavType, MessageField};
use crate::utils::{Buildable, Builder, Named};

/// Type of MAVLink message ID
pub type MessageId = u32;

/// MAVLink message
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "specta", specta(rename = "MavInspectMessage"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Message {
    id: MessageId,
    name: String,
    description: Description,
    fields: Vec<MessageField>,
    wip: bool,
    deprecated: Option<Deprecated>,
    defined_in: String,
    appears_in: Vec<String>,
}

impl Buildable for Message {
    type Builder = MessageBuilder;

    /// Creates [`MessageBuilder`] initialised with current message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::Message;
    /// use mavinspect::utils::{Buildable, Builder};
    ///
    /// let original = Message::builder()
    ///     .set_name("original")
    ///     .set_description("original")
    ///     .build();
    ///
    /// let updated = original.to_builder()
    ///     .set_description("updated")
    ///     .build();
    ///
    /// assert_eq!(updated.name(), "original");
    /// assert_eq!(updated.description(), "updated");
    /// ```
    fn to_builder(&self) -> MessageBuilder {
        MessageBuilder {
            message: self.clone(),
        }
    }
}

impl Named for Message {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for &Message {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Message {
    /// Initiates builder.
    ///
    /// Instead of constructor we use
    /// [builder](https://rust-unofficial.github.io/patterns/patterns/creational/builder.html)
    /// pattern. An instance of [`MessageBuilder`] returned by this function is initialized
    /// with default values. Once desired values are set, you can call [`MessageBuilder::build`] to
    /// obtain [`Message`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::Message;
    /// use mavinspect::utils::Builder;
    ///
    /// let message = Message::builder()
    ///     .set_name("name".to_string())
    ///     .set_description("description")
    ///     .build();
    ///
    /// assert!(matches!(message, Message { .. }));
    /// assert_eq!(message.name(), "name");
    /// assert_eq!(message.description(), "description");
    /// ```
    pub fn builder() -> MessageBuilder {
        MessageBuilder::new()
    }

    /// Unique message ID within dialect.
    pub fn id(&self) -> MessageId {
        self.id
    }

    /// Message name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Message description.
    pub fn description(&self) -> &str {
        self.description.as_str()
    }

    /// List of message fields.
    pub fn fields(&self) -> &[MessageField] {
        &self.fields
    }

    /// Returns message field specified by its name if exists.   
    pub fn get_field_by_name(&self, name: impl AsRef<str>) -> Option<&MessageField> {
        self.fields
            .iter()
            .find(|&field| field.name() == name.as_ref())
    }

    /// Work in progress status.
    pub fn wip(&self) -> bool {
        self.wip
    }

    /// Deprecation status.
    pub fn deprecated(&self) -> Option<&Deprecated> {
        self.deprecated.as_ref()
    }

    /// Dialect [canonical](Dialect::canonical_name) name where this
    /// message was finally defined.
    ///
    /// We track dialect in which message was defined to help to optimise code generation.
    ///
    /// The comprehensive list of dialects where this message was defined can be obtained in
    /// [`Self::appears_in`].
    ///
    /// Use [`Self::was_defined_in`] to check whether message was defined in a specific dialect.
    pub fn defined_in(&self) -> &str {
        self.defined_in.as_ref()
    }

    /// Returns the list of dialect [canonical](Dialect::canonical_name) names where this message
    /// was defined.
    ///
    /// The dialect where the message finally belongs to can be obtained in [`Self::defined_in`].
    ///
    /// Use [`Self::was_defined_in`] to check whether message was defined in a specific dialect.
    pub fn appears_in(&self) -> &[impl AsRef<str>] {
        self.appears_in.as_slice()
    }

    /// Returns `true` if this message was defined in a dialect with a specified
    /// [canonical](Dialect::canonical_name) name.
    ///
    /// See also: [`Self::defined_in`] and [`Self::appears_in`].
    pub fn was_defined_in(&self, dialect_canonical_name: impl AsRef<str>) -> bool {
        self.appears_in
            .contains(&dialect_canonical_name.as_ref().to_string())
    }

    /// Size of the fields payload according to
    /// [MAVLink 2](https://mavlink.io/en/guide/mavlink_2.html) protocol version.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::{MavType, Message, MessageField};
    /// use mavinspect::utils::Builder;
    ///
    /// let msg = Message::builder()
    ///     .set_fields(vec![
    ///         MessageField::builder()
    ///             .set_type(MavType::Array(Box::new(MavType::Float), 3))  // Must be 12
    ///             .build(),
    ///         MessageField::builder()
    ///             .set_type(MavType::Array(Box::new(MavType::UInt8), 5))  // Must be 5
    ///             .build(),
    ///         MessageField::builder()
    ///             .set_type(MavType::Float)                               // Must be 4
    ///             .build(),
    ///     ])
    ///     .build();
    ///
    /// assert_eq!(msg.size_v2(), 21);
    /// ```
    pub fn size_v2(&self) -> usize {
        self.fields_v2()
            .iter()
            .fold(0, |acc, fld| acc + fld.r#type().size())
    }

    /// Size of the fields payload according to
    /// [MAVLink 2](https://mavlink.io/en/guide/mavlink_2.html) protocol version.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::{MavType, Message, MessageField};
    /// use mavinspect::utils::Builder;
    ///
    /// let msg = Message::builder()
    ///     .set_fields(vec![
    ///         MessageField::builder()
    ///             .set_type(MavType::Array(Box::new(MavType::Float), 3))  // Must be 12
    ///             .build(),
    ///         MessageField::builder()
    ///             .set_type(MavType::Array(Box::new(MavType::UInt8), 5))  // Must be 5
    ///             .build(),
    ///         MessageField::builder()
    ///             .set_type(MavType::Float)                               // Ignored
    ///             .set_extension(true)
    ///             .build(),
    ///     ])
    ///     .build();
    ///
    /// assert_eq!(msg.size_v1(), 17);
    /// ```
    pub fn size_v1(&self) -> usize {
        self.fields_v1()
            .iter()
            .fold(0, |acc, fld| acc + fld.r#type().size())
    }

    /// Returns index of the first extension field if any.
    ///
    /// See:
    ///  * [MAVLink fields reordering](https://mavlink.io/en/guide/serialization.html#field_reordering).
    ///  * MAVLink [extension fields](https://mavlink.io/en/guide/define_xml_element.html#message_extensions).
    ///  * [`MessageField::extension`].
    pub fn extension_fields_idx(&self) -> Option<usize> {
        for (i, field) in self.fields.iter().enumerate() {
            if field.extension() {
                return Some(i);
            }
        }
        None
    }

    /// Returns whether this message has extension fields.
    ///
    /// See:
    ///  * [MAVLink fields reordering](https://mavlink.io/en/guide/serialization.html#field_reordering).
    ///  * MAVLink [extension fields](https://mavlink.io/en/guide/define_xml_element.html#message_extensions).
    ///  * [`MessageField::extension`].
    pub fn has_extension_fields(&self) -> bool {
        self.extension_fields_idx().is_some()
    }

    /// Returns fields applicable to [MavLink 1](https://mavlink.io/en/guide/mavlink_version.html)
    /// protocol version.
    ///
    /// Basically, as required by specification, all extension fields are excluded.
    ///
    /// See: [MAVLink message extensions](https://mavlink.io/en/guide/define_xml_element.html#message_extensions).
    pub fn fields_v1(&self) -> Vec<&MessageField> {
        self.base_fields_reordered()
    }

    /// Returns fields applicable to [MAVLink 2](https://mavlink.io/en/guide/mavlink_2.html)
    /// protocol version.
    ///
    /// All [extension fields](https://mavlink.io/en/guide/define_xml_element.html#message_extensions)
    /// will be included.
    ///
    /// Fields are reordered according to [MAVLink specification](https://mavlink.io/en/guide/serialization.html#field_reordering)
    pub fn fields_v2(&self) -> Vec<&MessageField> {
        self.reordered_fields()
    }

    /// Returns reordered fields according to
    /// [MAVLink specification](https://mavlink.io/en/guide/serialization.html#field_reordering).
    pub fn reordered_fields(&self) -> Vec<&MessageField> {
        // Fields must be rearranged only before the first extension field.
        let arrange_before_idx = self.extension_fields_idx();

        match arrange_before_idx {
            // Arrange everything
            None => {
                let mut rearranged: Vec<&MessageField> = Vec::from_iter(self.fields.iter());
                rearranged.sort_by(|left, right| MessageField::compare(left, right));
                rearranged
            }
            // Arrange only non-extension fields.
            Some(idx) => {
                let mut rearrangable: Vec<&MessageField> = self.fields[0..idx].iter().collect();
                rearrangable.sort_by(|left, right| MessageField::compare(left, right));

                let mut fields = Vec::from_iter(self.fields.iter());
                for (i, field) in rearrangable.iter().enumerate() {
                    fields[i] = field
                }

                fields
            }
        }
    }

    /// Returns base fields.
    ///
    /// Base fields are the fields which are not marked as extensions.
    ///
    /// These fields are not reordered. To get reordered base fields use [`Message::fields_v1`].
    ///
    /// See: [MAVLink message extensions](https://mavlink.io/en/guide/define_xml_element.html#message_extensions).
    pub fn base_fields(&self) -> Vec<&MessageField> {
        self.fields()
            .iter()
            .filter(|field| !field.extension())
            .collect()
    }

    /// Returns base fields reordered according to `MAVLink` specification.
    ///
    /// Base fields are the fields which are not marked as extensions.
    ///
    /// See:
    ///  * [MAVLink fields reordering](https://mavlink.io/en/guide/serialization.html#field_reordering).
    ///  * [MAVLink message extensions](https://mavlink.io/en/guide/define_xml_element.html#message_extensions).
    pub fn base_fields_reordered(&self) -> Vec<&MessageField> {
        self.reordered_fields()
            .iter()
            .filter(|field| !field.extension())
            .cloned()
            .collect()
    }

    /// Returns extension fields.
    ///
    /// See: [MAVLink message extensions](https://mavlink.io/en/guide/define_xml_element.html#message_extensions).
    pub fn extension_fields(&self) -> Vec<&MessageField> {
        self.fields()
            .iter()
            .filter(|field| field.extension())
            .collect()
    }

    /// Whether this message is compatible with MAVLink 1.
    ///
    /// See: [MAVLink versions](https://mavlink.io/en/guide/mavlink_version.html).
    pub fn is_v1_compatible(&self) -> bool {
        self.id <= 255
    }

    /// Message `CRC_EXTRA` calculated from message XML definition.
    ///
    /// Calculates CRC for message name and key message fields to detect incompatible changes in
    /// message definition.
    ///
    /// See: [CRC_EXTRA calculation](https://mavlink.io/en/guide/serialization.html#crc_extra) in
    /// `MAVLink` docs.
    pub fn crc_extra(&self) -> u8 {
        let mut crc_calculator = CRCu16::crc16mcrf4cc();

        crc_calculator.digest(self.name.as_bytes());
        crc_calculator.digest(b" ");

        for field in self.base_fields_reordered() {
            // Primitive type name as in definition
            crc_calculator.digest(field.r#type().base_type().c_type().as_bytes());
            crc_calculator.digest(b" ");

            // Field name
            crc_calculator.digest(field.name().as_bytes());
            crc_calculator.digest(b" ");

            // Type length for array types
            if let MavType::Array(_, length) = field.r#type() {
                crc_calculator.digest(&[*length as u8]);
            }
        }

        // Get CRC and convert it to `u8`
        let crc_value = crc_calculator.get_crc();
        ((crc_value & 0xFF) ^ (crc_value >> 8)) as u8
    }

    /// Calculates message fingerprint.
    ///
    /// A value of opaque type [`Fingerprint`] that contains message CRC.
    ///
    /// Fingerprint is similar to [`Self::crc_extra`] but takes into consideration all fields and all message metadata.
    ///
    /// This function calculates a relaxed version of a fingerprint, use [`Self::fingerprint_strict`]
    /// to take into account MAVLink enums within a dialect.
    #[inline(always)]
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint_strict(None)
    }

    /// Calculates message fingerprint.
    ///
    /// A value of opaque type [`Fingerprint`] that contains dialect CRC.
    ///
    /// Fingerprint is similar to [`Self::crc_extra`] but takes into consideration all fields and all message metadata.
    ///
    /// Calculates a strict version of fingerprint if `dialect` provided. Use [`Self::fingerprint`]
    /// to calculate a relaxed fingerprint.
    pub fn fingerprint_strict(&self, dialect: Option<&Dialect>) -> Fingerprint {
        let mut crc_calculator = CRCu64::crc64();

        crc_calculator.digest(&self.id.to_le_bytes());
        crc_calculator.digest(b" ");

        crc_calculator.digest(self.name.as_bytes());
        crc_calculator.digest(b" ");

        for field in self.reordered_fields() {
            crc_calculator.digest(&field.fingerprint_strict(dialect).as_bytes());
        }

        if let Some(deprecated) = &self.deprecated {
            crc_calculator.digest(format!("{deprecated:?} ").as_bytes());
        }

        crc_calculator.digest(format!("{} ", self.defined_in).as_bytes());

        crc_calculator.digest(&[self.wip as u8]);

        crc_calculator.get_crc().into()
    }
}

/// [`Builder`] for [`Message`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MessageBuilder {
    message: Message,
}

impl Builder for MessageBuilder {
    type Buildable = Message;

    fn build(&self) -> Message {
        // We need this to get error when `Message` changes
        #[allow(clippy::match_single_binding)]
        match self.message.clone() {
            Message {
                id,
                name,
                description,
                fields,
                wip,
                deprecated,
                defined_in,
                appears_in,
            } => Message {
                id,
                name,
                description,
                fields,
                wip,
                deprecated,
                defined_in,
                appears_in,
            },
        }
    }
}

impl MessageBuilder {
    /// Creates builder instance.
    ///
    /// Instantiates builder with default values for [`Message`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets unique message ID within dialect.
    ///
    /// See: [`Message::id`].
    pub fn set_id(&mut self, id: MessageId) -> &mut Self {
        self.message.id = id;
        self
    }

    /// Sets message name.
    ///
    /// See: [`Message::name`].
    pub fn set_name(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.message.name = name.as_ref().to_string();
        self
    }

    /// Sets message description.
    ///
    /// See: [`Message::description`].
    pub fn set_description(&mut self, description: impl AsRef<str>) -> &mut Self {
        self.message.description = Description::new(description);
        self
    }

    /// Sets list of message fields.
    ///
    /// See: [`Message::fields`].
    pub fn set_fields(&mut self, fields: Vec<MessageField>) -> &mut Self {
        self.message.fields = fields;
        self
    }

    /// Appends [`MessageField`] to the fields.
    ///
    /// See: [`Message::fields`].
    pub fn add_field(&mut self, field: MessageField) -> &mut Self {
        self.message.fields.push(field);
        self
    }

    /// Sets work in progress status.
    ///
    /// See: [`Message::wip`].
    pub fn set_wip(&mut self, wip: bool) -> &mut Self {
        self.message.wip = wip;
        self
    }

    /// Sets deprecation status.
    ///
    /// See: [`Message::deprecated`].
    pub fn set_deprecated(&mut self, deprecated: Option<Deprecated>) -> &mut Self {
        self.message.deprecated = deprecated;
        self
    }

    /// Sets dialect [canonical](Dialect::canonical_name) name where this
    /// message was defined.
    ///
    /// See: [`Message::defined_in`], [`Message::appears_in`], and [`Message::was_defined_in`].
    pub fn set_defined_in(&mut self, defined_in: impl AsRef<str>) -> &mut Self {
        let defined_in = defined_in.as_ref().to_string();
        if !self.message.appears_in.contains(&defined_in) {
            self.message.appears_in.push(defined_in.clone());
        }
        self.message.defined_in = defined_in;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::builders::MessageFieldBuilder;
    use crate::protocol::{DeprecatedSince, MavType};

    fn make_fields(fields: &[MavType]) -> Vec<MessageField> {
        fields
            .iter()
            .enumerate()
            .map(|(i, t)| {
                MessageField::builder()
                    .set_type(t.clone())
                    .set_name(i.to_string())
                    .build()
            })
            .collect()
    }

    #[test]
    fn message_builder() {
        let message = MessageBuilder::new()
            .set_name("name".to_string())
            .set_description("description".to_string())
            .set_fields(make_fields(&[
                MavType::Int16,
                MavType::UInt16,
                MavType::Float,
            ]))
            .set_wip(true)
            .set_deprecated(Some(Deprecated::new(
                DeprecatedSince::new(2023, 10),
                "new".to_string(),
            )))
            .set_defined_in("dialect".to_string())
            .build();

        assert!(matches!(message, Message { .. }));
        assert_eq!(message.name(), "name");
        assert_eq!(message.description(), "description");
        assert_eq!(message.fields().len(), 3);
        assert!(matches!(message.fields()[0].r#type(), MavType::Int16));
        assert!(message.wip());
        assert!(matches!(message.deprecated(), Some(Deprecated { .. })));
    }

    #[test]
    fn fields_v1_v2() {
        let message = MessageBuilder::new()
            .set_fields(vec![
                MessageFieldBuilder::new()
                    .set_name("first".to_string())
                    .build(),
                MessageFieldBuilder::new()
                    .set_name("second".to_string())
                    .build(),
                MessageFieldBuilder::new()
                    .set_name("third".to_string())
                    .build(),
                MessageFieldBuilder::new()
                    .set_name("fourth (extension)".to_string())
                    .set_extension(true)
                    .build(),
                MessageFieldBuilder::new()
                    .set_name("fifth (extension)".to_string())
                    .set_extension(true)
                    .build(),
            ])
            .build();

        assert_eq!(message.fields().len(), 5);
        assert_eq!(message.fields_v2().len(), 5);
        assert_eq!(message.fields_v1().len(), 3);
        assert_eq!(message.extension_fields().len(), 2);
    }

    #[test]
    fn basic_fields_reordering() {
        let message = Message::builder()
            .set_fields(make_fields(&[
                MavType::Int16,
                MavType::UInt16,
                MavType::UInt32,
                MavType::UInt8,
                MavType::Float,
            ]))
            .build();

        let reordered = message.reordered_fields();

        assert_eq!(reordered.get(0).unwrap().name(), "2");
        assert_eq!(reordered.get(1).unwrap().name(), "4");
        assert_eq!(reordered.get(2).unwrap().name(), "0");
        assert_eq!(reordered.get(3).unwrap().name(), "1");
        assert_eq!(reordered.get(4).unwrap().name(), "3");
    }

    #[test]
    fn extensions_fields_are_not_reordered() {
        let mut fields = make_fields(&[
            MavType::Int16,
            MavType::UInt16,
            MavType::UInt32,
            MavType::UInt8,
            MavType::Float,
        ]);
        fields[3] = fields[3].to_builder().set_extension(true).build();
        fields[4] = fields[4].to_builder().set_extension(true).build();

        let message = Message::builder().set_fields(fields).build();

        let reordered = message.reordered_fields();

        assert_eq!(reordered.get(0).unwrap().name(), "2");
        assert_eq!(reordered.get(1).unwrap().name(), "0");
        assert_eq!(reordered.get(2).unwrap().name(), "1");
        assert_eq!(reordered.get(3).unwrap().name(), "3");
        assert_eq!(reordered.get(4).unwrap().name(), "4");
    }

    #[test]
    fn crc_extra_heartbeat() {
        // `HEARTBEAT` message from `minimal` dialect (Dec 2023).
        //
        // We want to add this message to test suite since it contains a field with
        // `uint8_t_mavlink_version` type.
        let message = MessageBuilder::new()
            .set_name("HEARTBEAT".to_string())
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("type".to_string())
                    .set_type(MavType::UInt8)
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("autopilot".to_string())
                    .set_type(MavType::UInt8)
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("base_mode".to_string())
                    .set_type(MavType::UInt8)
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("custom_mode".to_string())
                    .set_type(MavType::UInt32)
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("system_status".to_string())
                    .set_type(MavType::UInt8)
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("mavlink_version".to_string())
                    .set_type(MavType::UInt8MavlinkVersion)
                    .build(),
            )
            .build();

        let crc = message.crc_extra();
        assert_eq!(crc, 50u8);
    }

    #[test]
    fn crc_extra_protocol_version() {
        // `PROTOCOL_VERSION` message from `minimal` dialect (Dec 2023)
        //
        // This message is still WIP at the moment of writing this code but it contains arrays
        // and this is what we need for the test.
        let message = MessageBuilder::new()
            .set_name("PROTOCOL_VERSION".to_string())
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("version".to_string())
                    .set_type(MavType::UInt16)
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("min_version".to_string())
                    .set_type(MavType::UInt16)
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("max_version".to_string())
                    .set_type(MavType::UInt16)
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("spec_version_hash".to_string())
                    .set_type(MavType::Array(Box::new(MavType::UInt8), 8))
                    .build(),
            )
            .add_field(
                MessageFieldBuilder::new()
                    .set_name("library_version_hash".to_string())
                    .set_type(MavType::Array(Box::new(MavType::UInt8), 8))
                    .build(),
            )
            .build();

        let crc = message.crc_extra();
        assert_eq!(crc, 217u8);
    }
}
