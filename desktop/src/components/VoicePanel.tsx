/**
 * The Voice section (PRD #802 M4), and the settings surface's fourth tenant.
 *
 * It implements `SettingsPanelProps` and nothing else — the sheet does not know
 * this is about voice, and this file does not know how the document is stored.
 * Three rows of choices, plus an API-key row for each backend that needs one.
 *
 * # The key row is the reason this panel exists at all in M4
 *
 * Everything else here is a select over a closed enum, and could have waited
 * for M6's surface. The credential could not: PRD #803 M5 named the
 * `SecretStore` seam and deliberately did not build it because *an unused trait
 * is dead code*, and building the store without the one control that reaches it
 * would have made the same mistake one layer up.
 *
 * # What must never happen here, and how the code says so
 *
 * **No credential reaches `onSave`.** The settings document is written to
 * `desktop.toml` and, in the browser preview, to `localStorage` — the two
 * places PRD #803's rule forbids a secret. So the key never enters the
 * document: it goes through `useSettingsBridge()`'s `storeSecret`, which is a
 * Tauri command, and the document holds no reference to it at all. What this
 * panel learns back is a boolean and, when something went wrong, a sentence —
 * never a value.
 *
 * **The value is never read back.** There is no `loadSecret` on the bridge and
 * there must not be: a stored key arriving here is one `JSON.stringify` from
 * the `localStorage` half of the rule. So the input starts empty every time,
 * even when a key is stored, and the panel says *A key is stored* rather than
 * showing a masked preview of one. A preview would be an information channel
 * out of the keychain whose only consumer is reassurance.
 *
 * **A failed save never looks like a saved key.** `storeSecret` rejects rather
 * than resolving with `stored: false`, and this panel renders the rejection's
 * sentence and leaves what the user typed in the field, so they can try again
 * without retyping it. The outcome PRD #802 M4 is written against is *a user
 * who thinks their key is stored and finds voice broken tomorrow*.
 *
 * # Why a select for every choice
 *
 * `docs/develop/desktop-gui.md` picks the control by cardinality: a switch for
 * a boolean, a segmented control for up to about four exclusive options, a
 * select beyond that. Two of these three are inside the segmented range today
 * and are selects anyway, because they will grow — the transcription list gains
 * local whisper at D1, the intent list gains a local model at D2, the
 * activation list gains two modes at D4 — and a panel whose controls change
 * shape as its options arrive reads as three conventions rather than one.
 *
 * The Activation row has exactly one option today. It renders rather than
 * hiding, for `ZoomPanel`'s reason: **the app has no other surface that says
 * what the activation mode is.** Telling the reader what it is, is information
 * even when there is nothing to change.
 */
import { useCallback, useEffect, useState } from "react";
import { AlertTriangle } from "lucide-react";
import {
  DEFAULT_VOICE_SETTINGS,
  VOICE_ACTIVATION_MODES,
  VOICE_INTENT_BACKENDS,
  VOICE_TRANSCRIPTION_BACKENDS,
  type SecretStatusDto,
  type VoiceSecretId,
  type VoiceSettingsDto,
} from "../lib/bridge";
import { useSettingsBridge } from "../lib/settingsBridge";
import type { SettingsPanelProps } from "../lib/settingsContract";

/** The visible label for each token, in the order the select offers them. */
const TRANSCRIPTION_LABELS: Record<string, string> = {
  off: "Off — type instead",
  remote: "Remote service",
};

const INTENT_LABELS: Record<string, string> = {
  claude: "Claude CLI on this machine",
  opencode: "OpenCode CLI on this machine",
  remote: "Remote API",
};

const ACTIVATION_LABELS: Record<string, string> = {
  toggle: "Press to start, press to stop",
};

/** The token a backend takes when it authenticates with a key of the app's own. */
const KEYED = "remote";

