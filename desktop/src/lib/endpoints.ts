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
 * # What these checks claim, and what keeps the claim true
 *
 * **Each predicate below decides the same verdict its Rust newtype decides**,
 * and the pair is pinned by a shared table rather than by this sentence:
 * `endpointValidationCases.json` lists values with the verdict each field owes
 * them, `endpoints.test.ts` asserts the predicate here agrees, and
 * `endpoint_field_parity.rs` asserts `Hostname`/`SshUser`/`KeyPath`/`HostAlias`/
 * `RemoteSocketPath`'s **deserializers** agree with the same rows. One file,
 * two readers, in two languages; a drift on either side reddens a gate.
 *
 * That is stricter than the property M7 originally wrote down here, which was
 * "a subset of Rust's, never a superset". The weaker claim was chosen for a
 * good reason — accepting what Rust refuses means a save failing with a serde
 * message the panel cannot explain — but it was **never true as shipped**, in
 * both directions at once, and nothing tested it: the universal refusals held
 * ten bytes Rust allows (so the Key-file field's own placeholder
 * `~/.ssh/id_ed25519` and the bracketed IPv6 literal its error text suggests
 * were both untypeable, and `Test connection` stayed disabled), `jumpProblem`
 * accepted an `@` that `HostAlias` refuses, the host charset omitted `_` and
 * `%`, and one 255-byte cap stood in for Rust's 64, 4096 and 253. Mirroring
 * exactly is what a table can check; "a subset" is a claim about every value
 * there is, and a claim of that shape is how this drifted unwatched.
 *
 * **The one deliberate divergence is the empty string.** Rust refuses it for
 * every one of the five types, because a stored field that exists must have a
 * value. Here, empty means *unset* for the four optional fields, and the panel
 * maps it back to `undefined` before a save (`EndpointsPanel.tsx`'s
 * `value || undefined`), so an empty string never reaches a newtype. Both
 * halves of that are pinned by name in the two test files.
 */

import type { EndpointSettingsDto, RemoteEndpointDto } from "./bridge";
import { DEFAULT_SSH_PORT, LOCAL_ENDPOINT_SELECTION } from "./bridge";
import { sanitizeText } from "./displayText";

export { DEFAULT_SSH_PORT, LOCAL_ENDPOINT_SELECTION };

/** `EndpointId`'s bound — `MAX_ENDPOINT_ID_BYTES` in `settings.rs`. */
export const MAX_ENDPOINT_ID_LENGTH = 64;

/*
 * The five bounds below are `MAX_BYTES` on the five `SshArgumentRules` impls,
 * one constant each — there is deliberately no shared "fields are short"
 * number, because Rust does not have one and a single cap here disagreed with
 * three of the five.
 *
 * Rust counts **bytes** and JavaScript counts UTF-16 units, and the difference
 * is unobservable: every non-ASCII byte is refused outright, so a value either
 * is pure ASCII (where the two counts are equal) or is refused by both sides
 * whatever its length.
 */

/**
 * `Hostname`: RFC 1035's 253-byte presentation limit, which also clears every
 * IP literal form.
 */
const MAX_HOST_LENGTH = 253;

/**
 * `SshUser`: comfortably past POSIX's 32-character `LOGIN_NAME_MAX` convention,
 * which UPN-style logins routinely exceed.
 */
const MAX_USER_LENGTH = 64;

/** `KeyPath`: Linux's `PATH_MAX`. A path is not required to be short. */
const MAX_KEY_PATH_LENGTH = 4096;

/** `HostAlias`: the same presentation limit a hostname gets. */
const MAX_JUMP_LENGTH = 253;

/** `RemoteSocketPath`'s bound: what a Unix socket address can hold. */
const MAX_SOCKET_LENGTH = 104;

/**
 * Everything outside printable ASCII, as one range: `validate`'s five
 * non-metacharacter refusals — NUL, non-ASCII, the C0 controls, DEL and ASCII
 * whitespace — are exactly the complement of `0x21..=0x7e`.
 */
const NON_PRINTABLE_ASCII = /[^\x21-\x7e]/;

