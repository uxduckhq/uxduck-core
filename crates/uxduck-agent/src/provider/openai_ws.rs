//! One bounded, cancellable Responses socket per model round, including its
//! native steering continuations. Tool execution stays in the shared loop.
use super::{ProviderError, ProviderEvent, ProviderRequest, socket::SocketRound};
use crate::input::Inputs;
use futures::{
    SinkExt, StreamExt,
    stream::{self, BoxStream},
};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

pub fn respond(
    url: String,
    headers: reqwest::header::HeaderMap,
    hosted: bool,
    request: ProviderRequest,
    inputs: Inputs,
) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut state = SocketRound::new(&request);
        let work = async {
            let mut handshake = url
                .into_client_request()
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            handshake.headers_mut().extend(headers.clone());
            let (mut socket, _) = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio_tungstenite::connect_async_with_config(
                    handshake,
                    Some(
                        tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                            .max_message_size(Some(8 * 1024 * 1024))
                            .max_frame_size(Some(8 * 1024 * 1024)),
                    ),
                    true,
                ),
            )
            .await
            .map_err(|_| {
                ProviderError::Transport("The model WebSocket connection timed out.".into())
            })?
            .map_err(|e| {
                ProviderError::Transport(format!("The model WebSocket could not connect: {e}"))
            })?;
            let start = if hosted {
                json!({"type":"uxduck.start", "token":headers.get("authorization").and_then(|v|v.to_str().ok()).unwrap_or_default().trim_start_matches("Bearer "),
                    "chat":headers.get("x-uxduck-thread").and_then(|v|v.to_str().ok()).unwrap_or_default(),
                    "share":headers.get("x-uxduck-share").and_then(|v|v.to_str().ok()) == Some("conversations"),
                    "continues":request.continues, "body":super::openai::body(&request)})
            } else {
                state.start()
            };
            socket
                .send(Message::Text(start.to_string().into()))
                .await
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            let mut bytes = 0;
            loop {
                tokio::select! {
                    input = inputs.next(), if state.accepts_input() => {
                        if hosted && !inputs.sharing() { socket.send(Message::Text(json!({"type":"uxduck.share","share":false}).to_string().into())).await.map_err(|e|ProviderError::Transport(e.to_string()))?; }
                        let steer = state.steer(input);
                        socket.send(Message::Text(steer.to_string().into())).await.map_err(|e|ProviderError::Transport(e.to_string()))?;
                    }
                    next = socket.next() => {
                        let next = next.ok_or_else(||ProviderError::Transport("The model socket closed before completion. Unapplied updates remain in the chat.".into()))?
                            .map_err(|e|ProviderError::Transport(e.to_string()))?;
                        match next {
                            Message::Text(text) => {
                                bytes += text.len();
                                if bytes > 8*1024*1024 { return Err(ProviderError::Transport("The model response exceeded its limit.".into())); }
                                let event: Value = serde_json::from_str(&text).map_err(|_|ProviderError::Transport("An unreadable model event arrived.".into()))?;
                                if event["type"] == "uxduck.response.done" {
                                    state.settled(&inputs);
                                    if !state.ready { return Err(ProviderError::Transport("The hosted response ended before completion.".into())); }
                                    for e in state.finish() { let _ = tx.send(Ok(e)); }
                                    break;
                                }
                                let (events, send) = state.event(&event, &inputs)?;
                                for e in events { let _ = tx.send(Ok(e)); }
                                for e in send { socket.send(Message::Text(e.to_string().into())).await.map_err(|e|ProviderError::Transport(e.to_string()))?; }
                                if state.ready && !hosted {
                                    for e in state.finish() { let _ = tx.send(Ok(e)); }
                                    break;
                                }
                            }
                            Message::Ping(data) => { socket.send(Message::Pong(data)).await.map_err(|e|ProviderError::Transport(e.to_string()))?; }
                            Message::Pong(_) => {}
                            _ => return Err(ProviderError::Transport("The model socket closed before completion.".into())),
                        }
                    }
                }
            }
            let _ = socket.close(None).await;
            Ok::<_, ProviderError>(())
        };
        let result = tokio::select! {
            () = tx.closed() => Ok(()),
            result = work => result,
        };
        if let Err(error) = result {
            state.abandon(&inputs);
            let _ = tx.send(Err(error));
        }
    });
    stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, openai::OpenAiProvider};
    use axum::{
        Router,
        extract::ws::{Message as AxumMessage, WebSocketUpgrade},
        routing::get,
    };
    #[tokio::test]
    async fn a_real_socket_delivers_steering_and_preserves_partial_usage() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app=Router::new().route("/v1/responses",get(|ws:WebSocketUpgrade|async move {
            ws.on_upgrade(|mut socket|async move {
                let AxumMessage::Text(text)=socket.recv().await.unwrap().unwrap() else{panic!()};
                let request:Value=serde_json::from_str(&text).unwrap();
                assert_eq!(request["type"],"response.create");assert_eq!(request["store"],false);assert!(request.get("stream").is_none());
                socket.send(AxumMessage::Text(json!({"type":"response.created","response":{"id":"r1"}}).to_string().into())).await.unwrap();
                let AxumMessage::Text(text)=socket.recv().await.unwrap().unwrap() else{panic!()};
                let input:Value=serde_json::from_str(&text).unwrap();
                assert_eq!(input,json!({"type":"response.steer","previous_response_id":"r1","input":"read only"}));
                for event in [json!({"type":"response.steer.accepted","steer":{"id":"s1"}}),
                    json!({"type":"response.incomplete","response":{"id":"r1","incomplete_details":{"reason":"steered"},"output":[],"usage":{"input_tokens":10,"output_tokens":2}}}),
                    json!({"type":"response.created","response":{"id":"r2"}}),
                    json!({"type":"response.output_text.delta","delta":"Read only."}),
                    json!({"type":"response.completed","response":{"id":"r2","output":[{"role":"assistant","content":[{"type":"output_text","text":"Read only."}]}],"usage":{"input_tokens":20,"output_tokens":3}}})] {
                    socket.send(AxumMessage::Text(event.to_string().into())).await.unwrap();
                }
            })
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = OpenAiProvider::new("fixture", Some(&format!("http://{address}/v1")));
        let inputs = Inputs::default();
        let mut stream = provider.respond_with_input(
            ProviderRequest {
                model: "gpt-6-sol".into(),
                instructions: "brief".into(),
                conversation: vec![json!({"role":"user","content":"look"})],
                tools: vec![],
                max_output_tokens: 100,
                effort: None,
                continues: None,
                cache_key: None,
            },
            inputs.clone(),
        );
        assert!(matches!(
            stream.next().await,
            Some(Ok(ProviderEvent::Started))
        ));
        inputs.push(crate::input::Input {
            share: true,
            id: "u2".into(),
            text: "read only".into(),
        });
        let events: Vec<_> =
            tokio::time::timeout(std::time::Duration::from_secs(10), stream.collect())
                .await
                .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e,Ok(ProviderEvent::InputApplied(i)) if i.id=="u2"))
        );
        assert!(events.iter().any(|e|matches!(e,Ok(ProviderEvent::Completed{response_id,usage,..}) if response_id=="r2" && usage.input_tokens==30 && usage.output_tokens==5)));
        server.abort();
    }
}
