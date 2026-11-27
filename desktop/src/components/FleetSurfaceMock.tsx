/*
 * PRD 2557 — MOCK-UP, round 3, never merged. The fleet view as the top-level
 * agent composed it, drawn component by component through
 * `fleet-mock/fleet-catalog.md`. The renderer knows no sections, no order and
 * no operations of its own. The deck adds only the line above the view (who
 * composed it and when) and what it checks itself inside DeckRef and AgentRef.
 *
 * The images are static: inputs show their starting values (from
 * `updateDataModel`) and a PromptButton shows the text those values produce.
 */
import { useMemo, type ReactNode } from "react";
import {
  BellRing, Check, CircleHelp, Clock, Eye, FlaskConical, GitMerge, Package, Pause, Pin, Play, Send, SquareTerminal, TriangleAlert, X,
  type LucideIcon,
} from "lucide-react";
import { GitMergeIcon, IssueClosedIcon, IssueOpenedIcon, QuestionIcon, type Icon } from "@primer/octicons-react";

import { Prose, StatusPill } from "./FleetViewMock";
import { PullRequestBadge } from "./PullRequestBadge";
import { referenceView } from "../lib/agentCardsTree";
import type { AgentCardCheckedRef, AgentCardEntry } from "../types";
import { clockAt, type A2uiComponent, type A2uiMessage, type LivePullRequest, type LiveIssue } from "../data/fleetMock";
import "./AgentViewMock.css";
import "./FleetSurfaceMock.css";

export const FLEET_SURFACE_CATALOG = [
  "Text", "Column", "Row", "List", "Card", "Divider", "Icon", "Stat", "Chip", "Progress", "Chart", "Board", "Lane", "Timeline",
  "DeckRef", "AgentRef", "ChoicePicker", "TextField", "PromptButton",
] as const;

interface LiveDoc {
  captured_at: string;
  daemon_status: { LABEL: string; TOOL: string }[];
  cards: { entries: AgentCardEntry[] };
  pull_requests: Record<string, LivePullRequest>;
  issues: Record<string, LiveIssue>;
}

/* Round 3's files arrive while the scene is built (the dispatcher writes its view, the capture follows), so they are found rather than imported. */
const FILES = import.meta.glob<{ default: unknown }>("../data/fleetMock.{dispatcherView,standinView.*,live.r3}.json", { eager: true });
const file = <T,>(name: string): T | undefined => FILES[`../data/fleetMock.${name}.json`]?.default as T | undefined;

const ICONS: Record<string, LucideIcon> = {
  bell: BellRing, alert: TriangleAlert, check: Check, clock: Clock, merge: GitMerge, pause: Pause, question: CircleHelp,
  terminal: SquareTerminal, flask: FlaskConical, package: Package, eye: Eye, pin: Pin, play: Play, x: X,
};
const ISSUE_ICON: Record<string, Icon> = { open: IssueOpenedIcon, closed: IssueClosedIcon, merged: GitMergeIcon };

interface Ctx { byId: Map<string, A2uiComponent>; live: LiveDoc; data: Map<string, unknown> }

const str = (value: unknown, fallback = "") => (value === undefined || value === null ? fallback : String(value));
const ids = (value: unknown): string[] => (Array.isArray(value) ? value.map(String) : value === undefined ? [] : [String(value)]);
const tone = (value: unknown) => `tone-${str(value, "default")}`;

/** The deck's own verdict on a pull request or issue, from its check, never from the agent's words. */
function verdict(live: LiveDoc, kind: string, number: number): { text: string; tone: string } {
  if (kind === "issue") {
    const issue = live.issues[String(number)];
    return issue ? { text: issue.state, tone: issue.state === "open" ? "info" : "muted" } : { text: "checking…", tone: "muted" };
  }
  const pr = live.pull_requests[String(number)];
  if (!pr) return { text: "checking…", tone: "muted" };
  if (pr.state === "merged") return { text: "merged", tone: "ok" };
  if (pr.state === "closed") return { text: "closed", tone: "muted" };
  if (pr.mergeable === "dirty") return { text: "conflicts with main", tone: "danger" };
  if (pr.ci.failing) return { text: "CI failing", tone: "danger" };
  if (pr.ci.pending) return { text: "CI running", tone: "info" };
  if (pr.review === "changes_requested") return { text: "changes requested", tone: "danger" };
  if (pr.review !== "approved") return { text: "needs review", tone: "attention" };
  return { text: "ready to merge", tone: "ok" };
}

