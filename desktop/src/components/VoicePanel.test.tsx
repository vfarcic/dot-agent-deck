import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { INTENT_DISCLOSURE, INTENT_DISCLOSURE_SHARED, INTENT_DISCLOSURE_WITHHELD, VoicePanel } from "./VoicePanel";
import {
  DEFAULT_DESKTOP_SETTINGS,
  DEFAULT_VOICE_SETTINGS,
  VOICE_ACTIVATION_MODES,
  VOICE_INTENT_BACKENDS,
  VOICE_STAGE_PRESETS,
  VOICE_TRANSCRIPTION_BACKENDS,
  MAX_TOKEN_CEILING,
  MIN_TOKEN_CEILING,
  type DesktopSettingsDto,
  type SecretStatusDto,
} from "../lib/bridge";
import { SettingsBridgeProvider, type SettingsBridge } from "../lib/settingsBridge";
import type { RuntimeMode } from "../types";

/** A credential-shaped value, so a leak is searchable rather than plausible. */
const KEY = "sk-not-a-real-key-0123456789";

/** What each key row is called now that the label names the provider. */
const COMMANDS_KEY = "Commands key for api.anthropic.com";
const SPEECH_KEY = "Speech key for api.openai.com";
/** What the Commands row is called on the defaults, which are OpenAI's. */
const DEFAULT_COMMANDS_KEY = "Commands key for api.openai.com";

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
  voice: { ...DEFAULT_VOICE_SETTINGS, intent: VOICE_STAGE_PRESETS.intent.anthropic },
};

/** A document where BOTH stages need a key. */
const BOTH_KEYED = {
  voice: {
    activation: "toggle",
    intent: VOICE_STAGE_PRESETS.intent.anthropic,
    transcription: VOICE_STAGE_PRESETS.transcription.remote,
    labels: "shared",
  },
};

