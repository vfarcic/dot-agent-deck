import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { SelfCopy, SelfUpgradeApi, SelfUpgradeCheck, SelfUpgradePlan, SelfUpgradeResult } from "../lib/selfUpgrade";
import { SelfUpgradeDialog } from "./SelfUpgradeDialog";

/* Plans in the crate's own words (`src/self_upgrade/plan.rs`), as
   `desktop_self_upgrade_check` serializes them. */
const APP_SWAP: SelfUpgradePlan = {
  copy: "app",
  label: "Agent Deck (desktop app)",
  headline: "Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)",
  current: "0.46.0",
  latest: "0.47.0",
  action: "swap-app",
  actionable: true,
  confirmQuestion: "Upgrade Agent Deck (desktop app) to v0.47.0?",
  provenance: { checked: true, reason: null },
  lines: [
    { text: "Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)", command: null },
    { text: "Agent Deck at /Applications/Agent Deck.app. Upgrading downloads `dot-agent-deck-desktop-alpha-macos-arm64.dmg` from release v0.47.0, checks its checksum, signature and notarization, and replaces the app. Agent Deck then restarts to run v0.47.0.", command: null },
  ],
};

const APP_DEB: SelfUpgradePlan = {
  ...APP_SWAP,
  action: "install-deb",
  lines: [
    APP_SWAP.lines[0],
    { text: "Installed from the Agent Deck `.deb` (package `agent-deck`).", command: null },
    { text: "Upgrading downloads `dot-agent-deck-desktop-alpha-linux-amd64.deb` from release v0.47.0, checks it, and asks for your password to install it. If the prompt does not install it, the command to install it is shown then.", command: null },
    { text: "Its checksum is checked against the release's checksum file, and that file's build provenance with `gh attestation verify`.", command: null },
  ],
};

const APP_NOT_WRITABLE: SelfUpgradePlan = {
  ...APP_SWAP,
  action: "manual-download",
  actionable: false,
  confirmQuestion: null,
  lines: [
    APP_SWAP.lines[0],
    { text: "/Applications is not writable by you, so Agent Deck cannot be replaced from here.", command: null },
    { text: "Download https://github.com/vfarcic/dot-agent-deck/releases/download/v0.47.0/dot-agent-deck-desktop-alpha-macos-arm64.dmg, open it, and drag Agent Deck.app into /Applications, replacing the old one.", command: null },
  ],
};

const CLI_BREW: SelfUpgradePlan = {
  copy: "cli",
  label: "dot-agent-deck",
  headline: "dot-agent-deck: update available: v0.47.0 (current: v0.46.0)",
  current: "0.46.0",
  latest: "0.47.0",
  action: "brew-upgrade",
  actionable: true,
  confirmQuestion: "Upgrade dot-agent-deck to v0.47.0?",
  provenance: { checked: false, reason: "the GitHub CLI (`gh`) is not installed" },
  lines: [
    { text: "dot-agent-deck: update available: v0.47.0 (current: v0.46.0)", command: null },
    { text: "Installed with Homebrew (/opt/homebrew/Cellar/dot-agent-deck/0.46.0/bin/dot-agent-deck). Upgrading runs `brew upgrade dot-agent-deck`, which installs the tap's latest release.", command: null },
  ],
};

const CLI_NIX: SelfUpgradePlan = {
  ...CLI_BREW,
  action: "notify-only",
  actionable: false,
  confirmQuestion: null,
  lines: [
    CLI_BREW.lines[0],
    { text: "Installed with Nix (/nix/store/abc/bin/dot-agent-deck), so it is not changed from here. Update your flake input (for example `nix flake update`) and rebuild, or run `nix profile upgrade`.", command: null },
  ],
};

const CLI_SHOW_COMMAND: SelfUpgradePlan = {
  ...CLI_BREW,
  action: "show-command",
  actionable: false,
  confirmQuestion: null,
  lines: [
    CLI_BREW.lines[0],
    { text: "Installed with Homebrew (/opt/homebrew/Cellar/dot-agent-deck/0.46.0/bin/dot-agent-deck), but `brew` was not found. Upgrade it with:", command: null },
    { text: "brew upgrade dot-agent-deck", command: "brew upgrade dot-agent-deck" },
  ],
};

function checkOf(app: SelfUpgradePlan, cli: SelfUpgradePlan | null = null, checkId = 1): SelfUpgradeCheck {
  return { checkId, latest: "0.47.0", updateAvailable: true, notice: app.headline, installed: null, app, cli, recheckAfterSecs: 21600 };
}

