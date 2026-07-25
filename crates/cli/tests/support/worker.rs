//! The in-memory Devspace Worker the fake CLI suites push and sync against.
//!
//! It dispatches on the request line, so it stays close to the routes in
//! `src/router.ts` without needing the real Worker.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use devspace_testutils::fake_worker::{create_server, read_http_request, respond};

use super::{request_body, request_json};

pub fn create_push_server(git_url: String) -> (String, JoinHandle<Vec<String>>) {
    let mut head = None::<String>;
    let mut head_cursor = 0_u64;
    let mut op_objects = BTreeMap::<String, String>::new();
    let mut activation_cursor = 0_u64;
    let mut cursors = Vec::<serde_json::Value>::new();
    let mut mappings = Vec::<serde_json::Value>::new();
    let mut pending = None::<serde_json::Value>;
    let mut pending_fence = 1_u64;
    let mut remotes = BTreeMap::<String, String>::new();
    create_server(move |_, request, stream| {
        let request_line = request.lines().next().unwrap();
        if request_line.starts_with("PUT ") && request_line.contains("/remotes/") {
            assert_eq!(request_json(request)["url"], git_url);
            let name = remote_name_from_request(request_line);
            remotes.insert(name.clone(), git_url.clone());
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"remote": {"name": name, "url": git_url}}).to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/packs?") {
            respond(
                stream,
                "200 OK",
                r#"{"packs":[],"nextAfter":0,"through":0,"hasMore":false}"#,
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/objects/inventory ")
        {
            respond(stream, "200 OK", r#"{"keys":[]}"#);
        } else if request_line.starts_with("PUT ") && request_line.contains("/packs/") {
            respond(stream, "200 OK", r#"{"inserted":true,"installed":false}"#);
        } else if request_line.starts_with("POST ")
            && request_line.contains("/packs/")
            && request_line.contains("/install ")
        {
            respond(
                stream,
                "200 OK",
                r#"{"installed":true,"insertedObjects":1}"#,
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/ops/heads ") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "cursor": head_cursor,
                    "heads": head.iter().collect::<Vec<_>>(),
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/git/ops/inventory ")
        {
            let requested = request_json(request)["keys"].as_array().unwrap().clone();
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "keys": requested.into_iter().filter(|key| {
                        op_objects.contains_key(key.as_str().unwrap())
                    }).collect::<Vec<_>>()
                })
                .to_string(),
            );
        } else if request_line.starts_with("PUT ")
            && (request_line.contains("/git/ops/views/")
                || request_line.contains("/git/ops/operations/"))
        {
            let path = request_line.split_whitespace().nth(1).unwrap();
            let (kind, id) = if let Some(id) = path.split("/git/ops/views/").nth(1) {
                ("v", id)
            } else {
                ("o", path.split("/git/ops/operations/").nth(1).unwrap())
            };
            op_objects.insert(format!("{kind}:{id}"), request_body(request).to_owned());
            respond(stream, "200 OK", r#"{}"#);
        } else if request_line.starts_with("GET ")
            && (request_line.contains("/git/ops/views/")
                || request_line.contains("/git/ops/operations/"))
        {
            let path = request_line.split_whitespace().nth(1).unwrap();
            let (kind, id) = if let Some(id) = path.split("/git/ops/views/").nth(1) {
                ("v", id)
            } else {
                ("o", path.split("/git/ops/operations/").nth(1).unwrap())
            };
            respond(
                stream,
                "200 OK",
                op_objects
                    .get(&format!("{kind}:{id}"))
                    .expect("requested operation object exists"),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/ops/heads/transactions ")
        {
            let body = request_json(request);
            head = Some(body["newHead"].as_str().unwrap().to_owned());
            head_cursor += 1;
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "cursor": head_cursor,
                    "heads": head.iter().collect::<Vec<_>>(),
                })
                .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "remotes": remotes
                        .iter()
                        .map(|(name, url)| serde_json::json!({"name": name, "url": url}))
                        .collect::<Vec<_>>()
                })
                .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
            let pending_batches = pending
                .iter()
                .map(|batch| {
                    serde_json::json!({
                        "batchId": batch["batchId"],
                        "remote": batch["remote"],
                        "ownerMachine": batch["machineId"],
                        "fence": pending_fence,
                        "refs": batch["updates"].as_array().unwrap().iter().map(|update| {
                            let proposed = update["proposedState"]
                                .as_u64()
                                .map(|index| update["states"][index as usize]["publicOid"].clone())
                                .or_else(|| update["identityOid"].as_str().map(serde_json::Value::from))
                                .unwrap_or(serde_json::Value::Null);
                            serde_json::json!({
                                "bookmark": update["bookmark"],
                                "expectedOldOid": update["expectedOldOid"],
                                "proposedPublicOid": proposed,
                                "identityOid": update["identityOid"],
                            })
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>();
            let next_after = mappings
                .last()
                .and_then(|mapping| mapping["activationSequence"].as_u64())
                .unwrap_or(0);
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "activationCursor": activation_cursor,
                    "cursors": cursors,
                    "mappings": mappings,
                    "nextAfter": next_after,
                    "through": activation_cursor,
                    "hasMore": false,
                    "pending": pending_batches,
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/fetches ")
        {
            let body = request_json(request);
            let remote = body["remote"].as_str().unwrap();
            for fetch_ref in body["refs"].as_array().unwrap() {
                let bookmark = fetch_ref["bookmark"].as_str().unwrap();
                let current = cursors
                    .iter()
                    .find(|cursor| cursor["remote"] == remote && cursor["bookmark"] == bookmark)
                    .map(|cursor| cursor["publicOid"].clone())
                    .unwrap_or(serde_json::Value::Null);
                assert_eq!(fetch_ref["expectedCursorOid"], current);
                cursors
                    .retain(|cursor| cursor["remote"] != remote || cursor["bookmark"] != bookmark);
                for state in fetch_ref["states"].as_array().unwrap() {
                    activation_cursor += 1;
                    let mapping = serde_json::json!({
                        "remote": remote,
                        "bookmark": bookmark,
                        "canonicalOid": state["canonicalOid"],
                        "publicOid": state["publicOid"],
                        "hiddenSetId": state["hiddenSetId"],
                        "activationSequence": activation_cursor,
                    });
                    if !mappings.iter().any(|existing| existing == &mapping) {
                        mappings.push(mapping);
                    }
                }
                let state = fetch_ref["proposedState"]
                    .as_u64()
                    .map(|index| fetch_ref["states"][index as usize].clone())
                    .or_else(|| {
                        fetch_ref["identityOid"].as_str().map(|oid| {
                            serde_json::json!({
                                "canonicalOid": oid,
                                "publicOid": oid,
                                "hiddenSetId": null,
                            })
                        })
                    })
                    .expect("fetch records a state or identity cursor");
                cursors.push(serde_json::json!({
                    "remote": remote,
                    "bookmark": bookmark,
                    "canonicalOid": state["canonicalOid"],
                    "publicOid": state["publicOid"],
                    "hiddenSetId": state["hiddenSetId"],
                    "activationSequence": activation_cursor,
                }));
            }
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "fetchId": body["fetchId"],
                    "activationCursor": activation_cursor,
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes/")
            && request_line.contains("/claim ")
        {
            pending_fence += 1;
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "pending": true,
                    "fence": pending_fence,
                })
                .to_string(),
            );
        } else if request_line.starts_with("GET ")
            && request_line.contains("/git/projection/pushes/")
            && request_line.contains("/replay?")
        {
            let batch = pending.as_ref().expect("replay follows a pending batch");
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "batchId": batch["batchId"],
                    "remote": batch["remote"],
                    "ownerMachine": batch["machineId"],
                    "fence": pending_fence,
                    "updates": batch["updates"],
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes/")
            && request_line.contains("/recover ")
        {
            let body = request_json(request);
            let batch = pending.take().expect("recover follows begin");
            let remote = batch["remote"].as_str().unwrap();
            for update in batch["updates"].as_array().unwrap() {
                let bookmark = update["bookmark"].as_str().unwrap();
                let observation = body["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|observation| observation["bookmark"] == bookmark)
                    .unwrap();
                cursors
                    .retain(|cursor| cursor["remote"] != remote || cursor["bookmark"] != bookmark);
                let mut state_sequences = Vec::new();
                for state in update["states"].as_array().unwrap() {
                    activation_cursor += 1;
                    state_sequences.push(activation_cursor);
                    mappings.push(serde_json::json!({
                        "remote": remote,
                        "bookmark": bookmark,
                        "canonicalOid": state["canonicalOid"],
                        "publicOid": state["publicOid"],
                        "hiddenSetId": state["hiddenSetId"],
                        "activationSequence": activation_cursor,
                    }));
                }
                if let Some(index) = update["proposedState"].as_u64() {
                    let state = &update["states"][index as usize];
                    assert_eq!(observation["liveOid"], state["publicOid"]);
                    cursors.push(serde_json::json!({
                        "remote": remote,
                        "bookmark": bookmark,
                        "canonicalOid": state["canonicalOid"],
                        "publicOid": state["publicOid"],
                        "hiddenSetId": state["hiddenSetId"],
                        "activationSequence": state_sequences[index as usize],
                    }));
                } else if let Some(identity_oid) = update["identityOid"].as_str() {
                    assert_eq!(observation["liveOid"], identity_oid);
                    activation_cursor += 1;
                    cursors.push(serde_json::json!({
                        "remote": remote,
                        "bookmark": bookmark,
                        "canonicalOid": identity_oid,
                        "publicOid": identity_oid,
                        "hiddenSetId": null,
                        "activationSequence": activation_cursor,
                    }));
                } else {
                    assert!(observation["liveOid"].is_null());
                }
            }
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "pending": false,
                    "fence": pending_fence,
                    "outcome": "accepted",
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes ")
        {
            let body = request_json(request);
            for update in body["updates"].as_array().unwrap() {
                let bookmark = &update["bookmark"];
                let current = cursors
                    .iter()
                    .find(|cursor| {
                        cursor["remote"] == body["remote"] && cursor["bookmark"] == *bookmark
                    })
                    .map(|cursor| cursor["publicOid"].clone())
                    .unwrap_or(serde_json::Value::Null);
                assert_eq!(update["expectedOldOid"], current);
            }
            pending = Some(body);
            pending_fence = 1;
            respond(
                stream,
                "200 OK",
                r#"{"pending":true,"fence":1,"outcome":null}"#,
            );
        } else {
            panic!("unexpected fake push request: {request_line}");
        }
        false
    })
}

fn remote_name_from_request(request_line: &str) -> String {
    request_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .split("/remotes/")
        .nth(1)
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_owned()
}

pub fn cloud_paused_at_remote_list() -> (String, Receiver<()>, SyncSender<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let (push_reached_tx, push_reached_rx) = sync_channel(0);
    let (release_push_tx, release_push_rx) = sync_channel(0);
    let server = thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let request_line = request.lines().next().unwrap();
            if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
                push_reached_tx.send(()).unwrap();
                release_push_rx.recv().unwrap();
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"activationCursor":0,"cursors":[],"mappings":[],"nextAfter":0,"through":0,"hasMore":false,"pending":[]}"#,
                );
                continue;
            }
            if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
                respond(&mut stream, "200 OK", r#"{"remotes":[]}"#);
                return;
            }
            if request_line.starts_with("GET ") && request_line.contains("/packs?") {
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"packs":[],"nextAfter":0,"through":0,"hasMore":false}"#,
                );
            } else if request_line.starts_with("POST ")
                && request_line.contains("/git/objects/inventory ")
            {
                respond(&mut stream, "200 OK", r#"{"keys":[]}"#);
            } else if request_line.starts_with("PUT ") && request_line.contains("/packs/") {
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"inserted":true,"installed":false}"#,
                );
            } else if request_line.starts_with("POST ")
                && request_line.contains("/packs/")
                && request_line.contains("/install ")
            {
                respond(
                    &mut stream,
                    "200 OK",
                    r#"{"installed":true,"insertedObjects":1}"#,
                );
            } else if request_line.starts_with("GET ") && request_line.contains("/git/ops/heads ") {
                respond(&mut stream, "200 OK", r#"{"cursor":0,"heads":[]}"#);
            } else if request_line.starts_with("POST ")
                && request_line.contains("/git/ops/inventory ")
            {
                let body: serde_json::Value = serde_json::from_str(request_body(&request)).unwrap();
                respond(
                    &mut stream,
                    "200 OK",
                    &serde_json::json!({ "keys": body["keys"] }).to_string(),
                );
            } else if request_line.starts_with("POST ")
                && request_line.contains("/git/ops/heads/transactions ")
            {
                let body: serde_json::Value = serde_json::from_str(request_body(&request)).unwrap();
                respond(
                    &mut stream,
                    "200 OK",
                    &serde_json::json!({ "cursor": 1, "heads": [body["newHead"]] }).to_string(),
                );
            } else if request_line.starts_with("PUT ")
                && (request_line.contains("/git/ops/views/")
                    || request_line.contains("/git/ops/operations/"))
            {
                respond(&mut stream, "200 OK", r#"{}"#);
            } else {
                panic!("unexpected fake cloud request: {request_line}");
            }
        }
    });
    (base_url, push_reached_rx, release_push_tx, server)
}
