/**
 * The Voice section (PRD #802), and the settings surface's fourth tenant.
 *
 * It implements `SettingsPanelProps` and nothing else — the sheet does not know
 * this is about voice, and this file does not know how the document is stored.
 * Two stages, each with the same three controls plus a key row where one is
 * needed.
 *
 * # Every stage is the same three questions
 *
 * **Which** backend, **where** it is, and **which** model — because a user who
 * cannot answer the second cannot answer the first either. The build used to
 * pick both coordinates: speech went to one hosted service and commands to
 * another, which meant the key rows asked for a credential without naming whose
 * it was, and a user with a provider we had not picked had no way in at all.
 * The selects still carry a preset for the common cases, so the fields are an
 * escape hatch rather than homework.
 *
 * **Picking a backend rewrites that stage's endpoint and model.** An endpoint
 * belongs to the backend it names: leaving a loopback URL in place after a
 * switch to a hosted service is a setting that cannot work and does not say so.
 * A user who wants a different one types it afterwards, which is one edit
 * rather than a trap.
 *
 * # What must never happen here, and how the code says so
 *
 * **No credential reaches `onSave`.** The settings document is written to
 * `desktop.toml` and, in the browser preview, to `localStorage` — the two
 * places PRD #803's rule forbids a secret. So the key never enters the
 * document: it goes through `useSettingsBridge()`'s `storeSecret`, which is a
 * Tauri command, and the document holds no reference to it at all. What this
 * panel learns back is a boolean and, when something went wrong, a sentence —
 * never a value. The endpoint fields are the new place that rule could have
 * been broken, and the Rust `ServiceUrl` refuses a `user:password@` authority
 * outright rather than trusting this side not to offer one.
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
 * # Rust validates the endpoint, and this file deliberately does not
 *
 * `ServiceUrl` and `ModelId` run their constructor's check inside their own
 * `Deserialize`, so an endpoint this panel sends is refused at the IPC seam and
 * the refusal arrives as `saveError`. A second copy of those rules here would
 * be a second answer to *what is a valid endpoint*, with nothing keeping the
 * two in step — the defect `normalizeVoiceSettings` avoids on the token side by
 * folding rather than inventing. What this file owes the user instead is the
 * rule in words, which is what the hint under each endpoint field is.
 *
 * # The Activation row is GONE
 *
 * It stated one mode nobody could change — `ActivationMode` has a single
 * variant until PRD #802 D4 brings hold-to-talk and always-on-with-VAD. A row
 * that reads as a control and answers nothing is worse than no row, and the
 * stored field stays either way, so D4 adds a select here and loses nothing.
 */
import { useCallback, useEffect, useState } from "react";
import { AlertTriangle } from "lucide-react";
import {
  DEFAULT_VOICE_SETTINGS,
  LOCAL_SPEECH_IMAGE,
  VOICE_INTENT_BACKENDS,
  VOICE_STAGE_PRESETS,
  VOICE_TRANSCRIPTION_BACKENDS,
  type SecretStatusDto,
  type VoiceSecretId,
  type VoiceSettingsDto,
  type VoiceStageDto,
} from "../lib/bridge";
import { useSettingsBridge } from "../lib/settingsBridge";
import type { SettingsPanelProps } from "../lib/settingsContract";

/**
 * The visible label for each token, in the order the select offers them.
 *
 * **Named by provider, not by mechanism.** `Remote service` told a user holding
 * three API keys nothing about which one to paste; the provider's name is the
 * word they recognise, because it is the one on the key. The keyless options
 * say so in the label rather than leaving it to be discovered by the absence of
 * a key row.
 */
const TRANSCRIPTION_LABELS: Record<string, string> = {
  local: "On this machine — speech container, no key",
  remote: "OpenAI — needs an OpenAI API key",
};

const INTENT_LABELS: Record<string, string> = {
  claude: "Claude CLI on this machine — no key",
  remote: "Anthropic API — needs an Anthropic API key",
};

/** The token a backend takes when it authenticates with a key of the app's own. */
const KEYED = "remote";

/** The stages, in the order the panel asks about them. */
type Stage = "transcription" | "intent";

/**
 * Which stages reach their endpoint over HTTP.
 *
 * `intent: "claude"` spawns a CLI on this machine, so its endpoint and model
 * are stored — they are what the user sees the moment they switch to the keyed
 * backend — and offering them beside a subprocess would be two controls that
 * change nothing. Mirrors `IntentBackend::is_http` Rust-side.
 */
const usesEndpoint = (stage: Stage, backend: string): boolean =>
  stage === "transcription" || backend === KEYED;

