//! Builds the Worker's Git pack fixtures and pins them against the encoder.
//!
//! `test/fixtures/repository.json` is the only place where this crate's DSPK
//! encoder meets the Worker's separate TypeScript decoder. The fixtures are
//! rebuilt on every `cargo test` and compared with the checked-in file, so a
//! changed offset or field order fails here instead of drifting unnoticed. The
//! `rejections` entries carry the same contract in the other direction: each is
//! a manifest that `PackManifest::decode` refuses, and the Worker suite asserts
//! that `decodeGitManifest` refuses it too.

use std::fs;
use std::path::PathBuf;

use devspace_kernel::{ObjectKind, Oid, validate};
use serde_json::{Value, json};

use crate::encode_lower_hex;
use crate::object_closure::{MAX_OBJECT_BYTES, ObjectKey};
use crate::pack::{MAX_PACK_OBJECTS, MIN_CHUNK_BYTES, hash};
use crate::pack_manifest::{ChunkEntry, ObjectEntry, PackManifest};

const UPDATE_ENV: &str = "DEVSPACE_UPDATE_FIXTURES";
/// DSPK v2 field positions, used to mutate an encoded manifest in place.
const OBJECT_COUNT_FIELD: usize = 16;
const PACK_LENGTH_FIELD: usize = 24;
const HEADS_OFFSET: usize = 96;
const HEAD_ENTRY_BYTES: usize = 20;
const OBJECT_ENTRY_BYTES: usize = 44;
const OBJECT_OFFSET_FIELD: usize = 28;
const OBJECT_LENGTH_FIELD: usize = 36;
const CHUNK_ENTRY_BYTES: usize = 80;
const CHUNK_LENGTH_FIELD: usize = 8;

#[test]
fn worker_git_pack_fixtures_match_the_encoder() {
    let built = build_fixtures();
    let path = repository_root().join("test/fixtures/repository.json");
    if std::env::var_os(UPDATE_ENV).is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec_pretty(&built).unwrap()).unwrap();
        return;
    }

    let checked_in: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let built = built.as_object().unwrap();
    for (name, value) in built {
        assert!(
            checked_in.get(name) == Some(value),
            "test/fixtures/repository.json `{name}` does not match the encoder; \
             regenerate with `{UPDATE_ENV}=1 cargo test -p devspace-machine worker_git_pack_fixtures`",
        );
    }
    assert_eq!(
        checked_in.as_object().unwrap().len(),
        built.len(),
        "test/fixtures/repository.json has fixtures the generator does not emit",
    );
}

fn build_fixtures() -> Value {
    let blob_bytes = b"worker Git fixture\n".to_vec();
    let blob = object(ObjectKind::Blob, blob_bytes.clone());

    let mut tree_bytes = b"100644 fixture.txt\0".to_vec();
    tree_bytes.extend_from_slice(&blob.0.id.0);
    let tree = object(ObjectKind::Tree, tree_bytes);

    let commit_bytes = format!(
        "tree {}\nauthor Worker Fixture <worker@example.invalid> 1700000000 +0000\ncommitter Worker Fixture <worker@example.invalid> 1700000000 +0000\n\nworker fixture\n",
        encode_lower_hex(&tree.0.id.0),
    )
    .into_bytes();
    let commit = object(ObjectKind::Commit, commit_bytes);

    let complete = fixture(
        vec![commit.0.id],
        vec![blob.clone(), tree.clone(), commit.clone()],
    );
    let journal_commits = (0..260)
        .map(|index| {
            object(
                ObjectKind::Commit,
                format!(
                    "tree {}\nauthor Journal Fixture <journal@example.invalid> {} +0000\ncommitter Journal Fixture <journal@example.invalid> {} +0000\n\njournal fixture {index}\n",
                    encode_lower_hex(&tree.0.id.0),
                    1_700_001_000 + index,
                    1_700_001_000 + index,
                )
                .into_bytes(),
            )
        })
        .collect::<Vec<_>>();
    let journal = fixture(
        journal_commits.iter().map(|commit| commit.0.id).collect(),
        [vec![blob.clone(), tree.clone()], journal_commits.clone()].concat(),
    );
    let dependency = fixture(Vec::new(), vec![blob.clone()]);
    let missing_reference = fixture(vec![commit.0.id], vec![tree.clone(), commit.clone()]);

    let (malformed_id, malformed_bytes) = truncated_golden_commit();
    let malformed = fixture(
        vec![malformed_id],
        vec![
            blob.clone(),
            (
                ObjectKey {
                    kind: ObjectKind::Commit,
                    id: malformed_id,
                },
                malformed_bytes,
            ),
        ],
    );

    let rejections = rejections(&blob, &tree, &commit, &journal_commits[0]);

    json!({
        "complete": complete,
        "journal": journal,
        "dependency": dependency,
        "missingReference": missing_reference,
        "malformed": malformed,
        "rejections": rejections,
    })
}