function SurfaceRef({ live, kind, number, detail }: { live: LiveDoc; kind: "pull_request" | "issue"; number: number; detail: string }) {
  const pr = kind === "pull_request" ? live.pull_requests[String(number)] : undefined;
  const issue = kind === "issue" ? live.issues[String(number)] : undefined;
  const url = `https://github.com/vfarcic/dot-agent-deck/${kind === "pull_request" ? "pull" : "issues"}/${number}`;
  const checked = pr ?? issue;
  const ref = {
    field: kind === "pull_request" ? "pull-request" : "issue", kind, claim: `#${number}`, repo: "vfarcic/dot-agent-deck", number, url,
    result: checked
      ? { status: "checked", state: checked.state, title: checked.title, checked_at: live.captured_at, stale: false, ...(pr ? { ci: pr.ci, review: pr.review } : {}) }
      : { status: "pending" },
  } as unknown as AgentCardCheckedRef;
  const view = referenceView(ref);
  const judged = verdict(live, kind, number);
  let badge: ReactNode;
  if ("pullRequest" in view) badge = <PullRequestBadge pullRequest={view.pullRequest} />;
  else {
    const StateIcon = ISSUE_ICON[str(issue?.state)] ?? QuestionIcon;
    badge = <span className={`pr-badge issue-badge issue-state-${str(issue?.state, "pending")}`}><StateIcon className="pr-badge-state" size={11} aria-hidden="true" /><span className="pr-badge-number">#{number}</span></span>;
  }
  return (
    <span className="fleet-ref fs-ref" title={checked?.title}>
      {badge}
      {detail !== "badge" && <span className={`fs-chip ${tone(judged.tone)}`}>{judged.text}</span>}
      {detail === "full" && pr && pr.state === "open" && <span className="fleet-ref-words">{pr.review.replace(/_/g, " ")} · {pr.ci.failing ? `${pr.failing.join(", ")} failing` : `CI ${pr.ci.passing} passing`}</span>}
    </span>
  );
}

/** "a, b and c". */
function spoken(value: unknown): string {
  const list = Array.isArray(value) ? value.map(String) : value === undefined || value === "" ? [] : [String(value)];
  return list.length <= 1 ? (list[0] ?? "") : `${list.slice(0, -1).join(", ")} and ${list[list.length - 1]}`;
}