export function VoicePanel({ settings, onSave, saveError }: SettingsPanelProps) {
  // Absence is `undefined`, not an empty section — see `DesktopSettingsDto`.
  // Materialised here for display; a save writes the whole section, which is
  // the first moment this build says anything about it.
  const voice = settings.voice ?? DEFAULT_VOICE_SETTINGS;

  const saveVoice = (next: Partial<VoiceSettingsDto>) =>
    onSave({ ...settings, voice: { ...voice, ...next } });

  return (
    <div className="settings-body" data-testid="voice-body">
      <div className="settings-row">
        <label htmlFor="voice-transcription">Speech</label>
        <select
          id="voice-transcription"
          value={voice.transcription}
          onChange={(event) => saveVoice({ transcription: event.target.value })}
        >
          {VOICE_TRANSCRIPTION_BACKENDS.map((token) => (
            <option key={token} value={token}>{TRANSCRIPTION_LABELS[token]}</option>
          ))}
        </select>
      </div>

      {/* The one piece of panel prose here, and it clears
          `docs/develop/desktop-gui.md`'s bar on the stated exception — "a
          consequence the user has to act on". With no transcription backend the
          microphone does nothing, and a user who does not know that reads an
          unresponsive button as broken. PRD #802 makes this a product statement
          rather than a degraded mode: the whole pipeline works from typed
          input. */}
      {voice.transcription === "off" && (
        <p className="settings-hint" data-testid="voice-typed-only">
          Voice commands are typed for now; pick a speech backend to talk instead.
        </p>
      )}

      <div className="settings-row">
        <label htmlFor="voice-intent">Commands</label>
        <select
          id="voice-intent"
          value={voice.intent}
          onChange={(event) => saveVoice({ intent: event.target.value })}
        >
          {VOICE_INTENT_BACKENDS.map((token) => (
            <option key={token} value={token}>{INTENT_LABELS[token]}</option>
          ))}
        </select>
      </div>

      <div className="settings-row">
        <label htmlFor="voice-activation">Activation</label>
        <select
          id="voice-activation"
          value={voice.activation}
          onChange={(event) => saveVoice({ activation: event.target.value })}
        >
          {VOICE_ACTIVATION_MODES.map((token) => (
            <option key={token} value={token}>{ACTIVATION_LABELS[token]}</option>
          ))}
        </select>
      </div>

      {/* One key row per backend that authenticates with a key of the app's
          own, and none otherwise. Asking for a credential a user's chosen
          backends do not need is how a feature becomes one most people never
          try — the reason the agent-CLI intent backend is the default in the
          first place. */}
      {voice.intent === KEYED && (
        <SecretRow id="voice-intent" label="Commands key" />
      )}
      {voice.transcription === KEYED && (
        <SecretRow id="voice-transcription" label="Speech key" />
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
 * One credential: type it, save it, forget it — and never read it back.
 *
 * Rendered only where `useSettingsBridge()` has a provider, the way
 * `EndpointsPanel` hides Test connection: with no bridge there is nothing to
 * press, and a panel rendered standalone in a test must not throw.
 *
 * Its own state is what the user is typing plus what the store last said. The
 * settings document holds none of it, which is the point.
 */
function SecretRow({ id, label }: { id: VoiceSecretId; label: string }) {
  const bridge = useSettingsBridge();
  const [typed, setTyped] = useState("");
  // Two pieces of state, not one, and the split is load-bearing: `status` is
  // what the store last REPORTED and `failure` is what the last save or forget
  // SAID. Folding them into one field looks tidier and is wrong — every
  // operation refreshes the status afterwards, so a single field would have the
  // refresh overwrite the failure sentence with `undefined` and a rejected save
  // would render as if nothing had happened.
  const [status, setStatus] = useState<SecretStatusDto>();
  const [failure, setFailure] = useState<string>();
  const [busy, setBusy] = useState(false);

  const secretStatus = bridge?.secretStatus;
  const refresh = useCallback(async () => {
    if (!secretStatus) return;
    try {
      // `status.problem` is "I could not find out", which is NOT "nothing is
      // stored" and must not render as it — otherwise an unreachable keychain
      // invites the user to type their key again into a store that cannot hold
      // it.
      setStatus(await secretStatus(id));
    } catch (error) {
      setStatus({ stored: false, problem: messageOf(error) });
    }
  }, [id, secretStatus]);

  // Asked on mount, which is the moment a machine with nowhere to put a key
  // should say so — before the user types one rather than after.
  useEffect(() => { void refresh(); }, [refresh]);

  if (!bridge) return null;

  const run = async (work: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await work();
      setFailure(undefined);
      // The field is cleared only on success. A rejected save leaves what was
      // typed where it is, so a retry after unlocking a keychain costs nothing.
      setTyped("");
    } catch (error) {
      setFailure(messageOf(error));
    } finally {
      setBusy(false);
      await refresh();
    }
  };

  const stored = status?.stored;
  const problem = failure ?? status?.problem;

  return (
    <>
      <div className="settings-row">
        <label htmlFor={`secret-${id}`}>{label}</label>
        <div className="voice-secret">
          <input
            id={`secret-${id}`}
            className="settings-input"
            // `password`, so the value is not on screen and a screenshot of a
            // settings panel does not carry it. Autofill is off for the same
            // reason `EndpointsPanel`'s fields turn it off: this is not a login
            // form and a password manager offering to fill it would be wrong.
            type="password"
            spellCheck={false}
            autoComplete="off"
            value={typed}
            placeholder={stored ? "A key is stored" : "Paste the key"}
            onChange={(event) => setTyped(event.target.value)}
          />
          <button
            className="button secondary compact"
            data-testid={`save-${id}`}
            disabled={busy || typed.trim().length === 0}
            onClick={() => void run(() => bridge.storeSecret(id, typed))}
          >
            {stored ? "Replace" : "Save"}
          </button>
          {stored && (
            <button
              className="button secondary compact"
              data-testid={`forget-${id}`}
              disabled={busy}
              onClick={() => void run(() => bridge.forgetSecret(id))}
            >
              Forget
            </button>
          )}
        </div>
      </div>
      <SecretState id={id} stored={stored} problem={problem} />
    </>
  );
}

/**
 * What the store last said, in one line.
 *
 * Three states and not two, because "I could not find out" is its own answer:
 * a keychain that is locked, missing or refusing is a situation the user has to
 * act on, and rendering it as *No key stored* would hide it behind an
 * invitation to retype.
 */
function SecretState({
  id,
  stored,
  problem,
}: {
  id: VoiceSecretId;
  stored: boolean | undefined;
  problem: string | undefined;
}) {
  if (problem) {
    return (
      <p className="settings-hint is-problem" role="alert" data-testid={`secret-problem-${id}`}>
        {problem}
      </p>
    );
  }
  if (stored === undefined) return null;
  return (
    <p className="settings-hint" data-testid={`secret-state-${id}`}>
      {stored ? "A key is stored in your OS keychain." : "No key stored yet."}
    </p>
  );
}

/**
 * The sentence a rejected bridge call carries.
 *
 * The Rust side composes a complete sentence and the Tauri IPC rejects with it
 * as a plain string, so an `Error` is what arrives in the browser preview and a
 * string is what arrives from the command. Anything else is a bug rather than a
 * message, and gets wording of this file's own rather than `String(error)` —
 * which renders `[object Object]` at a user.
 */
function messageOf(error: unknown): string {
  if (typeof error === "string" && error) return error;
  if (error instanceof Error && error.message) return error.message;
  return "The OS credential store could not be reached.";
}
