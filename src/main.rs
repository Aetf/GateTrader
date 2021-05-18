#![feature(try_blocks)]

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Error, Result};
use async_std::task;
use async_tungstenite::async_std::{self as async_ws, ConnectStream};
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use futures_util::sink::{self, SinkExt as _};
use futures_util::stream::{self, StreamExt as _};
use surf::{Client, Url};

use crate::rest::CurrencyPair;
use crate::ws::channels::SpotBalance;
use crate::ws::Dispatcher;
use async_channel::Sender;
use auth::GateIoAuth;
use auth::WsAuth;
use futures_util::TryStreamExt;
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use surf::http::Method;

mod auth;
mod rest;
mod utils;
mod ws;

/// data sent to server by client in ws socket
enum WsRequest {
    GateIo {
        channel: String,
        event: String,
        payload: serde_json::Value,
    },
    GateIoAuth {
        channel: String,
        event: String,
        payload: serde_json::Value,
    },
    Pong(Vec<u8>),
}

#[derive(Debug, Eq, PartialEq, serde::Deserialize)]
struct WsError {
    code: u64,
    message: String,
}

#[derive(Debug)]
struct Trader {
    ws_tx: Sender<WsRequest>,
    rest_client: Client,
    dispatcher: Dispatcher,
}

impl Trader {
    pub async fn spawn(key: impl Into<String>, secret: impl Into<String>) -> Result<Self> {
        let key = key.into();
        let secret = secret.into();

        // REST API client is already ARC internally, and can be shared among threads
        let client = Client::new()
            .with(rest::BaseUrl::new(Url::parse("https://api.gateio.ws/api/v4/")?))
            .with(GateIoAuth::new(key.clone(), secret.clone()))
            .with(rest::Logger);
        println!("[OK] gate.io REST API");

        // connect websockets
        let (ws, _) = async_ws::connect_async("wss://api.gateio.ws/ws/v4/").await?;
        let (mut ws_write, ws_read) = ws.split();
        println!("[ -] gate.io websocket API connection...");

        // spawn one task for websocket API sending, taking messages from a queue, so any tasks/threads can send
        // message
        let ws_tx = {
            let (ws_tx, mut rx) = async_channel::unbounded::<WsRequest>();
            task::spawn({
                async move {
                    let key = key;
                    let secret = secret;
                    let mut counter = 0;
                    while let Some(req) = rx.next().await {
                        counter += 1;
                        let res: Result<()> = try {
                            let msg = ws_build_message(req, counter, key.clone(), &secret)?;
                            ws_write.send(msg).await?;
                        };
                        match res {
                            Err(e) => eprintln!("{:#?}", e),
                            Ok(_) => (),
                        }
                    }
                }
            });
            ws_tx
        };
        println!("[ -] gate.io websocket API sender...");

        // spawn one task to dispatch received ws messages
        let mut dispatcher = ws::Dispatcher::new(ws_read);
        println!("[ -] gate.io websocket API dispatcher...");

        // spawn one task to response to ping
        let mut ping_rx = dispatcher.subscribe("ping", "");
        task::spawn({
            // this instance will be saved in the closure
            let ws_tx = ws_tx.clone();
            ping_rx.for_each_concurrent(None, move |msg| {
                // this instance will be saved in the async block
                let ws_tx = ws_tx.clone();
                async move {
                    let res: Result<()> = try {
                        match msg.content {
                            Ok(val) => {
                                let data = serde_json::from_value(val).context("deserialize ping data from json")?;
                                ws_tx.send(WsRequest::Pong(data)).await?;
                            }
                            Err(e) => {
                                eprintln!("Got server error: {:#?}", e);
                            }
                        }
                    };
                    if let Err(e) = res {
                        eprintln!("{:#?}", e);
                    }
                }
            })
        });
        println!("[OK] gate.io websocket API ping pong...");

        Ok(Self {
            rest_client: client,
            ws_tx,
            dispatcher,
        })
    }

    /// monitor the spot balance of `src` coin, and sell them out as `dst` coin
    /// as soon as there are nonzero balance
    pub async fn trade(&mut self, src: impl Into<String>, dst: impl Into<String>) -> Result<()> {
        let src = src.into();
        let dst = dst.into();
        let rest_client = self.rest_client.clone();

        // check basic info about the currency pair
        let info: rest::CurrencyPair = {
            let url = Url::parse(&format!("spot/currency_pairs/{}_{}", &src, &dst))?;
            let req = surf::RequestBuilder::new(Method::Get, url).build();
            rest_client
                .send(req)
                .await
                .map_err(|e| anyhow!(e))?
                .body_json()
                .await
                .map_err(|e| anyhow!(e))?
        };

        let mut rx = self.dispatcher.subscribe("spot.balances", "update");
        let fut = rx
            .map(|msg| msg.content.map_err(|e| anyhow!("Got server error: {:#?}", e)))
            .try_filter_map(move |val| {
                // each async block needs its own copy, because they may run concurrently
                let src = src.clone();
                async move {
                    let balances: Vec<SpotBalance> = serde_json::from_value(val).context("Response is invalid")?;
                    // we only interested in one specific coin,
                    let mut balances: Vec<_> = balances.into_iter().filter(|b| b.currency == src).collect();
                    // only need the latest one
                    balances.sort_unstable_by_key(|b| b.timestamp);
                    Ok(balances.pop())
                }
            })
            .and_then(move |balance| sell_balance(rest_client.clone(), balance, info.clone()))
            .for_each_concurrent(None, |res| async {
                if let Err(e) = res {
                    eprintln!("{:#?}", e);
                }
            });
        task::spawn(fut);
        println!("[OK] spot.balances");
        Ok(())
    }

    pub async fn run(mut self) -> Result<()> {
        let handle = task::spawn(self.dispatcher.run());
        println!("[OK] gate.io websocket API dispatcher run");

        handle.await;
        Ok(())
    }
}

fn ws_build_message(req: WsRequest, id: u64, key: impl Into<String>, secret: impl AsRef<[u8]>) -> Result<Message> {
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
            });
            Message::Text(serde_json::to_string(&obj)?)
        }
        WsRequest::GateIoAuth {
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
                "auth": auth::ws_sign(channel, event, time, key, secret),
            });
            Message::Text(serde_json::to_string(&obj)?)
        }
        WsRequest::Pong(data) => Message::Pong(data),
    };
    Ok(msg)
}

async fn sell_balance(_client: Client, balance: ws::channels::SpotBalance, info: CurrencyPair) -> Result<()> {
    // no need to trade if we don't have fund
    if balance.available < info.min_base_amount {
        return Ok(());
    }
    // TODO: get current market price
    let price: Decimal = todo!();
    // TODO: place order
    let total: Decimal = todo!();
    println!(
        "Sell {} {} => {} {} @ {} {}",
        balance.available, &info.base, total, &info.quote, price, &info.quote
    );
    Ok(())
}

#[async_std::main]
async fn main() -> Result<()> {
    let mut trader = Trader::spawn(
        "b8ffbcfa3e8eafc345ade75f84c4a490",
        "ced122bd871e07a142265503ff84118737341bc07717c784e9e4d167a2e25699",
    )
    .await?;

    trader.trade("ERG", "USDT").await?;

    trader.run().await?;

    Ok(())
}
