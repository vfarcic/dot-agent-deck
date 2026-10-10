/**
 * PRD #1497 — reading's webview half: which agents are read, and what the app
 * says about it.
 *
 * # One switch, deck-wide (decisions 1–3 of 2026-10-09)
 *
 * Reading has one state: Settings → Voice → **Reading**. "reading on" and
 * "reading off" flip that same switch (`VoiceControlPanel`, through the same
 * save as Settings). While it is on, {@link DeckReader} reads every agent on
 * the deck being viewed, on any screen: the Rust side subscribes to one
 * agent's turn events per session and sends back one finished sentence per
 * event — a turn's summary, or a permission prompt or error announced at once
 * — and this hands each to the speech queue. The agent's reply never reaches
 * here; only the sentence does.
 *
 * The reader is told the switch, the agents to read and the deck being viewed
 * ({@link DeckReader.update}) whenever any changes. An agent that appears
 * gets a session and one that goes has its session stopped. A change of deck
 * stops the old deck's sessions too, and also drops whatever the app still
 * had to say about that deck's agents, cutting off a sentence about one of
 * them mid-way (audit A7): the user is no longer looking at that deck. An
 * agent that exits on the deck being viewed is different: its session runs on
 * until its events close, for at most {@link EXITED_DRAIN_MS}, so a last
 * summary still being made when the deck stops listing the agent is heard
 * (re-audit R2). Changing panes or screens changes none of these, so it never
 * ends reading.
 *
 * # Starts that are refused for now
 *
 * A start refused because the switch was not yet on on disk is tried again
 * when a save reports it on, including when that report arrived while the
 * start was still being answered (audit A1). A window that could not install
 * the listener those reports arrive on ({@link DeckReader.consentOnUnheard})
 * asks again on a bounded schedule instead, and once that runs out says the
 * refusal, so reading never waits silently on a report that cannot reach it
 * (PR #1617's Qodo review). A start refused because the
 * agent's deck already serves as many readers as it allows (a deck limits how
 * many agents all windows read at once) is tried again a bounded number of
 * times, sooner when one of this window's sessions on that deck ends (audit
 * A4); the limit is said once per deck.
 *
 * # Turning it off
 *
 * The switch turned off — by voice, in Settings, or by a save that the Rust
 * side reports ({@link DeckReader.consentOff}) — cuts off what the app is
 * saying, drops everything queued, says "Reading off.", and stops every
 * session. Nothing is carried over: a later "reading on" starts from that
 * moment, never with a backlog (D3).
 *
 * An agent whose events end — it exited, or its deck went away — arrives as a
 * `closed` sentence after its last summary. That agent's session ends, and
 * nothing is cut off: its last summary is still heard.
 *
 * # What it does NOT end
 *
 * "stop" and "quiet" silence the app's speech and leave reading on: the next
 * turn is read as usual. Voice off turns the microphone off and leaves
 * reading on as well: the switch is reading's only state.
 *
 * # Who is named (decision 4 of 2026-10-09)
 *
 * Every sentence comes in two forms, naming the agent and not. Which one is
 * said is decided when the speech queue takes the sentence up to say it
 * ({@link DeckReader}'s `openPane`), and checked again right before provider
 * audio plays: the bare one when that agent's pane is the one open at that
 * moment, the named one otherwise.
 */

import type { SpeechQueue } from "./speech";

/** One agent to read: an agent on the deck being viewed. */
export interface ReadingTarget {
  deckId: string;
  agentId: string;
  /** What the deck calls the agent; named in a sentence about it (decision 4). */
  label: string;
}

/** An agent the reader is told about. */
export interface ReadingAgent extends ReadingTarget {
  /**
   * The agent's incarnation (its spawn time), when the deck reports one: a
   * daemon can replace an agent under the same id, and the new one is read
   * afresh.
   */
  incarnation?: number;
}

/** What `desktop_voice_reading_start` answers. */
export type ReadingStartDto =
  | { kind: "started"; session: number }
  | { kind: "not_enabled"; sentence: string }
  /** `scope` is `deck` when it is about the whole deck — said once for it. */
  | { kind: "unavailable"; sentence: string; scope?: "agent" | "deck" }
  /** The agent left its deck before it could be read; nothing is said. */
  | { kind: "gone" }
  /** The agent's deck serves as many readers as it allows: tried again later. */
  | { kind: "busy"; sentence: string };

