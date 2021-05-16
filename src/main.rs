use std::collections::HashMap;

use surf::{Client, Url};

use auth::GateIoAuth;

mod auth;

#[async_std::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut client = Client::new().with(GateIoAuth::new(
        "b8ffbcfa3e8eafc345ade75f84c4a490",
        "ced122bd871e07a142265503ff84118737341bc07717c784e9e4d167a2e25699",
    ));
    // client.set_base_url(Url::parse("https://api.gateio.ws/api/v4")?);
    client.set_base_url(Url::parse("https://ensp73vvqpsjx8g.m.pipedream.net/api/v4")?);
    let resp = client
        .get("/futures/orders")
        .await
        .map_err(|e| e.into_inner())?
        .body_json::<HashMap<String, String>>()
        .await
        .map_err(|e| e.into_inner())?;
    println!("{:#?}", resp);
    Ok(())
}
