//! PRD #1666 design v3.2 §3–§5: the watcher between an agent-side component
//! and the daemon's receipt stream — `dot-agent-deck inbox wait --stream
//! --adapter-integration N`, run by the OpenCode plugins.
//!
//! The component sees each hand-out as `{id, a, e, d}` and its framed text;
//! the attempt's two caps stay here. When the component says it handed an
//! attempt to its agent (`handed`) or that the agent started on it
//! (`committed`), this watcher publishes the matching receipt into the
//! generation's spool **before** it tells the daemon, so a stream that dies
//! between the two still leaves the receipt for the daemon's lookup to find.
//! A publish still in flight is added to every possession snapshot the
//! component sends meanwhile, so a slow spool write never reads as absence.
//! An attempt the component retained across a watcher restart is rebound to
//! the new watcher with its caps, so its receipts are still published: a
//! stage report that comes before this watcher has read its registration, or
//! before the daemon's `rebound` answer for its attempt, waits for them, the
//! attempt held meanwhile. A stage report for an attempt whose cap this
//! watcher lacks once registered (and rebound) publishes nothing and says so,
//! as a degraded report, rather than passing for a receipt, and so does one
//! whose watcher's registration does not come within the wait. Told to stop
//! (its stdin ends), or once its stream ends, the watcher allows bounded time
//! for the publications still in flight before it exits; a stage reported at
//! shutdown can still be lost or reported degraded.
//!
//! Every other control (`may`, `not_handed`, revoke answers, resume answers)
//! is forwarded on this authenticated connection only: a spool record is
//! bearer evidence and carries just the two stages whose meaning holds
//! whoever bears the cap.
//!
//! The daemon's keepalives are forwarded too, and the component answers each
//! with a fresh snapshot. A component that sends none for three keepalives is
//! taken as dead (§4 "Dead adapter, live watcher"): the watcher closes the
//! stream, and the daemon stops counting the component as holding anything.

/// TEST knob, read only by a build with the `e2e` feature: every receipt
/// publish waits this many milliseconds first, so a test can hold one in
/// flight across keepalives.
pub const ENV_TEST_PUBLISH_DELAY_MS: &str = "DOT_AGENT_DECK_INBOX_TEST_PUBLISH_DELAY_MS";

/// TEST knob, read only by a build with the `e2e` feature: the watcher
/// publishes each `committed` receipt but never forwards the control, as if
/// its stream died between the two, so a test can show the daemon settling
/// from the spool alone.
pub const ENV_TEST_DROP_COMMITTED: &str = "DOT_AGENT_DECK_INBOX_TEST_DROP_COMMITTED";

/// TEST knob, read only by a build with the `e2e` feature: the caps of a
/// `rebound` answer reach the watcher's publisher this many milliseconds
/// late, while the line itself goes on to the component at once, so a test
/// can report a retained attempt's stage inside that window.
pub const ENV_TEST_REBIND_DELAY_MS: &str = "DOT_AGENT_DECK_INBOX_TEST_REBIND_DELAY_MS";

/// TEST knob, read only by a build with the `e2e` feature: the watcher reads
/// the daemon's `registered` answer this many milliseconds late, so a test can
/// report a stage to a replacement watcher before it has registered.
pub const ENV_TEST_REGISTER_DELAY_MS: &str = "DOT_AGENT_DECK_INBOX_TEST_REGISTER_DELAY_MS";

/// Every TEST knob above. A spawned agent inherits none of them from the
/// daemon's environment (`agent_pty`); a test sets one explicitly.
pub const TEST_KNOBS: [&str; 4] = [
    ENV_TEST_PUBLISH_DELAY_MS,
    ENV_TEST_DROP_COMMITTED,
    ENV_TEST_REBIND_DELAY_MS,
    ENV_TEST_REGISTER_DELAY_MS,
];

