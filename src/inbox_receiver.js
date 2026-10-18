const INBOX_ADAPTER_INTEGRATION = __ADAPTER_INTEGRATION__;
const INBOX_MAX_CALLED = 4096;
const INBOX_GRANT_TIMEOUT_MS = 5000;
// Keepalives from a stream's close to the deck retiring it with no successor
// (three POSSESS_TTLs of three keepalives each).
const INBOX_RETIRE_KEEPALIVES = __RETIRE_KEEPALIVES__;
const inboxNow = () => performance.now();
// Whether `control` was handed to the watcher's stdin (which answers nothing).
const inboxWrite = (child, control) => {
  try {
    if (!child?.stdin || child.stdin.writableEnded || child.stdin.destroyed) return false;
    child.stdin.write(JSON.stringify(control) + "\n");
    return true;
  } catch (_) {
    return false;
  }
};
// Ending the watcher's stdin is how the receiver says it is done: the watcher
// then allows bounded time for publishing the receipts it was given before it
// exits, and one can still be lost or reported degraded. SIGTERM follows only
// if it is still running INBOX_STOP_GRACE_MS later, on a timer that keeps
// nothing alive. Stage reports queued while no watcher is registered (a
// restart's backoff, or before the new one registers) are not flushed here:
// there is no authority to publish them with. Their attempts may take the
// deck's checked, marked repeat if their message is still eligible for one;
// the generation's exit, the repeat limits and a message already ended stop
// that instead.
const INBOX_STOP_GRACE_MS = 3000;
const inboxStop = (child) => {
  if (!child) return;
  try {
    child.stdin.end();
  } catch (_) {}
  if (typeof child.exitCode === "number" || child.signalCode) return;
  const kill = () => {
    try {
      child.kill("SIGTERM");
    } catch (_) {}
  };
  try {
    const timer = setTimeout(kill, INBOX_STOP_GRACE_MS);
    timer.unref?.();
    child.once("exit", () => clearTimeout(timer));
  } catch (_) {
    kill();
  }
};
// PRD #1666: DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS sets every watcher respawn
// delay to that many milliseconds (a whole number, 1 to 3600000), as the Pi
// extension's does; unset, the delay doubles from 1 s to 30 s.
const inboxFixedBackoff = () => {
  const raw = String(process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS ?? "").trim();
  if (!/^[0-9]+$/.test(raw)) return undefined;
  const ms = Number(raw);
  return ms >= 1 && ms <= 3600000 ? ms : undefined;
};
const inboxFirstBackoff = () => inboxFixedBackoff() ?? 1000;
const inboxNextBackoff = (backoff, ranForMs) =>
  inboxFixedBackoff() ?? (ranForMs > 30000 ? 1000 : Math.min(backoff * 2, 30000));
