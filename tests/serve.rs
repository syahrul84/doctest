//! The built binary speaks MCP on stdio.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn serve_answers_initialize_and_tools_list() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_doctest"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#
    )
    .unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    let mut a = String::new();
    out.read_line(&mut a).unwrap();
    let mut b = String::new();
    out.read_line(&mut b).unwrap();
    drop(stdin);
    let _ = child.wait();
    let a: serde_json::Value = serde_json::from_str(&a).unwrap();
    let b: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(a["id"], 1);
    assert_eq!(a["result"]["serverInfo"]["name"], "doctest");
    assert_eq!(b["id"], 2);
    assert_eq!(b["result"]["tools"].as_array().unwrap().len(), 18);
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let t = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &t)
        } else {
            std::fs::copy(&p, &t).unwrap();
        }
    }
}

#[test]
fn two_tool_calls_both_answered_by_the_worker_pool() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let d = tempfile::tempdir().unwrap();
    let shop = d.path().join("shop");
    copy_dir(&root.join("tests/fixtures/shop"), &shop);
    std::fs::write(
        shop.join(".doctest.toml"),
        "[project]\nname=\"shop\"\nfrontends=[\"web\"]\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_doctest"))
        .args(["serve", "--workers", "2"])
        .env("DOCTEST_CODEMAP_FIXTURES", root.join("tests/fixtures/codemap"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    for id in [3, 4] {
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "issues", "arguments": {"project": shop, "scope": "all"}}});
        writeln!(stdin, "{msg}").unwrap();
    }
    let mut ids = Vec::new();
    while ids.len() < 2 {
        let mut l = String::new();
        if out.read_line(&mut l).unwrap() == 0 {
            break;
        }
        let v: serde_json::Value = serde_json::from_str(&l).unwrap();
        if let Some(id) = v["id"].as_i64() {
            assert!(
                v["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("points to fix"),
                "{v}"
            );
            ids.push(id);
        }
    }
    drop(stdin);
    let _ = child.wait();
    ids.sort();
    assert_eq!(ids, vec![3, 4]);
}
