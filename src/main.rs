use hmac::{Hmac, Mac, NewMac};
use sha2::Sha512;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use surf::middleware::{Middleware, Next};
use surf::{Body, Client, Error as SurfError, Request, Response, Result as SurfResult, Url};
use hmac::digest::Digest;
use percent_encoding::percent_decode_str;
use std::borrow::Cow;

type HmacSha512 = Hmac<Sha512>;

struct GateIoAuth {
    key: String,
    secret: Vec<u8>,
}

impl GateIoAuth {
    pub fn new(key: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            secret: secret.into().into_bytes(),
        }
    }

    /// Adds gate.io APIv4 auth to the request
    /// Sign the request by adding KEY, Timestamp, SIGN headers
    pub async fn sign(&self, mut req: Request, timestamp: u64) -> Result<Request, SurfError> {
        // 1. KEY header
        req.set_header("KEY", &self.key);
        // 2. Timestamp header
        req.set_header("Timestamp", format!("{}", timestamp));
        // 3. SIGN header
        let sign = {
            let mut mac = HmacSha512::new_from_slice(&self.secret).expect("HMAC can take key of any size");

            // method, all upper case
            let method = req.method().as_ref().to_uppercase().into_bytes();
            mac.update(&method);
            mac.update(b"\n");

            // path
            let path = req.url().path().as_bytes();
            mac.update(path);
            mac.update(b"\n");

            // all query params, and must NOT be url-encoded, so we directly get it from the url
            let query = req.url().query().unwrap_or("");
            let query: Cow<[u8]> = percent_decode_str(query).into();
            mac.update(&query);
            mac.update(b"\n");

            let payload_hash = self.digest_payload(&mut req).await?.into_bytes();
            mac.update(&payload_hash);
            mac.update(b"\n");

            mac.update(timestamp.to_string().as_bytes());

            let sign = mac.finalize().into_bytes();
            hex::encode(sign)
        };
        req.set_header("SIGN", sign);

        Ok(req)
    }

    /// compute sha512 digest of the request payload
    async fn digest_payload(&self, req: &mut Request) -> Result<String, SurfError> {
        let (mime, payload) = {
            // we have to take the body to do the computation, don't forget to put the body back
            let body = req.take_body();
            (body.mime().clone(), body.into_bytes().await?)
        };
        // compute sha512 of the body
        let payload_hash = hex::encode(Sha512::digest(&payload));

        // set back the body
        let mut body = Body::from_bytes(payload);
        body.set_mime(mime);
        req.set_body(body);

        Ok(payload_hash)
    }
}

#[surf::utils::async_trait]
impl Middleware for GateIoAuth {
    async fn handle(&self, req: Request, client: Client, next: Next<'_>) -> SurfResult<Response> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let req = self.sign(req, timestamp).await?;
        next.run(req, client).await
    }
}

#[async_std::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut client = Client::new().with(GateIoAuth::new(
        "b8ffbcfa3e8eafc345ade75f84c4a490",
        "ced122bd871e07a142265503ff84118737341bc07717c784e9e4d167a2e25699",
    ));
    // client.set_base_url(Url::parse("https://api.gateio.ws/api/v4")?);
    client.set_base_url(Url::parse(
        "https://ensp73vvqpsjx8g.m.pipedream.net/api/v4",
    )?);
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

#[cfg(test)]
mod test {
    use super::*;
    use surf::http::Method;

    #[async_std::test]
    async fn sign_example1() {
        let key = "key";
        let timestamp = 1541993715;
        let auth = GateIoAuth::new(key, "secret");

        let req = Request::builder(
            Method::Get,
            Url::parse("https://api.gateio.ws/api/v4/futures/orders?contract=BTC_USD&status=finished&limit=50").unwrap(),
        )
        .build();
        let req = auth.sign(req, timestamp).await.unwrap();

        assert_eq!(req.header("KEY").unwrap(), key);
        assert_eq!(req.header("Timestamp").unwrap(), &timestamp.to_string());
        assert_eq!(
            req.header("SIGN").unwrap(),
            "55f84ea195d6fe57ce62464daaa7c3c02fa9d1dde954e4c898289c9a2407a3d6fb3faf24deff16790d726b66ac9f74526668b13bd01029199cc4fcc522418b8a"
        );
    }

    #[async_std::test]
    async fn sign_example2() {
        let key = "key";
        let timestamp = 1541993715;
        let auth = GateIoAuth::new(key, "secret");

        let req = Request::builder(
            Method::Post,
            Url::parse("https://api.gateio.ws/api/v4/futures/orders").unwrap(),
        )
            .body(r#"{"contract":"BTC_USD","type":"limit","size":100,"price":6800,"time_in_force":"gtc"}"#)
            .build();
        let req = auth.sign(req, timestamp).await.unwrap();

        assert_eq!(req.header("KEY").unwrap(), key);
        assert_eq!(req.header("Timestamp").unwrap(), &timestamp.to_string());
        assert_eq!(
            req.header("SIGN").unwrap(),
            "eae42da914a590ddf727473aff25fc87d50b64783941061f47a3fdb92742541fc4c2c14017581b4199a1418d54471c269c03a38d788d802e2c306c37636389f0"
        );
    }
}
