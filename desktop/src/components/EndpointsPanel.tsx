/**
 * The Daemons section (PRD #741 M7) — add a remote daemon, choose which one the app
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
 * rather than nothing. It is therefore always present and never removable — and
 * it is why this panel is useful before anything is configured.
 *
 * # The chooser is `deckChoices`, the same list the Deck selector offers
 *
 * It was not, until PRD #742 M6. This panel built its own list — the local deck
 * and the stored rows — so when M1 added **All Decks** the panel had no entry
 * for it, and a document storing `all` rendered with no radio checked, no
 * detail form, and a **Test connection** button that was still enabled for a
 * token no probe can answer. One builder means an entry added there arrives
 * here; what stays local to this panel is the draft row, the remove buttons and
 * the detail form, none of which the top bar has.
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
 * # A deck's name is the shared deck list's (issue #1426)
 *
 * A deck is called by its name in the shared deck list — the one `dot-agent-deck
 * connect` takes — with its address beside it, so a name never hides which
 * machine it points at. The name is not free text: it is a validated slug
 * (`DeckName`) and the shared library is the only authority on the rule, so this
 * panel holds no copy of it. It asks the bridge for the name an added deck would
 * get (`defaultDeckName`), whether a typed name is acceptable
 * (`checkDeckName`), and to rename a stored deck (`renameDeck`).
 *
 * Adding a deck therefore takes an explicit confirmation (`save-new-deck`):
 * the name is pre-filled with the derived one and the user may change it
 * before anything is stored. A rename is not a save — a save never renames a
 * deck — so it does not go through `onSave`; whoever provides `renameDeck`
 * applies the document it answers with.
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
import { useEffect, useRef, useState } from "react";
import { AlertTriangle, Check, Pencil, Plug, Plus, Trash2 } from "lucide-react";
import type {
  DesktopSettingsDto,
  EndpointSettingsDto,
  EndpointTestReportDto,
  RemoteEndpointDto,
} from "../lib/bridge";
import { LOCAL_ENDPOINT_SELECTION } from "../lib/bridge";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import {
  ALL_ENDPOINT_SELECTION,
  deckChoices,
  endpointAddress,
  rowProblems,
} from "../lib/endpoints";
import { useSettingsBridge } from "../lib/settingsBridge";
import type { SettingsPanelProps } from "../lib/settingsContract";
import { DaemonAddressFields, DaemonNameRow, Problem, refusalText, saveEndpointSection, sectionOf, TextRow, useNewDaemon } from "./AddDaemonForm";

/*
  How the panel READS the document's endpoint section is `sectionOf`, in
  `AddDaemonForm.tsx` beside the form it shares with the dialog (PRD #2274).
  Its `??` is a render-time stand-in and nothing else; it must never reach a
  save on its own. PRD #742 M8 closed the one exception: {@link runTest}'s
  write-back wrote directly and was protected only by its own find-guard.
*/