describe("VoicePanel", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("renders every choice the app actually ships an adapter for", () => {
    renderPanel();
    expect(screen.getByLabelText("Speech")).toHaveValue("local");
    expect(screen.getByLabelText("Commands")).toHaveValue("openai_compatible");
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
    expect(screen.getByRole("option", { name: /OpenAI-compatible API — needs that provider's API key/ })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /On this machine — speech container, no key/ })).toBeInTheDocument();
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
    // Driven from Speech, which is one of two backend rows that would now do:
    // Commands ships two backends since PRD #802's provider work, so changing
    // either fires a save. It shipped one when this row was chosen.
    fireEvent.change(screen.getByLabelText("Speech"), { target: { value: "remote" } });
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
    fireEvent.change(screen.getByLabelText("Speech"), { target: { value: "remote" } });
    expect(onSave).toHaveBeenCalledWith({
      ...DEFAULT_DESKTOP_SETTINGS,
      appearance: { mode: "dark" },
      voice: { ...DEFAULT_VOICE_SETTINGS, transcription: VOICE_STAGE_PRESETS.transcription.remote },
    });
  });

  it("writes a full section on the first change, so an absent one becomes specified", () => {
    const { onSave } = renderPanel({ voice: undefined });
    fireEvent.change(screen.getByLabelText("Speech"), { target: { value: "remote" } });
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({
        voice: {
          activation: "toggle",
          intent: VOICE_STAGE_PRESETS.intent.openai_compatible,
          transcription: VOICE_STAGE_PRESETS.transcription.remote,
          labels: "shared",
        },
      }),
    );
  });

  /**
   * PRD #1223, audit finding A1: the panel says what each command sends to
   * the Commands endpoint, and the Names row decides whether the names on
   * screen are part of it.
   */
  /** Scenario: Says what each command sends, and changes the sentence with the Names row. */
  it("says what each command sends, and changes the sentence with the Names row", () => {
    const { onSave } = renderPanel({ voice: undefined });
    const disclosure = screen.getByTestId("voice-intent-disclosure");
    expect(disclosure).toHaveTextContent(INTENT_DISCLOSURE);
    expect(disclosure).toHaveTextContent(INTENT_DISCLOSURE_SHARED);
    expect(disclosure).toHaveTextContent("SSH user, host and any non-default port");
    expect(disclosure).toHaveTextContent("up to 200 directory names");
    // PRD #1223, closing audit F3: the always-sent components are named, and
    // the narrow fact Shared keeps is stated as exactly that.
    expect(disclosure).toHaveTextContent("every command's id, description, parameter names and kinds");
    expect(disclosure).toHaveTextContent("the hint shown when it cannot");
    expect(disclosure).toHaveTextContent("the model name and token limit");
    expect(disclosure).toHaveTextContent("your Commands API key in its authentication header");
    // PRD #1223, closing audit G2: the negations are about the app-observed
    // names only, and the words spoken are said to be always sent.
    // PRD #1223, closing audit H2: stated as FIELD provenance — the app adds
    // no such field — because a name is arbitrary text and can itself be a
    // path; and the words go with a command that REACHES the endpoint, since
    // the locally decided ones named above send nothing.
    expect(disclosure).toHaveTextContent("This app adds no field of its own for a filesystem path, a daemon or agent id, prompt text or a tool's arguments");
    expect(disclosure).toHaveTextContent("a name is whatever it was set to, so a name can itself be a path.");
    expect(disclosure).toHaveTextContent("Every command that reaches the endpoint also carries your words as heard, which may contain anything you say.");
    expect(disclosure).not.toHaveTextContent("Those names include no filesystem path");
    expect(disclosure).not.toHaveTextContent("always sent as heard");
    expect(disclosure).not.toHaveTextContent("It sends no filesystem path");
    expect(disclosure).not.toHaveTextContent("no prompt you typed");
    expect(disclosure).not.toHaveTextContent("Never a path, an id");
    const names = screen.getByRole("radiogroup", { name: "Names" });
    expect(within(names).getByLabelText("Shared")).toBeChecked();

    fireEvent.click(within(names).getByLabelText("Withheld"));
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({ voice: { ...DEFAULT_VOICE_SETTINGS, labels: "withheld" } }),
    );
  });

  it("says what a withheld Names row costs", () => {
    renderPanel({ voice: { ...DEFAULT_VOICE_SETTINGS, labels: "withheld" } });
    const disclosure = screen.getByTestId("voice-intent-disclosure");
    expect(disclosure).toHaveTextContent(INTENT_DISCLOSURE_WITHHELD);
    expect(disclosure).not.toHaveTextContent(INTENT_DISCLOSURE_SHARED);
    // It withholds the names, not the request: the always-sent part stays.
    expect(disclosure).toHaveTextContent(INTENT_DISCLOSURE);
    expect(disclosure).not.toHaveTextContent("sends nothing else");
    // PRD #1223, closing audit G2: withholding removes the observed names, not
    // the user's own words, and says so rather than implying a redaction.
    expect(disclosure).toHaveTextContent("none of the names this app reads from the screen");
    // Closing audit H2: scoped to the commands that reach the endpoint — the
    // always-sent paragraph beside it names the ones decided on this machine.
    expect(disclosure).toHaveTextContent("It does not redact your words: every command that reaches the endpoint still carries them as heard.");
    expect(disclosure).not.toHaveTextContent("what you speak is still sent as heard");
    expect(disclosure).not.toHaveTextContent("always sent as heard");
    expect(disclosure).not.toHaveTextContent("sends none of the names on screen");
    expect(within(screen.getByRole("radiogroup", { name: "Names" })).getByLabelText("Withheld")).toBeChecked();
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
    // By id rather than by label: both stages carry an Endpoint row now that
    // Commands is API-only, so the label alone names two fields.
    const field = document.getElementById("voice-transcription-endpoint")!;
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
    const model = document.getElementById("voice-transcription-model")!;
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
   * **Every backend on this panel is now an endpoint and a model**, on the
   * defaults as much as on any other choice. This pinned the opposite while
   * Commands defaulted to the agent CLI, which spawned a process on this
   * machine and so had no coordinates to offer; PRD #802's provider work
   * deleted that backend and the `usesEndpoint` predicate with it. What is
   * asserted here is the same property from the other side — that the rows
   * belong to their own stage and carry that stage's value, which is what
   * hiding them made easy to get wrong.
   */
  it("offers endpoint and model for both stages on the defaults", () => {
    renderPanel();
    expect(screen.getAllByLabelText("Endpoint")).toHaveLength(2);
    expect(screen.getAllByLabelText("Model")).toHaveLength(2);
    expect(document.getElementById("voice-transcription-endpoint")).toHaveValue(
      VOICE_STAGE_PRESETS.transcription.local.endpoint,
    );
    expect(document.getElementById("voice-intent-endpoint")).toHaveValue(
      VOICE_STAGE_PRESETS.intent.openai_compatible.endpoint,
    );
  });

  /**
   * The keyless speech backend sends no credential, so its endpoint may only be
   * on this machine (`settings::KEYLESS_OFF_MACHINE`) — and the field says so,
   * rather than letting a user type a hosted URL and meet the refusal as a
   * save error.
   *
   * **The field itself stays**, which is the part worth pinning: a user who
   * publishes the speech container on another port has to be able to say so,
   * and `unreachable_detail` names *their* port in the docker command for
   * exactly that reason. The panel is not the boundary either way — a
   * hand-edited `desktop.toml` never passes through it — so this is the hint
   * telling the truth, not the rule being enforced.
   */
  it("says the keyless speech endpoint may only be on this machine, and still offers the field", () => {
    renderPanel();
    expect(document.getElementById("voice-transcription-endpoint")).toHaveValue(
      VOICE_STAGE_PRESETS.transcription.local.endpoint,
    );
    expect(screen.getByTestId("voice-transcription-endpoint-hint")).toHaveTextContent(
      /this machine only/i,
    );

    // The keyed backend reaches another host by design, so it keeps the
    // general rule.
    renderPanel({ voice: { ...DEFAULT_VOICE_SETTINGS, transcription: VOICE_STAGE_PRESETS.transcription.remote } });
    expect(screen.getAllByTestId("voice-transcription-endpoint-hint")[1]).toHaveTextContent(
      /https to another machine/i,
    );
  });

  it("offers endpoint and model for both stages once both are reached over HTTP", () => {
    renderPanel(BOTH_KEYED);
    expect(screen.getAllByLabelText("Endpoint")).toHaveLength(2);
    expect(screen.getAllByLabelText("Model")).toHaveLength(2);
  });

  /**
   * **One key row on the defaults, not none**, and the asymmetry is the
   * product decision rather than an oversight: Speech's default is a keyless
   * container on loopback, and neither Commands preset is keyless since the
   * agent-CLI one went — a Commands stage goes keyless only by pointing its
   * endpoint at loopback, which `needsKey` reads from the endpoint rather than
   * from the backend token. A panel that asked for two keys before the feature
   * did anything would be the thing PRD #802 set out to avoid; one is what the
   * measurements left.
   *
   * **And that one names OpenAI now**, which is the point of the default
   * moving: the same key the hosted speech backend would want, so a user going
   * hosted end to end opens one account rather than two.
   */
  it("asks for a key only where the chosen backend needs one", () => {
    renderPanel();
    expect(screen.queryByLabelText(SPEECH_KEY)).not.toBeInTheDocument();
    expect(screen.getByLabelText(DEFAULT_COMMANDS_KEY)).toBeVisible();
    expect(screen.getAllByLabelText(/ key for /)).toHaveLength(1);
  });

  /**
   * Scenario: both stages are hosted on the defaults' provider, so both key
   * rows name the same host. Each still says which stage it belongs to.
   *
   * The collision the one-key default creates: `Key for api.openai.com` twice
   * is not a label, and these are two separate keychain entries under two
   * `SecretId`s. A user pasting the same key into both is fine; a user unable
   * to tell which field they are in is not.
   */
  it("tells the two key rows apart when both stages point at one provider", () => {
    renderPanel({
      voice: {
        ...DEFAULT_VOICE_SETTINGS,
        transcription: VOICE_STAGE_PRESETS.transcription.remote,
      },
    });
    expect(screen.getByLabelText(DEFAULT_COMMANDS_KEY)).toBeVisible();
    expect(screen.getByLabelText(SPEECH_KEY)).toBeVisible();
    expect(screen.getAllByLabelText(/ key for api\.openai\.com$/)).toHaveLength(2);
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
        intent: { ...DEFAULT_VOICE_SETTINGS.intent, backend: "openai_compatible", endpoint: "https://gateway.example.com/v1/chat/completions", model: "gpt-4.1-mini" },
      },
    });
    expect(screen.getByLabelText("Commands key for gateway.example.com")).toBeVisible();
    expect(screen.queryByLabelText(COMMANDS_KEY)).not.toBeInTheDocument();
  });

  /**
   * **A loopback command endpoint asks for no key**, because Rust will not read
   * one: `RemoteResolver::run` skips the keychain entirely for a loopback
   * address, so a key row there would collect a credential nothing reads. It is
   * the same rule Speech has from the other side — there the keyless choice is
   * a backend token and the document refuses to pair it with an off-machine
   * endpoint.
   *
   * PRD #802 ships no preset pointing here and recommends it nowhere: local
   * intent was measured twice against the phrase fixtures and was not good
   * enough. The panel telling the truth about a URL the user typed is a
   * different thing from offering it.
   */
  it("asks for no command key when the endpoint is on this machine", () => {
    renderPanel({
      voice: {
        ...DEFAULT_VOICE_SETTINGS,
        intent: { ...DEFAULT_VOICE_SETTINGS.intent, backend: "openai_compatible", endpoint: "http://127.0.0.1:8080/v1/chat/completions", model: "local-model" },
      },
    });
    expect(screen.queryByLabelText(/ key for /)).not.toBeInTheDocument();

    // And an endpoint this build cannot parse still offers the row, rather
    // than hiding it over a URL nobody has agreed about yet.
    renderPanel({
      voice: {
        ...DEFAULT_VOICE_SETTINGS,
        intent: { ...DEFAULT_VOICE_SETTINGS.intent, backend: "openai_compatible", endpoint: "not a url", model: "gpt-4.1-mini" },
      },
    });
    expect(screen.getAllByLabelText(/ key for /)).toHaveLength(1);
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

  /**
   * **Commands only.** A transcription is as long as the audio was, so a
   * ceiling on the Speech stage would bound nothing the user chose — and the
   * Rust `TranscriptionSettings` has no field to receive one, so a row there
   * would write a key that is dropped.
   */
  it("offers the answer ceiling on Commands and on nothing else", () => {
    renderPanel();
    expect(screen.getByLabelText("Max tokens")).toHaveValue(String(DEFAULT_VOICE_SETTINGS.intent.max_tokens));
    expect(screen.getAllByLabelText("Max tokens")).toHaveLength(1);
    // The hint is where the user is told it costs nothing unless it is used,
    // which is the fact that makes a generous default defensible.
    expect(screen.getByTestId("voice-intent-max-tokens-hint"))
      .toHaveTextContent("A ceiling, not a reservation");
  });

  /**
   * Committed on blur and on Enter, like the two text rows, and restored on
   * anything the bounds refuse. Rust would reject an out-of-range value at the
   * document seam and cost the WHOLE `[voice]` section, so declining to send
   * one is cheaper than showing the user that error.
   */
  it("commits a ceiling inside the bounds and restores one outside them", () => {
    const { onSave } = renderPanel();
    const field = screen.getByLabelText("Max tokens");

    fireEvent.change(field, { target: { value: "8192" } });
    fireEvent.blur(field);
    expect(onSave).toHaveBeenCalledTimes(1);
    expect(onSave.mock.calls[0][0].voice.intent.max_tokens).toBe(8192);

    for (const refused of [
      String(MIN_TOKEN_CEILING - 1),
      String(MAX_TOKEN_CEILING + 1),
      "0",
      "-1",
      "4096.5",
      "lots",
      "",
    ]) {
      fireEvent.change(field, { target: { value: refused } });
      fireEvent.blur(field);
      expect(onSave, `${refused} was committed`).toHaveBeenCalledTimes(1);
      expect(field).toHaveValue(String(DEFAULT_VOICE_SETTINGS.intent.max_tokens));
    }

    // Enter commits the same way, and Escape puts the stored value back.
    fireEvent.change(field, { target: { value: String(MAX_TOKEN_CEILING) } });
    fireEvent.keyDown(field, { key: "Enter" });
    expect(onSave.mock.calls[1][0].voice.intent.max_tokens).toBe(MAX_TOKEN_CEILING);
    fireEvent.change(field, { target: { value: "999" } });
    fireEvent.keyDown(field, { key: "Escape" });
    expect(field).toHaveValue(String(DEFAULT_VOICE_SETTINGS.intent.max_tokens));
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
