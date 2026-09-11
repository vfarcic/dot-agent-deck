/**
 * The vocabulary of a configured deck, on the webview's side (PRD #741 M7).
 *
 * Small on purpose. The *authority* for what a stored endpoint may be is Rust:
 * every field is a validating newtype whose `Deserialize` runs the same check
 * its constructor does, so a hand-edited `desktop.toml` and a settings form
 * reach exactly the same gate. What lives here is what a form needs before it
 * has anything to save — the reserved selection token, a label derived the way
 * `RemoteEndpoint::describe()` derives it, and a freshly minted id — plus the
 * charset checks that let the panel say "that is not a hostname" while the user
 * is typing instead of after a round trip.
 *
 * **These checks are deliberately a subset of Rust's, never a superset.** If
 * this file accepted something Rust refuses, a save would fail with a serde
 * message the panel cannot explain; if it refuses something Rust accepts, a
 * legitimate value becomes untypeable and there is no second route in. So each
 * predicate below mirrors one `SshArgumentRules` impl and nothing more, and the
 * comment on each names the impl it mirrors.
 */

import type { EndpointSettingsDto, RemoteEndpointDto } from "./bridge";
import { DEFAULT_SSH_PORT, LOCAL_ENDPOINT_SELECTION } from "./bridge";
import { sanitizeText } from "./displayText";

export { DEFAULT_SSH_PORT, LOCAL_ENDPOINT_SELECTION };

/** `EndpointId`'s bound — `MAX_ENDPOINT_ID_BYTES` in `settings.rs`. */
export const MAX_ENDPOINT_ID_LENGTH = 64;

/**
 * `Hostname`'s bound: RFC 1035's 253-byte presentation limit, which also clears
 * every IP literal form.
 */
const MAX_HOST_LENGTH = 253;

/** `RemoteSocketPath`'s bound: what a Unix socket address can hold. */
const MAX_SOCKET_LENGTH = 104;

/** `SshUser`, `KeyPath` and `HostAlias` are all comfortably inside this. */
const MAX_FIELD_LENGTH = 255;

/**
 * Bytes no ssh-argument field may contain, whatever its own charset says —
 * `validate`'s universal refusals in `remote_tunnel.rs`. Non-ASCII, controls,
 * whitespace and the shell metacharacters, because every one of these values is
 * handed to `ssh` as an argument and some of them reach `/bin/sh` through the
 * `ProxyCommand` that `-J` synthesises.
 */
