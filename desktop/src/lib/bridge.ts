import { createFixtureFleet, createFixtureStartedAgent, DEFAULT_PROFILES, FIXTURE_DEFAULT_COMMANDS, FIXTURE_EXPERIMENTAL_DECKS, FIXTURE_HOMES, fixtureAgentRegistry, fixtureDirectoryTree, fixtureProjectOrchestrations, FIXTURE_ROLE_COMMANDS, fixtureVoiceCommands, nextFixtureAgentId, fixtureVoiceHeard, fixtureVoiceScript, fixtureVoiceStatus, fixtureVoiceTranscription, resolveFixtureVoice, type FixtureState } from "../data/fixture";
import { actionErrorFrom, LaunchCleanupError } from "./actionError";
import { agentKey } from "./agentKey";
import { getTerminal } from "./terminalRegistry";
import { applyHandoffEvent, mapDaemonEvent, MAX_LIVE_EVIDENCE } from "./daemonEvents";
import { DISPLAY_LIMITS, displayText } from "./displayText";
import { describeEndpoint } from "./endpoints";
import { ambiguousOrchestrationReason } from "./newAgent";
import { clampZoom, DEFAULT_ZOOM } from "./zoom";
import { UNREPORTED } from "../types";
import type { HandoffEdge,
  AgentSession,
  AgentTarget,
  AgentStatus,
  AgentTab,
  DaemonProjectListing,
  DaemonResolvedProject,
  DeckAction,
  DeckActionResult,
  DeckDirectoryListing,
  DeckFleet,
  DeckSnapshot,
  EvidenceItem,
  NewAgentOptions,
  NewAgentOrchestrations,
  RuntimeMode,
  TerminalChunk,
  WorkflowStage,
} from "../types";

/** Exact DTO returned by the Tauri `desktop_get_snapshot` command. */
export interface DesktopSnapshotDto {
  connection: {
    status: "connected" | "disconnected" | "incompatible";
    /** What the deck is CALLED — `Endpoint::describe()`. A label, never a key. */
    socketPath: string;
    /**
     * What the deck IS — `EndpointIdentity::wire_id()`, an opaque
     * `deck-<16 hex>` token (PRD #742 M5).
     *
     * `daemonId` is derived from THIS and never from `socketPath` again.
     * `socketPath` is `describe()`, which renders neither the remote socket
     * path, the identity file nor the jump host — so two `[[endpoints.remote]]`
     * rows naming two daemons on ONE host described identically, folded into one
     * group, and their agents shared a key. The composite `(daemonId, agentId)`
     * does not save that, because the key component is the collision.
     */
    deckId: string;
    /** `"local"` or `"remote"` — PRD #741 M7. Always present. */
    deckKind: string;
    /** Why Stop and Replace are unavailable; present exactly for a remote deck. */
    localOnlyReason?: string;
    /** Why the app is on the local deck when the stored selection named another. */
    selectionFallback?: string;
    /**
     * Why projects and workflows cannot be started against this deck (PRD #741
     * M8), or absent when they can.
     *
     * Derived daemon-side from the `Hello` reply's ADVERTISED capability set,
     * not from a version digit or a build stamp — which is the point: it answers
     * "does this deck do what I am about to ask it to" rather than "is this deck
     * the same build as me".
     */
    projectActionsReason?: string;
    /** Why the New agent flow cannot start anything on this deck (PRD #1223); absent when it can. */
    newAgentReason?: string;
    error?: string;
    clientProtocolVersion: number;
    serverProtocolVersion?: number;
    clientBuildVersion: string;
    daemonBuildVersion?: string;
    daemonVersion?: string;
    runningAgentCount?: number;
    /**
     * Always emitted by the desktop crate, including as `false`. Optional here
     * only so a DTO literal in a test need not restate the common case; read it
     * as "an override exists", never as "something is wrong".
     */
    buildStampMismatchOnly?: boolean;
  };
  agents: DesktopAgentDto[];
  /*
   * PRD #819 M6: no `projectCwd`. The desktop crate's `desktop_project_cwd()`
   * guessed one from its own compile-time manifest directory and then from the
   * app process's `current_dir()` — an answer about the WRONG machine whenever
   * the daemon is not this one, and silently so. The fallback below therefore
   * loses its third tier; the daemon-reported agent `cwd` it already preferred
   * stays, and the project a launch runs in comes from `desktop_list_projects`
   * / `desktop_resolve_project` instead.
   */
  /**
   * The daemon's registered-schedule revision (issue #887) — a monotonic
   * counter it bumps whenever its registered task set changes, and nothing
   * else. No schedule reaches this app, and none should: there is no schedule
   * surface here.
   *
   * It exists because the daemon seeds its PROJECT list partly from every
   * registered schedule's working directory, so registering a schedule can add
   * a project — and `projectsRevision` in `App.tsx`, built only from what this
   * client can observe, had no way to move in response. Absent from a daemon
   * that reports none, and comparable only against earlier values on the same
   * connection (it counts from 0 on every daemon start).
   */
  scheduleRevision?: number;
  protocolVersion: number;
  source: "daemon";
  /**
   * Every deck the crate is observing, by `connection.deckId`, in observed
   * order with the SELECTED deck first (PRD #742 M5).
   *
   * # What it replaced, and why it had to
   *
   * M4 found there is no event for a deck LEAVING the observed set:
   * `apply_selection` ends the departed deck's watcher, and under `All` ->
   * `local` the resolved deck does not move, so nothing is emitted at all. A
   * bridge that only upserts what arrives would have kept that deck's agents on
   * screen, frozen and looking live. M4 approximated membership by resetting it
   * at `connect()`; this is the exact version, so the departed deck goes on the
   * next arrival from ANY deck rather than on the next handshake.
   *
   * It also carries "which deck is selected" as `fleet[0]` — a question nothing
   * on this stream could answer before, since under `All` every observed deck
   * emits the same shape.
   *
   * Every deck's snapshot carries the same list, so folding one may prune all.
   *
   * Optional here only so a single-deck DTO literal in a test need not restate
   * it — the crate always emits it, and never empty. Read an absent or empty
   * list as "this payload says nothing about membership", which is how
   * `pruneFleet` treats it: it prunes NOTHING rather than clearing the screen.
   */
  fleet?: string[];
  /**
   * The members of {@link fleet} the app cannot connect to (PRD #742 M12) — a
   * configured deck whose socket path is not filled in yet.
   *
   * Such a deck gets no watcher, because there is no address to watch, so it
   * never emits a snapshot of its own and {@link pruneFleet} would never have
   * anything to keep. The crate states it here instead: the row exists, it has
   * no address, and here is what to call it and what to say. That is a fact
   * from the settings document rather than a connection state nobody measured,
   * which is the distinction {@link pruneFleet} refuses to cross on its own.
   *
   * Carried on every snapshot for the same reason `fleet` is, and optional for
   * the same reason: a single-deck DTO literal in a test need not restate it.
   */
  unconfigured?: UnconfiguredDeckDto[];
  /**
   * The members of {@link fleet} the app CONNECTS to, each NAMED (PRD #742
   * M14) — whether or not it has reported yet.
   *
   * A deck joins {@link fleet} when the settings document is applied and emits
   * its own snapshot only once its watcher has established, which for a remote
   * deck is a tunnel, a handshake and a `ListAgents` away. This is what lets a
   * deck in that window be RENDERED rather than merely counted: `fleet` carries
   * `deck-<16 hex>` hashes, and a stored settings row's id is a different value
   * entirely, so there is no id-to-name join on this side to reach for.
   *
   * Every connectable deck is listed, the ones that have already reported
   * included — which of them this bridge has heard from is a question only this
   * bridge can answer, and {@link fleetView} answers it by looking in its own
   * map.
   *
   * Carried on every snapshot and optional for the same reasons {@link fleet}
   * and {@link unconfigured} are. Read an absent field as "this payload says
   * nothing about naming", exactly as {@link adoptUnconfigured} reads an absent
   * `unconfigured`.
   */
  observed?: ObservedDeckDto[];
}

/** One configured-but-unaddressed deck (PRD #742 M12). */
export interface UnconfiguredDeckDto {
  /** The crate's `unconfigured_deck_id` — disjoint from every real `deckId`. */
  deckId: string;
  /** `user@host[:port]`. */
  label: string;
  /** Why there is nothing to show. */
  reason: string;
}

/** One deck the app connects to, named before it has reported (PRD #742 M14). */
export interface ObservedDeckDto {
  /** The crate's `deck_wire_id` — the key this deck's own snapshot arrives under. */
  deckId: string;
  /** A socket path for a local deck, `user@host[:port]` for a remote one. */
  label: string;
  /** Whether this deck runs on this machine; what decides how it is named. */
  deckKind?: "local" | "remote";
}

export interface DesktopAgentDto {
  id: string;
  paneId?: string;
  displayName?: string;
  cwd?: string;
  rows: number;
  cols: number;
  agentType: "claude_code" | "open_code" | "pi" | "codex" | "devin" | "none";
  /**
   * The binary the agent registry says this type runs — `claude`, `opencode`,
   * `pi`, `codex`, `devin` (PRD #745). `agentType` above is the wire IDENTITY
   * and is not a name anybody types: rendering it showed Claude Code as
   * `claude_code`, OpenCode as `open_code`, and `codex` correctly only by
   * coincidence.
   *
   * Absent — the key is omitted, never blank — whenever the DAEMON named no
   * binary: an agent type whose spec has no default command (`none`, which is
   * also the landing spot for a type the daemon's peer has never heard of), a
   * record reporting no type, or a daemon predating the field. Nothing invents
   * one, and since issue #856 nothing derives one either — the value is the
   * daemon's, resolved from the registry of the process that forked the agent.
   */
  cliName?: string;
  status: "running" | "thinking" | "working" | "compacting" | "waiting_for_input" | "idle" | "error" | "unknown";
  activeTool?: { name: string; detail?: string };
  toolCount: number;
  /**
   * `SessionSnapshot.last_user_prompt`, surfaced by the desktop crate in M8.
   * Absent — the key is omitted, never null — when the agent has emitted no
   * prompt, when the record carries no live snapshot, or when the daemon
   * predates the field.
   */
  lastUserPrompt?: string;
  /**
   * `SessionSnapshot.live_target.writable`, projected by the desktop crate into
   * the deck's own vocabulary. Absent when the daemon declared no live target,
   * which is NOT the same as declaring a non-writable one.
   */
  writeLease?: "read" | "write" | "none";
  /**
   * `SessionSnapshot.last_activity_ms` (PRD #745 M9): when the daemon last saw
   * this agent do anything, as epoch milliseconds. Epoch milliseconds and not a
   * formatted string, so the relative wording stays a webview decision — see
   * `displayActivity` in `lib/displayText`, which also owns the clock-skew rule.
   *
   * Absent from a daemon that has no live session for the agent (a restarted
   * daemon has none at all) and from one that predates the field. A TYPE
   * ASSERTION, not a validated value: the DTO cannot stop a malformed daemon
   * sending a non-finite or out-of-range number, which is why the render seam
   * checks rather than trusts.
   */
  lastActivityMs?: number;
  /**
   * `AgentRecord.spawned_at_ms` (PRD #745 M11): when the daemon forked this
   * agent's process, as epoch milliseconds. Epoch milliseconds and not a
   * formatted string for the same reason `lastActivityMs` is — see
   * `displayUptime` in `lib/displayText`, which owns the wording and shares the
   * clock-skew rule.
   *
   * It comes off the registry RECORD rather than a live session, so unlike
   * `lastActivityMs` it is present for an agent that has never emitted a hook
   * event. Absent from a daemon that did not spawn the agent (an id-only
   * `ListAgents` reply) and from one that predates the field. A TYPE ASSERTION,
   * not a validated value, exactly like `lastActivityMs`: the render seam
   * checks rather than trusts.
   */
  spawnedAtMs?: number;
  /**
   * The desktop crate's `DesktopTab` is structurally identical to the app
   * model's `AgentTab`, so the DTO reuses it and `agentFromDto` copies the
   * value through rather than flattening it to a role string. If the IPC shape
   * ever diverges from the model, this is where the mapping function goes.
   */
  tab: AgentTab;
}

/** Result returned after the ordered Tauri output channel is registered. */
export interface TerminalAttachResult {
  sessionId: string;
  agentId: string;
  generation: number;
  reused: boolean;
  /**
   * PRD #882 — the geometry the daemon has APPLIED for this agent: the smallest
   * viewport among every client attached to it, which is not necessarily the
   * size this tile asked for.
   *
   * The tile sizes its xterm grid from this rather than from `FitAddon.fit()`.
   * Absent against a daemon predating the policy, and on a reused session (whose
   * grid is already sized) — in both cases the tile's own fit stands.
   */
  appliedRows?: number;
  appliedCols?: number;
}

/**
 * PRD #882 — payload of `desktop://terminal-geometry`: the daemon changed this
 * agent's applied geometry, because another client attached, detached or
 * resized it.
 *
 * A tile cannot learn this by asking — it only ever hears about its own
 * requests — so without the push it would keep parsing the agent's bytes at its
 * own geometry while the PTY sits at somebody else's, which is the mis-parse
 * PRD #104 exists to prevent.
 */
export interface DesktopTerminalGeometryDto {
  sessionId: string;
  agentId: string;
  generation: number;
  rows: number;
  cols: number;
}

/**
 * The subset of the Tauri `desktop_run_action` reply the frontend reads. The
 * command also returns the refreshed snapshot, which arrives separately on
 * `desktop://snapshot`.
 */
export interface DesktopActionResultDto {
  ok: boolean;
  sendResult?: import("../types").SendResult;
  message?: string;
  /** The agent the action acted on; for `start_agent`, the id the target deck minted. */
  agentId?: string;
}

/**
 * The desktop app's own settings document, exactly as `desktop_get_settings`
 * returns it and `desktop_set_settings` accepts it (PRD #803).
 *
 * The Rust struct in `src-tauri/src/settings.rs` is the source of truth and its
 * serialised shape is pinned by `default_document_shape_is_pinned`. Every key
 * is a single word, so the TOML on disk and the JSON on this wire agree byte
 * for byte.
 */
export interface DesktopSettingsDto {
  version: number;
  appearance: { mode: AppearanceMode };
  /**
   * The configured decks and which one the app is talking to (PRD #741 M6/M7).
   *
   * **Optional, and `undefined` means *unspecified* rather than *empty*.** The
   * Rust field is an `Option<EndpointSettings>` and its `None` is what stops a
   * build whose UI cannot render endpoints from deleting them: the save merges
   * the decoded struct over the document on disk, so a section this side
   * fabricated as `{ remote: [], selection: "local" }` would write that empty
   * table over every remote deck the user had. Never default it — round-trip it
   * or omit it.
   */
  endpoints?: EndpointSettingsDto;
  /**
   * Which backends voice uses, where they are, and how it is activated
   * (PRD #802).
   *
   * **Optional for `endpoints`' reason rather than a weaker version of it.**
   * The Rust field is an `Option<VoiceSettings>` and its `None` is what stops a
   * build whose UI cannot render voice from resetting it: the save merges the
   * decoded struct over the document on disk, so a section this side fabricated
   * from the defaults would write them over the user's choices — on a save
   * triggered by changing the theme. Never default it; round-trip it or omit it.
   *
   * No credential here, in any form. PRD #803's rule is that a secret goes in
   * neither `desktop.toml` nor `localStorage`, and this DTO is written to
   * `localStorage` verbatim by the fixture bridge. A key lives in the OS
   * keychain and is reached through `secretStatus` / `storeSecret` /
   * `forgetSecret` below, none of which can read one back.
   */
  voice?: VoiceSettingsDto;
  /**
   * The window's zoom level as a scale factor, always one of `ZOOM_LEVELS`
   * (PRD #744). A scale factor rather than a percentage because that is the
   * unit `webview.set_zoom` takes, so storage, this wire, the frontend ladder
   * and the platform call are all in one unit.
   */
  zoom: { level: number };
}

/**
 * The `[voice]` section: one mode plus the two stages (PRD #802).
 *
 * `activation` is one of the token arrays below, which mirror the Rust enums in
 * `src-tauri/src/settings.rs` — pinned on that side by
 * `the_voice_tokens_match_the_frontends_copy`, because a token offered here
 * that Rust does not recognise folds to the default and the user's choice
 * silently does not stick.
 *
 * The two stages are nested rather than flattened for the reason Rust's
 * `VoiceSettings` gives: one struct serialises to TOML and to this JSON, so a
 * flattened `speechEndpoint`/`commandsEndpoint` would be two spellings of the
 * same value waiting to drift. Same shape for both, so the panel renders them
 * from one component.
 */
export interface VoiceSettingsDto {
  activation: string;
  intent: VoiceIntentStageDto;
  transcription: VoiceStageDto;
}

/**
 * One voice stage: which backend, where it is, which model to ask it for
 * (PRD #802).
 *
 * References only, never a credential — `endpoint` is a URL whose Rust
 * counterpart (`ServiceUrl`) refuses a `user:password@` authority outright, and
 * `model` is an identifier whose counterpart (`ModelId`) is bounded at 128
 * bytes over a charset with no whitespace and no control bytes. The key those
 * endpoints authenticate with is in the OS keychain and has no field here.
 */
export interface VoiceStageDto {
  backend: string;
  endpoint: string;
  model: string;
}

/**
 * The command stage, which is one voice stage plus an answer ceiling.
 *
 * `max_tokens` is snake_case because every key on this side is the wire's own
 * spelling — the Rust struct serialises to both TOML and this JSON and nothing
 * renames, so a camelCase field here would be a key Rust never reads.
 *
 * **Only the command stage has one.** A transcription is as long as the audio
 * was, so a ceiling on it would bound nothing the user chose; the Rust
 * `TranscriptionSettings` has no such field, and a shared interface carrying it
 * would offer a control on the Speech stage that changes nothing. The number is
 * a ceiling and not a reservation — it costs nothing unless it is used — and
 * Rust refuses one outside 64..=32768 at the document seam, which the settings
 * footer shows.
 */
export interface VoiceIntentStageDto extends VoiceStageDto {
  max_tokens: number;
}

/** The `[endpoints]` section: the remote decks, and which deck is selected. */
export interface EndpointSettingsDto {
  remote: RemoteEndpointDto[];
  /**
   * A `Selection` token: the reserved word `local`, or a row's `id`. A token
   * this build does not recognise resolves to the local deck **and is written
   * back unchanged**, so an older build degrades rather than destroying a newer
   * build's selection.
   */
  selection: string;
}

/**
 * One `[[endpoints.remote]]` row. References only — a host, an optional user, a
 * port, an optional identity-file *path*, an optional jump-host *name* — and
 * never a secret. There is no display name: `RemoteEndpoint::describe()` derives
 * the label from the address, because a free-text label is exactly the arbitrary
 * `String` the settings field-type guard refuses.
 */
export interface RemoteEndpointDto {
  host: string;
  id: string;
  identity?: string;
  jump?: string;
  port: number;
  /**
   * The deck's attach socket path **on the remote host**. Optional because it
   * cannot be derived — OpenSSH expands neither `~` nor an environment variable
   * on the remote side of `-L` — so a row without one is storable and not
   * connectable, and Test connection is what discovers it.
   */
  socket?: string;
  user?: string;
}

/** How the app picks its light/dark palette. What it does is PRD #743's. */
export type AppearanceMode = "system" | "light" | "dark";

/**
 * The `Selection` token that means the local deck, and therefore one of the two
 * words an endpoint id may not be (`LOCAL_SELECTION_TOKEN` in `settings.rs`).
 *
 * The local deck is deliberately **not** a stored row: `Endpoint::local()`
 * resolves it from the platform paths the way every caller did before endpoints
 * existed, so a fresh install has no `[endpoints]` section and still works, and
 * deleting the section gets the local deck back rather than nothing.
 */
export const LOCAL_ENDPOINT_SELECTION = "local";

/**
 * The `Selection` token that means every configured deck at once — PRD #742's
 * fleet — and therefore the other word an endpoint id may not be
 * (`ALL_SELECTION_TOKEN` in `settings.rs`).
 *
 * Reserved on both sides rather than merely recognised here: `all` is a legal
 * id *shape*, so `EndpointId::parse` refuses it case-insensitively and a
 * hand-written `id = "all"` row fails to load. Without that the one stored
 * string would be ambiguous between the fleet and a row somebody named after
 * it.
 */
export const ALL_ENDPOINT_SELECTION = "all";

/** `RemoteEndpoint::DEFAULT_PORT` — what a row with no `port` key means. */
export const DEFAULT_SSH_PORT = 22;

const APPEARANCE_MODES: readonly AppearanceMode[] = ["system", "light", "dark"];

/**
 * How the microphone is started and stopped (PRD #802 M7).
 *
 * One entry today, and that is the truthful shape: PRD #802 ships press-once-
 * to-start / press-once-to-stop and defers hold-to-talk and always-on-with-VAD
 * to D4. Keep identical to `ActivationMode::TOKENS` in
 * `src-tauri/src/settings.rs`.
 */
export const VOICE_ACTIVATION_MODES = ["toggle"] as const;

/**
 * Which WIRE PROTOCOL the command backend speaks (PRD #802 M5, reshaped by its
 * provider work).
 *
 * Commands is API-only, so these name a dialect rather than a transport: both
 * are one HTTPS request to whatever endpoint the user gave, and what differs is
 * the request body and where the answer is read from. Keep identical to
 * `IntentBackend::TOKENS` in `src-tauri/src/settings.rs`.
 *
 * Two agent-CLI backends were here and both were withdrawn, plus the token
 * `remote` that this pair replaced. `opencode` went first, in PRD #802's
 * landed-work security audit: the agent-CLI backend had to run its child with
 * no tools, no hooks, no MCP, no project config and no session on disk, and
 * `opencode run` offers none of those switches. `claude` — the shipped DEFAULT
 * — went with the provider work, because a stage that spends a credential has
 * to let the user choose whose, and a subprocess has no endpoint, model or key
 * to choose. The Rust enum's folding deserializer is why `claude` or `opencode`
 * left in a settings document loads as the default instead of failing;
 * `remote` is mapped explicitly to `anthropic` instead, because it named that
 * API and the key stored for it is that vendor's.
 *
 * **`openai_compatible` is the default**, so one OpenAI key runs both voice
 * stages — Speech's hosted option is OpenAI's too. The order here is the order
 * the select offers, and it is the order the tokens have always had rather
 * than a ranking.
 */
export const VOICE_INTENT_BACKENDS = ["anthropic", "openai_compatible"] as const;

/**
 * Which backend turns speech into text (PRD #802).
 *
 * `local` is the default: a speech container on loopback that takes no key at
 * all, which is what makes voice try-able on the day it ships and keeps the
 * audio on the machine. `off` was here and is gone — a setting whose whole
 * function was to make the feature do nothing, kept only because transcription
 * was believed to have no keyless route. Keep identical to
 * `TranscriptionBackend::TOKENS` in `src-tauri/src/settings.rs`.
 */