function Node({ ctx, id, seen }: { ctx: Ctx; id: string; seen: Set<string> }): ReactNode {
  const node = ctx.byId.get(id);
  if (!node || seen.has(id)) return null;
  const path = new Set(seen).add(id);
  const kids = (list: string[]) => list.map((child) => <Node key={child} ctx={ctx} id={child} seen={path} />);
  const gap = `gap-${str(node.gap, "m")}`;
  switch (node.component) {
    case "Text": {
      const variant = str(node.variant, "body");
      const Tag = (variant === "h1" ? "h1" : variant === "h2" ? "h2" : variant === "h3" ? "h3" : variant === "quote" ? "blockquote" : "p") as "p";
      return <Tag className={`av-text v-${variant} ${tone(node.tone)}`}><Prose text={str(node.text)} /></Tag>;
    }
    case "Column": return <div className={`av-column ${gap}`}>{kids(ids(node.children))}</div>;
    case "Row":
      return (
        <div className={`av-row ${gap} align-${str(node.align, "center")}${node.wrap === false ? " no-wrap" : ""}`}>
          {ids(node.children).map((child) => {
            const weight = Number(ctx.byId.get(child)?.weight);
            return <div key={child} className="av-cell" style={weight > 0 ? { flex: `${weight} 1 0`, minWidth: 0 } : undefined}><Node ctx={ctx} id={child} seen={path} /></div>;
          })}
        </div>
      );
    case "List": return <div className={`av-list dir-${str(node.direction, "vertical")}`}>{ids(node.children).map((child) => <div key={child} className="av-list-item"><Node ctx={ctx} id={child} seen={path} /></div>)}</div>;
    case "Card": return <section className={`av-card fs-card ${tone(node.tone)}`}>{kids(ids(node.children ?? node.child))}</section>;
    case "Divider": return <hr className="av-divider" />;
    case "Icon": {
      const Glyph = ICONS[str(node.name)] ?? CircleHelp;
      return <Glyph size={14} className={`av-icon icon-${str(node.name)}`} aria-hidden="true" />;
    }
    case "Stat": return <div className={`av-stat ${tone(node.tone)}`}><strong>{str(node.value)}</strong><span>{str(node.label)}</span></div>;
    case "Chip": return <span className={`fs-chip ${tone(node.tone)}`}>{str(node.text)}</span>;
    case "Progress": {
      const max = Number(node.max) || 1;
      const value = Math.max(0, Math.min(max, Number(node.value) || 0));
      return (
        <div className={`fs-progress ${tone(node.tone)}`}>
          <div className="fs-progress-head"><span>{str(node.label)}</span><small>{value} of {max}</small></div>
          <div className="fs-progress-track"><i style={{ width: `${(value / max) * 100}%` }} /></div>
        </div>
      );
    }
    case "Chart": {
      const data = (Array.isArray(node.data) ? node.data : []) as { label?: unknown; value?: unknown; tone?: unknown }[];
      const total = data.reduce((sum, row) => sum + (Number(row.value) || 0), 0) || 1;
      const top = Math.max(1, ...data.map((row) => Number(row.value) || 0));
      if (node.kind === "donut") {
        let offset = 0;
        return (
          <figure className="fs-chart is-donut">
            {node.title !== undefined && <figcaption>{str(node.title)}</figcaption>}
            <div className="fs-donut-body">
              <svg viewBox="0 0 42 42" width="112" height="112" aria-hidden="true">
                <circle className="fs-donut-track" cx="21" cy="21" r="15.9155" />
                {data.map((row, at) => {
                  const share = ((Number(row.value) || 0) / total) * 100;
                  const arc = <circle key={at} className={`fs-donut-arc ${tone(row.tone)}`} cx="21" cy="21" r="15.9155" strokeDasharray={`${share} ${100 - share}`} strokeDashoffset={25 - offset} />;
                  offset += share;
                  return arc;
                })}
              </svg>
              <ul>{data.map((row, at) => <li key={at}><i className={`fs-dot ${tone(row.tone)}`} />{str(row.label)} <strong>{str(row.value)}</strong></li>)}</ul>
            </div>
          </figure>
        );
      }
      return (
        <figure className="fs-chart is-bar">
          {node.title !== undefined && <figcaption>{str(node.title)}</figcaption>}
          {data.map((row, at) => (
            <div key={at} className="fs-bar-row">
              <span className="fs-bar-label">{str(row.label)}</span>
              <span className="fs-bar-track"><i className={tone(row.tone)} style={{ width: `${((Number(row.value) || 0) / top) * 100}%` }} /></span>
              <strong>{str(row.value)}</strong>
            </div>
          ))}
        </figure>
      );
    }
    case "Board": return <div className="fs-board" style={{ gridTemplateColumns: `repeat(${Math.max(1, ids(node.children).length)}, minmax(0, 1fr))` }}>{kids(ids(node.children))}</div>;
    case "Lane":
      return (
        <section className={`fs-lane ${tone(node.tone)}`}>
          <header><span>{str(node.title)}</span><b>{ids(node.children).length}</b></header>
          <div className="fs-lane-body">{kids(ids(node.children))}</div>
        </section>
      );
    case "Timeline": {
      const items = (Array.isArray(node.items) ? node.items : []) as { time?: unknown; text?: unknown; tone?: unknown }[];
      return <ol className="fs-timeline">{items.map((item, at) => <li key={at}><i className={`fs-dot ${tone(item.tone)}`} /><time>{str(item.time)}</time><span><Prose text={str(item.text)} /></span></li>)}</ol>;
    }
    case "DeckRef": return <SurfaceRef live={ctx.live} kind={node.kind === "issue" ? "issue" : "pull_request"} number={Number(node.number)} detail={str(node.detail, "full")} />;
    case "AgentRef": {
      const card = ctx.live.cards.entries.find((entry) => entry.name === str(node.agent));
      const pane = ctx.live.daemon_status.find((row) => row.LABEL === str(node.agent));
      return (
        <span className="av-agent">
          <strong>{str(node.agent)}</strong>
          {card ? <StatusPill status={card.status} /> : <span className="fs-chip tone-muted">not on this deck</span>}
          {node.detail !== "name" && pane && pane.TOOL !== "-" && <small>{pane.TOOL}</small>}
        </span>
      );
    }
    case "ChoicePicker": {
      const options = ids(node.options);
      const start = ctx.data.get(str(node.path));
      const chosen = new Set(Array.isArray(start) ? start.map(String) : start === undefined ? [] : [String(start)]);
      const display = str(node.display, node.multiple ? "checkboxes" : "dropdown");
      const label = node.label !== undefined ? <span className="fs-input-label">{str(node.label)}</span> : null;
      if (display === "dropdown") {
        return <label className="fs-picker is-dropdown">{label}<select className="fs-select" defaultValue={[...chosen][0] ?? ""}><option value="" disabled>Choose…</option>{options.map((option) => <option key={option} value={option}>{option}</option>)}</select></label>;
      }
      if (display === "chips") {
        return <div className="fs-picker is-chips">{label}{options.map((option) => <span key={option} className={`fs-choice-chip${chosen.has(option) ? " is-chosen" : ""}`}>{chosen.has(option) && <Check size={11} aria-hidden="true" />}<Prose text={option} /></span>)}</div>;
      }
      return (
        <fieldset className="fs-picker is-checkboxes">
          {node.label !== undefined && <legend className="fs-input-label">{str(node.label)}</legend>}
          {options.map((option) => <label key={option} className="fs-check"><input type={node.multiple === false ? "radio" : "checkbox"} name={str(node.path)} defaultChecked={chosen.has(option)} /><span><Prose text={option} /></span></label>)}
        </fieldset>
      );
    }
    case "TextField": {
      const start = str(ctx.data.get(str(node.path)));
      return (
        <label className="fs-field">
          {node.label !== undefined && <span className="fs-input-label">{str(node.label)}</span>}
          {node.multiline ? <textarea className="fs-text" rows={3} placeholder={str(node.placeholder)} defaultValue={start} /> : <input className="fs-text" placeholder={str(node.placeholder)} defaultValue={start} />}
        </label>
      );
    }
    case "PromptButton": {
      let missing = false;
      const text = str(node.text).replace(/\{(\/[^}]+)\}/g, (_, at: string) => {
        const filled = spoken(ctx.data.get(at));
        if (!filled) missing = true;
        return filled || "…";
      });
      return (
        <button type="button" disabled={missing} className={`button ${node.primary ? "primary" : "secondary"} compact av-prompt`} title={`Sends exactly this text to ${str(node.agent)}`}>
          <Send size={12} aria-hidden="true" /><span>{text}</span>
        </button>
      );
    }
    default: return null;
  }
}

