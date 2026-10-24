// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The MCP server is a client (ADR-0058): trawl-server must never be in
//! trawl-mcp's dependency graph, or a tool could reach the pool around the
//! API's guards.

use std::collections::{BTreeSet, HashMap};
use std::process::Command;

use serde_json::Value;

/// Walk the resolved graph from trawl-mcp through normal (non-dev,
/// non-build) dependency edges, for the host platform's targets.
#[test]
fn trawl_server_is_not_a_normal_dependency() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--offline", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");

    let names: HashMap<&str, &str> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p["name"].as_str().unwrap()))
        .collect();
    let nodes: HashMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve nodes")
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n))
        .collect();
    let root = names
        .iter()
        .find(|(_, name)| **name == "trawl-mcp")
        .map(|(id, _)| *id)
        .expect("trawl-mcp is a workspace package");

    let mut seen = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        for dep in nodes[id]["deps"].as_array().expect("deps") {
            let normal = dep["dep_kinds"]
                .as_array()
                .expect("dep_kinds")
                .iter()
                .any(|kind| kind["kind"].is_null());
            if normal {
                stack.push(dep["pkg"].as_str().unwrap());
            }
        }
    }

    let reached: BTreeSet<&str> = seen.iter().map(|id| names[id]).collect();
    assert!(
        reached.contains("trawl-client"),
        "the walk reaches trawl-client"
    );
    assert!(
        !reached.contains("trawl-server"),
        "trawl-mcp must not depend on trawl-server"
    );
}
