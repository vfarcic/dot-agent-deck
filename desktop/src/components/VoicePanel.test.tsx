import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { VoicePanel } from "./VoicePanel";
import {
  DEFAULT_DESKTOP_SETTINGS,
  DEFAULT_VOICE_SETTINGS,
  VOICE_ACTIVATION_MODES,
  VOICE_INTENT_BACKENDS,
  VOICE_TRANSCRIPTION_BACKENDS,
  type DesktopSettingsDto,
  type SecretStatusDto,
} from "../lib/bridge";
import { SettingsBridgeProvider, type SettingsBridge } from "../lib/settingsBridge";
import type { RuntimeMode } from "../types";

/** A credential-shaped value, so a leak is searchable rather than plausible. */
const KEY = "sk-not-a-real-key-0123456789";

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

/** A document whose intent backend is the one that needs a key. */
const KEYED = { voice: { ...DEFAULT_VOICE_SETTINGS, intent: "remote" } };

describe("VoicePanel", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("renders every choice the app actually ships an adapter for", () => {
    renderPanel();
    expect(screen.getAllByRole("option", { name: /Off — type instead|Remote service/ })).toHaveLength(
      VOICE_TRANSCRIPTION_BACKENDS.length,
    );
    expect(screen.getByLabelText("Commands")).toHaveValue("claude");
    expect(screen.getByLabelText("Speech")).toHaveValue("off");
    // The lists are the closed sets, so a token the Rust side would fold away
    // cannot be offered here.
    expect(screen.getByLabelText("Commands").querySelectorAll("option")).toHaveLength(
      VOICE_INTENT_BACKENDS.length,
    );
  });

  /**
   * Activation is STATED, not offered (PRD #802 M5). There is one mode, and a
   * `<select>` with one option implies a choice the user does not have — it
   * opens, shows one item, and closing it changes nothing.
   */
  it("states the activation mode rather than offering it as a choice", () => {
    renderPanel();
    const row = screen.getByTestId("voice-activation");
    expect(row).toHaveTextContent("Press to start, press to stop");
    expect(row.tagName).not.toBe("SELECT");
    expect(row.querySelector("select")).toBeNull();
    expect(screen.getByTestId("voice-body").querySelectorAll("select")).toHaveLength(2);
    // Still named, so a screen reader gets the same row structure as the
    // chosen ones.
    expect(row).toHaveAccessibleName("Activation");
    // The list it will render from once D4 adds the other two modes.
    expect(VOICE_ACTIVATION_MODES).toEqual(["toggle"]);
  });

  /**
   * A document written by a newer build can name a mode this one has never
   * heard of. The Rust side folds it to the default on read; this is the same
   * tolerance at the render seam, where a stale in-memory value could still
   * arrive.
   */
  it("states this build's default for an activation mode it does not know", () => {
    renderPanel({
      voice: { ...DEFAULT_VOICE_SETTINGS, activation: "hold-to-talk" },
    });
    expect(screen.getByTestId("voice-activation")).toHaveTextContent(
      "Press to start, press to stop",
    );
  });

  /**
   * An absent `[voice]` section is *unspecified*, not empty — every document on
   * disk today has none — so the panel materialises this build's defaults for
   * display without the document having said anything.
   */
  it("shows this build's defaults for a document with no voice section", () => {
    renderPanel({ voice: undefined });
    expect(screen.getByLabelText("Speech")).toHaveValue(DEFAULT_VOICE_SETTINGS.transcription);
    expect(screen.getByLabelText("Commands")).toHaveValue(DEFAULT_VOICE_SETTINGS.intent);
    expect(screen.getByTestId("voice-typed-only")).toBeVisible();
  });

  /**
   * The panel's only channel for a choice is `onSave`, and it must send the
   * WHOLE document — a panel that sent just its own section would drop every
   * section this build's UI has not loaded, which is the guarantee
   * `settingsContract.ts` states.
   */
  it("saves the whole document with only its own section replaced", () => {
    const { onSave } = renderPanel({ appearance: { mode: "dark" } });
    fireEvent.change(screen.getByLabelText("Commands"), { target: { value: "opencode" } });
    expect(onSave).toHaveBeenCalledWith({
      ...DEFAULT_DESKTOP_SETTINGS,
      appearance: { mode: "dark" },
      voice: { ...DEFAULT_VOICE_SETTINGS, intent: "opencode" },
    });
  });

  it("writes a full section on the first change, so an absent one becomes specified", () => {
    const { onSave } = renderPanel({ voice: undefined });
    fireEvent.change(screen.getByLabelText("Speech"), { target: { value: "remote" } });
    expect(onSave).toHaveBeenCalledWith(
      expect.objectContaining({
        voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      }),
    );
  });

  /** No backend needs a key, so the panel does not ask for one. */
  it("asks for no key while no chosen backend uses one", () => {
    renderPanel();
    expect(screen.queryByLabelText("Commands key")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Speech key")).not.toBeInTheDocument();
  });

  it("asks for a key per backend that authenticates with one", async () => {
    renderPanel({ voice: { activation: "toggle", intent: "remote", transcription: "remote" } });
    expect(screen.getByLabelText("Commands key")).toBeVisible();
    expect(screen.getByLabelText("Speech key")).toBeVisible();
    await waitFor(() => expect(screen.getByTestId("secret-state-voice-intent")).toHaveTextContent("No key stored yet."));
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

    fireEvent.change(screen.getByLabelText("Commands key"), { target: { value: KEY } });
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
    await waitFor(() => expect(screen.getByLabelText("Commands key")).toHaveValue(""));
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
    expect(screen.getByLabelText("Commands key")).toHaveAttribute("type", "password");
    expect(screen.getByLabelText("Commands key")).toHaveValue("");
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

    fireEvent.change(screen.getByLabelText("Commands key"), { target: { value: KEY } });
    fireEvent.click(screen.getByTestId("save-voice-intent"));

    await waitFor(() =>
      expect(screen.getByTestId("secret-problem-voice-intent")).toHaveTextContent(sentence),
    );
    expect(screen.getByLabelText("Commands key")).toHaveValue(KEY);
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
