# Release-check store contract

These Linux-only, offline tests specify SC-03 of the revision-6 self-check design (§4.1–4.4 and §7.3). They use only `tempfile::tempdir()`, fake owner identities and real filesystem operations beneath those tempdirs. They start no daemon or agent and read no real HOME, process identity, credential or state directory. SC-03 depends on SC-02 only for the key: `(platform, agent, release, argv class, executable digest, companion digests, deck image hash)`. The fixtures supply already-measured digests and the `interactive` argv class; they do not pretend to test identity measurement, executable copying or argv classification.

The assignment explicitly requires bare `tempfile::tempdir()` isolation, so the single `scratch` constructor carries linkage-check rule 8's supported `allow-bare-tempdir` marker. It does not initialize the shared harness temp base or alter the process environment. The fixture holds small JSON/evidence files only, not recordings, agent homes, binaries or repository clones.

## Assumed interface

`tests/release_check_store.rs` intentionally cannot compile until `src/release_check/store.rs` exists. It compiles that source directly as a module under the integration test's `cfg(test)` so the crash seam can be `#[cfg(test)]` without an e2e feature, environment variable or production switch. Production declares the same source as `release_check::store`; its `testing` module and fault machinery must be compiled out of the normal library and binary. Keep dependencies on sibling modules explicit so the source can also compile in this integration crate. `#[rustfmt::skip]` on the path module prevents rustfmt from descending into a deliberately absent source file; it does not suppress formatting of the contract tests themselves.

The names below are the assumed boundary, not a production shim. Types implement `Debug`; value types also implement `Clone + PartialEq + Eq`, and checkpoint types implement `Copy`. Metadata, journal and view values are owned snapshots. A mark set is exposed as a sorted `Vec<Mark>` (owner identity then ring id), with unique elements. `Store` needs no `Clone`, but independent handles must serialize through the actual store lock.

```rust
struct Key {
    platform: String, agent: String, release: String, argv_class: String,
    executable_digest: String, companion_digests: Vec<String>, deck_image_hash: String,
}
struct Owner { boot_id: String, pid: u32, start_time: u64 }
struct Mark { owner: Owner, ring_id: String }
enum Outcome {
    Passed, Differs { fact: String }, Inconclusive { reason: String },
    Unavailable { reason: String },
}
struct Evidence { outcome: Outcome, runner_image_hash: String, body: Vec<u8> }
enum State {
    Passed { publication: u64 }, Differs { fact: String },
    Inconclusive { reason: String }, Unavailable { reason: String },
    Revoked { through: u64, cause: String }, Forgotten { at: u64 },
}
struct Journal {
    last_event: u64, revoked_through: u64, state: Option<State>,
    queued: Option<u64>, ringing: Vec<Mark>,
}
struct Metadata { store_id: [u8; 16], epoch: u64, floor: u64 }
struct View {
    journal: Option<Journal>, evidence: Option<Evidence>,
    pending_trip: bool, admission: Option<u64>,
}
struct Ticket { store_id: [u8; 16], key: Key, queued: u64 }
enum Publication { Published { epoch: u64 }, Stale }
enum Error {
    #[cfg(test)] InjectedCrash { checkpoint: testing::Checkpoint },
    /* ordinary I/O etc. */
}

impl Store {
    fn metadata(&self) -> Result<Metadata, Error>;
    fn keys(&self) -> Result<Vec<Key>, Error>;
    fn view(&self, key: &Key) -> Result<View, Error>;
    fn queue(&mut self, key: &Key) -> Result<Ticket, Error>;
    fn publish(&mut self, ticket: &Ticket, evidence: Evidence,
               publishing_daemon_image: &str) -> Result<Publication, Error>;
    fn ring_start(&mut self, key: &Key, mark: &Mark) -> Result<RingPermit, Error>;
    fn ring_end(&mut self, key: &Key, mark: &Mark, trip: Option<&str>) -> Result<(), Error>;
    fn convert_dead_owners(&mut self, key: &Key) -> Result<Option<u64>, Error>;
    fn observe(&mut self, key: &Key) -> Result<View, Error>;
    fn forget(&mut self, key: &Key) -> Result<u64, Error>;
    fn evict_evidence(&mut self, key: &Key) -> Result<(), Error>;
    fn retain_evidence(&mut self, per_agent: usize) -> Result<(), Error>;
    fn prune_to(&mut self, total: usize) -> Result<(), Error>;
}
```

All listed fields and methods must be accessible to the test crate. The store can re-export an eventual shared outcome type; these tests use only its listed fields. `RingPermit` has no assumed fields: receiving one is the sole boundary before the caller can write its first byte. `view` is a read-only projection used to inspect PendingTrip before conversion; `observe` performs the store portion of an observation, including retrying a dead-owner conversion. The daemon's atomic latch, permanent transaction binding, watch delivery, UI projection and shipped-record precedence belong to SC-09a and later slices. These tests do not claim to exercise them.

The evidence body is opaque to SC-03, which persists it and its outcome. SC-01 and the runner validate its measurement schema. Production uses the §4.1 constants (20 evidence files per agent and 4,096 journals); accepting smaller explicit retention thresholds here allows bounded tests without thousands of fsyncs. Pruning selects the oldest `last_event` and never any journal with a mark. Evidence retention removes the oldest non-Passed evidence first. Neither operation deletes an otherwise retained journal. Every path-bearing key field is validated before allocating an epoch or reading, writing or unlinking a file. Store operations anchor to an open records-directory descriptor and refuse symlinks with no-follow operations, including the lock and temporary names.

## Test-only crash and owner seam