const UNIVERSALLY_FORBIDDEN = /[^\x21-\x7e]|["'`\\$&|;<>()*?[\]{}~!#^]/;

/** A leading `-` would be read as an option however the value continues. */
const LEADING_DASH = /^-/;

/** What a field may be, or why it may not. `undefined` means "fine". */
export type FieldProblem = string | undefined;

/**
 * `Hostname`: a DNS name, an IPv4 literal, or a **bracketed** IPv6 literal.
 *
 * Brackets are required for IPv6 because `-L local:remote` uses `:` as its
 * separator, so an unbracketed `::1` is ambiguous to ssh's own parser and not
 * only to ours.
 */
export function hostProblem(raw: string): FieldProblem {
  if (!raw) return "A host is required.";
  if (raw.length > MAX_HOST_LENGTH) return `A host is at most ${MAX_HOST_LENGTH} characters.`;
  if (LEADING_DASH.test(raw)) return "A host cannot start with “-”.";
  if (UNIVERSALLY_FORBIDDEN.test(raw) || !/^[A-Za-z0-9.\-[\]:]+$/.test(raw)) {
    return "A host is letters, digits, “.” and “-” — or a bracketed IPv6 literal such as [2001:db8::1].";
  }
  return undefined;
}

/**
 * `SshUser`. `@` is accepted because a UPN-style login (`user@realm`) is
 * ordinary on Kerberos and cloud-managed hosts; `\` is not, so `DOMAIN\user` is
 * refused.
 */
export function userProblem(raw: string): FieldProblem {
  if (!raw) return undefined;
  if (raw.length > MAX_FIELD_LENGTH) return `A user is at most ${MAX_FIELD_LENGTH} characters.`;
  if (LEADING_DASH.test(raw)) return "A user cannot start with “-”.";
  if (UNIVERSALLY_FORBIDDEN.test(raw) || !/^[A-Za-z0-9._\-@]+$/.test(raw)) {
    return "A user is letters, digits and “.”, “_”, “-”, “@”.";
  }
  return undefined;
}

/**
 * `KeyPath`: a **path**, never key material and never a passphrase. `~` is
 * accepted because OpenSSH tilde-expands `-i` itself. Whitespace is refused, so
 * a key under a directory with a space in its name cannot be named here — a
 * real limitation and the deliberate side of the trade, since a space is the
 * first thing that goes wrong when a value is interpolated into a
 * `ProxyCommand`.
 */
export function identityProblem(raw: string): FieldProblem {
  if (!raw) return undefined;
  if (raw.length > MAX_FIELD_LENGTH) return `A key path is at most ${MAX_FIELD_LENGTH} characters.`;
  if (UNIVERSALLY_FORBIDDEN.test(raw) || !/^[A-Za-z0-9._\-/~]+$/.test(raw)) {
    return "A key path is letters, digits and “.”, “_”, “-”, “/”, “~” — with no spaces.";
  }
  if (!raw.startsWith("/") && !raw.startsWith("~/")) return "A key path starts with “/” or “~/”.";
  return undefined;
}

/**
 * `HostAlias`: a `Host` block name from the user's own `~/.ssh/config`, so the
 * jump host's address, port, user and key stay in that file. Wildcards are not
 * accepted — a jump *target* is one host.
 */
export function jumpProblem(raw: string): FieldProblem {
  if (!raw) return undefined;
  if (raw.length > MAX_FIELD_LENGTH) return `A jump host is at most ${MAX_FIELD_LENGTH} characters.`;
  if (LEADING_DASH.test(raw)) return "A jump host cannot start with “-”.";
  if (UNIVERSALLY_FORBIDDEN.test(raw) || !/^[A-Za-z0-9._\-@]+$/.test(raw)) {
    return "A jump host is the name of a Host block in your ~/.ssh/config.";
  }
  return undefined;
}

/**
 * `RemoteSocketPath`. `:` is off the charset and that is load-bearing rather
 * than tidy: `-L` is parsed by splitting on `:`, so a colon here would silently
 * re-interpret the forward specification.
 */
export function socketProblem(raw: string): FieldProblem {
  if (!raw) return undefined;
  if (raw.length > MAX_SOCKET_LENGTH) return `A socket path is at most ${MAX_SOCKET_LENGTH} characters.`;
  if (UNIVERSALLY_FORBIDDEN.test(raw) || !/^[A-Za-z0-9._\-/]+$/.test(raw)) {
    return "A socket path is letters, digits and “.”, “_”, “-”, “/”.";
  }
  if (!raw.startsWith("/")) return "A socket path is absolute.";
  return undefined;
}

/** A port ssh can be asked for. */
export function portProblem(raw: number): FieldProblem {
  if (!Number.isInteger(raw) || raw < 1 || raw > 65535) return "A port is a whole number from 1 to 65535.";
  return undefined;
}

/** Every problem a row has, in the order the form shows the fields. */
export function rowProblems(row: RemoteEndpointDto): FieldProblem[] {
  return [
    hostProblem(row.host),
    userProblem(row.user ?? ""),
    portProblem(row.port),
    identityProblem(row.identity ?? ""),
    jumpProblem(row.jump ?? ""),
    socketProblem(row.socket ?? ""),
  ].filter((problem): problem is string => Boolean(problem));
}

/**
 * A fresh endpoint id: sixteen lowercase hex characters.
 *
 * The other copy of this is `EndpointId::mint`, and the two are kept honest not
 * by sharing code but by `EndpointId::parse` — which every save and every
 * hand-edited document goes through — being the only thing that decides what an
 * id may be. Sixteen hex characters cannot collide with the reserved `local`
 * token or with any word a future `Selection` variant would reserve: those are
 * shorter and contain letters that are not hex digits.
 *
 * `crypto.getRandomValues` rather than `Math.random` because the Rust side
 * seeds from the operating system and the two should not differ in kind.
 * Uniqueness is what is wanted — nothing authenticates with this — but the call
 * that gives one already gives the other.
 */
export function mintEndpointId(): string {
  const bytes = new Uint8Array(8);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

/**
 * How a deck is named on screen, derived from its address exactly as
 * `RemoteEndpoint::describe()` derives it: `user@host`, with `:port` appended
 * when the port is not 22.
 *
 * Sanitised on the way out even though every stored byte came through a charset
 * that excludes the bidi codepoints. The seam is here because this string is
 * what the connection footer and the deck list render, and a label that strips
 * only control characters is a label a reordering character walks through — the
 * same reason `dto::safe_display_text` exists on the other side.
 */
export function describeEndpoint(row: RemoteEndpointDto): string {
  const userHost = row.user ? `${row.user}@${row.host}` : row.host;
  return sanitizeText(row.port === DEFAULT_SSH_PORT ? userHost : `${userHost}:${row.port}`);
}

/** A blank row, ready for the user to fill in. */
export function blankEndpoint(): RemoteEndpointDto {
  return { host: "", id: mintEndpointId(), port: DEFAULT_SSH_PORT };
}

/**
 * Which deck the app is talking to, on the webview's side (PRD #741 M9).
 *
 * # Why this is a union and not a string
 *
 * The *stored* form is one token — `local`, or a row's id — and `Selection` in
 * `settings.rs` is the authority on it. What travels through the screens is this
 * union, for exactly the reason M6 made the Rust side an enum rather than an
 * `Option<EndpointId>`: [#742](https://github.com/vfarcic/dot-agent-deck/issues/742)
 * adds **All Decks** to this selector, and that has to be *additive*. With a
 * union it is one variant plus one arm in each of three places —
 * {@link parseSelection}, {@link selectionToken} and {@link deckChoices}. With a
 * bare id threaded through the components it would be a change to every prop
 * that carries a selection, and a `""`-means-all convention in each of them.
 *
 * So: no component below this file handles a raw endpoint id, and none of them
 * needs to know that `local` is a reserved word.
 */
export type DeckSelection =
  | { kind: "local" }
  | { kind: "one"; id: string };

/** The default, and what an unrecognised token degrades to. */
export const LOCAL_DECK_SELECTION: DeckSelection = { kind: "local" };

/**
 * Read a stored token.
 *
 * An unknown token — including one a *newer* build wrote, such as #742's `all`
 * — parses as `One`, finds no row, and is therefore rendered as the local deck
 * with the fallback the Rust side already reports. That is the same degradation
 * `Selection`'s own deserializer performs, and it is why nothing here throws:
 * the document is hand-editable and may have been written by a build with more
 * variants than this one.
 */
export function parseSelection(token: string): DeckSelection {
  return token === LOCAL_ENDPOINT_SELECTION ? LOCAL_DECK_SELECTION : { kind: "one", id: token };
}

/** The token to store. The inverse of {@link parseSelection}. */
export function selectionToken(selection: DeckSelection): string {
  return selection.kind === "local" ? LOCAL_ENDPOINT_SELECTION : selection.id;
}

/** Whether two selections name the same deck. */
export function sameSelection(left: DeckSelection, right: DeckSelection): boolean {
  return selectionToken(left) === selectionToken(right);
}

/** One entry in the Deck selector: what it is, and how it is named on screen. */
export interface DeckChoice {
  /** Stable per entry, and what the selector keys and test ids use. */
  token: string;
  selection: DeckSelection;
  label: string;
}

/**
 * Every deck the user can choose, local first.
 *
 * The local deck leads and is always present because it needs no configuration
 * — `Endpoint::local()` resolves it from the platform paths — so this list is
 * never empty and the selector is useful before anything is stored.
 *
 * **Rendered text says Deck, never "daemon".** This is new surface and it is
 * written in the vocabulary the app is moving to; the sweep of the older strings
 * is M15's.
 */
export function deckChoices(section: EndpointSettingsDto | undefined): DeckChoice[] {
  const choices: DeckChoice[] = [
    { token: LOCAL_ENDPOINT_SELECTION, selection: LOCAL_DECK_SELECTION, label: "This machine" },
  ];
  for (const row of section?.remote ?? []) {
    choices.push({
      token: row.id,
      selection: { kind: "one", id: row.id },
      label: describeEndpoint(row) || "New deck",
    });
  }
  return choices;
}

/**
 * How the CHOSEN deck is named when it is not one of the listed ones.
 *
 * Reachable in one ordinary way: the stored selection names a row this build
 * cannot see — removed by hand, or written by a newer build. The app is then on
 * the local deck and says so through `connection.selectionFallback`; this is
 * only the label on the trigger while that sentence is on screen.
 */
export const UNKNOWN_DECK_LABEL = "Unknown deck";
