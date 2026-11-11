use std::collections::HashMap;
use std::io::{BufRead, Read};

use quick_xml::events::{BytesStart, BytesText, Event};
use quick_xml::reader::Reader;

use crate::errors::Result;
use crate::parser::errors::XmlParseError;
use crate::protocol::{Enum, EnumEntry, Message, MessageId};
use crate::utils::{dialect_canonical_name, Buildable, Builder};

use super::context::XmlParsingContext;
use super::context_stack::XmlParsingContextStack;
use super::entities::deprecated::XmlDeprecated;
use super::entities::enums::{
    XmlEnum, XmlEnumEntry, XmlEnumEntryMavCmdFlags, XmlEnumEntryMavCmdParam,
};
use super::entities::messages::{XmlMessage, XmlMessageField};

/// Parses `MAVLink` XML definitions.
#[derive(Debug)]
pub struct XmlParser<'a> {
    dialect_name: Option<String>,
    enums: &'a mut HashMap<String, Enum>,
    messages: &'a mut HashMap<MessageId, Message>,
    includes: Vec<String>,
    version: Option<u8>, // TODO: change to `u32`
    dialect_id: Option<u32>,
    context_stack: XmlParsingContextStack,
    tag_stack: Vec<String>,
}

impl<'a> XmlParser<'a> {
    pub fn new(
        enums: &'a mut HashMap<String, Enum>,
        messages: &'a mut HashMap<MessageId, Message>,
    ) -> Self {
        Self {
            dialect_name: None,
            enums,
            messages,
            includes: vec![],
            version: None,
            dialect_id: None,
            context_stack: XmlParsingContextStack::new(),
            tag_stack: Vec::new(),
        }
    }

