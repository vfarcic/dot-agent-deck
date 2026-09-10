import { useCallback, useEffect, useRef, useState } from "react";
import type { DaemonProject, DaemonResolvedProject } from "../types";

/**
 * PRD #819 M6: the project surface, sourced entirely from the connected daemon.
 *
 * # What this replaced, and why nothing takes its place
 *
 * `useProjects` kept a `dot-agent-deck.desktop.projects.v1` `localStorage`
 * list: locally minted ids, a free-typed `cwd`, a workflow name and notes,
 * seeded from the desktop crate's own `desktop_project_cwd()` guess. That list
 * was the SOURCE OF TRUTH for the launch working directory, and nothing
 * validated it against the daemon's world — so against a remote daemon a launch
 * ran in a directory that need not exist on the machine the agents run on, and
 * did not error.
 *
 * **Nothing is persisted in its place, on either side.** The TUI remembers no
 * projects and needs to remember none, because `cd` is its selection mechanism.
 * What a window lacks is not memory but an equivalent of `cd`, and that is a
 * selection problem — which is what this hook solves. The design self-seeds
 * through the action it exists to support: launching in a project puts an agent
 * there, so the daemon enumerates it for as long as anything is running there.
 * Any future convenience must be PREFILL for a field the daemon still resolves,
 * never an authority (PRD #819, *Nothing remembers a project*).
 *
 * # The three states, all of them ordinary
 *
 * 1. `"empty"` — the daemon knows nothing live and its startup cwd is not a
 *    project. Say so, and offer the path field. This is the only first-run
 *    behaviour, not one of two.
 * 2. `"vanished"` — a path that was listed no longer resolves. Enumeration is
 *    derived from LIVE state, so a project stops being known when its last
 *    agent exits, possibly seconds after being drawn. Presented like the empty
 *    state rather than as an error, because that is what it is.
 * 3. An older daemon never reaches here at all: the desktop refuses at the
 *    handshake on exact protocol equality, naming the version. There is
 *    deliberately no local fallback — one would reinstate the
 *    silently-wrong-filesystem behaviour on the least tested path.
 */
export type ProjectListingState = "idle" | "loading" | "ready" | "empty" | "unavailable";

/**
 * The daemon's stable refusal code for "that path did not resolve"
 * (`daemon_protocol::PROJECT_ERR_UNRESOLVED`), as the first token of its error
 * plus the `": "` its own convention puts after it. Matched as a CODE, never as
 * prose: the sentence after it is deliberately uninformative for a path the
 * daemon does not already know.
 */
const PROJECT_UNRESOLVED_CODE = "unresolved: ";

export interface DaemonProjectsState {
  projects: DaemonProject[];
  primary?: string;
  listing: ProjectListingState;
  /** Why the listing could not be fetched. Only set for `"unavailable"`. */
  listingError?: string;
  /**
   * The resolved selection this launch will use, or `undefined` for "no project
   * chosen". Transient by construction — it lives in React state and nothing
   * writes it anywhere.
   */
  selected?: DaemonResolvedProject;
  resolving: boolean;
  /**
   * A refusal from the last resolve attempt. Bounded, daemon-authored text —
   * the daemon deliberately says less about an arbitrary pasted path than about
   * one it already knows.
   */
  resolveError?: string;
  /**
   * The last selection stopped resolving: it left the daemon's known set, or
   * its config went away. Renders like the empty state.
   */
  vanished: boolean;
  refresh: () => Promise<void>;
  select: (path: string) => Promise<boolean>;
  clearSelection: () => void;
}

interface Deps {
  listProjects: () => Promise<{ projects: DaemonProject[]; primary?: string }>;
  resolveProject: (path: string) => Promise<DaemonResolvedProject>;
  /** Re-list whenever this changes — a reconnect, or the agent set moving. */
  revision: string;
  /** Skip the listing entirely while the daemon is not connected. */
  enabled: boolean;
}