export const VOICE_TRANSCRIPTION_BACKENDS = ["local", "remote"] as const;

/**
 * The bounds and the default for the command stage's answer ceiling.
 *
 * Mirrors `MIN_TOKEN_CEILING`, `MAX_TOKEN_CEILING` and `DEFAULT_TOKEN_CEILING`
 * in `src-tauri/src/model_service.rs`, pinned there by
 * `the_voice_presets_match_the_frontends_copy` for the reason the endpoints and
 * models are: the panel WRITES this value, so a frontend copy that drifted
 * would put a number into `desktop.toml` that this side then refuses — turning
 * a number field into a save error.
 *
 * The default was 256 and was hardwired, which is the defect the field exists
 * to fix: `max_completion_tokens` counts reasoning tokens, so any model that
 * reasons spent the whole ceiling before writing a character.
 */
export const MIN_TOKEN_CEILING = 64;
export const MAX_TOKEN_CEILING = 32768;
export const DEFAULT_TOKEN_CEILING = 4096;

/**
 * Where each backend lives and which model it is asked for, by stage.
 *
 * Mirrors the `*_ENDPOINT` / `*_MODEL` constants in
 * `src-tauri/src/settings.rs`, pinned there by
 * `the_voice_presets_match_the_frontends_copy`. The panel writes a stage's
 * whole preset when the backend changes, because an endpoint belongs to the
 * backend it names: leaving a loopback URL behind after a switch to a hosted
 * service is a setting that cannot work and does not say so.
 */
export const VOICE_STAGE_PRESETS: {
  intent: Record<string, VoiceIntentStageDto>;
  transcription: Record<string, VoiceStageDto>;
} = {
  transcription: {
    local: {
      backend: "local",
      endpoint: "http://127.0.0.1:18000/v1/audio/transcriptions",
      model: "Systran/faster-whisper-tiny.en",
    },
    remote: {
      backend: "remote",
      endpoint: "https://api.openai.com/v1/audio/transcriptions",
      model: "whisper-1",
    },
  },
  intent: {
    anthropic: {
      backend: "anthropic",
      endpoint: "https://api.anthropic.com/v1/messages",
      model: "claude-haiku-4-5",
      max_tokens: DEFAULT_TOKEN_CEILING,
    },
    openai_compatible: {
      backend: "openai_compatible",
      endpoint: "https://api.openai.com/v1/chat/completions",
      model: "gpt-5-mini",
      max_tokens: DEFAULT_TOKEN_CEILING,
    },
  },
};

/**
 * The container the Voice panel tells a user to start for keyless speech.
 *
 * Mirrors `LOCAL_SPEECH_IMAGE` in `src-tauri/src/settings.rs`, pinned there by
 * `the_voice_presets_match_the_frontends_copy`. Pinned to a tag rather than
 * `latest` for the reason any instruction in a product is pinned: the words
 * have to keep working after the upstream tag moves.
 */
export const LOCAL_SPEECH_IMAGE = "ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu";

/** Mirrors `VoiceSettings::default()`; what an absent section renders as. */
export const DEFAULT_VOICE_SETTINGS: VoiceSettingsDto = {
  activation: "toggle",
  intent: VOICE_STAGE_PRESETS.intent.openai_compatible,
  transcription: VOICE_STAGE_PRESETS.transcription.local,
};

/**
 * Which credential the app is being asked about (PRD #802 M4).
 *
 * Keep identical to `SecretId::ALL` in `src-tauri/src/secrets.rs`: these are
 * keychain account names, so a token this side invents names an entry nothing
 * over there reads.
 */
export const VOICE_SECRET_IDS = ["voice-intent", "voice-transcription"] as const;

export type VoiceSecretId = (typeof VOICE_SECRET_IDS)[number];

/**
 * What the app knows about one stored credential — a boolean, and never the
 * value (PRD #802 M4).
 *
 * `problem` is `Some` when the answer is "I could not find out", which is NOT
 * the same as "nothing is stored" and must not render as it: a panel that
 * showed *No key stored* for an unreachable keychain would invite the user to
 * type their key again into a store that cannot hold it.
 */
export interface SecretStatusDto {
  stored: boolean;
  problem?: string;
}

/**
 * Which top-level surface a voice command would run against (PRD #802 M6).
 *
 * The three `DeckView` kinds, and the same closed set `commands.toml`'s
 * `screens` column draws from — pinned on the Rust side as `voice::Screen` and
 * by `xtask/linkage-check`'s rule 13, which reads the union in `types.ts` so the
 * table and the app's own type stay in step without a second list.
 */
export type VoiceScreen = "deck" | "overview" | "agent";

/**
 * What the New agent dialog's directory browser is showing, declared with an
 * utterance (PRD #1223) — `voice::VoiceDirectories`, and the set a spoken
 * `dir_ref` resolves against.
 *
 * Declared by the webview for the screen's reason: it is `NewAgentDialog`'s
 * component state and lives nowhere else — the deck lists one level per
 * request and keeps none of them. Absent whenever there is nothing on screen to
 * name: the dialog closed, no deck chosen, no listing landed, or a start in
 * flight. `entries` are the children ON SCREEN, after the filter. Every path is
 * one the deck returned.
 */
export interface VoiceDirectoriesDto {
  deckId: string;
  path: string;
  hasParent: boolean;
  entries: { name: string; path: string }[];
}

/**
 * What the New agent dialog shows BESIDES its browser, declared with an
 * utterance while the dialog is open (PRD #1223) — `voice::VoiceNewAgent`.
 *
 * `form` is present only while the form's fields are live — a deck and a
 * directory chosen, no start in flight, and no start confirmation open — and
 * carries the Mode chips and Agent picker entries AS OFFERED: they vary by the
 * deck's capabilities, its experimental flag and whether the directory is a
 * project, and a spoken `mode_ref` or `agent_type_ref` resolves against these
 * and nothing else.
 */
export interface VoiceNewAgentDto {
  form?: {
    deckId: string;
    path: string;
    modes: { id: string; label: string }[];
    agentTypes: { id: string; label: string }[];
  };
}

/**
 * One param of a resolved command, as the Rust side resolved it
 * (`voice::ResolvedParam`).
 *
 * `spoken` is what the MODEL supplied and `value` is what the action is
 * dispatched with. The `kind`s resolve against different things, and the
 * difference is worth knowing before reading either field:
 *
 * * `agent_ref` resolves against **live state** — `spoken` is what the user
 *   called an agent, `value` is that agent's id, and `label` is the name the
 *   deck shows for it.
 * * `deck_ref` resolves against **the observed fleet** (PRD #1223) — `spoken`
 *   is what the user called a deck, `value` is that deck's `deckId`, and
 *   `label` is what the overview calls it ("Local deck", or `user@host`).
 * * `dir_ref` resolves against **the directory browser's children on screen**
 *   ({@link VoiceDirectoriesDto}, PRD #1223) — `spoken` is what the user called
 *   one, `value` is the deck's own path for it, and `label` its `displayName`.
 * * `mode_ref` and `agent_type_ref` resolve against **the New agent form's
 *   Mode chips and Agent picker as offered** ({@link VoiceNewAgentDto}, PRD
 *   #1223) — `value` is the chip's or entry's id, `label` what it shows.
 * * `spoken_prefix` resolves against **the transcript** — `spoken` is the
 *   boundary the model marked, the words that introduced a dictation, and
 *   `value` is what the app resolved that boundary to: the rest of the
 *   transcript, verbatim. A boundary that is not genuinely the front of the
 *   transcript never becomes a dispatch at all (`param_unresolved` instead), so
 *   a `value` of this kind is the user's own words or nothing. The model never
 *   supplies text that reaches an agent's prompt.
 *
 * The surface renders neither `spoken` nor `value` as prose: the sentence it
 * shows already names what it needs to, which is what `label` was derived for.
 */
export interface VoiceResolvedParamDto {
  name: string;
  kind: string;
  spoken: string;
  value: string;
  label: string;
}

/**
 * The closed set of situations one utterance can end in (`voice::VoiceOutcome`).
 *
 * **Every variant carries its own `sentence`, and that sentence is the only
 * thing the surface prints.** The panel never composes wording from `kind`,
 * `action`, `param` or `matches`: Rust renders each situation once, from the
 * table, so two surfaces cannot phrase the same situation differently. The
 * other fields are here because the shape is the wire's, not because the panel
 * reads them — `dispatch` is the one variant it does read, for `invoke` and
 * `params`.
 */
export type VoiceOutcomeDto =
  | { kind: "dispatch"; transcript: string; action: string; invoke: string; params: VoiceResolvedParamDto[]; sentence: string }
  | { kind: "unavailable"; transcript: string; action: string; hint: string; sentence: string }
  | { kind: "no_match"; transcript: string; sentence: string }
  | { kind: "unknown_action"; transcript: string; action: string; sentence: string }
  | { kind: "param_missing"; transcript: string; action: string; param: string; sentence: string }
  | { kind: "param_unresolved"; transcript: string; action: string; param: string; spoken: string; sentence: string }
  | { kind: "param_ambiguous"; transcript: string; action: string; param: string; spoken: string; matches: string[]; sentence: string }
  | { kind: "resolution_failed"; transcript: string; detail: string; sentence: string }
  | { kind: "transcription_failed"; detail: string; sentence: string };

/**
 * One utterance's outcome, plus what it cost (`voice::VoiceResult`).
 *
 * `resolveMs` is `null` when no backend was called — silence short-circuits
 * before the call — and the surface renders **no** timing for it rather than
 * `0 ms`, which would claim a measurement nobody took. `backend` is present
 * either way, because it names what *would* have answered.
 */
export interface VoiceResultDto {
  outcome: VoiceOutcomeDto;
  resolveMs: number | null;
  backend: string;
}

/** What the microphone is doing (`voice::CaptureState`). */
export type VoiceCaptureState = "idle" | "recording" | "transcribing" | "done" | "failed";

/**
 * What the webview is told about the microphone (`lib.rs`'s `VoiceStatus`).
 *
 * `available` is **always true from the Tauri app** since `Speech = off` went:
 * every speech backend it ships can run, so no settings document can turn the
 * stage off, and *nothing is set up* is reported where it bites instead — as a
 * `not_configured` transcription outcome naming the container to start or the
 * key to paste. Two other runtimes still answer `false`: one with no microphone
 * verbs at all, and the browser fixture, which has no Rust side to transcribe
 * with. The panel renders the Voice button either way — neither hidden nor
 * disabled, because a control that is not there says nothing and a greyed-out
 * one reads as a fault. What `false` changes is what the press does: it reports
 * `VOICE_UNAVAILABLE`, naming Settings → Voice, rather than turning voice on.
 *
 * `capped` is why the panel polls this between a start and a stop. From the
 * user's side the microphone simply stopped, and a surface that did not know
 * why would go on rendering *listening…* over a closed device.
 */
/**
 * One command as the discovery overlay lists it (PRD #802 D7).
 *
 * The **same** shape `desktop_voice_commands` hands the intent backend —
 * `voice::AnnotatedCommand`, field for field — because the overlay's
 * requirement is that it be generated from the table rather than maintained
 * beside it. A prettier projection for the webview would be that maintained
 * list under a better name, and the wordings would part company the first time
 * a row changed.
 *
 * `unavailable_hint` keeps its Rust spelling for the same reason: this struct
 * carries no `rename_all`, so what arrives is what Rust sends. Renaming it here
 * would be a translation layer with one member and one job, which is a place
 * for a mistake to live.
 *
 * So `description` is a PROMPT. It is written for a model, and it reads as one;
 * the trade is stated at `desktop_voice_commands` and at `commands.toml`'s own
 * column.
 */
export interface VoiceCommandDto {
  id: string;
  description: string;
  /** Whether the screen this was asked for can run it. */
  callable: boolean;
  unavailable_hint: string;
  params: { name: string; kind: string }[];
}

export interface VoiceStatusDto {
  state: VoiceCaptureState;
  capturedMs: number;
  maxMs: number;
  capped: boolean;
  /**
   * Whether anybody has SPOKEN since this recording opened — PRD #802's
   * dictation countdown, and the only field on this status that can answer it.
   *
   * `capturedMs` counts audio rather than speech, so it grows in a silent room;
   * `state: "done"` arrives only after the silence hold past the end of a
   * sentence, which for a long one is well after a five-second countdown would
   * have fired. So a surface waiting to send what it has typed reads THIS to
   * know the user is still talking.
   *
   * **Optional because a producer may not report it, and absence means exactly
   * that** — not "nobody is speaking". Both shipped bridges send it: the Tauri
   * one flattens `voice::CaptureStatus`, which carries it, and the browser
   * fixture sets it. A runtime that omits it is one that does no speech
   * detection, and dictation against such a runtime falls back to the timer
   * alone.
   */
  speech?: boolean;
  available: boolean;
  /**
   * Which transcriber would answer — `TranscriptionBackend::as_token`, which
   * is the same vocabulary the document and the Voice panel use.
   *
   * **It was typed `"off" | "remote"`, and `off` has not been on this wire
   * since PRD #802's provider work deleted the variant** — `voice_status` in
   * `lib.rs` reads the settings token, and the settings token is `local` or
   * `remote`. The union therefore named a value nothing could send and omitted
   * the one that is sent by default, which is worse than a plain `string`:
   * `backend === "off"` type-checked and could never be true.
   */
  backend: "local" | "remote";
}

/**
 * The closed set of situations transcribing one utterance can end in
 * (`voice::TranscriptionOutcome`).
 *
 * Four rather than two, and the two additions are the point: `not_configured`
 * is neither a transcript nor a failure, and neither is `silent`. Rendering
 * either as an error is the mistake calling `off` a product statement exists to
 * avoid. A noise ends a segment far more often than a sentence does, and a
 * whisper-family model handed the quiet room that follows answers with its own
 * training artefacts; refusing to call one is what stops the report claiming
 * the user said something they did not.
 *
 * **`silent` carries a `detail` and this comment used to say it carried none,
 * "because there is nothing to diagnose".** PRD #802's product owner met that
 * outcome with a real microphone, about words he had said, and its sentence
 * named no threshold and no measurement — so a quiet input, a short utterance
 * and a bug were indistinguishable from outside the app. The `detail` is the
 * measurement behind the sentence (`voice::transcribe::not_enough_speech`) and
 * is about the AUDIO, never about the device.
 *
 * Only `heard` continues the pipeline. `VoiceControlPanel` branches on that one
 * kind and prints `sentence` for every other, so a fifth situation would render
 * correctly here and cost no resolver call.
 */
export type VoiceTranscriptionOutcomeDto =
  | { kind: "heard"; transcript: string; sentence: string }
  | { kind: "not_configured"; detail: string; sentence: string }
  | { kind: "silent"; detail: string; sentence: string }
  | { kind: "failed"; detail: string; sentence: string };

/**
 * One recording's transcription, plus what it cost
 * (`voice::VoiceTranscription`).
 *
 * The mirror of {@link VoiceResultDto} for the stage in front of it: a user who
 * waited six seconds is owed the split between transcribing and resolving, so
 * each stage reports its own number.
 */
export interface VoiceTranscriptionDto {
  outcome: VoiceTranscriptionOutcomeDto;
  transcribeMs: number | null;
  backend: string;
  audioMs: number;
}

/**
 * Mirrors `DesktopSettings::default()`; used when nothing is stored yet.
 *
 * No `endpoints` key, deliberately: the Rust default is `None`, TOML omits it,
 * and a default `{ remote: [], selection: "local" }` here would be the exact
 * fabrication that deletes a user's decks.
 */
export const DEFAULT_DESKTOP_SETTINGS: DesktopSettingsDto = {
  version: 1,
  appearance: { mode: "system" },
  zoom: { level: DEFAULT_ZOOM },
};

/**
 * The fixture bridge's settings key.
 *
 * Deliberately NOT run through `modeScopedKey`: a theme choice is global, and
 * PRD #803 says so. There is no live-mode counterpart to collide with — the
 * live bridge keeps settings in `desktop.toml` and never reads localStorage —
 * so the key exists only so a `pnpm dev` + `?fixture=1` preview survives a
 * reload.
 */
export const FIXTURE_SETTINGS_KEY = "dot-agent-deck.desktop-settings";

/** The fingerprint of a document that declares no `[endpoints]` section. */
const UNSPECIFIED_ENDPOINTS = "unspecified";

/**
 * The `[endpoints]` section as one comparable string (PRD #742 M4).
 *
 * What it answers is "could this document have changed which decks the app
 * observes", and the whole section is the honest answer to that: the selection
 * decides whether the fleet is one deck or all of them, and each row's address
 * fields decide which deck a row IS — a changed `socket`, `identity` or `jump`
 * names a different daemon or a different route to it, which is exactly the
 * distinction `EndpointIdentity` was introduced for on the Rust side.
 *
 * It is deliberately coarse in one direction: a row edited from one unreachable
 * address to another re-establishes the fleet for nothing. That costs one
 * handshake against a deck the user is actively editing, which is the cheapest
 * possible moment to spend one.
 *
 * `JSON.stringify` over fields this module names in this order, so the answer
 * does not depend on the key order the crate happened to serialise.
 */
function endpointsFingerprint(settings: DesktopSettingsDto): string {
  const endpoints = settings.endpoints;
  // `undefined` is UNSPECIFIED and never empty — see `DesktopSettingsDto`. A
  // document that says nothing about the section left whatever is on disk
  // exactly where it was, so it cannot have changed which decks are observed.
  if (!endpoints) return UNSPECIFIED_ENDPOINTS;
  return JSON.stringify([
    endpoints.selection,
    endpoints.remote.map((row) => [row.id, row.host, row.user, row.port, row.socket, row.identity, row.jump]),
  ]);
}

/**
 * Coerce anything read back from storage into a valid document. An unknown
 * appearance value falls back to the default rather than propagating, matching
 * `AppearanceMode::from_str_lossy` on the Rust side.
 */
export function normalizeDesktopSettings(value: unknown): DesktopSettingsDto {
  const record = typeof value === "object" && value !== null ? value as Record<string, unknown> : {};
  const appearance = typeof record.appearance === "object" && record.appearance !== null
    ? record.appearance as Record<string, unknown>
    : {};
  const mode = APPEARANCE_MODES.find((candidate) => candidate === appearance.mode) ?? DEFAULT_DESKTOP_SETTINGS.appearance.mode;
  // The zoom section is absent from every document written before PRD #744,
  // which is every document on disk today. `clampZoom` answers the default for
  // an absent, corrupt or off-ladder value alike, so there is no separate
  // missing-section branch — and it is the same snapping `ZoomLevel::snap`
  // performs Rust-side, so the two ends cannot disagree about what is stored.
  const zoom = typeof record.zoom === "object" && record.zoom !== null
    ? record.zoom as Record<string, unknown>
    : {};
  return {
    version: typeof record.version === "number" && Number.isFinite(record.version) ? record.version : DEFAULT_DESKTOP_SETTINGS.version,
    appearance: { mode },
    endpoints: normalizeEndpointSettings(record.endpoints),
    voice: normalizeVoiceSettings(record.voice),
    zoom: { level: clampZoom(zoom.level) },
  };
}

/**
 * Coerce the `[voice]` section, **preserving absence** (PRD #802 M4).
 *
 * The same shape as `normalizeEndpointSettings` and for the same reason:
 * `undefined` in, `undefined` out, because `normalizeDesktopSettings` builds a
 * fresh object with a fixed key set and a section this function fabricated
 * would be merged over the user's file by `desktop_set_settings`. A section
 * that IS present is rebuilt field by field — no spread, which
 * `xtask/linkage-check` refuses here for the credential reason the parent has
 * one.
 *
 * An unrecognised token falls back to this build's default rather than
 * propagating, matching the folding deserializers on the Rust side. The cost is
 * the same one those carry and is worth knowing: a token a NEWER build wrote is
 * replaced rather than preserved.
 */
function normalizeVoiceSettings(value: unknown): VoiceSettingsDto | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  const activation = VOICE_ACTIVATION_MODES.find((candidate) => candidate === record.activation)
    ?? DEFAULT_VOICE_SETTINGS.activation;
  return {
    activation,
    intent: normalizeVoiceIntentStage(record.intent),
    transcription: normalizeVoiceStage(record.transcription, VOICE_TRANSCRIPTION_BACKENDS, DEFAULT_VOICE_SETTINGS.transcription, VOICE_STAGE_PRESETS.transcription),
  };
}

/**
 * One stage, rebuilt key by key — never spread, for the parent's reason.
 *
 * An unrecognised backend token falls back to this build's default, matching
 * the folding deserializer Rust-side. The endpoint and the model do **not**
 * fold: they are free-form on this side and are carried through as written,
 * because coercing an endpoint is how a user's own URL silently becomes
 * somebody else's service. Rust refuses an invalid one at the document seam
 * with a diagnostic the settings footer shows, which is the version of that
 * outcome a person can act on.
 *
 * A stage that is absent or not an object — which is what a document written
 * before the stages were nested looks like — becomes the preset for whichever
 * backend ends up chosen, so an older document upgrades rather than failing.
 */
function normalizeVoiceStage(
  value: unknown,
  backends: readonly string[],
  fallback: VoiceStageDto,
  presets: Record<string, VoiceStageDto>,
): VoiceStageDto {
  const record = typeof value === "object" && value !== null ? value as Record<string, unknown> : {};
  const backend = backends.find((candidate) => candidate === record.backend) ?? fallback.backend;
  const preset = presets[backend] ?? fallback;
  return {
    backend,
    endpoint: typeof record.endpoint === "string" && record.endpoint ? record.endpoint : preset.endpoint,
    model: typeof record.model === "string" && record.model ? record.model : preset.model,
  };
}

/**
 * The command stage, which is `normalizeVoiceStage` plus the answer ceiling.
 *
 * Rebuilt key by key rather than spread over the base, for the parent's
 * credential reason: a spread here would carry an undeclared field into the
 * document exactly as one in the parent would.
 *
 * The ceiling is coerced to an integer inside the bounds and otherwise falls
 * back to the chosen backend's preset. That is the opposite of what the
 * endpoint and the model do — they are carried through as written, because
 * coercing an endpoint is how a user's own URL silently becomes somebody
 * else's service. A number has no such hazard: there is no third party for a
 * ceiling to point at, and a value Rust would refuse costs the whole `[voice]`
 * section rather than the one field. Rust is still the boundary; this is the
 * webview declining to send a number it already knows is out of range.
 */
