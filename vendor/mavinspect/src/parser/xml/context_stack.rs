use crate::errors::Result;
use crate::parser::errors::XmlParseError;

use super::context::XmlParsingContext;

#[derive(Debug, Clone)]
pub(crate) struct XmlParsingContextStack {
    stack: Vec<XmlParsingContext>,
}

impl XmlParsingContextStack {
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    pub fn push(&mut self, context: XmlParsingContext) -> Result<()> {
        let previous_tag = self.to_tag_path_str();
        let current_tag = match previous_tag {
            None => context.to_tag_str().to_string(),
            Some(_) => [
                previous_tag.clone().unwrap(),
                context.to_tag_str().to_string(),
            ]
            .join("/"),
        };
        let order_error = XmlParseError::TagsNotInOrder {
            outer: previous_tag,
            inner: current_tag.clone(),
        };

        match context {
            // MavLink (root tag)
            XmlParsingContext::MavLink => match self.last() {
                None => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Include
            XmlParsingContext::Include(_) => match self.last() {
                Some(XmlParsingContext::MavLink) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Version
            XmlParsingContext::Version(_) => match self.last() {
                Some(XmlParsingContext::MavLink) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Include
            XmlParsingContext::Dialect(_) => match self.last() {
                Some(XmlParsingContext::MavLink) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Enum
            XmlParsingContext::Enum(_) => match self.last() {
                Some(XmlParsingContext::MavLink) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Enum entry
            XmlParsingContext::EnumEntry(_) => match self.last() {
                Some(XmlParsingContext::Enum(_)) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Enum entry `MAV_CMD` param
            XmlParsingContext::EnumEntryMavCmdParam(_) => match self.last() {
                Some(XmlParsingContext::EnumEntry(_)) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Message
            XmlParsingContext::Message { .. } => match self.last() {
                Some(XmlParsingContext::MavLink) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Message field
            XmlParsingContext::MessageField(_) => match self.last() {
                Some(XmlParsingContext::Message { .. }) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Description
            XmlParsingContext::Description(_) => match self.last() {
                Some(XmlParsingContext::Enum(_)) => self.stack.push(context),
                Some(XmlParsingContext::EnumEntry(_)) => self.stack.push(context),
                Some(XmlParsingContext::Message { .. }) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
            // Deprecation status
            XmlParsingContext::Deprecated(_) => match self.last() {
                Some(XmlParsingContext::EnumEntry(_)) => self.stack.push(context),
                Some(XmlParsingContext::Enum(_)) => self.stack.push(context),
                Some(XmlParsingContext::Message { .. }) => self.stack.push(context),
                _ => return Err(order_error.into()),
            },
        }

        Ok(())
    }

    pub fn to_tag_path_str(&self) -> Option<String> {
        if self.stack.is_empty() {
            None
        } else {
            let path_items: Vec<&str> = self.stack.iter().map(|item| item.to_tag_str()).collect();
            Some(path_items.join("/").to_string())
        }
    }

    pub fn pop_tag(&mut self, tag: &str) -> Option<XmlParsingContext> {
        match (tag, self.last()) {
            ("mavlink", Some(XmlParsingContext::MavLink)) => {}
            ("include", Some(XmlParsingContext::Include(_))) => {}
            ("version", Some(XmlParsingContext::Version(_))) => {}
            ("dialect", Some(XmlParsingContext::Dialect(_))) => {}
            ("enum", Some(XmlParsingContext::Enum(_))) => {}
            ("entry", Some(XmlParsingContext::EnumEntry(_))) => {}
            ("param", Some(XmlParsingContext::EnumEntryMavCmdParam(_))) => {}
            ("message", Some(XmlParsingContext::Message { .. })) => {}
            ("field", Some(XmlParsingContext::MessageField(_))) => {}
            ("description", Some(XmlParsingContext::Description(_))) => {}
            ("deprecated", Some(XmlParsingContext::Deprecated(_))) => {}
            (_, _) => return None,
        }

        self.stack.pop()
    }

    pub fn last(&self) -> Option<&XmlParsingContext> {
        self.stack.last()
    }

    pub fn last_mut(&mut self) -> Option<&mut XmlParsingContext> {
        self.stack.last_mut()
    }

    pub fn nth_from_end_mut(&mut self, shift: usize) -> Option<&mut XmlParsingContext> {
        if self.stack.is_empty() || self.stack.len() < shift + 1 {
            None
        } else {
            let index = self.stack.len() - (shift + 1);
            Some(self.stack.get_mut(index).unwrap())
        }
    }

    pub fn parent_context_mut(&mut self, err: XmlParseError) -> Result<&mut XmlParsingContext> {
        self.nth_from_end_mut(1).ok_or(err.into())
    }
}
