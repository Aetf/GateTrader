use std::time;

use rust_decimal::Decimal;
use surf::middleware::{Middleware, Next};
use surf::{Client, Request, Response, Result as SurfResult};

pub struct Logger;

#[surf::utils::async_trait]
impl Middleware for Logger {
    async fn handle(&self, req: Request, client: Client, next: Next<'_>) -> SurfResult<Response> {
        let start_time = time::Instant::now();
        let uri = format!("{}", req.url());
        let method = format!("{}", req.method());
        println!("sending request: {} {}", method, uri);

        let res = next.run(req, client).await?;

        let status = res.status();
        let elapsed = start_time.elapsed();

        println!("request completed: {} {}s", status, elapsed.as_secs_f64());

        Ok(res)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct CurrencyPair {
    pub id: String,
    pub base: String,
    pub quote: String,
    pub fee: Option<Decimal>,
    pub min_base_amount: Option<Decimal>,
    pub min_quote_amount: Option<Decimal>,
    pub amount_precision: Option<u64>,
    pub precision: Option<u64>,
    pub trade_status: Option<String>,
    pub sell_start: Option<u64>,
    pub buy_start: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
pub struct SpotTicker {
    pub currency_pair: String,
    pub last: Decimal,
    pub lowest_ask: Decimal,
    pub highest_bid: Decimal,
    pub change_percentage: Decimal,
    pub base_volume: Decimal,
    pub quote_volume: Decimal,
    pub high_24h: Decimal,
    pub low_24h: Decimal,
    pub etf_net_value: Decimal,
    pub etf_pre_net_value: Decimal,
    pub etf_pre_timestamp: u64,
    pub etf_leverage: Decimal,
}

#[derive(Debug, serde::Serialize)]
pub struct SpotTickersParams<'a> {
    pub currency_pair: &'a str,
}

#[derive(Debug, serde::Serialize)]
pub struct SpotOrderParams<'a> {
    pub text: Option<String>,
    pub currency_pair: &'a str,
    #[serde(rename = "type")]
    pub order_type: Option<String>,
    pub account: Option<String>,
    pub side: String,
    pub amount: Decimal,
    pub price: Decimal,
    pub time_in_force: Option<String>,
    pub iceberg: Option<String>,
    pub auto_borrow: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
pub struct Order {
    pub id: String,
    pub text: Option<String>,
    #[serde(deserialize_with = "crate::utils::de::number_from_string")]
    pub create_time: u64,
    #[serde(deserialize_with = "crate::utils::de::number_from_string")]
    pub update_time: u64,
    pub status: String,
    pub currency_pair: String,
    #[serde(rename = "type")]
    pub order_type: Option<String>,
    pub account: Option<String>,
    pub side: String,
    pub amount: Decimal,
    pub price: Decimal,
    pub time_in_force: Option<String>,
    pub iceberg: Option<String>,
    pub auto_borrow: Option<bool>,
    pub left: Decimal,
    pub filled_total: Decimal,
    pub fee: Decimal,
    pub fee_currency: String,
    pub point_fee: Decimal,
    pub gt_fee: Decimal,
    pub gt_discount: bool,
    pub rebated_fee: Decimal,
    pub rebated_fee_currency: String,
}