export function VoicePanel({ settings, onSave, saveError }: SettingsPanelProps) {
  // Absence is `undefined`, not an empty section — see `DesktopSettingsDto`.
  // Materialised here for display; a save writes the whole section, which is
  // the first moment this build says anything about it.
  const voice = settings.voice ?? DEFAULT_VOICE_SETTINGS;

  const saveVoice = (next: VoiceSettingsDto) => onSave({ ...settings, voice: next });

  const saveStage = (stage: Stage, next: Partial<VoiceStageDto>) =>
    saveVoice({ ...voice, [stage]: { ...voice[stage], ...next } });

  // A backend brings its own coordinates with it — see the file's header. A
  // backend with no preset (one a newer build wrote) keeps what is stored
  // rather than losing it to a fabricated default.
  const chooseBackend = (stage: Stage, backend: string) =>
    saveVoice({
      ...voice,
      [stage]: VOICE_STAGE_PRESETS[stage][backend] ?? { ...voice[stage], backend },
    });

  return (
    <div className="settings-body" data-testid="voice-body">
      <div className="settings-row">
        <label htmlFor="voice-transcription">Speech</label>
        <select
          id="voice-transcription"
          value={voice.transcription.backend}
          onChange={(event) => chooseBackend("transcription", event.target.value)}
        >
          {VOICE_TRANSCRIPTION_BACKENDS.map((token) => (
            <option key={token} value={token}>{TRANSCRIPTION_LABELS[token]}</option>
          ))}
        </select>
      </div>

      {/* The one piece of panel prose here, and it clears
          `docs/develop/desktop-gui.md`'s bar on the stated exception — "a
          consequence the user has to act on". The keyless default is a
          container the user has to be running, and somebody who does not know
          that reads the first failed utterance as the feature being broken.
          The wording tracks `voice::transcribe::unreachable_detail`, which is
          what they meet if it is not running. */}
      {voice.transcription.backend === "local" && (
        <p className="settings-hint" data-testid="voice-speech-local">
          Speech stays on this machine. Start the container with
          {" "}<code>docker run -d -p {portOf(voice.transcription.endpoint)}:8000 {LOCAL_SPEECH_IMAGE}</code>
          {" "}— the first utterance downloads the model.
        </p>
      )}

      <StageFields stage="transcription" value={voice.transcription} onSave={saveStage} />

      {voice.transcription.backend === KEYED && (
        <SecretRow id="voice-transcription" endpoint={voice.transcription.endpoint} />
      )}

      <div className="settings-row">
        <label htmlFor="voice-intent">Commands</label>
        <select
          id="voice-intent"
          value={voice.intent.backend}
          onChange={(event) => chooseBackend("intent", event.target.value)}
        >
          {VOICE_INTENT_BACKENDS.map((token) => (
            <option key={token} value={token}>{INTENT_LABELS[token]}</option>
          ))}
        </select>
      </div>

      <StageFields stage="intent" value={voice.intent} onSave={saveStage} />

      {/* One key row per backend that authenticates with a key of the app's
          own, and none otherwise. Asking for a credential a user's chosen
          backends do not need is how a feature becomes one most people never
          try — the reason both stages default to a backend that needs none. */}
      {voice.intent.backend === KEYED && (
        <SecretRow id="voice-intent" endpoint={voice.intent.endpoint} />
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
 * One stage's endpoint and model, or nothing where the backend uses neither.
 *
 * **Committed on blur and on Enter, not on every keystroke.** A URL is invalid
 * for most of the time it is being typed, so saving per character would put a
 * rejection under the field for `http`, `http:`, `http:/` and so on, and then
 * write eight documents to reach one value. Escape restores what is stored,
 * which is the undo a text field is expected to have.
 */
function StageFields({
  stage,
  value,
  onSave,
}: {
  stage: Stage;
  value: VoiceStageDto;
  onSave: (stage: Stage, next: Partial<VoiceStageDto>) => void;
}) {
  if (!usesEndpoint(stage, value.backend)) return null;
  return (
    <>
      <TextRow
        id={`voice-${stage}-endpoint`}
        label="Endpoint"
        value={value.endpoint}
        hint="https to another machine, or http to this one."
        onCommit={(endpoint) => onSave(stage, { endpoint })}
      />
      <TextRow
        id={`voice-${stage}-model`}
        label="Model"
        value={value.model}
        onCommit={(model) => onSave(stage, { model })}
      />
    </>
  );
}

/** One committed-on-blur text field, with the rule under it in words. */
function TextRow({
  id,
  label,
  value,
  hint,
  onCommit,
}: {
  id: string;
  label: string;
  value: string;
  hint?: string;
  onCommit: (next: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  // The stored value wins whenever it changes underneath — a backend switch
  // rewrites both fields, and a draft left over from the old backend would
  // otherwise sit there looking current.
  useEffect(() => { setDraft(value); }, [value]);

  const commit = () => {
    const next = draft.trim();
    if (next && next !== value) onCommit(next);
    else setDraft(value);
  };

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
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          onBlur={commit}
          onKeyDown={(event) => {
            if (event.key === "Enter") commit();
            if (event.key === "Escape") setDraft(value);
          }}
        />
      </div>
      {hint && <p className="settings-hint" data-testid={`${id}-hint`}>{hint}</p>}
    </>
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
 *
 * **Labelled from the endpoint rather than from the stage.** `Speech key` named
 * the stage the key was for and not the account it came from, which is the one
 * thing a user with several needs to know. The host is what they recognise:
 * they either typed it or picked the preset that carries it, and it stays true
 * when they point the stage somewhere else — which a hardcoded provider name
 * would not.
 */
function SecretRow({ id, endpoint }: { id: VoiceSecretId; endpoint: string }) {
  const bridge = useSettingsBridge();
  const label = `Key for ${hostOf(endpoint)}`;
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
 * The port an endpoint would connect to, for the `docker run` the hint shows.
 *
 * Read from the endpoint rather than from the preset, so a user who moved it is
 * told to publish the port they actually chose — `voice::transcribe::unreachable_detail`
 * does the same on the Rust side, and the two sentences a user may meet in one
 * session must not disagree about which port. `18000` only on a value neither
 * side can parse, which is one Rust would refuse at the save anyway.
 */
function portOf(endpoint: string): string {
  try {
    const url = new URL(endpoint);
    return url.port || (url.protocol === "https:" ? "443" : "80");
  } catch {
    return "18000";
  }
}

/**
 * The host an endpoint names, for the key row's label.
 *
 * Falls back to the whole value rather than to a generic word: a URL this
 * cannot parse is one Rust will refuse at the save, and a label reading "Key
 * for the remote service" would hide which of the two the user is looking at.
 */
function hostOf(endpoint: string): string {
  try {
    return new URL(endpoint).hostname || endpoint;
  } catch {
    return endpoint;
  }
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