export function EndpointsPanel({ settings, onSave, saveError, mode }: SettingsPanelProps) {
  // `undefined` wherever no provider is mounted — a panel rendered standalone,
  // or a surface with no bridge. The Test connection button is then absent
  // rather than present and broken; see `lib/settingsBridge.tsx`.
  const bridge = useSettingsBridge();
  const onTest = bridge?.testEndpoint;
  const checkDeckName = bridge?.checkDeckName;
  const renameDeck = bridge?.renameDeck;
  const section = sectionOf(settings);
  const selection = section.selection;
  const [reports, setReports] = useState<Record<string, EndpointTestReportDto>>({});
  const [testing, setTesting] = useState<string>();
  /**
   * A daemon being added, held until it is storable (PRD #741, Greptile P2 on
   * #1035) — by {@link useNewDaemon}, the same state and confirmation the
   * dashboard's Add a daemon dialog uses (PRD #2274).
   *
   * A draft is not in the document, so it cannot be named by
   * `section.selection` either — {@link shown} is what the panel focuses
   * instead, and the stored selection only moves when the row commits.
   */
  const adding = useNewDaemon(settings, onSave);
  const draft = adding.draft;
  /**
   * What the library said about the last name checked for a STORED daemon's
   * rename, keyed by the row it was checked for and the exact text. A verdict
   * for any other text is stale and is neither shown nor acted on. A draft's
   * name is checked by {@link useNewDaemon}.
   */
  const [nameCheck, setNameCheck] = useState<{ id: string; name: string; refusal: string | null }>();
  /**
   * A stored deck's name while it is being edited, and the name it was
   * edited FROM: a rename that lands, or one made elsewhere, moves the row's
   * name and so retires the edit rather than leaving a stale value on screen.
   */
  const [renameEdit, setRenameEdit] = useState<{ id: string; from: string; value: string }>();
  const [renameRefusal, setRenameRefusal] = useState<{ id: string; message: string }>();
  const [renaming, setRenaming] = useState(false);

  /**
   * The document as of the latest render, for a write-back that happens after
   * an `await` (PRD #741, Greptile P1 on #1035).
   *
   * `runTest` closes over `settings` at call time and an ssh probe takes
   * seconds, so anything the user edits in the meantime is in a NEWER document
   * that the closure cannot see. Spreading the captured one over it reverted
   * those edits — and, where the row had been removed, put it back.
   */
  const latest = useRef(settings);
  useEffect(() => {
    latest.current = settings;
  }, [settings]);

  // The row whose fields are on screen: a draft while one is being filled in,
  // otherwise the deck the document selects.
  const shown = draft ? draft.id : selection;
  const selected = draft ?? section.remote.find((row) => row.id === shown);
  // What the chooser offers: every stored deck, plus an unsaved draft at the
  // end so a user filling one in can see the row they are typing into.
  const rows = draft ? [...section.remote, draft] : section.remote;
  /**
   * The chooser's entries, from {@link deckChoices} — the same list the Deck
   * selector in the top bar offers, and PRD #742 M6's fix for a panel that had
   * its own (issue: this one had no **All Decks** entry, so with `all` stored
   * its `shown` matched no row, nothing was checked and no detail form
   * rendered). A draft is appended the way it always was, by handing the
   * builder the rows the panel is showing rather than the ones the document
   * holds; `deckChoices` reads `remote` and nothing else, so this constructed
   * section is render-only and is never saved.
   */
  const choices = deckChoices({ remote: rows, selection: shown });
  const removable = new Set(rows.map((row) => row.id));
  /**
   * All Decks is selected, so there is no single deck to show fields for or to
   * probe.
   *
   * The detail form's absence is CORRECT here rather than a gap — the fleet has
   * no host, port or key file of its own — so what this drives is the sentence
   * that says so, and the Test-connection button. A probe tests one deck:
   * `endpoint_test::unsealed` reaches its `UnknownDeck` arm for this token
   * because `EndpointId::parse` refuses the reserved word, which is safe and
   * still the wrong answer to give a user who has not chosen a deck.
   */
  const fleetSelected = shown === ALL_ENDPOINT_SELECTION;

  const storedName = selected && !draft ? selected.name ?? "" : "";
  const renameValue = selected && !draft && renameEdit?.id === selected.id && renameEdit.from === storedName
    ? renameEdit.value
    : storedName;
  const verdict = selected && !draft && nameCheck?.id === selected.id && nameCheck.name === renameValue ? nameCheck.refusal : undefined;
  const nameRefusal = (selected && renameRefusal?.id === selected.id ? renameRefusal.message : undefined) ?? verdict ?? undefined;
  /**
   * A rename refused because its deck left the deck list (removed in a
   * terminal meanwhile): the window now shows the list without that row, so
   * the sentence cannot sit beside its field and is said above the form
   * instead, until another deck is chosen or added.
   */
  const goneRefusal = renameRefusal && !rows.some((row) => row.id === renameRefusal.id)
    ? renameRefusal.message
    : undefined;

  /*
    Check a stored daemon's new name with the library as it is typed, so a
    refused one is said beside the field and cannot be confirmed. Its own
    current name is not checked: it is the name it already has. A draft's name
    is checked by `useNewDaemon`.
  */
  const checkedId = selected && !draft ? selected.id : undefined;
  const checkWanted = Boolean(checkedId && renameValue && renameValue !== storedName);
  useEffect(() => {
    if (!checkWanted || !checkedId || !checkDeckName) return;
    let current = true;
    const name = renameValue;
    checkDeckName(name, checkedId)
      .then((refusal) => {
        if (current) setNameCheck({ id: checkedId, name, refusal: refusal || null });
      })
      .catch(() => undefined);
    return () => { current = false; };
  }, [checkWanted, checkedId, renameValue, checkDeckName]);

  /**
   * Write a new endpoint section back into `document`. The whole document goes,
   * with it spread — see the round-trip note at the top of this file.
   *
   * Every write in this panel goes through here, and {@link endpointSectionToSave}
   * is what decides whether there is one to make: a change that changes nothing
   * is not written at all, so it cannot merge a fabricated `remote: []` over
   * rows this client was never shown (PRD #742 M6). `DeckSelector` shares the
   * same function for the same reason.
   *
   * **It takes the document rather than closing over `settings`** (PRD #742 M8).
   * Every handler here writes against the document of its own render, but
   * {@link runTest}'s write-back happens after an `await` and must write against
   * `latest.current`. It used to call `onSave` directly for exactly that reason,
   * which took it around this gate — the one write in the file that the three
   * places claiming "every write goes through here" did not cover. A parameter
   * is what lets both use the same gate rather than the doc-comment asserting
   * they do.
   */
  const saveSectionOf = (document: DesktopSettingsDto, next: EndpointSettingsDto) => saveEndpointSection(onSave, document, next);

  /** {@link saveSectionOf} against the document this render was given. */
  const saveSection = (next: EndpointSettingsDto) => saveSectionOf(settings, next);

  const addDeck = () => {
    // Focused on creation, but NOT stored and NOT selected: an added deck the
    // user then has to select is two steps for one intent, and a deck with no
    // host is not one the app can be pointed at yet.
    adding.start();
    setRenameRefusal(undefined);
  };

  /**
   * Store the draft and select it (issue #1426), through
   * {@link useNewDaemon}'s confirmation, which carries the name check and the
   * name-taken-meanwhile recovery for every surface that adds a daemon.
   */
  const confirmDraft = () => adding.confirm();

  /** Rename the stored deck on screen (issue #1426); never through `onSave`. */
  const rename = async (id: string, name: string) => {
    if (!renameDeck) return;
    setRenaming(true);
    setRenameRefusal(undefined);
    try {
      await renameDeck(id, name);
      setRenameEdit(undefined);
    } catch (cause) {
      setRenameRefusal({ id, message: refusalText(cause) });
    } finally {
      setRenaming(false);
    }
  };

  /**
   * Point the app at a stored deck. Abandons an unfinished draft, visibly — a
   * draft becomes a stored deck the moment it is valid, so the only thing that
   * can be lost here is a row that could not have been saved anyway.
   *
   * Clicking the draft's own radio is a no-op rather than a selection: it is
   * already the row on screen, and writing its id into `selection` would name a
   * deck the document does not contain, which `EndpointSettings::resolve`
   * answers with the local fallback.
   *
   * Clicking the deck ALREADY in force is a no-op too, and that one is a data
   * safety property rather than a tidiness one — abandoning a draft by clicking
   * back onto the selected deck used to write a whole section for a selection
   * that had not moved. {@link saveSection} is where it is refused, because
   * every other handler here can be reached the same way.
   */
  const chooseDeck = (id: string) => {
    if (draft?.id === id) return;
    setRenameRefusal(undefined);
    adding.cancel();
    saveSection({ ...section, selection: id });
  };

  const forgetReport = (id: string) =>
    setReports((current) => {
      const next = { ...current };
      delete next[id];
      return next;
    });

  const removeDeck = (id: string) => {
    if (draft?.id === id) {
      adding.cancel();
      forgetReport(id);
      return;
    }
    const remote = section.remote.filter((row) => row.id !== id);
    // Removing the deck in use falls back to local rather than to another
    // remote one: the app always has a deck, and choosing a different remote
    // one on the user's behalf is a decision they did not make.
    saveSection({ remote, selection: selection === id ? LOCAL_ENDPOINT_SELECTION : selection });
    forgetReport(id);
  };

  const editDeck = (id: string, change: Partial<RemoteEndpointDto>) => {
    if (draft?.id === id) {
      // Held until the user confirms it (`confirmDraft`), which is when it goes
      // into the document and becomes the selection. Until then nothing is
      // written, so no save fails.
      adding.edit(change);
      return;
    }
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
      //
      // Read off `latest`, never off the `settings`/`section` this call closed
      // over: the probe has been running for seconds and the user may have
      // typed, added or removed decks throughout it. Spreading the captured
      // document would revert every one of those edits, and a row removed
      // mid-probe would come back — the `find` below is what refuses that,
      // because a deck that is no longer in the document has no socket to
      // learn.
      if (report.discoveredSocket && id !== LOCAL_ENDPOINT_SELECTION) {
        const current = sectionOf(latest.current);
        const row = current.remote.find((candidate) => candidate.id === id);
        if (row && !row.socket) {
          // Through the shared gate (PRD #742 M8), against `latest.current`.
          //
          // It returns this section unchanged in every case that reaches it
          // today — the `!row.socket` find-guard above only fires on a genuine
          // change — so routing it here is behaviour-preserving. What it buys
          // is that the unreadable-document protection becomes STRUCTURAL: on a
          // document that failed to parse, `sectionOf` fabricates
          // `remote: []`, and it is the gate rather than this particular guard
          // that refuses to merge that over the rows still on disk. A future
          // write-back that fills in a port, a user or a jump host — or one
          // that upserts a row instead of patching one — is then covered
          // without anyone having to notice it needs to be.
          saveSectionOf(latest.current, {
            ...current,
            remote: current.remote.map((candidate) =>
              candidate.id === id ? { ...candidate, socket: report.discoveredSocket } : candidate,
            ),
          });
        }
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
        used to make the legend a grid item by floating it, which disqualifies
        it from being the fieldset's *rendered* legend. Chromium honours that;
        WebKit forces the rendered legend's `float` to `none` — measured on this
        tier, `getComputedStyle(legend).float` is `"left"` in Chromium and
        `"none"` in WebKit — so the legend never becomes a grid item there, the
        control lands in column ONE beside the label, and the 132px column is
        wasted. The app ships on WebKit (WebKitGTK under Tauri on Linux,
        WKWebView on macOS), so the fieldset form is the one that does not hold
        where it matters. `aria-labelledby` on a radiogroup gives the same
        accessible group name in both engines and lays out the same in both.
        This panel was written in that form from the start; issue #1032 then
        converted the one row that had the fieldset — Appearance — and
        `e2e/settings-rows.spec.ts` now measures every panel in both engines.
        (The Zoom row was never affected: a single control is a plain
        `<label for>`, which is an ordinary grid item in both engines.)
      */}
      <div className="settings-row">
        <span className="settings-row-label" id="deck-chooser-label">Daemon</span>
        <div className="deck-choices" role="radiogroup" aria-labelledby="deck-chooser-label" data-testid="deck-choices">
          {choices.map((choice) => (
            <DeckChoice
              key={choice.token}
              id={choice.token}
              label={displayText(choice.label, DISPLAY_LIMITS.name)}
              address={choice.address ? displayText(choice.address, DISPLAY_LIMITS.name) : undefined}
              selected={shown === choice.token}
              onSelect={() => chooseDeck(choice.token)}
              onRemove={removable.has(choice.token) ? () => removeDeck(choice.token) : undefined}
            />
          ))}
          <button className="add-deck" data-testid="add-deck" onClick={addDeck}>
            <Plus size={13} /> Add a daemon
          </button>
        </div>
      </div>

      <Problem text={goneRefusal} />

      {fleetSelected && (
        <p className="settings-hint" data-testid="deck-fleet-note">
          All daemons is every daemon at once, so it has no settings and nothing to test. Choose a daemon to
          change or test that daemon.
        </p>
      )}

      {selected && (
        <div className="deck-detail" data-testid="deck-detail">
          {/* Locked while Add this daemon re-checks it, so the name saved is
              the one that was checked and is on screen. */}
          {draft && adding.naming && <DaemonNameRow id={selected.id} form={adding} />}
          {!draft && renameDeck && (
            <>
              <TextRow
                id={`deck-name-${selected.id}`}
                label="Daemon name"
                value={renameValue}
                placeholder={endpointAddress(selected)}
                problem={nameRefusal}
                onChange={(value) => {
                  setRenameRefusal(undefined);
                  setRenameEdit({ id: selected.id, from: storedName, value });
                }}
              />
              {/* Under the field rather than beside it, so the row keeps the
                  house shape: a label and one input. */}
              <div className="deck-row-actions">
                <button
                  className="button secondary compact"
                  data-testid="rename-deck"
                  disabled={renaming || !renameValue || renameValue === storedName || Boolean(verdict)}
                  onClick={() => void rename(selected.id, renameValue)}
                >
                  <Pencil size={13} /> {renaming ? "Renaming…" : "Rename"}
                </button>
              </div>
            </>
          )}
          <DaemonAddressFields row={selected} onChange={(change) => editDeck(selected.id, change)} />
          {draft && (
            <div className="deck-row-actions">
              <button
                className="button primary compact"
                data-testid="save-new-deck"
                disabled={!adding.canConfirm}
                onClick={() => void confirmDraft()}
              >
                <Check size={13} /> Add this daemon
              </button>
            </div>
          )}
        </div>
      )}

      {onTest && (
        <div className="deck-test" data-testid="deck-test">
          <button
            className="button secondary compact"
            data-testid="test-connection"
            disabled={
              testing !== undefined
              || fleetSelected
              || (selected ? rowProblems(selected).length > 0 : false)
            }
            onClick={() => void runTest(shown)}
          >
            <Plug size={13} /> {testing === shown ? "Testing…" : "Test connection"}
          </button>
          <TestResult report={reports[shown]} mode={mode} />
        </div>
      )}

      {/* Rendered verbatim: `saveError` is a complete sentence composed by
          `useDesktopSettings`, because the same prop also carries "your
          settings file cannot be read" (issue #1072) — which must not acquire a
          "saving it failed" preamble it has not earned. */}
      {saveError && (
        <p className="settings-error" role="alert">
          <AlertTriangle size={13} />
          <span>{saveError}</span>
        </p>
      )}
    </div>
  );
}

