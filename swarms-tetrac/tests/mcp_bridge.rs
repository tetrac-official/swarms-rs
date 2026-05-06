//! Integration test for the MCP STDIO bridge binary.
//!
//! Spawns the bridge, drives a JSON-RPC handshake on its stdin, and asserts
//! it returns exactly the 19 tools we expect on tools/list.
//!
//! TTC_AUTH_TOKEN / TTC_PUBLIC_KEY are dummy values — install() only checks
//! they're Some, doesn't validate against a server. We never call tools/call
//! here, so no network is touched.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_swarms-tetrac-mcp-bridge");

#[test]
fn bridge_lists_all_19_tools() {
    let mut child = Command::new(BIN)
        .env("TTC_AUTH_TOKEN", "test-token")
        .env("TTC_PUBLIC_KEY", "test-public-key")
        .env("TTC_DRY_RUN", "true")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bridge binary");

    let stdin = child.stdin.as_mut().expect("stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"0"}}}}}}"#
    )
    .unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut init_line = String::new();
    reader.read_line(&mut init_line).expect("read init response");
    let init: Value = serde_json::from_str(init_line.trim()).expect("init json");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "swarms-tetrac");
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

    let mut list_line = String::new();
    reader.read_line(&mut list_line).expect("read list response");
    let list: Value = serde_json::from_str(list_line.trim()).expect("list json");
    assert_eq!(list["id"], 2);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 19, "expected 19 tools, got {}", tools.len());

    // Sanity-check a couple of names that must match ttc.box/api/v1/mcp.
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for required in [
        "get_tickers",
        "place_market_order",
        "cancel_order",
        "set_leverage",
    ] {
        assert!(
            names.contains(&required),
            "missing required tool {required}; got {names:?}"
        );
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn bridge_responds_to_unknown_method_with_error() {
    let mut child = Command::new(BIN)
        .env("TTC_AUTH_TOKEN", "test-token")
        .env("TTC_PUBLIC_KEY", "test-public-key")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bridge binary");

    let stdin = child.stdin.as_mut().expect("stdin");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":7,"method":"does/not/exist"}}"#).unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    let resp: Value = serde_json::from_str(line.trim()).expect("json");
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["error"]["code"], -32601);

    let _ = child.kill();
    let _ = child.wait();
}
