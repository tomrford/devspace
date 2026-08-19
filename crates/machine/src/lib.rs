#![forbid(unsafe_code)]
//! Local machine-store proofs for jj-lib's Git backend.
//!
//! Canonical Git objects move through a standard Git remote. jj's simple
//! operation store and operation-head store remain beside the Git ODB in
//! `op_store/` and `op_heads/`.

mod canonical_remote;
mod control_plane_client;
mod creation_intent;
mod fsync;
mod git_subprocess;
mod http_client;
mod http_transport;
mod journal_flow;
mod lift;
mod locked_json;
mod machine_config;
mod machine_store;
mod object_closure;
mod op_sync;
mod op_sync_state;
mod projection;
mod store;
mod wire;

pub use control_plane_client::{
    CloudRepository, ControlPlaneClient, ControlPlaneClientError, ControlPlaneRemoteErrorKind,
};
pub use creation_intent::{
    RepositoryCreationIntent, RepositoryCreationIntentError, RepositoryCreationKey,
    RepositoryCreationTarget,
};
pub use canonical_remote::{
    CanonicalGitRemote, CanonicalRemoteError, RetentionPush, delete_remote_ref,
    gc_bare_remote, init_bare_remote,
};
pub use fsync::sync_directory;
pub use git_subprocess::{
    FetchError, FetchReport, GitProcessEnvironment, GitProcessMode, LeaseUpdate, PushError,
    PushErrorKind, PushRefReport, PushRefStatus, PushReport, QualifiedRef, QualifiedRefError,
    RemoteHead, RemoteHeadsError, RemoteUrl, fetch, fetch_refspecs, ls_remote_head,
    ls_remote_heads, ls_remote_matching, push,
};
pub use http_transport::{
    GitHttpTransport, GitHttpTransportError, PendingProjectionGitBatch, PendingProjectionGitRef,
    ProjectionGitBatchResult, ProjectionGitClaimResult, ProjectionGitCursor, ProjectionGitFetchRef,
    ProjectionGitFetchResult, ProjectionGitMapping, ProjectionGitObservation, ProjectionGitReplay,
    ProjectionGitSnapshot, ProjectionGitState, ProjectionGitUpdate, RegisteredGitRemote,
};
pub use journal_flow::{
    FetchFlowResult, JournalFlowError, PushFailpoint, PushFlowResult, PushHead, fetch_with_journal,
    push_with_journal,
};
pub use lift::{Disclosure, LiftError, LiftedCommit, OverlayLiftResult, overlay_lift};
pub use machine_config::{MachineConfig, MachineConfigError, MachineId, SharedSecret};
pub use machine_store::{
    CatalogEntry, CheckoutDestinationGuard, MACHINE_STORE_OVERRIDE, MachineStore,
    MachineStoreError, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
    RepositorySyncGuard, StagedRepositoryClone,
};
pub use object_closure::{
    MAX_OBJECT_BYTES, MachineObject, ObjectClosure, ObjectClosureError, ObjectKey,
};
pub use op_sync::{
    CloudOpHeads, OpObjectKey, OpSyncEngine, OpSyncEngineError, OpSyncTransport,
    TransportError as OpTransportError,
};
pub use op_sync_state::{
    OpSyncState, OpSyncStateError, OpSyncStore, PendingOpHeadBatch, PendingOpHeadTransaction,
};
pub use projection::{
    CommitMapping, DSPRIVATE, HiddenSet, HiddenSetIdentity, ProjectionError, ProjectionMappings,
    ProjectionResult,
};
pub use store::OpReconcileError;
pub use store::{MachineGitRepository, MachineGitRepositoryError};
pub use wire::{LowerHexError, decode_lower_hex, encode_lower_hex};

pub use devspace_kernel::ops::OpObjectKind;
pub use devspace_kernel::{ObjectKind, Oid};

pub type OpId = [u8; 64];