/**
 * One sentence to speak, from `desktop_voice_reading_start`'s channel — or,
 * as `ended`, word that the Rust side ended the session (the Settings switch
 * was turned off), or, as `closed`, that the agent's events ended (it exited).
 * Neither of those two is spoken.
 */
export interface ReadingSentenceDto {
  kind: "turn" | "permission" | "blocked" | "ended" | "closed";
  /** The sentence naming the agent. */
  text: string;
  /** The same sentence without the agent's name, for when its pane is open. */
  bare?: string;
}

/** Spoken when reading is turned on. */
export const READING_ON = "Reading on.";
/** Spoken when reading is turned off, whatever turned it off. */
export const READING_OFF = "Reading off.";
/** Shown when "reading off" is said with reading already off. */
export const READING_ALREADY_OFF = "Reading is not on.";
/** Shown when "reading on" is said with reading already on. */
export const READING_ALREADY_ON = "Reading is already on.";

/** Said and shown when the app could not ask to read `label`, or got no answer. */
export function readingStartFailed(label: string): string {
  return `Reading could not start for ${label}.`;
}

/**
 * The speech queue's key for the mode's own sentences ("Reading on.",
 * "Reading off."), so a turn summary waiting for an agent never replaces one.
 */
export const READING_VOICE_KEY = "\u0000reading";

/** The speech queue's key for the one-time notice (decision 5). */
export const READING_NOTICE_VOICE_KEY = "\u0000reading\u0000notice";

/**
 * The one-time notice the first time Reading is turned on (decision 5 of
 * 2026-10-09): where each finished turn's reply goes — the Commands
 * connection's host, or, for an endpoint on this machine, that it stays here.
 */
export function readingNotice(endpoint: string): string {
  const host = endpointHost(endpoint);
  if (host === undefined) return "Each finished turn's reply is sent to the Commands service in Settings, Voice, to be summarised.";
  if (isThisMachine(host)) return "Each finished turn's reply stays on this machine: the Commands service that summarises it runs here.";
  return `Each finished turn's reply is sent to ${host} to be summarised.`;
}

function endpointHost(endpoint: string): string | undefined {
  try {
    const host = new URL(endpoint).hostname;
    return host === "" ? undefined : host;
  } catch {
    return undefined;
  }
}

/** Whether `host` (a URL's hostname) names this machine. */
export function isThisMachine(host: string): boolean {
  const name = host.toLowerCase().replace(/^\[|\]$/g, "");
  return name === "localhost" || name.endsWith(".localhost") || name === "::1" || name === "0.0.0.0" || /^127(\.\d{1,3}){3}$/.test(name);
}

/**
 * How long after a deck refused an agent for having as many readers as it
 * allows that agent is asked again, one delay per attempt. Past the last, it
 * is asked again only when one of this window's sessions on that deck ends.
 */
export const BUSY_RETRY_MS: readonly number[] = [2_000, 5_000, 15_000, 30_000, 60_000];

/**
 * How long after a start was refused because the switch was not yet on on
 * disk that agent is asked again, one delay per attempt — only in a window
 * that cannot hear a save report the switch on. Past the last, the refusal is
 * said and the agent waits for the switch to be turned off and on again.
 */
export const NOT_ENABLED_RETRY_MS: readonly number[] = [500, 2_000, 5_000, 15_000];

/** How long after one of this window's sessions on a deck ends its refused agents are asked again. */
export const CAPACITY_FREED_RETRY_MS = 1_000;

/**
 * How long a session is kept after its agent left the deck being viewed, for
 * its `closed` to arrive: the agent's last turn may still be being summarised
 * (a summary gets 10 s on the Rust side) when the deck stops listing it
 * (re-audit R2). Past this it is stopped like any other.
 */
export const EXITED_DRAIN_MS = 30_000;

