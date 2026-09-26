import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { EndpointsPanel } from "./EndpointsPanel";

/**
 * The shared write gate, spied but NOT replaced (PRD 742 M8's F3).
 *
 * The panel's data-safety property is "every write goes through
 * `endpointSectionToSave`", and that property is not observable in the panel's
 * output: the gate returns the section unchanged in every case that reaches it
 * from here today, so a write that bypasses it and a write that goes through it
 * produce the same `onSave`. That is precisely why the bypass survived — three
 * comments asserted the coverage and nothing checked it.
 *
 * So the assertion is on the call, and the implementation stays real: the mock
 * delegates to the original, every other test in this file is unaffected, and
 * the behavioural tests above and below still do the behavioural work.
 */
const writeGate = vi.hoisted(() => vi.fn());
vi.mock("../lib/endpoints", async () => {
  const actual = await vi.importActual<typeof import("../lib/endpoints")>("../lib/endpoints");
  writeGate.mockImplementation(actual.endpointSectionToSave);
  // Lazy getters rather than a spread. `endpoints.ts` and `bridge.ts` import
  // each other, so several of this module's exports are re-exports of the
  // other, and reading them eagerly inside a mock factory takes them before the
  // cycle has settled — measured: a spread here left
  // `ALL_ENDPOINT_SELECTION` `undefined` for the panel while the module's own
  // internal reference to it was fine. A getter defers the read to the moment
  // the panel actually uses the value, which is after both modules exist.
  return Object.defineProperties(
    {},
    Object.fromEntries(
      Object.keys(actual).map((name) => [
        name,
        {
          enumerable: true,
          get: () =>
            name === "endpointSectionToSave"
              ? writeGate
              : (actual as unknown as Record<string, unknown>)[name],
        },
      ]),
    ),
  );
});
import {
  DEFAULT_DESKTOP_SETTINGS,
  type DesktopSettingsDto,
  type EndpointTestReportDto,
  type RemoteEndpointDto,
} from "../lib/bridge";
import { SettingsBridgeProvider } from "../lib/settingsBridge";
import type { RuntimeMode } from "../types";

function deck(overrides: Partial<RemoteEndpointDto> = {}): RemoteEndpointDto {
  return { host: "build-box", id: "deck0000000000aa", port: 22, ...overrides };
}

function report(overrides: Partial<EndpointTestReportDto> = {}): EndpointTestReportDto {
  return {
    endpointId: "deck0000000000aa",
    deck: "build-box",
    state: "reachable",
    ok: true,
    message: "build-box answered and is compatible with this app.",
    disclosureKnown: true,
    forwards: [],
    knownHosts: [],
    clientProtocolVersion: 9,
    clientBuildVersion: "0.39.0-gabc1234",
    ...overrides,
  };
}

function renderPanel(
  overrides: Partial<DesktopSettingsDto> = {},
  options: {
    mode?: RuntimeMode;
    saveError?: string;
    testEndpoint?: (settings: DesktopSettingsDto, selection: string) => Promise<EndpointTestReportDto>;
  } = {},
) {
  const onSave = vi.fn();
  const settings = { ...DEFAULT_DESKTOP_SETTINGS, ...overrides };
  const panel = (
    <EndpointsPanel
      settings={settings}
      onSave={onSave}
      saveError={options.saveError}
      mode={options.mode ?? "live"}
    />
  );
  const wrap = (element: React.ReactElement) =>
    options.testEndpoint
      ? (
        <SettingsBridgeProvider
          value={{
            testEndpoint: options.testEndpoint,
            // PRD #802 M4 widened `SettingsBridge` with three credential
            // actions. This panel reaches none of them; they are here because
            // the context is one value and a partial one would not type-check.
            secretStatus: vi.fn(async () => ({ stored: false })),
            storeSecret: vi.fn(async () => ({ stored: true })),
            forgetSecret: vi.fn(async () => ({ stored: false })),
          }}
        >
          {element}
        </SettingsBridgeProvider>
      )
      : element;
  const { rerender } = render(wrap(panel));
  /**
   * Hand the panel a NEWER document, the way the app does after a save — the
   * harness holds `settings` fixed otherwise, so nothing else here can model an
   * edit that lands while an async probe is in flight.
   */
  const update = (next: DesktopSettingsDto) =>
    rerender(
      wrap(
        <EndpointsPanel
          settings={next}
          onSave={onSave}
          saveError={options.saveError}
          mode={options.mode ?? "live"}
        />,
      ),
    );
  return { onSave, settings, update };
}

