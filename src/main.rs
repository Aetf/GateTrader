#![feature(try_blocks)]

use anyhow::{anyhow, Context, Result};
use async_channel::Sender;
use async_std::task;
use async_tungstenite::async_std as async_ws;
use futures_util::sink::SinkExt as _;
use futures_util::stream::StreamExt as _;
use futures_util::TryStreamExt;
use structopt::clap::AppSettings;
use structopt::StructOpt;
use surf::{Client, Url};

use auth::GateIoAuth;
use rest::{CurrencyPair, Order, SpotOrderParams, SpotTicker, SpotTickersParams};
use ws::channels::SpotBalance;
use ws::{Dispatcher, WsRequest};

mod auth;
mod rest;
mod utils;
mod ws;

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
        let mut client = Client::new()
            .with(GateIoAuth::new(key.clone(), secret.clone()))
            .with(rest::Logger);
        client.set_base_url(Url::parse("https://api.gateio.ws/api/v4/")?);
        print_msg(format_args!("[OK] gate.io REST API"));

        // connect websockets
        let (ws, _) = async_ws::connect_async("wss://api.gateio.ws/ws/v4/").await?;
        let (mut ws_write, ws_read) = ws.split();
        print_msg(format_args!("[ -] gate.io websocket API connection..."));

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
                            let msg = ws::ws_build_message(req, counter, key.clone(), &secret)?;
                            ws_write.send(msg).await?;
                        };
                        if let Err(e) = res {
                            eprint_msg(format_args!("{:?}", e))
                        }
                    }
                }
            });
            ws_tx
        };
        print_msg(format_args!("[ -] gate.io websocket API sender..."));

        // spawn one task to dispatch received ws messages
        let mut dispatcher = ws::Dispatcher::new(ws_read);
        print_msg(format_args!("[ -] gate.io websocket API dispatcher..."));

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
                                eprint_msg(format_args!("Got server error: {:#?}", e));
                            }
                        }
                    };
                    if let Err(e) = res {
                        eprint_msg(format_args!("{:?}", e));
                    }
                }
            })
        });
        print_msg(format_args!("[ -] gate.io websocket API ping pong..."));

        Ok(Self {
            rest_client: client,
            ws_tx,
            dispatcher,
        })
    }

    pub async fn handle_keyboard(&mut self) -> Result<RawModeGuard> {
        let guard = RawModeGuard::new()?;
        let reader = EventStream::new();
        print_help();

        let client = self.rest_client.clone();

        let fut = reader.for_each(move |e| {
            let client = client.clone();
            async move {
                let res: Result<()> = try {
                    match e? {
                        Event::Key(KeyEvent {
                            code: KeyCode::Char('?' | 'h'),
                            ..
                        }) => {
                            print_help();
                        }
                        Event::Key(KeyEvent {
                            code: KeyCode::Char('p'),
                            ..
                        }) => {
                            print_balance(client).await?;
                        }
                        Event::Key(KeyEvent {
                            code: KeyCode::Char('s'),
                            ..
                        }) => {
                            // TODO: sell balance
                            todo!();
                        }
                        _ => Err(anyhow!("Unhandled event"))?,
                    }
                };
                if let Err(e) = res {
                    eprint_msg(format_args!("{:?}", e));
                }
            }
        });
        task::spawn(fut);
        Ok(guard)
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

        let fut = self
            .dispatcher
            .subscribe("spot.balances", "update")
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
                    eprint_msg(format_args!("{:?}", e));
                }
            });
        task::spawn(fut);
        // show the subscribe response only when it's error
        let fut = self
            .dispatcher
            .subscribe("spot.balances", "subscribe")
            .map(|msg| msg.content.map_err(|e| anyhow!("Got server error: {:#?}", e)))
            .for_each_concurrent(None, |res| async {
                match res {
                    Err(e) => eprint_msg(format_args!("{:?}", e)),
                    Ok(..) => print_msg(format_args!("[OK] spot.balances subscribed")),
                }
            });
        task::spawn(fut);

        // now actually subscribe to the notification on server
        self.ws_tx
            .send(WsRequest::GateIo {
                channel: "spot.balances".to_string(),
                event: "subscribe".to_string(),
                payload: Default::default(),
            })
            .await?;
        Ok(())
    }

    pub async fn run(self) -> Result<()> {
        let handle = task::spawn(self.dispatcher.run());
        print_msg(format_args!("[OK] gate.io websocket API"));

        handle.await;
        Ok(())
    }
}