function normalizeVoiceIntentStage(value: unknown): VoiceIntentStageDto {
  const stage = normalizeVoiceStage(
    value,
    VOICE_INTENT_BACKENDS,
    DEFAULT_VOICE_SETTINGS.intent,
    VOICE_STAGE_PRESETS.intent,
  );
  const preset = VOICE_STAGE_PRESETS.intent[stage.backend] ?? DEFAULT_VOICE_SETTINGS.intent;
  const record = typeof value === "object" && value !== null ? value as Record<string, unknown> : {};
  const raw = record.max_tokens;
  const inRange = typeof raw === "number" && Number.isInteger(raw)
    && raw >= MIN_TOKEN_CEILING && raw <= MAX_TOKEN_CEILING;
  return {
    backend: stage.backend,
    endpoint: stage.endpoint,
    model: stage.model,
    max_tokens: inRange ? raw as number : preset.max_tokens,
  };
}

/**
 * Coerce the `[endpoints]` section, **preserving absence** (PRD #741 M7).
 *
 * This is the frontend half of the pin `a_client_that_cannot_render_endpoints_cannot_delete_them`
 * makes Rust-side, and getting it wrong is silent data loss rather than a
 * visible bug. `normalizeDesktopSettings` builds a fresh object with a fixed key
 * set — that fixed set is itself a credential guard, so it is not going away —
 * which means a section this function does not read is gone before any panel
 * spreads the document, and `desktop_set_settings` then merges the decoded
 * struct over the file. Returning `{ remote: [], selection: "local" }` for an
 * absent section would therefore delete every remote deck the user had, on a
 * save triggered by changing the theme.
 *
 * So: `undefined` in, `undefined` out. A section that IS present is rebuilt
 * field by field — no spread, for the same credential-guard reason the parent
 * has none — and a row missing a `host` or an `id` is dropped rather than
 * repaired, because those two are what make a row a row and Rust would refuse
 * the whole document over one of them.
 */
function normalizeEndpointSettings(value: unknown): EndpointSettingsDto | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  const rows = Array.isArray(record.remote) ? record.remote : [];
  const remote: RemoteEndpointDto[] = [];
  for (const entry of rows) {
    const row = normalizeRemoteEndpoint(entry);
    if (row) remote.push(row);
  }
  return {
    remote,
    selection: typeof record.selection === "string" && record.selection ? record.selection : LOCAL_ENDPOINT_SELECTION,
  };
}

/** One row, rebuilt key by key. `undefined` for anything that is not a row. */
function normalizeRemoteEndpoint(value: unknown): RemoteEndpointDto | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  const host = typeof record.host === "string" ? record.host : "";
  const id = typeof record.id === "string" ? record.id : "";
  if (!host || !id) return undefined;
  const optional = (key: string): string | undefined => {
    const raw = record[key];
    return typeof raw === "string" && raw ? raw : undefined;
  };
  return {
    host,
    id,
    identity: optional("identity"),
    jump: optional("jump"),
    port: typeof record.port === "number" && Number.isInteger(record.port) && record.port > 0 && record.port <= 65535 ? record.port : DEFAULT_SSH_PORT,
    socket: optional("socket"),
    user: optional("user"),
  };
}

/**
 * The settings document plus where it lives, as `desktop_get_settings` returns
 * it (PRD #803).
 *
 * `path` is absent in the browser preview, which has no filesystem at all —
 * `FixtureDeckBridge` keeps settings in `localStorage`. The settings surface
 * says so in as many words rather than printing a plausible-looking path for a
 * file that does not exist.
 */
export interface DesktopSettingsSnapshotDto {
  settings: DesktopSettingsDto;
  path?: string;
  /**
   * Why `settings` is this build's defaults rather than the user's document
   * (issue #1072), when it is.
   *
   * A whole sentence, ready to render: what is wrong, where in the file, and
   * that nothing will be written over it meanwhile. Absent means the document
   * loaded — the Rust side omits the key entirely rather than sending an empty
   * string, so there is nothing to compare against.
   *
   * It is a **locator** (`line 3, column 9`) plus fixed prose and never a byte
   * of the document or a filesystem path, which is the same rule the parse
   * diagnostic follows and for the same reason (issue #827): `toml`'s own error
   * echoes the offending value. The path is carried beside it, in `path`.
   *
   * Present also means **saving is refused**: the app will not publish its
   * defaults over a document it could not read.
   */
  problem?: string;
}

/**
 * The twelve states a `Test connection` can end in (PRD #741 M10).
 *
 * Mirrors `EndpointTestState` in `desktop/src-tauri/src/endpoint_test.rs`, which
 * is the authority. Not one of them is "failed": "your ssh config has never seen
 * this host's key" and "the deck over there is not running" and "this build and
 * that daemon disagree about the wire" are three different things for a user to
 * do next.
 */
export type EndpointTestState =
  | "unknown_deck"
  | "ssh_unavailable"
  | "no_remote_socket"
  | "host_unreachable"
  | "host_key_unverified"
  | "auth_failed"
  | "transport_failed"
  | "deck_not_answering"
  | "handshake_refused"
  | "protocol_refused"
  | "contract_differs"
  | "reachable";

/** What `desktop_test_endpoint` reports. Every text field is already scrubbed. */
export interface EndpointTestReportDto {
  endpointId: string;
  deck: string;
  state: EndpointTestState;
  /**
   * Whether `state` means the deck is usable right now. Carried rather than
   * derived here, so the twelve-way classification stays in one place.
   */
  ok: boolean;
  message: string;
  /** A command to run, when there is one — today only the host-key state. */
  remedy?: string;
  /** ssh's own words, when there were any. */
  detail?: string;
  /** A socket path the probe learned. The panel writes it into the row. */
  discoveredSocket?: string;
  /**
   * Whether `forwards` and `knownHosts` are answers or absences. `false` means
   * the one `ssh -G` resolution behind both did not run or could not be read,
   * which is **not** the same claim as "there are none" — so the panel must not
   * render it as one, and must not render it as nothing either.
   */
  disclosureKnown: boolean;
  /**
   * What this deck's tunnel inherits from the user's own ssh config.
   *
   * Complete as of PRD #741 final audit F1: a forward line whose value `ssh -G`
   * printed unquoted — a path with a space is enough — is reported as
   * unreadable rather than dropped, so this is never quietly shorter than the
   * user's config.
   */
  forwards: string[];
  /**
   * Where ssh resolved the host keys it checks this deck against (PRD #741
   * final audit F2). The tunnel forces the host-key *check* and inherits the
   * *trust anchor*, so this is the only place a user can see which one their
   * config chose.
   *
   * Additive context, never a claim: empty means ssh named no source, and the
   * panel renders nothing rather than asserting there is none.
   */
  knownHosts: string[];
  clientProtocolVersion: number;
  serverProtocolVersion?: number;
  clientBuildVersion: string;
  daemonBuildVersion?: string;
  runningAgentCount?: number;
}

/** Coerce a `desktop_get_settings` reply into a valid snapshot. */
export function normalizeDesktopSettingsSnapshot(value: unknown): DesktopSettingsSnapshotDto {
  const record = typeof value === "object" && value !== null ? value as Record<string, unknown> : {};
  return {
    settings: normalizeDesktopSettings(record.settings),
    path: typeof record.path === "string" && record.path ? record.path : undefined,
    problem: typeof record.problem === "string" && record.problem ? record.problem : undefined,
  };
}

/** Low-volume lifecycle payload emitted as `desktop://terminal-state`. */
export interface DesktopTerminalStateDto {
  sessionId: string;
  agentId: string;
  generation: number;
  state: "attached" | "end" | "error";
  message?: string;
}

/** Exact tagged payload accepted by the Tauri `desktop_run_action` command. */
export type DesktopRunActionDto =
  | { type: "refresh" }
  | { type: "bootstrap"; startIfMissing?: boolean }
  | { type: "start_agent"; deckId: string; command?: string; cwd?: string; displayName?: string; rows?: number; cols?: number; authoringKind?: "schedule" | "schedule-issues" | "dispatcher" }
  | { type: "start_orchestration"; deckId: string; path: string; orchestration: string; displayTitle?: string; configRevision?: string; rows?: number; cols?: number }
  | { type: "stop_agent"; deckId: string; agentId: string }
  | { type: "stop_orchestration"; deckId: string; roles: { agentId: string; name: string }[] }
  | { type: "rename_agent"; agentId: string; displayName: string }
  | { type: "attach_terminal"; agentId: string; onOutput: import("@tauri-apps/api/core").Channel<ArrayBuffer> }
  | { type: "detach_terminal"; sessionId: string }
  | { type: "submit_text"; agentId: string; text: string }
  | { type: "start_workflow"; name: string; cwd: string; taskPrompt: string; roles: { role: string; command: string; start: boolean }[]; rows?: number; cols?: number; configRevision?: string }
  | { type: "stop_daemon"; force?: boolean }
  | { type: "restart_daemon" }
  | { type: "allow_build_mismatch" };

/**
 * PRD #742 M4: the listener takes the WHOLE fleet, never one deck's snapshot.
 *
 * It used to take one, and that was the shape the last-wins flicker came out
 * of: with `Selection::All` the desktop crate runs one watcher per observed
 * deck, each coalescing on its own 150 ms window, so N snapshots arrive per
 * window and a single-snapshot listener renders whichever landed last. The
 * bridge folds them into a fleet keyed by deck instead, and hands the listener
 * the fold — so an arriving snapshot updates ITS deck and leaves the others
 * exactly where they were.
 */
type FleetListener = (fleet: DeckFleet) => void;
type TerminalListener = (event: TerminalChunk) => void;
type Unsubscribe = () => void;

interface PendingTerminalAttachment {
  lifecycle: number;
  channel: import("@tauri-apps/api/core").Channel<ArrayBuffer>;
  output: Uint8Array[];
  stateEvents: DesktopTerminalStateDto[];
  session?: TerminalAttachResult;
  activated: boolean;
}

export interface DeckBridge {
  readonly mode: RuntimeMode;
  /**
   * Establish the fleet and answer every deck the app is observing, SELECTED
   * DECK FIRST (PRD #742 M4).
   *
   * It answered one `DeckSnapshot` until M4, which is where "one deck" was
   * baked into the CONTRACT rather than merely into the data — a bridge whose
   * only snapshot verb returns one deck cannot express a fleet however
   * multi-deck the wire underneath it becomes.
   *
   * **It also RESETS fleet membership**, and that is the half worth knowing
   * before calling it. The desktop crate pushes a per-deck snapshot but no
   * "this deck left the fleet" event, so the set of decks the bridge knows
   * about is the set seen since the last connect. Everything that can change
   * which decks are observed therefore goes through here: app start, Reconnect,
   * and a settings save that touched `[endpoints]` (see `saveSettings`).
   */
  connect(): Promise<DeckFleet>;
  subscribe(onFleet: FleetListener, onTerminal: TerminalListener): Promise<Unsubscribe>;
  runAction(action: DeckAction): Promise<DeckActionResult>;
  /**
   * Type into one agent's terminal, named by the composite `(deckId, agentId)`.
   *
   * PRD #1105's cross-deck pane. It took a bare `agentId` and resolved the
   * frontend session keyed by it, which under two attached same-id agents
   * routed the keystrokes to whichever deck's session happened to be in the
   * map — the write half of issue
   * [#1116](https://github.com/vfarcic/dot-agent-deck/issues/1116)'s open item
   * 2. The target is matched BY VALUE (see {@link AgentTarget}); an
   * unattached target rejects rather than writing anywhere.
   */
  sendTerminalInput(target: AgentTarget, data: string): Promise<void>;
  /** This pane's measured grid, for the agent named by the composite identity. */
  resizeTerminal(target: AgentTarget, cols: number, rows: number): Promise<void>;
  /**
   * PRD #882 — subscribe to the geometry the daemon has APPLIED for an agent,
   * replaying anything already known, and return an unsubscribe function.
   *
   * The replay matters: an agent can be constrained by another client long
   * before a tile here mounts, and the push that said so is not repeated.
   *
   * `deckId` is the deck the SESSION THIS GEOMETRY BELONGS TO was created
   * against — never whichever deck is selected when the event lands (PRD
   * #1105's security audit, and issue #1116's open item 4). Agent ids collide
   * across decks, so a listener that caches by bare id applies one machine's
   * grid to another's namesake, and a stamp read from mutable current selection
   * defeats a composite cache with a confidently wrong producer.
   */
  onTerminalGeometry(listener: (agentId: string, rows: number, cols: number, deckId?: string) => void): () => void;
  /**
   * Scale the whole window, terminals included (PRD #744).
   *
   * Applying only, never persisting — the level is written through
   * `saveSettings` behind a coalescer, because a held key would otherwise
   * rewrite `desktop.toml` once per key repeat. Resolves to the level actually
   * applied, i.e. the caller's level snapped to the ladder.
   *
   * The seam is on `DeckBridge` rather than on `TauriDeckBridge` alone so that
   * fixture mode's no-op is structural: `FixtureDeckBridge` never imports
   * `@tauri-apps/api/core`, so a browser preview cannot reach a webview even by
   * mistake.
   */
  setZoom(level: number): Promise<number>;
  /** The desktop app's own settings document, and where it lives (PRD #803). Never rejects. */
  getSettings(): Promise<DesktopSettingsSnapshotDto>;
  /**
   * Persist the whole document and resolve with what was written.
   *
   * PRD #742 M4: a write that changed the `[endpoints]` section also
   * **re-establishes the fleet**, because that section is the only thing that
   * decides which decks are observed and the crate emits no membership event a
   * listener could prune from. A theme save changes no deck and takes no such
   * path.
   */
  saveSettings(settings: DesktopSettingsDto): Promise<DesktopSettingsDto>;
  /**
   * Test one deck end to end and resolve with a **named state** (PRD #741 M10).
   *
   * `selection` is a `Selection` token: `local`, or a row's `id`. The whole
   * document goes with it because the row a user is testing is usually one they
   * have just typed — reading the file instead would test the previous value.
   *
   * Never rejects for a deck that failed: every outcome is a report. It rejects
   * only when the call itself could not be made.
   */
  testEndpoint(settings: DesktopSettingsDto, selection: string): Promise<EndpointTestReportDto>;
  /**
   * Whether a credential is stored under `id`, without reading it (PRD #802 M4).
   *
   * Never rejects for "I could not find out": that comes back as
   * `problem`, because it is a different answer from "nothing is stored" and a
   * panel has to be able to tell them apart.
   */
  secretStatus(id: VoiceSecretId): Promise<SecretStatusDto>;
  /**
   * Replace the credential stored under `id`, resolving with the new status.
   *
   * **Rejects when the store failed.** The outcome PRD #802 designs against is a
   * user who thinks their key is stored and finds voice broken tomorrow, so a
   * failure is never a resolved promise carrying `stored: false` — the caller's
   * `catch` is what shows the sentence.
   */
  storeSecret(id: VoiceSecretId, secret: string): Promise<SecretStatusDto>;
  /** Forget the credential stored under `id`. Rejects when the store failed. */
  forgetSecret(id: VoiceSecretId): Promise<SecretStatusDto>;
  /**
   * State which screen is mounted, so the next {@link resolveVoice} is
   * validated against it (PRD #802 M6).
   *
   * **It is a declaration rather than an argument of `resolveVoice`, and that is
   * the whole shape of the resolve seam.** An utterance is resolved against the
   * live state the app already holds, and every piece of that state is read
   * where it already lives: the agent list Rust-side from the deck's own
   * snapshot, the command table from the binary it was embedded in. The mounted
   * screen is the one piece that lives ONLY in the webview — it is React state,
   * a `useState<DeckView>` in `DeckShell` — so it is the one piece the webview
   * has to state. `resolveVoice` then takes the utterance and nothing else.
   *
   * Synchronous, and called immediately before each resolve rather than from an
   * effect: a declaration that lagged a navigation would validate the next
   * utterance against the screen the user just left, which is exactly the
   * `unavailable` outcome misfiring.
   *
   * `directories` is the second piece that lives only in the webview (PRD
   * #1223): what the New agent dialog's directory browser is showing, or
   * `undefined` when it is showing nothing. It rides the same declaration for
   * the same reason, and is what makes the directory rows callable at all.
   * `newAgent` is the third: the New agent dialog's form, present while the
   * dialog is open ({@link VoiceNewAgentDto}).
   */
  declareVoiceScreen(screen: VoiceScreen, directories?: VoiceDirectoriesDto, newAgent?: VoiceNewAgentDto): void;
  /**
   * Take one utterance — transcribed from the microphone — to an outcome
   * carrying the sentence to show (PRD #802 M6).
   *
   * **Plain text in, outcome out, and nothing about a microphone in the
   * signature.** A transcript from {@link voiceStop} goes in here as a string
   * like any other, which is what lets the fixture bridge and the vitest suites
   * drive the whole resolve path with no device anywhere near them. Since the
   * M6 rewrite {@link voiceStop} is the only producer of one — the panel has no
   * typed box — but this call does not know that and does not need to.
   *
   * Never rejects for a refusal: an action outside the table, an action this
   * screen cannot run, a param that resolves to nothing — each is a classified
   * outcome with its own rendered sentence, because each is something the user
   * has to be told rather than a fault of the call. It rejects only when the
   * call itself could not be made.
   */
  resolveVoice(utterance: string): Promise<VoiceResultDto>;
  /**
   * Every row of the command table, annotated for `screen`
   * (`desktop_voice_commands`).
   *
   * The screen is an ARGUMENT here where {@link resolveVoice} takes it as a
   * separate declaration, and the difference is deliberate: the declaration
   * exists so one utterance is judged against exactly the screen it was
   * declared with, which is a property of a pipeline. This is a query with no
   * pipeline behind it and no round trip to order against, so the parameter is
   * simply the honest shape.
   *
   * Reaches no daemon, no model and no device: the table is compiled into the
   * binary. Safe to ask every time the overlay opens, which is what keeps it
   * from being cached into something that can go stale.
   *
   * `directories` is what the directory browser shows, when it shows anything
   * (PRD #1223), so the overlay flags the directory rows exactly as a resolve
   * right now would — and `newAgent` likewise for the form rows.
   */
  voiceCommands(screen: VoiceScreen, directories?: VoiceDirectoriesDto, newAgent?: VoiceNewAgentDto): Promise<VoiceCommandDto[]>;
  /**
   * Open the microphone (PRD #802 M7's `desktop_voice_start`).
   *
   * **Rejects with the not-configured sentence when transcription is `off`**,
   * even though {@link voiceStatus} already said so: a user can change the
   * setting between the two calls, so the refusal is where the guarantee is and
   * the status is only what decides whether to offer the control.
   */
  voiceStart(): Promise<VoiceStatusDto>;
  /**
   * Close the microphone and transcribe what it heard
   * (`desktop_voice_stop`).
   *
   * Stop transcribes rather than handing back a buffer, so no audio crosses the
   * IPC boundary — see the Rust command's own note. What arrives here is a
   * transcript and nothing else.
   */
  voiceStop(): Promise<VoiceTranscriptionDto>;
  /**
   * What the microphone is doing, and whether one is offered at all
   * (`desktop_voice_status`).
   *
   * Polled between a start and a stop, because it is the only way the surface
   * learns the length cap ended the recording on its own.
   */
  voiceStatus(): Promise<VoiceStatusDto>;
  /**
   * Abandon a recording without transcribing it (`desktop_voice_cancel`).
   *
   * Idempotent and never refused, because a closed panel, an escape key and a
   * failed start can each arrive in any state and a caller that had to know
   * which one it was in would get it wrong.
   */
  voiceCancel(): Promise<VoiceStatusDto>;
  /**
   * States the WHOLE set of agents whose terminal is on screen right now
   * (PRD #745 M7). Attach follows this and nothing else — not `connect()`, not
   * a snapshot event — because an attach costs one daemon socket and one full
   * scrollback replay per agent, and "renders no output" and "opens no PTYs"
   * are different claims. Declarative, not imperative: the two facts the UI has
   * to state are "these nine tiles are showing a terminal" and "now none is",
   * and neither can be expressed by a per-agent show. Call it once per render
   * commit with every shown target.
   *
   * A target rather than a bare id since PRD #1105's cross-deck pane: the set
   * can name agents on more than one deck at once — a deck screen's tiles on
   * the selected deck, or an overview pane holding a terminal on another — and
   * each attach resolves its own deck's link.
   */
  setShownTerminals(targets: AgentTarget[]): Promise<void>;
  /**
   * The projects the connected daemon knows about (PRD #819 M6). Enumerated
   * from what the daemon already holds — its startup cwd, live agent cwds,
   * orchestration cwds, scheduled working dirs — with every candidate
   * revalidated, and nothing persisted on either side.
   *
   * An empty list is a normal answer. So is a list that shrinks between two
   * calls: enumeration is derived from LIVE state, so a project stops being
   * enumerable the moment its last agent exits.
   */
  listProjects(): Promise<DaemonProjectListing>;
  /**
   * Resolve one path the user pasted or the daemon listed. The reply's `path`
   * is the daemon's canonical spelling and replaces whatever was sent — see
   * `DaemonResolvedProject`.
   */
  resolveProject(path: string): Promise<DaemonResolvedProject>;
  /**
   * PRD #1223 M4 — one directory on the deck `deckId` names, for the New agent
   * dialog: a path that deck listed, or its home directory when `path` is
   * absent. The deck is NAMED rather than read
   * from the selection, for `start_agent`'s reason: under All Decks the
   * selection is the local deck (#1083).
   *
   * Resolves `unsupported` for a deck without the verb. Rejects with the
   * crate's shape refusal for a path that is not absolute, with the
   * deck's refusal for a path it cannot list, with the crate's
   * `DeckScope::resolve` wording for a deck the app no longer observes, or
   * with a connection error for one that stopped answering.
   */
  listDirectories(deckId: string, path?: string): Promise<DeckDirectoryListing>;
  /**
   * PRD #1223 M4 — what the New agent form needs to know about the deck
   * `deckId` names. Resolves `unsupported` for a deck without the query, and
   * rejects exactly as {@link listDirectories} does.
   */
  newAgentOptions(deckId: string): Promise<NewAgentOptions>;
  /**
   * PRD #1223 M6 — the orchestrations the New agent form can offer for `path`
   * on the deck `deckId` names: the project's, `not_project` for an ordinary
   * directory, or `unsupported` with the reason for a deck that cannot launch
   * one from this flow. Rejects exactly as {@link listDirectories} does.
   */
  newAgentOrchestrations(deckId: string, path: string): Promise<NewAgentOrchestrations>;
  dispose(): Promise<void>;
}


/**
 * The crate's `validate_pasted_project_path` refusal (PRD #1223 audit D2),
 * repeated by the fixture wherever the live crate applies it: to a start's
 * `cwd` and to a listing's `path`. The dialog sends only paths a deck returned,
 * so neither is reachable from it; the fixture keeps the refusal so it answers
 * a malformed request the way the crate does.
 */
