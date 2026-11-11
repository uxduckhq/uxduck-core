use super::entities::deprecated::XmlDeprecated;
use super::entities::enums::{XmlEnum, XmlEnumEntry, XmlEnumEntryMavCmdParam};
use super::entities::messages::{XmlMessage, XmlMessageField};

#[derive(Debug, Clone)]
pub(crate) enum XmlParsingContext {
    MavLink,
    Include(String),
    Version(u8),
    Dialect(u32),
    Enum(XmlEnum),
    EnumEntry(XmlEnumEntry),
    EnumEntryMavCmdParam(XmlEnumEntryMavCmdParam),
    Message {
        msg: XmlMessage,
        in_extension_section: bool,
    },
    MessageField(XmlMessageField),
    Description(String),
    Deprecated(XmlDeprecated),
}

impl XmlParsingContext {
    pub fn to_tag_str(&self) -> &str {
        match self {
            XmlParsingContext::MavLink => "mavlink",
            XmlParsingContext::Include(_) => "include",
            XmlParsingContext::Version(_) => "version",
            XmlParsingContext::Dialect(_) => "dialect",
            XmlParsingContext::Enum(_) => "enum",
            XmlParsingContext::EnumEntry(_) => "entry",
            XmlParsingContext::EnumEntryMavCmdParam(_) => "param",
            XmlParsingContext::Message { .. } => "message",
            XmlParsingContext::MessageField(_) => "field",
            XmlParsingContext::Description(_) => "description",
            XmlParsingContext::Deprecated(_) => "deprecated",
        }
    }
}