export interface DeckReaderDeps {
  /** Subscribe to one agent's turn events; each finished sentence arrives on `onSentence`. */
  start: (target: ReadingTarget, onSentence: (sentence: ReadingSentenceDto) => void) => Promise<ReadingStartDto>;
  /** End the subscription `start` answered with `session`. */
  stop: (session: number) => Promise<void>;
  speech: Pick<SpeechQueue, "say" | "interrupt" | "drop">;
  /** The agent whose pane is open right now, asked when a sentence is said. */
  openPane?: () => { deckId: string; agentId: string } | undefined;
  /** A sentence about reading itself that the voice row shows; it is spoken too. */
  onProblem?: (sentence: string) => void;
  /** Run `run` after `ms`; answers how to cancel it. `setTimeout` when absent. */
  schedule?: (run: () => void, ms: number) => () => void;
}

/** The key one agent incarnation is read under, and speaks under. */
export function readingKey(agent: ReadingAgent): string {
  return `${agent.deckId}\u0000${agent.agentId}\u0000${agent.incarnation ?? ""}`;
}

interface Session {
  target: ReadingAgent;
  /** Absent while the start is in progress. */
  session?: number;
  /**
   * Set while the agent is no longer listed on a deck still viewed: the
   * session is kept for its `closed`, and this cancels the bound on that
   * wait ({@link EXITED_DRAIN_MS}).
   */
  drain?: () => void;
}

interface DeckProblem {
  /** The deck's agents when the problem was met; another set retries. */
  agents: string;
}

interface Busy {
  /** How many times the deck refused it for having as many readers as it allows. */
  attempts: number;
  /** Cancels the retry waiting to run, if one is. */
  cancel?: () => void;
}

/**
 * Reading for one window: every agent on the deck being viewed, while the
 * switch is on.
 *
 * Every start keeps its own record, so an answer that arrives after the
 * reader moved on — the agent went, or reading was turned off — is undone
 * rather than applied, and a sentence from a session that has since ended is
 * not spoken.
 */
export class DeckReader {
  /** The switch as last told. */
  private enabled = false;
  /** Whether reading runs: the switch on, and not turned off since on the Rust side. */
  private active = false;
  private wanted = new Map<string, ReadingAgent>();
  private sessions = new Map<string, Session>();
  /** Agents whose events closed: not read again while they are still listed. */
  private closed = new Set<string>();
  /** Agents refused because the switch was not yet on on disk: tried again on {@link consentOn}. */
  private refused = new Set<string>();
  /** How many times a save has reported the switch on; a start remembers it (audit A1). */
  private consents = 0;
  /** Whether a save reporting the switch on can reach this window ({@link consentOnUnheard}). */
  private hearsConsentOn = true;
  /** Agents refused because the switch was not yet on on disk, asked again on {@link NOT_ENABLED_RETRY_MS}: only while {@link hearsConsentOn} is false. */
  private unconfirmed = new Map<string, Busy>();
  /** Agents refused because their deck has as many readers as it allows (audit A4). */
  private busy = new Map<string, Busy>();
  /** The decks being viewed, as last told. */
  private viewedDecks: ReadonlySet<string> | undefined;
  /** Agents that cannot be read: not tried again while they are still listed. */
  private failed = new Set<string>();
  /** Decks whose daemon cannot be read: no agent of theirs is started. */
  private deckProblems = new Map<string, DeckProblem>();
  /** What was already said about each deck, so it is said once. */
  private deckSaid = new Map<string, string>();
  private alerts = 0;
  private problems = 0;

  constructor(private readonly deps: DeckReaderDeps) {}

  /** Whether reading is on. */
  get on(): boolean {
    return this.active;
  }

  /** The agents being read or being started, by {@link readingKey}. */
  get reading(): string[] {
    return [...this.sessions.keys()];
  }

  /**
   * The switch, the agents to read and the decks being viewed (`decks`: one,
   * or every observed deck under All Decks). A change of the switch says
   * "Reading on." or "Reading off." when `announce` (not on the first render,
   * which is not a change the user made); a change of agents starts and stops
   * sessions; a deck that leaves `decks` also has what was still to be said
   * about its agents dropped. Without `decks`, no speech is dropped.
   */
  update(enabled: boolean, agents: readonly ReadingAgent[], announce = true, decks?: readonly string[]): void {
    this.wanted = new Map(agents.map((agent) => [readingKey(agent), agent]));
    if (decks !== undefined) {
      const viewed = new Set(decks);
      const left = [...(this.viewedDecks ?? [])].filter((deckId) => !viewed.has(deckId)).map((deckId) => `${deckId}\u0000`);
      if (left.length > 0) this.deps.speech.drop((key) => left.some((prefix) => key.startsWith(prefix)));
      this.viewedDecks = viewed;
    }
    if (enabled !== this.enabled) {
      this.enabled = enabled;
      if (enabled) this.activate(announce);
      else this.deactivate(announce);
      return;
    }
    if (this.active) this.reconcile();
  }

