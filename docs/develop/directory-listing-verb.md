# The directory-listing verb

PRD [#1223](https://github.com/vfarcic/dot-agent-deck/issues/1223) M1. This page records `AttachRequest::ListDirectories` — the daemon verb behind the desktop's new-agent directory step — its bounds, and the threat-model argument for adding a listing verb after PRD #819 had deliberately stopped at resolving one path. The mechanism lives in [`src/directory_listing.rs`](../../src/directory_listing.rs), whose module docs carry the bounds next to the code; the wire variant and its dispatch arm are in [`src/daemon_protocol.rs`](../../src/daemon_protocol.rs), and the client method is `DaemonClient::list_directories` in [`src/daemon_client.rs`](../../src/daemon_client.rs).

## Why a daemon verb

The TUI's Ctrl+n opens a directory picker that reads the TUI process's own filesystem. The desktop cannot do that for a deck: against a remote deck the desktop's filesystem is not the one the agent will run in, and even for a local deck the path an agent starts in has to be one the daemon can use. So the directory step asks the daemon, one level at a time, and sends back only paths the daemon supplied or the user typed — PRD #819's rule. The desktop never joins a listed parent and a child name itself.

## What it answers

The request is `{"op": "list-directories"}` with an optional absolute `path`. With no `path`, the daemon lists the daemon user's home directory, canonicalised — not its startup cwd, which for a lazily spawned daemon is wherever some TUI happened to be launched.

A successful reply carries `ok: true` and a top-level `directories` object:

| field | meaning |
| --- | --- |
| `path` | the canonical absolute path that was listed (a typed symlinked spelling lists — and names — its target) |
| `parent` | the canonical absolute path of its parent; absent at the filesystem root, and the only "up" there is — there is no `..` entry |
| `entries` | `{ name, path, is_project }` per immediate subdirectory, sorted by name; `path` is canonical and joined by the daemon |
| `truncated` | `true` when the entry cap or the time budget cut the listing short |

`is_project` is `true` when the subdirectory holds a `.dot-agent-deck.toml` that is a regular file and not a symlink — the same two type refusals the project reader (`project_resolve::read_config_file`) applies to that file. It is a hint for the form, which then asks `ResolveProject` for orchestrations; it is one `lstat`, not a parse, so a marked directory whose config is oversized or malformed still fails that request.

## The bounds

The list is carried from issue #1048 and each item is enforced in `src/directory_listing.rs` rather than trusted to a caller:

- **One level per request.** One `read_dir`; nothing recurses or walks.
- **Directories only.** An entry carries a name, a canonical path and the project marker. No files, sizes, times, owners or modes reach the reply.
- **Hidden entries are skipped and symlinked entries are not listed**, as the TUI's picker does: a name starting with `.` is dropped, and an entry is kept only when `DirEntry::file_type()` — which does not follow a symlink — reports a directory, and is reported only when an `lstat` taken as the reply is built still does (audit A3; see *A point-in-time snapshot* below). A symlink the user *types* is different: the typed path is canonicalised, so it lists the target.
- **A result cap**, `MAX_DIRECTORY_ENTRIES` = **1000**. Past it the reply carries the 1000 smallest names and `truncated: true`. Scanning continues within the time budget so the survivors are a sorted prefix rather than whichever names `readdir` produced first — the smallest of the whole directory when the scan finishes inside the budget, and of the part it reached when the budget cuts it; memory stays at one name more than the cap.
- **A time budget**, `DIRECTORY_LISTING_BUDGET` = **2 s**, measured from the start of the request's filesystem work. When it runs out the reply is what was gathered, with `truncated: true`. It is checked between entries and between marker probes, so it does **not** interrupt one system call that blocks — a `readdir` or `stat` on an unresponsive network mount takes as long as it takes. What contains that case is the concurrency bound below, not this deadline.
- **A concurrency bound of its own**, `new_agent_options::MAX_CONCURRENT_NEW_AGENT_QUERIES` = **4**, shared by `ListDirectories` and `NewAgentOptions` and by nothing else (audit A4). The permit is taken with try-acquire: when all four are held the request is refused at once as `busy` (below) instead of waiting, so at most four blocking threads are ever occupied by these two verbs, and none of them is one of the project verbs' `MAX_CONCURRENT_PROJECT_READS` permits — a burst of listings against a slow directory cannot hold `ResolveProject` or `PrepareWorkflow` up. The listing first ran under the project verbs' permits and the options query under none; both of those are gone.
- **Canonical, absolute paths both ways.** A caller path must be absolute, non-empty, at most 4096 bytes and free of ASCII control characters — the same predicate `ResolveProject`'s boundary check applies — so `relative/path` and `./x` are refused before any filesystem access rather than resolved against the daemon's cwd. The accepted path is canonicalised by the same helper the project reader uses, which also refuses a non-directory and a canonical form that is not UTF-8. An entry whose own name is not UTF-8 is skipped, because this JSON wire cannot carry it and a lossy spelling would be a path the caller could not send back. So is a subdirectory whose joined path fails that same predicate — a control character such as LF, CR or ESC in its name, or a join past 4096 bytes (audit A2) — so every path the reply offers is one the daemon would accept back. That matters beyond this verb: an authoring `StartAgent` (`authoring_kind` present) writes its `cwd` verbatim into the agent's seed, and the daemon refuses one whose `cwd` fails the predicate, before spawning; a plain `StartAgent`'s `cwd` rules are unchanged.

## A point-in-time snapshot

A reply describes the directory as the request saw it, not as it stays. The scan chooses names from one `readdir`, and each chosen name is looked at again with `lstat` when the reply is built, so a child that was swapped for a symlink, replaced by a file or removed between the two is dropped instead of being reported as a real directory. That narrows the window to the moment of that re-check; it does not close it. A change after the re-check, or racing it, is not prevented — the daemon holds nothing open against mutation, and a path in the reply can name a symlink, or nothing, by the time a client sends it back. Descriptor-relative no-follow operations (`openat` with `O_NOFOLLOW` from a held directory handle) would narrow it further and were declined as disproportionate at today's trust boundary: every peer that can send this verb can also send `StartAgent`, which takes its `cwd` as a string rather than a handle, so the consumer of a listed path is in the same position whatever the listing does (see *Threat model*).

## Refusals

A refusal is the ordinary `ok: false` with an `error` string, and apart from `busy` (below) it reuses the project verbs' codes rather than adding new ones. A malformed path answers `invalid-path: …`. Anything that goes wrong once the filesystem is consulted — no such path, not a directory, not readable, a non-UTF-8 canonical form — answers `unresolved:` followed by one fixed sentence that names no path and carries no OS error, so the reply does not tell a missing path from a regular file. That is the shape `ResolveProject` gives an arbitrary caller-supplied path; the sentence differs only in its noun, because this verb resolves a directory rather than a project. As with that verb, no timing property is claimed: the cases do observably different amounts of work.

One refusal is about the daemon rather than the request: `busy: …` means the new-agent query pool was full, nothing was read, and the same request sent again once an earlier one finishes is answered normally. `NewAgentOptions` returns it too, under the same code and sentence.

## Threat model

**What a caller learns that it could not learn before: nothing, for any caller that can reach the attach endpoint today.** The same endpoint accepts `StartAgent` with an arbitrary command and working directory, executed as the daemon's user — see the trust-boundary note on `AttachRequest::StartAgent`. A caller that can send `ListDirectories` can already start `ls -la` anywhere that user can read and receive its output over `AttachStream`. The verb adds a **structured, bounded** route to information the endpoint already exposes. It adds no authority.

That is the honest answer to PRD #819's "resolve-only, never list" bound. That bound constrained the project verbs, whose purpose was naming projects, and it was never what stopped enumeration — `StartAgent` was always the larger capability on the same wire. `ResolveProject` itself is unchanged and stays resolve-only; its doc comment now points here.

**It is not a reason to leave the verb unbounded.** #819's audit recorded that bounding the project verbs is not a substitute for authentication if PRD #741 ever admits a peer with less than full account authority, and the same holds here. The bounds above exist for robustness — a huge or slow directory degrades to a partial listing rather than a stalled request — and so that the verb's surface is already small when that day comes. **If such a peer is ever admitted, this verb must be re-examined alongside `StartAgent`, not after it.**

## Cross-version

The verb is on CLAUDE.md rule 18's capability-gated rung, so it contributes no `PROTOCOL_VERSION` bump. The daemon advertises `list-directories` in `DAEMON_CAPABILITIES` on every platform (the dispatch arm is not `#[cfg]`-gated), and `DaemonClient::list_directories` checks the advertised set itself and returns `GatedQuery::Unsupported` **without sending the request** when it is absent — including against a daemon that advertises no capabilities at all. No call site needs its own check. A sender that skips the client library and sends the frame to an older daemon gets that daemon's generic `malformed request: …` refusal and no listing, which fails closed. No existing field changes meaning: the reply's `directories` field is additive and optional and appears on no other response.

The sibling query `NewAgentOptions` (PRD #1223 M2, capability `new-agent-options`, `DaemonClient::new_agent_options`) is gated the same way. It reads no caller-selected path — it reports the daemon host's `DashboardConfig.default_command`, the compiled agent registry and the daemon's experimental flag — so it has no threat model beyond this one. It runs in the pool above, and it reads `config.toml` with `DashboardConfig::load_bounded` rather than the TUI's `load`: the open is non-blocking so a FIFO at the path cannot hold the thread, only a regular file is read (a symlink to one is followed, as `load` follows it), and at most `MAX_DASHBOARD_CONFIG_BYTES` (1 MiB, the project-config reader's cap) — anything else answers with the defaults, so `default_command` is absent, and logs why without echoing the file.

## Tests

- `src/directory_listing.rs` unit tests: sorted one-level listing with canonical paths, hidden and file entries skipped, symlinked entries not listed while a typed symlink resolves to its target, a kept child swapped for a symlink, replaced by a file or removed between the scan and the reply dropped (through a test seam on the internal function, not a production knob), children whose names carry LF, CR, ESC or DEL not listed and never taking a slot under the cap, the project marker's type policy, relative-path refusal, one generic refusal for a missing path and a regular file, the cap keeping the smallest names with `truncated`, an exhausted budget returning a partial listing, the root having no parent, a non-UTF-8 name skipped (Linux), and the wire shape.
- `src/daemon_client.rs`: both queries withheld, and never sent, against a daemon advertising no capabilities and against one advertising everything up to `focus-gained`; both answered by the real dispatch at this build; and, with the new-agent pool saturated, both refused as `busy` at once while `ResolveProject` on the same daemon still resolves.
- `src/new_agent_options.rs`: a full pool refuses the next query without spawning it rather than queueing it, and the permits return when the holders finish; with the daemon-wide pool saturated, `project_resolve::run_bounded` still acquires.
- `src/config.rs`: the bounded config read parses an ordinary file and one behind a symlink, and answers the defaults for a missing, malformed or oversized file and — without blocking — for a FIFO, directly or behind a link.
- `src/daemon_protocol.rs`: both capability strings advertised and equal to their `op`; request and reply wire shapes.
- `src/authoring_seeds.rs`: an authoring start whose `cwd` carries LF, CR, ESC or DEL, or is relative, is refused for every kind without echoing the path; `src/daemon_client.rs`: through the real dispatch, such a start is refused and starts nothing while a plain start with the same `cwd` still starts.
- `tests/e2e_new_agent_queries.rs` (lane 1, headless `daemon serve`): `newagent/browse/001`, `newagent/browse/002` and `newagent/browse/003` (control-byte children not listed) in [`tests/CATALOG.md`](../../tests/CATALOG.md); `tests/e2e_authoring_seed_delivery.rs`: `newagent/authoring/003` (a control-byte `cwd` refuses the authoring start and leaves a plain start unchanged).

What none of them forces is a real time-budget truncation against a live daemon: the budget has no injectable clock at the wire, so the budget path is covered at the function level with an already-expired deadline.