const INSTALL_DEB_PLACEHOLDER = "sudo apt install /stage/v0.47.0-1f2e/x.deb";
const INSTALL_DEB = "echo '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  /stage/v0.47.0-1f2e/x.deb' | sha256sum -c - && sudo apt install /stage/v0.47.0-1f2e/x.deb";

/** A staged CLI install the bridge reported, with `command` as the crate built it. */
function stagedResult(command: string, text = command): SelfUpgradeResult {
  return {
    copy: "cli",
    ok: true,
    relaunch: false,
    lines: [
      { text: "Downloaded and checked. Install it with:", command: null },
      { text, command },
    ],
  };
}

const SWAPPED: SelfUpgradeResult = {
  copy: "app",
  ok: true,
  relaunch: true,
  lines: [
    { text: "Replaced /Applications/Agent Deck.app with v0.47.0. Quit and reopen Agent Deck to run it.", command: null },
    { text: "Build provenance verified with `gh attestation verify`.", command: null },
  ],
};

/* The `.deb` installed behind the password prompt replaces the running app's
   files, so the bridge offers Relaunch for it as for the `.dmg` swap. */
const DEB_INSTALLED: SelfUpgradeResult = {
  copy: "app",
  ok: true,
  relaunch: true,
  lines: [{ text: "Installed v0.47.0.", command: null }],
};

/* The `.deb` downloaded and checked, with the command to install it: nothing
   was installed over the running app, so no Relaunch. */
const DEB_STAGED: SelfUpgradeResult = {
  copy: "app",
  ok: true,
  relaunch: false,
  lines: [
    { text: "Downloaded and checked. Install it with:", command: null },
    { text: INSTALL_DEB_PLACEHOLDER, command: INSTALL_DEB_PLACEHOLDER },
  ],
};

const BREWED: SelfUpgradeResult = {
  copy: "cli",
  ok: true,
  relaunch: false,
  lines: [{ text: "`brew upgrade dot-agent-deck` finished; it now reports v0.47.0.", command: null }],
};

/** An api whose runs are resolved from the test, one copy at a time. */
function controlledApi() {
  const finishers = new Map<SelfCopy, (result: SelfUpgradeResult) => void>();
  const failers = new Map<SelfCopy, (cause: Error) => void>();
  const api = {
    check: vi.fn<SelfUpgradeApi["check"]>(),
    run: vi.fn((copy: SelfCopy) => new Promise<SelfUpgradeResult>((resolve, reject) => { finishers.set(copy, resolve); failers.set(copy, reject); })),
    relaunch: vi.fn(async () => undefined),
  };
  return {
    api,
    finish: async (result: SelfUpgradeResult) => { await act(async () => finishers.get(result.copy)!(result)); },
    fail: async (copy: SelfCopy, cause: Error) => { await act(async () => failers.get(copy)!(cause)); },
  };
}

