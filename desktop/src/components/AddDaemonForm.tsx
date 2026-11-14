/**
 * The add-a-daemon form, in one place for every surface that offers it
 * (PRD #2274).
 *
 * Settings → Daemons had it inline until PRD #2274 opened the same form from
 * the dashboard's empty-agents note and the Daemon selector, in a dialog
 * (`AddDaemonDialog`). Two copies of a form whose confirmation carries three
 * races (the name check in flight, the user editing during it, the name taken
 * between check and save) would drift, so the state and the confirmation are
 * {@link useNewDaemon} and the fields are {@link DaemonNameRow} and
 * {@link DaemonAddressFields}, and both surfaces render those.
 *
 * What stays in `EndpointsPanel` is what only Settings has: the chooser the
 * draft is listed in while it is filled in, renaming a stored daemon, removing
 * one, and Test connection.
 */
import { useEffect, useRef, useState } from "react";
import type { DesktopSettingsDto, EndpointSettingsDto, RemoteEndpointDto } from "../lib/bridge";
import { LOCAL_ENDPOINT_SELECTION } from "../lib/bridge";
import {
  blankEndpoint,
  endpointSectionToSave,
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

/**
 * How a form READS the document's endpoint section.
 *
 * The `??` is a render-time stand-in and nothing else: a document with no
 * `[endpoints]` section still has a local deck and still has to draw a chooser.
 * It must never reach a save on its own — `remote: []` is the assertion "this
 * user has no decks", and PRD #742 M6 routes every write through
 * {@link endpointSectionToSave}, which is where that rule is written down.
 */
export function sectionOf(settings: DesktopSettingsDto): EndpointSettingsDto {
  return settings.endpoints ?? { remote: [], selection: LOCAL_ENDPOINT_SELECTION };
}

/** A rejection as the sentence to show: the bridge rejects with a plain string. */
export function refusalText(cause: unknown): string {
  if (typeof cause === "string") return cause;
  return cause instanceof Error ? cause.message : String(cause);
}

/**
 * Write a new endpoint section back into `document`, through the one gate
 * every endpoint write goes through ({@link endpointSectionToSave}): a change
 * that changes nothing is not written at all, so it cannot merge a fabricated
 * `remote: []` over rows this client was never shown (PRD #742 M6).
 *
 * `document` is named as what the edit was made against (issue #828): a
 * write-back after an `await` passes the latest document, which can trail the
 * one on screen by a render.
 */
export function saveEndpointSection(onSave: SettingsPanelProps["onSave"], document: DesktopSettingsDto, next: EndpointSettingsDto) {
  const write = endpointSectionToSave(document.endpoints, next);
  if (!write) return undefined;
  return onSave({ ...document, endpoints: write }, document);
}

/** How {@link NewDaemon.confirm} ended. */
export type ConfirmResult =
  /**
   * Stored and selected. That includes a save the settings hook kept on screen
   * after its write failed: `useDesktopSettings` applies the change, says the
   * write failed, and carries the edit into the next save, so the daemon is
   * added in this session exactly as it is from Settings.
   */
  | "added"
  /** Nothing stored: the name was refused, and the reason is beside the field. */
  | "kept"
  /** Nothing stored: the save ran and the file on disk shows the daemon is not in it; the draft is back, and the save error says why. */
  | "restored"
  /** There was nothing valid to store, or the add was cancelled before its save started. */
  | "none";

export interface NewDaemon {
  /** The daemon being added, while it is being filled in and not yet stored. */
  draft: RemoteEndpointDto | undefined;
  /** The row being stored right now, while its save is in flight. */
  pending: RemoteEndpointDto | undefined;
  /** The name it will be stored under; empty means the one the library derives. */
  name: string;
  /** Why the name on screen cannot be used, when the library said so. */
  refusal: string | undefined;
  /** The name is being checked one last time before the save. */
  confirming: boolean;
  /** Whether the name can be chosen here at all: only with a bridge that suggests or checks one. */
  naming: boolean;
  /** Whether {@link confirm} would store something now. */
  canConfirm: boolean;
  start: () => void;
  cancel: () => void;
  edit: (change: Partial<RemoteEndpointDto>) => void;
  setName: (name: string) => void;
  confirm: () => Promise<ConfirmResult>;
}

/**
 * A daemon being added, held HERE until it is storable (PRD #741, Greptile P2
 * on #1035).
 *
 * Adding used to write the blank row straight into the document. Rust's
 * `Hostname` refuses an empty string, so `desktop_set_settings` rejected the
 * whole document — which meant the row lived only in optimistic state and
 * vanished on restart, AND that **every unrelated save failed for as long as
 * it was there**.
 *
 * `startOpen` starts with a draft already being filled in, for a surface that
 * is the form and nothing else (the dialog).
 */
export function useNewDaemon(settings: DesktopSettingsDto, onSave: SettingsPanelProps["onSave"], { startOpen = false }: { startOpen?: boolean } = {}): NewDaemon {
  // `undefined` wherever no provider is mounted — a form rendered standalone.
  // The name field is then absent, and a save stores no name (the library
  // derives one); see `lib/settingsBridge.tsx`.
  const bridge = useSettingsBridge();
  const defaultDeckName = bridge?.defaultDeckName;
  const checkDeckName = bridge?.checkDeckName;
  const [draft, setDraft] = useState<RemoteEndpointDto | undefined>(() => (startOpen ? blankEndpoint() : undefined));
  const [pending, setPending] = useState<RemoteEndpointDto>();
  /**
   * The name the draft will be stored under (issue #1426), held beside it
   * rather than in it so a pre-fill can arrive without the draft's fields
   * moving. Empty means "the one the library derives", which is what a save
   * without a name gets.
   */
  const [name, setNameState] = useState("");
  /** Once the user types a name, a later pre-fill must not overwrite it. */
  const touched = useRef(false);
  /** The draft as of the latest edit, for the confirmation that resolves after an `await`. */
  const latestDraft = useRef(draft);
  latestDraft.current = draft;
  /**
   * Whether the surface holding this form is still mounted. A dialog closed
   * while the name check is in flight unmounts with the confirmation still
   * awaiting; it must not then store the daemon the user walked away from
   * (Greptile and Qodo, PR #2319). Set in an effect rather than at render so
   * a StrictMode remount sets it back.
   */
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; };
  }, []);
  /**
   * What the library said about the last name checked, keyed by the draft it
   * was checked for and the exact text. A verdict for any other text is stale
   * and is neither shown nor acted on.
   */
  const [nameCheck, setNameCheck] = useState<{ id: string; name: string; refusal: string | null }>();
  const [confirming, setConfirming] = useState(false);
  /**
   * The document as of the latest render, for the save that happens after an
   * `await` (PRD #741, Greptile P1 on #1035): spreading the document captured
   * before it over a newer one reverts whatever the user changed meanwhile.
   */
  const latest = useRef(settings);
  useEffect(() => {
    latest.current = settings;
  }, [settings]);

  /*
    Issue #1426: pre-fill the name with the one the library would derive for
    the draft, as soon as it has an address to derive from — and only until the
    user types one of their own. A stale answer (the host changed again while
    it was in flight) is dropped.
  */
  const draftId = draft?.id;
  const draftHost = draft?.host ?? "";
  const draftUser = draft?.user;
  useEffect(() => {
    if (!draftId || !defaultDeckName || touched.current) return;
    if (hostProblem(draftHost) || userProblem(draftUser ?? "")) return;
    let current = true;
    defaultDeckName(draftHost, draftUser || undefined)
      .then((derived) => {
        if (current && !touched.current) setNameState(derived);
      })
      .catch(() => undefined);
    return () => { current = false; };
  }, [draftId, draftHost, draftUser, defaultDeckName]);

  /* Check the name on screen with the library as it is typed, so a refused one
     is said beside the field and cannot be confirmed. */
  useEffect(() => {
    if (!draftId || !name || !checkDeckName) return;
    let current = true;
    const checked = name;
    checkDeckName(checked, undefined)
      .then((refusal) => {
        if (current) setNameCheck({ id: draftId, name: checked, refusal: refusal || null });
      })
      .catch(() => undefined);
    return () => { current = false; };
  }, [draftId, name, checkDeckName]);

  const refusal = draft && nameCheck?.id === draft.id && nameCheck.name === name ? nameCheck.refusal ?? undefined : undefined;

  const start = () => {
    setDraft(blankEndpoint());
    setNameState("");
    touched.current = false;
  };

  const cancel = () => {
    // The ref too, at once: a confirmation awaiting the name check reads it
    // next, possibly before (or without) another render.
    latestDraft.current = undefined;
    setDraft(undefined);
    setNameState("");
  };

  const edit = (change: Partial<RemoteEndpointDto>) => {
    setDraft((current) => (current ? { ...current, ...change } : current));
  };

  const setName = (next: string) => {
    touched.current = true;
    setNameState(next);
  };

  /**
   * Store the draft and select it (issue #1426) — the confirmation is the
   * user's, so the pre-filled name can be changed first.
   *
   * The name is checked once more at the moment of confirming, because the
   * verdict on screen may still be in flight; a refusal is shown and nothing is
   * stored. An empty name stores none, and the library derives one. The name
   * field is locked (`confirming`) while that check runs, so the name captured
   * here is the one checked, saved and on screen.
   *
   * The name can still be taken between that check and the save (a `remote
   * add` in a terminal). The save then writes nothing and the window is shown
   * the list as it is on disk, without this daemon — so the draft comes back,
   * name and all, and the name check beside the field says why, for the user
   * to choose another name and add it again.
   */
  const confirm = async (): Promise<ConfirmResult> => {
    const asked = latestDraft.current;
    if (!asked || rowProblems(asked).length > 0) return "none";
    const chosen = name;
    if (chosen && checkDeckName) {
      setConfirming(true);
      let verdict: string | null;
      try {
        verdict = await checkDeckName(chosen, undefined);
      } catch (cause) {
        verdict = refusalText(cause);
      } finally {
        setConfirming(false);
      }
      if (verdict) {
        setNameCheck({ id: asked.id, name: chosen, refusal: verdict });
        return "kept";
      }
    }
    // The draft as it is NOW, which the user may have edited while the check
    // ran; abandoned meanwhile — cancelled, or its surface closed — means there
    // is nothing to store.
    const now = latestDraft.current;
    if (!alive.current || !now || now.id !== asked.id || rowProblems(now).length > 0) return "none";
    const document = latest.current;
    const current = sectionOf(document);
    const row: RemoteEndpointDto = chosen ? { ...now, name: chosen } : now;
    setDraft(undefined);
    setPending(row);
    let outcome: Awaited<ReturnType<SettingsPanelProps["onSave"]>> | undefined;
    try {
      outcome = await saveEndpointSection(onSave, document, { remote: [...current.remote, row], selection: row.id });
    } finally {
      setPending(undefined);
    }
    // A failure with no document from disk leaves the change applied and
    // carried into the next save (`useDesktopSettings`), so the daemon IS
    // added for this session; bringing the draft back would offer to add it a
    // second time over a list that already holds it (Qodo, PR #2319).
    if (!outcome || outcome.saved || !outcome.disk) return "added";
    if (sectionOf(outcome.disk).remote.some((stored) => stored.id === row.id)) return "added";
    // Another draft started meanwhile is the user's newer intent; leave it.
    if (latestDraft.current) return "restored";
    setDraft(now);
    setNameState(chosen);
    touched.current = Boolean(chosen);
    setNameCheck(undefined);
    return "restored";
  };

  return {
    draft,
    pending,
    name,
    refusal,
    confirming,
    naming: Boolean(defaultDeckName || checkDeckName),
    canConfirm: Boolean(draft) && !confirming && pending === undefined && rowProblems(draft ?? blankEndpoint()).length === 0 && !refusal,
    start,
    cancel,
    edit,
    setName,
    confirm,
  };
}

