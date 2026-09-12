/**
 * The Decks section (PRD #741 M7) — add a remote deck, choose which one the app
 * talks to, remove one, and test the connection (M10).
 *
 * It implements `SettingsPanelProps` and nothing else. The sheet does not know
 * this is about ssh, and this file does not know how the document is stored.
 *
 * # The local deck is a row without being stored
 *
 * `Endpoint::local()` resolves it from the platform paths the way every caller
 * did before endpoints existed, so a fresh install has **no** `[endpoints]`
 * section and still works, and deleting the section gets the local deck back
 * rather than nothing. It is therefore rendered first, always present, never
 * removable — and it is why this panel is useful before anything is configured.
 *
 * # The round trip, which is a silent data-loss bug if it is got wrong
 *
 * Every save sends the whole document with `endpoints` in it. That is necessary
 * and not sufficient: `normalizeDesktopSettings` builds a fresh object with a
 * fixed key set, so a section it does not read is gone *before* this panel
 * spreads anything, and `desktop_set_settings` then merges the decoded struct
 * over the file. The half that makes it work is in `bridge.ts` —
 * `normalizeEndpointSettings` preserves absence and rebuilds a present section
 * row by row — and `bridge.test.ts` pins it. What this panel owes is the other
 * direction: it spreads `settings` and replaces only `endpoints`, so a section
 * some future build adds survives a save made from here.
 *
 * # There is no display name
 *
 * M6 decided that deliberately: a user-chosen label is exactly the arbitrary
 * `String` the settings field-type guard refuses, and it would need its own bidi
 * and control-character handling before anything rendered it. `describeEndpoint`
 * derives the label from the address, and every byte of that came through a
 * validated ASCII charset. `sanitizeText` is still applied at this seam, because
 * the line that names the deck is the one place a reordering character has a
 * direct consequence — not because the value is expected to carry one.
 *
 * # Layout
 *
 * The house convention is one setting, one row, two columns — a 132px label
 * column, a 16px gutter, the control (`docs/develop/desktop-gui.md`). The deck
 * *chooser* is exactly that, and uses `.settings-row` unchanged. A deck's own
 * fields are not one setting, so they live in a disclosure under the chosen row
 * — six two-column rows of the same shape, so a reader scanning down sees one
 * convention rather than a form pasted into a settings sheet.
 *
 * Every sentence here is one the reader acts on: what went wrong, what to run,
 * what their ssh config is doing on their behalf. Explanation of *why* the app
 * is built this way is in `docs/` and in the PRD, per that document's text rule.
 */
import { useState } from "react";
import { AlertTriangle, Check, Plug, Plus, Trash2 } from "lucide-react";
import type {
  DesktopSettingsDto,
  EndpointSettingsDto,
  EndpointTestReportDto,
  RemoteEndpointDto,
} from "../lib/bridge";
import { LOCAL_ENDPOINT_SELECTION } from "../lib/bridge";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import {
  blankEndpoint,
  describeEndpoint,
  FIELD_PLACEHOLDERS,
  hostProblem,
  identityProblem,
  jumpProblem,
  portProblem,
  rowProblems,
  socketProblem,
  userProblem,
} from "../lib/endpoints";
import { useSettingsBridge } from "../lib/settingsBridge";
import type { SettingsPanelProps } from "../lib/settingsContract";

/** How the panel reads the document's endpoint section without fabricating one. */
function sectionOf(settings: DesktopSettingsDto): EndpointSettingsDto {
  return settings.endpoints ?? { remote: [], selection: LOCAL_ENDPOINT_SELECTION };
}