async fn print_balance(client: Client) -> Result<()> {
    let accounts: Vec<SpotAccount> = client
        .get("spot/accounts")
        .await
        .map_err(|e| anyhow!(e))?
        .body_json()
        .await
        .map_err(|e| anyhow!(e))?;
    print_msg(format_args!("======================================================"));
    print_msg(format_args!("Coin | Available | Locked"));
    print_msg(format_args!("------------------------------------------------------"));
    for acc in accounts.into_iter() {
        print_msg(format_args!("{} | {} | {}", acc.currency, acc.available, acc.locked));
    }
    print_msg(format_args!("======================================================"));
    Ok(())
}

async fn sell_balance(client: Client, balance: ws::channels::SpotBalance, info: CurrencyPair) -> Result<()> {
    // no need to trade if we don't have fund
    if let Some(min_base) = info.min_base_amount {
        if balance.available < min_base {
            return Ok(());
        }
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
    let total = balance.available * price;
    if let Some(min_quote) = info.min_quote_amount {
        if total < min_quote {
            return Ok(());
        }
    }
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
    println!(
        "#{} Sell {} {} => {} {} @ {} {}",
        order.id, balance.available, &info.base, total, &info.quote, price, &info.quote
    );
    Ok(())
}

const HELP: &str = r#"The following keyboard shortcuts are available:
 - Hit "s" to sell current available balance
 - Hit "p" to print current balance
 - Hit "h" or "?" to print this help
 - Hit Esc or Ctrl-C to quit
"#;

fn print_help() {
    print_msg(format_args!("{}", HELP));
}

fn print_msg(msg: std::fmt::Arguments) {
    println!("{}\r", msg);
}

fn eprint_msg(msg: std::fmt::Arguments) {
    eprintln!("{}\r", msg);
}

mod terminal {
    use anyhow::Result;
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    pub struct RawModeGuard {
        _private: (),
    }

    impl RawModeGuard {
        pub fn new() -> Result<Self> {
            enable_raw_mode()?;
            Ok(Self { _private: () })
        }
    }

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            disable_raw_mode().expect("Unable to disable raw mode");
        }
    }
}
use crate::rest::SpotAccount;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent};
use terminal::RawModeGuard;

/// Automatically sell coins on gate.io.
///
/// This program will load `.env` file from its working directory.
#[derive(Debug, StructOpt)]
#[structopt(
    setting = AppSettings::UnifiedHelpMessage,
    setting = AppSettings::ColoredHelp,
)]
struct Cli {
    /// gate.io APIv4 key
    #[structopt(short, long, env)]
    key: String,
    /// gate.io APIv4 secret
    #[structopt(short, long, env, hide_env_values = true)]
    secret: String,
    /// currency pair source
    #[structopt(default_value = "ERG")]
    src_coin: String,
    /// currency pair destination
    #[structopt(default_value = "USDT")]
    dst_coin: String,
}

#[async_std::main]
async fn main() -> Result<()> {
    dotenv::dotenv()?;
    let cli = Cli::from_args();

    let mut trader = Trader::spawn(cli.key, cli.secret).await?;

    let _guard = trader.handle_keyboard().await?;

    trader.trade(cli.src_coin, cli.dst_coin).await?;

    trader.run().await?;

    Ok(())
}