const inboxKey = (ref) => ref.id + "|" + ref.a + "|" + ref.e;
const inboxRef = (entry) => ({ id: entry.id, a: entry.a, e: entry.e });
// PRD #1666 §2–§4: one context's receiver on the deck's receipt stream — the
// attempts it holds, from hand-out until the agent started on them (their
// commit) or they were positively not handed, its possession snapshots, the
// final hand-over check, and its answers to revokes and resumes.
//
// `epoch` counts the watchers this receiver has run. An approval of the final
// hand-over check carries the epoch it was granted under and when it was
// asked for, and a callback may reach OpenCode only while its context is
// still active, the epoch still that one, the approval younger than the
// deck's retirement of a closed stream, and the attempt neither revoked nor
// dropped (`ready`), checked with no await between it and the call. A
// watcher restart (a rebinding), the context's disposal or a stall the deck
// may have retired the stream across therefore voids every outstanding
// approval: a callback that resumes after any of them never reaches OpenCode.
//
// `exited` is told each watcher's exit code before the receiver decides
// whether to respawn it. `state` is what another receiver's `exportState`
// gave up (the Pi extension's replacement across a reload of a different
// version): this one starts holding it, and registers `--resumed` from its
// first watcher, so the deck asks about those attempts.
const inboxReceiver = ({ agent, doorbellOnly = false, active, deliver, exited, state }) => {
  const held = new Map((state?.held ?? []).map((entry) => [inboxKey(entry), { ...entry }]));
  const revoked = new Set(state?.revoked ?? []);
  // Attempts a callback took into OpenCode's API, or that the agent started
  // on, and that did not end in a positive refusal: possibly handed, whatever
  // happens to their held entry afterwards (a commit, an abort, a revert, an
  // ambiguous failure). A revoke of one is answered `already_handed` and it is
  // never reported not handed. Bounded, oldest dropped first; once one was
  // dropped, an attempt this receiver does not hold may be one of them, so
  // it is answered the same way.
  const called = new Set(state?.called ?? []);
  let forgot = state?.forgot === true;
  const call = (key) => {
    called.delete(key);
    called.add(key);
    if (called.size > INBOX_MAX_CALLED) {
      called.delete(called.values().next().value);
      forgot = true;
    }
  };
  const handedBefore = (key) =>
    held.get(key)?.stage === "handed" || called.has(key) || (forgot && !held.has(key));
  // §2 "Revoke proof and callback quiescence": the deck retires a stream
  // that closed no sooner than INBOX_RETIRE_KEEPALIVES after the close, and
  // the close comes after any approval it sent. An approval used within that
  // span of being asked for is therefore used before any retirement, so a
  // context stalled past it, with the approval buffered, cannot call once
  // the deck has fenced the attempt and a Cancel may have ended it.
  const approval = () => ({
    epoch,
    at: inboxNow(),
    valid: keepaliveMs ? Math.min(INBOX_GRANT_TIMEOUT_MS, keepaliveMs * INBOX_RETIRE_KEEPALIVES) : INBOX_GRANT_TIMEOUT_MS,
  });
  const fresh = (granted) =>
    granted !== undefined && granted.epoch === epoch && inboxNow() - granted.at < granted.valid;
  // Stage reports (`handed`, `committed`) no registered watcher has taken
  // yet, or that a watcher took and then exited. One made while no watcher
  // runs, or before a new one has registered, waits here and its attempt
  // answers a resume `retained`; the registered watcher is sent it after the
  // resume answers, and publishes its receipt with the caps the deck rebinds
  // to it. One a watcher that has since exited took is sent again only when
  // the new registration hands back authority to publish it: its attempt is
  // asked back (`resume`), or it is a `committed` report for an attempt the
  // deck lists as `correctable` (a final attempt whose late commit still
  // corrects the sender, sent with its commit cap to the watcher only). Any
  // other is dropped: no correction authority survives for it. Bounded,
  // oldest dropped first.
  // A report another receiver's watcher took counts as taken by a watcher
  // that has exited (`to` is none of this receiver's children).
  const reports = new Map(
    (state?.reports ?? []).map((entry) => [
      entry.stage + "|" + inboxKey(entry.ref),
      { stage: entry.stage, ref: inboxRef(entry.ref), to: entry.taken ? state : undefined },
    ])
  );
  const reported = (key) => reports.has("handed|" + key) || reports.has("committed|" + key);
  const flush = () => {
    if (!child || rx === undefined) return;
    for (const stage of ["handed", "committed"]) {
      const due = [...reports.values()].filter((entry) => entry.stage === stage && entry.to !== child);
      if (!due.length || !inboxWrite(child, { [stage]: due.map((entry) => entry.ref) })) continue;
      for (const entry of due) entry.to = child;
    }
  };
  const report = (stage, refs) => {
    for (const ref of refs) {
      const key = stage + "|" + inboxKey(ref);
      reports.delete(key);
      reports.set(key, { stage, ref: inboxRef(ref), to: undefined });
      if (reports.size > INBOX_MAX_CALLED) reports.delete(reports.keys().next().value);
    }
    flush();
  };
  const grants = new Map();
  let watermark = { seen: 0, above: new Set() };
  let child;
  let epoch = 0;
  let keepaliveMs;
  let rx;
  let seq = 0;
  let q = 0;
  let runs = state ? 1 : 0;
  // A respawn is scheduled and has not started yet.
  let restarting = false;
  const restart = (next, delay) => {
    restarting = true;
    setTimeout(() => {
      restarting = false;
      start(next);
    }, delay).unref?.();
  };
  const observe = (d) => {
    if (typeof d !== "number" || d <= watermark.seen) return;
    watermark.above.add(d);
    while (watermark.above.delete(watermark.seen + 1)) watermark.seen += 1;
  };
  const snapshot = () => {
    if (!child || rx === undefined) return;
    seq += 1;
    inboxWrite(child, {
      possess: { seq, seen: watermark.seen, held: [...held.values()].map(inboxRef) },
    });
  };
  // An attempt a revoke already answered is not held again: a doorbell fetch
  // that issued it can return after the deck revoked it.
  const take = (entry) => {
    observe(entry.d);
    if (!held.has(inboxKey(entry)) && !revoked.has(inboxKey(entry))) {
      held.set(inboxKey(entry), { id: entry.id, a: entry.a, e: entry.e, d: entry.d, stage: "queued" });
    }
    return held.get(inboxKey(entry));
  };
  const answerRevoke = (ref) => {
    const key = inboxKey(ref);
    const handed = handedBefore(key);
    revoked.add(key);
    if (handed) {
      inboxWrite(child, { already_handed: [ref] });
    } else {
      held.delete(key);
      inboxWrite(child, { revoked_ack: [ref] });
    }
  };
  const onLine = (line) => {
    let reply;
    try {
      reply = JSON.parse(line);
    } catch (_) {
      return;
    }
    if (!reply || typeof reply !== "object") return;
    if (reply.registered && typeof reply.registered.rx === "number") {
      rx = reply.registered.rx;
      keepaliveMs = typeof reply.registered.keepalive_ms === "number" ? reply.registered.keepalive_ms : undefined;
      watermark = { seen: 0, above: new Set() };
      const resume = Array.isArray(reply.resume) ? reply.resume : [];
      const asked = new Set(resume.map(inboxKey));
      const correctable = new Set((Array.isArray(reply.correctable) ? reply.correctable : []).map(inboxKey));
      for (const [key, entry] of reports) {
        if (entry.to === undefined) continue;
        const ref = inboxKey(entry.ref);
        if (!asked.has(ref) && !(entry.stage === "committed" && correctable.has(ref))) reports.delete(key);
      }
      const answers = resume.map((ref) => ({
        id: ref.id,
        a: ref.a,
        e: ref.e,
        answer: held.has(inboxKey(ref)) || reported(inboxKey(ref)) ? "retained" : "unknown",
      }));
      if (answers.length) inboxWrite(child, { resume_answers: answers });
      flush();
      snapshot();
    }
    for (const rebound of Array.isArray(reply.rebound) ? reply.rebound : []) {
      const entry = held.get(inboxKey(rebound));
      if (entry) entry.d = rebound.d;
      observe(rebound.d);
    }
    if (reply.grant && typeof reply.grant.q === "number") {
      const resolve = grants.get(reply.grant.q);
      grants.delete(reply.grant.q);
      resolve?.(reply.grant.ok === true);
    }
    for (const ref of Array.isArray(reply.revoke) ? reply.revoke : []) answerRevoke(ref);
    if (Array.isArray(reply.messages)) {
      for (const message of reply.messages) {
        if (!message || typeof message.id !== "string" || typeof message.text !== "string") continue;
        if (typeof message.a !== "number" || typeof message.e !== "number") continue;
        take(message);
      }
      snapshot();
      for (const message of reply.messages) {
        if (message && typeof message.id === "string" && typeof message.a === "number") deliver?.(message);
      }
    }
    if (reply.keepalive || (Array.isArray(reply.rebound) && reply.rebound.length)) snapshot();
  };
  const start = (backoff) => {
    if (!active()) return;
    const started = Date.now();
    const args = ["inbox", "wait", "--agent", agent, "--stream", "--adapter-integration", String(INBOX_ADAPTER_INTEGRATION)];
    if (doorbellOnly) args.push("--doorbell-only");
    if (runs > 0) args.push("--resumed");
    runs += 1;
    epoch += 1;
    rx = undefined;
    for (const [, resolve] of grants) resolve(false);
    grants.clear();
    let spawned;
    try {
      spawned = spawn(hookBinary(), args, { stdio: ["pipe", "pipe", "inherit"] });
    } catch (_) {
      restart(inboxNextBackoff(backoff, 0), backoff);
      return;
    }
    child = spawned;
    let buffer = "";
    spawned.stdout.setEncoding("utf8");
    spawned.stdout.on("data", (chunk) => {
      try {
        buffer += chunk;
        let newline;
        while ((newline = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, newline);
          buffer = buffer.slice(newline + 1);
          if (child === spawned) onLine(line);
        }
      } catch (_) {}
    });
    spawned.stdin.on("error", () => {});
    spawned.on("error", () => {});
    spawned.on("exit", (code) => {
      try {
        if (child === spawned) {
          child = undefined;
          rx = undefined;
        }
        exited?.(code);
        if (!active() || spawned.stdin.writableEnded) return;
        restart(inboxNextBackoff(backoff, Date.now() - started), backoff);
      } catch (_) {}
    });
  };
  return {
    start: () => start(inboxFirstBackoff()),
    // Start a watcher unless one runs or a respawn is already scheduled.
    ensure: () => {
      if (!child && !restarting) start(inboxFirstBackoff());
    },
    // Like `stop`, but resolves once the watcher exited ("exited"), or at
    // the deadline after SIGTERM and then SIGKILL ("forced"): a successor
    // may start only once this watcher can publish nothing more.
    drain: () =>
      new Promise((resolve) => {
        const gone = child;
        child = undefined;
        rx = undefined;
        for (const [, resolve] of grants) resolve(false);
        grants.clear();
        if (!gone || typeof gone.exitCode === "number" || gone.signalCode) {
          resolve("exited");
          return;
        }
        const signal = (name) => {
          try {
            gone.kill(name);
          } catch (_) {}
        };
        const term = setTimeout(() => signal("SIGTERM"), INBOX_STOP_GRACE_MS);
        const kill = setTimeout(() => {
          signal("SIGKILL");
          resolve("forced");
        }, INBOX_STOP_GRACE_MS + 2000);
        term.unref?.();
        kill.unref?.();
        gone.once("exit", () => {
          clearTimeout(term);
          clearTimeout(kill);
          resolve("exited");
        });
        try {
          gone.stdin.end();
        } catch (_) {}
      }),
    // What a successor needs to answer for this receiver's attempts: what it
    // holds, what it called or revoked, and its unpublished stage reports.
    exportState: () => ({
      held: [...held.values()].map((entry) => ({ ...entry })),
      called: [...called],
      forgot,
      revoked: [...revoked],
      reports: [...reports.values()].map((entry) => ({ stage: entry.stage, ref: entry.ref, taken: entry.to !== undefined })),
    }),
    stop: () => {
      const gone = child;
      child = undefined;
      rx = undefined;
      for (const [, resolve] of grants) resolve(false);
      grants.clear();
      inboxStop(gone);
    },
    rx: () => rx,
    epoch: () => epoch,
    send: (control) => inboxWrite(child, control),
    // The attempts a doorbell fetch issued to this receiver.
    take: (messages) => {
      for (const message of messages) take(message);
      snapshot();
    },
    // §3: the daemon's final hand-over check; resolves to the approval (its
    // epoch and when it was asked for), or undefined when it was refused or
    // went unanswered.
    may: async (ref) => {
      if (!child || rx === undefined) return undefined;
      const asked = approval();
      q += 1;
      const id = q;
      const answered = new Promise((resolve) => {
        grants.set(id, resolve);
        const timer = setTimeout(() => {
          if (grants.delete(id)) resolve(false);
        }, INBOX_GRANT_TIMEOUT_MS);
        timer.unref?.();
      });
      if (!inboxWrite(child, { may: { q: id, ...ref } })) {
        grants.delete(id);
        return undefined;
      }
      return (await answered) ? asked : undefined;
    },
    // An approval the deck gives in this synchronous step: a doorbell fetch
    // issues its attempts under the deck's lock.
    approval: () => approval(),
    // §2: whether a callback holding the approval `granted` may reach
    // OpenCode right now. Synchronous: call it with no await between it and
    // the native call. One native call per attempt: once a callback has
    // called, every other approval of the attempt is refused, so a call's
    // outcome is the attempt's.
    ready: (ref, granted) => {
      const key = inboxKey(ref);
      const entry = held.get(key);
      return active() && fresh(granted) && entry !== undefined &&
        entry.stage !== "handed" && !called.has(key) && !revoked.has(key);
    },
    // The callback is about to reach OpenCode: from here a revoke is
    // answered `already_handed`.
    calling: (ref) => {
      const entry = held.get(inboxKey(ref));
      if (entry) entry.stage = "handed";
      call(inboxKey(ref));
    },
    handed: (refs) => {
      if (refs.length) report("handed", refs);
    },
    // The agent started on these: no longer held, and possibly handed for
    // good, so a revoke from now on is answered `already_handed`. Their
    // receipts are handed to the watcher after this returns (to the next one
    // to register, if none is registered now), which allows bounded time for
    // publication; a stop can still lose one, or it is reported degraded.
    committed: (refs) => {
      if (!refs.length) return;
      for (const ref of refs) {
        held.delete(inboxKey(ref));
        call(inboxKey(ref));
      }
      report("committed", refs);
      snapshot();
    },
    // Positively not handed to OpenCode by this callback. A refusal is only
    // a refusal: while another approved callback of the attempt is inside
    // its call, that call reports for itself. Otherwise the attempt is
    // revoked locally first, so no approval still outstanding for it can
    // reach OpenCode once the deck believes it was not handed.
    notHanded: (ref) => {
      const key = inboxKey(ref);
      if (handedBefore(key)) return;
      revoked.add(key);
      held.delete(key);
      inboxWrite(child, { not_handed: [ref] });
      snapshot();
    },
    // OpenCode refused the call this callback made before accepting it
    // (`inboxRefusedBeforeAcceptance`). It was the attempt's only call
    // (`ready`), so the attempt was positively not handed: revoked locally
    // first, like `notHanded`, so no approval still outstanding for it can
    // reach OpenCode once the deck believes it was not handed.
    rejected: (ref) => {
      const key = inboxKey(ref);
      revoked.add(key);
      called.delete(key);
      held.delete(key);
      inboxWrite(child, { not_handed: [ref] });
      snapshot();
    },
    // Lost without a witness — an aborted turn, a reverted prompt, a call
    // that failed after it began: no longer held, so the deck may repeat it,
    // marked as a possible repeat. What was handed stays possibly handed.
    lost: (ref) => {
      if (held.get(inboxKey(ref))?.stage === "handed") call(inboxKey(ref));
      if (held.delete(inboxKey(ref))) snapshot();
    },
    // The held attempts of deck message `id`.
    of: (id) => [...held.values()].filter((entry) => entry.id === id).map(inboxRef),
    // The attempts handed to OpenCode in `sessionID` and not yet started on.
    inSession: (sessionID) =>
      [...held.values()].filter((entry) => entry.stage === "handed" && entry.sessionID === sessionID).map(inboxRef),
    // Remember which session an attempt was handed to.
    session: (ref, sessionID) => {
      const entry = held.get(inboxKey(ref));
      if (entry) entry.sessionID = sessionID;
    },
  };
};