/// One manifest per refusal rule, each mutated from a manifest the encoder
/// accepts. The chunk payloads are left out: only the manifest is decoded.
fn rejections(
    blob: &(ObjectKey, Vec<u8>),
    tree: &(ObjectKey, Vec<u8>),
    commit: &(ObjectKey, Vec<u8>),
    other_commit: &(ObjectKey, Vec<u8>),
) -> Vec<Value> {
    // One head, three objects, one chunk.
    let (base, _) = encode_fixture(
        vec![commit.0.id],
        vec![blob.clone(), tree.clone(), commit.clone()],
    );
    let base_objects = HEADS_OFFSET + HEAD_ENTRY_BYTES;

    // Two heads, so the head order can be broken.
    let mut heads = vec![commit.0.id, other_commit.0.id];
    heads.sort_unstable();
    let (two_heads, _) = encode_fixture(heads, vec![blob.clone(), tree.clone(), commit.clone()]);

    // One object spanning two chunks, so the first chunk can be made short.
    let (two_chunks, _) = encode_fixture(
        Vec::new(),
        vec![object(
            ObjectKind::Blob,
            vec![b'x'; MIN_CHUNK_BYTES as usize + 1],
        )],
    );
    let two_chunks_chunks = HEADS_OFFSET + OBJECT_ENTRY_BYTES;

    // A first object at exactly the object limit. Growing it by one byte also
    // moves the second object, the pack length and the final chunk, so the
    // ranges stay canonical and both decoders reach their object-size rule
    // rather than a range rule.
    let (at_limit, at_limit_data) = encode_fixture(
        Vec::new(),
        vec![
            object(ObjectKind::Blob, vec![b'y'; MAX_OBJECT_BYTES as usize]),
            tree.clone(),
        ],
    );
    let at_limit_final_chunk = HEADS_OFFSET
        + 2 * OBJECT_ENTRY_BYTES
        + (at_limit_data.len().div_ceil(MIN_CHUNK_BYTES as usize) - 1) * CHUNK_ENTRY_BYTES;
    let at_limit_final_length = at_limit_data.len() as u32 % MIN_CHUNK_BYTES;

    vec![
        rejection(&base, "bad-magic", |bytes| bytes[3] = b'X'),
        rejection(&base, "bad-version", |bytes| bytes[4] = 3),
        rejection(&base, "reserved-bytes", |bytes| bytes[6] = 1),
        rejection(&two_heads, "head-order", |bytes| {
            bytes[HEADS_OFFSET..HEADS_OFFSET + 2 * HEAD_ENTRY_BYTES].rotate_left(HEAD_ENTRY_BYTES);
        }),
        rejection(&base, "object-order", |bytes| {
            bytes[base_objects..base_objects + 2 * OBJECT_ENTRY_BYTES]
                .rotate_left(OBJECT_ENTRY_BYTES);
        }),
        rejection(&base, "range-gap", |bytes| {
            write_u64(bytes, base_objects + OBJECT_OFFSET_FIELD, 1);
        }),
        rejection(&two_chunks, "short-chunk", |bytes| {
            write_u32(
                bytes,
                two_chunks_chunks + CHUNK_LENGTH_FIELD,
                MIN_CHUNK_BYTES - 1,
            );
        }),
        rejection(&base, "too-many-objects", |bytes| {
            write_u32(bytes, OBJECT_COUNT_FIELD, MAX_PACK_OBJECTS + 1);
        }),
        rejection(&at_limit, "object-too-large", |bytes| {
            write_u64(
                bytes,
                HEADS_OFFSET + OBJECT_LENGTH_FIELD,
                MAX_OBJECT_BYTES + 1,
            );
            write_u64(
                bytes,
                HEADS_OFFSET + OBJECT_ENTRY_BYTES + OBJECT_OFFSET_FIELD,
                MAX_OBJECT_BYTES + 1,
            );
            write_u64(bytes, PACK_LENGTH_FIELD, at_limit_data.len() as u64 + 1);
            write_u32(
                bytes,
                at_limit_final_chunk + CHUNK_LENGTH_FIELD,
                at_limit_final_length + 1,
            );
        }),
    ]
}

