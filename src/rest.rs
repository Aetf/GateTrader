use std::time;

use rust_decimal::Decimal;
use surf::middleware::{Middleware, Next};
use surf::{Client, Request, Response, Result as SurfResult, Url};

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

/// Set baseurl for all requests
pub struct BaseUrl(Url);

impl BaseUrl {
    pub fn new(url: Url) -> Self {
        Self(url)
    }
}

#[surf::utils::async_trait]
impl Middleware for BaseUrl {
    async fn handle(&self, mut req: Request, client: Client, next: Next<'_>) -> SurfResult<Response> {
        // get mutable inner http::Request to modify the url
        {
            let req: &mut surf::http::Request = req.as_mut();
            *req.url_mut() = self.0.join(req.url().as_ref())?;
        }

        let res = next.run(req, client).await?;
        Ok(res)
    }
}

#[derive(Debug, Eq, PartialEq, serde::Deserialize)]
pub struct CurrencyPair {
    pub id: String,
    pub base: String,
    pub quote: String,
    pub fee: String,
    pub min_base_amount: Decimal,
    pub min_quote_amount: Decimal,
    pub amount_precision: u64,
    pub precision: u64,
    pub trade_status: String,
    pub sell_start: u64,
    pub buy_start: u64,
}
