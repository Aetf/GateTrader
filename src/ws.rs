use std::collections::HashMap;
use std::convert::TryInto;
use std::num::Wrapping;
use std::time;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use async_io::Timer;
use async_tungstenite::async_std as async_ws;
use async_tungstenite::async_std::ConnectStream;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use futures_util::stream;
use futures_util::{select, select_biased, FutureExt, StreamExt as _, TryStreamExt as _};
use serde::Deserializer;

use crate::{eprint_msg, print_msg};

pub mod channels;

#[derive(Debug)]
pub struct WsClient {
    key: String,
    secret: String,
    /// dispatcher
    subscribers: HashMap<MessageLocation, Sender<ServerMessage>>,
    /// sender
    send_req_tx: Sender<WsRequest>,
    send_req_rx: Receiver<WsRequest>,
}

impl WsClient {
    pub fn new(key: impl Into<String>, secret: impl Into<String>) -> Self {
        let (send_req_tx, send_req_rx) = async_channel::unbounded();
        Self {
            key: key.into(),
            secret: secret.into(),
            subscribers: Default::default(),
            send_req_tx,
            send_req_rx,
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

    pub async fn send(&self, req: WsRequest) -> Result<()> {
        self.send_req_tx.send(req).await?;
        Ok(())
    }

    /// connect to the websocket address and run forever, reconnect if necessary
    pub async fn run_forever(&self) -> Result<()> {
        loop {
            let (ws, _) = async_ws::connect_async("wss://api.gateio.ws/ws/v4/").await?;
            print_msg(format_args!("[OK] gate.io websocket API"));
            if let Err(e) = self.run(ws).await {
                eprint_msg(format_args!("[Er] Server connection lost: {}", e));
                print_msg(format_args!("[ -] Reconnecting..."));
            }
        }
    }

    fn build_message(&self, req: WsRequest, id: u64) -> Result<Message> {
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
                    "auth": crate::auth::ws_sign(channel, event, time, &self.key, &self.secret),
                });
                Message::Text(serde_json::to_string(&obj)?)
            }
            WsRequest::Pong(data) => Message::Pong(data),
            WsRequest::Ping(data) => Message::Ping(data),
        };
        Ok(msg)
    }

    /// Given a websocket connection, run the sending and receiving future, and return error on ping or network failure
    async fn run(&self, socket: WebSocketStream<ConnectStream>) -> Result<()> {
        let (sink, stream) = socket.split();
        // setup ping
        let (server_pongs_tx, server_pongs) = async_channel::unbounded();
        // setup subscribers
        let mut channels: Vec<_> = self.subscribers.keys().map(|ml| &ml.channel).collect();
        channels.sort_unstable();
        channels.dedup();
        for channel in channels.into_iter() {
            self.send(WsRequest::GateIo {
                channel: channel.clone(),
                event: "subscribe".to_string(),
                payload: Default::default(),
            })
            .await
            .context("Error subscribing to channel")?;
        }

        // all the following futures only return when connection is lost, so no need to do loop
        select!(
            _ = self.dispatching(stream, server_pongs_tx).fuse() => Ok(()),
            ping = self.ping(server_pongs).fuse() => ping,
            sending = self.sending(self.send_req_rx.clone(), sink).fuse() => sending,
        )?;
        Ok(())
    }

    /// Given a websocket sink, transform WsRequest into Message and send it on the sink.
    /// May return error when connection is lost.
    async fn sending(
        &self,
        send_req_rx: Receiver<WsRequest>,
        sink: stream::SplitSink<WebSocketStream<ConnectStream>, Message>,
    ) -> Result<()> {
        // first create a wrapping counter stream for the purpose of message id
        let counter = {
            let mut counter = Wrapping(0u64);
            stream::repeat_with(move || {
                counter += Wrapping(1);
                counter.0
            })
        };
        send_req_rx
            // zip the counter and incoming requests together
            .zip(counter)
            // build websocket message from request
            .map(|(req, id)| self.build_message(req, id))
            // creating message error is not fatal, just log it
            .filter_map(|res| async { res.map_err(|e| eprint_msg(format_args!("{:?}", e))).ok() })
            // forward is only available on `TryStream`, so wrap msg in a result
            .map(Ok)
            // sending on websocket error is fatal, will return the first error
            .forward(sink)
            .await?;
        Ok(())
    }

    /// Given a websocket stream, dispatch incoming messages to subscribers
    /// Only return on stream ending
    async fn dispatching(
        &self,
        stream: stream::SplitStream<WebSocketStream<ConnectStream>>,
        server_pongs_tx: Sender<Vec<u8>>,
    ) {
        stream
            // convert the incoming ws message stream error to anyhow::Error
            .map_err(|e| anyhow!("Error receiving ws message: {}", e))
            // parse message into ServerMessage
            .and_then(|msg| async {
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
                    // directly send back pong
                    Message::Ping(data) => {
                        self.send(WsRequest::Pong(data)).await?;
                        return Ok(None);
                    }
                    Message::Pong(data) => {
                        server_pongs_tx.send(data).await?;
                        return Ok(None);
                    }
                    msg => return Err(anyhow!("Unhandled message: {:#?}", msg)),
                };
                Ok(Some(msg))
            })
            // pass ServerMessage to subscribers
            .and_then(|msg: Option<ServerMessage>| async {
                if let Some(msg) = msg {
                    if let Some(tx) = self.subscribers.get(&msg.which) {
                        tx.send(msg).await?;
                    } else {
                        eprint_msg(format_args!("Unknown message from server: {:#?}", msg));
                    }
                }
                Ok(())
            })
            // drive the stream to complete. Errors are limited to single message, so just log them
            .for_each_concurrent(3, |res| async {
                if let Err(e) = res {
                    eprint_msg(format_args!("{:?}", e))
                }
            })
            .await;
    }

    /// regularly send ping to server to make sure the connection is alive
    /// will run forever until the connection is lost
    async fn ping(&self, mut server_pongs: Receiver<Vec<u8>>) -> Result<()> {
        let mut counter = Wrapping(0usize);
        loop {
            self.send(WsRequest::Ping(counter.0.to_be_bytes().to_vec())).await?;
            let data: Option<Vec<_>> = select_biased! {
                data = server_pongs.next() => Ok(data),
                _ = FutureExt::fuse(Timer::after(time::Duration::from_secs(5))) => Err(anyhow!("Server did not respond to ping")),
            }?;
            let data = data.ok_or_else(|| anyhow!("Pong data stream closed"))?;
            let data = data
                .chunks(std::mem::size_of::<usize>())
                .next()
                .ok_or_else(|| anyhow!("Pong data incorrect"))?;
            let _got = usize::from_be_bytes(data.try_into().context("Pong data incorrect")?);
            counter += Wrapping(1);
            Timer::after(time::Duration::from_secs(2)).await;
        }
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

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct MessageLocation {
    pub channel: String,
    pub event: String,
}

/// data sent to server by client in ws socket
pub enum WsRequest {
    GateIo {
        channel: String,
        event: String,
        payload: serde_json::Value,
    },
    Pong(Vec<u8>),
    Ping(Vec<u8>),
}

#[derive(Debug, Eq, PartialEq, serde::Deserialize)]
pub struct WsError {
    code: u64,
    message: String,
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
