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
 * The reader is told the switch and the agents to read
 * ({@link DeckReader.update}) whenever either changes. An agent that appears
 * gets a session, one that goes has its session stopped, and a change of deck
 * is the same thing: the old deck's agents go and the new deck's come.
 * Changing panes or screens changes neither, so it never ends reading.
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
 * ({@link DeckReader}'s `openPane`): the bare one when that agent's pane is
 * the one open at that moment, the named one otherwise.
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
  | { kind: "gone" };

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

export interface DeckReaderDeps {
  /** Subscribe to one agent's turn events; each finished sentence arrives on `onSentence`. */
  start: (target: ReadingTarget, onSentence: (sentence: ReadingSentenceDto) => void) => Promise<ReadingStartDto>;
  /** End the subscription `start` answered with `session`. */
  stop: (session: number) => Promise<void>;
  speech: Pick<SpeechQueue, "say" | "interrupt">;
  /** The agent whose pane is open right now, asked when a sentence is said. */
  openPane?: () => { deckId: string; agentId: string } | undefined;
  /** A sentence about reading itself that the voice row shows; it is spoken too. */
  onProblem?: (sentence: string) => void;
}

/** The key one agent incarnation is read under, and speaks under. */
export function readingKey(agent: ReadingAgent): string {
  return `${agent.deckId}\u0000${agent.agentId}\u0000${agent.incarnation ?? ""}`;
}

interface Session {
  target: ReadingAgent;
  /** Absent while the start is in progress. */
  session?: number;
}

interface DeckProblem {
  /** The deck's agents when the problem was met; another set retries. */
  agents: string;
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
   * The switch and the agents to read. A change of the switch says "Reading
   * on." or "Reading off." when `announce` (not on the first render, which
   * is not a change the user made); a change of agents starts and stops
   * sessions.
   */
  update(enabled: boolean, agents: readonly ReadingAgent[], announce = true): void {
    this.wanted = new Map(agents.map((agent) => [readingKey(agent), agent]));
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
    if (!this.enabled) return;
    if (!this.active) {
      this.activate(false);
      return;
    }
    this.refused.clear();
    this.reconcile();
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
      if (session.session !== undefined) void this.deps.stop(session.session).catch(() => undefined);
    }
  }

  private forget(): void {
    this.closed.clear();
    this.refused.clear();
    this.failed.clear();
    this.deckProblems.clear();
    this.deckSaid.clear();
  }

  private reconcile(): void {
    for (const [key, session] of [...this.sessions]) {
      if (this.wanted.has(key)) continue;
      this.sessions.delete(key);
      if (session.session !== undefined) void this.deps.stop(session.session).catch(() => undefined);
    }
    for (const set of [this.closed, this.refused, this.failed]) {
      for (const key of [...set]) if (!this.wanted.has(key)) set.delete(key);
    }
    for (const [deckId, problem] of [...this.deckProblems]) {
      if (problem.agents !== this.deckAgents(deckId)) this.deckProblems.delete(deckId);
    }
    for (const [key, agent] of this.wanted) {
      if (this.sessions.has(key) || this.closed.has(key) || this.refused.has(key) || this.failed.has(key) || this.deckProblems.has(agent.deckId)) continue;
      void this.startOne(key, agent);
    }
  }

  /** The deck's agents as one comparable value. */
  private deckAgents(deckId: string): string {
    return [...this.wanted.values()].filter((agent) => agent.deckId === deckId).map(readingKey).sort().join("\u0001");
  }

  private async startOne(key: string, agent: ReadingAgent): Promise<void> {
    const record: Session = { target: agent };
    this.sessions.set(key, record);
    const current = () => this.sessions.get(key) === record;
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
      if (current()) record.session = answer.session;
      else void this.deps.stop(answer.session).catch(() => undefined);
      return;
    }
    if (!current()) return;
    this.sessions.delete(key);
    if (answer.kind === "not_enabled") {
      this.refused.add(key);
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

  private heard(key: string, record: Session, sentence: ReadingSentenceDto): void {
    if (sentence.kind === "ended") {
      this.consentOff();
      return;
    }
    if (sentence.kind === "closed") {
      // Nothing cut off: the last summary is already queued and is heard.
      this.sessions.delete(key);
      this.closed.add(key);
      return;
    }
    const { deckId, agentId } = record.target;
    const text = () => {
      const open = this.deps.openPane?.();
      return open !== undefined && open.deckId === deckId && open.agentId === agentId ? (sentence.bare ?? sentence.text) : sentence.text;
    };
    /* A turn's summary replaces a waiting summary for the same agent (D6),
       and only that agent's: another agent's news is never dropped for it.
       Each permission prompt or error waits under a key of its own, so
       nothing later drops what the user has to act on. */
    this.deps.speech.say(sentence.kind === "turn" ? key : `${key}\u0000alert\u0000${++this.alerts}`, text);
  }

  private problem(sentence: string): void {
    this.deps.speech.say(`${READING_VOICE_KEY}\u0000problem\u0000${++this.problems}`, sentence);
    this.deps.onProblem?.(sentence);
  }
}
