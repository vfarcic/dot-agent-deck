import {
  Activity, Archive, ArrowRight, Bell, BookOpen, Bot, Bug, Check, CheckCheck, CircleDot, CircleHelp, ClipboardCheck, CloudUpload, Code, Database,
  Download, Eye, FileText, Flag, FlaskConical, Gauge, GitBranch, GitCommitHorizontal, GitMerge, GitPullRequest, Globe, Hammer, Hourglass, Info,
  Lightbulb, Link, ListChecks, Lock, MessageSquare, MessageSquareText, Package, Paintbrush, Pause, Pencil, Play, RefreshCw, Rocket, RotateCcw, Save,
  Scissors, Search, Send, Server, Settings, ShieldCheck, Ship, Sparkles, Square, Star, Terminal, TestTube, ThumbsDown, ThumbsUp, Timer, Trash2,
  Undo2, Upload, Wrench, Zap, type LucideIcon,
} from "lucide-react";

import { exactTextSegments } from "./agentCards";
import { DISPLAY_LIMITS, displayText, sanitizeText } from "./displayText";

/**
 * PRD #2374 decision 2026-11-24 — the icons a card action can draw, by the
 * name a catalog entry's `icon` uses (`icon = "rocket"`). A curated set of the
 * icon library the app already bundles: the card's and the batch's icon
 * buttons draw these, and the card actions editor's picker offers exactly
 * these. A name outside it — no icon, or one only a newer app knows — draws
 * {@link GENERIC_CARD_ICON}.
 */
const CARD_ICONS: Readonly<Record<string, LucideIcon>> = {
  play: Play,
  "git-merge": GitMerge,
  rocket: Rocket,
  check: Check,
  "check-check": CheckCheck,
  "git-pull-request": GitPullRequest,
  "git-branch": GitBranch,
  "git-commit": GitCommitHorizontal,
  bug: Bug,
  wrench: Wrench,
  hammer: Hammer,
  "test-tube": TestTube,
  "flask-conical": FlaskConical,
  "file-text": FileText,
  "book-open": BookOpen,
  search: Search,
  eye: Eye,
  "message-square": MessageSquare,
  send: Send,
  "thumbs-up": ThumbsUp,
  "thumbs-down": ThumbsDown,
  pause: Pause,
  square: Square,
  "refresh-cw": RefreshCw,
  "rotate-ccw": RotateCcw,
  "undo-2": Undo2,
  "trash-2": Trash2,
  upload: Upload,
  download: Download,
  package: Package,
  ship: Ship,
  "cloud-upload": CloudUpload,
  "shield-check": ShieldCheck,
  lock: Lock,
  sparkles: Sparkles,
  lightbulb: Lightbulb,
  "list-checks": ListChecks,
  "clipboard-check": ClipboardCheck,
  terminal: Terminal,
  code: Code,
  scissors: Scissors,
  activity: Activity,
  "arrow-right": ArrowRight,
  "circle-dot": CircleDot,
  link: Link,
  zap: Zap,
  flag: Flag,
  star: Star,
  bell: Bell,
  "circle-help": CircleHelp,
  info: Info,
  save: Save,
  pencil: Pencil,
  gauge: Gauge,
  timer: Timer,
  hourglass: Hourglass,
  settings: Settings,
  paintbrush: Paintbrush,
  database: Database,
  server: Server,
  globe: Globe,
  archive: Archive,
  bot: Bot,
};

/** The name a card action's icon button reports (`data-icon`) when it draws the generic icon. */
export const GENERIC_CARD_ICON = "generic";

/** Every icon a card action can choose, in the picker's order. */
export const CARD_ICON_NAMES: readonly string[] = Object.keys(CARD_ICONS);

/** The icon `name` draws: itself when the app bundles it, else {@link GENERIC_CARD_ICON}. */
export function cardIconName(name: string | null | undefined): string {
  return name && Object.hasOwn(CARD_ICONS, name) ? name : GENERIC_CARD_ICON;
}

/** One card action's icon, decorative: the button that holds it carries the name. */
export function CardActionIcon({ name, size = 14 }: { name: string | null | undefined; size?: number }) {
  const shown = cardIconName(name);
  const Drawn = shown === GENERIC_CARD_ICON ? MessageSquareText : CARD_ICONS[shown];
  return <Drawn size={size} aria-hidden="true" focusable="false" />;
}

/** The icons whose name contains every word of `query`, in the picker's order. */
export function searchCardIcons(query: string): string[] {
  const words = query.toLowerCase().split(/[\s-]+/).filter(Boolean);
  return CARD_ICON_NAMES.filter((name) => words.every((word) => name.includes(word)));
}

/** The longest prompt a hover quotes before it says the rest is in the card's overlay. */
const HOVER_PROMPT_CHARS = 1200;

/**
 * A card action icon's hover: its label, then the exact text it sends with
 * its line breaks kept and every invisible or reordering character shown as a
 * marker (as the overlay shows it), then `notes` — that it asks first, why it
 * is disabled. A text longer than the hover holds ends by saying so.
 */
export function cardActionHover(label: string, prompt: string | null | undefined, notes: readonly (string | false | null | undefined)[] = []): string {
  const lines = [displayText(label, DISPLAY_LIMITS.name)];
  if (prompt) {
    const shown = exactTextSegments(prompt).map((segment) => segment.shown).join("");
    const chars = Array.from(shown);
    const cut = chars.length > HOVER_PROMPT_CHARS;
    const quoted = (cut ? chars.slice(0, HOVER_PROMPT_CHARS).join("") : shown).split("\n").map((line) => sanitizeText(line)).join("\n");
    lines.push(`Sends: ${quoted}${cut ? "… (open the card to read all of it)" : ""}`);
  }
  for (const note of notes) if (note) lines.push(displayText(note, DISPLAY_LIMITS.message));
  return lines.join("\n");
}