export const FIXTURE_PASTED_PATH_REFUSAL = "enter an absolute directory path, without control characters, that the deck can see";

/** Whether the crate's `validate_pasted_project_path` would accept `path` on a Unix deck — absolute, and free of ASCII controls. */
function fixtureAcceptsPath(path: string): boolean {
  return path.startsWith("/") && !/[\u0000-\u001f\u007f]/.test(path);
}

/** The sentence the live crate's `newAgentReason` carries for a deck without `list-directories` (PRD #1223 U1), repeated by the fixture's older decks. */
export const FIXTURE_NO_LISTING_REASON = "This deck does not advertise list-directories, so it cannot be browsed for a directory to start in. Start agents on it from the TUI on its host, or upgrade the deck.";

/** What a fixture deck says about a path that names no directory it has, in the daemon's own `unresolved` wording. */
/** The live crate's `CONFIGURED_ROLE_COMMAND_UNSUPPORTED`, repeated by the fixture's older and non-Unix decks (PRD #1223 M6). */
const FIXTURE_CONFIGURED_ROLES_UNSUPPORTED = "This deck cannot start orchestration roles with their configured commands, so its orchestrations are not offered here. Nothing was started. Launch them from the TUI on that deck's host, or upgrade the deck.";

const FIXTURE_UNRESOLVED_REFUSAL = "daemon returned error: unresolved: that path did not resolve to a readable directory on this daemon";

/**
 * The daemon's closed status vocabulary (src-tauri `session_status_name`),
 * mapped exhaustively — PR #416 review B1/M3. The record makes a NEW daemon
 * status a visible fallthrough here instead of a silent one, and the
 * fallthrough itself is "waiting", never a terminal state: the daemon's own
 * `SessionStatus::Unknown` doc says it must be "rendered neutrally (like
 * Idle) so it never masquerades as an active state" — and a status this
 * build has never heard of gets the same treatment, because per PRD #162 a
 * newer daemon can add one without a protocol bump. The old substring
 * matcher sent "unknown" to "stopped", which locked a LIVE agent's terminal
 * read-only.
 */
const DAEMON_STATUS: Record<string, AgentStatus> = {
  // `running` is what the Rust side emits for an agent with no hook state yet
  // — `map_agent` falls back to it when `AgentRecord.live` is absent
  // (`desktop/src-tauri/src/dto.rs`, pinned by
  // `record_without_hook_state_is_still_running`). It is a documented member of
  // `DesktopAgentDto["status"]`, and its absence here sent a live, hookless
  // agent down the unknown-status fallthrough and labelled it "waiting".
  running: "running",
  thinking: "running",
  working: "running",
  compacting: "running",
  waiting_for_input: "waiting",
  idle: "waiting",
  error: "failed",
  unknown: "waiting",
};

function statusFromDaemon(status: string): AgentStatus {
  return DAEMON_STATUS[status.toLowerCase()] ?? "waiting";
}

function roleFromAgent(agent: DesktopAgentDto, index: number): string {
  const value = agent.tab.kind === "orchestration"
    ? agent.tab.roleName
    : agent.agentType.replaceAll("_", " ").trim();
  return value ? value.charAt(0).toUpperCase() + value.slice(1) : `Agent ${index + 1}`;
}

/**
 * The deck's assignment line, as a DISPLAY COPY — sanitised and clamped here
 * rather than at the tile that prints it.
 *
 * PRD #745 M8 made this line carry `lastUserPrompt`, which is free-form,
 * agent-influenceable text bounded only by the daemon's 64 KiB per-prompt
 * ceiling; before that it carried a hardcoded placeholder or a restatement of
 * the active tool. `AgentTile` renders `agent.task` straight into a DOM text
 * node and the deck is the screen the app opens on, so a `U+202E` in a prompt
 * reversed the assignment line — the daemon-side scrub removes category `Cc`
 * and bidi formatting characters are `Cf` — and fifteen agents put about a
 * megabyte of prompt text in the deck's DOM on every refreshed snapshot.
 *
 * Bounding it at the projection rather than at the tile is what makes the
 * property structural: `task` is display-only — nothing sorts, groups or keys
 * on it — so every consumer of it, present and future, gets the bounded copy
 * and no raw daemon text reaches a deck DOM node through this field at all.
 * The raw prompt stays on `lastUserPrompt` for the surfaces that need more of
 * it, each of which passes it through this same seam with its own budget.
 *
 * The active-tool restatement goes through it too, which closes the same hole
 * one field over: a tool detail is the agent's own command line and was never
 * sanitised on this path either.
 */
function taskLine(agent: DesktopAgentDto): string {
  // The daemon's own last user prompt is the honest answer to "what is this
  // agent doing", so it leads. The active-tool restatement is the fallback it
  // always was, and the placeholder is reached only when the daemon reported
  // neither (PRD #745 M8).
  const reported = agent.lastUserPrompt
    ?? (agent.activeTool ? `Active tool: ${agent.activeTool.name}${agent.activeTool.detail ? ` · ${agent.activeTool.detail}` : ""}` : undefined);
  return reported === undefined ? "Task metadata unavailable from the deck" : displayText(reported, DISPLAY_LIMITS.prompt);
}

function agentFromDto(agent: DesktopAgentDto, index: number, daemonId: string): AgentSession {
  const status = statusFromDaemon(agent.status);
  const role = roleFromAgent(agent, index);
  const orchestration = agent.tab.kind === "orchestration" ? agent.tab : undefined;
  return {
    id: agent.id,
    daemonId,
    paneId: agent.paneId,
    role,
    displayName: agent.displayName || role,
    // The BINARY, not the enum (PRD #745). `agentType` is the wire identity —
    // `claude_code`, `open_code` — and nobody types either of those.
    //
    // Issue #856: this is the DAEMON's answer now, resolved from the registry
    // of the process that forked the agent and copied through untouched. It
    // used to be looked up here, in this app's own compiled-in copy of that
    // table — a value the client had no authority over.
    //
    // Absent stays absent, and the `"agent"` word that used to stand in for it
    // is gone with the lookup. A generic word reads as a fact about the agent;
    // an empty cell reads as "the deck did not say", which is what is true. It
    // is also the disposition the uptime and activity columns already take, and
    // — the load-bearing part — it means there is no local table left to fall
    // back to, which is the whole point of the issue.
    cli: agent.cliName,
    model: UNREPORTED,
    status,
    task: taskLine(agent),
    // Absent, not sentinel-encoded. The deck's own stand-in word is a legal
    // working directory (`src/agent_pty.rs` accepts any non-empty, bounded,
    // control-free `cwd`), so writing it here let an agent launched in a
    // directory called "Unavailable" have its real, reported directory erased
    // by `toOverviewAgent`'s reversal. Absence that cannot be spelled by the
    // daemon cannot collide with it; the one surface that still wants a word
    // for it supplies its own at its own render seam (`AgentTile`).
    cwd: agent.cwd,
    // No `attempt`: the daemon has no retry counter, and live mode used to
    // hardcode `1` here, which every tile then printed as `ATT 01` (PRD #745 M8).
    duration: "—",
    tokens: 0,
    cost: 0,
    contextPercent: 0,
    worktree: UNREPORTED,
    // `"unknown"` is this field's absence sentinel, not a third lease state:
    // the daemon declared no live target. `toOverviewAgent` reverses it.
    writeLease: agent.writeLease ?? "unknown",
    lastUserPrompt: agent.lastUserPrompt,
    lastActivityMs: agent.lastActivityMs,
    spawnedAtMs: agent.spawnedAtMs,
    rows: agent.rows,
    cols: agent.cols,
    activeTool: agent.activeTool?.name,
    activeToolDetail: agent.activeTool?.detail,
    toolCount: agent.toolCount,
    transcript: "",
    diff: [],
    checks: [],
    handoffIds: [],
    artifacts: [],
    tab: agent.tab,
    inOrchestration: Boolean(orchestration),
    isStartRole: orchestration?.isStartRole ?? false,
  };
}

/**
 * PR #416 review M1: every persisted-preferences key is scoped by runtime
 * mode. Fixture sessions used to write projects/profiles/prompts under the
 * SAME keys live mode read back — so one fixture visit could hand a real
 * workflow launch a working directory that never existed.
 */
export function modeScopedKey(base: string): string {
  return `${base}.${selectRuntimeMode()}`;
}

/**
 * The message shown when the desktop crate sent none. It always does today, so
 * this is a belt-and-braces path — but it used to hardcode `Protocol mismatch`
 * for EVERY incompatible status, which is wrong for the far more common
 * build-stamp case and would have told a user to look at a protocol version
 * that matched (issue #801). It now says which of the two checks failed, using
 * the same flag the Connect anyway affordance is gated on.
 */
function fallbackConnectionMessage(connection: DesktopSnapshotDto["connection"]): string {
  if (connection.status === "connected") return "Deck responding";
  if (connection.status !== "incompatible") return "Deck unavailable";
  if (connection.buildStampMismatchOnly) {
    return `Build mismatch: desktop is ${connection.clientBuildVersion}, deck is ${connection.daemonBuildVersion ?? "unreported"}.`;
  }
  return `Protocol mismatch: desktop v${connection.clientProtocolVersion}, deck v${connection.serverProtocolVersion ?? "unknown"}`;
}

/**
 * The snapshot a configured deck with no address renders as (PRD #742 M12).
 *
 * Built as a `DesktopSnapshotDto` and mapped through {@link mapDesktopSnapshot}
 * rather than hand-assembled, so this group has exactly the shape every other
 * deck's does and cannot drift from it — the overview's `DeckGroup` then needs
 * no knowledge of this state at all, because "a deck that is not answering" is
 * already what it renders as a degraded group.
 *
 * `disconnected` is the honest status: nothing answered, and nothing was asked.
 * It keeps the deck out of `decksUp` — an unconfigured deck must never count as
 * one that answered — while leaving it in `decks.length`, which is the
 * denominator the header states.
 *
 * Every field here comes from the crate. The rest of the DTO is the minimum
 * `mapDesktopSnapshot` requires, and each value is a statement about a deck
 * that was never contacted: no protocol version was exchanged, no build stamp
 * was reported, and no agent count is known.
 */
export function unconfiguredDeckSnapshot(deck: UnconfiguredDeckDto, clientProtocolVersion: number, clientBuildVersion: string): DeckSnapshot {
  const snapshot = mapDesktopSnapshot({
    connection: {
      status: "disconnected",
      socketPath: deck.label,
      deckId: deck.deckId,
      deckKind: "remote",
      error: deck.reason,
      clientProtocolVersion,
      clientBuildVersion,
    },
    agents: [],
    protocolVersion: clientProtocolVersion,
    source: "daemon",
  });
  // Set after the map rather than carried through the DTO: this is what the
  // BRIDGE knows about an entry it built, not something a deck reported, and
  // `mapDesktopSnapshot` describes decks that answered.
  snapshot.connection.unconfigured = true;
  return snapshot;
}

/**
 * What a deck that has not reported yet says instead of a state (PRD #742 M14).
 *
 * Written on THIS side rather than carried from the crate, unlike
 * `UnconfiguredDeckDto.reason`: the crate does not know which decks the webview
 * has heard from, so "has not reported yet" is a statement only this bridge is
 * in a position to make. It is deliberately calm — there is nothing for the
 * reader to do and nothing has gone wrong — and it names the fleet rather than
 * the connection, because what the reader is being told is why a group is on
 * screen with nothing in it.
 */
export const PENDING_DECK_MESSAGE = "In the fleet, waiting for it to report.";

/**
 * What a fleet member that has NOT REPORTED YET renders as (PRD #742 M14).
 *
 * # The defect it closes is the moving denominator, not the delay
 *
 * `desktop_bootstrap` answers the RESOLVED deck alone. Every other observed
 * deck appears when its own watcher emits, which for a remote deck means
 * acquiring an ssh tunnel, handshaking and running `ListAgents` — bounded by
 * the crate's reconcile interval for a quiet deck and by `FORWARD_READY_TIMEOUT`
 * (30s) for a tunnel that never comes up. So with two decks configured the
 * header read `DECKS 1/1` and then, seconds later, `2/2`: two statements that
 * both read as "everything is fine", with a TOTAL that changed under the
 * reader. A total that moves is worse than one that is merely incomplete, and
 * this makes it `1/2` then `2/2` — the denominator right from the first frame,
 * with only the numerator climbing.
 *
 * # Why it is a state of its own and not `disconnected`
 *
 * `disconnected` asserts a measurement: something was asked and nothing
 * answered. Nothing has been asked of this deck yet. `loading` is the honest
 * status and {@link ConnectionView.pending} is what tells it from the runtime's
 * own pre-connect seed, which is the app having no deck rather than a deck
 * having no snapshot.
 *
 * # Built through the mapper, like its M12 sibling
 *
 * Same reason {@link unconfiguredDeckSnapshot} is: the group then has exactly
 * the shape every other deck's does and cannot drift from it. The two fields
 * the mapper cannot express are set after it — `loading` is not a status the
 * crate can send, and `pending` is something this bridge knows about an entry
 * it built rather than something a deck reported.
 *
 * It keeps the deck out of `decksUp`, which counts decks that ANSWERED, while
 * leaving it in `decks.length`, which is the denominator the header states —
 * and out of every agent count beside it, for the same reason a disconnected
 * deck is out of them: what it is running is unknown, and adding zero for it
 * would be a wrong number that looks exactly like a right one.
 */
export function pendingDeckSnapshot(deck: ObservedDeckDto, clientProtocolVersion: number, clientBuildVersion: string): DeckSnapshot {
  const snapshot = mapDesktopSnapshot({
    connection: {
      /*
        The nearest thing the wire can say — `loading` is this side's and is set
        below. The mapper does read it on the way through, and what it derives
        is right anyway: `health: "idle"` for a deck with nothing to report, and
        `daemonDetected: false`, since no daemon has answered. The one thing it
        would get wrong is the message, and `error` below supplies that.
      */
      status: "disconnected",
      socketPath: deck.label,
      deckId: deck.deckId,
      deckKind: deck.deckKind === "remote" ? "remote" : "local",
      error: PENDING_DECK_MESSAGE,
      clientProtocolVersion,
      clientBuildVersion,
    },
    agents: [],
    protocolVersion: clientProtocolVersion,
    source: "daemon",
  });
  snapshot.connection.status = "loading";
  snapshot.connection.pending = true;
  return snapshot;
}

export function mapDesktopSnapshot(dto: DesktopSnapshotDto, previous?: DeckSnapshot, evidence?: EvidenceItem[], handoffs?: HandoffEdge[]): DeckSnapshot {
  /*
    PRD #742 M5. This was `dto.connection.socketPath` — `Endpoint::describe()`,
    a sentence for a human — and that string is not an identity: it renders
    neither the remote socket path, the identity file nor the jump host, so two
    stored rows naming two daemons on ONE host produced one `daemonId` and one
    group. The crate now carries `EndpointIdentity` itself, and this reads it.

    `socketPath` is still what the UI renders; it just no longer keys anything.
  */
  const daemonId = dto.connection.deckId;
  const agents = dto.agents.map((agent, index) => agentFromDto(agent, index, daemonId));
  // Three tiers since PRD #819 M6, not four. The daemon-reported agent cwd
  // leads, as it always did; the removed tier was the desktop's own guess at a
  // project directory, which is the read this PRD moved daemon-side.
  const cwd = agents.find((agent) => agent.cwd)?.cwd
    ?? (previous?.worktree?.startsWith("/") ? previous.worktree : undefined)
    ?? "No active project";
  const repo = cwd.split("/").filter(Boolean).at(-1) ?? cwd;
  const stages: WorkflowStage[] = agents.map((agent, index) => ({
    id: `agent-${agent.id}`,
    label: agent.role,
    agentId: agent.id,
    status: agent.status === "running" ? "active" : agent.status === "passed" ? "passed" : agent.status === "failed" ? "failed" : "queued",
    // No attempt: it was read straight off the hardcoded per-agent one, so
    // every live node claimed a retry count no daemon tracks (PRD #745 M8).
    enabled: true,
  }));

  return {
    runId: previous?.runId ?? "live-deck",
    repo,
    // No branch: nothing daemon-side tracks one, and the literal "Unavailable"
    // this used to carry was a placeholder the topbar printed as if it were the
    // checked-out branch (PRD #745 M8).
    worktree: cwd,
    // Issue #887: copied through so `projectsRevision` can key on it. Nothing
    // renders it.
    scheduleRevision: dto.scheduleRevision,
    connection: {
      status: dto.connection.status === "incompatible" ? "error" : dto.connection.status,
      deckId: dto.connection.deckId,
      socketPath: dto.connection.socketPath,
      message: dto.connection.error ?? fallbackConnectionMessage(dto.connection),
      daemonDetected: dto.connection.status === "connected" || dto.connection.status === "incompatible",
      runningAgentCount: dto.connection.runningAgentCount,
      buildStampMismatchOnly: dto.connection.buildStampMismatchOnly,
      clientBuildVersion: dto.connection.clientBuildVersion,
      daemonBuildVersion: dto.connection.daemonBuildVersion,
      deckKind: dto.connection.deckKind === "remote" ? "remote" : "local",
      localOnlyReason: dto.connection.localOnlyReason,
      selectionFallback: dto.connection.selectionFallback,
      projectActionsReason: dto.connection.projectActionsReason,
      newAgentReason: dto.connection.newAgentReason,
    },
    health: dto.connection.status === "incompatible" ? "failed" : dto.connection.status === "disconnected" ? "idle" : agents.some((agent) => agent.status === "failed") ? "failed" : "healthy",
    elapsed: previous?.elapsed ?? "—",
    spend: previous?.spend ?? 0,
    currentNode: Math.max(1, agents.findIndex((agent) => agent.status === "running") + 1),
    totalNodes: agents.length,
    // No currentAttempt, for the same reason as the per-agent one.
    paused: false,
    stages,
    agents: agents.map((agent) => {
      const old = previous?.agents.find((candidate) => candidate.id === agent.id);
      return old ? { ...agent, transcript: old.transcript } : agent;
    }),
    evidence: evidence ?? previous?.evidence ?? [],
    handoffs: handoffs ?? previous?.handoffs ?? [],
    profiles: previous?.profiles ?? DEFAULT_PROFILES.map((profile) => ({ ...profile })),
  };
}

/**
 * Every scenario `?state=` accepts. Keeping the accepted values in one list
 * next to `FixtureState` means adding a scenario cannot silently fail to be
 * reachable from the URL — the previous inline `||` chain had to be edited in
 * lockstep with the fixture and was not.
 */
const FIXTURE_STATES: readonly FixtureState[] = ["connected", "crowded", "disconnected", "error", "empty", "fleet"];

class FixtureDeckBridge implements DeckBridge {
  readonly mode = "fixture" as const;
  /**
   * The whole fixture fleet, selected deck first (PRD #742 M4). One entry for
   * every scenario but `fleet`, which is the three-deck one.
   */
  private fleet: DeckFleet;
  private fleetListeners = new Set<FleetListener>();
  private terminalListeners = new Set<TerminalListener>();
  private fixtureStep = 0;
  private settings?: DesktopSettingsDto;
  /**
   * PRD #1223 M4 — the decks this preview plays as OLDER than the PRD: no
   * listing verb and no options query. Since U1 removed the typed path such a
   * deck has no directory step, so its connection carries the crate's
   * `newAgentReason` and the New agent dialog shows it disabled at the deck
   * step. Chosen by `?older=`: `1` or `all` for every deck, otherwise a
   * comma-separated list of fixture deck ids. Empty by default.
   */
  private olderDecks: "all" | ReadonlySet<string> = new Set();
  /**
   * PRD #1223 M6 — the decks this preview plays as built for a non-Unix
   * platform: they list directories and answer the options query, but do not
   * advertise `prepared-role-command`, so they cannot launch an orchestration
   * from this flow. Chosen by `?nonunix=`, a comma-separated list of fixture
   * deck ids. Empty by default.
   */
  private nonUnixDecks: ReadonlySet<string> = new Set();
  /** PRD #1223 M4 — the command each fixture deck last started a plain agent with, as the live crate keeps it: per deck, in memory. */
  private lastCommands = new Map<string, string>();

  /**
   * The selected deck, which is the only one every mutating fixture action
   * touches — `runAction` is the deck screen's, and the deck screen is
   * single-deck (PRD #742 DECISION 1). An accessor rather than a second field
   * so the two can never disagree.
   */
  private get snapshot(): DeckSnapshot {
    return this.fleet[0];
  }

  private set snapshot(value: DeckSnapshot) {
    this.fleet[0] = value;
  }

  constructor() {
    const requestedState = new URLSearchParams(window.location.search).get("state");
    const state = FIXTURE_STATES.find((candidate) => candidate === requestedState) ?? "connected";
    this.fleet = createFixtureFleet(state);
    const older = new URLSearchParams(window.location.search).get("older");
    if (older === "1" || older === "all") this.olderDecks = "all";
    else if (older) this.olderDecks = new Set(older.split(",").map((deckId) => deckId.trim()).filter(Boolean));
    const nonUnix = new URLSearchParams(window.location.search).get("nonunix");
    if (nonUnix) this.nonUnixDecks = new Set(nonUnix.split(",").map((deckId) => deckId.trim()).filter(Boolean));
    this.fleet.forEach((deck) => this.markOlderDeck(deck));
  }

  private isOlderDeck(deckId: string): boolean {
    return this.olderDecks === "all" || this.olderDecks.has(deckId);
  }

  /** Whether `deckId` cannot launch an orchestration from the New agent flow: an older deck, or a non-Unix one. */
  private withholdsConfiguredRoles(deckId: string): boolean {
    return this.isOlderDeck(deckId) || this.nonUnixDecks.has(deckId);
  }

  /** Give a deck this preview plays as older the connection's `newAgentReason`, as the live crate does for a deck without `list-directories`. */
  private markOlderDeck(deck: DeckSnapshot): void {
    const deckId = deck.connection.deckId;
    if (deckId !== undefined && deck.connection.status === "connected" && this.isOlderDeck(deckId)) deck.connection.newAgentReason = FIXTURE_NO_LISTING_REASON;
  }

  /**
   * The fixture half of `DeckScope::resolve` plus a live handshake: a deck the
   * preview does not show is refused in the crate's own wording, and one it
   * shows as unreachable is refused as not connected. Every deck-targeted
   * fixture verb goes through here, so none of them can fall back to the
   * selected deck.
   */
  private connectedDeck(deckId: string): DeckSnapshot {
    const deck = this.fleet.find((candidate) => candidate.connection.deckId === deckId);
    if (!deck) {
      throw new Error(`that deck is not one this app is observing: ${deckId}`);
    }
    if (deck.connection.status !== "connected") {
      throw new Error(`that deck is not connected: ${deckId}`);
    }
    return deck;
  }

