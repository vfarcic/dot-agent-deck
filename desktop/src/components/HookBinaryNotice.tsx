import { useState } from "react";
import { AlertTriangle, Check, Copy } from "lucide-react";
import type { HookBinaryNotice } from "../types";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { writeClipboardText } from "../lib/clipboard";

/**
 * Issue #1637 — the sentence for one notice. The same words the TUI's
 * dashboard footer row uses (`hook_notice_line` in `src/ui.rs`), so a user
 * moving between the two reads the same thing (CLAUDE.md rule 22).
 */
export function hookNoticeSentence(notice: HookBinaryNotice): string {
  const agents = notice.agents.join(", ");
  switch (notice.reason) {
    case "older":
      return `${agents} hooks run dot-agent-deck ${notice.version ?? "?"} (${notice.binary}); this deck is ${notice.daemonVersion}`;
    case "unreported":
      return `${agents} hooks run an older dot-agent-deck (${notice.binary}) that predates version reporting; this deck is ${notice.daemonVersion}`;
    case "unprobeable":
      return `${agents} hooks run a dot-agent-deck that did not report its version (${notice.binary}); this deck is ${notice.daemonVersion}`;
    case "ephemeral_location":
      return `Agent hooks are off: this deck runs from a disk image or a temporary location (${notice.binary})`;
    default:
      return `${agents} hooks run (${notice.binary})`;
  }
}

/**
 * What the Copy button puts on the clipboard: the notice's command, and only
 * when it displays exactly as it is — no control, bidi or line-separator
 * character and within the display limit — so what is copied is what the
 * strip shows. `undefined` (and no button) otherwise, and always when the
 * notice has no command: the remedy's words are never copied.
 */
export function hookNoticeCopyText(notice: HookBinaryNotice): string | undefined {
  const command = notice.command;
  if (!command) return undefined;
  return displayText(command, DISPLAY_LIMITS.message) === command ? command : undefined;
}

/**
 * Issue #1637 — a strip under the connection banner naming agents whose hooks
 * on this deck run an older `dot-agent-deck`, with the daemon's remedy and a
 * button that copies it. It is not the connection banner: the connection is
 * fine, and nothing here blocks the deck. Renders nothing when there is no
 * notice.
 */
export function HookBinaryNotices({ notices }: { notices?: HookBinaryNotice[] }) {
  if (!notices?.length) return null;
  return (
    <div className="hook-binary-notices">
      {notices.map((notice) => (
        <HookBinaryNoticeRow key={`${notice.reason}:${notice.binary}:${notice.agents.join(",")}`} notice={notice} />
      ))}
    </div>
  );
}

function HookBinaryNoticeRow({ notice }: { notice: HookBinaryNotice }) {
  const [copied, setCopied] = useState(false);
  const command = hookNoticeCopyText(notice);
  const copy = () => {
    if (command === undefined) return;
    void writeClipboardText(command).then(
      () => setCopied(true),
      () => setCopied(false),
    );
  };
  return (
    <div className="hook-binary-notice" data-testid="hook-binary-notice" role="status">
      <AlertTriangle size={14} aria-hidden="true" />
      <div>
        <span data-testid="hook-binary-notice-message">{displayText(hookNoticeSentence(notice), DISPLAY_LIMITS.message)}</span>
        <span data-testid="hook-binary-notice-remedy">
          {displayText(notice.remedy, DISPLAY_LIMITS.message)}
          {command !== undefined && (
            <>
              {" "}
              <code data-testid="hook-binary-notice-command">{command}</code>
            </>
          )}
        </span>
      </div>
      {command !== undefined && (
        <button className="button secondary compact" data-testid="hook-binary-notice-copy" onClick={copy} title="Copy the command to the clipboard">
          {copied ? <Check size={13} /> : <Copy size={13} />}
          <span>{copied ? "Copied" : "Copy"}</span>
        </button>
      )}
    </div>
  );
}
