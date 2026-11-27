import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import type { CardCatalogEdit, EditableCardAction, EditableCardCatalog } from "../types";
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
    expect(one.runtime.readCardCatalog).toHaveBeenCalledExactlyOnceWith(LOCAL.deckId, undefined);
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
    expect(two.runtime.readCardCatalog).toHaveBeenCalledExactlyOnceWith(BUILD.deckId, undefined);

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
    expect(preferred.runtime.readCardCatalog).toHaveBeenCalledExactlyOnceWith(BUILD.deckId, undefined);
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

describe("a late answer batched with a daemon restart (PR #2491 audit, A2)", () => {
  const action = (label: string): EditableCardAction => ({ name: "ship", label, description: "", prompt: "Ship it.", icon: null, placement: "card", confirm: false, always: false, source: "user", default: false, overridden: false, hidden: false });
  const catalogOf = (label: string): EditableCardCatalog => ({ actions: [action(label)], errors: [], user_config: null });
  const at = (instanceId: string): CardActionsDaemon[] => [{ ...LOCAL, instanceId }];

  /* The editor opened on process `process-a`, its read under way, a spoken request waiting for it; then `process-a`'s answer and the adoption of `process-b` land in one batch. */
  async function batchedRestart(asked: (controls: ReturnType<typeof useCardActionsEditor>["controls"]) => unknown) {
    const reads: ((answer: EditableCardCatalog) => void)[] = [];
    const runtime = {
      readCardCatalog: vi.fn((_deckId: string, _instanceId?: string) => new Promise<EditableCardCatalog>((resolve) => { reads.push(resolve); })),
      editCardCatalog: vi.fn(async (_deckId: string, _edit: CardCatalogEdit, _instanceId?: string) => catalogOf("Deploy production")),
    };
    const hook = renderHook(({ list }) => useCardActionsEditor({ runtime, daemons: list }), { initialProps: { list: at("process-a") } });
    act(() => { hook.result.current.controls.openCardActionsEditor(LOCAL.deckId); });
    let spoken: unknown;
    act(() => { spoken = asked(hook.result.current.controls); });
    let settled: { said: string | undefined } | undefined;
    void Promise.resolve(spoken).then((said) => { settled = { said: said as string | undefined }; });
    await act(async () => {
      reads[0](catalogOf("Ship staging"));
      await Promise.resolve();
      hook.rerender({ list: at("process-b") });
    });
    return { hook, runtime, reads, settled: () => settled };
  }

  /// Scenario: With the Card actions editor open on a daemon whose list is still being read, say "remove Ship staging"; the old process's list ("Ship staging") arrives in the same moment the daemon restarts as a new process. No removal is asked from the old list under the new process; once the new process's list arrives ("Deploy production"), the confirmation names what it lists, and confirming sends the new process's action as the base, bound to the new process.
  it("desktop/agent-cards/129: a pending spoken removal never confirms the old process's action under the new one", async () => {
    const { hook, runtime, reads, settled } = await batchedRestart((controls) => controls.removeCardAction({ name: "ship" }));
    expect(hook.result.current.view?.removing, "no confirmation made from the old process's list").toBeUndefined();
    expect(settled(), "the spoken request waits for the new process's list").toBeUndefined();
    expect(runtime.readCardCatalog).toHaveBeenLastCalledWith(LOCAL.deckId, "process-b");

    await act(async () => { reads[1](catalogOf("Deploy production")); });
    expect(hook.result.current.view?.removing?.label).toBe("Deploy production");
    expect(settled()).toEqual({ said: undefined });
    await act(async () => { hook.result.current.controls.confirmRemoveCardAction(); });
    expect(runtime.editCardCatalog).toHaveBeenCalledExactlyOnceWith(LOCAL.deckId, { kind: "remove", name: "ship", base: action("Deploy production") }, "process-b");
  });

  /// Scenario: The same as the removal, with "edit Ship staging" said instead. The old process's form never opens under the new process; once the new process's list arrives, the form holds its action, and Save sends that action as the base, bound to the new process.
  it("desktop/agent-cards/130: a pending spoken edit never opens the old process's form under the new one", async () => {
    const { hook, runtime, reads, settled } = await batchedRestart((controls) => controls.editCardAction({ name: "ship" }));
    expect(hook.result.current.view?.form, "no form made from the old process's list").toBeUndefined();
    expect(settled()).toBeUndefined();

    await act(async () => { reads[1](catalogOf("Deploy production")); });
    expect(hook.result.current.view?.form?.values.label).toBe("Deploy production");
    expect(settled()).toEqual({ said: undefined });
    await act(async () => { hook.result.current.controls.saveCardAction(); });
    expect(runtime.editCardCatalog).toHaveBeenCalledOnce();
    const [deckId, edit, instanceId] = runtime.editCardCatalog.mock.calls[0];
    expect([deckId, instanceId]).toEqual([LOCAL.deckId, "process-b"]);
    expect(edit).toMatchObject({ kind: "update", base: action("Deploy production") });
  });

  /// Scenario: A Save and a confirmed removal made from one daemon process's list are each sent bound to that process. Then an edit form is open when the daemon restarts as another process, and Save is pressed; once the new process's list is read, a removal confirmation is open when the daemon restarts once more, and Confirm Remove is pressed. Neither press sends anything, so no edit made from an old list reaches the new process.
  it("desktop/agent-cards/131: a save or a removal is bound to the process whose list the user saw", async () => {
    const reads: ((answer: EditableCardCatalog) => void)[] = [];
    const runtime = {
      readCardCatalog: vi.fn((_deckId: string, _instanceId?: string) => new Promise<EditableCardCatalog>((resolve) => { reads.push(resolve); })),
      editCardCatalog: vi.fn(async (_deckId: string, _edit: CardCatalogEdit, _instanceId?: string) => catalogOf("Ship staging")),
    };
    const hook = renderHook(({ list }) => useCardActionsEditor({ runtime, daemons: list }), { initialProps: { list: at("process-a") } });
    act(() => { hook.result.current.controls.openCardActionsEditor(LOCAL.deckId); });
    await act(async () => { reads[0](catalogOf("Ship staging")); });
    expect(runtime.readCardCatalog).toHaveBeenLastCalledWith(LOCAL.deckId, "process-a");
    act(() => { hook.result.current.controls.editCardAction({ name: "ship" }); });
    await act(async () => { hook.result.current.controls.saveCardAction(); });
    expect(runtime.editCardCatalog).toHaveBeenLastCalledWith(LOCAL.deckId, expect.objectContaining({ kind: "update", base: action("Ship staging") }), "process-a");
    act(() => { hook.result.current.controls.removeCardAction({ name: "ship" }); });
    await act(async () => { hook.result.current.controls.confirmRemoveCardAction(); });
    expect(runtime.editCardCatalog).toHaveBeenLastCalledWith(LOCAL.deckId, { kind: "remove", name: "ship", base: action("Ship staging") }, "process-a");

    /* A form from `process-a`'s list is open when the daemon restarts: Save is pressed and sends nothing. */
    act(() => { hook.result.current.controls.editCardAction({ name: "ship" }); });
    expect(hook.result.current.view?.form?.values.label).toBe("Ship staging");
    hook.rerender({ list: at("process-b") });
    expect(hook.result.current.view?.form).toBeUndefined();
    expect(hook.result.current.view?.catalog).toBeUndefined();
    let saved: string | undefined;
    await act(async () => { saved = hook.result.current.controls.saveCardAction(); });
    expect(saved, "Save answers in words that no form is open").toBe("No card action form is open.");
    expect(runtime.editCardCatalog, "Save sent nothing after the restart").toHaveBeenCalledTimes(2);

    /* A removal confirmation from `process-b`'s list is open when the daemon restarts again: Confirm Remove sends nothing. */
    await act(async () => { reads[1](catalogOf("Deploy production")); });
    expect(runtime.readCardCatalog).toHaveBeenLastCalledWith(LOCAL.deckId, "process-b");
    act(() => { hook.result.current.controls.removeCardAction({ name: "ship" }); });
    expect(hook.result.current.view?.removing?.label).toBe("Deploy production");
    hook.rerender({ list: at("process-c") });
    expect(hook.result.current.view?.removing).toBeUndefined();
    let confirmed: string | undefined;
    await act(async () => { confirmed = hook.result.current.controls.confirmRemoveCardAction(); });
    expect(confirmed, "Confirm Remove answers in words that nothing is being removed").toBe("The editor is not asking to remove anything.");
    expect(runtime.editCardCatalog, "Confirm Remove sent nothing after the restart").toHaveBeenCalledTimes(2);
    expect(runtime.editCardCatalog.mock.calls.map(([, , instanceId]) => instanceId)).toEqual(["process-a", "process-a"]);
  });
});