  async connect(): Promise<DeckFleet> {
    await Promise.resolve();
    return structuredClone(this.fleet);
  }

  async subscribe(onFleet: FleetListener, onTerminal: TerminalListener): Promise<Unsubscribe> {
    this.fleetListeners.add(onFleet);
    this.terminalListeners.add(onTerminal);
    return () => {
      this.fleetListeners.delete(onFleet);
      this.terminalListeners.delete(onTerminal);
    };
  }

  private emitSnapshot(): void {
    const value = structuredClone(this.fleet);
    this.fleetListeners.forEach((listener) => listener(value));
  }

  async runAction(action: DeckAction): Promise<DeckActionResult> {
    if (action.type === "start_agent") {
      // PRD #1223 M3 — the one fixture action that is NOT the selected deck's:
      // it names its deck, like the live one, so a start from the overview
      // lands on the deck the user picked whichever deck is selected.
      return this.startAgent(action);
    }
    if (action.type === "start_orchestration") return this.startOrchestration(action);
    if (action.type === "stop_agent") return this.stopAgents(action.deckId, [{ agentId: action.agentId, name: action.agentId }]);
    if (action.type === "stop_orchestration") return this.stopAgents(action.deckId, action.roles);
    if (action.type === "pause_run" || action.type === "resume_run") {
      this.snapshot.paused = action.type === "pause_run";
    } else if (action.type === "approve_run") {
      this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === "approve" ? { ...stage, status: "passed" } : stage);
      this.snapshot.health = "healthy";
    } else if (action.type === "retry_stage") {
      // Counting up from an absent attempt would invent one, so a stage with no
      // count keeps none. Every fixture stage has one; live mode has no retry
      // action at all (PRD #745 M8).
      this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === action.stageId ? { ...stage, status: "active", attempt: stage.attempt === undefined ? undefined : stage.attempt + 1 } : stage);
    } else if (action.type === "rename_agent") {
      this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === action.agentId ? { ...agent, displayName: action.displayName } : agent);
    } else if (action.type === "submit_text") {
      this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === action.agentId ? { ...agent, transcript: `${agent.transcript}\r\n> ${action.text}\r\n` } : agent);
      this.terminalListeners.forEach((listener) => listener({ agentId: action.agentId, deckId: this.snapshot.connection.deckId, data: new TextEncoder().encode(`\r\n> ${action.text}\r\n`), stream: "output", operation: "append" }));
    } else if (action.type === "advance_fixture") {
      this.fixtureStep = (this.fixtureStep + 1) % 3;
      if (this.fixtureStep === 1) {
        this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === "review" ? { ...stage, status: "passed" } : stage.id === "test" ? { ...stage, status: "active" } : stage);
        this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === "reviewer" ? { ...agent, status: "passed" } : agent.id === "tester" ? { ...agent, status: "running", transcript: `${agent.transcript}\u001b[36mACTIVE\u001b[0m running browser smoke…\r\n` } : agent);
        this.snapshot.currentNode = 5;
      } else if (this.fixtureStep === 2) {
        this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === "test" ? { ...stage, status: "passed" } : stage.id === "approve" ? { ...stage, status: "waiting" } : stage);
        this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === "tester" ? { ...agent, status: "passed", transcript: `${agent.transcript}\u001b[32mPASS\u001b[0m browser · a11y · PTY fixture\r\n` } : agent);
        this.snapshot.currentNode = 6;
      } else {
        // Only the SELECTED deck is reset: `advance_fixture` is the deck
        // screen's own control, and the deck screen is single-deck.
        this.snapshot = createFixtureFleet("connected")[0];
        this.markOlderDeck(this.snapshot);
      }
    }
    this.emitSnapshot();
    return { ok: true, sendResult: action.type === "submit_text" ? "applied" : undefined };
  }

  /**
   * The fixture half of the deck-targeted start: refuse a deck this preview
   * does not show with the crate's own wording (`DeckScope::resolve`), refuse
   * one that is not connected, and otherwise add the agent to THAT deck's
   * fleet entry and hand back the id it minted — so a spec can wait for
   * `(deckId, agentId)` to appear exactly as the live flow will.
   */
  private startAgent(action: Extract<DeckAction, { type: "start_agent" }>): DeckActionResult {
    // PRD #1223 audit D2: the live crate refuses a relative directory before
    // resolving the deck, in `validate_pasted_project_path`'s sentence; so does the preview.
    if (action.cwd !== undefined && !fixtureAcceptsPath(action.cwd)) throw new Error(FIXTURE_PASTED_PATH_REFUSAL);
    const deck = this.connectedDeck(action.deckId);
    // PRD #1223 M7: a deck this preview plays as older cannot compose a seed,
    // and refuses an authoring start the way the live crate does — before
    // anything is started, in the crate's own sentence.
    if (action.authoringKind && this.isOlderDeck(action.deckId)) {
      throw new Error(`This deck cannot start a \`${action.authoringKind}\` agent: it predates daemon-composed authoring seeds, and would start a plain agent with no seed. Nothing was started. Start it from the TUI on that deck's host, or upgrade the deck.`);
    }
    const agentId = nextFixtureAgentId(deck.agents);
    deck.agents = [
      ...deck.agents,
      createFixtureStartedAgent({
        id: agentId,
        daemonId: action.deckId,
        displayName: action.displayName,
        command: action.command,
        cwd: action.cwd,
        rows: action.rows,
        cols: action.cols,
      }),
    ];
    // PRD #1223 M4: the live crate's rule — recorded once the deck accepted the
    // start, and a blank command (the default shell) never overwrites one. An
    // authoring start records its (resolved) command too.
    if (action.command?.trim()) this.lastCommands.set(action.deckId, action.command);
    this.emitSnapshot();
    return { ok: true, agentId };
  }

  /**
   * PRD #1223 M6 — the fixture half of the deck-targeted orchestration launch.
   * The named deck is resolved as for {@link startAgent}; a deck this preview
   * plays as older refuses in the crate's own sentence, as does a path that is
   * not one of its projects. Otherwise every role of the orchestration joins
   * THAT deck's fleet entry under one orchestration id and the run's title —
   * the orchestration's name when none was given, as the TUI's tab does — and
   * the START role's id comes back, so a spec can wait for it and open its
   * pane exactly as the live flow will.
   */
  /**
   * PRD #1223 U4 — the fixture half of the deck-targeted stop and of the
   * orchestration close. The named deck is resolved as for {@link startAgent};
   * each listed agent leaves THAT deck's fleet entry, as a stopped agent leaves
   * a live deck's agent list. An id the deck does not list is refused, and a
   * close with any refusal rejects as a {@link LaunchCleanupError} naming those
   * roles — the crate's shape — after the rest have stopped.
   */
  private stopAgents(deckId: string, roles: readonly { agentId: string; name: string }[]): DeckActionResult {
    const deck = this.connectedDeck(deckId);
    const listed = new Set(deck.agents.map((agent) => agent.id));
    const refused = roles.filter((role) => !listed.has(role.agentId));
    const stopping = new Set(roles.map((role) => role.agentId));
    deck.agents = deck.agents.filter((agent) => !stopping.has(agent.id));
    this.emitSnapshot();
    if (refused.length > 0) {
      const reasons = refused.map((role) => `${role.name} (${role.agentId}: no such agent)`).join(", ");
      if (roles.length === 1) throw new Error(`daemon returned error: no such agent: ${refused[0].agentId}`);
      throw new LaunchCleanupError(`could not confirm stop for ${refused.length} of ${roles.length} role(s): ${reasons}`, refused.map((role) => role.name));
    }
    return { ok: true, agentId: roles.length === 1 ? roles[0].agentId : undefined };
  }

  private startOrchestration(action: Extract<DeckAction, { type: "start_orchestration" }>): DeckActionResult {
    const deck = this.connectedDeck(action.deckId);
    if (this.withholdsConfiguredRoles(action.deckId)) throw new Error(FIXTURE_CONFIGURED_ROLES_UNSUPPORTED);
    const home = FIXTURE_HOMES[action.deckId] ?? "/home/dev";
    // PRD #1223 audit V4: the live action's cardinality check, mirrored — a
    // name the project defines twice is refused here rather than launching the
    // first definition, exactly as `ensure_one_orchestration_of_that_name`
    // refuses it in the crate. The dialog's disabled chips stay presentation.
    const defined = fixtureProjectOrchestrations(home, action.path)?.filter((candidate) => candidate.name === action.orchestration) ?? [];
    if (defined.length > 1) throw new Error(`${ambiguousOrchestrationReason(action.orchestration)} Nothing was started.`);
    const orchestration = defined[0];
    if (!orchestration) throw new Error(FIXTURE_UNRESOLVED_REFUSAL);
    const orchestrationId = `fixture-orchestration-${nextFixtureAgentId(deck.agents)}`;
    let startAgentId: string | undefined;
    orchestration.roles.forEach((role, roleIndex) => {
      const agentId = nextFixtureAgentId(deck.agents);
      if (role.start) startAgentId = agentId;
      deck.agents = [
        ...deck.agents,
        {
          ...createFixtureStartedAgent({ id: agentId, daemonId: action.deckId, displayName: role.name, command: FIXTURE_ROLE_COMMANDS[role.name], cwd: action.path, rows: action.rows, cols: action.cols }),
          tab: { kind: "orchestration", orchestrationId, name: orchestration.name, displayTitle: action.displayTitle, roleName: role.name, roleIndex, isStartRole: role.start, cwd: action.path },
          inOrchestration: true,
          isStartRole: role.start,
        },
      ];
    });
    this.emitSnapshot();
    return { ok: true, agentId: startAgentId };
  }

  /**
   * The deck stamp is the TARGET's, for the same reason the live bridge's is
   * (PRD #1105's security audit): the runtime keys its buffers by
   * `(deckId, agentId)` and the fixture's own decks run colliding agent ids.
   *
   * It read `this.snapshot.connection.deckId` — the selected deck — until the
   * cross-deck pane made that wrong rather than merely redundant: the preview
   * can type into a non-selected deck's agent, and echoing those bytes under
   * the selected deck's key is the same wrong-producer stamp issue #1116's open
   * item 1 describes, reproduced in the fixture.
   */
  async sendTerminalInput(target: AgentTarget, data: string): Promise<void> {
    this.terminalListeners.forEach((listener) => listener({ agentId: target.agentId, deckId: target.deckId, data: new TextEncoder().encode(data), stream: "output", operation: "append" }));
    await Promise.resolve();
  }

  async resizeTerminal(): Promise<void> {
    await Promise.resolve();
  }

  /**
   * PRD #882 — the browser preview has no daemon, so nothing ever applies a
   * geometry and the tile keeps whatever its own fit produced. Returning a
   * no-op unsubscribe keeps the tile's cleanup path identical in both modes.
   */
  onTerminalGeometry(): () => void {
    return () => {};
  }

  /**
   * Fixture mode owns no webview, so there is nothing to scale — and the
   * browser it runs in has its own zoom, which is why `useZoom` does not bind
   * the keys here at all. Echoing the snapped level keeps the signature honest
   * for a caller that logs what was applied.
   */
  async setZoom(level: number): Promise<number> {
    await Promise.resolve();
    return clampZoom(level);
  }

  /**
   * Settings for the browser preview. This class never imports
   * `@tauri-apps/api/core`, so it structurally cannot reach `desktop.toml` — a
   * fixture visit can only ever write the localStorage key above.
   */
  async getSettings(): Promise<DesktopSettingsSnapshotDto> {
    await Promise.resolve();
    // No `path`: there is no file. The surface renders the browser-preview
    // wording instead of naming one that does not exist.
    if (this.settings) return { settings: structuredClone(this.settings) };
    let stored: unknown;
    try {
      const raw = window.localStorage.getItem(FIXTURE_SETTINGS_KEY);
      stored = raw ? JSON.parse(raw) : undefined;
    } catch {
      stored = undefined;
    }
    this.settings = normalizeDesktopSettings(stored);
    return { settings: structuredClone(this.settings) };
  }

  async saveSettings(settings: DesktopSettingsDto): Promise<DesktopSettingsDto> {
    await Promise.resolve();
    this.settings = normalizeDesktopSettings(settings);
    try {
      window.localStorage.setItem(FIXTURE_SETTINGS_KEY, JSON.stringify(this.settings));
    } catch {
      // A preview whose storage is unavailable still behaves for this session.
    }
    return structuredClone(this.settings);
  }

  /**
   * The browser preview has no socket, no `IpcStream` and no ssh, so it cannot
   * test anything — and says so rather than inventing a verdict.
   *
   * A synthesised `reachable` would be worse than useless here: the preview is
   * where the panel's layout is driven in Playwright, and a fixture that
   * pretended a deck answered would make a screen that can never be wrong. The
   * honest answer is a real state with a real sentence, which is also what the
   * browser tier needs in order to assert the panel renders one.
   */
  async testEndpoint(settings: DesktopSettingsDto, selection: string): Promise<EndpointTestReportDto> {
    await Promise.resolve();
    const row = settings.endpoints?.remote.find((candidate) => candidate.id === selection);
    const deck = selection === LOCAL_ENDPOINT_SELECTION
      ? "this machine"
      : row
        ? describeEndpoint(row)
        : selection;
    return {
      endpointId: selection,
      deck,
      state: row || selection === LOCAL_ENDPOINT_SELECTION ? "ssh_unavailable" : "unknown_deck",
      ok: false,
      message: row || selection === LOCAL_ENDPOINT_SELECTION
        ? "Browser preview — it has no way to reach a deck, so nothing was tested."
        : "That deck is no longer in this settings document.",
      disclosureKnown: false,
      forwards: [],
      knownHosts: [],
      clientProtocolVersion: 0,
      clientBuildVersion: "browser-preview",
    };
  }

  /**
   * The browser preview has no OS keychain, and it deliberately does not
   * pretend otherwise (PRD #802 M4).
   *
   * There is no in-memory stand-in here, which is the whole point: a preview
   * that "stored" a key would put a real credential somewhere — this class's
   * only persistence is `localStorage`, which is the exact half of PRD #803's
   * rule the settings-secret guard pins. So the preview reports the same
   * situation a headless Linux box does, the panel renders the same sentence,
   * and the browser tier gets to drive that state without a keychain anywhere
   * near it.
   */
  async secretStatus(): Promise<SecretStatusDto> {
    await Promise.resolve();
    return {
      stored: false,
      problem: "Browser preview — it has no OS credential store, so no key can be saved here.",
    };
  }

  async storeSecret(): Promise<SecretStatusDto> {
    await Promise.resolve();
    throw new Error("Browser preview — it has no OS credential store, so no key can be saved here.");
  }

  async forgetSecret(): Promise<SecretStatusDto> {
    await Promise.resolve();
    throw new Error("Browser preview — it has no OS credential store, so there is nothing to forget.");
  }

  /**
   * PRD #802 M6 — the screen the next resolve is validated against.
   *
   * Held rather than acted on, exactly as the live bridge holds it: a
   * declaration is not a request, and the only thing that reads it is the next
   * `resolveVoice`.
   */
  private voiceScreen: VoiceScreen = "deck";

  /* The preview's vocabulary has no directory rows, so a declared browser is
     accepted and has nothing to feed. */
  declareVoiceScreen(screen: VoiceScreen): void {
    this.voiceScreen = screen;
  }

  /**
   * The whole voice backend, deterministically (PRD #802 M6).
   *
   * The vocabulary and every sentence it renders live in `data/fixture.ts` with
   * the snapshots, because in this mode they ARE fixture data — see the note
   * there for why a panel carrying its own would undo the property the pipeline
   * is built on.
   */
  async resolveVoice(utterance: string): Promise<VoiceResultDto> {
    await Promise.resolve();
    return resolveFixtureVoice(utterance, this.voiceScreen);
  }

  /**
   * The preview's vocabulary, annotated the way Rust annotates the real one.
   *
   * Built from the same `FIXTURE_VOICE_COMMANDS` the preview resolves against,
   * so the overlay in the browser lists exactly what the browser can actually
   * run — which is fewer rows than a live build has, and saying so is the point
   * of a preview rather than a shortcoming of one.
   */
  async voiceCommands(screen: VoiceScreen): Promise<VoiceCommandDto[]> {
    await Promise.resolve();
    return fixtureVoiceCommands(screen);
  }

  /**
   * The preview's simulated microphone (PRD #802 M6).
   *
   * `recording` is whether a start has been accepted and not yet released;
   * `delivered` latches once this activation's utterance has been handed over,
   * so the preview says **one thing per activation** and is then quiet. A
   * stand-in that spoke every quarter second would be a loop that overwrote its
   * own report before anyone could read it.
   *
   * `spoken` counts how far through {@link fixtureVoiceScript} the session has
   * got, and — unlike `delivered` — it is NOT reset by `voiceCancel`. The
   * script is the whole session's lines rather than one activation's, so a
   * preview that rewound on every stop could never be driven past its first
   * utterance: *"voice off"* would turn voice off, and the next press would say
   * it again. With no `?voice=` parameter the script is one line, which is the
   * behaviour this had before the parameter existed.
   */
  private microphone = { recording: false, delivered: false, spoken: 0 };

  /** What this preview's microphone will say, in order. */
  private readonly voiceScript = fixtureVoiceScript(window.location.search);

  /**
   * Whether the preview offers a microphone path.
   *
   * **The preview transcribes nothing, ever** — it has no Rust side, no
   * container, and `tauri.conf.json`'s CSP leaves the webview unable to reach a
   * network origin anyway. So this is a preview switch rather than a reading of
   * the settings, and it says so: a document that names no `[voice]` section is
   * a fresh install, and drives the unavailable path; one that names a section
   * drives the other. It used to read `transcription !== "off"`, which was the
   * same kind of switch over a token that no longer exists.
   *
   * Read through `getSettings` rather than from a field, because the browser
   * tier sets the document in `localStorage` before the page loads and the
   * bridge may not have been asked for it yet.
   */
  private async voiceAvailable(): Promise<boolean> {
    const { settings } = await this.getSettings();
    return settings.voice !== undefined;
  }

  /**
   * What the preview's microphone is doing.
   *
   * With no `[voice]` section it reports what a runtime with no microphone
   * reports, so this tier drives the *unavailable* path with no device anywhere
   * near it. With one it drives the other path: the first poll after a start
   * reports the utterance over, which is the boundary `voice::Vad` produces
   * live.
   */
  async voiceStatus(): Promise<VoiceStatusDto> {
    if (!(await this.voiceAvailable())) return fixtureVoiceStatus();
    const speaking = { available: true, backend: "remote" as const };
    const more = this.microphone.spoken < this.voiceScript.length;
    if (this.microphone.recording && more && !this.microphone.delivered) {
      this.microphone.delivered = true;
      return fixtureVoiceStatus({ ...speaking, state: "done", capturedMs: 1_200 });
    }
    return fixtureVoiceStatus({ ...speaking, state: this.microphone.recording ? "recording" : "idle" });
  }

  /**
   * Refused, with the sentence rather than a silence — the live command refuses
   * the same way when transcription is `off`.
   */
  async voiceStart(): Promise<VoiceStatusDto> {
    if (!(await this.voiceAvailable())) {
      throw new Error("Nothing to listen with — the browser preview has no microphone.");
    }
    this.microphone.recording = true;
    return fixtureVoiceStatus({ state: "recording", available: true, backend: "remote" });
  }

  async voiceStop(): Promise<VoiceTranscriptionDto> {
    const available = await this.voiceAvailable();
    this.microphone.recording = false;
    // Re-armed for the NEXT activation, which the pipeline opens by itself: the
    // cycle ends by listening again, so this is what lets the script's second
    // line be heard without a second press.
    this.microphone.delivered = false;
    if (!available) return fixtureVoiceTranscription();
    const line = this.voiceScript[this.microphone.spoken];
    // A stop with the script exhausted is not reachable through the surface —
    // `voiceStatus` reports `done` only while a line is left — so this is the
    // defensive arm rather than a path. An empty transcript resolves to
    // no-match, which is the honest answer to a microphone that heard nothing.
    this.microphone.spoken = Math.min(this.microphone.spoken + 1, this.voiceScript.length);
    return fixtureVoiceHeard(line ?? "");
  }

  /** Idempotent and never refused, for the reason the live one is not. */
  async voiceCancel(): Promise<VoiceStatusDto> {
    await Promise.resolve();
    // `spoken` survives: see the field's own note. A cancel releases the
    // device; it does not rewind the session's script.
    this.microphone = { ...this.microphone, recording: false, delivered: false };
    return fixtureVoiceStatus();
  }

  /**
   * Fixture mode owns no PTYs, so there is nothing to attach or evict — but the
   * seam lives on `DeckBridge` rather than on `TauriDeckBridge` alone, so no
   * screen ever has to know which bridge it is holding.
   */
  async setShownTerminals(): Promise<void> {
    await Promise.resolve();
  }

  /**
   * The fixture owns no daemon, so it answers the way a daemon with nothing
   * live does: an empty listing and no primary. That is the FIRST-RUN state
   * PRD #819 M6 asks the picker to render — a paste-a-path field and a sentence
   * saying so — and having the deterministic preview show it means the state is
   * reachable without a daemon at all.
   */
  async listProjects(): Promise<DaemonProjectListing> {
    await Promise.resolve();
    return { projects: [] };
  }

  /**
   * And it cannot resolve one either. Inventing a plausible project here would
   * be the fixture teaching the same lesson `desktop_project_cwd()` did: that a
   * client may answer a question only the daemon can.
   */
  async resolveProject(): Promise<DaemonResolvedProject> {
    await Promise.resolve();
    throw new Error("The deterministic preview has no deck, so it can resolve no project. Run against a live deck to choose one.");
  }

  /**
   * PRD #1223 M4 — the named fixture deck's tree ({@link fixtureDirectoryTree}),
   * answered the way a deck answers: its home for no path, and any other path
   * in the deck's own spelling. The one normalisation here — trailing and
   * doubled slashes dropped — is the fixture playing the DECK's canonicaliser.
   */
  async listDirectories(deckId: string, path?: string): Promise<DeckDirectoryListing> {
    await Promise.resolve();
    this.connectedDeck(deckId);
    if (this.isOlderDeck(deckId)) return { kind: "unsupported" };
    if (path !== undefined && !fixtureAcceptsPath(path)) throw new Error(FIXTURE_PASTED_PATH_REFUSAL);
    const home = FIXTURE_HOMES[deckId] ?? "/home/dev";
    const wanted = path === undefined ? home : path.replace(/\/+/g, "/").replace(/(.)\/$/, "$1");
    const directory = fixtureDirectoryTree(home).get(wanted);
    if (!directory) throw new Error(FIXTURE_UNRESOLVED_REFUSAL);
    return {
      kind: "listing",
      path: directory.path,
      displayPath: directory.path,
      ...(directory.parent === undefined ? {} : { parent: directory.parent }),
      entries: directory.entries.map((entry) => ({ ...entry })),
      truncated: false,
    };
  }

  /** PRD #1223 M4 — the named fixture deck's options, or `unsupported` for one this preview plays as older. */
  async newAgentOptions(deckId: string): Promise<NewAgentOptions> {
    await Promise.resolve();
    this.connectedDeck(deckId);
    const lastCommand = this.lastCommands.get(deckId);
    const remembered = lastCommand === undefined ? {} : { lastCommand };
    if (this.isOlderDeck(deckId)) return { kind: "unsupported", desktopAgents: fixtureAgentRegistry(), ...remembered };
    const defaultCommand = FIXTURE_DEFAULT_COMMANDS[deckId];
    return {
      kind: "deck",
      ...(defaultCommand === undefined ? {} : { defaultCommand }),
      agents: fixtureAgentRegistry(),
      experimental: FIXTURE_EXPERIMENTAL_DECKS.has(deckId),
      authoringKinds: ["schedule", "schedule-issues", "dispatcher"],
      ...remembered,
    };
  }

  /**
   * PRD #1223 M6 — the named fixture deck's answer for `path`: `demo-project`'s
   * orchestration ({@link fixtureProjectOrchestrations}), `not_project` for any
   * other path — the live deck's generic `unresolved` refusal, read the same
   * way — and `unsupported` for a deck this preview plays as older or as
   * non-Unix.
   */
  async newAgentOrchestrations(deckId: string, path: string): Promise<NewAgentOrchestrations> {
    await Promise.resolve();
    this.connectedDeck(deckId);
    if (this.withholdsConfiguredRoles(deckId)) return { kind: "unsupported", reason: FIXTURE_CONFIGURED_ROLES_UNSUPPORTED };
    const home = FIXTURE_HOMES[deckId] ?? "/home/dev";
    const orchestrations = fixtureProjectOrchestrations(home, path);
    if (!orchestrations) return { kind: "not_project" };
    return { kind: "project", path, displayPath: path, displayName: path.split("/").at(-1) ?? path, orchestrations, configRevision: "fixture-revision" };
  }

  async dispose(): Promise<void> {
    this.fleetListeners.clear();
    this.terminalListeners.clear();
  }
}