/** The draft's **Deck name** row, pre-filled and checked as it is typed (issue #1426). */
export function DaemonNameRow({ id, form }: { id: string; form: NewDaemon }) {
  return (
    <TextRow
      id={`deck-name-${id}`}
      label="Deck name"
      value={form.name}
      placeholder="derived from the address"
      problem={form.refusal}
      disabled={form.confirming || form.pending !== undefined}
      onChange={form.setName}
    />
  );
}

/**
 * A daemon's address fields — Host, User, Port, Key file, Jump host and Daemon
 * socket — for a stored daemon in Settings and for one being added, wherever
 * it is added from. Each says beside it what is wrong with it.
 */
export function DaemonAddressFields({ row, onChange, disabled, autoFocus }: {
  row: RemoteEndpointDto;
  onChange: (change: Partial<RemoteEndpointDto>) => void;
  disabled?: boolean;
  /** Focus the Host field when it mounts: the first thing a new daemon needs. */
  autoFocus?: boolean;
}) {
  return (
    <>
      <TextRow
        id={`deck-host-${row.id}`}
        label="Host"
        value={row.host}
        placeholder={FIELD_PLACEHOLDERS.host}
        problem={hostProblem(row.host)}
        disabled={disabled}
        autoFocus={autoFocus}
        onChange={(host) => onChange({ host })}
      />
      <TextRow
        id={`deck-user-${row.id}`}
        label="User"
        value={row.user ?? ""}
        placeholder={FIELD_PLACEHOLDERS.user}
        problem={userProblem(row.user ?? "")}
        disabled={disabled}
        onChange={(user) => onChange({ user: user || undefined })}
      />
      <div className="settings-row">
        <label htmlFor={`deck-port-${row.id}`}>Port</label>
        <input
          id={`deck-port-${row.id}`}
          className="settings-input is-narrow"
          type="number"
          min={1}
          max={65535}
          value={row.port}
          disabled={disabled}
          onChange={(event) => onChange({ port: Number(event.target.value) })}
        />
      </div>
      <Problem text={portProblem(row.port)} />
      <TextRow
        id={`deck-identity-${row.id}`}
        label="Key file"
        value={row.identity ?? ""}
        placeholder={FIELD_PLACEHOLDERS.identity}
        problem={identityProblem(row.identity ?? "")}
        disabled={disabled}
        onChange={(identity) => onChange({ identity: identity || undefined })}
      />
      <TextRow
        id={`deck-jump-${row.id}`}
        label="Jump host"
        value={row.jump ?? ""}
        placeholder={FIELD_PLACEHOLDERS.jump}
        problem={jumpProblem(row.jump ?? "")}
        disabled={disabled}
        onChange={(jump) => onChange({ jump: jump || undefined })}
      />
      <TextRow
        id={`deck-socket-${row.id}`}
        label="Daemon socket"
        value={row.socket ?? ""}
        placeholder={FIELD_PLACEHOLDERS.socket}
        problem={socketProblem(row.socket ?? "")}
        disabled={disabled}
        onChange={(socket) => onChange({ socket: socket || undefined })}
      />
    </>
  );
}

export function TextRow({ id, label, value, placeholder, problem, disabled, autoFocus, onChange }: {
  id: string;
  label: string;
  value: string;
  placeholder: string;
  problem: string | undefined;
  disabled?: boolean;
  autoFocus?: boolean;
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
          disabled={disabled}
          autoFocus={autoFocus}
          aria-invalid={problem ? true : undefined}
          onChange={(event) => onChange(event.target.value)}
        />
      </div>
      <Problem text={problem} />
    </>
  );
}

export function Problem({ text }: { text: string | undefined }) {
  if (!text) return null;
  return <p className="settings-hint is-problem" role="alert">{text}</p>;
}