export function EndpointsPanel({ settings, onSave, saveError, mode }: SettingsPanelProps) {
  // `undefined` wherever no provider is mounted — a panel rendered standalone,
  // or a surface with no bridge. The Test connection button is then absent
  // rather than present and broken; see `lib/settingsBridge.tsx`.
  const onTest = useSettingsBridge()?.testEndpoint;
  const section = sectionOf(settings);
  const selection = section.selection;
  const selected = section.remote.find((row) => row.id === selection);
  const [reports, setReports] = useState<Record<string, EndpointTestReportDto>>({});
  const [testing, setTesting] = useState<string>();

  /**
   * Write a new endpoint section back. The whole document goes, with `settings`
   * spread — see the round-trip note at the top of this file.
   */
  const saveSection = (next: EndpointSettingsDto) => onSave({ ...settings, endpoints: next });

  const addDeck = () => {
    const row = blankEndpoint();
    // Selected on creation: an added deck the user then has to select is two
    // steps for one intent, and the fields below are the chosen deck's.
    saveSection({ remote: [...section.remote, row], selection: row.id });
  };

  const removeDeck = (id: string) => {
    const remote = section.remote.filter((row) => row.id !== id);
    // Removing the deck in use falls back to local rather than to another
    // remote one: the app always has a deck, and choosing a different remote
    // one on the user's behalf is a decision they did not make.
    saveSection({ remote, selection: selection === id ? LOCAL_ENDPOINT_SELECTION : selection });
    setReports((current) => {
      const next = { ...current };
      delete next[id];
      return next;
    });
  };

  const editDeck = (id: string, change: Partial<RemoteEndpointDto>) => {
    saveSection({
      ...section,
      remote: section.remote.map((row) => (row.id === id ? { ...row, ...change } : row)),
    });
  };

  const runTest = async (id: string) => {
    if (!onTest) return;
    setTesting(id);
    try {
      const report = await onTest(settings, id);
      setReports((current) => ({ ...current, [id]: report }));
      // The discovered socket path is written back HERE rather than in Rust,
      // so the document has one writer: `useDesktopSettings` already serialises
      // this side's read-modify-write. Only when the row still lacks one — a
      // probe never overwrites a path the user typed.
      if (report.discoveredSocket && id !== LOCAL_ENDPOINT_SELECTION) {
        const row = section.remote.find((candidate) => candidate.id === id);
        if (row && !row.socket) editDeck(id, { socket: report.discoveredSocket });
      }
    } catch (cause) {
      setReports((current) => ({
        ...current,
        [id]: {
          endpointId: id,
          deck: "",
          state: "transport_failed",
          ok: false,
          message: cause instanceof Error ? cause.message : String(cause),
          disclosureKnown: false,
          forwards: [],
          knownHosts: [],
          clientProtocolVersion: 0,
          clientBuildVersion: "",
        },
      }));
    } finally {
      setTesting(undefined);
    }
  };

  return (
    <div className="settings-body" data-testid="endpoints-body">
      {/*
        The chooser is one setting and one row, so it is `.settings-row`
        unchanged — the same 132px label column every other panel uses.

        **A `role="radiogroup"` rather than a `<fieldset>`/`<legend>`, and that
        is a WebKit finding rather than a preference.** The house convention
        makes the legend a grid item by floating it, which disqualifies it from
        being the fieldset's *rendered* legend. Chromium honours that; WebKit
        forces the rendered legend's `float` to `none` — measured on this tier,
        `getComputedStyle(legend).float` is `"left"` in Chromium and `"none"` in
        WebKit — so the legend never becomes a grid item there, the control
        lands in column ONE beside the label, and the 132px column is wasted.
        The app ships on WebKit (WebKitGTK under Tauri on Linux, WKWebView on
        macOS), so the fieldset form is the one that does not hold where it
        matters. `aria-labelledby` on a radiogroup gives the same accessible
        group name in both engines and lays out the same in both. The Appearance
        and Zoom rows still use the fieldset form and still diverge; that is
        recorded rather than changed here, because their markup belongs to PRDs
        #743 and #744.
      */}
      <div className="settings-row">
        <span className="settings-row-label" id="deck-chooser-label">Deck</span>
        <div className="deck-choices" role="radiogroup" aria-labelledby="deck-chooser-label" data-testid="deck-choices">
          <DeckChoice
            id={LOCAL_ENDPOINT_SELECTION}
            label="This machine"
            selected={selection === LOCAL_ENDPOINT_SELECTION}
            onSelect={() => saveSection({ ...section, selection: LOCAL_ENDPOINT_SELECTION })}
          />
          {section.remote.map((row) => (
            <DeckChoice
              key={row.id}
              id={row.id}
              label={displayText(describeEndpoint(row) || "New deck", DISPLAY_LIMITS.name)}
              selected={selection === row.id}
              onSelect={() => saveSection({ ...section, selection: row.id })}
              onRemove={() => removeDeck(row.id)}
            />
          ))}
          <button className="add-deck" data-testid="add-deck" onClick={addDeck}>
            <Plus size={13} /> Add a deck
          </button>
        </div>
      </div>

      {selected && (
        <div className="deck-detail" data-testid="deck-detail">
          <TextRow
            id={`deck-host-${selected.id}`}
            label="Host"
            value={selected.host}
            placeholder={FIELD_PLACEHOLDERS.host}
            problem={hostProblem(selected.host)}
            onChange={(host) => editDeck(selected.id, { host })}
          />
          <TextRow
            id={`deck-user-${selected.id}`}
            label="User"
            value={selected.user ?? ""}
            placeholder={FIELD_PLACEHOLDERS.user}
            problem={userProblem(selected.user ?? "")}
            onChange={(user) => editDeck(selected.id, { user: user || undefined })}
          />
          <div className="settings-row">
            <label htmlFor={`deck-port-${selected.id}`}>Port</label>
            <input
              id={`deck-port-${selected.id}`}
              className="settings-input is-narrow"
              type="number"
              min={1}
              max={65535}
              value={selected.port}
              onChange={(event) => editDeck(selected.id, { port: Number(event.target.value) })}
            />
          </div>
          <Problem text={portProblem(selected.port)} />
          <TextRow
            id={`deck-identity-${selected.id}`}
            label="Key file"
            value={selected.identity ?? ""}
            placeholder={FIELD_PLACEHOLDERS.identity}
            problem={identityProblem(selected.identity ?? "")}
            onChange={(identity) => editDeck(selected.id, { identity: identity || undefined })}
          />
          <TextRow
            id={`deck-jump-${selected.id}`}
            label="Jump host"
            value={selected.jump ?? ""}
            placeholder={FIELD_PLACEHOLDERS.jump}
            problem={jumpProblem(selected.jump ?? "")}
            onChange={(jump) => editDeck(selected.id, { jump: jump || undefined })}
          />
          <TextRow
            id={`deck-socket-${selected.id}`}
            label="Deck socket"
            value={selected.socket ?? ""}
            placeholder={FIELD_PLACEHOLDERS.socket}
            problem={socketProblem(selected.socket ?? "")}
            onChange={(socket) => editDeck(selected.id, { socket: socket || undefined })}
          />
        </div>
      )}

      {onTest && (
        <div className="deck-test" data-testid="deck-test">
          <button
            className="button secondary compact"
            data-testid="test-connection"
            disabled={
              testing !== undefined
              || (selected ? rowProblems(selected).length > 0 : false)
            }
            onClick={() => void runTest(selection)}
          >
            <Plug size={13} /> {testing === selection ? "Testing…" : "Test connection"}
          </button>
          <TestResult report={reports[selection]} mode={mode} />
        </div>
      )}

      {saveError && (
        <p className="settings-error" role="alert">
          <AlertTriangle size={13} />
          <span>This change is applied, but saving it failed, so it will not survive a restart. {saveError}</span>
        </p>
      )}
    </div>
  );
}