/**
 * `SHELL_METACHARACTERS` from `remote_tunnel.rs`, the same twelve bytes in the
 * same order.
 *
 * They are refused in every field because the value is handed to `ssh` as an
 * argument and some of them reach `/bin/sh` through the `ProxyCommand` that
 * `-J` synthesises — CVE-2023-51385's class, which a bundled app cannot rule
 * out by requiring an OpenSSH version it does not choose.
 *
 * A string rather than a regex character class on purpose: a bare backtick in a
 * regex literal reads as the start of a template literal to anything scanning
 * this file that does not parse regex syntax, and one such scanner is a
 * required gate. `desktop_palette`'s comment masker does not recover a
 * backtick-quoted run at the newline — template literals are legitimately
 * multi-line — so it read the doc comments that followed as code and reported
 * three of their issue references (`#741`, `#742`) as hard-coded colours.
 * Inside a double-quoted string the same twelve bytes parse as what they are.
 */
const SHELL_METACHARACTERS = "`$;&|<>()'\"\\";

/**
 * Bytes no ssh-argument field may contain, whatever its own charset says —
 * `validate`'s universal byte refusals, and **exactly** those.
 *
 * Nothing else belongs here. It once also refused `* ? [ ] { } ~ ! # ^`, which
 * are not shell metacharacters to Rust and are not refused universally there —
 * and because every predicate consults this *before* its own charset, the
 * extras silently overrode all five charsets. `[` and `~` are on `Hostname`'s
 * and `KeyPath`'s charsets respectively, so the two values the UI itself
 * suggests were rejected by the field that suggested them. Let each charset
 * refuse what it should.
 */
function universallyForbidden(raw: string): boolean {
  return NON_PRINTABLE_ASCII.test(raw) || [...raw].some((char) => SHELL_METACHARACTERS.includes(char));
}

/** A leading `-` would be read as an option however the value continues. */
const LEADING_DASH = /^-/;

/** What a field may be, or why it may not. `undefined` means "fine". */
export type FieldProblem = string | undefined;

/**
 * The two checks `validate` runs before it scans a byte, in its order: the
 * length bound, then the leading `-`. Separate from the charset for the reason
 * that function gives — `-` is legal *inside* a hostname and illegal at the
 * front, so collapsing the two would either reject `build-box` or accept
 * `-oProxyCommand=…`.
 *
 * `noun` is the field named the way `SshArgumentRules::FIELD` names it, so the
 * sentence reads as a whole.
 */
function lengthAndDashProblem(raw: string, max: number, noun: string): FieldProblem {
  if (raw.length > max) return `${noun} is at most ${max} characters.`;
  if (LEADING_DASH.test(raw)) return `${noun} cannot start with “-”.`;
  return undefined;
}

/**
 * `Hostname`: a DNS name, an IPv4 literal, or a **bracketed** IPv6 literal.
 *
 * Brackets are required for IPv6 because `-L local:remote` uses `:` as its
 * separator, so an unbracketed `::1` is ambiguous to ssh's own parser and not
 * only to ours. `:` and `%` are on the charset so a bracketed literal and its
 * zone id can be typed at all; {@link hostShapeProblem} is what confines them
 * to the brackets, mirroring `Hostname::check_shape`.
 */
export function hostProblem(raw: string): FieldProblem {
  if (!raw) return "A host is required.";
  const bounds = lengthAndDashProblem(raw, MAX_HOST_LENGTH, "A host");
  if (bounds) return bounds;
  if (universallyForbidden(raw) || !/^[A-Za-z0-9._\-[\]:%]+$/.test(raw)) {
    return "A host is letters, digits and “.”, “-”, “_” — or a bracketed IPv6 literal such as [2001:db8::1].";
  }
  return hostShapeProblem(raw);
}

/**
 * `Hostname::check_shape`, rule for rule.
 *
 * Note what it does *not* say: a `[` after the first byte is left alone, so
 * `a[b` is accepted here exactly as Rust accepts it. That is Rust's rule and
 * this file mirrors rather than second-guesses it — a frontend that refused
 * more than the authority is the defect this whole file was corrected for.
 */
function hostShapeProblem(raw: string): FieldProblem {
  if (raw.startsWith("[")) {
    const closed = raw.endsWith("]") && raw.length >= 3;
    const inner = raw.slice(1, -1);
    if (!closed || inner.includes("[") || inner.includes("]")) {
      return "A host opening with “[” is a bracketed IPv6 literal and must end with “]”.";
    }
    return undefined;
  }
  if (raw.includes(":") || raw.includes("%") || raw.includes("]")) {
    return "A host may only use “:” or “%” inside a bracketed IPv6 literal such as [2001:db8::1] — the port has its own field.";
  }
  return undefined;
}