  /**
   * A save turned the switch off — this window's or another's, reported by
   * the Rust side — or a session found it off. Reading ends here too, a start
   * in progress included.
   */
  consentOff(): void {
    if (this.active) this.deactivate(true);
  }

  /**
   * A save left the switch on: starts refused because they read the settings
   * before that save reached the disk are tried again, and reading resumes if
   * a save had turned it off while the switch here still shows it on.
   */
  consentOn(): void {
    this.consents += 1;
    if (!this.enabled) return;
    if (!this.active) {
      this.activate(false);
      return;
    }
    this.refused.clear();
    this.reconcile();
  }

  /**
   * This window could not install the listener {@link consentOn} is called
   * from, so no save will ever report the switch on here. A start refused
   * because the switch was not yet on on disk is then asked again on
   * {@link NOT_ENABLED_RETRY_MS} instead of waiting for that report.
   */
  consentOnUnheard(): void {
    this.hearsConsentOn = false;
    if (!this.active) return;
    for (const key of [...this.refused]) {
      const agent = this.wanted.get(key);
      this.refused.delete(key);
      if (agent !== undefined) this.unconfirmedRefusal(key, agent, undefined);
    }
  }

  /** "stop" / "quiet": silence the app now. Reading stays on. */
  quiet(): void {
    this.deps.speech.interrupt();
  }

  /** The voice surface is going away: stop every session and silence the app, saying nothing. */
  dispose(): void {
    this.enabled = false;
    this.end(false);
  }

  private activate(announce: boolean): void {
    this.active = true;
    this.forget();
    if (announce) this.deps.speech.say(READING_VOICE_KEY, READING_ON);
    this.reconcile();
  }

  private deactivate(announce: boolean): void {
    const was = this.active;
    this.end(announce && was);
  }

  /**
   * End reading, in this order: no late answer or sentence of any session is
   * applied any more; what is being said is cut off and everything queued
   * dropped; "Reading off." is said (when `announce`); every session stops.
   */
  private end(announce: boolean): void {
    this.active = false;
    const ended = [...this.sessions.values()];
    this.sessions.clear();
    this.forget();
    this.deps.speech.interrupt();
    if (announce) this.deps.speech.say(READING_VOICE_KEY, READING_OFF);
    for (const session of ended) {
      session.drain?.();
      if (session.session !== undefined) void this.deps.stop(session.session).catch(() => undefined);
    }
  }

  private forget(): void {
    this.closed.clear();
    this.refused.clear();
    this.failed.clear();
    for (const pending of [...this.busy.values(), ...this.unconfirmed.values()]) pending.cancel?.();
    this.busy.clear();
    this.unconfirmed.clear();
    this.deckProblems.clear();
    this.deckSaid.clear();
  }

  private reconcile(): void {
    for (const [key, session] of [...this.sessions]) {
      if (this.wanted.has(key)) {
        // Listed again before its session closed: read as before.
        session.drain?.();
        session.drain = undefined;
        continue;
      }
      /* Gone from a deck still viewed: it exited, and its last summary may
         still be on its way, so the session runs on to its `closed`, for at
         most EXITED_DRAIN_MS (re-audit R2). A deck no longer viewed stops it
         now. */
      if (this.exited(session.target)) {
        this.drain(key, session);
        continue;
      }
      this.sessions.delete(key);
      session.drain?.();
      if (session.session !== undefined) this.release(session);
    }
    for (const set of [this.closed, this.refused, this.failed]) {
      for (const key of [...set]) if (!this.wanted.has(key)) set.delete(key);
    }
    for (const pendings of [this.busy, this.unconfirmed]) {
      for (const [key, pending] of [...pendings]) {
        if (this.wanted.has(key)) continue;
        pending.cancel?.();
        pendings.delete(key);
      }
    }
    for (const [deckId, problem] of [...this.deckProblems]) {
      if (problem.agents !== this.deckAgents(deckId)) this.deckProblems.delete(deckId);
    }
    for (const [key, agent] of this.wanted) {
      if (this.sessions.has(key) || this.closed.has(key) || this.refused.has(key) || this.failed.has(key) || this.busy.has(key) || this.unconfirmed.has(key) || this.deckProblems.has(agent.deckId)) continue;
      void this.startOne(key, agent);
    }
  }

