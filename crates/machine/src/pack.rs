use std::collections::{BTreeMap, BTreeSet};

use blake2::{Blake2b512, Digest as _};
use devspace_kernel::{ValidationError, validate};
use thiserror::Error;

use crate::pack_manifest::{ChunkEntry, ObjectEntry, PackManifest, PackManifestError};
use crate::{MachineGitRepository, ObjectClosure, ObjectClosureError, ObjectKey, Oid, hex};

pub type Digest = [u8; 64];

pub const MIN_CHUNK_BYTES: u32 = 64 * 1024;
const DEFAULT_CHUNK_BYTES: u32 = 1024 * 1024;
pub const MAX_CHUNK_BYTES: u32 = 8 * 1024 * 1024;
pub const MIN_PACK_BYTES: u64 = 1024 * 1024;
const DEFAULT_PACK_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PACK_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_PACK_OBJECTS: u32 = 65_536;
pub const MAX_PACK_OBJECTS: u32 = 65_536;
pub(crate) const MAX_PACK_HEADS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackOptions {
    pub chunk_bytes: u32,
    pub pack_bytes: u64,
    pub pack_objects: u32,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            pack_bytes: DEFAULT_PACK_BYTES,
            pack_objects: DEFAULT_PACK_OBJECTS,
        }
    }
}

