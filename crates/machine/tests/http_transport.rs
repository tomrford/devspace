use std::collections::BTreeSet;
use std::error::Error;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use devspace_kernel::{ObjectKind, Oid};
use devspace_machine::{
    GitHttpTransport, GitHttpTransportError, MachineGitRepository, ObjectKey, OpSyncTransport as _,
};
use devspace_testutils::fake_worker::{create_server, respond};
use devspace_testutils::stalling_server::StallingServer;
use jj_lib::settings::UserSettings;

mod common;

use common::{oid_hex, write_raw};

const CHILD_ENV: &str = "DEVSPACE_MACHINE_GIT_TIMEOUT_TEST_CHILD";

fn settings() -> UserSettings {
    devspace_testutils::settings("HTTP Transport Test", "transport@example.invalid", false)
}

#[tokio::test(flavor = "current_thread")]
async fn http_transport_times_out_when_worker_stalls() {
    if std::env::var_os(CHILD_ENV).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "http_transport_times_out_when_worker_stalls",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env("DEVSPACE_HTTP_TEST_HOOKS", "1")
            .env("DEVSPACE_HTTP_TEST_REQUEST_TIMEOUT_MS", "100")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let server = StallingServer::start();
    let transport = GitHttpTransport::new(
        server.base_url(),
        "timeout-test-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    let started = Instant::now();
    let error = transport.list_packs(0, None).await.unwrap_err();

    assert!(
        started.elapsed() < Duration::from_secs(20),
        "Git catalog request took {:?}",
        started.elapsed(),
    );
    assert!(
        error_chain_contains(&error, "operation timed out"),
        "{error:?}"
    );
}

fn error_chain_contains(error: &(dyn Error + 'static), needle: &str) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.to_string().contains(needle) {
            return true;
        }
        current = error.source();
    }
    false
}

