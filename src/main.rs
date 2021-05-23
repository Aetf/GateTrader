#![feature(try_blocks)]
#![deny(unused_must_use)]

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_std::task;
use async_std::task::JoinHandle;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures_util::lock::Mutex;
use futures_util::stream::{self, StreamExt as _};
use futures_util::TryStreamExt;
use rust_decimal::Decimal;
use structopt::clap::AppSettings;
use structopt::StructOpt;
use surf::{Body, Client, Url};

use crate::auth::GateIoAuth;
use crate::rest::{CurrencyPair, Order, SpotAccount, SpotOrderParams, SpotTicker, SpotTickersParams};
use crate::terminal::RawModeGuard;
use crate::ws::channels::SpotBalance;
use crate::ws::WsClient;

mod auth;
mod rest;
mod utils;
mod ws;

#[derive(Debug)]
struct Trader {
    ws_client: WsClient,
    rest_client: Client,
    trading_pairs: Arc<Mutex<Vec<(String, String)>>>,
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

        let ws_client = WsClient::new(key, secret);

        Ok(Self {
            rest_client: client,
            ws_client,
            trading_pairs: Arc::new(Mutex::new(vec![])),
        })
    }

    pub async fn handle_keyboard(&mut self) -> Result<(RawModeGuard, JoinHandle<()>)> {
        let guard = RawModeGuard::new()?;
        let mut reader = EventStream::new();
        print_help();

        let client = self.rest_client.clone();
        let pairs = self.trading_pairs.clone();

        let fut = async move {
            while let Some(e) = reader.next().await {
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
                            print_balance(client.clone()).await?;
                        }
                        Event::Key(KeyEvent {
                            code: KeyCode::Char('s'),
                            ..
                        }) => {
                            sell_all_pairs(client.clone(), pairs.clone()).await?;
                        }
                        Event::Key(
                            KeyEvent { code: KeyCode::Esc, .. }
                            | KeyEvent {
                                code: KeyCode::Char('c'),
                                modifiers: KeyModifiers::CONTROL,
                            },
                        ) => {
                            break;
                        }
                        _ => (),
                    }
                };
                if let Err(e) = res {
                    eprint_msg(format_args!("{:?}", e));
                }
            }
        };
        let handle = task::spawn(fut);
        Ok((guard, handle))
    }

    /// monitor the spot balance of `src` coin, and sell them out as `dst` coin
    /// as soon as there are nonzero balance
    pub async fn trade(&mut self, src: impl Into<String>, dst: impl Into<String>) -> Result<()> {
        let src = src.into();
        let dst = dst.into();
        self.trading_pairs.lock().await.push((src.clone(), dst.clone()));

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
            .ws_client
            .subscribe("spot.balances", "update")
            .map(|msg| msg.content.map_err(|e| anyhow!("Got server error: {:#?}", e)))
            .try_filter_map(move |val| {
                // each async block needs its own copy, because they may run concurrently
                // todo: revisit this, may not need clone
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
            .and_then(move |balance| sell(rest_client.clone(), balance.available, info.clone()))
            .for_each_concurrent(None, |res| async {
                if let Err(e) = res {
                    eprint_msg(format_args!("{:?}", e));
                }
            });
        task::spawn(fut);
        // show the subscribe response only when it's error
        let fut = self
            .ws_client
            .subscribe("spot.balances", "subscribe")
            .map(|msg| msg.content.map_err(|e| anyhow!("Got server error: {:#?}", e)))
            .for_each_concurrent(None, |res| async {
                match res {
                    Err(e) => eprint_msg(format_args!("{:?}", e)),
                    Ok(..) => print_msg(format_args!("[OK] spot.balances subscribed")),
                }
            });
        task::spawn(fut);

        Ok(())
    }

    pub async fn run(self) -> Result<()> {
        self.ws_client.run_forever().await?;
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
    print_msg(format_args!("Coin \t| Available \t| Locked"));
    print_msg(format_args!("------------------------------------------------------"));
    for acc in accounts.into_iter() {
        print_msg(format_args!(
            "{} \t| {:.8} \t| {:.8}",
            acc.currency, acc.available, acc.locked
        ));
    }
    print_msg(format_args!("======================================================"));
    Ok(())
}

async fn sell_all_pairs(client: Client, pairs: Arc<Mutex<Vec<(String, String)>>>) -> Result<()> {
    // these are what we have
    let accounts: Vec<SpotAccount> = client
        .get("spot/accounts")
        .await
        .map_err(|e| anyhow!(e))?
        .body_json()
        .await
        .map_err(|e| anyhow!(e))?;

    let pairs: Vec<_> = {
        let pairs = pairs.lock().await;
        // cross reference the pairs with funds we have
        // we format the url as owned String so we can drop the lock early
        accounts
            .into_iter()
            .filter_map(|acc| {
                pairs
                    .iter()
                    .find(|(src, _)| src == &acc.currency && acc.available > 0.into())
                    .map(|(src, dst)| (format!("spot/currency_pairs/{}_{}", src, dst), acc.available))
            })
            .collect()
    };
    // for each viable pair, pull latest info
    stream::iter(pairs.into_iter())
        .then(|(url, amount)| {
            let client = client.clone();
            async move {
                let res: Result<_> = try {
                    let info: rest::CurrencyPair = client
                        .get(url)
                        .await
                        .map_err(|e| anyhow!(e))?
                        .body_json()
                        .await
                        .map_err(|e| anyhow!(e))?;
                    (amount, info)
                };
                res
            }
        })
        .try_for_each(|(amount, info)| sell(client.clone(), amount, info))
        .await?;

    Ok(())
}

async fn sell(client: Client, amount: Decimal, info: CurrencyPair) -> Result<()> {
    // no need to trade if we don't have fund
    if amount <= 0.into() {
        return Ok(());
    }
    if let Some(min_base) = info.min_base_amount {
        if amount < min_base {
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
    let total = amount * price;
    if let Some(min_quote) = info.min_quote_amount {
        if total < min_quote {
            return Ok(());
        }
    }
    let order: Order = client
        .post("spot/orders")
        .body(
            Body::from_json(&SpotOrderParams {
                text: None,
                currency_pair: &info.id,
                order_type: Some("limit".into()),
                account: Some("spot".into()),
                side: "sell".into(),
                amount,
                price,
                time_in_force: None,
                iceberg: None,
                auto_borrow: None,
            })
            .map_err(|e| anyhow!(e))?,
        )
        .await
        .map_err(|e| anyhow!(e))?
        .body_json()
        .await
        .map_err(|e| anyhow!(e))?;
    println!(
        "#{} Sell {} {} => {} {} @ {} {}",
        order.id.as_deref().unwrap_or("?"),
        amount,
        &info.base,
        total,
        &info.quote,
        price,
        &info.quote
    );
    Ok(())
}

fn print_help() {
    print_msg(format_args!("{}", "The following keyboard shortcuts are available:"));
    print_msg(format_args!("{}", r#"  - Hit "p" to print current balance"#));
    print_msg(format_args!(
        "{}",
        r#"  - Hit "s" to sell all available balance for trading coins"#
    ));
    print_msg(format_args!("{}", r#"  - Hit "h" or "?" to print this help"#));
    print_msg(format_args!("{}", r#"  - Hit Esc or Ctrl-C to quit"#));
}

fn print_msg(msg: std::fmt::Arguments) {
    // canonical carriage return and line feed
    let msg = format!("{}", msg).replace('\n', "\r\n");
    print!("{}\r\n", msg);
}

fn eprint_msg(msg: std::fmt::Arguments) {
    let msg = format!("{}", msg).replace('\n', "\r\n");
    eprint!("{}\r\n", msg);
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

    let (_guard, input) = trader.handle_keyboard().await?;

    trader.trade(cli.src_coin, cli.dst_coin).await?;

    task::spawn(trader.run());

    input.await;

    print_msg(format_args!("[OK] Bye"));

    Ok(())
}
