//! D9 — install path + read tool + dry-run gate, exercised against mockito.
//!
//! One test, one install. We can't split this into multiple tests because
//! `swarms_tetrac::install` writes to a process-global OnceLock; the second
//! call would return `AlreadyInstalled` and tokio runs tests in parallel.

use serde_json::Value;
use swarms_rs::structs::tool::ToolDyn;
use swarms_tetrac::TtcConfig;
use swarms_tetrac::tools::{GetFundingRatesTool, PlaceMarketOrderTool};

#[tokio::test]
async fn install_routes_reads_through_base_url_and_dry_run_blocks_writes() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("GET", "/markets/funding-rates")
        .match_query(mockito::Matcher::UrlEncoded(
            "symbol".into(),
            "BTCUSDT".into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"success":true,"data":[{"exchange":"mock","symbol":"BTCUSDT","fundingRate":0.001,"timestamp":1234567890}]}"#,
        )
        .create_async()
        .await;

    let cfg = TtcConfig {
        auth_token: "fake-token".into(),
        public_key: "fake-public".into(),
        base_url: server.url(),
        default_exchange: None,
        dry_run: true,
        max_loops_per_minute: 60,
    };
    swarms_tetrac::install(&cfg).expect("install");

    // 1. Read tool → mockito serves, data parsed.
    let read_result = GetFundingRatesTool
        .call(r#"{"symbol":"BTCUSDT"}"#.into())
        .await
        .expect("get_funding_rates ok");
    let read_value: Value = serde_json::from_str(&read_result).expect("read json");
    assert_eq!(read_value[0]["exchange"], "mock");
    assert_eq!(read_value[0]["fundingRate"], 0.001);
    mock.assert_async().await;

    // 2. Mutating tool → dry-run envelope. No mock for /exchanges, so a real
    //    network call would 501 against mockito. Reaching this assertion at
    //    all proves dry_run blocked the call.
    let write_result = PlaceMarketOrderTool
        .call(
            r#"{"exchange":"orderly","symbol":"BTC-USDT","side":"buy","quantity":0.001}"#.into(),
        )
        .await
        .expect("place_market_order ok");
    let write_value: Value = serde_json::from_str(&write_result).expect("write json");
    assert_eq!(write_value["dry_run"], true);
    assert_eq!(write_value["action"], "place_market_order");
    assert_eq!(write_value["args"]["exchange"], "orderly");
    assert!(
        write_value["note"]
            .as_str()
            .unwrap()
            .contains("TTC_DRY_RUN"),
        "note must point at the env flip: {}",
        write_value["note"]
    );
}