impl PackOptions {
    fn validate(self) -> Result<(), PackBuildError> {
        if !(MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES).contains(&self.chunk_bytes) {
            return Err(PackBuildError::InvalidChunkSize(self.chunk_bytes));
        }
        if !(MIN_PACK_BYTES..=MAX_PACK_BYTES).contains(&self.pack_bytes) {
            return Err(PackBuildError::InvalidPackSize(self.pack_bytes));
        }
        if self.pack_objects == 0 || self.pack_objects > MAX_PACK_OBJECTS {
            return Err(PackBuildError::InvalidPackObjectCount(self.pack_objects));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackMetrics {
    pub discovered_objects: usize,
    pub skipped_known_objects: usize,
    pub packed_objects: usize,
    pub packed_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltPack {
    pub id: Digest,
    pub manifest: PackManifest,
    pub manifest_bytes: Vec<u8>,
    pub chunks: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltPacks {
    pub packs: Vec<BuiltPack>,
    pub metrics: PackMetrics,
}

pub struct PackProducer<'a> {
    repository: &'a MachineGitRepository,
    head_commits: &'a [Oid],
    objects: Vec<&'a crate::MachineObject>,
    next_object: usize,
    options: PackOptions,
    metrics: PackMetrics,
}

impl<'a> PackProducer<'a> {
    pub fn new(
        repository: &'a MachineGitRepository,
        closure: &'a ObjectClosure,
        known_objects: &BTreeSet<ObjectKey>,
        options: PackOptions,
    ) -> Result<Self, PackBuildError> {
        options.validate()?;
        let mut objects = dependency_order(&closure.objects)?;
        let skipped_known_objects = objects
            .iter()
            .filter(|object| known_objects.contains(&object.key))
            .count();
        objects.retain(|object| !known_objects.contains(&object.key));
        if closure.head_commits.len() > MAX_PACK_HEADS {
            return Err(PackBuildError::TooManyHeads(closure.head_commits.len()));
        }
        Ok(Self {
            repository,
            head_commits: &closure.head_commits,
            objects,
            next_object: 0,
            options,
            metrics: PackMetrics {
                discovered_objects: closure.objects.len(),
                skipped_known_objects,
                packed_objects: 0,
                packed_bytes: 0,
            },
        })
    }

    pub fn next_pack(&mut self) -> Result<Option<BuiltPack>, PackBuildError> {
        if self.next_object == self.objects.len() {
            return Ok(None);
        }
        let mut builder = PackBuilder::new(self.options);
        while let Some(object) = self.objects.get(self.next_object).copied() {
            if object.length > self.options.pack_bytes {
                return Err(PackBuildError::ObjectExceedsPackLimit {
                    key: object.key,
                    length: object.length,
                    limit: self.options.pack_bytes,
                });
            }
            if !builder.is_empty() && !builder.can_fit(object.length) {
                break;
            }
            let bytes = self.repository.read_object(object.key)?;
            if bytes.len() as u64 != object.length {
                return Err(PackBuildError::ObjectLengthChanged {
                    key: object.key,
                    discovered: object.length,
                    actual: bytes.len() as u64,
                });
            }
            let validated = validate(object.key.kind, &bytes).map_err(|source| {
                PackBuildError::ValidateObject {
                    key: object.key,
                    source,
                }
            })?;
            if validated.id != object.key.id {
                return Err(PackBuildError::ObjectIdMismatch {
                    key: object.key,
                    actual: hex(&validated.id.0),
                });
            }
            self.metrics.packed_objects += 1;
            self.metrics.packed_bytes += bytes.len() as u64;
            builder.push(object.key, bytes);
            self.next_object += 1;
        }
        Ok(Some(builder.finish(self.head_commits)?))
    }

    pub fn metrics(&self) -> &PackMetrics {
        &self.metrics
    }
}

pub fn build_packs(
    repository: &MachineGitRepository,
    closure: &ObjectClosure,
    known_objects: &BTreeSet<ObjectKey>,
    options: PackOptions,
) -> Result<BuiltPacks, PackBuildError> {
    let mut producer = PackProducer::new(repository, closure, known_objects, options)?;
    let mut packs = Vec::new();
    while let Some(pack) = producer.next_pack()? {
        packs.push(pack);
    }
    Ok(BuiltPacks {
        metrics: producer.metrics().clone(),
        packs,
    })
}

fn dependency_order(
    objects: &[crate::MachineObject],
) -> Result<Vec<&crate::MachineObject>, PackBuildError> {
    let mut by_key = BTreeMap::new();
    for object in objects {
        if by_key.insert(object.key, object).is_some() {
            return Err(PackBuildError::DuplicateObject(object.key));
        }
    }

    let mut remaining_dependencies = BTreeMap::new();
    let mut dependents = BTreeMap::<ObjectKey, Vec<ObjectKey>>::new();
    for object in by_key.values() {
        for dependency in &object.dependencies {
            if !by_key.contains_key(dependency) {
                return Err(PackBuildError::MissingDependency {
                    source_key: object.key,
                    target: *dependency,
                });
            }
            dependents.entry(*dependency).or_default().push(object.key);
        }
        remaining_dependencies.insert(object.key, object.dependencies.len());
    }

    let mut ready = remaining_dependencies
        .iter()
        .filter_map(|(key, count)| (*count == 0).then_some(*key))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(by_key.len());
    while let Some(key) = ready.pop_first() {
        ordered.push(
            by_key
                .remove(&key)
                .expect("ready objects must remain in the closure"),
        );
        for dependent in dependents.get(&key).into_iter().flatten() {
            let count = remaining_dependencies
                .get_mut(dependent)
                .expect("closure dependencies must have a count");
            *count -= 1;
            if *count == 0 {
                ready.insert(*dependent);
            }
        }
    }
    if !by_key.is_empty() {
        return Err(PackBuildError::DependencyCycle);
    }
    Ok(ordered)
}

struct PackBuilder {
    options: PackOptions,
    length: u64,
    objects: Vec<(ObjectKey, Vec<u8>)>,
}

impl PackBuilder {
    fn new(options: PackOptions) -> Self {
        Self {
            options,
            length: 0,
            objects: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    fn can_fit(&self, length: u64) -> bool {
        self.objects.len() < self.options.pack_objects as usize
            && self
                .length
                .checked_add(length)
                .is_some_and(|total| total <= self.options.pack_bytes)
    }

    fn push(&mut self, key: ObjectKey, bytes: Vec<u8>) {
        self.length += bytes.len() as u64;
        self.objects.push((key, bytes));
    }

    fn finish(mut self, head_commits: &[Oid]) -> Result<BuiltPack, PackBuildError> {
        self.objects.sort_unstable_by_key(|(key, _)| *key);
        let mut entries = Vec::with_capacity(self.objects.len());
        let mut chunks = Vec::new();
        let mut pack_hash = Blake2b512::new();
        let mut offset = 0_u64;
        for (key, bytes) in self.objects {
            entries.push(ObjectEntry {
                key,
                offset,
                length: bytes.len() as u64,
            });
            offset += bytes.len() as u64;
            pack_hash.update(&bytes);
            let mut remaining = bytes.as_slice();
            while !remaining.is_empty() {
                if chunks
                    .last()
                    .is_none_or(|chunk: &Vec<u8>| chunk.len() == self.options.chunk_bytes as usize)
                {
                    chunks.push(Vec::with_capacity(self.options.chunk_bytes as usize));
                }
                let chunk = chunks
                    .last_mut()
                    .expect("the chunk list was initialized above");
                let available = self.options.chunk_bytes as usize - chunk.len();
                let count = available.min(remaining.len());
                chunk.extend_from_slice(&remaining[..count]);
                remaining = &remaining[count..];
            }
        }
        debug_assert_eq!(offset, self.length);
        let pack_hash = pack_hash.finalize().into();
        let mut chunk_offset = 0_u64;
        let chunk_entries = chunks
            .iter()
            .map(|chunk| {
                let entry = ChunkEntry {
                    offset: chunk_offset,
                    length: chunk.len() as u32,
                    hash: hash(chunk),
                };
                chunk_offset += chunk.len() as u64;
                entry
            })
            .collect();
        let manifest = PackManifest::new(
            self.options.chunk_bytes,
            self.length,
            pack_hash,
            head_commits.to_vec(),
            entries,
            chunk_entries,
        )?;
        let manifest_bytes = manifest.encode();
        let id = hash(&manifest_bytes);
        Ok(BuiltPack {
            id,
            manifest,
            manifest_bytes,
            chunks,
        })
    }
}

pub(crate) fn hash(bytes: &[u8]) -> Digest {
    Blake2b512::digest(bytes).into()
}

#[derive(Debug, Error)]
pub enum PackBuildError {
    #[error(transparent)]
    Manifest(#[from] PackManifestError),
    #[error(transparent)]
    Closure(#[from] ObjectClosureError),
    #[error("invalid chunk size {0}")]
    InvalidChunkSize(u32),
    #[error("invalid pack size {0}")]
    InvalidPackSize(u64),
    #[error("invalid pack object count {0}")]
    InvalidPackObjectCount(u32),
    #[error("pack has {0} heads; maximum is {MAX_PACK_HEADS}")]
    TooManyHeads(usize),
    #[error("object closure contains duplicate {0:?}")]
    DuplicateObject(ObjectKey),
    #[error("object {source_key:?} references {target:?} outside its closure")]
    MissingDependency {
        source_key: ObjectKey,
        target: ObjectKey,
    },
    #[error("object closure contains a dependency cycle")]
    DependencyCycle,
    #[error("object {key:?} changed length from {discovered} to {actual}")]
    ObjectLengthChanged {
        key: ObjectKey,
        discovered: u64,
        actual: u64,
    },
    #[error("object {key:?} is not canonical")]
    ValidateObject {
        key: ObjectKey,
        #[source]
        source: ValidationError,
    },
    #[error("object {key:?} hashes to {actual}")]
    ObjectIdMismatch { key: ObjectKey, actual: String },
    #[error("object {key:?} is {length} bytes; pack limit is {limit}")]
    ObjectExceedsPackLimit {
        key: ObjectKey,
        length: u64,
        limit: u64,
    },
}
