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

function editorOver(daemons: CardActionsDaemon[], preferredDeckId?: string) {
  const runtime = {
    readCardCatalog: vi.fn(async () => ({ actions: [], errors: [], user_config: null })),
    editCardCatalog: vi.fn(),
  };
  const hook = renderHook(({ list, preferred }) => useCardActionsEditor({ runtime, daemons: list, preferredDeckId: preferred }), { initialProps: { list: daemons, preferred: preferredDeckId } });
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

describe("open card actions with a selection on screen (PR #2491, 2026-11-25)", () => {
  /// Scenario: With two daemons showing cards and the selected cards all on the second, say "open card actions" without naming a daemon: the second daemon's editor opens with no question. Naming the first still opens the first. With no preferred daemon, or one not on the dashboard, a bare open asks which daemon as before.
  it("desktop/agent-cards/119: a bare open card actions prefers the selected cards' only daemon", async () => {
    const said: string[] = [];
    const report = { reportRefused: (sentence: string) => { said.push(sentence); } };
    const preferred = editorOver([LOCAL, BUILD], BUILD.deckId);
    await act(async () => { VOICE_ACTIONS.openCardActionsEditor.run({ ...preferred.hook.result.current.controls, ...report }, {}); });
    expect(said).toEqual([]);
    expect(preferred.hook.result.current.view?.daemon.deckId).toBe(BUILD.deckId);
    expect(preferred.runtime.readCardCatalog).toHaveBeenCalledExactlyOnceWith(BUILD.deckId);
    await act(async () => { VOICE_ACTIONS.openCardActionsEditor.run({ ...preferred.hook.result.current.controls, ...report }, { preselectDeckId: LOCAL.deckId }); });
    expect(preferred.hook.result.current.view?.daemon.deckId, "a daemon named wins over the selection").toBe(LOCAL.deckId);
    preferred.hook.unmount();

    const stale = editorOver([LOCAL, BUILD], "deck-moon");
    await act(async () => { VOICE_ACTIONS.openCardActionsEditor.run({ ...stale.hook.result.current.controls, ...report }, {}); });
    expect(said).toEqual([cardActionsWhich([LOCAL.name, BUILD.name])]);
    expect(stale.hook.result.current.view).toBeUndefined();
  });
});

describe("a daemon restarted under the same deck (PR #2491 review, Qodo 2)", () => {
  /// Scenario: The editor is open on a daemon whose read of the card actions is still under way when the daemon restarts as another process under the same deck. The editor reads the list again from the new process, and the old process's late answer never fills it. With the new list on screen and a form open, a save is under way when the daemon restarts once more: the form closes, the list is read again, and the save's late answer changes nothing.
  it("reads again on a new daemon process, dropping the old process's reads and writes", async () => {
    type Catalog = { actions: { name: string }[]; errors: string[]; user_config: string | null };
    const catalog = (name: string): Catalog => ({ actions: [{ name, label: name, description: "", prompt: "p", icon: null, placement: "card", confirm: false, always: false, source: "user", default: false, overrides_default: false, hidden: false } as never], errors: [], user_config: null });
    const reads: ((answer: Catalog) => void)[] = [];
    const writes: ((answer: Catalog) => void)[] = [];
    const runtime = {
      readCardCatalog: vi.fn(() => new Promise<Catalog>((resolve) => { reads.push(resolve); })),
      editCardCatalog: vi.fn(() => new Promise<Catalog>((resolve) => { writes.push(resolve); })),
    };
    const at = (instanceId: string): CardActionsDaemon[] => [{ ...LOCAL, instanceId }];
    const hook = renderHook(({ list }) => useCardActionsEditor({ runtime: runtime as never, daemons: list }), { initialProps: { list: at("process-a") } });
    act(() => { hook.result.current.controls.openCardActionsEditor(LOCAL.deckId); });
    expect(runtime.readCardCatalog).toHaveBeenCalledTimes(1);

    hook.rerender({ list: at("process-b") });
    expect(runtime.readCardCatalog, "the new process's list is read").toHaveBeenCalledTimes(2);
    await act(async () => { reads[0](catalog("from-a")); });
    expect(hook.result.current.view?.catalog, "the old process's answer is dropped").toBeUndefined();
    await act(async () => { reads[1](catalog("from-b")); });
    expect(hook.result.current.view?.catalog?.actions.map((action) => action.name)).toEqual(["from-b"]);

    act(() => { hook.result.current.controls.addCardAction(); });
    act(() => { hook.result.current.setField("name", "ship"); });
    act(() => { hook.result.current.controls.saveCardAction(); });
    expect(runtime.editCardCatalog).toHaveBeenCalledTimes(1);
    hook.rerender({ list: at("process-c") });
    expect(hook.result.current.view?.form, "the form made from the old list closes").toBeUndefined();
    expect(runtime.readCardCatalog).toHaveBeenCalledTimes(3);
    await act(async () => { writes[0](catalog("saved-on-b")); });
    expect(hook.result.current.view?.catalog, "the old process's save answer is dropped").toBeUndefined();
    await act(async () => { reads[2](catalog("from-c")); });
    expect(hook.result.current.view?.catalog?.actions.map((action) => action.name)).toEqual(["from-c"]);

    hook.rerender({ list: at("process-c") });
    expect(runtime.readCardCatalog, "the same process is not read again").toHaveBeenCalledTimes(3);
  });
});