/** One deck in the chooser: a radio, plus a remove button for a stored one. */
function DeckChoice({ id, label, selected, onSelect, onRemove }: {
  id: string;
  label: string;
  selected: boolean;
  onSelect: () => void;
  onRemove?: () => void;
}) {
  return (
    <div className={selected ? "deck-choice is-selected" : "deck-choice"} data-testid={`deck-choice-${id}`}>
      <label>
        <input type="radio" name="deck" value={id} checked={selected} onChange={onSelect} />
        <span>{label}</span>
      </label>
      {onRemove && (
        <button className="icon-button" aria-label={`Remove ${label}`} data-testid={`remove-deck-${id}`} onClick={onRemove}>
          <Trash2 size={13} />
        </button>
      )}
    </div>
  );
}

/** A two-column text row, in the same shape as every other settings row. */
function TextRow({ id, label, value, placeholder, problem, onChange }: {
  id: string;
  label: string;
  value: string;
  placeholder: string;
  problem: string | undefined;
  onChange: (value: string) => void;
}) {
  return (
    <>
      <div className="settings-row">
        <label htmlFor={id}>{label}</label>
        <input
          id={id}
          className="settings-input"
          type="text"
          spellCheck={false}
          autoComplete="off"
          value={value}
          placeholder={placeholder}
          aria-invalid={problem ? true : undefined}
          onChange={(event) => onChange(event.target.value)}
        />
      </div>
      <Problem text={problem} />
    </>
  );
}