/**
 * `SshUser`. `@` is accepted because a UPN-style login (`user@realm`) is
 * ordinary on Kerberos and cloud-managed hosts; `\` is not, so `DOMAIN\user` is
 * refused. `SshUser::check_shape` additionally requires both halves of an `@`
 * to be there, because `RemoteEndpoint::user_host` is `{user}@{host}` and a
 * stored `@` yields a login attempt as the literal name `@`.
 */
export function userProblem(raw: string): FieldProblem {
  if (!raw) return undefined;
  const bounds = lengthAndDashProblem(raw, MAX_USER_LENGTH, "A user");
  if (bounds) return bounds;
  if (universallyForbidden(raw) || !/^[A-Za-z0-9._\-@]+$/.test(raw)) {
    return "A user is letters, digits and “.”, “_”, “-”, “@”.";
  }
  if (raw.split("@").some((part) => !part)) {
    return "A user written with “@” is the UPN spelling user@realm — both halves must be there.";
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
  const bounds = lengthAndDashProblem(raw, MAX_KEY_PATH_LENGTH, "A key path");
  if (bounds) return bounds;
  if (universallyForbidden(raw) || !/^[A-Za-z0-9._\-/~]+$/.test(raw)) {
    return "A key path is letters, digits and “.”, “_”, “-”, “/”, “~” — with no spaces.";
  }
  if (!raw.startsWith("/") && !raw.startsWith("~/")) return "A key path starts with “/” or “~/”.";
  return undefined;
}

/**
 * `HostAlias`: a `Host` block name from the user's own `~/.ssh/config`, so the
 * jump host's address, port, user and key stay in that file. Wildcards are not
 * accepted — a jump *target* is one host — and neither is `@`: the alias is a
 * name, and the user it implies is the one that file gives it.
 */
export function jumpProblem(raw: string): FieldProblem {
  if (!raw) return undefined;
  const bounds = lengthAndDashProblem(raw, MAX_JUMP_LENGTH, "A jump host");
  if (bounds) return bounds;
  if (universallyForbidden(raw) || !/^[A-Za-z0-9._\-]+$/.test(raw)) {
    return "A jump host is the name of a Host block in your ~/.ssh/config — letters, digits and “.”, “_”, “-”.";
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
  const bounds = lengthAndDashProblem(raw, MAX_SOCKET_LENGTH, "A socket path");
  if (bounds) return bounds;
  if (universallyForbidden(raw) || !/^[A-Za-z0-9._\-/]+$/.test(raw)) {
    return "A socket path is letters, digits and “.”, “_”, “-”, “/”.";
  }
  if (!raw.startsWith("/")) return "A socket path is absolute.";
  return undefined;
}

/**
 * A port ssh can be asked for.
 *
 * No newtype on the other side: the stored field is a `u16` and the form's
 * input is `type="number"`, so this is the only place a `0` or a `70000` is
 * caught.
 */
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

/** The five text fields a deck row has, named as the shared table names them. */
export type EndpointField = "host" | "user" | "identity" | "jump" | "socket";

/**
 * What each field's input shows while it is empty.
 *
 * Here rather than inline in `EndpointsPanel.tsx` so the tests can hold them to
 * the predicates beside them. Two of the five are **specimen values** — text a
 * user could type unchanged — and both were *rejected* by the very field that
 * displayed them until this file's universal refusals were corrected, which is
 * why they are now pinned rather than merely written down. The other three tell
 * the user where the value comes from instead of showing one; each contains
 * spaces, so each is refused, and {@link SPECIMEN_PLACEHOLDER_FIELDS} is what
 * says which kind a placeholder is meant to be.
 */
export const FIELD_PLACEHOLDERS: Record<EndpointField, string> = {
  host: "build-box.example.com",
  user: "from your ssh config",
  identity: "~/.ssh/id_ed25519",
  jump: "a Host block in ~/.ssh/config",
  socket: "found by Test connection",
};

/**
 * The placeholders that are specimen values rather than prose hints.
 *
 * Each must be accepted by its own field, and each is a row of
 * `endpointValidationCases.json` so the Rust newtype is pinned to accept it
 * too. Listing a prose hint here, or leaving a new specimen out, fails a test
 * rather than costing the user a field they cannot fill in.
 */
export const SPECIMEN_PLACEHOLDER_FIELDS: readonly EndpointField[] = ["host", "identity"];

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
 * **Rendered text says Deck, never "daemon".** M15 swept the rest of the app
 * into the same vocabulary, so this is no longer the only surface written in it:
 * rendered text says Deck everywhere, while code, protocol, CLI, docs, CSS class
 * names and `data-testid`s deliberately keep `daemon`.
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
