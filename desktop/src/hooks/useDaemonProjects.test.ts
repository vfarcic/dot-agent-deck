import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { DaemonProject, DaemonResolvedProject } from "../types";
import { useDaemonProjects } from "./useDaemonProjects";

/**
 * Request ordering (PRD #819, Greptile P2(d)).
 *
 * Neither `refresh` nor `select` ordered its in-flight RPCs, so whichever reply
 * arrived LAST won regardless of which request was newest. For the listing that
 * means the picker can go back to offering a project that has since disappeared;
 * for the selection it means the launch cwd can snap back to the project the
 * user clicked before the one they are looking at.
 *
 * It is not a hypothetical race here. Enumeration is derived from LIVE state, so
 * the hook re-lists whenever the daemon's seeds move — and PRD #819's P2(c) fix
 * widened that key from `status:agentCount` to the seeds themselves, which makes
 * two listings overlap MORE often rather than less. Fixing one without the other
 * would have made this worse.
 *
 * These drive the hook directly rather than through a screen, because the
 * property is about the order of two promises and nothing about what a panel
 * renders — the same reasoning `useDesktopSettings.test.ts` records for PRD
 * #803's save ordering. Every reply here is settled by hand, so "the older
 * request had not answered yet" is observable rather than inferred from timing.
 */

function project(path: string): DaemonProject {
  return { path, displayPath: path, displayName: path };
}

function resolved(path: string): DaemonResolvedProject {
  return { path, displayPath: path, displayName: path, orchestrations: [], configRevision: "revision-1" };
}

/** A `listProjects` whose every call the test settles by hand. */
function deferredListings() {
  const pending: { resolve: (listed: { projects: DaemonProject[]; primary?: string }) => void; reject: (cause: unknown) => void }[] = [];
  const listProjects = vi.fn(() => new Promise<{ projects: DaemonProject[]; primary?: string }>((resolve, reject) => {
    pending.push({ resolve, reject });
  }));
  return { listProjects, pending };
}

/** A `resolveProject` whose every call the test settles by hand. */
function deferredResolves() {
  const pending: { resolve: (project: DaemonResolvedProject) => void; reject: (cause: unknown) => void }[] = [];
  const resolveProject = vi.fn(() => new Promise<DaemonResolvedProject>((resolve, reject) => {
    pending.push({ resolve, reject });
  }));
  return { resolveProject, pending };
}

describe("useDaemonProjects request ordering", () => {
  it("drops a listing whose reply arrives after a newer one's", async () => {
    const { listProjects, pending } = deferredListings();
    const { resolveProject } = deferredResolves();
    const { result, rerender } = renderHook(
      ({ revision }: { revision: string }) => useDaemonProjects({ listProjects, resolveProject, revision, enabled: true }),
      { initialProps: { revision: "seed-a" } },
    );

    await waitFor(() => expect(pending.length).toBe(1));
    rerender({ revision: "seed-b" });
    await waitFor(() => expect(pending.length).toBe(2));

    // The NEWER request answers first, which is the whole scenario: nothing in
    // the transport orders two sockets.
    await act(async () => { pending[1].resolve({ projects: [project("/still/here")], primary: "/still/here" }); });
    expect(result.current.projects.map((entry) => entry.path)).toEqual(["/still/here"]);

    // The older one lands second, naming a project that has since gone. It must
    // be discarded rather than re-offered.
    await act(async () => { pending[0].resolve({ projects: [project("/has/vanished")], primary: "/has/vanished" }); });
    expect(result.current.projects.map((entry) => entry.path)).toEqual(["/still/here"]);
    expect(result.current.primary).toBe("/still/here");
    expect(result.current.listing).toBe("ready");
  });

  it("drops a superseded listing's FAILURE too, rather than reporting the daemon as unavailable", async () => {
    const { listProjects, pending } = deferredListings();
    const { resolveProject } = deferredResolves();
    const { result, rerender } = renderHook(
      ({ revision }: { revision: string }) => useDaemonProjects({ listProjects, resolveProject, revision, enabled: true }),
      { initialProps: { revision: "seed-a" } },
    );

    await waitFor(() => expect(pending.length).toBe(1));
    rerender({ revision: "seed-b" });
    await waitFor(() => expect(pending.length).toBe(2));

    await act(async () => { pending[1].resolve({ projects: [project("/still/here")] }); });
    await act(async () => { pending[0].reject(new Error("the socket went away")); });

    expect(result.current.listing).toBe("ready");
    expect(result.current.listingError).toBeUndefined();
    expect(result.current.projects.map((entry) => entry.path)).toEqual(["/still/here"]);
  });

  it("keeps the selection the user asked for last when an earlier resolve answers after it", async () => {
    const { listProjects, pending: listings } = deferredListings();
    const { resolveProject, pending: resolves } = deferredResolves();
    const { result } = renderHook(() => useDaemonProjects({ listProjects, resolveProject, revision: "seed", enabled: true }));

    await waitFor(() => expect(listings.length).toBe(1));
    await act(async () => { listings[0].resolve({ projects: [project("/first"), project("/second")] }); });

    // Two clicks in a row, the second before the first has answered.
    let first: Promise<boolean> | undefined;
    let second: Promise<boolean> | undefined;
    act(() => { first = result.current.select("/first"); });
    act(() => { second = result.current.select("/second"); });
    await waitFor(() => expect(resolves.length).toBe(2));

    await act(async () => { resolves[1].resolve(resolved("/second")); });
    expect(result.current.selected?.path).toBe("/second");
    expect(result.current.resolving).toBe(false);

    // The first click's answer arrives last and must not become the selection —
    // the launch would then run in a directory the user is not looking at.
    await act(async () => { resolves[0].resolve(resolved("/first")); });
    expect(result.current.selected?.path).toBe("/second");
    await expect(first).resolves.toBe(false);
    await expect(second).resolves.toBe(true);
  });

  it("keeps the newest selection when a superseded resolve FAILS", async () => {
    const { listProjects, pending: listings } = deferredListings();
    const { resolveProject, pending: resolves } = deferredResolves();
    const { result } = renderHook(() => useDaemonProjects({ listProjects, resolveProject, revision: "seed", enabled: true }));

    await waitFor(() => expect(listings.length).toBe(1));
    await act(async () => { listings[0].resolve({ projects: [project("/first"), project("/second")] }); });

    act(() => { void result.current.select("/first"); });
    act(() => { void result.current.select("/second"); });
    await waitFor(() => expect(resolves.length).toBe(2));

    await act(async () => { resolves[1].resolve(resolved("/second")); });
    // The superseded click's refusal must not clear the selection, raise the
    // "vanished" state, or surface an error about a path nobody is waiting on.
    await act(async () => { resolves[0].reject(new Error("unresolved: that path is not a project this daemon can offer")); });

    expect(result.current.selected?.path).toBe("/second");
    expect(result.current.vanished).toBe(false);
    expect(result.current.resolveError).toBeUndefined();
  });
});
