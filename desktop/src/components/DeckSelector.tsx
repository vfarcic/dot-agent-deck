/**
 * The Deck selector (PRD #741 M9) — which deck the agent screens are showing.
 *
 * # What it is for
 *
 * Until M9 the deck was chosen in Settings and every other screen simply obeyed.
 * This is the control that makes one remote deck *usable*: it sits on the two
 * screens that show agents — the deck and the overview — so the fleet, the
 * instruments beside it and the terminals below it are all scoped by something
 * the user can see and change without leaving them.
 *
 * It is also the seam [#742](https://github.com/vfarcic/dot-agent-deck/issues/742)
 * extends rather than replaces. That issue adds **All Decks** to this list; the
 * cost of doing so is one variant of {@link DeckSelection} and one arm in each
 * of `parseSelection`, `selectionToken` and `deckChoices`. Nothing in this file
 * handles a raw endpoint id, so nothing here has to change for it.
 *
 * # One control, both shells
 *
 * It goes in the top bar's `.repo-context` on each, so it reads as the same
 * control in both and sits with the thing it scopes rather than in a corner. The
 * shells pass their own settings state; there is no second source.
 *
 * # Switching goes through the path that already exists
 *
 * Choosing a deck writes the document, and nothing else. `desktop_set_settings`
 * → `apply_selection` is what puts a selection into force, and it is one
 * function precisely so that dropping the links, releasing the tunnels, telling
 * the watcher and emitting a fresh snapshot cannot be done by halves. A second
 * route from here — a bespoke "switch deck" command — would be a second half.
 *
 * # An unreachable deck shows its state; it never blanks the screen
 *
 * The state line is keyed on `selectionFallback` **before** `status`, and that
 * order is the bug M7's first pass had. A `NoRemoteSocket` fallback leaves the
 * app **connected** — to the local deck, under the chosen deck's name — so
 * anything reading `status` alone reports "connected" and says nothing about the
 * substitution. Acting on the wrong machine's agents is the outcome that makes
 * it worth a line of its own.
 *
 * # Markup
 *
 * A `<span id>` plus `aria-labelledby`, never a `<fieldset>`/`<legend>`. WebKit
 * forces a rendered legend's `float` to `none`
 * ([#1032](https://github.com/vfarcic/dot-agent-deck/issues/1032), opened by
 * this PRD's own M7 finding), and the app ships on WebKit — WebKitGTK under
 * Tauri on Linux, WKWebView on macOS — so the fieldset form is the one that does
 * not hold where it matters. This is new surface, so it is written in the form
 * that works in both engines from the start.
 *
 * # Vocabulary
 *
 * Rendered text says **Deck**, never "daemon". The sweep of the existing strings
 * is M15's; nothing new is written in the old vocabulary.
 */
import { useEffect, useId, useRef, useState } from "react";
import { AlertTriangle, Check, ChevronsUpDown, Server } from "lucide-react";
import { LOCAL_ENDPOINT_SELECTION } from "../lib/bridge";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import {
  type DeckSelection,
  deckChoices,
  parseSelection,
  sameSelection,
  selectionToken,
  UNKNOWN_DECK_LABEL,
} from "../lib/endpoints";
import type { DesktopSettingsState } from "../hooks/useDesktopSettings";
import type { ConnectionView } from "../types";

/**
 * What this deck's connection has to say, or `undefined` when it has nothing.
 *
 * Exported because it is the part worth pinning: the ORDER of these branches is
 * the behaviour, not the wording.
 */
export function deckStateNote(connection: ConnectionView): string | undefined {
  // First, and deliberately. `SelectionFallback::NoRemoteSocket` leaves the app
  // CONNECTED — to the local deck, while the selector names another one — so a
  // check on `status` alone would find nothing wrong and say nothing.
  if (connection.selectionFallback) return connection.selectionFallback;
  if (connection.status === "loading") return "Connecting to this deck…";
  if (connection.status !== "connected") return connection.message;
  // Connected, with a caveat the app is required to keep on screen for the whole
  // session (issue #801, PRD #741 M8). For a remote deck the build stamp is an
  // informational badge and never a refusal, which is exactly the case where
  // `status` is "connected" and there is still something to say.
  if (connection.buildStampMismatchOnly) return connection.message;
  return undefined;
}

/** Whether the state note is a problem or merely a disclosure. */
function noteIsProblem(connection: ConnectionView): boolean {
  return Boolean(connection.selectionFallback) || connection.status === "disconnected" || connection.status === "error";
}

