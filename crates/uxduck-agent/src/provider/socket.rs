//! Responses WebSocket state, shared by native and browser transports.
//! Only the final response's calls escape this adapter. Calls superseded by
//! steering get explicit tool results and are never executed.
use super::{ProviderError, ProviderEvent, ProviderRequest, Usage, openai};
use crate::{
    conversation,
    input::{Input, Inputs},
};
use serde_json::{Value, json};

#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent wire state and usage availability"
)]
pub struct SocketRound {
    request: Value,
    current: Option<String>,
    waiting: Option<Input>,
    accepted: bool,
    disabled: bool,
    output: Vec<Value>,
    usage: Usage,
    usage_known: bool,
    terminal: Option<Value>,
    pub ready: bool,
}
impl SocketRound {
    #[must_use]
    pub fn new(request: &ProviderRequest) -> Self {
        Self {
            request: create(request),
            current: None,
            waiting: None,
            accepted: false,
            disabled: false,
            output: Vec::new(),
            usage: Usage::default(),
            usage_known: true,
            terminal: None,
            ready: false,
        }
    }
    #[must_use]
    pub fn start(&self) -> Value {
        self.request.clone()
    }
    #[must_use]
    pub fn accepts_input(&self) -> bool {
        !self.disabled && self.current.is_some() && self.waiting.is_none() && !self.ready
    }
    pub fn steer(&mut self, input: Input) -> Value {
        let event = json!({"type":"response.steer", "previous_response_id":self.current, "input": input.text});
        self.waiting = Some(input);
        event
    }
    /// # Errors
    /// A provider failure or an unsupported steering/tool event.
    pub fn event(
        &mut self,
        event: &Value,
        inbox: &Inputs,
    ) -> Result<(Vec<ProviderEvent>, Vec<Value>), ProviderError> {
        let mut events = Vec::new();
        let mut send = Vec::new();
        match event["type"].as_str().unwrap_or_default() {
            "response.created" => {
                events.push(ProviderEvent::Started);
                self.current = event["response"]["id"].as_str().map(str::to_owned);
                if self.accepted {
                    if let Some(input) = self.waiting.take() {
                        self.output
                            .extend(conversation::user(conversation::Wire::OpenAi, &input.text));
                        events.push(ProviderEvent::InputApplied(input));
                    }
                    self.accepted = false;
                    self.terminal = None;
                }
            }
            "response.steer.accepted" => self.accepted = true,
            "response.steer.failed" | "uxduck.steer.refused" => {
                if let Some(input) = self.waiting.take() {
                    inbox.push(input);
                }
                self.accepted = false;
                self.disabled = true;
                self.ready = self.terminal.is_some();
            }
            "response.steer.pending" => {
                let required = event["required_input"].as_array().ok_or_else(|| {
                    ProviderError::Refused("Missing steering tool results.".into())
                })?;
                let mut results = Vec::new();
                for item in required {
                    if item["type"] != "function_call_output" || !item["call_id"].is_string() {
                        return Err(ProviderError::Refused(
                            "Unsupported steering approval. The update remains in the chat.".into(),
                        ));
                    }
                    results.push(json!({"type":"function_call_output", "call_id":item["call_id"], "output":"Superseded by an operator update before execution. Reconsider this action with the new instructions."}));
                }
                self.output.extend(results.clone());
                let mut next = self.request.clone();
                next["previous_response_id"] = json!(self.current);
                next["input"] = json!(results);
                send.push(next);
            }
            "response.completed" | "response.incomplete" => {
                if event["type"] == "response.incomplete"
                    && event["response"]["incomplete_details"]["reason"] != "steered"
                {
                    return openai::event(event).map(|_| (events, send));
                }
                let mut complete = event.clone();
                complete["type"] = json!("response.completed");
                if let Some(ProviderEvent::Completed {
                    output,
                    usage,
                    usage_known,
                    ..
                }) = openai::event(&complete)?
                {
                    self.output.extend(output);
                    self.usage.add(&usage);
                    self.usage_known &= usage_known;
                }
                self.terminal = Some(event["response"].clone());
                self.ready = self.waiting.is_none();
            }
            // Calls are emitted only after the response and any native steering finish.
            "response.output_item.done" => {}
            _ => {
                if let Some(next) = openai::event(event)? {
                    events.push(next);
                }
            }
        }
        Ok((events, send))
    }
    /// The socket failed: an update sent but not yet part of a response goes
    /// back to the inbox, so a retry or the next round still applies it.
    pub fn abandon(&mut self, inbox: &Inputs) {
        if let Some(input) = self.waiting.take() {
            inbox.push(input);
        }
    }
    /// A hosted round can settle just before the client sends an update.
    /// Return any unacknowledged input to the portable boundary path.
    pub fn settled(&mut self, inbox: &Inputs) {
        if !self.accepted && self.terminal.is_some() {
            if let Some(input) = self.waiting.take() {
                inbox.push(input);
            }
            self.ready = true;
        }
    }
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        let response = self.terminal.take().unwrap_or(Value::Null);
        if let Some(output) = response["output"].as_array() {
            for item in output {
                if item["type"] == "function_call" {
                    events.push(ProviderEvent::ToolCall {
                        call_id: item["call_id"].as_str().unwrap_or_default().into(),
                        name: item["name"].as_str().unwrap_or_default().into(),
                        arguments: item["arguments"].as_str().unwrap_or_default().into(),
                    });
                }
            }
        }
        events.push(ProviderEvent::Completed {
            response_id: response["id"].as_str().unwrap_or_default().into(),
            output: std::mem::take(&mut self.output),
            usage: std::mem::take(&mut self.usage),
            usage_known: self.usage_known && !response.is_null(),
        });
        events
    }
}
fn create(request: &ProviderRequest) -> Value {
    let Value::Object(mut body) = openai::body(request) else {
        unreachable!("The Responses body is always an object")
    };
    body.remove("stream");
    body.insert("type".into(), json!("response.create"));
    Value::Object(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> SocketRound {
        SocketRound::new(&ProviderRequest {
            model: "gpt-6-sol".into(),
            instructions: "brief".into(),
            conversation: vec![json!({"role":"user","content":"look"})],
            tools: vec![],
            max_output_tokens: 100,
            effort: None,
            continues: None,
            cache_key: None,
        })
    }
    fn terminal(id: &str, output: &[Value]) -> Value {
        json!({"type":"response.completed","response":{"id":id,"output":output,"usage":{"input_tokens":10,"output_tokens":2}}})
    }
    #[test]
    fn native_steering_commits_at_successor_creation_and_accounts_for_both_responses() {
        let mut round = state();
        let inbox = Inputs::default();
        round
            .event(
                &json!({"type":"response.created","response":{"id":"a"}}),
                &inbox,
            )
            .unwrap();
        assert!(round.accepts_input());
        let steer = round.steer(Input {
            share: true,
            id: "u2".into(),
            text: "stay read only".into(),
        });
        assert_eq!(steer["previous_response_id"], "a");
        round
            .event(&json!({"type":"response.steer.accepted"}), &inbox)
            .unwrap();
        round.event(&terminal("a", &[]), &inbox).unwrap();
        assert!(!round.ready);
        let (events, _) = round
            .event(
                &json!({"type":"response.created","response":{"id":"b"}}),
                &inbox,
            )
            .unwrap();
        assert!(
            matches!(&events[..],[ProviderEvent::Started, ProviderEvent::InputApplied(Input{id,..})] if id=="u2")
        );
        round.event(&terminal("b", &[]), &inbox).unwrap();
        assert!(round.ready);
        let result = round.finish();
        assert!(
            matches!(&result[..],[ProviderEvent::Completed{usage,output,..}] if usage.input_tokens==20 && usage.output_tokens==4 && output[0]["content"]=="stay read only")
        );
    }
    #[test]
    fn a_superseded_tool_gets_a_result_and_is_never_dispatched() {
        let mut round = state();
        let inbox = Inputs::default();
        round
            .event(
                &json!({"type":"response.created","response":{"id":"a"}}),
                &inbox,
            )
            .unwrap();
        round.steer(Input {
            share: true,
            id: "u2".into(),
            text: "do not land".into(),
        });
        round
            .event(&json!({"type":"response.steer.accepted"}), &inbox)
            .unwrap();
        let call =
            json!({"type":"function_call","call_id":"land","name":"command_send","arguments":"{}"});
        round.event(&terminal("a", &[call]), &inbox).unwrap();
        let (_,send)=round.event(&json!({"type":"response.steer.pending","required_input":[{"type":"function_call_output","call_id":"land"}]}),&inbox).unwrap();
        assert_eq!(send[0]["input"][0]["call_id"], "land");
        round
            .event(
                &json!({"type":"response.created","response":{"id":"b"}}),
                &inbox,
            )
            .unwrap();
        round.event(&terminal("b", &[]), &inbox).unwrap();
        let events = round.finish();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProviderEvent::ToolCall { .. }))
        );
        let ProviderEvent::Completed { output, .. } = &events[0] else {
            panic!()
        };
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[1]["type"], "function_call_output");
        assert_eq!(output[2]["content"], "do not land");
    }
    #[test]
    fn an_update_in_flight_when_the_socket_fails_goes_back_to_the_inbox() {
        let mut round = state();
        let inbox = Inputs::default();
        round
            .event(
                &json!({"type":"response.created","response":{"id":"a"}}),
                &inbox,
            )
            .unwrap();
        round.steer(Input {
            share: true,
            id: "u2".into(),
            text: "slower".into(),
        });
        round.abandon(&inbox);
        assert_eq!(inbox.drain()[0].id, "u2");
    }
    #[test]
    fn refused_native_steering_is_returned_to_the_shared_inbox_without_loss() {
        let mut round = state();
        let inbox = Inputs::default();
        round
            .event(
                &json!({"type":"response.created","response":{"id":"a"}}),
                &inbox,
            )
            .unwrap();
        round.steer(Input {
            share: true,
            id: "u2".into(),
            text: "new detail".into(),
        });
        round.event(&terminal("a", &[]), &inbox).unwrap();
        round
            .event(
                &json!({"type":"response.steer.failed","steer":{"id":"s1"}}),
                &inbox,
            )
            .unwrap();
        assert!(round.ready);
        assert!(!round.accepts_input());
        assert_eq!(inbox.drain()[0].id, "u2");
    }
}
