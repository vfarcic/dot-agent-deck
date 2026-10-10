/**
 * PRD #1401 — the GitHub section: "Sign out of GitHub" for the app's own
 * pull request browser.
 *
 * The in-app browser keeps its own GitHub sign-in, apart from the system
 * browser's, in a profile that survives restarts (`pr_browser.rs`). This is
 * the one place to end it: the button clears that profile — cookies, storage
 * and cache — so the next pull request opens signed out. It stores nothing in
 * the settings document, which is why it implements `SettingsPanelProps` and
 * reads none of it.
 */
import { useContext, useState } from "react";
import { AlertTriangle, LogOut } from "lucide-react";
import type { SettingsPanelProps } from "../lib/settingsContract";
import { PrBrowserHostContext } from "../lib/prBrowser";

type SignOut = { state: "idle" } | { state: "busy" } | { state: "done" } | { state: "failed"; reason: string };

export function GitHubPanel({ mode }: SettingsPanelProps) {
  const host = useContext(PrBrowserHostContext);
  const [signOut, setSignOut] = useState<SignOut>({ state: "idle" });
  const available = mode === "live" && host.available;

  const run = () => {
    setSignOut({ state: "busy" });
    host.signOut().then(
      () => setSignOut({ state: "done" }),
      (error: unknown) => setSignOut({ state: "failed", reason: error instanceof Error ? error.message : String(error) }),
    );
  };

  return (
    <div className="settings-body">
      <div className="settings-row">
        <span className="settings-row-label" id="github-sign-out-label">Pull requests in the app</span>
        <button
          type="button"
          aria-describedby="github-sign-out-label"
          className="button secondary"
          disabled={!available || signOut.state === "busy"}
          onClick={run}
        >
          <LogOut size={13} aria-hidden="true" />
          {signOut.state === "busy" ? "Signing out…" : "Sign out of GitHub"}
        </button>
      </div>
      {signOut.state === "done" && <p className="settings-hint" role="status">Signed out. The next pull request you open asks you to sign in again.</p>}
      {signOut.state === "failed" && (
        <p className="settings-error" role="alert">
          <AlertTriangle size={13} />
          <span>{`Could not sign out: ${signOut.reason}`}</span>
        </p>
      )}
      {!available && <p className="settings-hint">Pull requests open in the desktop app, not in the browser preview.</p>}
    </div>
  );
}
