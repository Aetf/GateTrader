use rust_decimal::Decimal;

#[derive(Debug, Eq, PartialEq, serde::Deserialize)]
pub struct SpotBalance {
    #[serde(deserialize_with = "crate::utils::de::number_from_string")]
    pub timestamp: u64,
    pub currency: String,
    pub change: Decimal,
    pub total: Decimal,
    pub available: Decimal,
}

#[cfg(test)]
mod test {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn spot_balance_de() {
        const JSON: &str = "[{\"timestamp\":\"1621262774\",\"timestamp_ms\":\"1621262774038\",\"user\":\"2851747\",\"currency\":\"ERG\",\"change\":\"1.010939090000\",\"total\":\"1.01174993000000000000\",\"available\":\"1.01174993000000000000\"}]";
        let balances: Vec<SpotBalance> = serde_json::from_str(JSON).unwrap();
        assert_eq!(
            balances,
            vec![SpotBalance {
                timestamp: 1621262774,
                currency: "ERG".into(),
                change: Decimal::from_str("1.010939090000").unwrap(),
                total: Decimal::from_str("1.01174993000000000000").unwrap(),
                available: Decimal::from_str("1.01174993000000000000").unwrap(),
            }]
        )
    }
}
