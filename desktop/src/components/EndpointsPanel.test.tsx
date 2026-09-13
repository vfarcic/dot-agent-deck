import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { EndpointsPanel } from "./EndpointsPanel";
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
      ? <SettingsBridgeProvider value={{ testEndpoint: options.testEndpoint }}>{element}</SettingsBridgeProvider>
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
  /**
   * Scenario: open the Decks section on a fresh install — a document with no
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
   * Scenario: press "Add a deck". A row appears, focused, with its fields ready
   * to fill in — and **nothing is saved yet**, because a deck with no host is
   * not a document Rust will accept (PRD #741, Greptile P2 on #1035). Writing
   * it immediately meant the row lived only in this component's optimistic
   * state and that every unrelated settings save failed while it sat there.
   */
  it("adds a deck as a local draft, saving nothing until it is valid", () => {
    const { onSave } = renderPanel({}, { testEndpoint: async () => report() });
    fireEvent.click(screen.getByTestId("add-deck"));

    expect(onSave).not.toHaveBeenCalled();
    // Present, focused, and named for what it is until it has a host.
    expect(screen.getByTestId("deck-detail")).toBeInTheDocument();
    expect(screen.getByLabelText("Host")).toHaveValue("");
    expect(screen.getByText("New deck")).toBeInTheDocument();
    // And it cannot be probed while it is unusable.
    expect(screen.getByTestId("test-connection")).toBeDisabled();
  });

  /**
   * Scenario: fill the draft's Host in. The moment the row is storable it stops
   * being a draft — it goes into the document with a freshly minted id and
   * becomes the selection, which is what pressing "Add a deck" was asking for.
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
   * Scenario: press "Add a deck" and then remove the row again without typing
   * anything. Nothing was ever written, so nothing has to be unwritten.
   */
  it("drops an abandoned draft without touching the document", () => {
    const { onSave } = renderPanel();
    fireEvent.click(screen.getByTestId("add-deck"));
    const remove = screen.getByLabelText("Remove New deck");
    fireEvent.click(remove);

    expect(onSave).not.toHaveBeenCalled();
    expect(screen.queryByTestId("deck-detail")).toBeNull();
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
   * Scenario: remove the deck currently in use. The row goes and the selection
   * falls back to the local deck — the app always has a deck, and picking a
   * different remote one on the user's behalf is a decision they did not make.
   */
  it("removes a deck and falls back to local when it was the one in use", () => {
    const row = deck();
    const { onSave } = renderPanel({ endpoints: { remote: [row], selection: row.id } });

    fireEvent.click(screen.getByTestId(`remove-deck-${row.id}`));

    const saved = onSave.mock.calls[0][0] as DesktopSettingsDto;
    expect(saved.endpoints?.remote).toEqual([]);
    expect(saved.endpoints?.selection).toBe("local");
  });

  /**
   * Scenario: a deck is named by its address, because there is no display name
   * to give it — a user-chosen label would be exactly the arbitrary `String`
   * the settings field-type guard refuses.
   */
  it("labels a deck from its address, port included when it is not 22", () => {
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
   * text around the deck's name.
   */
  it("strips a bidi override from a deck label", () => {
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
   * Scenario: press Test connection against a deck nothing answers on. The
   * panel reports the state and the reason **in place** — the section it is
   * in is still there, the deck list is still there, and the fields are still
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
   * Scenario: the user removes the deck while its probe is still running. The
   * write-back must not resurrect it — a deck that is no longer in the document
   * has no socket to learn (PRD #741, Greptile P1 on #1035).
   */
  it("does not resurrect a deck removed while its probe was running", async () => {
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
   */
  it("says a failed save will not survive a restart", () => {
    renderPanel({}, { saveError: "Permission denied" });
    expect(screen.getByRole("alert")).toHaveTextContent("will not survive a restart");
    expect(screen.getByRole("alert")).toHaveTextContent("Permission denied");
  });
});