/**
 * How many terminals stay attached after you have left them. The bound governs
 * the WARM set alone — terminals currently on screen are never capped, because
 * the deck mounts a terminal on every tile and a bound over everything attached
 * would kill six of nine visible panes. Three keeps bouncing between the
 * handful of agents you are actually working with free of a scrollback replay
 * while holding the idle cost of a nine-agent fleet at three sockets instead of
 * nine (PRD #745 M7).
 */
export const MAX_WARM_TERMINALS = 3;

/**
 * One installed terminal session, and the deck it was created against.
 *
 * The target is stored WITH the session rather than re-derived when a frame
 * arrives, which is the whole of issue #1116's fix at this layer: everything
 * the session produces — its bytes, its geometry, the authority to write to it
 * — is attributed to the deck that created it, not to whichever deck the bridge
 * has since accepted as selected.
 */
interface InstalledTerminalSession {
  target: AgentTarget;
  result: TerminalAttachResult;
}

export class TauriDeckBridge implements DeckBridge {
  readonly mode = "live" as const;
  /*
   * # Every per-agent map below is keyed by `agentKey(deckId, agentId)`
   *
   * PRD #1105's cross-deck pane, and issue
   * [#1116](https://github.com/vfarcic/dot-agent-deck/issues/1116). Agent ids
   * are per-daemon monotonic integers, so `"planner"` names an agent on every
   * deck — and since the agent pane can hold a terminal on a deck that is not
   * the selected one, two of them can be attached at once. Under bare-id keys
   * the second attach's session replaced the first's, `sendTerminalInput`
   * resolved whichever was in the map, and `evictTerminal` for one deck's agent
   * tore down the other's.
   *
   * The key is `agentKey`'s NUL-joined string and is never split back apart.
   * Where the pair itself is needed again — to attach, to stamp a chunk, to
   * notify a geometry listener — it travels as an {@link AgentTarget} value
   * beside the key rather than being reassembled from it.
   */
  private attached = new Set<string>();
  private sessions = new Map<string, InstalledTerminalSession>();
  /**
   * `sessionId` -> the composite key its session is filed under.
   *
   * The daemon's own events (`desktop://terminal-state`,
   * `desktop://terminal-geometry`) name a session id and a bare agent id, and
   * the session id is the unambiguous half: it is minted from a process-wide
   * counter in the desktop crate, so it names one attach on one deck with no
   * further context. Resolving through this index is what lets those two
   * listeners find the right deck's session without reading the current
   * selection — issue #1116's open items 1 and 4.
   */
  private sessionKeys = new Map<string, string>();
  private terminalChannels = new Map<string, import("@tauri-apps/api/core").Channel<ArrayBuffer>>();
  private pendingAttachments = new Map<string, PendingTerminalAttachment>();
  private pendingTerminal = new Map<string, TerminalChunk[]>();
  private pendingResizes = new Map<string, { cols: number; rows: number }>();
  private resizeFrames = new Map<string, number>();
  private resizeInFlight = new Set<string>();
  /**
   * Agents whose terminal is on screen right now, as last declared by
   * `setShownTerminals`. Unbounded, and never an eviction candidate.
   *
   * A `Map` to its {@link AgentTarget} rather than a `Set` of keys, because the
   * attach that follows needs the deck back and a key may never be split.
   */
  private shown = new Map<string, AgentTarget>();
  /**
   * Agents whose terminal has been left but is still attached, insertion-ordered
   * least-recently-left first so eviction takes the head. Bounded by
   * `MAX_WARM_TERMINALS`. Membership is by composite identity and does NOT
   * require the attach to have landed — a pending attach that is never a warm
   * member is never selected for eviction, and installs itself afterwards past
   * the bound.
   */
  private warm = new Map<string, AgentTarget>();
  /**
   * Agents with a `desktop_terminal_attach` invocation still outstanding —
   * added before the invoke, removed when it settles either way. It is what
   * stops a second invocation from starting behind the first: the Rust side
   * serialises every agent through one attach gate with no timeout and no
   * cancellation, so a daemon that never answers would otherwise collect one
   * more channel, closure, promise and queued command per hide/reshow cycle.
   *
   * Deliberately NOT `pendingAttachments`, and deliberately NOT cleared by
   * `evictTerminal`: eviction cancels an attach by *marking* it (deleting
   * `pendingAttachments` is what makes the post-await guard fail), and the
   * backend command it started keeps running whatever the frontend forgets.
   * A guard that eviction clears re-arms on every cycle and guards nothing.
   * The one place it IS cleared without settling is `clearAttachSuppression`,
   * so an attach that never answers cannot suppress its agent forever.
   */
  private attachInvocations = new Set<string>();
  /**
   * Agents whose attach the guard above suppressed, coalesced to at most one
   * request each. Replayed once when the outstanding invocation settles, so a
   * hide/reshow that raced an attach still ends with a live pane — suppressing
   * without this would trade an unbounded queue for a dead terminal.
   */
  private attachRequested = new Map<string, AgentTarget>();
  private terminalListener?: TerminalListener;
  /**
   * The fold every `desktop://snapshot` lands in (PRD #742 M4): one entry per
   * deck, keyed by `connection.deckId`, in insertion order — which is observed
   * order, because `connect()` seeds the selected deck and every other deck is
   * inserted by its own watcher's first emit.
   *
   * A `Map` rather than an array because the wire is an UPSERT stream: N
   * watchers each emit their own deck on their own coalescing window, so what
   * arrives is "here is deck X as of now" and never "here is the fleet". The
   * array the listeners see is built from this on every emit.
   *
   * **PRD #742 M5 changed the key from `connection.socketPath` to
   * `connection.deckId`**, and that is the whole of this milestone at this
   * layer: `socketPath` is `Endpoint::describe()`, which two daemons on one host
   * share, so `fleet.set(socketPath, …)` folded the second deck ON TOP of the
   * first — one entry, last writer wins, and a screen that looks like one
   * healthy deck.
   */
  private fleet = new Map<string, DeckSnapshot>();
  /**
   * Which deck the single-deck surfaces are bound to.
   *
   * **Read off `DesktopSnapshotDto.fleet[0]` on every arrival since PRD #742
   * M5**, and seeded from `connect()`. Before M5 nothing on the snapshot stream
   * said "this one is selected" and it could not be inferred — under `All`
   * every observed deck emits the same shape — so it was re-learnt at
   * `connect()` and nowhere else. The crate now states it on every snapshot,
   * which is why membership no longer depends on a handshake landing.
   */
  private selectedDeckId?: string;
  /**
   * The unconfigured decks as the crate last stated them, with the two client
   * facts the payload that stated them carried (PRD #742 M12).
   *
   * Held rather than folded into {@link fleet}, because these are not upserts
   * competing with a watcher's snapshots — the whole list is restated on every
   * arrival and replaces what was here, so a row that gains a socket path (and
   * therefore a watcher) simply stops being listed and the group it had is
   * rebuilt from the snapshot that deck now emits.
   *
   * The protocol version and build stamp travel WITH the list rather than being
   * defaulted, because they are facts about this app that every snapshot
   * already carries and that a deck nobody contacted has no way to report. One
   * field rather than three so they cannot be read from different arrivals, and
   * `undefined` until the first one, so nothing is rendered before the crate
   * has said anything.
   */
  private unconfigured?: { decks: UnconfiguredDeckDto[]; clientProtocolVersion: number; clientBuildVersion: string };
  /**
   * Every connectable deck as the crate last NAMED it, with the same two client
   * facts (PRD #742 M14) — the source {@link fleetView} builds a pending group
   * from.
   *
   * Held, replaced and read exactly like {@link unconfigured} above, and for
   * the same reason: the crate restates the whole list on every arrival, so the
   * newest one is the whole truth about what the applied document observes.
   *
   * It is deliberately NOT filtered to the decks that have yet to report — that
   * is decided per view, by looking in {@link fleet}, so a deck that reports
   * between two emits stops being pending without anything having to notice.
   */
  private observed?: { decks: ObservedDeckDto[]; clientProtocolVersion: number; clientBuildVersion: string };
  private fleetListener?: FleetListener;
  /**
   * The `[endpoints]` section as this bridge last saw it, serialised. Compared
   * on every `saveSettings` so a theme save does not re-establish a fleet, and
   * an endpoint edit does. `undefined` until a read or a write has been seen.
   */
  private lastEndpoints?: string;
  /**
   * PRD #882 — the geometry the daemon has applied per agent, and who wants to
   * hear about it changing.
   *
   * A tile subscribes when it mounts and reads the cached value immediately,
   * because the push that carried it may well have arrived before the tile
   * existed — a second client can shrink an agent long before anyone opens a
   * terminal on it here.
   *
   * Keyed by the composite identity and carrying its {@link AgentTarget}, which
   * is what retired `adoptGeometryDeck` (issue #1116's open item 4). That method
   * emptied the whole cache when the selected deck moved, and emptying is not an
   * identity boundary: a deck-A geometry event arriving after the clear rewrote
   * the bare-id cache and notified listeners with the deck selected NOW, so
   * both caches recorded A's dimensions as B's and B's next attach submitted
   * another machine's viewport to it. An entry keyed and stamped with its own
   * session's deck cannot be read for any other deck at all, so there is
   * nothing left for a clear to protect against.
   */
  private appliedGeometry = new Map<string, { target: AgentTarget; rows: number; cols: number }>();
  private geometryListeners = new Set<(agentId: string, rows: number, cols: number, deckId?: string) => void>();
  private invoke?: typeof import("@tauri-apps/api/core")["invoke"];
  private lifecycle = 0;
  /**
   * Newest-first ring of mapped hook events for the SELECTED deck, capped at
   * MAX_LIVE_EVIDENCE. {@link handoffs} is the same thing for the handoff rail.
   *
   * **A working copy of one deck's history, not a process-global one** (PRD
   * #742 M8). It was global, `foldSnapshot` handed it to whichever deck was
   * currently selected, and `connect()` cleared `fleet` but not this — so hook
   * events recorded while the local deck was selected survived a switch and
   * `build-box`'s first snapshot arrived carrying them. One machine's hook
   * history, with its agent ids, roles and pane ids, under another machine's
   * name.
   *
   * That was **pre-existing** rather than introduced by the fleet — `4a7ae532`
   * passed the same global ring — but the fleet is what makes it easy to meet,
   * and `isSelectedDeckEvent` stops *live* events crossing while doing nothing
   * about a ring that survives the switch. A reader who sees that filter will
   * reasonably conclude the drawer is deck-clean; it was not.
   *
   * {@link adoptEvidenceDeck} is the whole of the fix. The per-deck storage
   * already existed — each `DeckSnapshot` in {@link fleet} carries its own
   * `evidence`/`handoffs` — so switching decks swaps this working copy for the
   * arriving deck's own history rather than carrying it across, and no parallel
   * per-deck map is needed.
   */
  private evidence: EvidenceItem[] = [];
  private handoffs: HandoffEdge[] = [];
  /**
   * Which deck {@link evidence} and {@link handoffs} describe, or `undefined`
   * before this bridge knows which deck it is on.
   *
   * `undefined` is load-bearing rather than an initial value: `subscribe()` runs
   * BEFORE `connect()` in `useDeckRuntime`, so hook events genuinely arrive
   * before any snapshot has said which deck is selected. Those belong to the
   * first deck that becomes selected — which is what `undefined` means here, and
   * why {@link adoptEvidenceDeck} adopts rather than clears in that one case.
   */
  private evidenceDeckId?: string;
  /**
   * Mints `hook-<n>` ids, and NEVER reset — not on a deck switch, not on
   * `connect()`.
   *
   * These ids are React keys on the evidence drawer's rows, and a deck switch
   * puts another deck's rows on screen; a counter that restarted would mint
   * `hook-0` for the second deck while the first deck's `hook-0` is still held
   * in its own snapshot, so switching back would collide two distinct items
   * under one key. Monotonic for the life of the bridge costs a larger integer
   * and nothing else.
   */
  private evidenceSequence = 0;
  private agentIndex: AgentSession[] = [];

  /**
   * Resolves a hook event's agent by registry id first, then by the pane id the
   * daemon tagged the pane with — hook payloads from external agents carry only
   * one or the other.
   */
  private resolveAgent = (agentId?: string, paneId?: string): { id: string; role: string } | undefined => {
    const match = this.agentIndex.find((agent) => (agentId && agent.id === agentId) || (paneId && agent.paneId === paneId));
    return match ? { id: match.id, role: match.role } : undefined;
  };

  /**
   * Whether a `desktop://daemon-event` payload belongs to the deck the deck
   * screen is on (PRD #742 M4).
   *
   * `deck` is the flat string PRD #742 M3 stamps beside the event, carrying
   * exactly what `connection.deckId` does — M5 moved BOTH from the label to the
   * key in one change, and they have to move together or this compares two
   * different naming schemes and silently drops every event.
   *
   * An absent or non-string `deck` reads as YES, deliberately: the field is
   * additive, and treating its absence as "some other deck" would silence every
   * event from a build that predates the stamp and every event a fixture
   * synthesises.
   */
  private isSelectedDeckEvent(payload: unknown): boolean {
    if (this.selectedDeckId === undefined) return true;
    if (typeof payload !== "object" || payload === null) return true;
    const deck = (payload as { deck?: unknown }).deck;
    return typeof deck !== "string" || deck === this.selectedDeckId;
  }

  /**
   * Point the evidence ring and the handoff rail at `deckId` (PRD #742 M8).
   *
   * Called wherever {@link selectedDeckId} is learnt or re-learnt — `connect()`
   * and `foldSnapshot` — and a no-op whenever it has not moved, so it costs
   * nothing on the ordinary snapshot and does its work on the first arrival and
   * on the ones that follow a selection change.
   *
   * Three cases, and the middle one is the finding:
   *
   * - *same deck* — nothing to do.
   * - *a different deck* — the working copy is replaced by that deck's OWN
   *   held history, taken from its snapshot in {@link fleet}, or emptied when it
   *   has none yet. This is what stops one machine's hook history being handed
   *   to another machine's snapshot, and it also means switching back restores
   *   what that deck had rather than showing an empty drawer.
   * - *no deck yet* (`evidenceDeckId === undefined`) — whatever has accumulated
   *   is ADOPTED, not cleared. `subscribe()` runs before `connect()`, so events
   *   that land in that window have no deck of their own and belong to the first
   *   one selected; clearing here would lose the drawer's earliest entries,
   *   which is the behaviour that was already deliberate before M8.
   */
  private adoptEvidenceDeck(deckId: string): void {
    if (this.evidenceDeckId === deckId) return;
    if (this.evidenceDeckId !== undefined) {
      const held = this.fleet.get(deckId);
      this.evidence = held?.evidence ?? [];
      this.handoffs = held?.handoffs ?? [];
    }
    this.evidenceDeckId = deckId;
  }

  private recordDaemonEvent(payload: unknown): boolean {
    const edges = applyHandoffEvent(this.handoffs, payload);
    const edgesChanged = edges !== this.handoffs;
    this.handoffs = edges;
    const item = mapDaemonEvent(payload, this.evidenceSequence, this.resolveAgent);
    if (!item) return edgesChanged;
    this.evidenceSequence += 1;
    this.evidence = [item, ...this.evidence].slice(0, MAX_LIVE_EVIDENCE);
    return true;
  }

  private async getInvoke(): Promise<typeof import("@tauri-apps/api/core")["invoke"]> {
    if (!this.invoke) this.invoke = (await import("@tauri-apps/api/core")).invoke;
    return this.invoke;
  }

  /**
   * The single attach trigger (PRD #745 M7). Takes the whole shown set, diffs it
   * against the previous one, and does all four things in one pass: attach what
   * is newly shown, move what is newly hidden into the warm set, evict warm
   * overflow, and flush the warm set entirely when nothing is shown at all.
   *
   * It must be called ONCE per render commit with every shown id, never once
   * per tile: nine single-id calls would leave eight of the nine warm and evict
   * five of them, which is the same broken deck the bound exists to avoid.
   */
  async setShownTerminals(targets: AgentTarget[]): Promise<void> {
    const next = new Map(targets.map((target) => [agentKey(target.deckId, target.agentId), target] as const));

    // Leaving a terminal does not detach it. It moves to the warm set, delete-
    // then-add so the tail is the most recently left and the head is the LRU.
    for (const [key, target] of this.shown) {
      if (next.has(key)) continue;
      this.warm.delete(key);
      this.warm.set(key, target);
    }
    // A shown terminal is never an eviction candidate, so showing a warm one
    // takes it back out of the warm set. It stays in `attached`, so coming back
    // costs no attach and produces no replay — the whole point of warm.
    for (const key of next.keys()) this.warm.delete(key);
    this.shown = next;

    // Bounded against `warm.size` ALONE — never against the shown or the
    // attached count, which is what would kill visible panes.
    const overflow = this.shown.size === 0
      // Flushed to ZERO rather than down to the bound, so "no terminals
      // attached" is true however you arrived at a screen that shows none.
      ? this.warm.size
      : Math.max(0, this.warm.size - MAX_WARM_TERMINALS);
    // `evictTerminal` is synchronous up to its `desktop_terminal_detach`, so
    // every evicted agent is out of `sessions` / `terminalChannels` /
    // `pendingAttachments` BEFORE the attach below writes its new entries.
    const evictions = Array.from(this.warm.keys()).slice(0, overflow).map((key) => this.evictTerminal(key));

    // Every shown target, not only the newly shown ones: `attachAgents` filters
    // out whatever is already attached, so re-declaring an unchanged set is a
    // no-op except where a shown terminal lost its session to a daemon
    // `end`/`error` state event and has to be brought back.
    const attaching = this.shown.size ? this.attachAgents(Array.from(this.shown.values())) : Promise.resolve();
    await Promise.all([...evictions, attaching]);
  }

  /**
   * Drops one terminal completely — client-side state first, then the daemon.
   *
   * Every map keyed by agent id has to be cleared here, not just `sessions`: a
   * surviving `terminalChannels` entry would leave the dead attach's channel
   * able to deliver output, and a surviving `pendingResizes` / `resizeFrames`
   * entry would push a size computed for the old pane at the next session
   * (`attachAgents` re-schedules a pending resize on re-attach).
   *
   * Dropping `pendingAttachments` and `terminalChannels` IS how an attach still
   * in flight gets cancelled: `attachAgents`' post-await guard then fails and
   * takes the orphan-detach branch instead of installing a session behind a
   * screen that no longer shows it. Cancellation is marking, never awaiting —
   * one slow attach must not freeze every later terminal switch. The marking is
   * also why `attachInvocations` is the ONE agent-keyed set deliberately left
   * alone here: the Tauri command an evicted attach started is still running,
   * and forgetting it is what would let a stalled daemon collect one queued
   * invocation per hide/reshow cycle.
   *
   * Per-agent, and deliberately NOT a `lifecycle` bump: `lifecycle` is a
   * whole-bridge generation, and bumping it here would also void every SHOWN
   * attach still in flight and leak `resizeInFlight` for any agent mid-resize.
   */
  private async evictTerminal(key: string): Promise<void> {
    const session = this.sessions.get(key);
    this.shown.delete(key);
    this.warm.delete(key);
    this.sessions.delete(key);
    if (session) this.sessionKeys.delete(session.result.sessionId);
    this.attached.delete(key);
    /*
      `appliedGeometry` is deliberately NOT dropped here, and the composite key
      is what makes leaving it correct rather than merely tolerated. PRD #882
      wants a re-attach to declare the grid this agent was last known to be at,
      so the entry is worth keeping; the audit's objection was that a bare-id
      entry left behind by deck A was then read as deck B's. An entry filed
      under `(A, planner)` can only ever be read back for `(A, planner)`.
    */
    this.terminalChannels.delete(key);
    this.pendingAttachments.delete(key);
    // Chunks buffered while no listener was installed belong to the session
    // being torn down here. Keeping them would replay a dead pane's scrollback
    // ahead of the live one at the next `subscribe` drain.
    this.pendingTerminal.delete(key);
    this.attachRequested.delete(key);
    this.pendingResizes.delete(key);
    const frame = this.resizeFrames.get(key);
    if (frame !== undefined) {
      window.cancelAnimationFrame(frame);
      this.resizeFrames.delete(key);
    }
    this.resizeInFlight.delete(key);
    // Nothing installed yet: the teardown above is the whole cancellation, and
    // the pending attach detaches its own late-arriving session.
    if (!session) return;
    const invoke = await this.getInvoke();
    await invoke("desktop_terminal_detach", { sessionId: session.result.sessionId }).catch(() => undefined);
  }