  /**
   * Whether `agent`, no longer listed, exited from a deck still being viewed:
   * its deck is among the decks last told, and no other incarnation of it is
   * listed — a replaced agent's old session would otherwise read the new
   * one's turns as well. Without the decks being told, nothing is known to
   * be viewed.
   */
  private exited(agent: ReadingAgent): boolean {
    if (this.viewedDecks === undefined || !this.viewedDecks.has(agent.deckId)) return false;
    for (const other of this.wanted.values()) {
      if (other.deckId === agent.deckId && other.agentId === agent.agentId) return false;
    }
    return true;
  }

  /** Keep `session` for its `closed`, and stop it if that has not come within {@link EXITED_DRAIN_MS}. */
  private drain(key: string, session: Session): void {
    if (session.drain !== undefined) return;
    session.drain = this.schedule(() => {
      session.drain = undefined;
      if (this.sessions.get(key) !== session) return;
      this.sessions.delete(key);
      if (session.session !== undefined) this.release(session);
    }, EXITED_DRAIN_MS);
  }

  /** Stop a running session, and once it has stopped, ask its deck's refused agents again. */
  private release(session: Session): void {
    const deckId = session.target.deckId;
    void this.deps.stop(session.session!).catch(() => undefined).then(() => this.capacityFreed(deckId));
  }

  /** One of this window's sessions on `deckId` ended: its agents the deck refused for being full are asked again soon. */
  private capacityFreed(deckId: string): void {
    if (!this.active) return;
    for (const [key, busy] of this.busy) {
      const agent = this.wanted.get(key);
      if (agent === undefined || agent.deckId !== deckId) continue;
      busy.cancel?.();
      busy.cancel = this.schedule(() => this.retryBusy(key), CAPACITY_FREED_RETRY_MS);
    }
  }

  private retryBusy(key: string): void {
    this.retry(this.busy, key);
  }

  /** Ask `key` again, if it is still waiting in `pendings` and still wanted. */
  private retry(pendings: Map<string, Busy>, key: string): void {
    const pending = pendings.get(key);
    if (pending === undefined) return;
    pending.cancel = undefined;
    const agent = this.wanted.get(key);
    if (!this.active || agent === undefined || this.sessions.has(key)) return;
    void this.startOne(key, agent);
  }

  private schedule(run: () => void, ms: number): () => void {
    if (this.deps.schedule !== undefined) return this.deps.schedule(run, ms);
    const timer = setTimeout(run, ms);
    return () => clearTimeout(timer);
  }

  /** The deck's agents as one comparable value. */
  private deckAgents(deckId: string): string {
    return [...this.wanted.values()].filter((agent) => agent.deckId === deckId).map(readingKey).sort().join("\u0001");
  }

  private async startOne(key: string, agent: ReadingAgent): Promise<void> {
    const record: Session = { target: agent };
    this.sessions.set(key, record);
    const current = () => this.sessions.get(key) === record;
    const consents = this.consents;
    let answer: ReadingStartDto;
    try {
      answer = await this.deps.start({ deckId: agent.deckId, agentId: agent.agentId, label: agent.label }, (sentence) => {
        if (current()) this.heard(key, record, sentence);
      });
    } catch {
      if (!current()) return;
      this.sessions.delete(key);
      this.failed.add(key);
      this.problem(readingStartFailed(agent.label));
      return;
    }
    if (answer.kind === "started") {
      if (current()) {
        record.session = answer.session;
        for (const pendings of [this.busy, this.unconfirmed]) {
          pendings.get(key)?.cancel?.();
          pendings.delete(key);
        }
      } else {
        void this.deps.stop(answer.session).catch(() => undefined);
      }
      return;
    }
    if (!current()) return;
    this.sessions.delete(key);
    if (answer.kind === "not_enabled") {
      /* A save reported the switch on while this start was being answered,
         so its settings read may predate that save: ask again at once
         (audit A1). Otherwise it waits for the next such report, or, where
         no such report can arrive, is asked again on a schedule. */
      if (this.consents !== consents) void this.startOne(key, agent);
      else if (this.hearsConsentOn) this.refused.add(key);
      else this.unconfirmedRefusal(key, agent, answer.sentence);
      return;
    }
    if (answer.kind === "busy") {
      this.full(key, agent, answer.sentence);
      return;
    }
    if (answer.kind === "gone") {
      // As if its events had closed: not asked again while it is listed.
      this.closed.add(key);
      return;
    }
    if (answer.scope === "deck") {
      /* About the whole deck (an older daemon, one that did not answer): no
         other agent of it is started, and it is said once however many of
         its agents were starting. */
      this.deckProblems.set(agent.deckId, { agents: this.deckAgents(agent.deckId) });
      for (const [other, session] of [...this.sessions]) {
        if (session.target.deckId !== agent.deckId || session.session !== undefined) continue;
        this.sessions.delete(other);
      }
      if (this.deckSaid.get(agent.deckId) === answer.sentence) return;
      this.deckSaid.set(agent.deckId, answer.sentence);
      this.problem(answer.sentence);
      return;
    }
    this.failed.add(key);
    this.problem(answer.sentence);
  }

