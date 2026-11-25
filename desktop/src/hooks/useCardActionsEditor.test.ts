import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import { cardActionsWhich, useCardActionsEditor, type CardActionsDaemon } from "./useCardActionsEditor";

/*
 * PR #2491 decision 2026-11-25 — the Card actions editor is bound to one
 * daemon, so "open card actions" needs to know which (`desktop/agent-cards/115`).
 */

const LOCAL: CardActionsDaemon = { deckId: "deck-local", name: "Local daemon", editable: true };
const BUILD: CardActionsDaemon = { deckId: "deck-build-box", name: "deploy@build-box", editable: true };

function editorOver(daemons: CardActionsDaemon[]) {
  const runtime = {
    readCardCatalog: vi.fn(async () => ({ actions: [], errors: [], user_config: null })),
    editCardCatalog: vi.fn(),
  };
  const hook = renderHook(({ list }) => useCardActionsEditor({ runtime, daemons: list }), { initialProps: { list: daemons } });
  return { runtime, hook };
}

describe("opening the Card actions editor by voice (PR #2491 decision 2026-11-25)", () => {
  /// Scenario: Say "open card actions" with one daemon showing cards, then with two, then name one of the two, then name a daemon the dashboard does not show. With one daemon it opens on it; with two and none named nothing opens: the voice panel is handed both daemons to ask "Which daemon?", and where no panel can ask, the sentence says which to name; naming one opens the editor on that daemon; naming one not on the dashboard is refused in words.
  it("desktop/agent-cards/115: open card actions opens the only daemon's editor, and with several asks which", async () => {
    const one = editorOver([LOCAL]);
    const said: string[] = [];
    const report = { reportRefused: (sentence: string) => { said.push(sentence); } };
    await act(async () => { VOICE_ACTIONS.openCardActionsEditor.run({ ...one.hook.result.current.controls, ...report }, {}); });
    expect(said).toEqual([]);
    expect(one.hook.result.current.view?.daemon.deckId).toBe(LOCAL.deckId);
    expect(one.runtime.readCardCatalog).toHaveBeenCalledExactlyOnceWith(LOCAL.deckId);
    one.hook.unmount();

    const two = editorOver([LOCAL, BUILD]);
    await act(async () => { VOICE_ACTIONS.openCardActionsEditor.run({ ...two.hook.result.current.controls, ...report }, {}); });
    expect(said).toEqual([cardActionsWhich([LOCAL.name, BUILD.name])]);
    expect(said[0]).toBe("Several daemons are on the dashboard: say which one, like “open card actions on Local daemon”.");
    expect(two.hook.result.current.view).toBeUndefined();
    expect(two.runtime.readCardCatalog).not.toHaveBeenCalled();

    /* Where the voice panel can offer a choice, the daemons are handed to it to ask "Which daemon?". */
    const askVoiceChoice = vi.fn();
    await act(async () => { VOICE_ACTIONS.openCardActionsEditor.run({ ...two.hook.result.current.controls, ...report, askVoiceChoice }, {}); });
    expect(askVoiceChoice).toHaveBeenCalledExactlyOnceWith({
      param: "deck",
      kind: "deck_ref",
      candidates: [{ value: LOCAL.deckId, label: LOCAL.name }, { value: BUILD.deckId, label: BUILD.name }],
      sentence: cardActionsWhich([LOCAL.name, BUILD.name]),
    });
    expect(said, "asked, not refused twice").toHaveLength(1);
    expect(two.hook.result.current.view).toBeUndefined();

    await act(async () => { VOICE_ACTIONS.openCardActionsEditor.run({ ...two.hook.result.current.controls, ...report }, { preselectDeckId: BUILD.deckId }); });
    expect(said).toHaveLength(1);
    expect(two.hook.result.current.view?.daemon.deckId).toBe(BUILD.deckId);
    expect(two.runtime.readCardCatalog).toHaveBeenCalledExactlyOnceWith(BUILD.deckId);

    await act(async () => { VOICE_ACTIONS.openCardActionsEditor.run({ ...two.hook.result.current.controls, ...report }, { preselectDeckId: "deck-moon" }); });
    expect(said[1]).toBe("That daemon is not on the dashboard, or shows no cards, so its card actions cannot be shown.");
    expect(two.hook.result.current.view?.daemon.deckId, "the editor stays where it was").toBe(BUILD.deckId);

    expect(two.hook.result.current.offeredFor(BUILD.deckId)).toBe(true);
    expect(two.hook.result.current.offeredFor("deck-moon")).toBe(false);
  });
});
