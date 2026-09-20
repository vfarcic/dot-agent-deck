import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { VoicePanel } from "./VoicePanel";
import {
  DEFAULT_DESKTOP_SETTINGS,
  DEFAULT_VOICE_SETTINGS,
  VOICE_ACTIVATION_MODES,
  VOICE_INTENT_BACKENDS,
  VOICE_STAGE_PRESETS,
  VOICE_TRANSCRIPTION_BACKENDS,
  type DesktopSettingsDto,
  type SecretStatusDto,
} from "../lib/bridge";
import { SettingsBridgeProvider, type SettingsBridge } from "../lib/settingsBridge";
import type { RuntimeMode } from "../types";

/** A credential-shaped value, so a leak is searchable rather than plausible. */
const KEY = "sk-not-a-real-key-0123456789";

/** What each key row is called now that the label names the provider. */
const COMMANDS_KEY = "Key for api.anthropic.com";
const SPEECH_KEY = "Key for api.openai.com";

function renderPanel(
  overrides: Partial<DesktopSettingsDto> = {},
  options: {
    mode?: RuntimeMode;
    saveError?: string;
    bridge?: Partial<SettingsBridge>;
  } = {},
) {
  const onSave = vi.fn();
  const secretStatus = vi.fn(async (): Promise<SecretStatusDto> => ({ stored: false }));
  const storeSecret = vi.fn(async (): Promise<SecretStatusDto> => ({ stored: true }));
  const forgetSecret = vi.fn(async (): Promise<SecretStatusDto> => ({ stored: false }));
  const bridge: SettingsBridge = {
    testEndpoint: vi.fn(),
    secretStatus,
    storeSecret,
    forgetSecret,
    ...options.bridge,
  } as SettingsBridge;

  render(
    <SettingsBridgeProvider value={bridge}>
      <VoicePanel
        settings={{ ...DEFAULT_DESKTOP_SETTINGS, ...overrides }}
        onSave={onSave}
        saveError={options.saveError}
        mode={options.mode ?? "live"}
      />
    </SettingsBridgeProvider>,
  );
  return { onSave, bridge };
}

/** A document whose command backend is the one that needs a key. */
const KEYED = {
  voice: { ...DEFAULT_VOICE_SETTINGS, intent: VOICE_STAGE_PRESETS.intent.remote },
};

/** A document where BOTH stages need a key. */
const BOTH_KEYED = {
  voice: {
    activation: "toggle",
    intent: VOICE_STAGE_PRESETS.intent.remote,
    transcription: VOICE_STAGE_PRESETS.transcription.remote,
  },
};