describe("SelfUpgradeDialog", () => {
  /** Scenario: The dialog shows the app's plan and the Homebrew CLI's own plan; Upgrade upgrades the app, and only then is the CLI offered with its own question; the end offers Relaunch, because the `.deb` install replaced the running app. */
  it("self_upgrade_dialog_001 upgrades the app, then offers the CLI under a separate confirm", async () => {
    const { api, finish } = controlledApi();
    const onClose = vi.fn();
    render(<SelfUpgradeDialog check={checkOf(APP_DEB, CLI_BREW)} api={api} onClose={onClose} copyText={vi.fn(async () => undefined)} />);

    expect(screen.getByTestId("self-upgrade-plan-app")).toHaveTextContent(APP_DEB.headline);
    expect(screen.getByTestId("self-upgrade-plan-cli")).toHaveTextContent("Upgrading runs `brew upgrade dot-agent-deck`");
    expect(screen.getByTestId("self-upgrade-question")).toHaveTextContent("Upgrade Agent Deck (desktop app) to v0.47.0?");
    expect(api.run).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    expect(api.run).toHaveBeenCalledWith("app", 1);
    expect(screen.getByTestId("self-upgrade-running-app")).toBeInTheDocument();
    await finish(DEB_INSTALLED);
    expect(screen.getByTestId("self-upgrade-result-app")).toHaveTextContent("Installed v0.47.0.");

    expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "cli");
    expect(screen.getByTestId("self-upgrade-question")).toHaveTextContent("Upgrade dot-agent-deck to v0.47.0?");
    expect(api.run).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    expect(api.run).toHaveBeenLastCalledWith("cli", 1);
    await finish(BREWED);

    expect(screen.getByTestId("self-upgrade-result-cli")).toHaveTextContent("it now reports v0.47.0");
    expect(screen.getByTestId("self-upgrade-relaunch")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    expect(onClose).toHaveBeenCalled();
    expect(api.relaunch).not.toHaveBeenCalled();
  });

  /** Scenario: After the `.dmg` swap, the CLI is offered first; passing on it leads to the end, where Relaunch is offered and the app restarts only when Relaunch is pressed. */
  it("self_upgrade_dialog_002 offers Relaunch after a dmg swap and relaunches only when pressed", async () => {
    const { api, finish } = controlledApi();
    render(<SelfUpgradeDialog check={checkOf(APP_SWAP, CLI_BREW)} api={api} onClose={vi.fn()} />);

    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await finish(SWAPPED);
    expect(screen.getByTestId("self-upgrade-result-app")).toHaveTextContent("Replaced /Applications/Agent Deck.app with v0.47.0.");
    expect(screen.queryByTestId("self-upgrade-relaunch")).not.toBeInTheDocument();

    fireEvent.click(screen.getByTestId("self-upgrade-cancel"));
    expect(api.run).toHaveBeenCalledTimes(1);
    expect(api.relaunch).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("self-upgrade-relaunch"));
    expect(api.relaunch).toHaveBeenCalledTimes(1);
  });

  /** Scenario: The `.deb` is downloaded and checked but not installed (no password prompt here), so the result shows the command to install it and the dialog ends with Close alone: nothing replaced the running app. */
  it("self_upgrade_dialog_003 offers no Relaunch when the app was not replaced", async () => {
    const { api, finish } = controlledApi();
    render(<SelfUpgradeDialog check={checkOf(APP_DEB)} api={api} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await finish(DEB_STAGED);
    expect(screen.getByTestId("self-upgrade-done")).toBeInTheDocument();
    expect(screen.queryByTestId("self-upgrade-relaunch")).not.toBeInTheDocument();
  });

  /** Scenario: The app's folder is not writable and the CLI came from Nix: both plans say what to do instead, and the only button is Close — nothing can be upgraded from here. */
  it("self_upgrade_dialog_004 shows notify-only plans with Close and no Upgrade", () => {
    const { api } = controlledApi();
    const onClose = vi.fn();
    render(<SelfUpgradeDialog check={checkOf(APP_NOT_WRITABLE, CLI_NIX)} api={api} onClose={onClose} />);

    expect(screen.getByTestId("self-upgrade-plan-app")).toHaveTextContent("/Applications is not writable by you");
    expect(screen.getByTestId("self-upgrade-plan-cli")).toHaveTextContent("Update your flake input");
    expect(screen.queryByTestId("self-upgrade-start")).not.toBeInTheDocument();
    expect(screen.queryByTestId("self-upgrade-cancel")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    expect(onClose).toHaveBeenCalled();
    expect(api.run).not.toHaveBeenCalled();
  });

  /** Scenario: A Homebrew CLI with no `brew` to run shows the exact command as code with a Copy button; pressing Copy puts that command on the clipboard. */
  it("self_upgrade_dialog_005 shows a show-command plan's command, copyable", () => {
    const { api } = controlledApi();
    const copyText = vi.fn(async () => undefined);
    render(<SelfUpgradeDialog check={checkOf(APP_NOT_WRITABLE, CLI_SHOW_COMMAND)} api={api} onClose={vi.fn()} copyText={copyText} />);

    const cli = screen.getByTestId("self-upgrade-plan-cli");
    expect(within(cli).getByText("brew upgrade dot-agent-deck").tagName).toBe("CODE");
    fireEvent.click(within(cli).getByTestId("self-upgrade-copy"));
    expect(copyText).toHaveBeenCalledWith("brew upgrade dot-agent-deck");
    expect(screen.queryByTestId("self-upgrade-start")).not.toBeInTheDocument();
  });

  /** Scenario: Cancel on the first question closes the dialog having run nothing; Escape and a click on the backdrop do the same. */
  it("self_upgrade_dialog_006 Cancel does nothing", () => {
    const { api } = controlledApi();
    const onClose = vi.fn();
    const { unmount } = render(<SelfUpgradeDialog check={checkOf(APP_SWAP, CLI_BREW)} api={api} onClose={onClose} />);
    fireEvent.click(screen.getByTestId("self-upgrade-cancel"));
    expect(onClose).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(screen.getByTestId("self-upgrade-dialog"), { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(2);
    unmount();
    expect(api.run).not.toHaveBeenCalled();
    expect(api.relaunch).not.toHaveBeenCalled();
  });

  /** Scenario: The password prompt was dismissed; the result shows the error and the exact `apt` command to install the checked file, and nothing is offered to relaunch. */
  it("self_upgrade_dialog_007 shows a failure with the command to run instead", async () => {
    const { api, finish } = controlledApi();
    render(<SelfUpgradeDialog check={checkOf(APP_DEB)} api={api} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await finish({
      copy: "app",
      ok: false,
      relaunch: false,
      lines: [
        { text: "`/usr/bin/pkexec /usr/bin/apt-get install -y /stage/x.deb` failed: Request dismissed", command: null },
        { text: "It was downloaded and checked, but not installed. Install it with:", command: null },
        { text: INSTALL_DEB, command: INSTALL_DEB },
      ],
    });
    const result = screen.getByTestId("self-upgrade-result-app");
    expect(result).toHaveAttribute("data-ok", "false");
    expect(result).toHaveTextContent("Request dismissed");
    expect(within(result).getByTestId("self-upgrade-command").querySelector("code")?.textContent).toBe(INSTALL_DEB);
    expect(screen.queryByTestId("self-upgrade-relaunch")).not.toBeInTheDocument();
  });

  /** Scenario: While an upgrade runs, Escape and a backdrop click leave the dialog open, so its outcome is not lost; a run the bridge rejects is shown as a failed result. */
  it("self_upgrade_dialog_008 stays open while running and shows a rejected run", async () => {
    const { api, fail } = controlledApi();
    const onClose = vi.fn();
    render(<SelfUpgradeDialog check={checkOf(APP_SWAP)} api={api} onClose={onClose} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    fireEvent.keyDown(screen.getByTestId("self-upgrade-dialog"), { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
    await fail("app", new Error("An upgrade is already running."));
    expect(screen.getByTestId("self-upgrade-result-app")).toHaveTextContent("An upgrade is already running.");
    expect(screen.getByTestId("self-upgrade-done")).toBeInTheDocument();
  });

  /** Scenario: A staged install's command is longer than the display limit; the dialog shows it whole and Copy puts exactly that command, unshortened, on the clipboard. */
  it("self_upgrade_dialog_009 shows and copies a long command whole", async () => {
    const { api, finish } = controlledApi();
    const copyText = vi.fn(async () => undefined);
    const staged = `/home/u/${"segment-".repeat(120)}/dot-agent-deck-linux-amd64`;
    const command = `echo '${"a".repeat(64)}  ${staged}' | sha256sum -c - && sudo install -m 0755 ${staged} /usr/local/bin/dot-agent-deck`;
    expect(command.length).toBeGreaterThan(2048);
    render(<SelfUpgradeDialog check={checkOf(APP_NOT_WRITABLE, CLI_BREW)} api={api} onClose={vi.fn()} copyText={copyText} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    // The bridge's display copy is shortened; the command is not.
    await finish(stagedResult(command, `${command.slice(0, 2048)}`));
    const result = screen.getByTestId("self-upgrade-result-cli");
    expect(within(result).getByTestId("self-upgrade-command").querySelector("code")?.textContent).toBe(command);
    fireEvent.click(within(result).getByTestId("self-upgrade-copy"));
    expect(copyText).toHaveBeenCalledWith(command);
  });

  /** Scenario: A command carrying a character the dialog's sanitiser would remove (a bidi override) is shown sanitised and offers no Copy, so nothing different from what is shown can be copied. */
  it("self_upgrade_dialog_010 offers Copy only for an unaltered command", async () => {
    const { api, finish } = controlledApi();
    const copyText = vi.fn(async () => undefined);
    render(<SelfUpgradeDialog check={checkOf(APP_NOT_WRITABLE, CLI_BREW)} api={api} onClose={vi.fn()} copyText={copyText} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await finish(stagedResult("sudo install /stage/\u202Ex /usr/local/bin/dot-agent-deck"));
    const command = within(screen.getByTestId("self-upgrade-result-cli")).getByTestId("self-upgrade-command");
    expect(command).toHaveAttribute("data-copyable", "false");
    expect(within(command).queryByTestId("self-upgrade-copy")).not.toBeInTheDocument();
    expect(command).toHaveTextContent("sudo install /stage/x /usr/local/bin/dot-agent-deck");
    expect(copyText).not.toHaveBeenCalled();
  });
  /** Scenario: Each question opens with Cancel focused, as the TUI starts on Cancel, so Enter alone never upgrades: on the first question Enter on the focused button closes the dialog having run nothing, and after an upgrade the next copy's question focuses Cancel again. */
  it("self_upgrade_dialog_011 focuses Cancel on every question so Enter never upgrades", async () => {
    const { api, finish } = controlledApi();
    const onClose = vi.fn();
    const { unmount } = render(<SelfUpgradeDialog check={checkOf(APP_DEB, CLI_BREW)} api={api} onClose={onClose} />);

    expect(screen.getByTestId("self-upgrade-cancel")).toHaveFocus();
    expect(screen.getByTestId("self-upgrade-start")).not.toHaveFocus();
    /* A focused button's Enter is its click. */
    fireEvent.click(document.activeElement as HTMLElement);
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(api.run).not.toHaveBeenCalled();
    unmount();

    render(<SelfUpgradeDialog check={checkOf(APP_DEB, CLI_BREW)} api={api} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await finish(DEB_INSTALLED);
    expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "cli");
    expect(screen.getByTestId("self-upgrade-cancel")).toHaveFocus();
    expect(api.run).toHaveBeenCalledTimes(1);
  });

  /** Scenario: A background check finds a newer release while the dialog is open: the dialog keeps showing the plan it opened with, and Upgrade runs that plan, by its check id, never the newer one the user has not read. */
  it("self_upgrade_dialog_012 keeps the plan it opened with and runs exactly that one", () => {
    const { api } = controlledApi();
    const opened = checkOf(APP_SWAP, CLI_BREW, 4);
    const newer: SelfUpgradeCheck = {
      ...checkOf({ ...APP_SWAP, latest: "0.48.0", headline: "Agent Deck (desktop app): update available: v0.48.0 (current: v0.46.0)", confirmQuestion: "Upgrade Agent Deck (desktop app) to v0.48.0?" }, CLI_BREW, 5),
      latest: "0.48.0",
    };
    const { rerender } = render(<SelfUpgradeDialog check={opened} api={api} onClose={vi.fn()} />);
    rerender(<SelfUpgradeDialog check={newer} api={api} onClose={vi.fn()} />);

    expect(screen.getByRole("heading", { level: 2 })).toHaveTextContent("Upgrade to v0.47.0");
    expect(screen.getByTestId("self-upgrade-question")).toHaveTextContent("Upgrade Agent Deck (desktop app) to v0.47.0?");
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    expect(api.run).toHaveBeenCalledWith("app", 4);
  });

  /** Scenario: The app itself was upgraded earlier and has not relaunched: reopening the dialog shows that result in place of an offer, offers the CLI that is still behind, and after Cancel ends with Relaunch; with no CLI it opens straight at Relaunch. */
  it("self_upgrade_dialog_013 shows an installed app's result and keeps Relaunch reachable", () => {
    const { api } = controlledApi();
    const installed = (cli: SelfUpgradePlan | null): SelfUpgradeCheck => ({
      ...checkOf({ ...APP_SWAP, actionable: false, confirmQuestion: null }, cli, 7),
      installed: SWAPPED,
    });
    const { unmount } = render(<SelfUpgradeDialog check={installed(CLI_BREW)} api={api} onClose={vi.fn()} />);
    expect(screen.getByTestId("self-upgrade-result-app")).toHaveTextContent("Replaced /Applications/Agent Deck.app with v0.47.0.");
    expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "cli");
    fireEvent.click(screen.getByTestId("self-upgrade-cancel"));
    fireEvent.click(screen.getByTestId("self-upgrade-relaunch"));
    expect(api.relaunch).toHaveBeenCalledTimes(1);
    expect(api.run).not.toHaveBeenCalled();
    unmount();

    render(<SelfUpgradeDialog check={installed(null)} api={api} onClose={vi.fn()} />);
    expect(screen.queryByTestId("self-upgrade-start")).not.toBeInTheDocument();
    expect(screen.getByTestId("self-upgrade-relaunch")).toHaveFocus();
  });

  /** Scenario: Homebrew's tap does not carry the release yet: the CLI's result is a failure in the crate's words, and no Relaunch is offered. */
  it("self_upgrade_dialog_014 shows a brew upgrade that did not reach the release as a failure", async () => {
    const { api, finish } = controlledApi();
    render(<SelfUpgradeDialog check={checkOf(APP_NOT_WRITABLE, CLI_BREW)} api={api} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await finish({
      copy: "cli",
      ok: false,
      relaunch: false,
      lines: [{ text: "`brew upgrade dot-agent-deck` finished, but dot-agent-deck still reports v0.46.0, not v0.47.0: Homebrew does not offer v0.47.0 yet. Try again later.", command: null }],
    });
    const result = screen.getByTestId("self-upgrade-result-cli");
    expect(result).toHaveAttribute("data-ok", "false");
    expect(result).toHaveTextContent("Homebrew does not offer v0.47.0 yet");
    expect(screen.queryByTestId("self-upgrade-relaunch")).not.toBeInTheDocument();
  });
});
