import { expect, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

/**
 * Issue #2330 — a deck refused to start an orchestration that is not approved
 * as its config now reads (an agent's dispatch, a schedule firing) and asks
 * every attached client to show the review, as a user sees it in the app.
 *
 * Driven through the real live bridge over a mocked Tauri IPC, like
 * `deck-reconnect.spec.ts`: the bundle cannot tell this from the desktop
 * crate, so the snapshot mapping and the commands the review sends run as
 * they ship. The mock records each trust command it is sent, and
 * `window.__dadEmit` delivers a deck's next snapshot.
 */

const LOCAL = "deck-0000000000002330";
const REMOTE = "deck-0000000000002331";
const HOST = "deploy@build-box";

type Request = { id: number; project: string; configDir: string; orchestration: string; digest: string; askedBy: string };

const REQUEST: Request = { id: 4, project: "/srv/demo", configDir: "/srv/demo", orchestration: "team", digest: "ab12", askedBy: "dispatch" };

/** An agent the local deck runs, for the user to be in. */
const BUILDER = { id: "agent-1", paneId: "pane-1", displayName: "Builder", agentType: "claude", status: "working", toolCount: 0, rows: 32, cols: 120, incarnation: `process-${LOCAL}`, tab: { kind: "dashboard" } };

function snapshot(deckId: string, requests: Request[], agents: unknown[] = []) {
  const local = deckId === LOCAL;
  return {
    connection: {
      status: "connected",
      deckId,
      deckKind: local ? "local" : "remote",
      socketPath: local ? "/var/tmp/trust-request-browser.sock" : HOST,
      clientProtocolVersion: 10,
      serverProtocolVersion: 10,
      clientBuildVersion: "0.50.0",
      daemonBuildVersion: "0.50.0",
      runningAgentCount: 0,
      incarnation: `process-${deckId}`,
      ...(requests.length ? { trustRequests: requests } : {}),
    },
    agents,
    protocolVersion: 10,
    source: "daemon",
    fleet: [LOCAL, REMOTE],
    observed: [
      { deckId: LOCAL, label: "/var/tmp/trust-request-browser.sock", deckKind: "local" },
      { deckId: REMOTE, label: HOST, deckKind: "remote" },
    ],
  };
}

type Call = { command: string; args: Record<string, unknown> };
type MockWindow = Window & { __dadCalls: Call[]; __dadEmit: (payload: unknown) => void; __dadListening: () => boolean };

/** The app on its live bridge, the local deck connected with nothing pending, and the remote deck listing `remote`. */
async function open(page: Page, remote: Request[], before?: () => Promise<void>, localAgents: unknown[] = []): Promise<void> {
  await page.addInitScript(({ local }) => {
    const callbacks = new Map<number, (event: unknown) => void>();
    const listeners = new Map<string, number[]>();
    const calls: { command: string; args: Record<string, unknown> }[] = [];
    let next = 0;
    const emit = (payload: unknown) => {
      for (const id of listeners.get("desktop://snapshot") ?? []) callbacks.get(id)?.({ event: "desktop://snapshot", id, payload });
    };
    const view = (args: Record<string, unknown>, state: string) => ({
      kind: "review",
      project: args.path,
      orchestration: args.orchestration,
      state,
      roles: [{ name: "coder", command: "claude --model haiku", start: true, clear: false, prompt: ["Work the task."] }],
      removedRoles: [],
      digest: "ab12",
    });
    Object.defineProperty(window, "__dadCalls", { value: calls });
    Object.defineProperty(window, "__dadEmit", { value: emit });
    Object.defineProperty(window, "__dadListening", { value: () => (listeners.get("desktop://snapshot") ?? []).length > 0 });
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {
      transformCallback: (callback: (event: unknown) => void) => {
        const id = ++next;
        callbacks.set(id, callback);
        return id;
      },
      unregisterCallback: (id: number) => callbacks.delete(id),
      invoke: async (command: string, args: Record<string, unknown> = {}) => {
        if (command === "plugin:event|listen") {
          const event = String(args.event);
          listeners.set(event, [...(listeners.get(event) ?? []), Number(args.handler)]);
          return args.handler;
        }
        if (command === "plugin:event|unlisten") {
          const event = String(args.event);
          listeners.set(event, (listeners.get(event) ?? []).filter((id) => id !== args.eventId));
          return;
        }
        if (command === "desktop_bootstrap") return local;
        if (command === "desktop_project_trust_state") {
          calls.push({ command, args });
          return view(args, "never_approved");
        }
        if (command === "desktop_approve_project_trust") {
          calls.push({ command, args });
          return view(args, "approved");
        }
        if (command === "desktop_deny_trust_request") {
          calls.push({ command, args });
          return null;
        }
        if (command === "desktop_features") return {};
        if (command === "desktop_get_settings") return { settings: { version: 1, appearance: { mode: "light" }, zoom: { level: 1 } } };
        if (command === "desktop_set_zoom") return args.level;
        return { ok: true };
      },
    } });
    Object.defineProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__", {
      value: { unregisterListener: (_event: string, id: number) => callbacks.delete(id) },
    });
  }, { local: snapshot(LOCAL, [], localAgents) });
  await page.goto("/?live=1");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  // The remote deck's watcher reports after the bootstrap, as the crate's do —
  // once the bridge listens, or the snapshot is emitted to nobody (seen on
  // WebKit, whose start is slower).
  await page.waitForFunction(() => (window as unknown as MockWindow).__dadListening());
  await before?.();
  await page.evaluate((payload) => (window as unknown as MockWindow).__dadEmit(payload), snapshot(REMOTE, remote));
}