  /** The deck refused `agent` for having as many readers as it allows: say so once for the deck, and ask again later. */
  private full(key: string, agent: ReadingAgent, sentence: string): void {
    const busy = this.busy.get(key) ?? { attempts: 0 };
    busy.attempts += 1;
    busy.cancel?.();
    busy.cancel = undefined;
    this.busy.set(key, busy);
    const delay = BUSY_RETRY_MS[busy.attempts - 1];
    if (delay !== undefined) busy.cancel = this.schedule(() => this.retryBusy(key), delay);
    if (this.deckSaid.get(agent.deckId) === sentence) return;
    this.deckSaid.set(agent.deckId, sentence);
    this.problem(sentence);
  }

  /**
   * `agent` was refused because the switch was not yet on on disk, in a
   * window no save can report the switch on to: ask again after the next
   * delay of {@link NOT_ENABLED_RETRY_MS}, and once those run out, stop asking
   * and say the refusal (`sentence`) once for the deck.
   */
  private unconfirmedRefusal(key: string, agent: ReadingAgent, sentence: string | undefined): void {
    const pending = this.unconfirmed.get(key) ?? { attempts: 0 };
    pending.attempts += 1;
    pending.cancel?.();
    pending.cancel = undefined;
    const delay = NOT_ENABLED_RETRY_MS[pending.attempts - 1];
    if (delay !== undefined) {
      this.unconfirmed.set(key, pending);
      pending.cancel = this.schedule(() => this.retry(this.unconfirmed, key), delay);
      return;
    }
    this.unconfirmed.delete(key);
    this.refused.add(key);
    if (sentence === undefined || this.deckSaid.get(agent.deckId) === sentence) return;
    this.deckSaid.set(agent.deckId, sentence);
    this.problem(sentence);
  }

  private heard(key: string, record: Session, sentence: ReadingSentenceDto): void {
    if (sentence.kind === "ended") {
      this.consentOff();
      return;
    }
    if (sentence.kind === "closed") {
      // Nothing cut off: the last summary is already queued and is heard.
      this.sessions.delete(key);
      record.drain?.();
      if (this.wanted.has(key)) this.closed.add(key);
      this.capacityFreed(record.target.deckId);
      return;
    }
    const { deckId, agentId } = record.target;
    const say = () => {
      const open = this.deps.openPane?.();
      return open !== undefined && open.deckId === deckId && open.agentId === agentId ? (sentence.bare ?? sentence.text) : sentence.text;
    };
    /* A turn's summary replaces a waiting summary for the same agent (D6),
       and only that agent's: another agent's news is never dropped for it.
       Each permission prompt or error waits under a key of its own, so
       nothing later drops what the user has to act on. */
    this.deps.speech.say(sentence.kind === "turn" ? key : `${key}\u0000alert\u0000${++this.alerts}`, { say, safe: sentence.text });
  }

  private problem(sentence: string): void {
    this.deps.speech.say(`${READING_VOICE_KEY}\u0000problem\u0000${++this.problems}`, sentence);
    this.deps.onProblem?.(sentence);
  }
}
