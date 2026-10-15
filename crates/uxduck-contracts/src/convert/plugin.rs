use uxduck_core::domain::plugin as p;

use crate::dto::plugin as dto;

// The two shapes are the same on the wire: convert through it, so a new
// widget field cannot be forgotten on one side. A button's parameters are
// JSON text in the core, as a declared command's schema is, and JSON here.
impl From<&p::Widget> for dto::Widget {
    fn from(v: &p::Widget) -> Self {
        serde_json::to_value(v)
            .map(|mut json| {
                if let Some(text) = json.get("params").and_then(serde_json::Value::as_str) {
                    json["params"] = serde_json::from_str(text).unwrap_or_default();
                }
                json
            })
            .and_then(serde_json::from_value)
            .unwrap_or_else(|_| Self::Text {
                text: String::new(),
                heading: false,
                tone: None,
                help: None,
            })
    }
}

impl From<dto::Widget> for p::Widget {
    fn from(v: dto::Widget) -> Self {
        serde_json::to_value(&v)
            .map(|mut json| {
                if let Some(params) = json.get("params").filter(|p| !p.is_string()) {
                    json["params"] = params.to_string().into();
                }
                json
            })
            .and_then(serde_json::from_value)
            .unwrap_or_else(|_| Self::Text {
                text: String::new(),
                heading: false,
                tone: None,
                help: None,
            })
    }
}

impl From<&p::Panel> for dto::Panel {
    fn from(v: &p::Panel) -> Self {
        Self {
            id: v.id.clone(),
            title: v.title.clone(),
            widgets: v.widgets.iter().map(Into::into).collect(),
            tab: v.tab,
            vehicle: v.vehicle,
        }
    }
}

impl From<dto::Panel> for p::Panel {
    fn from(v: dto::Panel) -> Self {
        Self {
            id: v.id,
            title: v.title,
            widgets: v.widgets.into_iter().map(Into::into).collect(),
            tab: v.tab,
            vehicle: v.vehicle,
        }
    }
}

impl From<&p::Plugin> for dto::Plugin {
    fn from(v: &p::Plugin) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            version: v.version.clone(),
            description: v.description.clone(),
            path: v.path.clone(),
            capabilities: v.capabilities.iter().copied().map(Into::into).collect(),
            subscribe: (&v.subscribe).into(),
            sends: v.sends.clone(),
            commands: v.commands.iter().map(Into::into).collect(),
            status: match v.status {
                p::PluginStatus::Starting => dto::PluginStatus::Starting,
                p::PluginStatus::Running => dto::PluginStatus::Running,
                p::PluginStatus::Failed => dto::PluginStatus::Failed,
                p::PluginStatus::Off => dto::PluginStatus::Off,
            },
            detail: v.detail.clone(),
            panels: v.panels.iter().map(Into::into).collect(),
            client_id: v.client_id(),
            written_by: v.written_by.as_ref().map(|w| dto::WrittenBy {
                agent: w.agent.clone(),
                thread_id: w.thread_id.clone(),
            }),
            waiting: v.waiting.iter().copied().map(Into::into).collect(),
            origin: v.origin.as_ref().map(|o| match o {
                p::PluginOrigin::Synced { digest } => dto::PluginOrigin::Synced {
                    digest: digest.clone(),
                },
                p::PluginOrigin::Shared { shared_by, token } => dto::PluginOrigin::Shared {
                    shared_by: shared_by.clone(),
                    token: token.clone(),
                },
            }),
            enabled: v.enabled,
            included: v.included,
        }
    }
}

impl From<&dto::Subscriptions> for p::Subscriptions {
    fn from(v: &dto::Subscriptions) -> Self {
        Self {
            items: v.items.as_ref().map(|s| p::Subscription {
                to: s.keys.clone(),
                cadence_ms: s.cadence_ms,
            }),
            messages: v.messages.as_ref().map(|s| p::Subscription {
                to: s.names.clone(),
                cadence_ms: s.cadence_ms,
            }),
            frames: v.frames.as_ref().map(|s| p::Subscription {
                to: s.ids.clone(),
                cadence_ms: s.cadence_ms,
            }),
            tick_ms: v.tick_ms,
        }
    }
}

impl From<&p::Subscriptions> for dto::Subscriptions {
    fn from(v: &p::Subscriptions) -> Self {
        Self {
            items: v.items.as_ref().map(|s| dto::ItemSubscription {
                keys: s.to.clone(),
                cadence_ms: s.cadence_ms,
            }),
            messages: v.messages.as_ref().map(|s| dto::MessageSubscription {
                names: s.to.clone(),
                cadence_ms: s.cadence_ms,
            }),
            frames: v.frames.as_ref().map(|s| dto::FrameSubscription {
                ids: s.to.clone(),
                cadence_ms: s.cadence_ms,
            }),
            tick_ms: v.tick_ms,
        }
    }
}

impl From<&dto::DeclaredCommand> for p::DeclaredCommand {
    fn from(v: &dto::DeclaredCommand) -> Self {
        Self {
            name: v.name.clone(),
            title: v.title.clone(),
            description: v.description.clone(),
            parameters: v.parameters.as_ref().map(ToString::to_string),
            shortcut: v.shortcut.clone(),
            risk: match v.risk {
                crate::dto::command::Risk::None => uxduck_core::domain::command::Risk::None,
                crate::dto::command::Risk::Confirm => uxduck_core::domain::command::Risk::Confirm,
                crate::dto::command::Risk::Dangerous => {
                    uxduck_core::domain::command::Risk::Dangerous
                }
            },
        }
    }
}

impl From<&p::DeclaredCommand> for dto::DeclaredCommand {
    fn from(v: &p::DeclaredCommand) -> Self {
        Self {
            name: v.name.clone(),
            title: v.title.clone(),
            description: v.description.clone(),
            parameters: v
                .parameters
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok()),
            shortcut: v.shortcut.clone(),
            risk: match v.risk {
                uxduck_core::domain::command::Risk::None => crate::dto::command::Risk::None,
                uxduck_core::domain::command::Risk::Confirm => crate::dto::command::Risk::Confirm,
                uxduck_core::domain::command::Risk::Dangerous => {
                    crate::dto::command::Risk::Dangerous
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_widget_survives_the_journal() {
        let widgets: Vec<dto::Widget> = serde_json::from_value(serde_json::json!([
            { "type": "text", "text": "Home", "heading": true },
            { "type": "value", "item": "home.distance_m", "label": "From home", "help": "Where it started." },
            { "type": "gauge", "item": "battery.remaining_pct", "label": "Battery", "min": 0.0, "max": 100.0, "warnBelow": 20.0, "tone": "muted" },
            { "type": "chart", "item": "home.distance_m", "label": "Distance", "windowS": 120 },
            { "type": "table", "item": "home.marks", "label": "Marks", "columns": ["Time", "Label"], "tone": "warning" },
            { "type": "button", "label": "Mark", "command": "mark" },
            { "type": "time-input", "label": "Alarm", "command": "alarm-set", "help": "Rings then." }
        ]))
        .unwrap();
        for widget in widgets {
            let core: p::Widget = widget.clone().into();
            assert_eq!(dto::Widget::from(&core), widget);
        }
    }
}