async function calls(page: Page, command: string): Promise<Record<string, unknown>[]> {
  return page.evaluate((name) => (window as unknown as MockWindow).__dadCalls.filter((call) => call.command === name).map((call) => call.args), command);
}

function review(page: Page) {
  return page.getByRole("alertdialog", { name: "Review project commands" });
}

test.describe("An in-deck approval request (issue #2330)", () => {
  /** Scenario: the remote deck lists a request after a refused dispatch. The review comes up on its own naming that deck and what asked, with Deny and Approve; Approve sends the approval of the text shown to that deck, the review closes, and the app says the dispatch starts when it is retried. */
  test("Approve sends the approval to the deck that asked", async ({ page }) => {
    await open(page, [REQUEST]);
    const dialog = review(page);
    await expect(dialog).toBeVisible();
    await expect(dialog.getByTestId("project-trust-deck")).toHaveText(HOST);
    await expect(dialog.getByTestId("project-trust-asked-by")).toHaveText("dispatch");
    await expect(dialog).toContainText("The deck refused to start this orchestration because it is not approved as its config now reads.");
    await expect(dialog.getByTestId("project-trust-command")).toHaveText("claude --model haiku");
    await expect(dialog.getByRole("button", { name: "Deny", exact: true })).toBeInViewport({ ratio: 1 });
    await expect(dialog.getByRole("button", { name: "Approve", exact: true })).toBeInViewport({ ratio: 1 });

    await dialog.getByRole("button", { name: "Approve", exact: true }).click();

    await expect(dialog).toBeHidden();
    await expect(page.getByTestId("trust-request-toast")).toContainText("Approved orchestration 'team'. It starts when dispatch tries again.");
    expect(await calls(page, "desktop_approve_project_trust")).toEqual([{ deckId: REMOTE, path: "/srv/demo", orchestration: "team", digest: "ab12" }]);
    expect(await calls(page, "desktop_deny_trust_request")).toEqual([]);
  });

  /** Scenario: the user clicks Deny. The deck that asked is told by the request's number, the review closes, and the app says the deck will not ask about this text again until it restarts. */
  test("Deny sends the decline to the deck that asked", async ({ page }) => {
    await open(page, [REQUEST]);
    const dialog = review(page);
    await expect(dialog).toBeVisible();
    await dialog.getByRole("button", { name: "Deny", exact: true }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByTestId("trust-request-toast")).toContainText("Declined orchestration 'team'. The deck will not ask about this text again until it restarts.");
    expect(await calls(page, "desktop_deny_trust_request")).toEqual([{ deckId: REMOTE, id: 4, digest: "ab12" }]);
    expect(await calls(page, "desktop_approve_project_trust")).toEqual([]);
  });

  /** Scenario: Esc pressed the moment the review comes up does nothing; once it has been up a moment, Esc declines it. */
  test("Esc declines only once the review has been up a moment", async ({ page }) => {
    await open(page, [REQUEST]);
    const dialog = review(page);
    await expect(dialog).toHaveAttribute("data-keys-ready", "true");
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    expect(await calls(page, "desktop_deny_trust_request")).toEqual([{ deckId: REMOTE, id: 4, digest: "ab12" }]);
  });

  /** Scenario: the request is answered in the TUI while the review is open, so the deck's next snapshot no longer lists it. The review closes on its own, the app says it was answered elsewhere, and nothing is sent. */
  test("closes when the request is answered elsewhere", async ({ page }) => {
    await open(page, [REQUEST]);
    const dialog = review(page);
    await expect(dialog).toBeVisible();
    await page.evaluate((payload) => (window as unknown as MockWindow).__dadEmit(payload), snapshot(REMOTE, []));
    await expect(dialog).toBeHidden();
    await expect(page.getByTestId("trust-request-toast")).toContainText("The approval request for orchestration 'team' was answered elsewhere.");
    expect(await calls(page, "desktop_deny_trust_request")).toEqual([]);
    expect(await calls(page, "desktop_approve_project_trust")).toEqual([]);
  });

  /** Scenario: the user is in an agent's pane when a deck asks for an approval. The pane stays on screen and is not covered, and a marker saying "1 approval waiting" is on screen over it; clicking the marker opens the review over the pane, naming the deck that asked. */
  test("says an approval is waiting while an agent's pane is open", async ({ page }) => {
    await open(page, [REQUEST], async () => {
      await page.getByRole("row", { name: "Open Builder agent", exact: true }).locator(".overview-state .status-label").click();
      await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
    }, [BUILDER]);
    const marker = page.getByRole("button", { name: /1 approval waiting/ });
    await expect(marker).toBeInViewport({ ratio: 1 });
    await expect(review(page)).toHaveCount(0);
    await marker.click();
    const dialog = review(page);
    await expect(dialog).toBeVisible();
    await expect(dialog.getByTestId("project-trust-deck")).toHaveText(HOST);
    await expect(dialog.getByRole("button", { name: "Approve", exact: true })).toBeInViewport({ ratio: 1 });
    await expect(page.getByTestId("agent-pane-overlay")).toBeAttached();
    await expect(marker).toHaveCount(0);
  });

  /** Scenario: a deck asks for an approval while the user is in an agent's pane, and the user goes back to the dashboard. The review comes up on its own. */
  test("opens a held review once the pane closes", async ({ page }) => {
    await open(page, [REQUEST], async () => {
      await page.getByRole("row", { name: "Open Builder agent", exact: true }).locator(".overview-state .status-label").click();
      await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
    }, [BUILDER]);
    await expect(review(page)).toHaveCount(0);
    await page.getByTestId("agent-pane-overlay").getByRole("button", { name: "Back to dashboard" }).click();
    await expect(review(page)).toBeVisible();
  });
});
