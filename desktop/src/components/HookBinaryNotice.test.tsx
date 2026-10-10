import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { HookBinaryNotice } from "../types";

const writeClipboardText = vi.fn(async (_text: string) => undefined);
vi.mock("../lib/clipboard", () => ({ writeClipboardText }));

const { HookBinaryNotices, hookNoticeCopyText, hookNoticeRemedy, hookNoticeSentence, MAX_NOTICE_COMMAND_BYTES, REMEDY_RUN, REMEDY_UPGRADE_OR_REINSTALL } = await import("./HookBinaryNotice");

const OLDER: HookBinaryNotice = {
  binary: "/opt/homebrew/bin/dot-agent-deck",
  agents: ["Claude Code", "Codex"],
  version: "0.45.1",
  daemonVersion: "0.46.0",
  reason: "older",
  remedy: "Run:",
  command: "brew upgrade dot-agent-deck",
};

describe("HookBinaryNotices (issue #1637)", () => {
  /**
   * Scenario: a deck whose daemon reports that Claude Code's and Codex's hooks
   * run an older copy. The strip names the agents, the copy and both versions,
   * shows the daemon's remedy, and its Copy button puts the command on the
   * clipboard.
   */
  it("renders the remedy and copies the command", async () => {
    render(<HookBinaryNotices notices={[OLDER]} />);

    const strip = screen.getByTestId("hook-binary-notice");
    expect(strip).toHaveTextContent("Claude Code, Codex hooks run dot-agent-deck 0.45.1 (/opt/homebrew/bin/dot-agent-deck); this deck is 0.46.0");
    expect(screen.getByTestId("hook-binary-notice-remedy")).toHaveTextContent("Run: brew upgrade dot-agent-deck");
    expect(screen.getByTestId("hook-binary-notice-command")).toHaveTextContent("brew upgrade dot-agent-deck");

    await act(async () => {
      fireEvent.click(screen.getByTestId("hook-binary-notice-copy"));
    });
    expect(writeClipboardText).toHaveBeenCalledWith("brew upgrade dot-agent-deck");
    expect(screen.getByTestId("hook-binary-notice-copy")).toHaveTextContent("Copied");
  });

  /** Scenario: a deck with nothing to report draws no strip at all. */
  it("is absent when there is no notice", () => {
    const { container } = render(<HookBinaryNotices notices={[]} />);
    expect(container).toBeEmptyDOMElement();
    const { container: absent } = render(<HookBinaryNotices />);
    expect(absent).toBeEmptyDOMElement();
  });

  /**
   * Scenario: the strip sits beside the rest of the deck screen. It is a
   * status, not a dialog, and the controls next to it stay usable.
   */
  it("does not make the deck inert", () => {
    const onClick = vi.fn();
    render(
      <main>
        <HookBinaryNotices notices={[OLDER]} />
        <button onClick={onClick}>Spawn agent</button>
      </main>,
    );
    expect(screen.getByTestId("hook-binary-notice")).toHaveAttribute("role", "status");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    const sibling = screen.getByRole("button", { name: "Spawn agent" });
    expect(sibling.closest("[inert]")).toBeNull();
    expect(sibling.closest("[aria-hidden='true']")).toBeNull();
    fireEvent.click(sibling);
    expect(onClick).toHaveBeenCalled();
  });

  /** Scenario: each reason reads the way the TUI's footer row reads it. */
  it("words each reason the way the TUI does", () => {
    expect(hookNoticeSentence({ ...OLDER, reason: "unreported", version: undefined })).toBe(
      "Claude Code, Codex hooks run an older dot-agent-deck (/opt/homebrew/bin/dot-agent-deck) that predates version reporting; this deck is 0.46.0",
    );
    expect(hookNoticeSentence({ ...OLDER, reason: "unprobeable", version: undefined })).toBe(
      "Claude Code, Codex hooks run a dot-agent-deck that did not report its version (/opt/homebrew/bin/dot-agent-deck); this deck is 0.46.0",
    );
    expect(
      hookNoticeSentence({ ...OLDER, reason: "ephemeral_location", agents: [], binary: "/Volumes/Agent Deck/Agent Deck.app/Contents/MacOS/dot-agent-deck" }),
    ).toBe("Agent hooks are off: this deck runs from a disk image or a temporary location (/Volumes/Agent Deck/Agent Deck.app/Contents/MacOS/dot-agent-deck)");
  });

  /**
   * Scenario: a notice whose remedy has no command (moving the app, or
   * upgrading a copy the deck knows nothing about) shows the words and no
   * Copy button: prose is never copied.
   */
  it("offers no Copy button when the fix is not a command", () => {
    render(
      <HookBinaryNotices
        notices={[{ ...OLDER, reason: "ephemeral_location", agents: [], remedy: "Move Agent Deck to /Applications and reopen it to turn agent hooks on.", command: undefined }]}
      />,
    );
    expect(screen.getByTestId("hook-binary-notice-remedy")).toHaveTextContent("Move Agent Deck to /Applications and reopen it to turn agent hooks on.");
    expect(screen.queryByTestId("hook-binary-notice-copy")).toBeNull();
    expect(screen.queryByTestId("hook-binary-notice-command")).toBeNull();
  });

  /**
   * Scenario: a notice arrives whose remedy and command each hide a second
   * shell command after a newline (audit A3). The strip shows no newline,
   * offers no Copy button for the altered command, and nothing reaches the
   * clipboard.
   */
  it("never puts a newline-bearing remedy or command on the clipboard", async () => {
    writeClipboardText.mockClear();
    const hostile: HookBinaryNotice = {
      ...OLDER,
      remedy: "Run:\n touch /tmp/hook-notice-marker #",
      command: "brew upgrade dot-agent-deck\n touch /tmp/hook-notice-marker #",
    };
    expect(hookNoticeCopyText(hostile)).toBeUndefined();
    expect(hookNoticeCopyText({ ...OLDER, command: "brew upgrade \u202Ekced" })).toBeUndefined();
    expect(hookNoticeCopyText({ ...OLDER, command: undefined })).toBeUndefined();
    expect(hookNoticeCopyText(OLDER)).toBe("brew upgrade dot-agent-deck");

    render(<HookBinaryNotices notices={[hostile]} />);
    expect(screen.getByTestId("hook-binary-notice-remedy").textContent).not.toContain("\n");
    expect(screen.queryByTestId("hook-binary-notice-copy")).toBeNull();
    expect(screen.queryByTestId("hook-binary-notice-command")).toBeNull();
    expect(writeClipboardText).not.toHaveBeenCalled();
  });

  /** Scenario: what the Copy button copies is exactly the command the strip shows. */
  it("copies exactly the command it displays", async () => {
    writeClipboardText.mockClear();
    const own: HookBinaryNotice = {
      ...OLDER,
      remedy: "Run:",
      command: "'/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck' hooks install --agent codex",
    };
    render(<HookBinaryNotices notices={[own]} />);
    const shown = screen.getByTestId("hook-binary-notice-command").textContent;
    await act(async () => {
      fireEvent.click(screen.getByTestId("hook-binary-notice-copy"));
    });
    expect(writeClipboardText).toHaveBeenCalledWith(shown);
    expect(shown).toBe(own.command);
  });

  /**
   * Scenario: a notice groups four agents, so the daemon's command repeats the
   * app's path four times and runs well past a message's 240 characters. The
   * strip shows the whole command after `Run:`, and Copy copies exactly it.
   */
  it("shows a long command whole rather than a bare Run:", async () => {
    writeClipboardText.mockClear();
    const exe = "'/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck'";
    const command = ["claude-code", "opencode", "codex", "devin"].map((agent) => `${exe} hooks install --agent ${agent}`).join(" && ");
    expect(command.length).toBeGreaterThan(240);
    const long: HookBinaryNotice = { ...OLDER, agents: ["Claude Code", "OpenCode", "Codex", "Devin"], remedy: "Run:", command };
    render(<HookBinaryNotices notices={[long]} />);
    expect(screen.getByTestId("hook-binary-notice-command").textContent).toBe(command);
    expect(screen.getByTestId("hook-binary-notice-remedy").textContent).toBe(`Run: ${command}`);
    await act(async () => {
      fireEvent.click(screen.getByTestId("hook-binary-notice-copy"));
    });
    expect(writeClipboardText).toHaveBeenCalledWith(command);
  });

  /**
   * Scenario: a notice from a daemon whose command is over the cap the strip
   * shows. The strip drops the command and shows the upgrade-or-reinstall
   * advice in place of `Run:`, never a bare `Run:`.
   */
  it("falls back to the advice when it cannot show the command", () => {
    const tooLong: HookBinaryNotice = { ...OLDER, remedy: REMEDY_RUN, command: `/${"a".repeat(MAX_NOTICE_COMMAND_BYTES)} hooks install` };
    expect(hookNoticeCopyText(tooLong)).toBeUndefined();
    expect(hookNoticeRemedy(tooLong)).toBe(REMEDY_UPGRADE_OR_REINSTALL);
    render(<HookBinaryNotices notices={[tooLong]} />);
    expect(screen.getByTestId("hook-binary-notice-remedy").textContent).toBe(REMEDY_UPGRADE_OR_REINSTALL);
    expect(screen.queryByTestId("hook-binary-notice-command")).toBeNull();
    expect(screen.queryByTestId("hook-binary-notice-copy")).toBeNull();
    // A `Run:` that arrives with no command at all reads the same.
    expect(hookNoticeRemedy({ ...OLDER, command: undefined })).toBe(REMEDY_UPGRADE_OR_REINSTALL);
    expect(hookNoticeRemedy(OLDER)).toBe(REMEDY_RUN);
  });
});