#[cfg(not(unix))]
pub fn wait_receipt_stream(_json: &str) -> std::process::ExitCode {
    eprintln!("inbox wait --stream is not supported on this platform.");
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
pub use unix::wait_receipt_stream;

#[cfg(unix)]
mod unix {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::ExitCode;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use crate::event::{InboxAttemptCaps, InboxAttemptRef, InboxStreamControl, InboxWaitReply};
    use crate::inbox_receipt::{Cap, GenerationSpool, RecordKey, RecordStage};

    /// Keepalives without a snapshot after which the component counts as
    /// dead.
    const SNAPSHOT_KEEPALIVES: u32 = crate::inbox_attempts::KEEPALIVES_PER_POSSESS_TTL;

    /// How long a stage report waits for the authority to publish it: this
    /// watcher's registration, which opens the spool and names the attempts
    /// being rebound, and for an attempt being rebound the caps the daemon's
    /// `rebound` answer brings. The daemon answers a `retained` resume at
    /// once; one it no longer rebinds (its resume window ran out first) never
    /// answers, and the report is then degraded. A report whose watcher's
    /// registration does not come within this wait is degraded too: with no
    /// spool open there is nothing to publish into. Once the stream has ended
    /// neither wait goes on, since nothing will arrive on it any more.
    const REBIND_WAIT: Duration = Duration::from_secs(5);

    /// How long a watcher that was told to stop, or whose stream ended, waits
    /// for its publications in flight before it exits. The OpenCode plugin
    /// sends SIGTERM only after a longer grace (`INBOX_STOP_GRACE_MS`).
    const DRAIN_WAIT: Duration = Duration::from_secs(2);

    type Key = (String, u32, u32);
    type SpoolOrError = Result<Arc<GenerationSpool>, String>;

    #[cfg(feature = "e2e")]
    fn test_ms(knob: &str) -> Option<u64> {
        std::env::var(knob)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
    }

    #[cfg(not(feature = "e2e"))]
    fn test_ms(_knob: &str) -> Option<u64> {
        None
    }

    #[cfg(feature = "e2e")]
    fn test_drops_committed() -> bool {
        std::env::var_os(super::ENV_TEST_DROP_COMMITTED).is_some()
    }

    #[cfg(not(feature = "e2e"))]
    fn test_drops_committed() -> bool {
        false
    }

    /// The watcher's state, shared by its threads; `rebound` is signalled
    /// whenever what a publication waits for changes: the registration is
    /// read, an attempt's rebinding ends, a publication finishes or the
    /// stream ends.
    #[derive(Default)]
    struct Watch {
        shared: Mutex<Shared>,
        rebound: Condvar,
    }

    impl Watch {
        fn lock(&self) -> MutexGuard<'_, Shared> {
            self.shared.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// A rebound attempt's caps arrived (or the daemon had none): its
        /// stage reports go ahead.
        fn rebind(&self, key: Key, caps: Option<&InboxAttemptCaps>) {
            let mut shared = self.lock();
            if let Some(caps) = caps {
                shared.caps.insert(key.clone(), caps_of(caps));
            }
            shared.rebinding.remove(&key);
            drop(shared);
            self.rebound.notify_all();
        }

        /// The stream is over: nothing waits for a registration any longer.
        fn end(&self) {
            self.lock().ended = true;
            self.rebound.notify_all();
        }

        /// Wait, up to [`DRAIN_WAIT`], for every publication in flight to be
        /// written and reported.
        fn drain(&self) {
            let deadline = Instant::now() + DRAIN_WAIT;
            let mut shared = self.lock();
            while !shared.in_flight.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                shared = self
                    .rebound
                    .wait_timeout(shared, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
    }

    /// Ends the stream and drains the publications in flight when the
    /// daemon's side of the watcher returns, by whichever path.
    struct Ending<'a>(&'a Watch);

    impl Drop for Ending<'_> {
        fn drop(&mut self) {
            self.0.end();
            self.0.drain();
        }
    }

    fn caps_of(caps: &InboxAttemptCaps) -> (Option<Cap>, Option<Cap>) {
        (Cap::from_hex(&caps.handed), Cap::from_hex(&caps.commit))
    }

    #[derive(Default)]
    struct Shared {
        caps: HashMap<Key, (Option<Cap>, Option<Cap>)>,
        /// Attempts the daemon asked this watcher's component to resume, whose
        /// `rebound` answer (and caps) has not arrived yet.
        rebinding: HashSet<Key>,
        /// Publications still being written, per attempt: one per stage
        /// worker, so an attempt stays in every snapshot until the last of
        /// its overlapping `handed` and `committed` publishes is done.
        in_flight: BTreeMap<Key, u32>,
        last_snapshot: Option<Instant>,
        keepalive: Option<Duration>,
        /// Set once this watcher has read its registration: until then there
        /// is no authority to publish with yet, which is not the same as none.
        spool: Option<Result<Arc<GenerationSpool>, String>>,
        /// The stream ended, so a registration will not come any more.
        ended: bool,
    }

    fn key(at: &InboxAttemptRef) -> Key {
        (at.id.clone(), at.a, at.e)
    }

    fn write_line(writer: &Mutex<UnixStream>, line: &str) -> std::io::Result<()> {
        let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(format!("{line}\n").as_bytes())?;
        writer.flush()
    }

    fn open_spool() -> Result<Arc<GenerationSpool>, String> {
        let path = std::env::var_os(crate::inbox_receipt::ENV_INBOX_SPOOL)
            .ok_or_else(|| "the deck did not say where its receipt spool is".to_string())?;
        let pane = std::env::var(crate::agent_pty::DOT_AGENT_DECK_PANE_ID)
            .map_err(|_| "no pane id".to_string())?;
        let agent = std::env::var(crate::agent_pty::DOT_AGENT_DECK_AGENT_ID)
            .map_err(|_| "no agent id".to_string())?;
        GenerationSpool::open(
            std::path::Path::new(&path),
            &crate::inbox::encode_token(&pane),
            &crate::inbox::encode_token(&agent),
        )
        .map(Arc::new)
        .map_err(|error| error.to_string())
    }

    fn cap(shared: &Shared, key: &Key, stage: RecordStage) -> Option<Cap> {
        shared
            .caps
            .get(key)
            .and_then(|(handed, commit)| match stage {
                RecordStage::Handed => handed.clone(),
                RecordStage::Committed => commit.clone(),
            })
    }

    /// Publish `stage`'s receipt for each attempt, with the attempt's own
    /// cap. Before this watcher has read its registration, and for an attempt
    /// still being rebound, it waits, up to [`REBIND_WAIT`], for the spool and
    /// the caps its `rebound` answer brings; its caller keeps it in flight, so
    /// held, meanwhile. The first failure is returned. A cap this watcher
    /// still lacks is a failure too: the receipt it would have protected is
    /// not there, so the generation must read as degraded rather than
    /// protected.
    fn publish(watch: &Watch, stage: RecordStage, attempts: &[InboxAttemptRef]) -> Option<String> {
        if let Some(ms) = test_ms(super::ENV_TEST_PUBLISH_DELAY_MS) {
            std::thread::sleep(Duration::from_millis(ms));
        }
        let (spool, caps): (SpoolOrError, Vec<(Key, Option<Cap>)>) = {
            let mut shared = watch.lock();
            let deadline = Instant::now() + REBIND_WAIT;
            loop {
                let unregistered = shared.spool.is_none() && !shared.ended;
                let waiting = unregistered
                    || (!shared.ended
                        && attempts.iter().any(|at| {
                            let k = key(at);
                            cap(&shared, &k, stage).is_none() && shared.rebinding.contains(&k)
                        }));
                let left = deadline.saturating_duration_since(Instant::now());
                if !waiting || left.is_zero() {
                    break;
                }
                shared = watch
                    .rebound
                    .wait_timeout(shared, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            let spool = match &shared.spool {
                Some(Ok(spool)) => Ok(Arc::clone(spool)),
                Some(Err(error)) => Err(error.clone()),
                None => Err("the receipt spool is not open".to_string()),
            };
            let caps = attempts
                .iter()
                .map(|at| {
                    let k = key(at);
                    let cap = cap(&shared, &k, stage);
                    (k, cap)
                })
                .collect();
            (spool, caps)
        };
        let spool = match spool {
            Ok(spool) => spool,
            Err(error) => return Some(error),
        };
        let mut failed = None;
        for ((msg, a, e), cap) in caps {
            let Some(cap) = cap else {
                failed.get_or_insert_with(|| {
                    format!(
                        "no receipt cap for attempt {a}.{e} of {}",
                        crate::untrusted_text::display_line(&msg, 80)
                    )
                });
                continue;
            };
            let record = RecordKey { msg, a, e, stage };
            match spool.publish(&record, &cap) {
                Ok(()) => {}
                // Published already: a duplicate stage report.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    failed.get_or_insert_with(|| error.to_string());
                }
            }
        }
        failed
    }

    pub fn wait_receipt_stream(json: &str) -> ExitCode {
        let path = crate::endpoint_resolve::client_socket_path();
        let Ok(socket) = UnixStream::connect(&path) else {
            return ExitCode::SUCCESS;
        };
        let Ok(writer) = socket.try_clone() else {
            return ExitCode::SUCCESS;
        };
        let Ok(closer) = socket.try_clone() else {
            return ExitCode::SUCCESS;
        };
        let writer = Arc::new(Mutex::new(writer));
        if write_line(&writer, json).is_err() {
            return ExitCode::SUCCESS;
        }
        let watch = Arc::new(Watch::default());
        // The component's controls. Its closing our stdin is how it says it
        // is done, so the watcher then exits.
        {
            let writer = Arc::clone(&writer);
            let watch = Arc::clone(&watch);
            std::thread::spawn(move || {
                let stdin = std::io::stdin();
                for line in stdin.lock().lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(mut control) = serde_json::from_str::<InboxStreamControl>(line) else {
                        continue;
                    };
                    let handed = std::mem::take(&mut control.handed);
                    let committed = std::mem::take(&mut control.committed);
                    // An attempt the component did not retain is not
                    // rebound: nothing waits for its caps.
                    for answer in &control.resume_answers {
                        if answer.answer != "retained" {
                            watch.rebind((answer.id.clone(), answer.a, answer.e), None);
                        }
                    }
                    if let Some(possess) = control.possess.as_mut() {
                        let mut shared = watch.lock();
                        shared.last_snapshot = Some(Instant::now());
                        for (id, a, e) in shared.in_flight.keys() {
                            if !possess
                                .held
                                .iter()
                                .any(|h| &h.id == id && h.a == *a && h.e == *e)
                            {
                                possess.held.push(InboxAttemptRef {
                                    id: id.clone(),
                                    a: *a,
                                    e: *e,
                                });
                            }
                        }
                    }
                    if control != InboxStreamControl::default()
                        && let Ok(text) = serde_json::to_string(&control)
                        && write_line(&writer, &text).is_err()
                    {
                        break;
                    }
                    if handed.is_empty() && committed.is_empty() {
                        continue;
                    }
                    {
                        let mut shared = watch.lock();
                        for at in handed.iter().chain(&committed) {
                            *shared.in_flight.entry(key(at)).or_default() += 1;
                        }
                    }
                    let writer = Arc::clone(&writer);
                    let watch = Arc::clone(&watch);
                    std::thread::spawn(move || {
                        let mut degraded = None;
                        if !handed.is_empty() {
                            degraded = publish(&watch, RecordStage::Handed, &handed);
                        }
                        if !committed.is_empty() {
                            degraded =
                                publish(&watch, RecordStage::Committed, &committed).or(degraded);
                        }
                        let dropped = test_drops_committed();
                        let stages = InboxStreamControl {
                            handed: handed.clone(),
                            committed: if dropped {
                                Vec::new()
                            } else {
                                committed.clone()
                            },
                            degraded,
                            ..InboxStreamControl::default()
                        };
                        if stages != InboxStreamControl::default()
                            && let Ok(text) = serde_json::to_string(&stages)
                        {
                            let _ = write_line(&writer, &text);
                        }
                        let mut shared = watch.lock();
                        for at in handed.iter().chain(&committed) {
                            let k = key(at);
                            if let Some(count) = shared.in_flight.get_mut(&k) {
                                *count -= 1;
                                if *count == 0 {
                                    shared.in_flight.remove(&k);
                                }
                            }
                        }
                        drop(shared);
                        watch.rebound.notify_all();
                    });
                }
                // Told to stop: finish what is in flight first.
                watch.drain();
                std::process::exit(0);
            });
        }
        let _ending = Ending(&watch);
        let reader = BufReader::new(socket);
        let mut stdout = std::io::stdout().lock();
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            let Ok(mut reply) = serde_json::from_str::<InboxWaitReply>(&line) else {
                continue;
            };
            if reply.keepalive {
                let dead = {
                    let shared = watch.lock();
                    match (shared.keepalive, shared.last_snapshot) {
                        (Some(keepalive), Some(last)) => {
                            last.elapsed() >= keepalive * SNAPSHOT_KEEPALIVES
                        }
                        _ => false,
                    }
                };
                if dead {
                    // §4: the component stopped answering. Closing the
                    // stream ends its incarnation; nothing it holds counts
                    // as possessed any longer.
                    let _ = closer.shutdown(std::net::Shutdown::Both);
                    return ExitCode::SUCCESS;
                }
            }
            if let Some(registered) = &reply.registered {
                if let Some(ms) = test_ms(super::ENV_TEST_REGISTER_DELAY_MS) {
                    std::thread::sleep(Duration::from_millis(ms));
                }
                let spool = open_spool();
                let mut shared = watch.lock();
                shared.keepalive = Some(Duration::from_millis(registered.keepalive_ms.max(1)));
                shared.last_snapshot = Some(Instant::now());
                shared.spool = Some(spool);
                shared.rebinding = reply.resume.iter().map(key).collect();
                drop(shared);
                watch.rebound.notify_all();
            }
            if !reply.issued.is_empty() {
                let mut shared = watch.lock();
                for issued in std::mem::take(&mut reply.issued) {
                    shared
                        .caps
                        .insert((issued.id, issued.a, issued.e), caps_of(&issued.caps));
                }
                if reply == InboxWaitReply::default() {
                    continue;
                }
            }
            let gone = reply.messages.is_none()
                && !reply.keepalive
                && reply.registered.is_none()
                && reply.resume.is_empty()
                && reply.rebound.is_empty()
                && reply.grant.is_none()
                && reply.revoke.is_empty();
            if gone {
                // Superseded, refused or not in a deck: nothing more will come.
                let Ok(text) = serde_json::to_string(&reply) else {
                    break;
                };
                let _ = writeln!(stdout, "{text}").and_then(|()| stdout.flush());
                break;
            }
            // §4, §5: a retained attempt rebound to this watcher comes with its
            // caps, so its receipts can still be published from here.
            for rebound in reply.rebound.iter_mut() {
                let key = (rebound.id.clone(), rebound.a, rebound.e);
                let caps = rebound.caps.take();
                match test_ms(super::ENV_TEST_REBIND_DELAY_MS) {
                    Some(ms) => {
                        let watch = Arc::clone(&watch);
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(ms));
                            watch.rebind(key, caps.as_ref());
                        });
                    }
                    None => watch.rebind(key, caps.as_ref()),
                }
            }
            if let Some(messages) = reply.messages.as_mut() {
                let mut shared = watch.lock();
                for message in messages.iter_mut() {
                    if let Some(attempt) = message.attempt.as_mut()
                        && let Some(caps) = attempt.caps.take()
                    {
                        shared
                            .caps
                            .insert((message.id.clone(), attempt.a, attempt.e), caps_of(&caps));
                    }
                }
            }
            let Ok(text) = serde_json::to_string(&reply) else {
                continue;
            };
            if writeln!(stdout, "{text}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return ExitCode::SUCCESS;
            }
        }
        ExitCode::SUCCESS
    }

    #[cfg(test)]
    mod rebind_wait_tests {
        use super::*;
        use spec::spec;

        /// Scenario: A registered watcher whose spool could not open is still waiting for a rebound attempt's caps when its stream ends. The commit report it is publishing stops waiting at once and comes back as a failure, instead of holding the watcher's exit for the whole five-second rebinding wait.
        #[spec("inbox/opencode/051")]
        #[test]
        fn opencode_051_a_rebinding_wait_stops_when_the_stream_ends() {
            let watch = Arc::new(Watch::default());
            let attempt = InboxAttemptRef {
                id: "m-ended".into(),
                a: 1,
                e: 1,
            };
            {
                let mut shared = watch.lock();
                shared.spool = Some(Err("no spool in this test".into()));
                shared.rebinding.insert(key(&attempt));
            }
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let publisher = {
                let watch = Arc::clone(&watch);
                std::thread::spawn(move || {
                    let started = Instant::now();
                    started_tx.send(()).unwrap();
                    let failed = publish(&watch, RecordStage::Committed, &[attempt]);
                    (failed, started.elapsed())
                })
            };
            started_rx.recv().unwrap();
            // The stream ends while the report waits for the caps: the
            // publisher is held for this long first, so a return before it
            // would mean it never waited at all.
            let held = Duration::from_millis(300);
            std::thread::sleep(held);
            watch.end();
            let (failed, elapsed) = publisher.join().unwrap();
            assert!(
                elapsed >= held,
                "precondition: the report waited for the caps ({elapsed:?})"
            );
            assert!(
                elapsed < Duration::from_secs(2),
                "the rebinding wait outlived the stream's end by {elapsed:?}"
            );
            assert!(
                failed.is_some(),
                "a report whose wait ended unanswered is a failure, not a receipt"
            );
        }
    }

    #[cfg(all(test, not(feature = "e2e")))]
    mod tests {
        /// Scenario: A build without the `e2e` feature runs with every TEST knob set in its environment. It reads none of them: no publish delay, no rebind or registration delay, and committed controls are forwarded.
        #[test]
        fn a_build_without_e2e_reads_no_test_knob() {
            // SAFETY: nextest runs each test in its own process, and no
            // other code in this one reads these variables.
            unsafe {
                for knob in super::super::TEST_KNOBS {
                    std::env::set_var(knob, "1");
                }
            }
            let read = (
                super::test_ms(super::super::ENV_TEST_PUBLISH_DELAY_MS),
                super::test_ms(super::super::ENV_TEST_REBIND_DELAY_MS),
                super::test_ms(super::super::ENV_TEST_REGISTER_DELAY_MS),
                super::test_drops_committed(),
            );
            // SAFETY: as above.
            unsafe {
                for knob in super::super::TEST_KNOBS {
                    std::env::remove_var(knob);
                }
            }
            assert_eq!(read, (None, None, None, false), "a knob was read");
        }
    }
}