#[tokio::test(flavor = "current_thread")]
async fn paged_projection_snapshot_keeps_first_page_metadata_during_concurrent_update() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let first = projection_page_json(2, "11", "aa", 1, true, "01");
        let second = projection_page_json(3, "22", "bb", 2, false, "02");
        for body in [first, second] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            if body.contains("\"nextAfter\":1") {
                assert!(request.contains("after=0"));
                assert!(!request.contains("through="));
            } else {
                assert!(request.contains("after=1&through=2"));
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });
    let transport = GitHttpTransport::new(
        &format!("http://{address}"),
        "snapshot-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    let snapshot = transport.projection_snapshot_all().await.unwrap();

    assert_eq!(snapshot.activation_cursor, 2);
    assert_eq!(snapshot.through, 2);
    assert_eq!(snapshot.next_after, 2);
    assert!(!snapshot.has_more);
    assert_eq!(
        snapshot.cursors[0].canonical_oid,
        devspace_kernel::Oid([0x11; 20])
    );
    assert_eq!(snapshot.pending[0].owner_machine, [0xaa; 16]);
    assert_eq!(snapshot.mappings.len(), 2);
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sync_upload_uses_inventory_to_omit_a_cloud_known_closure() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineGitRepository::init(temp.path().join("repository"), &settings())
        .await
        .unwrap();
    let blob = write_raw(&repository, ObjectKind::Blob, b"known sync closure\n");
    let mut tree_bytes = b"100644 file\0".to_vec();
    tree_bytes.extend_from_slice(&blob.0);
    let tree = write_raw(&repository, ObjectKind::Tree, &tree_bytes);
    let commit_bytes = format!(
        "tree {}\nauthor Sync <sync@example.invalid> 1700000000 +0000\ncommitter Sync <sync@example.invalid> 1700000000 +0000\n\nknown\n",
        oid_hex(tree)
    );
    let head = write_raw(&repository, ObjectKind::Commit, commit_bytes.as_bytes());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let length = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..length]);
        assert!(request.starts_with("POST /repositories/"));
        assert!(request.contains("/git/objects/inventory HTTP/1.1"));
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["keys"].as_array().unwrap().len(), 3);
        let response = serde_json::json!({"keys": body["keys"]}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
    });
    let mut transport = GitHttpTransport::new(
        &format!("http://{address}"),
        "inventory-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    transport
        .upload_git_objects(&repository, &BTreeSet::from([head]))
        .await
        .unwrap();
    server.join().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn git_inventory_splits_large_candidate_sets_and_validates_every_response_key() {
    let candidates = (0..=4_096_u32)
        .map(|index| ObjectKey {
            kind: ObjectKind::Blob,
            id: indexed_oid(index),
        })
        .collect::<Vec<_>>();
    let (base_url, server) = create_server(|request_index, request, stream| {
        let request_line = request.lines().next().unwrap();
        assert!(request_line.contains("/git/objects/inventory "));
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        let keys = body["keys"].as_array().unwrap();
        assert_eq!(keys.len(), if request_index == 0 { 4_096 } else { 1 });
        respond(
            stream,
            "200 OK",
            &serde_json::json!({"keys": keys}).to_string(),
        );
        request_index == 1
    });
    let transport = GitHttpTransport::new(
        &base_url,
        "inventory-pages-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    let present = transport.inventory_git_objects(&candidates).await.unwrap();

    assert_eq!(present, candidates.iter().copied().collect());
    assert_eq!(server.join().unwrap().len(), 2);

    let requested = ObjectKey {
        kind: ObjectKind::Blob,
        id: indexed_oid(0),
    };
    for (response_key, expected_message) in [
        (
            format!("x:{}", oid_hex(indexed_oid(0))),
            "Git object inventory returned an invalid kind",
        ),
        (
            format!("b:{}", oid_hex(indexed_oid(1))),
            "Git object inventory returned an unrequested object",
        ),
    ] {
        let (base_url, server) = create_server(move |_, _, stream| {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"keys": [response_key]}).to_string(),
            );
            true
        });
        let transport = GitHttpTransport::new(
            &base_url,
            "inventory-response-secret",
            &"11".repeat(16),
            &"ab".repeat(32),
            &"cd".repeat(16),
        )
        .unwrap();

        let error = transport
            .inventory_git_objects(&[requested])
            .await
            .unwrap_err();
        assert!(
            matches!(&error, GitHttpTransportError::Protocol(message) if message == expected_message),
            "{error:?}"
        );
        server.join().unwrap();
    }
}

fn projection_page_json(
    activation_cursor: u64,
    cursor_byte: &str,
    owner_byte: &str,
    next_after: u64,
    has_more: bool,
    mapping_byte: &str,
) -> String {
    format!(
        r#"{{
          "activationCursor":{activation_cursor},
          "cursors":[{{
            "remote":"origin","bookmark":"main",
            "canonicalOid":"{cursor_oid}","publicOid":"{cursor_oid}",
            "hiddenSetId":null,"activationSequence":{activation_cursor}
          }}],
          "mappings":[{{
            "remote":"origin","bookmark":"main",
            "canonicalOid":"{mapping_oid}","publicOid":"{public_oid}",
            "hiddenSetId":null,"activationSequence":{next_after}
          }}],
          "nextAfter":{next_after},"through":2,"hasMore":{has_more},
          "pending":[{{
            "batchId":"{batch_id}","remote":"origin",
            "ownerMachine":"{owner_machine}","fence":1,"refs":[]
          }}]
        }}"#,
        cursor_oid = cursor_byte.repeat(20),
        mapping_oid = mapping_byte.repeat(20),
        public_oid = "cc".repeat(20),
        batch_id = mapping_byte.repeat(16),
        owner_machine = owner_byte.repeat(16),
    )
}

fn indexed_oid(index: u32) -> Oid {
    let mut bytes = [0; 20];
    bytes[16..].copy_from_slice(&index.to_be_bytes());
    Oid(bytes)
}
