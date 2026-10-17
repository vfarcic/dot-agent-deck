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
//! the new watcher with its caps, so its receipts are still published; a
//! stage report for an attempt whose cap this watcher lacks publishes nothing
//! and says so, as a degraded report, rather than passing for a receipt.
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

/// TEST knob: every receipt publish waits this many milliseconds first, so a
/// test can hold one in flight across keepalives.
pub const ENV_TEST_PUBLISH_DELAY_MS: &str = "DOT_AGENT_DECK_INBOX_TEST_PUBLISH_DELAY_MS";

/// TEST knob: the watcher publishes each `committed` receipt but never
/// forwards the control, as if its stream died between the two, so a test can
/// show the daemon settling from the spool alone.
pub const ENV_TEST_DROP_COMMITTED: &str = "DOT_AGENT_DECK_INBOX_TEST_DROP_COMMITTED";

#[cfg(not(unix))]
pub fn wait_receipt_stream(_json: &str) -> std::process::ExitCode {
    eprintln!("inbox wait --stream is not supported on this platform.");
    std::process::ExitCode::FAILURE
}

#[cfg(unix)]
pub use unix::wait_receipt_stream;

#[cfg(unix)]
mod unix {
    use std::collections::{BTreeMap, HashMap};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{ENV_TEST_DROP_COMMITTED, ENV_TEST_PUBLISH_DELAY_MS};
    use crate::event::{InboxAttemptRef, InboxStreamControl, InboxWaitReply};
    use crate::inbox_receipt::{Cap, GenerationSpool, RecordKey, RecordStage};

    /// Keepalives without a snapshot after which the component counts as
    /// dead.
    const SNAPSHOT_KEEPALIVES: u32 = crate::inbox_attempts::KEEPALIVES_PER_POSSESS_TTL;

    type Key = (String, u32, u32);
    type SpoolOrError = Result<Arc<GenerationSpool>, String>;

    #[derive(Default)]
    struct Shared {
        caps: HashMap<Key, (Option<Cap>, Option<Cap>)>,
        /// Publications still being written, per attempt: one per stage
        /// worker, so an attempt stays in every snapshot until the last of
        /// its overlapping `handed` and `committed` publishes is done.
        in_flight: BTreeMap<Key, u32>,
        last_snapshot: Option<Instant>,
        keepalive: Option<Duration>,
        spool: Option<Result<Arc<GenerationSpool>, String>>,
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

    /// Publish `stage`'s receipt for each attempt, with the attempt's own
    /// cap. The first failure is returned. A cap this watcher was never handed
    /// is a failure too: the receipt it would have protected is not there,
    /// so the generation must read as degraded rather than protected.
    fn publish(
        shared: &Mutex<Shared>,
        stage: RecordStage,
        attempts: &[InboxAttemptRef],
    ) -> Option<String> {
        let delay = std::env::var(ENV_TEST_PUBLISH_DELAY_MS)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok());
        if let Some(ms) = delay {
            std::thread::sleep(Duration::from_millis(ms));
        }
        let (spool, caps): (SpoolOrError, Vec<(Key, Option<Cap>)>) = {
            let shared = shared.lock().unwrap_or_else(|e| e.into_inner());
            let spool = match &shared.spool {
                Some(Ok(spool)) => Ok(Arc::clone(spool)),
                Some(Err(error)) => Err(error.clone()),
                None => Err("the receipt spool is not open".to_string()),
            };
            let caps = attempts
                .iter()
                .map(|at| {
                    let k = key(at);
                    let cap = shared
                        .caps
                        .get(&k)
                        .and_then(|(handed, commit)| match stage {
                            RecordStage::Handed => handed.clone(),
                            RecordStage::Committed => commit.clone(),
                        });
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
        let shared = Arc::new(Mutex::new(Shared::default()));
        // The component's controls. Its closing our stdin is how it says it
        // is done, so the watcher then exits.
        {
            let writer = Arc::clone(&writer);
            let shared = Arc::clone(&shared);
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
                    if let Some(possess) = control.possess.as_mut() {
                        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
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
                        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
                        for at in handed.iter().chain(&committed) {
                            *shared.in_flight.entry(key(at)).or_default() += 1;
                        }
                    }
                    let writer = Arc::clone(&writer);
                    let shared = Arc::clone(&shared);
                    std::thread::spawn(move || {
                        let mut degraded = None;
                        if !handed.is_empty() {
                            degraded = publish(&shared, RecordStage::Handed, &handed);
                        }
                        if !committed.is_empty() {
                            degraded =
                                publish(&shared, RecordStage::Committed, &committed).or(degraded);
                        }
                        let dropped = std::env::var_os(ENV_TEST_DROP_COMMITTED).is_some();
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
                        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
                        for at in handed.iter().chain(&committed) {
                            let k = key(at);
                            if let Some(count) = shared.in_flight.get_mut(&k) {
                                *count -= 1;
                                if *count == 0 {
                                    shared.in_flight.remove(&k);
                                }
                            }
                        }
                    });
                }
                std::process::exit(0);
            });
        }
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
                    let shared = shared.lock().unwrap_or_else(|e| e.into_inner());
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
                let spool = open_spool();
                let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
                shared.keepalive = Some(Duration::from_millis(registered.keepalive_ms.max(1)));
                shared.last_snapshot = Some(Instant::now());
                shared.spool = Some(spool);
            }
            let caps_of = |caps: &crate::event::InboxAttemptCaps| {
                (Cap::from_hex(&caps.handed), Cap::from_hex(&caps.commit))
            };
            if !reply.issued.is_empty() {
                let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
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
            if !reply.rebound.is_empty() {
                let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
                for rebound in reply.rebound.iter_mut() {
                    if let Some(caps) = rebound.caps.take() {
                        shared
                            .caps
                            .insert((rebound.id.clone(), rebound.a, rebound.e), caps_of(&caps));
                    }
                }
            }
            if let Some(messages) = reply.messages.as_mut() {
                let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
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
}