/**
 * What is wrong with a field, said only when something is.
 *
 * It earns its row by being actionable: the charsets are Rust's and a save
 * would otherwise fail with a serde message the panel cannot explain.
 */
function Problem({ text }: { text: string | undefined }) {
  if (!text) return null;
  return <p className="settings-hint is-problem" role="alert">{text}</p>;
}

/**
 * What a `Test connection` found.
 *
 * The whole point of M10 is that this is a **named state** and not one
 * "failed", so the sentence, the remedy and the forwards each render only when
 * there is one — an empty block under a button that has not been pressed is
 * chrome, and a remedy printed for a state that has none is noise.
 *
 * The screen must not blank while a deck is unreachable, which is why this is a
 * block inside the panel rather than anything that replaces it.
 */
function TestResult({ report, mode }: { report: EndpointTestReportDto | undefined; mode: string }) {
  if (!report) return null;
  return (
    <div
      className={report.ok ? "deck-result is-ok" : "deck-result"}
      data-testid="deck-result"
      data-state={report.state}
      role="status"
    >
      <p className="deck-result-line">
        {report.ok ? <Check size={13} /> : <AlertTriangle size={13} />}
        <span data-testid="deck-result-message">{displayText(report.message, DISPLAY_LIMITS.message)}</span>
      </p>
      {report.remedy && (
        <p className="deck-result-remedy">
          <code data-testid="deck-result-remedy">{displayText(report.remedy, DISPLAY_LIMITS.path)}</code>
        </p>
      )}
      {report.detail && (
        <p className="deck-result-detail" data-testid="deck-result-detail">
          {displayText(report.detail, DISPLAY_LIMITS.message)}
        </p>
      )}
      {/* The `ssh -G` disclosure. `disclosureKnown` is the difference between an
          answer and an absence, and rendering an unknown as "none" would be the
          one claim this block must not make: the tunnel inherits these for its
          whole life, and nothing else in the app says so.

          All THREE cases render, which is the PRD #741 final audit's F1 fix on
          this side of the seam. Rendering only the non-empty list made "could
          not look" and "there are none" both come out as an empty screen —
          exactly the conflation the paragraph above forbids, arrived at from
          the render rather than from the parse. */}
      <div className="deck-result-forwards" data-testid="deck-result-forwards">
        {report.disclosureKnown ? (
          <>
            {report.forwards.length > 0 ? (
              <>
                <p>This deck's tunnel will also carry, from your ssh config:</p>
                {/* Keyed by position, not by text: `ssh -G` accumulates
                    forwards, so two matching `Host` blocks carrying the same
                    directive print the same line twice — and two unreadable
                    lines of the same shape collapse to the same text too. */}
                <ul data-testid="deck-result-forward-list">
                  {report.forwards.map((forward, index) => (
                    <li key={`${index}-${forward}`}>{displayText(forward, DISPLAY_LIMITS.path)}</li>
                  ))}
                </ul>
              </>
            ) : (
              <p data-testid="deck-result-no-forwards">Your ssh config adds no forwards to this deck's tunnel.</p>
            )}
            {/* Additive, so absence asserts nothing: ssh naming no host-key
                source at all is what a local deck looks like, and claiming
                "none is configured" there would be the same over-reach in the
                other direction. */}
            {report.knownHosts.length > 0 && (
              <>
                <p>Host keys for this deck are checked against:</p>
                <ul data-testid="deck-result-known-hosts">
                  {report.knownHosts.map((source, index) => (
                    <li key={`${index}-${source}`}>{displayText(source, DISPLAY_LIMITS.path)}</li>
                  ))}
                </ul>
              </>
            )}
          </>
        ) : (
          <p data-testid="deck-result-forwards-unknown">
            This test could not read your resolved ssh config, so it cannot say what forwards this deck's
            tunnel will inherit or where its host keys are checked.
          </p>
        )}
      </div>
      {mode === "fixture" && (
        <p className="settings-hint">Open the packaged app to reach a deck.</p>
      )}
    </div>
  );
}