  /**
   * Drops the per-agent attach guard and everything queued behind it.
   *
   * The guard in `attachAgents` is cleared per invocation by its own `finally`,
   * which never runs for an attach the daemon accepts and never answers — and
   * `evictTerminal` deliberately leaves it alone, because forgetting a running
   * command is what lets a stalled daemon collect one more queued invocation
   * per hide/reshow cycle. Both are right in the steady state and together they
   * make one agent PERMANENTLY unattachable: every later declaration for it is
   * suppressed, while the replay that would undo the suppression is itself
   * waiting on the invocation that never settles.
   *
   * So it is cleared exactly where a whole-bridge restart happens — `connect()`
   * and `dispose()` — and nowhere else. Both mean the frontend is starting its
   * relationship with the daemon over, which is the only moment at which
   * forgetting an outstanding command is a fresh start rather than an
   * unbounded queue: it costs at most one extra queued command per Reconnect,
   * bounded by a deliberate user action, against a pane that is otherwise dead
   * until the app restarts.
   */
  private clearAttachSuppression(): void {
    this.attachInvocations.clear();
    this.attachRequested.clear();
  }

  private async attachAgents(targets: AgentTarget[], expectedLifecycle = this.lifecycle): Promise<void> {
    if (expectedLifecycle !== this.lifecycle) return;
    const invoke = await this.getInvoke();
    if (expectedLifecycle !== this.lifecycle) return;
    const { Channel } = await import("@tauri-apps/api/core");
    if (expectedLifecycle !== this.lifecycle) return;
    // Re-read membership after the dynamic imports, not before them: an agent
    // shown when this call started can have been hidden and evicted while they
    // resolved, and attaching it then would leave a live PTY behind a screen
    // that shows no terminal at all.
    await Promise.allSettled(targets.filter((target) => {
      const key = agentKey(target.deckId, target.agentId);
      if (this.attached.has(key) || !(this.shown.has(key) || this.warm.has(key))) return false;
      // One outstanding invocation per agent, whatever the frontend has since
      // forgotten about it. Queue the declaration instead of starting a second
      // command — the settling invocation replays it.
      if (this.attachInvocations.has(key)) {
        this.attachRequested.set(key, target);
        return false;
      }
      return true;
    }).map(async (target) => {
      const lifecycle = expectedLifecycle;
      /*
        The composite key AND the target are both captured here, once, before
        the first await — this is the "origin stamping at creation" issue #1116
        asks for. Everything downstream of this closure (the channel callback,
        the replay, the geometry notification, the error chunk) reads `target`
        and never `this.selectedDeckId`, so a frame that arrives after the
        selection has moved is still attributed to the deck that produced it.
      */
      const key = agentKey(target.deckId, target.agentId);
      this.attached.add(key);
      const onOutput = new Channel<ArrayBuffer>();
      const attempt: PendingTerminalAttachment = {
        lifecycle,
        channel: onOutput,
        output: [],
        stateEvents: [],
        activated: false,
      };
      onOutput.onmessage = (chunk) => {
        if (
          lifecycle !== this.lifecycle
          || this.terminalChannels.get(key) !== onOutput
        ) return;
        const data = new Uint8Array(chunk);
        if (!attempt.activated) {
          attempt.output.push(data);
          return;
        }
        if (attempt.session) this.deliverOutput(target, data, attempt.session.generation);
      };
      this.terminalChannels.set(key, onOutput);
      this.pendingAttachments.set(key, attempt);
      this.attachInvocations.add(key);
      try {
        // PRD #882: declare this tile's measured geometry so the agent is sized
        // to the smallest pane among every client watching it — including this
        // one — rather than to whichever client resized last.
        const viewport = this.pendingResizes.get(key) ?? this.appliedGeometry.get(key);
        const session = await invoke<TerminalAttachResult>("desktop_terminal_attach", {
          /* The deck is named explicitly so the crate resolves ITS link through
             `DaemonLinks` rather than the process-global selected endpoint. */
          deckId: target.deckId,
          agentId: target.agentId,
          onOutput,
          rows: viewport?.rows,
          cols: viewport?.cols,
        });
        const appliedRows = session.appliedRows;
        const appliedCols = session.appliedCols;
        if (appliedRows && appliedCols) {
          this.appliedGeometry.set(key, { target, rows: appliedRows, cols: appliedCols });
          // PRD #882 (raised by Greptile on PR #895): reshape the grid
          // SYNCHRONOUSLY, before the buffered replay is delivered a few lines
          // below.
          //
          // Notifying the listeners only schedules a React state update, and
          // the tile resizes in an effect after that commits — but the replay
          // is written to xterm immediately. When the applied geometry differs
          // from what the tile fitted (which is the whole point of the policy),
          // xterm would parse the replay at the wrong grid and keep the
          // resulting wrapping and cursor damage, since the daemon's snapshot
          // is a single dimension epoch that will not be re-sent. Resizing the
          // live instance first closes that window; the listener below still
          // fires so React state and any later re-render agree with it.
          try {
            const terminal = getTerminal(target.deckId, target.agentId);
            if (terminal && (terminal.cols !== appliedCols || terminal.rows !== appliedRows)) {
              terminal.resize(appliedCols, appliedRows);
            }
          } catch {
            // A tile mid-mount can reject a resize; the effect reconciles it.
          }
          this.geometryListeners.forEach((listener) => listener(target.agentId, appliedRows, appliedCols, target.deckId));
        }
        if (
          lifecycle !== this.lifecycle
          || this.terminalChannels.get(key) !== onOutput
          || this.pendingAttachments.get(key) !== attempt
        ) {
          await invoke("desktop_terminal_detach", { sessionId: session.sessionId }).catch(() => undefined);
          return;
        }
        attempt.session = session;
        /*
          Installed under ITS OWN deck's key, which is issue #1116's open item 3
          — "a pending attach that resolves after the selection moved must
          install against its own deck". It used to install into a bare-id map,
          so an attach started for deck A and settling after the selection moved
          to B became the session `sendTerminalInput("planner", …)` resolved
          while the user was looking at B's namesake. There is nothing to
          discard here and nothing to reattribute: the pane that asked for this
          stream is still showing it.
        */
        this.sessions.set(key, { target, result: session });
        this.sessionKeys.set(session.sessionId, key);
        this.pendingAttachments.delete(key);

        const replayLength = attempt.output.reduce((total, chunk) => total + chunk.byteLength, 0);
        const replay = new Uint8Array(replayLength);
        let replayOffset = 0;
        for (const chunk of attempt.output) {
          replay.set(chunk, replayOffset);
          replayOffset += chunk.byteLength;
        }
        attempt.output = [];
        this.deliverTerminal(target, {
          agentId: target.agentId,
          data: replay,
          stream: "output",
          operation: "replace",
          generation: session.generation,
        });
        attempt.activated = true;
        attempt.stateEvents.forEach((event) => this.handleTerminalState(event));
        if (this.pendingResizes.has(key)) this.scheduleResize(key);
      } catch (cause) {
        if (lifecycle === this.lifecycle) {
          if (this.pendingAttachments.get(key) === attempt) this.pendingAttachments.delete(key);
          this.attached.delete(key);
          if (this.terminalChannels.get(key) === onOutput) this.terminalChannels.delete(key);
          this.deliverTerminal(target, {
            agentId: target.agentId,
            data: new Uint8Array(),
            stream: "error",
            operation: "append",
            message: "Terminal attach failed. The agent is still running; reconnect to retry its terminal.",
          });
        }
        throw cause;
      } finally {
        this.attachInvocations.delete(key);
        // A declaration suppressed while this invocation was outstanding is
        // coalesced rather than dropped: replay exactly one, and only while the
        // agent is still wanted and still unattached. A failed attach queues no
        // request of its own, so this cannot become a retry loop.
        const requested = this.attachRequested.get(key);
        if (
          requested !== undefined
          && this.attachRequested.delete(key)
          && lifecycle === this.lifecycle
          && !this.attached.has(key)
          && (this.shown.has(key) || this.warm.has(key))
        ) {
          void this.attachAgents([requested], lifecycle);
        }
      }
    }));
  }

  private deliverOutput(target: AgentTarget, data: Uint8Array, generation: number): void {
    this.deliverTerminal(target, { agentId: target.agentId, data, stream: "output", operation: "append", generation });
  }

  /**
   * Hand one chunk to the runtime, stamped with the deck that produced it.
   *
   * **The stamp is the ORIGIN deck, supplied by the caller, and never
   * `selectedDeckId`** — issue
   * [#1116](https://github.com/vfarcic/dot-agent-deck/issues/1116)'s open item
   * 1, and the finding the second audit round called central. Agent ids are
   * per-daemon monotonic, so a consumer keying buffers by bare id replays the
   * previous deck's output under the next deck's namesake — and the composite
   * key that was supposed to fix it was defeated by *this* line, which read the
   * deck selected when the chunk was DELIVERED rather than the deck that
   * created the channel. `foldSnapshot` moves `selectedDeckId` before React
   * receives the fleet, while existing channel callbacks stay valid, so a late
   * frame from deck A was filed under deck B's composite key with complete
   * confidence.
   *
   * Every producer holds its session's `target` in scope from before the attach
   * was invoked, so there is no path here that has to guess.
   *
   * A queued chunk keeps that stamp for the same reason: it is drained after
   * the listener installs, potentially long after a switch.
   */
  private deliverTerminal(target: AgentTarget, event: TerminalChunk): void {
    const stamped: TerminalChunk = { ...event, deckId: target.deckId };
    if (this.terminalListener) {
      this.terminalListener(stamped);
      return;
    }
    const key = agentKey(target.deckId, target.agentId);
    const pending = this.pendingTerminal.get(key) ?? [];
    pending.push(stamped);
    this.pendingTerminal.set(key, pending);
  }

  /**
   * Resolved through the SESSION ID, not the bare agent id the event also
   * carries: the id is minted per attach by the desktop crate, so it names one
   * stream on one deck. Looking the agent up by name would be the same
   * cross-deck collision one layer down.
   */
  private handleTerminalState(event: DesktopTerminalStateDto): void {
    if (event.state === "attached") return;
    const key = this.sessionKeys.get(event.sessionId);
    const session = key === undefined ? undefined : this.sessions.get(key);
    if (
      !key
      || !session
      || session.result.sessionId !== event.sessionId
      || session.result.generation !== event.generation
    ) return;

    this.sessions.delete(key);
    this.sessionKeys.delete(event.sessionId);
    this.attached.delete(key);
    this.terminalChannels.delete(key);
    this.deliverTerminal(session.target, {
      agentId: session.target.agentId,
      data: new Uint8Array(),
      stream: event.state,
      operation: "append",
      generation: event.generation,
      message: event.message,
    });
  }

  /**
   * The fleet as the bridge holds it, selected deck first.
   *
   * Built on every emit rather than maintained as an array, because the wire is
   * an upsert stream and the order a listener needs is not the order snapshots
   * arrive in: the selected deck must lead however late its watcher happened to
   * fire. Insertion order carries the rest, which is observed order.
   */
  private fleetView(): DeckFleet {
    const entries = Array.from(this.fleet.entries());
    const selectedAt = entries.findIndex(([deckId]) => deckId === this.selectedDeckId);
    const decks = entries.map(([, deck]) => deck);
    /*
      PRD #742 M12: the configured decks with no address, last and never first.
      They are built here rather than held in `fleet` because nothing upserts
      them — the crate restates the whole list on every arrival, so deriving
      them per view is what keeps a row that has just gained a socket path from
      lingering as a ghost beside the real group its new watcher emits.

      Never `fleet[0]`: `selectedDeckId` is a real deck's key, so one of these
      can only lead when the fleet is otherwise empty — which is the loading
      seed's job and not a state the crate produces (`observed_fleet` always
      carries the resolved deck).
    */
    const stated = this.unconfigured;
    const unconfigured = stated ? stated.decks.map((deck) => unconfiguredDeckSnapshot(deck, stated.clientProtocolVersion, stated.clientBuildVersion)) : [];
    const pending = this.pendingDecks();
    /*
      PRD #742 M14: after the decks that answered and before the ones with no
      address, which is the order the three states degrade in — a deck with an
      agent list, then one whose list is still coming, then one that has nowhere
      to get a list from. Never first, for the reason the M12 note gives: every
      single-deck surface binds to `fleet[0]`, and a pending entry has no agents
      and no terminals to bind them to.
    */
    if (selectedAt <= 0) return [...decks, ...pending, ...unconfigured];
    const [selected] = decks.splice(selectedAt, 1);
    return [selected, ...decks, ...pending, ...unconfigured];
  }

  /**
   * The observed decks this bridge has not heard from yet, as groups (PRD #742
   * M14).
   *
   * # Derived, never stored
   *
   * Computed per view rather than held, which is what makes the state
   * self-clearing: a deck stops being pending the instant its own snapshot
   * lands in {@link fleet}, with nothing to remember to delete. It is also why
   * this is not a synthesized entry in that map — {@link pruneFleet}
   * deliberately refuses to invent one for an id it has heard nothing about,
   * and a fabricated snapshot in there would be fighting that guard rather than
   * using it. This reads the crate's own statement of what the applied document
   * observes, which is a fact about the document and not a connection state.
   *
   * # Read off `observed` rather than off `fleet`
   *
   * `fleet` is ids, and its unconfigured members are ids too — deriving from it
   * would mean subtracting one list from another and then having nothing to
   * name what was left. Every entry here carries its own label, so a pending
   * group is always nameable, and the unconfigured rows are not in this list at
   * all.
   */
  private pendingDecks(): DeckSnapshot[] {
    const stated = this.observed;
    if (!stated) return [];
    return stated.decks
      .filter((deck) => !this.fleet.has(deck.deckId))
      .map((deck) => pendingDeckSnapshot(deck, stated.clientProtocolVersion, stated.clientBuildVersion));
  }

  /**
   * Drop every deck the crate no longer observes (PRD #742 M5).
   *
   * # Why this is the exact answer M4 could not write
   *
   * A deck LEAVING the observed set produces no event. `apply_selection` ends
   * its watcher and, for `All` -> `local`, does not even take the path that
   * emits — so the departed deck simply stops arriving, which is
   * indistinguishable from a quiet deck. Left alone the bridge keeps its last
   * snapshot and the overview renders its agents, frozen and looking live.
   *
   * `DesktopSnapshotDto.fleet` states membership on EVERY snapshot, and every
   * deck's snapshot carries the same list, so any arrival is enough to prune.
   *
   * # It does not seed
   *
   * A deck named in `fleet` that has not emitted yet is not invented here: this
   * bridge has nothing to render for it and a fabricated entry would be a
   * connection state nobody measured. It appears when its watcher emits, which
   * for a newly started one is immediate.
   *
   * An empty or absent list prunes NOTHING. The crate's invariant is
   * "never empty" (a deck it cannot reach is still an entry carrying a
   * `disconnected` connection), so an empty list is a malformed payload rather
   * than an empty fleet — and acting on it would clear the screen.
   */
  /**
   * Take the crate's statement of which configured decks have no address yet
   * (PRD #742 M12).
   *
   * Replaces rather than merges: the list is a property of the applied
   * document, restated on every snapshot from every deck, so the newest
   * arrival is the whole truth. That is what drops a row the moment `Test
   * connection` fills its socket path in — it leaves this list, and the watcher
   * the crate now spawns for it emits the real group.
   *
   * An ABSENT field changes nothing, and an empty array clears. The two differ
   * on purpose: absent is a DTO literal in a test that says nothing about this,
   * while `[]` is the crate saying every configured deck has an address. This
   * is the opposite reading from {@link pruneFleet}, and for the opposite
   * reason — an empty membership list would blank the screen, whereas an empty
   * unconfigured list is the ordinary, healthy case.
   */
  private adoptUnconfigured(dto: DesktopSnapshotDto): void {
    if (!Array.isArray(dto.unconfigured)) return;
    this.unconfigured = {
      decks: dto.unconfigured,
      clientProtocolVersion: dto.connection.clientProtocolVersion,
      clientBuildVersion: dto.connection.clientBuildVersion,
    };
  }

  /**
   * Take the crate's statement of what the applied document observes, and what
   * each of those decks is called (PRD #742 M14).
   *
   * Replaces rather than merges, and an ABSENT field changes nothing — the same
   * two readings {@link adoptUnconfigured} makes, for the same two reasons. An
   * empty array would be the crate saying it observes nothing, which
   * `connectable_endpoints` cannot answer: it is the one selected endpoint for
   * every selection but `All`, and for `All` the local deck plus the rows with
   * an address.
   *
   * Note an empty list here CANNOT blank the screen the way an empty `fleet`
   * would: this list only ever ADDS groups for decks that have not reported,
   * and every deck that has reported is rendered from {@link fleet} whatever
   * this says.
   */
  private adoptObserved(dto: DesktopSnapshotDto): void {
    if (!Array.isArray(dto.observed)) return;
    this.observed = {
      decks: dto.observed,
      clientProtocolVersion: dto.connection.clientProtocolVersion,
      clientBuildVersion: dto.connection.clientBuildVersion,
    };
  }

  private pruneFleet(observed: readonly string[] | undefined): void {
    if (!Array.isArray(observed) || observed.length === 0) return;
    const keep = new Set(observed);
    this.fleet.forEach((_, deckId) => {
      if (!keep.has(deckId)) this.fleet.delete(deckId);
    });
  }

  async connect(): Promise<DeckFleet> {
    // Reconnect is the user's remedy for a wedged control room, so it has to be
    // able to remedy this too. `useDeckRuntime` memoizes the bridge on `mode`
    // alone and `reconnect()` calls straight into here, so nothing is disposed
    // and nothing is recreated in between.
    this.clearAttachSuppression();
    const invoke = await this.getInvoke();
    const dto = await invoke<DesktopSnapshotDto>("desktop_bootstrap", { options: { startIfMissing: false } });
    // PRD #745 M7: connecting attaches NOTHING. It used to attach every agent
    // the daemon owns, so a nine-agent fleet cost nine sockets and nine
    // scrollback replays before a single terminal was on screen. The UI states
    // what it shows through `setShownTerminals`, and that is the only trigger.
    /*
      PRD #742 M8: BEFORE the map, and before the `fleet.clear()` below that
      would take every deck's held history with it. `connect()` used to hand the
      ring to `dto.connection.deckId` whatever deck it actually described, which
      is the reconnect-shaped half of the cross-deck attribution — reconnecting
      while `build-box` is selected handed it the local deck's drawer.
    */
    this.selectedDeckId = dto.fleet?.[0] ?? dto.connection.deckId;
    this.adoptEvidenceDeck(this.selectedDeckId);
    const selected = dto.connection.deckId === this.selectedDeckId;
    const snapshot = mapDesktopSnapshot(
      dto,
      this.fleet.get(dto.connection.deckId),
      selected ? this.evidence : undefined,
      selected ? this.handoffs : undefined,
    );
    this.agentIndex = snapshot.agents;
    /*
      PRD #742 M4 reset membership here because this was the ONLY moment a
      departed deck could be forgotten — the crate emitted no event that said one
      had left. M5 put `fleet` on every snapshot, so forgetting is no longer this
      call's job and `foldSnapshot` does it on every arrival.

      The reset stays anyway, and for a reason that outlives the one it replaced:
      `desktop_bootstrap` is a fresh statement of the whole world, and a
      `connect()` that MERGED would carry a stale deck across a reconnect that
      the user reached for precisely because the app looked wrong. Every
      still-observed deck reinstates itself on its watcher's next emit —
      immediate for a newly established one, bounded by the crate's reconcile
      interval for a quiet one.
    */
    this.fleet.clear();
    this.fleet.set(dto.connection.deckId, snapshot);
    this.adoptUnconfigured(dto);
    this.adoptObserved(dto);
    return this.fleetView();
  }

  /**
   * Fold one deck's snapshot into the fleet and answer the new whole.
   *
   * The previous snapshot passed to `mapDesktopSnapshot` is THIS DECK'S, never
   * the last one to arrive: it carries the per-agent transcripts forward, and
   * agent ids are per-daemon monotonic integers, so folding deck B's arrival
   * against deck A's previous state would graft one machine's scrollback onto
   * another machine's agent of the same id.
   */
  private foldSnapshot(dto: DesktopSnapshotDto): DeckFleet {
    const deckId = dto.connection.deckId;
    /*
      PRD #742 M5: the selection comes off the wire now, and it is read BEFORE
      the fold because `selected` below decides which deck gets the evidence
      ring and which deck the hook-event resolver indexes.

      `fleet[0]` is the crate's own `resolve()`, restated on every snapshot. M4
      re-learnt the selection at `connect()` alone, which is why a settings save
      that moved the selection had to re-handshake to be believed.
    */
    if (dto.fleet?.length) this.selectedDeckId = dto.fleet[0];
    this.adoptUnconfigured(dto);
    this.adoptObserved(dto);
    /*
      PRD #742 M8: and the ring follows the selection, rather than being handed
      to whoever the selection now names. Before this the ring was global, so a
      switch to `build-box` mapped its first snapshot carrying every hook event
      recorded while local was selected.
    */
    if (this.selectedDeckId !== undefined) {
      this.adoptEvidenceDeck(this.selectedDeckId);
    }
    /*
      The evidence ring and the handoff edges are the SELECTED deck's — the
      only deck whose events this bridge records at all, since the stamped
      `desktop://daemon-event` filter drops the rest — so they are handed to
      that deck's snapshot and to no other. Passing them to every deck would
      hang one machine's hook history off another machine's snapshot; an
      omitted argument leaves each other deck with whatever it already had,
      which on a first arrival is nothing.
    */
    const selected = deckId === this.selectedDeckId;
    const mapped = mapDesktopSnapshot(
      dto,
      this.fleet.get(deckId),
      selected ? this.evidence : undefined,
      selected ? this.handoffs : undefined,
    );
    this.fleet.set(deckId, mapped);
    /*
      AFTER the set, deliberately: a watcher that is itself being torn down can
      land one last snapshot whose `fleet` — read fresh from the crate's applied
      document — no longer names its own deck. Pruning first would delete the
      entry and then this line would put it straight back. Pruning last drops
      that deck on the emit that announced its own departure.
    */
    this.pruneFleet(dto.fleet);
    // The hook-event resolver is the deck screen's, and the deck screen is
    // single-deck — so it indexes the SELECTED deck alone. Indexing the fleet
    // would let a colliding agent id resolve an event to the wrong machine's
    // agent, which is the same mislabel the composite key exists to stop.
    if (selected) this.agentIndex = mapped.agents;
    return this.fleetView();
  }