/**
 * One deck in the chooser: a radio, plus a remove button for a stored one.
 * A named deck shows its address beside the name (issue #1426).
 */
function DeckChoice({ id, label, address, selected, onSelect, onRemove }: {
  id: string;
  label: string;
  address?: string;
  selected: boolean;
  onSelect: () => void;
  onRemove?: () => void;
}) {
  return (
    <div className={selected ? "deck-choice is-selected" : "deck-choice"} data-testid={`deck-choice-${id}`}>
      <label>
        <input type="radio" name="deck" value={id} checked={selected} onChange={onSelect} />
        <span>{label}</span>
        {address && <small className="deck-choice-address">{address}</small>}
      </label>
      {onRemove && (
        <button className="icon-button" aria-label={`Remove ${label}`} data-testid={`remove-deck-${id}`} onClick={onRemove}>
          <Trash2 size={13} />
        </button>
      )}
    </div>
  );
}

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
                <p>This daemon's tunnel will also carry, from your ssh config:</p>
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
              <p data-testid="deck-result-no-forwards">Your ssh config adds no forwards to this daemon's tunnel.</p>
            )}
            {/* Additive, so absence asserts nothing: ssh naming no host-key
                source at all is what a local deck looks like, and claiming
                "none is configured" there would be the same over-reach in the
                other direction. */}
            {report.knownHosts.length > 0 && (
              <>
                <p>Host keys for this daemon are checked against:</p>
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
            This test could not read your resolved ssh config, so it cannot say what forwards this daemon's
            tunnel will inherit or where its host keys are checked.
          </p>
        )}
      </div>
      {mode === "fixture" && (
        <p className="settings-hint">Open the packaged app to reach a daemon.</p>
      )}
    </div>
  );
}