/**
 * Pull requests opened by other boxes' dispatchers, left out of the posted images (the
 * repository is public and they are not this deck's work). A DeckRef to one becomes a muted
 * placeholder chip and its number in the agent's words becomes "another box's PR", so a
 * lane's count still matches what the agent composed.
 */
const OFF_DECK = [2545, 2548, 2549, 2554];
const OFF_DECK_WORDS = new RegExp(`#(?:${OFF_DECK.join("|")})\\b(?: \\([^)]*\\))?`, "g");

function leftOut(component: A2uiComponent): A2uiComponent {
  if (component.component === "DeckRef" && OFF_DECK.includes(Number(component.number))) {
    return { id: component.id, component: "Chip", text: "another box's PR, left out", tone: "muted" } as A2uiComponent;
  }
  return JSON.parse(JSON.stringify(component).replace(OFF_DECK_WORDS, "another box's PR")) as A2uiComponent;
}

export function FleetSurfaceMock({ composer }: { composer: string }) {
  const messages = file<A2uiMessage[]>(composer === "dispatcher" ? "dispatcherView" : `standinView.${composer.replace(/^standin-/, "")}`) ?? [];
  const live = file<LiveDoc>("live.r3");
  const ctx = useMemo(() => {
    const byId = new Map<string, A2uiComponent>();
    const refused: string[] = [];
    const data = new Map<string, unknown>();
    for (const message of messages) {
      if ("updateComponents" in message) {
        for (const component of message.updateComponents.components) {
          // Refused before the view reaches the desktop: a type outside the catalog.
          if ((FLEET_SURFACE_CATALOG as readonly string[]).includes(component.component)) byId.set(component.id, leftOut(component));
          else refused.push(`${component.id} (${component.component})`);
        }
      } else if ("updateDataModel" in message) {
        data.set(message.updateDataModel.path, message.updateDataModel.value);
      }
    }
    return { byId, refused, data };
  }, [messages]);
  if (!live || !messages.length) return <div className="agent-view" data-testid="fleet-view"><p className="av-text">No composition from {composer} yet.</p></div>;
  const composedAt = str(ctx.data.get("/composedAt"));
  const top = live.cards.entries.find((card) => card.kind === "dispatcher");
  return (
    <div className="agent-view" data-testid="fleet-view" data-composer={composer}>
      <div className="av-strip">
        <strong>{top?.name ?? "dispatcher"}</strong>
        {top && <StatusPill status={top.status} />}
        <span className="av-strip-fresh"><Check size={13} aria-hidden="true" />Composed by {composer === "dispatcher" ? "the dispatcher" : `a stand-in agent (${composer})`} at {composedAt ? clockAt(0, composedAt) : "?"} · pull request, issue and agent states checked by the deck at {clockAt(0, live.captured_at)}</span>
        {ctx.refused.length > 0 && <span className="av-strip-refused">Refused: {ctx.refused.join(", ")}</span>}
      </div>
      <div className="av-surface"><Node ctx={{ byId: ctx.byId, live, data: ctx.data }} id="root" seen={new Set()} /></div>
    </div>
  );
}
