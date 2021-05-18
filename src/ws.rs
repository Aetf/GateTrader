use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use async_tungstenite::async_std::ConnectStream;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use futures_util::stream::{self, StreamExt as _};
use serde::Deserializer;

pub mod channels;

#[derive(Debug)]
pub struct Dispatcher {
    socket: stream::SplitStream<WebSocketStream<ConnectStream>>,
    subscribers: HashMap<MessageLocation, Sender<ServerMessage>>,
}

impl Dispatcher {
    pub fn new(socket: stream::SplitStream<WebSocketStream<ConnectStream>>) -> Self {
        Self {
            socket,
            subscribers: Default::default(),
        }
    }

    pub fn subscribe(&mut self, channel: impl Into<String>, event: impl Into<String>) -> Receiver<ServerMessage> {
        let (tx, rx) = async_channel::unbounded();
        self.subscribers.insert(
            MessageLocation {
                channel: channel.into(),
                event: event.into(),
            },
            tx,
        );
        rx
    }

    pub async fn run(mut self) {
        // take out subscribers so it can be handed into the future
        let subscribers = std::mem::take(&mut self.subscribers);
        self.socket
            .for_each_concurrent(3, |msg| async {
                let res: Result<()> = try {
                    let msg = msg.context("Get websockets message")?;
                    let msg = match msg {
                        Message::Text(text) => {
                            serde_json::from_str(&text).context(format!("Message is not valid: {:#?}", &text))?
                        }
                        Message::Binary(data) => {
                            let text = String::from_utf8(data).map_err(|e| {
                                let bytes = e.into_bytes();
                                anyhow!("Message is not text: {:#?}", bytes)
                            })?;
                            serde_json::from_str(&text).context(format!("Message is not valid: {:#?}", &text))?
                        }
                        // being lazying here and just stuff the ping into a server message
                        Message::Ping(data) => ServerMessage {
                            time: 0,
                            id: None,
                            which: MessageLocation {
                                channel: "ping".into(),
                                event: "".into(),
                            },
                            content: Ok(serde_json::to_value(data).context("can't serialize ping data to json")?),
                        },
                        msg => Err(anyhow!("Unhandled message: {:#?}", msg))?,
                    };
                    if let Some(tx) = subscribers.get(&msg.which) {
                        tx.send(msg).await?;
                    } else {
                        println!("Unknown message from server: {:#?}", msg);
                    }
                };
                if let Err(e) = res {
                    eprintln!("{:?}", e)
                }
            })
            .await;
    }
}

/// data sent by server in ws socket
#[derive(Debug)]
pub struct ServerMessage {
    pub time: u64,
    pub id: Option<u64>,
    pub which: MessageLocation,
    pub(crate) content: Result<serde_json::Value, WsError>,
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct MessageLocation {
    pub channel: String,
    pub event: String,
}

impl<'de> serde::Deserialize<'de> for ServerMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Debug, serde::Deserialize)]
        struct WsResponse {
            time: u64,
            id: Option<u64>,
            channel: String,
            event: String,
            error: Option<WsError>,
            result: Option<serde_json::Value>,
        }
        let msg = WsResponse::deserialize(deserializer)?;
        // save a string for error reporting
        let msg_str = format!("{:#?}", &msg);

        match (msg.result, msg.error) {
            (Some(result), _) => Ok(ServerMessage {
                time: msg.time,
                id: msg.id,
                which: MessageLocation {
                    channel: msg.channel,
                    event: msg.event,
                },
                content: Ok(result),
            }),
            (_, Some(error)) => Ok(ServerMessage {
                time: msg.time,
                id: msg.id,
                which: MessageLocation {
                    channel: msg.channel,
                    event: msg.event,
                },
                content: Err(error),
            }),
            _ => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Other(&msg_str),
                &"an object with non-null result or error fields",
            )),
        }
    }
}

/// data sent to server by client in ws socket
pub enum WsRequest {
    GateIo {
        channel: String,
        event: String,
        payload: serde_json::Value,
    },
    Pong(Vec<u8>),
}

#[derive(Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WsError {
    code: u64,
    message: String,
}

pub fn ws_build_message(req: WsRequest, id: u64, key: impl Into<String>, secret: impl AsRef<[u8]>) -> Result<Message> {
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let msg = match req {
        WsRequest::GateIo {
            channel,
            event,
            payload,
        } => {
            let obj = serde_json::json!({
                "time": time,
                "id": id,
                "channel": channel,
                "event": event,
                "payload": payload,
                "auth": crate::auth::ws_sign(channel, event, time, key, secret),
            });
            Message::Text(serde_json::to_string(&obj)?)
        }
        WsRequest::Pong(data) => Message::Pong(data),
    };
    Ok(msg)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn ws_response_de() {
        const JSON: &str = "{\"time\":1621262774,\"channel\":\"spot.balances\",\"event\":\"update\",\"result\":[{\"timestamp\":\"1621262774\",\"timestamp_ms\":\"1621262774038\",\"user\":\"2851747\",\"currency\":\"ERG\",\"change\":\"1.010939090000\",\"total\":\"1.01174993000000000000\",\"available\":\"1.01174993000000000000\"}]}";
        let resp: ServerMessage = serde_json::from_str(JSON).unwrap();
        dbg!(resp);
    }
}
