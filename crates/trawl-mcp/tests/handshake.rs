// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`TrawlServer`] over an in-memory pipe, with hand-written JSON-RPC
//! frames: a 2025-11-25 `initialize`, then `tools/list`, `tools/call`,
//! `resources/list` and `resources/read`.
//! The backend points at a port nothing listens on; no call here reaches
//! it.

use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use trawl_client::HttpClient;
use trawl_mcp::TrawlServer;

struct Client {
    writer: tokio::io::WriteHalf<DuplexStream>,
    lines: Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
}

impl Client {
    async fn send(&mut self, frame: Value) {
        let mut line = serde_json::to_vec(&frame).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn receive(&mut self) -> Value {
        let line = self
            .lines
            .next_line()
            .await
            .unwrap()
            .expect("a response frame");
        serde_json::from_str(&line).expect("every line is one JSON-RPC frame")
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let response = self.receive().await;
        assert_eq!(response["id"], json!(id), "{response}");
        response
    }
}

async fn start() -> Client {
    let (client, server) = tokio::io::duplex(1 << 20);
    let backend = HttpClient::new("http://127.0.0.1:9", "unused").unwrap();
    let (read, write) = tokio::io::split(server);
    tokio::spawn(async move {
        let running = TrawlServer::new(backend)
            .serve((read, write))
            .await
            .unwrap();
        running.waiting().await.unwrap();
    });
    let (read, writer) = tokio::io::split(client);
    let mut client = Client {
        writer,
        lines: BufReader::new(read).lines(),
    };
    let init = client
        .request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "handshake-test", "version": "0"}
            }),
        )
        .await;
    let result = &init["result"];
    assert_eq!(result["protocolVersion"], json!("2025-11-25"), "{init}");
    assert_eq!(result["serverInfo"]["name"], json!("trawl"));
    assert!(result["capabilities"]["tools"].is_object(), "{init}");
    assert!(result["instructions"].as_str().unwrap().contains("orient"));
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    client
}

#[tokio::test]
async fn tools_list_sends_the_fixed_table_with_private_cache_hints() {
    let mut client = start().await;
    let response = client.request(2, "tools/list", json!({})).await;
    let result = &response["result"];
    let names: Vec<_> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "orient",
            "search",
            "read",
            "context",
            "sightings",
            "compare",
            "fields",
            "investigation"
        ]
    );
    assert_eq!(result["cacheScope"], json!("private"));
    assert_eq!(result["ttlMs"], json!(trawl_mcp::server::TOOLS_TTL_MS));
    assert_eq!(
        result["tools"][1]["annotations"]["readOnlyHint"],
        json!(false)
    );
    assert_eq!(
        result["tools"][0]["annotations"]["readOnlyHint"],
        json!(true)
    );
    assert!(result["tools"][1]["outputSchema"].is_object());
}

#[tokio::test]
async fn a_known_tool_answers_a_tool_error_and_an_unknown_one_a_protocol_error() {
    let mut client = start().await;
    let stub = client
        .request(2, "tools/call", json!({"name": "orient", "arguments": {}}))
        .await;
    assert_eq!(stub["result"]["isError"], json!(true), "{stub}");

    let bad = client
        .request(
            3,
            "tools/call",
            json!({"name": "search", "arguments": {"mode": "run"}}),
        )
        .await;
    assert_eq!(bad["result"]["isError"], json!(true), "{bad}");
    let text = bad["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("invalid arguments for search"), "{text}");

    let unknown = client
        .request(
            4,
            "tools/call",
            json!({"name": "ask_trawl", "arguments": {}}),
        )
        .await;
    assert_eq!(unknown["error"]["code"], json!(-32602), "{unknown}");
}

#[tokio::test]
async fn the_dsl_reference_lists_and_reads_as_markdown_sections() {
    let mut client = start().await;
    let listed = client.request(2, "resources/list", json!({})).await;
    let resources = listed["result"]["resources"].as_array().unwrap();
    assert_eq!(
        resources[0]["uri"],
        json!("trawl://dsl/overview"),
        "{listed}"
    );
    assert!(
        resources
            .iter()
            .all(|r| r["mimeType"] == json!("text/markdown"))
    );
    let uris: Vec<_> = resources
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"trawl://dsl/search-stage"), "{uris:?}");

    let read = client
        .request(3, "resources/read", json!({"uri": "trawl://dsl/examples"}))
        .await;
    let contents = &read["result"]["contents"][0];
    assert_eq!(contents["uri"], json!("trawl://dsl/examples"), "{read}");
    assert!(
        contents["text"]
            .as_str()
            .unwrap()
            .starts_with("## Examples")
    );

    let missing = client
        .request(4, "resources/read", json!({"uri": "trawl://dsl/nope"}))
        .await;
    assert!(missing["error"]["code"].is_i64(), "{missing}");
}