  async subscribe(onFleet: FleetListener, onTerminal: TerminalListener): Promise<Unsubscribe> {
    const { listen } = await import("@tauri-apps/api/event");
    this.terminalListener = onTerminal;
    this.fleetListener = onFleet;
    this.pendingTerminal.forEach((events) => events.forEach((event) => onTerminal(event)));
    this.pendingTerminal.clear();
    const emit = (dto: DesktopSnapshotDto) => onFleet(this.foldSnapshot(dto));
    const stopSnapshot = await listen<DesktopSnapshotDto>("desktop://snapshot", (event) => {
      // PRD #745 M7: a snapshot reports what the daemon owns, which says nothing
      // about what is on screen — so re-declare the set the UI last declared,
      // NEVER the fleet the snapshot carries. This re-establishes "everything
      // shown is attached" after a daemon `end`/`error` state event dropped one
      // of them, which no re-render can do because a dead session does not
      // change the derived shown set. It is a no-op whenever the invariant
      // already holds: `attachAgents` filters on `attached`.
      //
      // The invariant is "everything SHOWN is attached", not "everything
      // attached is alive" — a warm terminal whose session dies stays dead
      // until it is shown again. Deliberate: nobody is looking at it, and
      // healing it off screen would spend a socket and a scrollback replay for
      // nothing.
      emit(event.payload);
      void this.attachAgents(Array.from(this.shown.values()));
    });
    // The daemon emits a coalesced snapshot after each event, but not every hook
    // event produces one within the coalescing window; republishing the last
    // mapped snapshot keeps the drawer current without waiting for the next.
    const stopDaemonEvent = await listen<unknown>("desktop://daemon-event", (event) => {
      // PRD #742 M3 stamped every payload with the deck it came from, and this
      // is the reader that stamp was for: the evidence drawer and the handoff
      // rail are the DECK SCREEN's, which DECISION 1 keeps single-deck, so an
      // event from a deck the screen is not on is dropped rather than folded
      // into another machine's drawer. An UNSTAMPED payload is kept — a fixture
      // sends none, and an older crate sent none either.
      if (!this.isSelectedDeckEvent(event.payload)) return;
      // Recorded BEFORE the guards below, exactly as it was when the guard was
      // `!latest`: an event that lands before the first snapshot still belongs
      // in the ring, and dropping it would lose the drawer's earliest entries.
      const changed = this.recordDaemonEvent(event.payload);
      const selectedDeckId = this.selectedDeckId;
      const selected = selectedDeckId === undefined ? undefined : this.fleet.get(selectedDeckId);
      if (!changed || selectedDeckId === undefined || selected === undefined) return;
      this.fleet.set(selectedDeckId, { ...selected, evidence: this.evidence, handoffs: this.handoffs });
      onFleet(this.fleetView());
    });
    const stopTerminalState = await listen<DesktopTerminalStateDto>("desktop://terminal-state", (event) => {
      if (event.payload.state === "attached") return;
      if (this.sessionKeys.has(event.payload.sessionId)) {
        this.handleTerminalState(event.payload);
        return;
      }
      /*
        No installed session answers to this id. It can still belong to an
        attach that has not finished installing, and the only identity that
        attach has published yet is the agent NAME — the session id is minted on
        the far side of the await. Matching on the name here is therefore both
        necessary and safe: the queued event is re-checked against the session
        id and generation by `handleTerminalState` once the attach activates, so
        a same-name event from another deck is dropped there rather than acted
        on. Deliberately narrowed to pending attempts for exactly that reason.
      */
      for (const [key, pending] of this.pendingAttachments) {
        if (pending.lifecycle !== this.lifecycle) continue;
        if (this.sessions.has(key)) continue;
        if ((this.shown.get(key) ?? this.warm.get(key))?.agentId !== event.payload.agentId) continue;
        pending.stateEvents.push(event.payload);
      }
    });
    // PRD #882: the daemon changed this agent's applied geometry. Route it to
    // the tile so it reshapes its grid. Gated on the session AND generation
    // matching, exactly like the state listener above: a frame for a session
    // that has been replaced would otherwise resize the tile now showing a
    // different attach.
    const stopTerminalGeometry = await listen<DesktopTerminalGeometryDto>("desktop://terminal-geometry", (event) => {
      // Resolved through the session id, and the deck it answers with is the
      // one that session was CREATED against (issue #1116's open item 4). It
      // used to look the agent up by bare id and notify with the current
      // `selectedDeckId`, so a late frame from the deck the user had just left
      // recorded that machine's dimensions as the new deck's — and the next
      // attach submitted them to it, which the daemon's viewer size policy
      // turns into a real reflow of that PTY and of every other client watching
      // it.
      const key = this.sessionKeys.get(event.payload.sessionId);
      const session = key === undefined ? undefined : this.sessions.get(key);
      if (
        key === undefined
        || !session
        || session.result.sessionId !== event.payload.sessionId
        || session.result.generation !== event.payload.generation
      ) return;
      const target = session.target;
      this.appliedGeometry.set(key, { target, rows: event.payload.rows, cols: event.payload.cols });
      // Same reasoning as the attach path: reshape the live grid synchronously,
      // because output keeps arriving while a React state update waits for its
      // commit, and those bytes were drawn for the new geometry.
      try {
        const terminal = getTerminal(target.deckId, target.agentId);
        if (terminal && (terminal.cols !== event.payload.cols || terminal.rows !== event.payload.rows)) {
          terminal.resize(event.payload.cols, event.payload.rows);
        }
      } catch {
        // A tile mid-mount can reject a resize; the effect reconciles it.
      }
      this.geometryListeners.forEach((listener) => listener(target.agentId, event.payload.rows, event.payload.cols, target.deckId));
    });
    return () => {
      stopSnapshot();
      stopDaemonEvent();
      stopTerminalState();
      stopTerminalGeometry();
      if (this.terminalListener === onTerminal) this.terminalListener = undefined;
    };
  }

  async runAction(action: DeckAction): Promise<DeckActionResult> {
    const invoke = await this.getInvoke();
    if (action.type === "start_agent" || action.type === "start_orchestration" || action.type === "stop_agent" || action.type === "stop_orchestration" || action.type === "rename_agent" || action.type === "submit_text" || action.type === "start_workflow" || action.type === "stop_daemon" || action.type === "restart_daemon" || action.type === "allow_build_mismatch") {
      // `desktop_run_action` resolves with `ok: false` for a non-delivered
      // send rather than raising, so the result must be returned, not dropped.
      //
      // `start_agent` carries its target `deckId` through untouched (PRD #1223
      // M3), and so do `stop_agent` and `stop_orchestration` (U4): the crate
      // resolves it against the decks this app observes and
      // refuses anything else, so nothing here may fill it in from the
      // selection.
      //
      // A rejection is rethrown through `actionErrorFrom`: the crate's one
      // structured failure — a launch whose cleanup it could not confirm (PRD
      // #1223 audit F6) — becomes a `LaunchCleanupError`, and every other
      // rejection is rethrown exactly as it arrived.
      let result: DesktopActionResultDto;
      try {
        result = await invoke<DesktopActionResultDto>("desktop_run_action", { action: action satisfies DesktopRunActionDto });
      } catch (cause) {
        throw actionErrorFrom(cause);
      }
      if (action.type === "stop_daemon" || action.type === "restart_daemon") {
        this.sessions.clear();
        this.sessionKeys.clear();
        this.attached.clear();
        this.terminalChannels.clear();
        // Nothing is attached any more, so nothing is warm. `shown` is left
        // alone on purpose: it mirrors what the UI is displaying, which a
        // daemon stop does not change, and re-declaring it re-attaches.
        this.warm.clear();
        this.lifecycle += 1;
      }
      // The crate already returned `agentId` for every agent-scoped action and
      // this dropped it, which left a started agent's id — the one thing the
      // caller needs to open its pane — unreadable (#1041).
      const agentId = typeof result?.agentId === "string" ? result.agentId : undefined;
      return { ok: result?.ok !== false, sendResult: result?.sendResult, message: result?.message, ...(agentId === undefined ? {} : { agentId }) };
    }
    if (action.type === "start_daemon") {
      const dto = await invoke<DesktopSnapshotDto>("desktop_bootstrap", { options: { startIfMissing: true } });
      if (dto.connection.status !== "connected") {
        throw new Error(dto.connection.error ?? "The local deck did not become connected.");
      }
      // PRD #745 M7: starting the daemon no longer attaches its whole fleet
      // either — this was the third eager call site, and the one reachable
      // without a snapshot event at all.
      return { ok: true };
    }
    throw new Error("This orchestration control is available in the fixture preview but is not yet exposed by the live deck.");
  }

  /**
   * Settings live in `desktop.toml`, read and written by the Rust core — never
   * in localStorage, which is invisible to the user, uneditable outside the
   * app, cleared by a webview data reset, and the wrong place for anything
   * #802 later wants to reference.
   *
   * Loading never fails on the Rust side, so a rejection here can only be an
   * IPC-level failure. It **propagates**, like a failed *save* does — the
   * defaults still keep the app usable, but `useDesktopSettings` supplies them
   * rather than this method fabricating a document.
   *
   * That distinction used to not exist and it now matters (issue #845). This
   * method answered a failure with `DEFAULT_DESKTOP_SETTINGS`, mode `system`,
   * which is indistinguishable from a real document saying "follow the OS" —
   * so a caller could not tell "the user chose System" from "we never found
   * out". Since the app now applies the stored choice to the document root
   * *before this bundle runs*, that ambiguity had a visible cost: one dropped
   * IPC call and the effect would clear a Light or Dark palette that had been
   * read successfully, from the same file, seconds earlier. Both callers land
   * on the same defaults either way; only the caller can tell them apart.
   */
  async getSettings(): Promise<DesktopSettingsSnapshotDto> {
    const invoke = await this.getInvoke();
    const snapshot = normalizeDesktopSettingsSnapshot(await invoke<DesktopSettingsSnapshotDto>("desktop_get_settings"));
    this.lastEndpoints = endpointsFingerprint(snapshot.settings);
    return snapshot;
  }

  async saveSettings(settings: DesktopSettingsDto): Promise<DesktopSettingsDto> {
    const invoke = await this.getInvoke();
    const written = normalizeDesktopSettings(await invoke<DesktopSettingsDto>("desktop_set_settings", { settings }));
    const fingerprint = endpointsFingerprint(written);
    // An unspecified section is not a change and must not become the baseline
    // either: recording the sentinel would make the NEXT real edit compare
    // against it and re-establish the fleet for nothing.
    const known = fingerprint !== UNSPECIFIED_ENDPOINTS;
    const moved = known && this.lastEndpoints !== undefined && this.lastEndpoints !== fingerprint;
    if (known) this.lastEndpoints = fingerprint;
    /*
      PRD #742 M4 needed this for CORRECTNESS and M5 does not, so the reason is
      rewritten rather than inherited — and the call stays.

      M4's version: the crate emitted no membership signal at all, so unticking a
      deck left its last-known agents frozen on the overview looking live, and
      moving the selection left the app believing the old deck was still the
      selected one. Re-connecting was the only way to learn either, because
      `desktop_bootstrap` was the one place the fleet map was reset and the one
      statement of `resolve()` the frontend ever received.

      M5 put both on the wire: `DesktopSnapshotDto.fleet` names the observed set
      and leads with the selected deck, so `foldSnapshot` prunes and re-learns on
      every arrival, from any deck. The screen is now self-correcting whether or
      not this fires.

      What it still buys is PROMPTNESS, and the number is what makes it worth a
      handshake. The self-correction is bounded by the crate's `RECONCILE_INTERVAL`
      — 5 seconds — because a quiet deck's watcher emits on that timer and the
      `All` -> `local` case takes `apply_selection`'s no-emit path entirely. Five
      seconds of a deck the user just removed still sitting on their overview
      reads as the app ignoring them. One handshake against a deck they are
      actively editing is the cheapest moment this app ever spends one.

      Gated on the section having MOVED, so an appearance save — which sends the
      whole document too — costs no handshake, and `undefined` (nothing read or
      written yet on this bridge) never counts as a move.
    */
    if (moved) {
      const fleet = await this.connect().catch(() => undefined);
      if (fleet) this.fleetListener?.(fleet);
    }
    return written;
  }

  /**
   * PRD #741 M10. The reply is a classified report and is rendered as text, so
   * it is not run through a normaliser: there is no field here a malformed
   * value could reach state or storage through, and the crate has already
   * scrubbed every string it carries. The panel bounds what it renders.
   */
  async testEndpoint(settings: DesktopSettingsDto, selection: string): Promise<EndpointTestReportDto> {
    const invoke = await this.getInvoke();
    return invoke<EndpointTestReportDto>("desktop_test_endpoint", { settings, selection });
  }

  /**
   * The three credential commands (PRD #802 M4), and the one that is absent.
   *
   * There is no `loadSecret` and there must not be. The crate exposes no
   * command that returns a stored credential to this side, because a value
   * reaching here is one `JSON.stringify` from the `localStorage` half of PRD
   * #803's rule. `SecretStore::load` exists Rust-side, where M5's and M7's
   * backends make their network call — which is where the CSP already forces
   * every network hop, so nothing over here needs the value.
   *
   * Nothing is normalised on the way back: `SecretStatusDto` is a boolean plus
   * a sentence the crate has already scrubbed, and there is no field a
   * malformed value could reach state or storage through. The panel bounds what
   * it renders.
   */
  async secretStatus(id: VoiceSecretId): Promise<SecretStatusDto> {
    const invoke = await this.getInvoke();
    return invoke<SecretStatusDto>("desktop_secret_status", { id });
  }

  async storeSecret(id: VoiceSecretId, secret: string): Promise<SecretStatusDto> {
    const invoke = await this.getInvoke();
    return invoke<SecretStatusDto>("desktop_store_secret", { id, secret });
  }

  async forgetSecret(id: VoiceSecretId): Promise<SecretStatusDto> {
    const invoke = await this.getInvoke();
    return invoke<SecretStatusDto>("desktop_forget_secret", { id });
  }

  /**
   * PRD #802 M6 — the screen the webview has stated, held until the next
   * resolve reads it.
   *
   * Held here rather than sent as its own IPC call: a declaration that crossed
   * the boundary on every navigation would be a message per screen change to
   * serve one message per utterance, and the Rust command takes the screen as a
   * parameter anyway. What the seam buys is that `resolveVoice` has one
   * argument — see `DeckBridge.declareVoiceScreen`.
   */
  private voiceScreen: VoiceScreen = "deck";
  /** PRD #1223 — the directory browser declared with that screen, if any. */
  private voiceDirectories: VoiceDirectoriesDto | undefined;
  /** PRD #1223 — the New agent dialog declared with it, while it is open. */
  private voiceNewAgent: VoiceNewAgentDto | undefined;

  declareVoiceScreen(screen: VoiceScreen, directories?: VoiceDirectoriesDto, newAgent?: VoiceNewAgentDto): void {
    this.voiceScreen = screen;
    this.voiceDirectories = directories;
    this.voiceNewAgent = newAgent;
  }

  async resolveVoice(utterance: string): Promise<VoiceResultDto> {
    const invoke = await this.getInvoke();
    return invoke<VoiceResultDto>("desktop_voice_resolve", { utterance, screen: this.voiceScreen, directories: this.voiceDirectories ?? null, newAgent: this.voiceNewAgent ?? null });
  }

  /**
   * The screen is passed rather than read off {@link declareVoiceScreen}'s
   * held value, which is the one place these two verbs differ deliberately.
   *
   * The declaration exists to fix which screen ONE utterance is judged against
   * across a round trip nobody can order from the webview. A list has no such
   * round trip to be wrong about — the caller knows the screen it is asking for
   * and wants that one — so borrowing the held value would couple the overlay
   * to whether an utterance happened to be in flight.
   */
  async voiceCommands(screen: VoiceScreen, directories?: VoiceDirectoriesDto, newAgent?: VoiceNewAgentDto): Promise<VoiceCommandDto[]> {
    const invoke = await this.getInvoke();
    return invoke<VoiceCommandDto[]>("desktop_voice_commands", { screen, directories: directories ?? null, newAgent: newAgent ?? null });
  }

  async voiceStart(): Promise<VoiceStatusDto> {
    const invoke = await this.getInvoke();
    return invoke<VoiceStatusDto>("desktop_voice_start");
  }

  async voiceStop(): Promise<VoiceTranscriptionDto> {
    const invoke = await this.getInvoke();
    return invoke<VoiceTranscriptionDto>("desktop_voice_stop");
  }

  async voiceStatus(): Promise<VoiceStatusDto> {
    const invoke = await this.getInvoke();
    return invoke<VoiceStatusDto>("desktop_voice_status");
  }

  async voiceCancel(): Promise<VoiceStatusDto> {
    const invoke = await this.getInvoke();
    return invoke<VoiceStatusDto>("desktop_voice_cancel");
  }

  /**
   * Resolved by composite VALUE, never by object identity: the caller allocates
   * a fresh `{ deckId, agentId }` on every render, so a `Map` keyed on the
   * object would find nothing in production while passing any test that reused
   * one reference.
   */
  async sendTerminalInput(target: AgentTarget, data: string): Promise<void> {
    const invoke = await this.getInvoke();
    const session = this.sessions.get(agentKey(target.deckId, target.agentId));
    if (!session) throw new Error(`Terminal for ${target.agentId} is not attached.`);
    await invoke("desktop_terminal_write", { sessionId: session.result.sessionId, data: Array.from(new TextEncoder().encode(data)) });
  }

  onTerminalGeometry(listener: (agentId: string, rows: number, cols: number, deckId?: string) => void): () => void {
    this.geometryListeners.add(listener);
    // Replay what is already known: an agent can have been constrained by
    // another client long before this tile mounted, and the push that said so
    // is not repeated. Each entry carries the deck its session was created
    // against, so the replay names the producer rather than the selection.
    this.appliedGeometry.forEach((geometry) => listener(geometry.target.agentId, geometry.rows, geometry.cols, geometry.target.deckId));
    return () => {
      this.geometryListeners.delete(listener);
    };
  }

  async setZoom(level: number): Promise<number> {
    const invoke = await this.getInvoke();
    // Snapped here as well as Rust-side, so the number this resolves with is
    // the number that was applied even though the command also snaps. The
    // command's own snap is the one that matters for safety; this one is so the
    // caller does not have to re-derive it from a reply it would otherwise
    // have to trust.
    return clampZoom(await invoke<number>("desktop_set_zoom", { level: clampZoom(level) }));
  }

  async resizeTerminal(target: AgentTarget, cols: number, rows: number): Promise<void> {
    if (cols < 1 || rows < 1) return;
    const key = agentKey(target.deckId, target.agentId);
    this.pendingResizes.set(key, { cols, rows });
    this.scheduleResize(key);
    await Promise.resolve();
  }

  private scheduleResize(key: string): void {
    if (this.resizeFrames.has(key) || this.resizeInFlight.has(key)) return;
    const frame = window.requestAnimationFrame(() => {
      this.resizeFrames.delete(key);
      void this.flushResize(key);
    });
    this.resizeFrames.set(key, frame);
  }

  private async flushResize(key: string): Promise<void> {
    if (this.resizeInFlight.has(key)) return;
    const lifecycle = this.lifecycle;
    const size = this.pendingResizes.get(key);
    const session = this.sessions.get(key);
    if (!size || !session) return;
    this.pendingResizes.delete(key);
    this.resizeInFlight.add(key);
    try {
      const invoke = await this.getInvoke();
      // The session id carries the deck: the crate resolves the daemon from the
      // endpoint this session was attached over, so a resize can never reach
      // another machine's same-id agent however the selection has moved.
      await invoke("desktop_terminal_resize", { sessionId: session.result.sessionId, cols: size.cols, rows: size.rows });
    } finally {
      if (lifecycle === this.lifecycle) {
        this.resizeInFlight.delete(key);
        if (this.pendingResizes.has(key)) this.scheduleResize(key);
      }
    }
  }

  async listProjects(): Promise<DaemonProjectListing> {
    const invoke = await this.getInvoke();
    return invoke<DaemonProjectListing>("desktop_list_projects");
  }

  async resolveProject(path: string): Promise<DaemonResolvedProject> {
    const invoke = await this.getInvoke();
    return invoke<DaemonResolvedProject>("desktop_resolve_project", { path });
  }

  /**
   * PRD #1223 M4. The deck and the path go through untouched: the crate
   * resolves `deckId` against the decks this app observes, and `path` is the
   * deck's own spelling or the user's typing — nothing here fills either in.
   */
  async listDirectories(deckId: string, path?: string): Promise<DeckDirectoryListing> {
    const invoke = await this.getInvoke();
    return invoke<DeckDirectoryListing>("desktop_list_directories", { deckId, path: path ?? null });
  }

  async newAgentOptions(deckId: string): Promise<NewAgentOptions> {
    const invoke = await this.getInvoke();
    return invoke<NewAgentOptions>("desktop_new_agent_options", { deckId });
  }

  /** PRD #1223 M6. The deck and the path go through untouched, as for {@link listDirectories}. */
  async newAgentOrchestrations(deckId: string, path: string): Promise<NewAgentOrchestrations> {
    const invoke = await this.getInvoke();
    return invoke<NewAgentOrchestrations>("desktop_new_agent_orchestrations", { deckId, path });
  }

  async dispose(): Promise<void> {
    this.lifecycle += 1;
    const invoke = this.invoke;
    const sessions = Array.from(this.sessions.values());
    this.resizeFrames.forEach((frame) => window.cancelAnimationFrame(frame));
    this.attached.clear();
    this.sessions.clear();
    this.sessionKeys.clear();
    this.terminalChannels.clear();
    this.pendingAttachments.clear();
    this.pendingTerminal.clear();
    this.pendingResizes.clear();
    this.resizeFrames.clear();
    this.resizeInFlight.clear();
    // PRD #882: the applied geometries belonged to sessions this dispose just
    // dropped. Keeping them would hand the next attach a stale viewport to
    // declare, which under a smallest-wins policy would shrink the agent to a
    // pane nobody is looking at any more.
    this.appliedGeometry.clear();
    this.geometryListeners.clear();
    this.shown.clear();
    this.warm.clear();
    this.clearAttachSuppression();
    this.terminalListener = undefined;
    if (!invoke) return;
    await Promise.allSettled(sessions.map((session) => invoke("desktop_terminal_detach", { sessionId: session.result.sessionId })));
  }
}

export function selectRuntimeMode(): RuntimeMode {
  const params = new URLSearchParams(window.location.search);
  const configured = import.meta.env.VITE_DECK_TRANSPORT;
  if (params.get("fixture") === "1" || configured === "fixture") return "fixture";
  if (params.get("live") === "1" || configured === "live") return "live";
  return window.__TAURI_INTERNALS__ ? "live" : "fixture";
}

export function createDeckBridge(mode = selectRuntimeMode()): DeckBridge {
  return mode === "live" ? new TauriDeckBridge() : new FixtureDeckBridge();
}