describe("EndpointsPanel", () => {
  // Braced, not an expression body: `mockClear()` returns the mock for
  // chaining, and a value returned from `beforeEach` is taken by vitest as a
  // cleanup function and CALLED after the test — with no arguments.
  beforeEach(() => {
    writeGate.mockClear();
  });

  /**
   * Scenario: open the Daemons section on a fresh install — a document with no
   * `[endpoints]` section at all. The local deck is listed and chosen, and
   * nothing had to be configured for that to be true.
   */
  it("shows the local deck as present and chosen without any configuration", () => {
    renderPanel();
    const local = screen.getByTestId("deck-choice-local");
    expect(local).toHaveTextContent("This machine");
    expect(local.querySelector("input")).toBeChecked();
    // Not removable: it is not a stored row, it is what `Endpoint::local()`
    // resolves, so there is nothing to delete.
    expect(screen.queryByTestId("remove-deck-local")).toBeNull();
  });

  /**
   * Scenario: press "Add a daemon". A row appears, focused, with its fields ready
   * to fill in — and **nothing is saved yet**, because a daemon with no host is
   * not a document Rust will accept (PRD #741, Greptile P2 on #1035). Writing
   * it immediately meant the row lived only in this component's optimistic
   * state and that every unrelated settings save failed while it sat there.
   */
  it("adds a daemon as a local draft, saving nothing until it is valid", () => {
    const { onSave } = renderPanel({}, { testEndpoint: async () => report() });
    fireEvent.click(screen.getByTestId("add-deck"));

    expect(onSave).not.toHaveBeenCalled();
    // Present, focused, and named for what it is until it has a host.
    expect(screen.getByTestId("deck-detail")).toBeInTheDocument();
    expect(screen.getByLabelText("Host")).toHaveValue("");
    expect(screen.getByText("New daemon")).toBeInTheDocument();
    // And it cannot be probed while it is unusable.
    expect(screen.getByTestId("test-connection")).toBeDisabled();
  });

  /**
   * Scenario: fill the draft's Host in. The moment the row is storable it stops
   * being a draft — it goes into the document with a freshly minted id and
   * becomes the selection, which is what pressing "Add a daemon" was asking for.
   */
  it("stores a draft and selects it as soon as it is valid", () => {
    const { onSave } = renderPanel();
    fireEvent.click(screen.getByTestId("add-deck"));
    fireEvent.change(screen.getByLabelText("Host"), { target: { value: "build-box" } });

    const saved = onSave.mock.calls[0][0] as DesktopSettingsDto;
    expect(saved.endpoints?.remote).toHaveLength(1);
    const added = saved.endpoints!.remote[0];
    expect(added.id).toMatch(/^[0-9a-f]{16}$/);
    expect(added.host).toBe("build-box");
    expect(added.port).toBe(22);
    expect(saved.endpoints?.selection).toBe(added.id);
    // The whole document travels, not just this section.
    expect(saved.appearance).toEqual(DEFAULT_DESKTOP_SETTINGS.appearance);
    expect(saved.zoom).toEqual(DEFAULT_DESKTOP_SETTINGS.zoom);
  });

  /**
   * Scenario: press "Add a daemon" and then remove the row again without typing
   * anything. Nothing was ever written, so nothing has to be unwritten.
   */
  it("drops an abandoned draft without touching the document", () => {
    const { onSave } = renderPanel();
    fireEvent.click(screen.getByTestId("add-deck"));
    const remove = screen.getByLabelText("Remove New daemon");
    fireEvent.click(remove);

    expect(onSave).not.toHaveBeenCalled();
    expect(screen.queryByTestId("deck-detail")).toBeNull();
  });

  /*
    -------------------------------------------------------------------------
    PRD 742 M6 — the client-side counterpart of Rust's
    `a_client_that_cannot_render_endpoints_cannot_delete_them`.

    That test pins the protection a client which CANNOT render decks gets from
    `DesktopSettings::endpoints` being an `Option`. These pin the other half:
    this panel CAN render decks, it reads an absent section through a
    `{ remote: [], selection: "local" }` stand-in, and `remote: []` is the
    assertion "this user has no decks" — which `merged_document` writes over
    whatever `[[endpoints.remote]]` rows are on disk. The webview is handed an
    absent-looking section not only when there genuinely is none, but also when
    `desktop.toml` failed to parse and `load_from` fell back to defaults with
    every row still in the file. Nobody reproduces that by hand.
    -------------------------------------------------------------------------
  */

  /**
   * Scenario: a document with no `[endpoints]` section. Press "Add a daemon",
   * then abandon the draft by clicking back onto **This machine** — the daemon
   * that was already in force. Nothing about the document changed, so nothing
   * may be written: the section this panel would write is a stand-in it
   * invented, and it would delete rows it was never shown.
   */
  it("a client that CAN render endpoints does not delete them by re-choosing the daemon already in force", () => {
    const { onSave } = renderPanel();
    fireEvent.click(screen.getByTestId("add-deck"));
    expect(screen.getByTestId("deck-detail")).toBeInTheDocument();

    fireEvent.click(screen.getByTestId("deck-choice-local").querySelector("input")!);

    // The draft is abandoned, which is what the click was for...
    expect(screen.queryByTestId("deck-detail")).toBeNull();
    // ...and the document is untouched, which is the property.
    expect(onSave).not.toHaveBeenCalled();
  });

  /**
   * Scenario: the same click against a document that DOES declare a section,
   * with a daemon stored and selected. Re-choosing it is still a no-op, so the
   * rows are never rewritten — the guard is about the change being empty, not
   * about the section being absent.
   */
  it("does not rewrite a declared section for a selection that has not moved", () => {
    const row = deck();
    const { onSave } = renderPanel({ endpoints: { remote: [row], selection: row.id } });
    fireEvent.click(screen.getByTestId("add-deck"));

    fireEvent.click(screen.getByTestId(`deck-choice-${row.id}`).querySelector("input")!);

    // The draft is gone and the stored deck's own fields are back on screen.
    expect(screen.getByLabelText("Host")).toHaveValue(row.host);
    expect(onSave).not.toHaveBeenCalled();
  });

  /*
    -------------------------------------------------------------------------
    PRD 742 M6 — a stored fleet selection.

    M1 added **All daemons** to `deckChoices`, which the top bar's Deck selector
    is built from. This panel had a chooser of its own, so with `all` stored its
    `shown` matched no row: no radio checked, no detail form, and a **Test
    connection** button still enabled for a token no probe can answer.
    -------------------------------------------------------------------------
  */

  /**
   * Scenario: the document stores the fleet selection. The chooser shows **All
   * Daemons** checked and says why there are no fields under it, rather than
   * rendering a chooser with nothing chosen.
   */
  it("shows a stored fleet selection as chosen, with a reason there are no fields", () => {
    renderPanel({ endpoints: { remote: [deck()], selection: "all" } });

    expect(screen.getByTestId("deck-choice-all").querySelector("input")).toBeChecked();
    expect(screen.getByTestId("deck-choice-local").querySelector("input")).not.toBeChecked();
    expect(screen.getByTestId("deck-choice-deck0000000000aa").querySelector("input")).not.toBeChecked();
    // No fields, and a sentence saying that is correct rather than missing.
    expect(screen.queryByTestId("deck-detail")).toBeNull();
    expect(screen.getByTestId("deck-fleet-note")).toHaveTextContent("All daemons is every daemon at once");
    // The fleet is not a row, so there is nothing to remove.
    expect(screen.queryByTestId("remove-deck-all")).toBeNull();
  });

  /**
   * Scenario: press **Test connection** while the fleet is selected. You
   * cannot: a probe tests one deck, and the fleet token names a set. Before
   * this the button was live and reached `endpoint_test::unsealed`, which
   * answered "That deck is no longer in this settings document" — safe, and the
   * wrong sentence for a selection that is in force.
   */
  it("cannot probe under a fleet selection", () => {
    const testEndpoint = vi.fn(async () => report());
    renderPanel({ endpoints: { remote: [deck()], selection: "all" } }, { testEndpoint });

    expect(screen.getByTestId("test-connection")).toBeDisabled();
    fireEvent.click(screen.getByTestId("test-connection"));
    expect(testEndpoint).not.toHaveBeenCalled();
  });

  /**
   * Scenario: choose **All daemons** from this panel on a document that already
   * declares a section. It is a real change, so it is written — and the stored
   * rows travel with it rather than being replaced by the chooser's own idea of
   * the list.
   */
  it("stores the fleet selection without disturbing the rows", () => {
    const row = deck();
    const { onSave } = renderPanel({ endpoints: { remote: [row], selection: row.id } });

    fireEvent.click(screen.getByTestId("deck-choice-all").querySelector("input")!);

    const saved = onSave.mock.calls[0][0] as DesktopSettingsDto;
    expect(saved.endpoints?.selection).toBe("all");
    expect(saved.endpoints?.remote).toEqual([row]);
  });

  /**
   * Scenario: two decks are configured and the second is selected; click the
   * first. The selection moves and both rows survive.
   */
  it("selects a stored deck without disturbing the others", () => {
    const first = deck({ id: "deck0000000000aa", host: "build-box" });
    const second = deck({ id: "deck0000000000bb", host: "ci-box" });
    const { onSave } = renderPanel({
      endpoints: { remote: [first, second], selection: second.id },
    });

    expect(screen.getByTestId("deck-choice-deck0000000000bb").querySelector("input")).toBeChecked();
    fireEvent.click(screen.getByTestId("deck-choice-deck0000000000aa").querySelector("input")!);

    const saved = onSave.mock.calls[0][0] as DesktopSettingsDto;
    expect(saved.endpoints?.selection).toBe(first.id);
    expect(saved.endpoints?.remote.map((row) => row.id)).toEqual([first.id, second.id]);
  });

  /**
   * Scenario: remove the daemon currently in use. The row goes and the selection
   * falls back to the local deck — the app always has a daemon, and picking a
   * different remote one on the user's behalf is a decision they did not make.
   */
  it("removes a daemon and falls back to local when it was the one in use", () => {
    const row = deck();
    const { onSave } = renderPanel({ endpoints: { remote: [row], selection: row.id } });

    fireEvent.click(screen.getByTestId(`remove-deck-${row.id}`));

    const saved = onSave.mock.calls[0][0] as DesktopSettingsDto;
    expect(saved.endpoints?.remote).toEqual([]);
    expect(saved.endpoints?.selection).toBe("local");
  });

  /**
   * Scenario: a daemon is named by its address, because there is no display name
   * to give it — a user-chosen label would be exactly the arbitrary `String`
   * the settings field-type guard refuses.
   */
  it("labels a daemon from its address, port included when it is not 22", () => {
    renderPanel({
      endpoints: {
        remote: [deck({ user: "deploy", port: 2222 })],
        selection: "local",
      },
    });
    expect(screen.getByTestId("deck-choice-deck0000000000aa")).toHaveTextContent("deploy@build-box:2222");
  });

  /**
   * Scenario: a settings document supplies a host carrying a right-to-left
   * override. The label renders without it, so the character cannot reorder the
   * text around the daemon's name.
   */
  it("strips a bidi override from a daemon label", () => {
    renderPanel({
      endpoints: { remote: [deck({ host: "build‮box" })], selection: "local" },
    });
    const label = screen.getByTestId("deck-choice-deck0000000000aa").textContent ?? "";
    expect(label).toContain("buildbox");
    expect(label).not.toContain("‮");
  });

  /**
   * Scenario: edit the chosen deck's host. Only that row changes, and the rest
   * of the section — the other row, the selection — is carried through.
   */
  it("edits only the chosen deck", () => {
    const first = deck({ id: "deck0000000000aa" });
    const second = deck({ id: "deck0000000000bb", host: "ci-box" });
    const { onSave } = renderPanel({
      endpoints: { remote: [first, second], selection: first.id },
    });

    fireEvent.change(screen.getByLabelText("Host"), { target: { value: "new-box" } });

    const saved = onSave.mock.calls[0][0] as DesktopSettingsDto;
    expect(saved.endpoints?.remote[0].host).toBe("new-box");
    expect(saved.endpoints?.remote[1]).toEqual(second);
    expect(saved.endpoints?.selection).toBe(first.id);
  });

  /**
   * Scenario: type a host ssh could not be handed safely. The panel says so
   * where the user is typing rather than letting the save come back with a
   * serde message it cannot explain, and Test connection is unavailable while
   * the row is unusable.
   */
  it("names what is wrong with a field and refuses to test an unusable row", () => {
    const testEndpoint = vi.fn(async () => report());
    renderPanel(
      { endpoints: { remote: [deck({ host: "build box" })], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    expect(screen.getByLabelText("Host")).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByTestId("test-connection")).toBeDisabled();
  });

  /**
   * Scenario: press Test connection against a daemon nothing answers on. The
   * panel reports the state and the reason **in place** — the section it is
   * in is still there, the daemon list is still there, and the fields are still
   * editable. A screen that blanked would be the one thing a user cannot
   * recover from without restarting the app.
   */
  it("shows an unreachable deck's state without blanking the panel", async () => {
    const testEndpoint = vi.fn(async () =>
      report({
        state: "host_key_unverified",
        ok: false,
        message: "This machine has not verified build-box's host key.",
        remedy: "ssh -J bastion -p 2222 deploy@build-box",
        detail: "Host key verification failed.",
      }),
    );
    renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));

    await waitFor(() => expect(screen.getByTestId("deck-result")).toBeInTheDocument());
    expect(screen.getByTestId("deck-result")).toHaveAttribute("data-state", "host_key_unverified");
    expect(screen.getByTestId("deck-result-message")).toHaveTextContent("has not verified");
    expect(screen.getByTestId("deck-result-remedy")).toHaveTextContent("ssh -J bastion -p 2222 deploy@build-box");
    // Still a panel, not a replacement screen.
    expect(screen.getByTestId("deck-choices")).toBeInTheDocument();
    expect(screen.getByLabelText("Host")).toHaveValue("build-box");
  });

  /**
   * Scenario: a probe discovers the remote deck's socket path. The panel writes
   * it into the row, so the next connection needs no probe — the write-back M6
   * left to M10, made on this side because `useDesktopSettings` already
   * serialises the document's read-modify-write and a second writer in Rust
   * would be a race created on purpose.
   */
  it("writes a discovered socket path back into the row", async () => {
    const testEndpoint = vi.fn(async () =>
      report({ discoveredSocket: "/run/user/1000/dot-agent-deck-attach.sock" }),
    );
    const { onSave } = renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));

    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const saved = onSave.mock.calls[0][0] as DesktopSettingsDto;
    expect(saved.endpoints?.remote[0].socket).toBe("/run/user/1000/dot-agent-deck-attach.sock");
  });

  /**
   * Scenario: a probe is running — ssh takes seconds — and the user edits
   * another field of the same deck while it runs. When the probe's socket
   * write-back lands it must build on the document as it is NOW, not on the one
   * the click closed over (PRD #741, Greptile P1 on #1035): spreading the
   * captured copy silently reverted every edit made during the probe.
   */
  it("writes a discovered socket back onto the document as it is when the probe lands", async () => {
    let settle: (value: EndpointTestReportDto) => void = () => {};
    const testEndpoint = vi.fn(
      () => new Promise<EndpointTestReportDto>((resolve) => { settle = resolve; }),
    );
    const { onSave, settings, update } = renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));
    await waitFor(() => expect(testEndpoint).toHaveBeenCalled());
    // The user types a port while ssh is still out there.
    const edited: DesktopSettingsDto = {
      ...settings,
      endpoints: { remote: [deck({ port: 2222 })], selection: "deck0000000000aa" },
    };
    update(edited);
    settle(report({ discoveredSocket: "/run/user/1000/dot-agent-deck-attach.sock" }));

    await waitFor(() => expect(onSave).toHaveBeenCalled());
    const saved = onSave.mock.calls[0][0] as DesktopSettingsDto;
    expect(saved.endpoints?.remote[0].socket).toBe("/run/user/1000/dot-agent-deck-attach.sock");
    expect(saved.endpoints?.remote[0].port).toBe(2222);
  });

  /**
   * Scenario: the user removes the daemon while its probe is still running. The
   * write-back must not resurrect it — a daemon that is no longer in the document
   * has no socket to learn (PRD #741, Greptile P1 on #1035).
   */
  it("does not resurrect a daemon removed while its probe was running", async () => {
    let settle: (value: EndpointTestReportDto) => void = () => {};
    const testEndpoint = vi.fn(
      () => new Promise<EndpointTestReportDto>((resolve) => { settle = resolve; }),
    );
    const { onSave, settings, update } = renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));
    await waitFor(() => expect(testEndpoint).toHaveBeenCalled());
    update({ ...settings, endpoints: { remote: [], selection: "local" } });
    settle(report({ discoveredSocket: "/run/user/1000/dot-agent-deck-attach.sock" }));

    await waitFor(() => expect(testEndpoint).toHaveBeenCalled());
    expect(onSave).not.toHaveBeenCalled();
  });

  /**
   * **PRD 742 M8's F3.** Scenario: a probe discovers a socket and the panel
   * writes it back. That write must go through the shared gate, like every
   * other write this panel makes.
   *
   * It did not. `runTest`'s write-back called `onSave` directly — because it
   * writes against `latest.current` (the document as of the newest render)
   * rather than the one its handler closed over, and `saveSection` closed over
   * `settings` — while three separate comments said every write in this panel
   * goes through `endpointSectionToSave`: the doc on `saveSection`, the doc on
   * the gate itself (which *named* this write-back among the sites it covers),
   * and the M6 section of `docs/develop/desktop-gui.md`.
   *
   * **What this proves:** that the claim is now true, at the one site that
   * falsified it.
   *
   * **What it does NOT prove, and this is why the test is shaped like this:**
   * that the gate changes the outcome here. It does not, and cannot — the gate
   * returns the section unchanged in every case that reaches it from this site,
   * because the `row && !row.socket` find-guard above the write only fires when
   * the row genuinely gains a socket it did not have. There is therefore **no
   * input that distinguishes the two implementations**, which is exactly how
   * three comments came to assert a coverage nothing checked. So the assertion
   * is on the call rather than on the output, and the implementation stays real
   * — the tests around this one do the behavioural work and are unaffected.
   *
   * What the routing buys is future-tense and structural: on an unreadable
   * `desktop.toml` the webview is handed a fabricated `{ remote: [] }`, and it
   * is now the gate rather than this one find-guard that refuses to merge that
   * over the rows still on disk. A probe write-back that filled in a port, a
   * user or a jump host — or one that upserted a row instead of patching one —
   * is covered without anyone having to notice that it needs to be.
   */
  it("routes the discovered-socket write-back through the shared write gate", async () => {
    const testEndpoint = vi.fn(async () =>
      report({ discoveredSocket: "/run/user/1000/dot-agent-deck-attach.sock" }),
    );
    const { onSave } = renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));
    await waitFor(() => expect(onSave).toHaveBeenCalled());

    expect(writeGate).toHaveBeenCalledTimes(1);
    const [received, next] = writeGate.mock.calls[0];
    // The document as of the newest render, not the one the click closed over —
    // both halves of the property in one call.
    expect(received).toEqual({ remote: [deck()], selection: "deck0000000000aa" });
    expect(next?.remote[0].socket).toBe("/run/user/1000/dot-agent-deck-attach.sock");
    // And the gate let it through, because it is a genuine change.
    expect(writeGate.mock.results[0].value).toBe(next);
  });

  /**
   * Scenario: the same probe against a row whose socket path the user typed.
   * Discovery does not overwrite it — a probe answers a question the user has
   * already answered, and silently replacing their value is the one thing a
   * write-back must not do.
   */
  it("never overwrites a socket path the user typed", async () => {
    const testEndpoint = vi.fn(async () =>
      report({ discoveredSocket: "/run/user/1000/dot-agent-deck-attach.sock" }),
    );
    const { onSave } = renderPanel(
      {
        endpoints: {
          remote: [deck({ socket: "/tmp/mine.sock" })],
          selection: "deck0000000000aa",
        },
      },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));

    await waitFor(() => expect(testEndpoint).toHaveBeenCalled());
    expect(onSave).not.toHaveBeenCalled();
  });

  /**
   * Scenario: the probe resolved the user's ssh config and found forwards this
   * deck's tunnel will inherit. The panel lists them — the one place a user can
   * find out what their own ssh config is doing on their behalf, since the
   * tunnel carries them for its whole life and nothing else in the app says so.
   */
  it("discloses the forwards the tunnel will inherit", async () => {
    const testEndpoint = vi.fn(async () =>
      report({
        disclosureKnown: true,
        forwards: ["SOCKS proxy on this machine at 1080", "remote 9999 forwarded to localhost:22"],
      }),
    );
    renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));

    await waitFor(() => expect(screen.getByTestId("deck-result-forwards")).toBeInTheDocument());
    expect(screen.getByTestId("deck-result-forwards")).toHaveTextContent("SOCKS proxy on this machine at 1080");
    expect(screen.getByTestId("deck-result-forwards")).toHaveTextContent("remote 9999 forwarded to localhost:22");
  });

  /**
   * Scenario: the probe could not read the resolved configuration. The panel
   * SAYS it could not look, rather than rendering nothing — "I could not look"
   * and "there are none" are different claims, and a blank screen is how the
   * second one gets made by accident (PRD #741 final audit F1).
   */
  it("says it could not read the ssh config rather than showing nothing", async () => {
    const testEndpoint = vi.fn(async () =>
      report({ disclosureKnown: false, forwards: [], knownHosts: [] }),
    );
    renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));

    await waitFor(() => expect(screen.getByTestId("deck-result")).toBeInTheDocument());
    expect(screen.getByTestId("deck-result-forwards-unknown")).toHaveTextContent(
      "could not read your resolved ssh config",
    );
    expect(screen.queryByTestId("deck-result-no-forwards")).toBeNull();
    expect(screen.queryByTestId("deck-result-forward-list")).toBeNull();
  });

  /**
   * Scenario: the probe read the config and it genuinely holds no forwards. The
   * panel says so in words, so that the silence a truncated or unreadable
   * resolution produces can never be mistaken for this answer.
   */
  it("says none inherited when it read the config and found none", async () => {
    const testEndpoint = vi.fn(async () =>
      report({ disclosureKnown: true, forwards: [], knownHosts: [] }),
    );
    renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));

    await waitFor(() => expect(screen.getByTestId("deck-result")).toBeInTheDocument());
    expect(screen.getByTestId("deck-result-no-forwards")).toHaveTextContent("no forwards");
    expect(screen.queryByTestId("deck-result-forwards-unknown")).toBeNull();
  });

  /**
   * Scenario: a forward whose value `ssh -G` printed unquoted — a macOS home
   * directory with a space is enough — reaches the panel as an unreadable line
   * rather than being dropped in the parser. This is F1's whole point: the
   * tunnel carries that forward either way, so the screen must show it.
   */
  it("lists a forward whose endpoints could not be split", async () => {
    const testEndpoint = vi.fn(async () =>
      report({
        disclosureKnown: true,
        forwards: [
          "a local forward, which this build could not split into endpoints — ssh printed `/Users/First Last/db.sock /var/run/pg.sock`",
        ],
      }),
    );
    renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));

    await waitFor(() => expect(screen.getByTestId("deck-result-forward-list")).toBeInTheDocument());
    expect(screen.getByTestId("deck-result-forward-list")).toHaveTextContent("a local forward");
    expect(screen.getByTestId("deck-result-forward-list")).toHaveTextContent("/Users/First Last/db.sock");
    expect(screen.queryByTestId("deck-result-no-forwards")).toBeNull();
  });

  /**
   * Scenario: the resolved config redirects where host keys are checked. The
   * tunnel forces the host-key CHECK and inherits the TRUST ANCHOR, so a
   * `KnownHostsCommand` that answers with whatever key the server presents
   * satisfies the forced `yes` against any host — and this row is the only
   * place a user can see that their config chose one (PRD #741 final audit F2).
   */
  it("discloses where host keys are checked", async () => {
    const testEndpoint = vi.fn(async () =>
      report({
        disclosureKnown: true,
        forwards: [],
        knownHosts: [
          "host keys come from the command `/bin/inventory-keys %H`, not from a file",
          "your known-hosts file: /dev/null",
        ],
      }),
    );
    renderPanel(
      { endpoints: { remote: [deck()], selection: "deck0000000000aa" } },
      { testEndpoint },
    );

    fireEvent.click(screen.getByTestId("test-connection"));

    await waitFor(() => expect(screen.getByTestId("deck-result-known-hosts")).toBeInTheDocument());
    expect(screen.getByTestId("deck-result-known-hosts")).toHaveTextContent("/bin/inventory-keys %H");
    expect(screen.getByTestId("deck-result-known-hosts")).toHaveTextContent("/dev/null");
  });

  /**
   * Scenario: the panel is rendered where no bridge is mounted. There is no
   * Test connection button — a button that cannot do anything is worse than no
   * button, and the panel still adds, selects and removes.
   */
  it("hides Test connection where no bridge is mounted", () => {
    renderPanel({ endpoints: { remote: [deck()], selection: "deck0000000000aa" } });
    expect(screen.queryByTestId("test-connection")).toBeNull();
    expect(screen.getByTestId("deck-choices")).toBeInTheDocument();
  });

  /**
   * Scenario: a save failed. The change is still applied for this session, and
   * the panel says what that means rather than silently reverting.
   *
   * The lead-in moved into `useDesktopSettings` at issue #1072, because the same
   * prop now also carries "your settings file cannot be read" — so the panel
   * renders whatever it is handed and this passes the composed sentence.
   */
  it("says a failed save will not survive a restart", () => {
    renderPanel({}, { saveError: "This change is applied, but saving it failed, so it will not survive a restart. Permission denied" });
    expect(screen.getByRole("alert")).toHaveTextContent("will not survive a restart");
    expect(screen.getByRole("alert")).toHaveTextContent("Permission denied");
  });

  /**
   * Scenario (issue #1072): the document on disk cannot be read. The deck list
   * shown is this build's defaults, every save is refused so the user's decks
   * survive, and the panel says exactly that with no invented preamble.
   */
  it("renders an unreadable-document message verbatim", () => {
    renderPanel({}, { saveError: "The desktop settings file cannot be read: line 4, column 8 is not valid settings. This session is using default settings, and nothing will be saved over the file until it is fixed or removed." });
    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent("nothing will be saved over the file");
    expect(alert).not.toHaveTextContent("saving it failed");
  });
});