describe("VoicePanel", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("renders every choice the app actually ships an adapter for", () => {
    renderPanel();
    expect(screen.getByLabelText("Speech")).toHaveValue("local");
    expect(screen.getByLabelText("Commands")).toHaveValue("claude");
    // The lists are the closed sets, so a token the Rust side would fold away
    // cannot be offered here.
    expect(screen.getByLabelText("Speech").querySelectorAll("option")).toHaveLength(
      VOICE_TRANSCRIPTION_BACKENDS.length,
    );
    expect(screen.getByLabelText("Commands").querySelectorAll("option")).toHaveLength(
      VOICE_INTENT_BACKENDS.length,
    );
  });

  /**
   * The owner's reason for making these selectable at all: a user cannot know
   * which key to paste while the option is called *Remote service*. Every keyed
   * option names its provider, and every keyless one says it needs no key.
   */
  it("names the provider on every option, and says which ones need a key", () => {
    renderPanel();
    expect(screen.getByRole("option", { name: /OpenAI — needs an OpenAI API key/ })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /Anthropic API — needs an Anthropic API key/ })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /On this machine — speech container, no key/ })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /Claude CLI on this machine — no key/ })).toBeInTheDocument();
  });

  /**
   * The Activation row is GONE (PRD #802's provider work). It stated one mode
   * nobody could change, which reads as a control and answers nothing. The
   * stored field stays for D4 — which is what `VOICE_ACTIVATION_MODES` is still
   * here for — and a save still round-trips it.
   */
  it("renders no Activation row while there is one mode to state", () => {
    renderPanel();
    expect(screen.queryByTestId("voice-activation")).not.toBeInTheDocument();
    expect(screen.queryByText(/Press to start, press to stop/)).not.toBeInTheDocument();
    expect(screen.queryByText("Activation")).not.toBeInTheDocument();
    // The list D4 will render a select from, still exported and still one long.
    expect(VOICE_ACTIVATION_MODES).toEqual(["toggle"]);
  });

  it("round-trips the stored activation mode even though nothing renders it", () => {
    const { onSave } = renderPanel({
      voice: { ...DEFAULT_VOICE_SETTINGS, activation: "hold-to-talk" },
    });
    fireEvent.change(screen.getByLabelText("Commands"), { target: { value: "remote" } });
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({
        voice: expect.objectContaining({ activation: "hold-to-talk" }),
      }),
    );
  });

  /**
   * An absent `[voice]` section is *unspecified*, not empty — every document on
   * disk today has none — so the panel materialises this build's defaults for
   * display without the document having said anything.
   */
  it("shows this build's defaults for a document with no voice section", () => {
    renderPanel({ voice: undefined });
    expect(screen.getByLabelText("Speech")).toHaveValue(DEFAULT_VOICE_SETTINGS.transcription.backend);
    expect(screen.getByLabelText("Commands")).toHaveValue(DEFAULT_VOICE_SETTINGS.intent.backend);
    // The keyless default has a prerequisite, and the panel names the command
    // that satisfies it rather than leaving it to the first failed utterance.
    expect(screen.getByTestId("voice-speech-local")).toBeVisible();
    expect(screen.getByTestId("voice-speech-local")).toHaveTextContent(
      "docker run -d -p 18000:8000 ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu",
    );
  });

  /**
   * The hint's port comes from the endpoint the user actually configured, the
   * way `voice::transcribe::unreachable_detail` does Rust-side. Two sentences a
   * user can meet in one session must not disagree about which port to publish.
   */
  it("names the port the configured endpoint uses, not the preset's", () => {
    renderPanel({
      voice: {
        ...DEFAULT_VOICE_SETTINGS,
        transcription: { backend: "local", endpoint: "http://127.0.0.1:9123/v1/audio/transcriptions", model: "Systran/faster-whisper-tiny.en" },
      },
    });
    expect(screen.getByTestId("voice-speech-local")).toHaveTextContent("-p 9123:8000");
  });

  /**
   * The panel's only channel for a choice is `onSave`, and it must send the
   * WHOLE document — a panel that sent just its own section would drop every
   * section this build's UI has not loaded, which is the guarantee
   * `settingsContract.ts` states.
   */
  it("saves the whole document with only its own section replaced", () => {
    const { onSave } = renderPanel({ appearance: { mode: "dark" } });
    fireEvent.change(screen.getByLabelText("Commands"), { target: { value: "remote" } });
    expect(onSave).toHaveBeenCalledWith({
      ...DEFAULT_DESKTOP_SETTINGS,
      appearance: { mode: "dark" },
      voice: { ...DEFAULT_VOICE_SETTINGS, intent: VOICE_STAGE_PRESETS.intent.remote },
    });
  });

  it("writes a full section on the first change, so an absent one becomes specified", () => {
    const { onSave } = renderPanel({ voice: undefined });
    fireEvent.change(screen.getByLabelText("Speech"), { target: { value: "remote" } });
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({
        voice: {
          activation: "toggle",
          intent: VOICE_STAGE_PRESETS.intent.claude,
          transcription: VOICE_STAGE_PRESETS.transcription.remote,
        },
      }),
    );
  });

  /**
   * An endpoint belongs to the backend it names. Leaving the loopback URL in
   * place after a switch to a hosted service is a setting that cannot work and
   * does not say so, so the switch brings its own coordinates.
   */
  it("rewrites the stage's endpoint and model when the backend changes", () => {
    const { onSave } = renderPanel();
    fireEvent.change(screen.getByLabelText("Speech"), { target: { value: "remote" } });
    expect(onSave.mock.calls[0][0].voice.transcription).toEqual({
      backend: "remote",
      endpoint: "https://api.openai.com/v1/audio/transcriptions",
      model: "whisper-1",
    });
  });

  /**
   * The other half of the owner's decision: a user can point a stage at a
   * provider this build did not pick. Committed on blur, because a URL is
   * invalid for most of the time it is being typed.
   */
  it("saves a hand-typed endpoint on blur rather than on every keystroke", () => {
    const { onSave } = renderPanel();
    const field = screen.getByLabelText("Endpoint");
    fireEvent.change(field, { target: { value: "http://127.0.0.1:9000/v1/audio/transcriptions" } });
    expect(onSave).not.toHaveBeenCalled();
    fireEvent.blur(field);
    expect(onSave.mock.calls[0][0].voice.transcription).toEqual({
      backend: "local",
      endpoint: "http://127.0.0.1:9000/v1/audio/transcriptions",
      model: "Systran/faster-whisper-tiny.en",
    });
  });

  it("saves a hand-typed model, and abandons an edit on Escape", () => {
    const { onSave } = renderPanel();
    const model = screen.getByLabelText("Model");
    fireEvent.change(model, { target: { value: "Systran/faster-whisper-base.en" } });
    fireEvent.keyDown(model, { key: "Enter" });
    expect(onSave.mock.calls[0][0].voice.transcription.model).toBe("Systran/faster-whisper-base.en");

    onSave.mockClear();
    fireEvent.change(model, { target: { value: "whatever" } });
    fireEvent.keyDown(model, { key: "Escape" });
    fireEvent.blur(model);
    expect(onSave).not.toHaveBeenCalled();
  });

  /**
   * The agent-CLI command backend spawns a process on this machine, so an
   * endpoint and a model beside it would be two controls that change nothing.
   * Speech has no such variant — both of its backends are HTTP — so its fields
   * are always there.
   */
  it("offers no endpoint or model beside the command backend that spawns a CLI", () => {
    renderPanel();
    // Speech only: the command stage is on the agent CLI, which is reached by
    // spawning a process rather than by making a request.
    expect(screen.getAllByLabelText("Endpoint")).toHaveLength(1);
    expect(screen.getAllByLabelText("Model")).toHaveLength(1);
    expect(screen.getByLabelText("Endpoint")).toHaveValue(
      VOICE_STAGE_PRESETS.transcription.local.endpoint,
    );
  });

  it("offers endpoint and model for both stages once both are reached over HTTP", () => {
    renderPanel(BOTH_KEYED);
    expect(screen.getAllByLabelText("Endpoint")).toHaveLength(2);
    expect(screen.getAllByLabelText("Model")).toHaveLength(2);
  });

  /** No backend needs a key, so the panel does not ask for one. */
  it("asks for no key while no chosen backend uses one", () => {
    renderPanel();
    expect(screen.queryByLabelText(COMMANDS_KEY)).not.toBeInTheDocument();
    expect(screen.queryByLabelText(SPEECH_KEY)).not.toBeInTheDocument();
    expect(screen.queryByLabelText(/^Key for /)).not.toBeInTheDocument();
  });

  it("asks for a key per backend that authenticates with one", async () => {
    renderPanel(BOTH_KEYED);
    expect(screen.getByLabelText(COMMANDS_KEY)).toBeVisible();
    expect(screen.getByLabelText(SPEECH_KEY)).toBeVisible();
    await waitFor(() => expect(screen.getByTestId("secret-state-voice-intent")).toHaveTextContent("No key stored yet."));
  });

  /**
   * The key row is labelled from the ENDPOINT, so it keeps telling the truth
   * for a user who pointed the stage at a provider this build did not pick.
   */
  it("names the key row after wherever the stage actually points", () => {
    renderPanel({
      voice: {
        ...DEFAULT_VOICE_SETTINGS,
        intent: { backend: "remote", endpoint: "https://gateway.example.com/v1/messages", model: "claude-haiku-4-5" },
      },
    });
    expect(screen.getByLabelText("Key for gateway.example.com")).toBeVisible();
    expect(screen.queryByLabelText(COMMANDS_KEY)).not.toBeInTheDocument();
  });

  /**
   * **The rule this panel exists under**: PRD #803 forbids a secret in
   * `desktop.toml` and in `localStorage`, and the settings document is written
   * to both. So the key must reach the bridge and nothing else — not `onSave`,
   * not storage, not the rendered document.
   */
  it("sends a typed key to the credential store and puts it nowhere else", async () => {
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    const { onSave, bridge } = renderPanel(KEYED);

    fireEvent.change(screen.getByLabelText(COMMANDS_KEY), { target: { value: KEY } });
    fireEvent.click(screen.getByTestId("save-voice-intent"));

    await waitFor(() => expect(bridge.storeSecret).toHaveBeenCalledWith("voice-intent", KEY));
    // The document never carries it: not through a save, and not as a field.
    expect(onSave).not.toHaveBeenCalled();
    // Nor does storage, by any key — the fixture bridge writes the document to
    // `localStorage` verbatim, so a key on the document would land there.
    for (const call of setItem.mock.calls) {
      expect(String(call[1])).not.toContain(KEY);
    }
    expect(JSON.stringify(window.localStorage)).not.toContain(KEY);
    // And it is cleared from the field once stored, because nothing reads it
    // back — the panel says a key is stored rather than showing one.
    await waitFor(() => expect(screen.getByLabelText(COMMANDS_KEY)).toHaveValue(""));
    setItem.mockRestore();
  });

  /**
   * The endpoint and model fields are the new way a value could reach
   * `localStorage`, so the same rule is asserted over them: typing a
   * credential-shaped string into one and committing it must be the user's own
   * doing and nothing the panel invents — and no key row's value ever joins it.
   */
  it("puts no key value in localStorage even while both key rows are on screen", async () => {
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    renderPanel(BOTH_KEYED);

    fireEvent.change(screen.getByLabelText(COMMANDS_KEY), { target: { value: KEY } });
    fireEvent.change(screen.getByLabelText(SPEECH_KEY), { target: { value: `${KEY}-speech` } });
    // A commit on the endpoint field, which is the one control here that DOES
    // reach the document — it must carry the endpoint and nothing beside it.
    fireEvent.blur(screen.getAllByLabelText("Endpoint")[0]);

    for (const call of setItem.mock.calls) {
      expect(String(call[1])).not.toContain(KEY);
    }
    expect(JSON.stringify(window.localStorage)).not.toContain(KEY);
    // What a key row's field holds is deliberately NOT asserted here: it has to
    // hold what the user typed for a save to be possible at all. The rule is
    // about where it goes next, which is the keychain and nowhere else.
    setItem.mockRestore();
  });

  it("never renders the stored value, only that there is one", async () => {
    renderPanel(KEYED, {
      bridge: { secretStatus: vi.fn(async () => ({ stored: true })) },
    });
    await waitFor(() =>
      expect(screen.getByTestId("secret-state-voice-intent")).toHaveTextContent(
        "A key is stored in your OS keychain.",
      ),
    );
    // A password field, so it is not on screen and a screenshot of the panel
    // does not carry what is being typed.
    expect(screen.getByLabelText(COMMANDS_KEY)).toHaveAttribute("type", "password");
    expect(screen.getByLabelText(COMMANDS_KEY)).toHaveValue("");
    expect(document.body.textContent ?? "").not.toContain(KEY);
  });

  /**
   * The outcome PRD #802 M4 is written against: *a user who thinks their key is
   * stored and finds voice broken tomorrow.* A rejected store shows its own
   * sentence, keeps what was typed so a retry costs nothing, and never claims a
   * key is stored.
   */
  it("shows a failed save as a failure and keeps what was typed", async () => {
    const sentence =
      "This machine has no OS credential store available, so the key was not saved.";
    renderPanel(KEYED, {
      bridge: { storeSecret: vi.fn(async () => { throw new Error(sentence); }) },
    });

    fireEvent.change(screen.getByLabelText(COMMANDS_KEY), { target: { value: KEY } });
    fireEvent.click(screen.getByTestId("save-voice-intent"));

    await waitFor(() =>
      expect(screen.getByTestId("secret-problem-voice-intent")).toHaveTextContent(sentence),
    );
    expect(screen.getByLabelText(COMMANDS_KEY)).toHaveValue(KEY);
    expect(screen.queryByTestId("secret-state-voice-intent")).not.toBeInTheDocument();
  });

  /**
   * "I could not find out" is not "nothing is stored", and rendering it as one
   * would invite the user to type their key again into a store that cannot hold
   * it.
   */
  it("reports an unreachable credential store rather than saying no key is stored", async () => {
    const sentence = "Your OS credential store refused access, so the stored key could not be read.";
    renderPanel(KEYED, {
      bridge: { secretStatus: vi.fn(async () => ({ stored: false, problem: sentence })) },
    });
    await waitFor(() =>
      expect(screen.getByTestId("secret-problem-voice-intent")).toHaveTextContent(sentence),
    );
    expect(screen.queryByTestId("secret-state-voice-intent")).not.toBeInTheDocument();
  });

  it("offers Forget only once a key is stored, and asks the store to remove it", async () => {
    const forgetSecret = vi.fn(async () => ({ stored: false }));
    renderPanel(KEYED, {
      bridge: { secretStatus: vi.fn(async () => ({ stored: true })), forgetSecret },
    });
    await waitFor(() => expect(screen.getByTestId("forget-voice-intent")).toBeVisible());
    fireEvent.click(screen.getByTestId("forget-voice-intent"));
    await waitFor(() => expect(forgetSecret).toHaveBeenCalledWith("voice-intent"));
  });

  /*
   * Rendered verbatim, because `saveError` also carries "your settings file
   * cannot be read" (issue #1072) and must not acquire a "saving it failed"
   * preamble it has not earned.
   */
  it("renders a save error as the complete sentence it is", () => {
    renderPanel({}, { saveError: "Settings could not be saved." });
    expect(screen.getByRole("alert")).toHaveTextContent("Settings could not be saved.");
  });
});