export function useDaemonProjects({ listProjects, resolveProject, revision, enabled }: Deps): DaemonProjectsState {
  const [projects, setProjects] = useState<DaemonProject[]>([]);
  const [primary, setPrimary] = useState<string>();
  const [listing, setListing] = useState<ProjectListingState>("idle");
  const [listingError, setListingError] = useState<string>();
  const [selected, setSelected] = useState<DaemonResolvedProject>();
  const [resolving, setResolving] = useState(false);
  const [resolveError, setResolveError] = useState<string>();
  const [vanished, setVanished] = useState(false);
  /*
   * PRD #819 Greptile P2(d): NOTHING ORDERS TWO IN-FLIGHT RPCs. Both of these
   * are `async` and neither had a guard, so an older reply landing after a newer
   * one overwrote the newer one — a listing could go back to offering a project
   * that has since disappeared, and a selection could snap back to the project
   * the user clicked before the one they are looking at.
   *
   * A monotonic generation per operation, captured before the await and re-read
   * after it: a reply that is no longer the newest is dropped, including its
   * error and its loading state. Refs rather than state, deliberately — the
   * counter must be readable by a closure that is already running, and a state
   * update would give that closure the value it captured on render.
   *
   * This interacts with P2(c): a refresh key that tracks the daemon's actual
   * seeds re-lists more often than `status:agentCount` did, so fixing that one
   * and not this one would have made the race MORE likely rather than less.
   */
  const listingGeneration = useRef(0);
  const selectionGeneration = useRef(0);

  const refresh = useCallback(async () => {
    const generation = ++listingGeneration.current;
    if (!enabled) {
      // Not an error state: there is no daemon to ask, so there are no projects
      // to offer, and nothing local may stand in for the answer. The generation
      // was still bumped, so an in-flight listing from before the disconnect
      // cannot land on top of this.
      setProjects([]);
      setPrimary(undefined);
      setListing("idle");
      setListingError(undefined);
      return;
    }
    setListing((current) => (current === "idle" ? "loading" : current));
    try {
      const listed = await listProjects();
      if (listingGeneration.current !== generation) return;
      setProjects(listed.projects);
      setPrimary(listed.primary);
      setListingError(undefined);
      setListing(listed.projects.length ? "ready" : "empty");
    } catch (cause) {
      if (listingGeneration.current !== generation) return;
      setProjects([]);
      setPrimary(undefined);
      setListingError(cause instanceof Error ? cause.message : String(cause));
      setListing("unavailable");
    }
  }, [enabled, listProjects]);

  useEffect(() => {
    void refresh();
  }, [refresh, revision]);

  /**
   * Resolve one path and adopt the result as the selection. `path` is either
   * one the daemon listed or one the user typed — never one this app derived
   * from its own environment, which is the invariant the whole PRD exists for.
   *
   * The daemon's canonical spelling is what lands in state, so the string the
   * launch sends is the daemon's own and not the one that was clicked.
   */
  const select = useCallback(async (path: string) => {
    const generation = ++selectionGeneration.current;
    setResolving(true);
    setResolveError(undefined);
    try {
      const resolved = await resolveProject(path);
      // Superseded: a later `select` is the one whose answer the user is waiting
      // for. Returning `false` is honest — this call did not become the
      // selection. The one caller that reads the boolean is `submitPasted`,
      // which clears the pasted-path field on a `true`; leaving that field
      // populated for a click the user has already moved on from is the right
      // outcome, since the path it holds is not what is selected.
      if (selectionGeneration.current !== generation) return false;
      setSelected(resolved);
      setVanished(false);
      return true;
    } catch (cause) {
      if (selectionGeneration.current !== generation) return false;
      const message = cause instanceof Error ? cause.message : String(cause);
      setSelected(undefined);
      // "Was listed and now will not resolve" is the leaves-the-set case, not a
      // fault — and only for a refusal that came from RESOLVING. A transport
      // failure against a listed path is still a failure and says so.
      const wasListed = projects.some((project) => project.path === path)
        && message.includes(PROJECT_UNRESOLVED_CODE);
      setVanished(wasListed);
      setResolveError(wasListed ? undefined : message);
      // Re-listing is what turns the picker back into an honest picture of what
      // is live.
      if (wasListed) void refresh();
      return false;
    } finally {
      // Only the newest attempt owns the spinner: an older one clearing it would
      // report the newest as finished while it is still in flight.
      if (selectionGeneration.current === generation) setResolving(false);
    }
  }, [projects, refresh, resolveProject]);

  const clearSelection = useCallback(() => {
    setSelected(undefined);
    setResolveError(undefined);
    setVanished(false);
  }, []);

  return {
    projects,
    primary,
    listing,
    listingError,
    selected,
    resolving,
    resolveError,
    vanished,
    refresh,
    select,
    clearSelection,
  };
}