    pub fn parse<T: BufRead + Read>(
        &mut self,
        dialect_name: &str,
        reader: &mut Reader<T>,
    ) -> Result<()> {
        // Clear parser state
        self.clear();
        // Set current dialect name
        self.dialect_name = Some(dialect_name.to_string());

        // Allocate buffer
        let mut buf = Vec::new();

        // Parse XML message definition
        loop {
            match reader.read_event_into(&mut buf) {
                Err(e) => panic!("Error at position {}: {:?}", reader.buffer_position(), e),
                // exits the loop when reaching end of file
                Ok(Event::Eof) => break,
                // Process tag start
                Ok(Event::Start(e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    self.push_tag(tag_name.as_ref())?;

                    match self.tag_stack.last().unwrap().as_str() {
                        // MavLink
                        "mavlink" => self.process_mavlink_start()?,
                        // Include
                        "include" => self.process_include_start()?,
                        // Version
                        "version" => self.process_version_start()?,
                        // Dialect
                        "dialect" => self.process_dialect_start()?,

                        // Enum
                        "enum" => self.process_enum_start(e)?,
                        // Enum entry
                        "entry" => self.process_enum_entry_start(e)?,
                        // Enum entry param
                        "param" => self.process_enum_entry_mav_cmd_param_start(e)?,

                        // Message
                        "message" => self.process_message_start(e)?,
                        // Message field
                        "field" => self.process_message_field_start(e)?,

                        // Description
                        "description" => self.process_description_start()?,
                        // Deprecation status
                        "deprecated" => self.process_deprecated_start(e)?,

                        // Message fields extensions section
                        "extensions" => self.process_message_extensions()?,
                        // Message is work in progress
                        "wip" => self.process_wip()?,
                        &_ => {}
                    }
                }
                // Process text within tag
                Ok(Event::Text(t)) => self.process_text(t)?,
                Ok(Event::End(e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    // Pop tag and verify tag consistency
                    self.pop_tag(tag_name.as_str())?;

                    // Clone context stack for safety
                    let context_stack = self.context_stack.clone();

                    match context_stack.last() {
                        // Include
                        Some(XmlParsingContext::Include(include)) => {
                            self.process_include_end(include)?
                        }
                        // Version
                        Some(XmlParsingContext::Version(version)) => {
                            self.process_version_end(*version)?
                        }
                        // Dialect
                        Some(XmlParsingContext::Dialect(dialect)) => {
                            self.process_dialect_end(*dialect)?
                        }

                        // Enum (will be parsed and pushed to results)
                        Some(XmlParsingContext::Enum(xml_enum)) => {
                            self.process_enum_end(xml_enum)?
                        }
                        // Enum entry
                        Some(XmlParsingContext::EnumEntry(entry)) => {
                            self.process_enum_entry_end(entry)?
                        }
                        // Enum entry `MAV_CMD` param
                        Some(XmlParsingContext::EnumEntryMavCmdParam(param)) => {
                            self.process_enum_entry_mav_cmd_param_end(param)?
                        }

                        // Message (will be parsed and pushed to results)
                        Some(XmlParsingContext::Message { msg: xml_msg, .. }) => {
                            self.process_message_end(xml_msg)?
                        }
                        // Message field
                        Some(XmlParsingContext::MessageField(fld)) => {
                            self.process_message_field_end(fld)?
                        }

                        // Description
                        Some(XmlParsingContext::Description(description)) => {
                            self.process_description_end(description.to_string())?
                        }
                        // Deprecation status
                        Some(XmlParsingContext::Deprecated(deprecated)) => {
                            self.process_deprecated_end(deprecated)?;
                        }
                        _ => {}
                    }

                    self.context_stack.pop_tag(tag_name.as_str());
                }
                Ok(Event::Empty(e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                    match tag_name.as_str() {
                        // Enum
                        "enum" => {
                            self.process_enum_start(e)?;
                            if let Some(XmlParsingContext::Enum(value)) =
                                self.context_stack.clone().last()
                            {
                                self.process_enum_end(value)?;
                            }
                            self.context_stack.pop_tag(tag_name.as_str());
                        }
                        // Enum entry
                        "entry" => {
                            self.process_enum_entry_start(e)?;
                            if let Some(XmlParsingContext::EnumEntry(value)) =
                                self.context_stack.clone().last()
                            {
                                self.process_enum_entry_end(value)?;
                            }
                            self.context_stack.pop_tag(tag_name.as_str());
                        }
                        // Enum entry param
                        "param" => {
                            self.process_enum_entry_mav_cmd_param_start(e)?;
                            if let Some(XmlParsingContext::EnumEntryMavCmdParam(value)) =
                                self.context_stack.clone().last()
                            {
                                self.process_enum_entry_mav_cmd_param_end(value)?;
                            }
                            self.context_stack.pop_tag(tag_name.as_str());
                        }

                        // Message
                        "message" => {
                            self.process_message_start(e)?;
                            if let Some(XmlParsingContext::Message { msg, .. }) =
                                self.context_stack.clone().last()
                            {
                                self.process_message_end(msg)?;
                            }
                            self.context_stack.pop_tag(tag_name.as_str());
                        }
                        // Message field
                        "field" => {
                            self.process_message_field_start(e)?;
                            if let Some(XmlParsingContext::MessageField(value)) =
                                self.context_stack.clone().last()
                            {
                                self.process_message_field_end(value)?;
                            }
                            self.context_stack.pop_tag(tag_name.as_str());
                        }

                        // Description
                        "description" => {} // Skipping empty description
                        // Deprecation status
                        "deprecated" => {
                            self.process_deprecated_start(e)?;
                            if let Some(XmlParsingContext::Deprecated(value)) =
                                self.context_stack.clone().last()
                            {
                                self.process_deprecated_end(value)?;
                            }
                            self.context_stack.pop_tag(tag_name.as_str());
                        }

                        // Message fields extensions section
                        "extensions" => self.process_message_extensions()?,
                        // Message is work in progress
                        "wip" => self.process_wip()?,

                        &_ => {}
                    }
                }
                // There are several other `Event`s we do not consider here
                _ => (),
            }
        }

        Ok(())
    }

    fn process_mavlink_start(&mut self) -> Result<()> {
        self.context_stack.push(XmlParsingContext::MavLink)
    }

    fn process_include_start(&mut self) -> Result<()> {
        self.context_stack
            .push(XmlParsingContext::Include("".to_string()))
    }

    fn process_include_end(&mut self, content: &str) -> Result<()> {
        self.includes.push(content.to_string());
        Ok(())
    }

    fn process_version_start(&mut self) -> Result<()> {
        self.context_stack.push(XmlParsingContext::Version(0))
    }

    fn process_version_end(&mut self, version: u8) -> Result<()> {
        self.version = Some(version);
        Ok(())
    }

    fn process_dialect_start(&mut self) -> Result<()> {
        self.context_stack.push(XmlParsingContext::Dialect(0))
    }

    fn process_dialect_end(&mut self, dialect: u32) -> Result<()> {
        self.dialect_id = Some(dialect);
        Ok(())
    }

    fn process_enum_start(&mut self, bytes_start: BytesStart) -> Result<()> {
        let mut name: String = "".to_string();
        let mut description: String = "".to_string();
        let mut bitmask = false;

        for attr in bytes_start.attributes() {
            let attr = attr.unwrap();
            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
            let value = String::from_utf8_lossy(attr.value.as_ref()).to_string();

            match key.as_ref() {
                "name" => name = value,
                "description" => description = value,
                "bitmask" => {
                    if value == "true" {
                        bitmask = true
                    }
                }
                &_ => {}
            }
        }

        self.context_stack.push(XmlParsingContext::Enum(XmlEnum {
            name,
            bitmask,
            description,
            entries: HashMap::new(),
            deprecated: None,
            defined_in: Some(self.dialect_canonical_name()),
        }))?;

        Ok(())
    }

    fn process_enum_end(&mut self, xml_enum: &XmlEnum) -> Result<()> {
        let enum_name = xml_enum.name.clone();
        let new_enum = xml_enum.to_enum()?;

        if self.enums.contains_key(&enum_name) {
            let old_enum = self.enums.get(&enum_name).unwrap();
            let mut entries: HashMap<String, EnumEntry> = HashMap::from_iter(
                old_enum
                    .entries()
                    .iter()
                    .map(|entry| (entry.name().to_string(), entry.clone())),
            );

            for entry in new_enum.entries() {
                entries.insert(entry.name().to_string(), entry.clone());
            }

            // Insert as updated enum
            let updated_enum = new_enum
                .to_builder()
                .set_entries(
                    entries
                        .values()
                        .cloned()
                        .collect::<Vec<EnumEntry>>()
                        .as_slice(),
                )
                .set_defined_in(self.dialect_canonical_name())
                .build();
            self.enums.insert(enum_name.clone(), updated_enum);
        } else {
            // Insert new enum
            self.enums.insert(enum_name, new_enum);
        }

        Ok(())
    }

    fn process_enum_entry_start(&mut self, bytes_start: BytesStart) -> Result<()> {
        if let Some(XmlParsingContext::Enum(_)) = self.context_stack.last() {
            let mut enum_value = "".to_string();
            let mut name = "".to_string();
            let mut description = "".to_string();
            let mut cmd_flags = XmlEnumEntryMavCmdFlags {
                has_location: None,
                is_destination: None,
                mission_only: None,
            };

            for attr in bytes_start.attributes() {
                let attr = attr.unwrap();
                let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                let value = String::from_utf8_lossy(attr.value.as_ref()).to_string();

                match key.as_ref() {
                    "value" => enum_value = value,
                    "name" => name = value,
                    "description" => description = value,
                    "hasLocation" => {
                        cmd_flags.has_location = Some(
                            value
                                .parse::<bool>()
                                .map_err(XmlParseError::EnumEntryMavCmdFlagsInvalidHasLocation)?,
                        )
                    }
                    "isDestination" => {
                        cmd_flags.is_destination = Some(
                            value
                                .parse::<bool>()
                                .map_err(XmlParseError::EnumEntryMavCmdFlagsInvalidIsDestination)?,
                        )
                    }
                    "missionOnly" => {
                        cmd_flags.mission_only = Some(
                            value
                                .parse::<bool>()
                                .map_err(XmlParseError::EnumEntryMavCmdFlagsInvalidMissionOnly)?,
                        )
                    }
                    &_ => {}
                }
            }

            let enum_name = if let Some(XmlParsingContext::Enum(enm)) = self.context_stack.last() {
                enm.name.clone()
            } else {
                "".to_string()
            };

            self.context_stack
                .push(XmlParsingContext::EnumEntry(XmlEnumEntry {
                    value: enum_value,
                    name,
                    enum_name,
                    description,
                    cmd_flags,
                    params: Vec::new(),
                    wip: false,
                    deprecated: None,
                    defined_in: self.dialect_name.clone(),
                }))?;
        }

        Ok(())
    }

    fn process_enum_entry_end(&mut self, entry: &XmlEnumEntry) -> Result<()> {
        let subject = self
            .context_stack
            .parent_context_mut(XmlParseError::EnumEntryWithoutEnum)?;

        if let XmlParsingContext::Enum(xml_enum) = subject {
            xml_enum.entries.insert(entry.name.clone(), entry.clone());
        }

        Ok(())
    }

    fn process_enum_entry_mav_cmd_param_start(&mut self, bytes_start: BytesStart) -> Result<()> {
        if let Some(XmlParsingContext::EnumEntry(_)) = self.context_stack.last() {
            let mut index = "".to_string();
            let mut description = "".to_string();
            let mut label = None;
            let mut units = None;
            let mut r#enum = None;
            let mut decimal_places = None;
            let mut increment = None;
            let mut min_value = None;
            let mut max_value = None;
            let mut reserved = None;
            let mut default = None;

            for attr in bytes_start.attributes() {
                let attr = attr.unwrap();
                let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                let value = String::from_utf8_lossy(attr.value.as_ref()).to_string();

                match key.as_ref() {
                    "index" => index = value,
                    "description" => description = value,
                    "label" => label = Some(value),
                    "units" => units = Some(value),
                    "enum" => r#enum = Some(value),
                    "decimalPlaces" => decimal_places = Some(value),
                    "increment" => increment = Some(value),
                    "minValue" => min_value = Some(value),
                    "maxValue" => max_value = Some(value),
                    "reserved" => reserved = Some(value),
                    "default" => default = Some(value),
                    &_ => {}
                }
            }

            self.context_stack
                .push(XmlParsingContext::EnumEntryMavCmdParam(
                    XmlEnumEntryMavCmdParam {
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
                ))?;
        }

        Ok(())
    }

    fn process_enum_entry_mav_cmd_param_end(
        &mut self,
        param: &XmlEnumEntryMavCmdParam,
    ) -> Result<()> {
        let subject = self
            .context_stack
            .parent_context_mut(XmlParseError::MavCmdParamWithoutEnumEntry)?;

        if let XmlParsingContext::EnumEntry(xml_enum_entry) = subject {
            xml_enum_entry.params.push(param.clone());
        }

        Ok(())
    }

    fn process_deprecated_start(&mut self, bytes_start: BytesStart) -> Result<()> {
        let mut since = "".to_string();
        let mut replaced_by = "".to_string();

        for attr in bytes_start.attributes() {
            let attr = attr.unwrap();
            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
            let value = String::from_utf8_lossy(attr.value.as_ref()).to_string();

            match key.as_ref() {
                "since" => since = value,
                "replaced_by" => replaced_by = value,
                &_ => {}
            }
        }

        self.context_stack
            .push(XmlParsingContext::Deprecated(XmlDeprecated {
                since,
                replaced_by,
            }))?;

        Ok(())
    }

    fn process_deprecated_end(&mut self, deprecated: &XmlDeprecated) -> Result<()> {
        let subject = self
            .context_stack
            .parent_context_mut(XmlParseError::DeprecationWithoutSubject)?;

        match subject {
            XmlParsingContext::Enum(enm) => enm.deprecated = Some(deprecated.clone()),
            XmlParsingContext::EnumEntry(entry) => entry.deprecated = Some(deprecated.clone()),
            XmlParsingContext::Message { msg, .. } => msg.deprecated = Some(deprecated.clone()),
            &mut _ => {}
        }

        Ok(())
    }

    fn process_description_start(&mut self) -> Result<()> {
        self.context_stack
            .push(XmlParsingContext::Description("".to_string()))?;

        Ok(())
    }

    fn process_description_end(&mut self, description: String) -> Result<()> {
        let subject = self
            .context_stack
            .parent_context_mut(XmlParseError::DescriptionWithoutSubject)?;

        match subject {
            XmlParsingContext::Enum(enm) => enm.description = description,
            XmlParsingContext::EnumEntry(entry) => entry.description = description,
            XmlParsingContext::Message { msg, .. } => msg.description = description,
            &mut _ => {}
        }

        Ok(())
    }

    fn process_message_start(&mut self, bytes_start: BytesStart) -> Result<()> {
        let mut id = "".to_string();
        let mut name = "".to_string();
        let mut description = "".to_string();

        for attr in bytes_start.attributes() {
            let attr = attr.unwrap();
            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
            let value = String::from_utf8_lossy(attr.value.as_ref()).to_string();

            match key.as_ref() {
                "id" => id = value,
                "name" => name = value,
                "description" => description = value,
                &_ => {}
            }
        }

        self.context_stack.push(XmlParsingContext::Message {
            msg: XmlMessage {
                id,
                name,
                description,
                fields: vec![],
                wip: false,
                deprecated: None,
                defined_in: self.dialect_name.clone(),
            },
            in_extension_section: false,
        })?;

        Ok(())
    }

    fn process_message_end(&mut self, xml_message: &XmlMessage) -> Result<()> {
        let message_id = xml_message
            .id
            .parse::<MessageId>()
            .map_err(XmlParseError::IncorrectMessageId)?;
        let message = xml_message.to_message()?;

        if self.messages.contains_key(&message_id) {
            log::warn!(
                "Message '{}' is already defined in dialect '{:?}' but found in {:?}. \
                It will be replaced with current definition.",
                message.name(),
                self.messages.get(&message_id).unwrap().defined_in(),
                message.defined_in(),
            );
        }

        self.messages.insert(message_id, message);
        Ok(())
    }

    fn process_message_field_start(&mut self, bytes_start: BytesStart) -> Result<()> {
        if let Some(XmlParsingContext::Message {
            in_extension_section,
            ..
        }) = self.context_stack.last()
        {
            let mut name = "".to_string();
            let mut description = "".to_string();
            let mut field_type = "".to_string();
            let mut r#enum: Option<String> = None;
            let mut units = None;
            let mut bitmask = false;
            let mut print_format: Option<String> = None;
            let mut default: Option<String> = None;
            let mut invalid: Option<String> = None;
            let mut instance = false;

            for attr in bytes_start.attributes() {
                let attr = attr.unwrap();
                let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                let value = String::from_utf8_lossy(attr.value.as_ref()).to_string();

                match key.as_ref() {
                    "name" => name = value,
                    "description" => description = value,
                    "type" => field_type = value,
                    "enum" => r#enum = Some(value),
                    "units" => units = Some(value),
                    "bitmask" => {
                        bitmask = value.parse::<bool>().map_err(|err| {
                            XmlParseError::IncorrectMessageFieldInstance(err.clone())
                        })?
                    }
                    "print_format" => print_format = Some(value),
                    "default" => default = Some(value),
                    "invalid" => invalid = Some(value),
                    "instance" => {
                        instance = value.parse::<bool>().map_err(|err| {
                            XmlParseError::IncorrectMessageFieldInstance(err.clone())
                        })?
                    }
                    &_ => {}
                }
            }

            self.context_stack
                .push(XmlParsingContext::MessageField(XmlMessageField {
                    name,
                    description,
                    field_type,
                    r#enum,
                    units,
                    bitmask,
                    print_format,
                    default,
                    invalid,
                    instance,
                    extension: *in_extension_section,
                }))?;
        }

        Ok(())
    }

    fn process_message_field_end(&mut self, fld: &XmlMessageField) -> Result<()> {
        let subject = self
            .context_stack
            .parent_context_mut(XmlParseError::MessageFieldWithoutMessage)?;

        if let XmlParsingContext::Message { msg: message, .. } = subject {
            message.fields.push(fld.clone())
        }

        Ok(())
    }

    fn process_wip(&mut self) -> Result<()> {
        let subject = self
            .context_stack
            .last_mut()
            .ok_or(XmlParseError::WipWithoutSubject)?;

        match subject {
            XmlParsingContext::Message {
                msg: xml_message, ..
            } => xml_message.wip = true,
            XmlParsingContext::EnumEntry(xml_enum) => xml_enum.wip = true,
            &mut _ => return Err(XmlParseError::WipWithoutSubject.into()),
        }

        Ok(())
    }

    fn process_message_extensions(&mut self) -> Result<()> {
        let subject = self
            .context_stack
            .last_mut()
            .ok_or(XmlParseError::MessageExtensionsWithoutMessage)?;

        if let XmlParsingContext::Message {
            in_extension_section,
            ..
        } = subject
        {
            *in_extension_section = true;
        }

        match subject {
            XmlParsingContext::Message {
                in_extension_section,
                ..
            } => *in_extension_section = true,
            &mut _ => return Err(XmlParseError::MessageExtensionsWithoutMessage.into()),
        }

        Ok(())
    }

    fn process_text(&mut self, bytes_text: BytesText) -> Result<()> {
        match self.context_stack.last() {
            Some(XmlParsingContext::Description(_)) => {}
            Some(XmlParsingContext::MessageField(_)) => {}
            Some(XmlParsingContext::EnumEntryMavCmdParam(_)) => {}
            Some(XmlParsingContext::Include(_)) => {}
            Some(XmlParsingContext::Version(_)) => {}
            Some(XmlParsingContext::Dialect(_)) => {}
            _ => return Ok(()),
        }

        let text = String::from_utf8_lossy(bytes_text.as_ref()).to_string();
        let subject = self
            .context_stack
            .last_mut()
            .ok_or(XmlParseError::DescriptionWithoutSubject)?;

        match subject {
            XmlParsingContext::Include(s) => *s = text,
            XmlParsingContext::Version(s) => {
                *s = text
                    .parse::<u8>()
                    .map_err(XmlParseError::VersionParseFailed)?
            }
            XmlParsingContext::Dialect(s) => {
                *s = text
                    .parse::<u32>()
                    .map_err(XmlParseError::DialectIdParseFailed)?
            }
            XmlParsingContext::Description(s) => *s = text,
            XmlParsingContext::MessageField(fld) => fld.description = text,
            XmlParsingContext::EnumEntryMavCmdParam(param) => param.description = text,
            &mut _ => {}
        }

        Ok(())
    }

    fn clear(&mut self) {
        self.includes = Vec::new();
        self.version = None;
        self.dialect_id = None;

        self.tag_stack = Vec::new();
        self.context_stack = XmlParsingContextStack::new();
    }

    fn push_tag(&mut self, opening_tag: &str) -> Result<()> {
        self.tag_stack.push(opening_tag.to_string());
        Ok(())
    }

    fn pop_tag(&mut self, closing_tag: &str) -> Result<()> {
        let last_opened_tag = self.tag_stack.last().map(|t| t.to_string());
        match last_opened_tag {
            Some(previous_tag) if previous_tag == closing_tag => {}
            _ => {
                return Err(XmlParseError::UnexpectedClosingTag {
                    opened_with: last_opened_tag,
                    closed_with: closing_tag.to_string(),
                }
                .into())
            }
        }

        self.tag_stack.pop();
        Ok(())
    }

    fn dialect_canonical_name(&self) -> String {
        dialect_canonical_name(self.dialect_name.as_ref().unwrap())
    }
}