export function DeckSelector({ settings, connection }: { settings: DesktopSettingsState; connection: ConnectionView }) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  // `useId` rather than a constant: the label is referenced by
  // `aria-labelledby`, and two selectors in one document (a test rendering both
  // shells) must not share an id.
  const labelId = useId();
  const section = settings.settings.endpoints;
  const choices = deckChoices(section);
  const selection = parseSelection(section?.selection ?? LOCAL_ENDPOINT_SELECTION);
  const current = choices.find((choice) => sameSelection(choice.selection, selection));
  const note = deckStateNote(connection);

  /*
    Dismiss on a pointer-down anywhere else, exactly as `OverviewColumnPicker`
    does and for the same three reasons: `pointerdown` rather than `click`, so
    the menu closes at the moment the intent is expressed instead of after focus
    has already moved; anything inside this root ignored, INCLUDING the trigger,
    so a second press closes rather than closing-then-reopening; and the listener
    bound only while the menu is open, with `open` in the dependency list so
    React runs the cleanup on the same transition that hides it.
  */
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: Event) => {
      if (event.target instanceof Node && root.current?.contains(event.target)) return;
      setOpen(false);
    };
    document.addEventListener("pointerdown", dismiss);
    return () => document.removeEventListener("pointerdown", dismiss);
  }, [open]);

  const choose = (next: DeckSelection) => {
    setOpen(false);
    // A no-op guard, and it carries a second job. When the document has NO
    // `[endpoints]` section the only choice is the local deck and it is already
    // selected, so every click lands here — which is what stops this component
    // from writing back the `{ remote: [], selection: "local" }` that
    // `normalizeEndpointSettings` exists to avoid fabricating. The section below
    // is therefore only ever built from one that already exists.
    if (sameSelection(next, selection)) return;
    settings.save({
      ...settings.settings,
      endpoints: { remote: section?.remote ?? [], selection: selectionToken(next) },
    });
  };

  return (
    <div
      className="deck-selector"
      ref={root}
      onKeyDown={(event) => {
        if (event.key !== "Escape") return;
        setOpen(false);
        event.stopPropagation();
      }}
    >
      <button
        className="deck-selector-trigger"
        data-testid="deck-selector-toggle"
        aria-expanded={open}
        aria-haspopup="true"
        title="Choose which deck these screens are showing."
        onClick={() => setOpen((wasOpen) => !wasOpen)}
      >
        <span className={`connection-lamp connection-${connection.status}`} aria-hidden="true" />
        <Server size={13} aria-hidden="true" />
        <strong data-testid="deck-selector-current">{displayText(current?.label ?? UNKNOWN_DECK_LABEL, DISPLAY_LIMITS.name)}</strong>
        <ChevronsUpDown size={12} aria-hidden="true" />
      </button>
      {/*
        Rendered whether or not the menu is open, and whether or not the app is
        connected. This is the "shows its state without blanking" half: the deck
        keeps its name, the screen keeps its layout, and the reason sits under
        it.
      */}
      {note && (
        <small
          className={noteIsProblem(connection) ? "deck-selector-state is-problem" : "deck-selector-state"}
          data-testid="deck-selector-state"
          title={displayText(note, DISPLAY_LIMITS.message)}
        >
          <AlertTriangle size={11} aria-hidden="true" />
          <span>{displayText(note, DISPLAY_LIMITS.message)}</span>
        </small>
      )}
      {open && (
        <div className="deck-selector-menu" data-testid="deck-selector-menu">
          <span className="deck-selector-menu-label" id={labelId}>Deck</span>
          <div role="radiogroup" aria-labelledby={labelId}>
            {choices.map((choice) => {
              const chosen = sameSelection(choice.selection, selection);
              return (
                <button
                  key={choice.token}
                  role="radio"
                  aria-checked={chosen}
                  className={chosen ? "deck-selector-option is-selected" : "deck-selector-option"}
                  data-testid={`deck-selector-option-${choice.token}`}
                  onClick={() => choose(choice.selection)}
                >
                  <span>{displayText(choice.label, DISPLAY_LIMITS.name)}</span>
                  {chosen && <Check size={12} aria-hidden="true" />}
                </button>
              );
            })}
          </div>
          <p>Decks are added and removed in Settings.</p>
        </div>
      )}
    </div>
  );
}