fn rejection(base: &[u8], slug: &str, mutate: impl FnOnce(&mut Vec<u8>)) -> Value {
    assert!(
        PackManifest::decode(base).is_ok(),
        "rejection base for `{slug}` is not a valid manifest",
    );
    let mut bytes = base.to_vec();
    mutate(&mut bytes);
    assert!(
        PackManifest::decode(&bytes).is_err(),
        "rejection fixture `{slug}` is accepted by the Rust decoder",
    );
    json!({ "slug": slug, "manifest": encode_lower_hex(&bytes) })
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn object(kind: ObjectKind, bytes: Vec<u8>) -> (ObjectKey, Vec<u8>) {
    let id = validate(kind, &bytes).unwrap().id;
    (ObjectKey { kind, id }, bytes)
}

fn fixture(heads: Vec<Oid>, objects: Vec<(ObjectKey, Vec<u8>)>) -> Value {
    let (manifest, data) = encode_fixture(heads, objects);
    json!({
        "id": encode_lower_hex(&hash(&manifest)),
        "manifest": encode_lower_hex(&manifest),
        "chunks": [encode_lower_hex(&data)],
    })
}

fn encode_fixture(
    mut heads: Vec<Oid>,
    mut objects: Vec<(ObjectKey, Vec<u8>)>,
) -> (Vec<u8>, Vec<u8>) {
    heads.sort_unstable();
    objects.sort_unstable_by_key(|(key, _)| *key);
    let mut data = Vec::new();
    let entries = objects
        .into_iter()
        .map(|(key, bytes)| {
            let offset = data.len() as u64;
            let length = bytes.len() as u64;
            data.extend_from_slice(&bytes);
            ObjectEntry {
                key,
                offset,
                length,
            }
        })
        .collect();
    let chunks = data
        .chunks(MIN_CHUNK_BYTES as usize)
        .enumerate()
        .map(|(index, chunk)| ChunkEntry {
            offset: (index * MIN_CHUNK_BYTES as usize) as u64,
            length: chunk.len() as u32,
            hash: hash(chunk),
        })
        .collect();
    let manifest = PackManifest::new(
        MIN_CHUNK_BYTES,
        data.len() as u64,
        hash(&data),
        heads,
        entries,
        chunks,
    )
    .unwrap();
    (manifest.encode(), data)
}

fn truncated_golden_commit() -> (Oid, Vec<u8>) {
    let golden =
        fs::read_to_string(repository_root().join("crates/kernel/tests/git_golden.txt")).unwrap();
    let line = golden
        .lines()
        .find(|line| line.starts_with("commit|"))
        .unwrap();
    let mut fields = line.split('|');
    assert_eq!(fields.next(), Some("commit"));
    let id = Oid::from_hex(fields.next().unwrap().as_bytes()).unwrap();
    let bytes = decode_hex(fields.next().unwrap());
    let terminator = bytes.windows(2).position(|pair| pair == b"\n\n").unwrap();
    (id, bytes[..=terminator].to_vec())
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}