```rust
#[cfg(test)]
pub mod testing {
    enum FileKind { Journal, Store, Evidence, Lock }
    enum WriteStep { TempSynced, Renamed, DirectorySynced }
    enum Checkpoint {
        Write { file: FileKind, step: WriteStep },
        QueuePersistedBeforeLaunch,
        CreateJournalBeforeWrite, CreateJournalDurable,
        PruneBeforeFloor, PruneFloorDurable,
        PruneAfterUnlink { index: usize }, PruneUnlinkDurable { index: usize },
        EvidenceUnlinked, EvidenceUnlinkDurable,
    }
    enum Fault { CrashAt(Checkpoint), RefuseAt(Checkpoint) }
    enum CrashImage { KeepUnsynced, DropUnsynced }
    fn open(records: &Path, alive: Vec<Owner>) -> Result<Store, Error>;
    fn recover(records: &Path, alive: Vec<Owner>, image: CrashImage) -> Result<Store, Error>;
    fn owners(store: &mut Store, alive: Vec<Owner>);
    fn arm(store: &mut Store, fault: Fault);
    fn prune_keys(store: &mut Store, selected: &[Key]) -> Result<(), Error>;
    fn remove_journal_without_pruning(store: &mut Store, key: &Key) -> Result<(), Error>;
    fn path(store: &Store, key: &Key, file: FileKind) -> PathBuf;
}
```

Faults and owner observations are instance-local, never global or environment-controlled. `open` and `recover` use a synthetic owner resolver: exact boot/pid/start-time tuples listed in `alive` are alive; every other tuple is dead. The production resolver remains the SC-03 implementation's real boot/pid/start-time check. This pins pid reuse without reading or signaling host processes.

`CrashAt` stops immediately **after** the named completed filesystem step (except `CreateJournalBeforeWrite` and `PruneBeforeFloor`, which mean before that operation). It returns `InjectedCrash` carrying the exact checkpoint, consumes the one-shot fault and permits no remaining steps of the interrupted operation. Each per-file step names the first such step of that file in the interrupted operation. `CreateJournalDurable` is after the empty floor-fenced journal's rename and directory fsync, before any queue/result mutation. QueuePersistedBeforeLaunch is after q is durably persisted and before the caller receives a launchable ticket. PruneFloorDurable is after both floor-file and directory fsync; unlink indices are zero-based in the requested selection order. The tests assert every requested checkpoint was actually hit; unreachable fault names cannot silently pass.

`recover` reopens from a simulated crash image, not an ordinary close that flushes unfinished work. The test-only filesystem wrapper performs the real temp-file, fsync, rename and unlink operations and tracks the last durable contents and directory entries under the tempdir. KeepUnsynced keeps completed but unfsynced changes; DropUnsynced restores the last fsynced file/directory state, undoing only changes that lacked their required durability barrier. Already durable changes must survive either choice. The pending crash image belongs to the records-directory fixture and survives dropping an interrupted Store handle; recover releases/abandons that handle's lock safely. No host crash, process kill, sleep or filesystem durability assumption is involved. This is a seam around the production filesystem operations, not a second implementation of the state machine or CAS. `RefuseAt` injects an ordinary EIO instead of completing the corresponding filesystem step, exercises the normal error return and keeps the failed conversion fail-closed as PendingTrip until observe retries it. Failure after an uncertain rename must not turn an uncommitted mark write into a ring permit or silently retire PendingTrip.

`prune_keys` is the test-only trigger named in design revision 6: it uses the production floor-before-unlink path and still skips marked journals. `remove_journal_without_pruning` models a missing journal with a ticket newer than the floor, solely to reach the absent-journal CAS arm; it must never be used by production. `path` only exposes the actual file name for symlink controls. Crash simulation and restore are bounded to the fixture's tempdir. No production flag or hidden CLI is required for this fast suite.

## Coverage and RED evidence

All 54 Scenario-annotated functions use `inbox/doorbell/061`. The nine `doorbell_061_row_*` functions each assert the full clean transition and crash at every relevant file fsync, rename and directory-fsync, forcing both recovered images; a nonzero starting watermark and existing live peer marks make preservation assertions load-bearing. The fifteen `doorbell_061_prune_<checkpoint>_<column>` functions cover every pruning-table cell. `unlink_unsynced` explicitly forces present and absent recoveries and checks the outcome from the recovered journal. `prune_between_unlinks_keeps_floor` covers a two-key removal and an untouched third key. `recreation_after_forget_fences_old_ticket` and `recreation_after_trip_fences_old_ticket` cover both invalidators, reloads between every pair of steps, and crashes before and after creation. The base acceptance controls, dead-owner and failure controls, and successor-state watermark assertions have separately named functions; conversion also rechecks an owner that becomes live after the initial PendingTrip observation. The randomized watermark control runs 8 fixed seeds × 64 row choices with a guaranteed crash at a seeded queue write boundary, uses xorshift64 rather than an added random dependency, and prints the seed on failure. Extra controls pin runner/key/daemon image agreement, private permissions, input validation, descriptor-anchored removal after a directory rename, nonpassing-state ring refusal, retention order and independent-handle locking.

The expected initial RED is the compiler's missing `src/release_check/store.rs` error. It is **interface RED**, not a behavioral assertion observed failing: none of these assertions can execute until the coder supplies the interface. GREEN must run the specific contract, all `doorbell_061_*` functions and `cargo test-fast`; SC-01's already-stacked missing evaluator is an independent compile blocker for the full fast tier. There is no real-agent, lane-2 or full-e2e requirement for this slice.
