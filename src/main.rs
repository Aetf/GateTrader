#![feature(try_blocks)]

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use async_channel::Sender;
use async_std::task;
use async_tungstenite::async_std::{self as async_ws};
use async_tungstenite::tungstenite::Message;
use futures_util::sink::SinkExt as _;
use futures_util::stream::StreamExt as _;
use futures_util::TryStreamExt;
use surf::{Client, Url};

use auth::GateIoAuth;
use rest::{CurrencyPair, Order, SpotOrderParams, SpotTicker, SpotTickersParams};
use ws::channels::SpotBalance;
use ws::Dispatcher;

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
                        if let Err(e) = res {
                            eprintln!("{:#?}", e)
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
        let ping_rx = dispatcher.subscribe("ping", "");
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
        let info: rest::CurrencyPair = rest_client
            .get(format!("spot/currency_pairs/{}_{}", &src, &dst))
            .await
            .map_err(|e| anyhow!(e))?
            .body_json()
            .await
            .map_err(|e| anyhow!(e))?;

        let rx = self.dispatcher.subscribe("spot.balances", "update");
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

        // now actually enable the notification
        self.ws_tx
            .send(WsRequest::GateIo {
                channel: "spot.balances".to_string(),
                event: "subscribe".to_string(),
                payload: Default::default(),
            })
            .await?;
        println!("[OK] spot.balances");
        Ok(())
    }

    pub async fn run(self) -> Result<()> {
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
                "auth": auth::ws_sign(channel, event, time, key, secret),
            });
            Message::Text(serde_json::to_string(&obj)?)
        }
        WsRequest::Pong(data) => Message::Pong(data),
    };
    Ok(msg)
}

async fn sell_balance(client: Client, balance: ws::channels::SpotBalance, info: CurrencyPair) -> Result<()> {
    // no need to trade if we don't have fund
    if balance.available < info.min_base_amount {
        return Ok(());
    }
    // get current market price
    let tickers: Vec<SpotTicker> = client
        .get("spot/tickers")
        .query(&SpotTickersParams {
            currency_pair: &info.id,
        })
        .map_err(|e| anyhow!(e))?
        .await
        .map_err(|e| anyhow!(e))?
        .body_json()
        .await
        .map_err(|e| anyhow!(e))?;
    let ticker = tickers
        .last()
        .ok_or_else(|| anyhow!("No info about the currency pair"))?;
    // determine price to place the order
    let price = ticker.highest_bid;
    // place order
    let order: Order = client
        .get("spot/orders")
        .query(&SpotOrderParams {
            text: None,
            currency_pair: &info.id,
            order_type: Some("limit".into()),
            account: Some("spot".into()),
            side: "sell".into(),
            amount: balance.available,
            price,
            time_in_force: None,
            iceberg: None,
            auto_borrow: None,
        })
        .map_err(|e| anyhow!(e))?
        .await
        .map_err(|e| anyhow!(e))?
        .body_json()
        .await
        .map_err(|e| anyhow!(e))?;
    let total = order.amount * order.price;
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
