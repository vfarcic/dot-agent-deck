//! PRD #1666 part 3, round 5-wake (plan §10, commit 5): the doorbell
//! transaction's pure model (`src/doorbell_txn.rs`), driven by a model editor
//! and a clock the test moves. Nothing here touches a PTY, a lock or the
//! daemon: activation (commit 6) runs the same transaction against the real
//! writer.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dot_agent_deck::doorbell_txn::{
    Action, Admission, AttemptCounter, AttemptRing, Caps, Cause, ComposerView, Event, Frame, Job,
    Latch, LatchCell, NotTyped, Outcome, Publisher, Resolution, RingToken, Seam, SeamRead, Shown,
    TXN_MAX, Txn, admit, hook_evidence, pick_token, project, ring_entry, seam_until,
};
use spec::spec;

const ID: &str = "m-k3xzq2ab-0f3c9a71d2e4b850";

/// What the model editor does with the deck's Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enter {
    /// Submits the composer: a user message in the transcript, the composer
    /// empty, and the prompt hook's report when the agent has one.
    Submit,
    /// Drops it: the composer still holds the line.
    Drop,
    /// Keeps it as a newline in the composer.
    Newline,
}

/// What the writer thread does with the next job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Write {
    Whole,
    /// Takes only this many bytes, then fails.
    Partial(usize),
    /// Takes every byte, then the flush fails.
    FlushError,
    /// Starts and never finishes.
    Stall,
    /// Never starts while the transaction runs.
    NeverStarts,
}

type Change = Box<dyn FnOnce(&mut World)>;

/// A change to the world made with the line in the composer.
type LineChange = Box<dyn Fn(&mut World, &str)>;

/// A model agent and the daemon around one transaction.
struct World {
    now: Instant,
    caps: Caps,
    composer: String,
    /// What the screen shows of the composer, when it lags the editor.
    stale: Option<String>,
    cursor_at_end: bool,
    selection: bool,
    dialog: Option<&'static str>,
    /// Bytes a dialog received.
    dialog_took: Vec<String>,
    user_cells: Vec<String>,
    epoch: u64,
    clean: bool,
    enter: Enter,
    hook: bool,
    writes: VecDeque<Write>,
    /// How long each job takes to finish.
    latency: Duration,
    watermark: u64,
    job_mark: u64,
    mode: Result<(), &'static str>,
    open: bool,
    contended: bool,
    /// Everything that reached the pane, in order: the deck's jobs and the
    /// human frames.
    pane: Vec<String>,
    /// Human frames waiting on the writer.
    human: Vec<String>,
    /// A started deck job still running when the writer was released.
    running: Option<String>,
    /// Changes made just before the n-th job (0-based) is applied.
    before_job: Vec<(usize, Change)>,
    /// Changes made once the clock reaches an offset from the start.
    at: Vec<(Duration, Change)>,
    /// A change made after admission and before the first seam.
    after_admission: Option<Change>,
    jobs: usize,
    start: Instant,
    cell: Arc<LatchCell>,
    ring: Arc<AttemptRing>,
    counter: Arc<AttemptCounter>,
    events: VecDeque<Event>,
    released_at: Option<Instant>,
    /// The cell when the writer was released.
    cell_at_release: Option<Latch>,
}

fn caps() -> Caps {
    Caps {
        delete_key: true,
        placeholder: true,
        hook_route: true,
        transcript_route: true,
        staged_head: None,
    }
}

impl World {
    fn new() -> Self {
        let now = Instant::now();
        World {
            now,
            caps: caps(),
            composer: String::new(),
            stale: None,
            cursor_at_end: true,
            selection: false,
            dialog: None,
            dialog_took: Vec::new(),
            user_cells: Vec::new(),
            epoch: 1,
            clean: true,
            enter: Enter::Submit,
            hook: true,
            writes: VecDeque::new(),
            latency: Duration::from_millis(5),
            watermark: 10,
            job_mark: 4,
            mode: Ok(()),
            open: true,
            contended: false,
            pane: Vec::new(),
            human: Vec::new(),
            running: None,
            before_job: Vec::new(),
            at: Vec::new(),
            after_admission: None,
            jobs: 0,
            start: now,
            cell: Arc::default(),
            ring: Arc::default(),
            counter: Arc::default(),
            events: VecDeque::new(),
            released_at: None,
            cell_at_release: None,
        }
    }

    fn frame(&self) -> Frame {
        let text = self.stale.clone().unwrap_or_else(|| self.composer.clone());
        let composer = self.dialog.is_none().then_some(ComposerView {
            placeholder: text.is_empty() && self.caps.placeholder,
            text,
            cursor_at_end: self.cursor_at_end,
            selection: self.selection,
        });
        Frame {
            epoch: self.epoch,
            clean: self.clean,
            dialog: self.dialog,
            composer,
            user_cells: self.user_cells.clone(),
            text: self.user_cells.join("\n"),
        }
    }

    fn admission(&self) -> Admission {
        Admission {
            activation: Ok(()),
            mode: self.mode,
            ledger_idle: true,
            watermark: self.watermark,
            job_mark: self.job_mark,
            frame: self.frame(),
            nudge_in_flight: false,
            settling: false,
            submit_route: true,
            still_admitted: true,
        }
    }

    /// A human frame admitted while the deck holds the writer: it moves the
    /// watermark and waits.
    fn human_types(&mut self, text: &str) {
        self.watermark += 1;
        self.human.push(text.to_string());
    }

    fn type_into_composer(&mut self, text: &str) {
        if self.dialog.is_some() {
            self.dialog_took.push(text.to_string());
            return;
        }
        for ch in text.chars() {
            match ch {
                '\r' => match self.enter {
                    Enter::Submit => {
                        let line = std::mem::take(&mut self.composer);
                        if self.hook
                            && let Some(token) = line
                                .strip_prefix('(')
                                .and_then(|rest| rest.get(..6))
                                .and_then(RingToken::parse)
                        {
                            let exact = token.line(ID) == line;
                            hook_evidence(&self.cell, &self.ring, token, exact);
                            self.events.push_back(Event::Evidence { token, exact });
                        }
                        self.user_cells.push(line);
                    }
                    Enter::Drop => {}
                    Enter::Newline => self.composer.push('\n'),
                },
                '\u{7f}' => {
                    self.composer.pop();
                }
                ch => self.composer.push(ch),
            }
        }
    }

    fn apply(&mut self, job: &Job) {
        let n = self.jobs;
        self.jobs += 1;
        let changes: Vec<Change> = {
            let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.before_job)
                .into_iter()
                .partition(|(at, _)| *at == n);
            self.before_job = later;
            now.into_iter().map(|(_, change)| change).collect()
        };
        for change in changes {
            change(self);
        }
        let bytes = match job {
            Job::Line(text) => text.clone(),
            Job::Cr => "\r".to_string(),
            Job::Deletes(count) => "\u{7f}".repeat(*count),
        };
        let shown = match job {
            Job::Line(text) => text.clone(),
            Job::Cr => "<CR>".to_string(),
            Job::Deletes(count) => format!("<DEL×{count}>"),
        };
        match self.writes.pop_front().unwrap_or(Write::Whole) {
            Write::NeverStarts => {}
            Write::Stall => {
                self.events.push_back(Event::JobStarted);
                self.events.push_back(Event::JobStalled);
                self.running = Some(shown);
            }
            Write::Partial(take) => {
                self.events.push_back(Event::JobStarted);
                let taken: String = bytes.chars().take(take).collect();
                self.pane.push(match job {
                    Job::Line(_) => taken.clone(),
                    _ => shown,
                });
                self.type_into_composer(&taken);
                self.now += self.latency;
                self.events.push_back(Event::JobEnded {
                    accepted: taken.len(),
                    ok: false,
                });
                self.events.push_back(Event::Frame(self.frame()));
            }
            write => {
                self.events.push_back(Event::JobStarted);
                self.pane.push(shown);
                self.type_into_composer(&bytes);
                self.now += self.latency;
                self.events.push_back(Event::JobEnded {
                    accepted: bytes.len(),
                    ok: write != Write::FlushError,
                });
                self.events.push_back(Event::Frame(self.frame()));
            }
        }
    }

    fn timed_changes(&mut self) -> bool {
        let elapsed = self.now - self.start;
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.at)
            .into_iter()
            .partition(|(at, _)| *at <= elapsed);
        self.at = later;
        let any = !due.is_empty();
        for (_, change) in due {
            change(self);
        }
        any
    }
}

impl Seam for World {
    fn read(&mut self) -> Option<SeamRead> {
        (!self.contended).then(|| SeamRead {
            watermark: self.watermark,
            job_mark: self.job_mark,
            frame: self.frame(),
            mode: self.mode,
            open: self.open,
        })
    }
}

/// A token draw that spells `bxdmpw`'s cousins from the alphabet in turn.
fn draws() -> impl FnMut() -> u8 {
    let mut n = 0u8;
    move || {
        n = n.wrapping_add(5);
        n
    }
}

/// Admit an attempt in `world` and run it to its release; returns the outcome
/// and the line typed.
fn run(world: &mut World) -> (Result<Outcome, NotTyped>, String) {
    let admission = world.admission();
    let start = world.now;
    world.start = start;
    // The generation's one cell, ring and counter, shared with the model's
    // hook arm as the daemon shares them.
    let (cell, ring, counter) = (
        Arc::clone(&world.cell),
        Arc::clone(&world.ring),
        Arc::clone(&world.counter),
    );
    let mut txn: Txn<'_> = match admit(
        &admission,
        &counter,
        &ring,
        &cell,
        draws(),
        ID,
        world.caps,
        start,
    ) {
        Ok(txn) => txn,
        Err(why) => return (Err(why), String::new()),
    };
    let line = txn.line().to_string();
    if let Some(change) = world.after_admission.take() {
        change(world);
    }
    let mut actions = txn.start(world, start);
    let mut steps = 0;
    loop {
        steps += 1;
        assert!(steps < 100_000, "the transaction never released");
        let mut wake = None;
        for action in std::mem::take(&mut actions) {
            match action {
                Action::Enqueue(job) => world.apply(&job),
                Action::TakeBack => {}
                Action::WakeAt(at) => wake = Some(at),
                Action::Reproject => {}
                Action::Release(outcome) => {
                    world.released_at = Some(world.now);
                    world.cell_at_release = Some(world.cell.load());
                    // Option A: what was waiting on the writer goes in order,
                    // after a started deck job still running.
                    if let Some(running) = world.running.take() {
                        world.pane.push(format!("{running} (late)"));
                    }
                    let human = std::mem::take(&mut world.human);
                    world.pane.extend(human);
                    return (Ok(outcome), line);
                }
            }
        }
        if let Some(event) = world.events.pop_front() {
            let now = world.now;
            actions = txn.on(event, world, now);
            continue;
        }
        // Move the clock to the transaction's wake-up, or to the next
        // scheduled change if that comes first, and deliver what the change
        // produced before the tick.
        let at = wake.unwrap_or(world.now + Duration::from_millis(10));
        let next_change = world
            .at
            .iter()
            .map(|(offset, _)| world.start + *offset)
            .min();
        let at = next_change.map_or(at, |change| at.min(change));
        world.now = world.now.max(at).max(world.now + Duration::from_micros(1));
        if world.timed_changes() {
            world.events.push_back(Event::Frame(world.frame()));
            continue;
        }
        let now = world.now;
        actions = txn.on(Event::Tick, world, now);
    }
}

fn crs(world: &World) -> usize {
    world.pane.iter().filter(|w| *w == "<CR>").count()
}

/// Scenario: Type the line into a model agent that echoes it exactly and submits it on Enter, its prompt hook reporting the token. The deck presses Enter once, after the echo, and the attempt is Submitted.
#[spec("inbox/doorbell/037")]
#[test]
fn doorbell_037_an_exact_echo_is_followed_by_one_enter() {
    let mut world = World::new();
    let (outcome, line) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Submitted));
    assert_eq!(world.pane, vec![line.clone(), "<CR>".to_string()]);
    assert!(line.starts_with('(') && line.ends_with(" inbox fetch"));
    assert!(matches!(
        world.cell.load(),
        Latch::Resolved(1, Resolution::Submitted)
    ));
}

/// Scenario: The screen keeps showing the empty composer while the agent already holds a draft the user typed, so the deck's line lands after it; the echo shows the draft and the line together. The deck presses no Enter, and the line cannot be withdrawn as its own.
#[spec("inbox/doorbell/037")]
#[test]
fn doorbell_037_a_stale_screen_with_a_draft_in_front_gets_no_enter() {
    let mut world = World::new();
    world.composer = "half a thought".to_string();
    world.stale = Some(String::new());
    world.before_job.push((0, Box::new(|w| w.stale = None)));
    let (outcome, _) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::EchoMismatch)));
    assert_eq!(crs(&world), 0);
    assert!(!world.pane.iter().any(|w| w.starts_with("<DEL")));
}

/// Scenario: The agent draws the line in two frames, a prefix and then the rest. The deck keeps waiting through the prefix and presses Enter only on the whole line. An agent that never draws the line gets no Enter by the end of the echo wait.
#[spec("inbox/doorbell/037")]
#[test]
fn doorbell_037_a_prefix_keeps_waiting_and_a_missing_echo_sends_nothing() {
    let mut world = World::new();
    world.before_job.push((
        0,
        Box::new(|w| {
            w.stale = Some("(".to_string());
            w.at.push((Duration::from_millis(60), Box::new(|w| w.stale = None)));
        }),
    ));
    let (outcome, _) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Submitted));
    assert_eq!(crs(&world), 1);

    let mut silent = World::new();
    silent
        .before_job
        .push((0, Box::new(|w| w.stale = Some(String::new()))));
    let (outcome, line) = run(&mut silent);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::EchoMismatch)));
    assert_eq!(silent.pane, vec![line], "the line alone, no Enter");
}

/// Scenario: The echo holds, then the frame changes before the deck's Enter: the composer now shows other text. The synchronous re-check before the Enter refuses it, and no Enter is sent.
#[spec("inbox/doorbell/037")]
#[test]
fn doorbell_037_a_frame_changed_after_the_echo_gets_no_enter() {
    let mut world = World::new();
    world.before_job.push((
        0,
        Box::new(|w| {
            w.at.push((
                Duration::from_millis(100),
                Box::new(|w| {
                    w.composer.push('x');
                }),
            ))
        }),
    ));
    let (outcome, _) = run(&mut world);
    assert_eq!(crs(&world), 0);
    assert!(
        matches!(outcome, Ok(Outcome::Unresolved(Cause::EchoMismatch))),
        "{outcome:?}"
    );
}

/// Scenario: The writer takes only part of the line. With the whole token taken, the deck deletes exactly what was taken and sees the placeholder: Withdrawn. With less than the token taken, it cannot tell its text from anyone's: Unresolved, nothing deleted. Neither presses Enter or writes again.
#[spec("inbox/doorbell/038")]
#[test]
fn doorbell_038_a_partial_line_is_withdrawn_only_with_its_whole_token() {
    let mut world = World::new();
    world.writes.push_back(Write::Partial(20));
    let (outcome, _) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Withdrawn));
    assert_eq!(world.pane[1], "<DEL×20>");
    assert_eq!(crs(&world), 0);

    let mut short = World::new();
    short.writes.push_back(Write::Partial(5));
    let (outcome, _) = run(&mut short);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::PartialWrite)));
    assert_eq!(short.pane.len(), 1, "no delete, no retry: {:?}", short.pane);
}

/// Scenario: The line's write stalls; separately, its flush fails after every byte was taken. A stall ends Unresolved with the cell suspended before the writer is released and the card asked to show it, and the late bytes land after release ahead of the human frame waiting behind them; the flush error withdraws the whole line under the strict recogniser. No Enter either way.
#[spec("inbox/doorbell/038")]
#[test]
fn doorbell_038_a_stalled_write_or_a_failed_flush_never_presses_enter() {
    let mut world = World::new();
    world.writes.push_back(Write::Stall);
    world
        .before_job
        .push((0, Box::new(|w| w.human.push("typed".to_string()))));
    let (outcome, line) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::WriteStalled)));
    assert_eq!(
        world.cell_at_release,
        Some(Latch::Suspended(1, Cause::WriteStalled))
    );
    assert_eq!(project(world.cell.load()), Some(Shown::UncertainInput));
    assert_eq!(
        world.pane,
        vec![format!("{line} (late)"), "typed".to_string()]
    );

    let mut flush = World::new();
    flush.writes.push_back(Write::FlushError);
    let (outcome, _) = run(&mut flush);
    assert_eq!(outcome, Ok(Outcome::Withdrawn));
    assert_eq!(crs(&flush), 0);
}

/// Scenario: The agent shows no sign of taking the line's Enter and the human has typed text and Enter behind the transaction. The attempt ends Unresolved and suspended, the deck never presses a second Enter, and the human's frames go in after the release, in order (option A); the suspension survives the queue emptying and new mail. A delete job that stalls ends Unresolved too.
#[spec("inbox/doorbell/038")]
#[test]
fn doorbell_038_option_a_writes_waiting_frames_after_an_unresolved_end() {
    let mut world = World::new();
    world.enter = Enter::Submit;
    world.hook = false;
    world.caps.transcript_route = false;
    world.caps.delete_key = false;
    world.before_job.push((
        0,
        Box::new(|w| {
            w.human.push("my text".to_string());
            w.human.push("\r".to_string());
        }),
    ));
    let (outcome, line) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::NoSubmitEvidence)));
    assert_eq!(
        world.pane,
        vec![line, "<CR>".into(), "my text".into(), "\r".into()]
    );
    assert!(
        world.cell.load().suspended(),
        "nothing but the user, exact evidence or the generation's end lifts it"
    );

    let mut deletes = World::new();
    deletes.enter = Enter::Drop;
    deletes.hook = false;
    deletes
        .writes
        .extend([Write::Whole, Write::Whole, Write::Stall]);
    let (outcome, _) = run(&mut deletes);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::WithdrawalStalled)));
}

/// Scenario: The agent keeps the deck's Enter as a newline, or drops it. Only after the submit window does the deck delete its line (an agent that takes the Enter late inside the window is Submitted, never withdrawn from), newline included, in one job; it calls the attempt Withdrawn only once the placeholder is back, the human's waiting frame goes in after that, and no second Enter is ever sent. A profile with no placeholder deletes nothing and ends Unresolved, withdrawal refused.
#[spec("inbox/doorbell/042")]
#[test]
fn doorbell_042_an_enter_not_taken_is_withdrawn_only_on_the_placeholder() {
    for (enter, extra) in [(Enter::Newline, 1), (Enter::Drop, 0)] {
        let mut world = World::new();
        world.enter = enter;
        world.hook = false;
        world
            .before_job
            .push((1, Box::new(|w| w.human.push("mine".to_string()))));
        let (outcome, line) = run(&mut world);
        assert_eq!(outcome, Ok(Outcome::Withdrawn), "{enter:?}");
        let deletes = format!("<DEL×{}>", line.chars().count() + extra);
        assert_eq!(
            world.pane,
            vec![line, "<CR>".into(), deletes, "mine".into()],
            "{enter:?}"
        );
        assert_eq!(world.composer, "");
    }
    // An agent that takes the Enter late, inside the submit window, is never
    // withdrawn from.
    let mut slow = World::new();
    slow.enter = Enter::Drop;
    slow.before_job.push((
        1,
        Box::new(|w| {
            w.at.push((
                Duration::from_millis(450),
                Box::new(|w| {
                    w.enter = Enter::Submit;
                    w.type_into_composer("\r");
                }),
            ));
        }),
    ));
    let (outcome, line) = run(&mut slow);
    assert_eq!(outcome, Ok(Outcome::Submitted));
    assert_eq!(slow.pane, vec![line, "<CR>".to_string()]);

    let mut blank = World::new();
    blank.enter = Enter::Newline;
    blank.hook = false;
    blank.caps.placeholder = false;
    let (outcome, line) = run(&mut blank);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::WithdrawalRefused)));
    assert_eq!(blank.pane, vec![line, "<CR>".to_string()]);
}

/// Scenario: Three traces the token is there to tell apart: the screen still shows an empty composer while the user's copy of an earlier attempt's exact line sits in the agent; a history recall puts an older attempt's line in the composer instead of the deck's; and an agent whose terminal still echoes (cooked mode). None gets an Enter, and the cooked one gets no byte at all.
#[spec("inbox/doorbell/043")]
#[test]
fn doorbell_043_an_earlier_line_or_a_kernel_echo_is_never_the_deck_s() {
    let earlier = RingToken::parse("abcdfg").unwrap().line(ID);
    let mut stale = World::new();
    stale.composer = earlier.clone();
    stale.stale = Some(String::new());
    stale.before_job.push((0, Box::new(|w| w.stale = None)));
    let (outcome, _) = run(&mut stale);
    assert!(matches!(outcome, Ok(Outcome::Unresolved(_))), "{outcome:?}");
    assert_eq!(crs(&stale), 0);

    let mut recall = World::new();
    let older = earlier.clone();
    recall
        .before_job
        .push((0, Box::new(move |w| w.stale = Some(older))));
    let (outcome, _) = run(&mut recall);
    assert!(matches!(outcome, Ok(Outcome::Unresolved(_))), "{outcome:?}");
    assert_eq!(crs(&recall), 0);

    let mut cooked = World::new();
    cooked.mode = Err("input_mode_cooked");
    let (outcome, _) = run(&mut cooked);
    assert_eq!(outcome, Err(NotTyped::CannotType("input_mode_cooked")));
    assert!(cooked.pane.is_empty());
}

/// Scenario: Pick a token while the admission screen shows the first candidate the draw would give, and again while the ring holds the next one as a recent attempt's. Each time the candidate is drawn again, so the token was neither on screen nor recent.
#[spec("inbox/doorbell/043")]
#[test]
fn doorbell_043_a_token_on_screen_or_recent_is_drawn_again() {
    let first = RingToken::from_draws(draws());
    let ring = AttemptRing::default();
    let picked = pick_token(draws(), &format!("screen {} text", first.letters()), &ring);
    assert_ne!(picked, first);
    ring.record(7, picked);
    let again = pick_token(draws(), &format!("screen {} text", first.letters()), &ring);
    assert_ne!(again, picked);
    assert_ne!(again, first);
    for token in [first, picked, again] {
        assert!(
            token
                .letters()
                .bytes()
                .all(|b| dot_agent_deck::doorbell_txn::TOKEN_ALPHABET.contains(&b))
        );
    }
}

/// Scenario: After the line is typed and echoed, the message is taken, the status moves, a receiver arms or the admission is lost, with a human frame arriving right after. With a delete key the deck withdraws its line; with none it submits it, a stale line's fetch finding nothing. Either way before the human frame goes in.
#[spec("inbox/doorbell/045")]
#[test]
fn doorbell_045_a_cancellation_after_the_line_resolves_before_human_frames() {
    for delete_key in [true, false] {
        let mut world = World::new();
        world.caps.delete_key = delete_key;
        world.at.push((
            Duration::from_millis(50),
            Box::new(|w| {
                w.events.push_back(Event::AdmissionLost);
                w.human.push("after".to_string());
            }),
        ));
        let (outcome, line) = run(&mut world);
        if delete_key {
            assert_eq!(outcome, Ok(Outcome::Withdrawn));
            let deletes = format!("<DEL×{}>", line.chars().count());
            assert_eq!(world.pane, vec![line, deletes, "after".into()]);
        } else {
            assert_eq!(outcome, Ok(Outcome::Submitted));
            assert_eq!(world.pane, vec![line, "<CR>".into(), "after".into()]);
        }
    }
}

/// Scenario: Four ways an empty-looking composer lies, each with the human's text and Enter waiting: the agent draws an empty composer mid-redraw, restores the line and keeps the Enter as a newline; it drops the Enter; a dialog stashes the composer and restores it; and an agent with no hook, or an older hook's report without a token. None is Submitted, and the human's frames go in only after a withdrawal or an Unresolved end.
#[spec("inbox/doorbell/047")]
#[test]
fn doorbell_047_an_empty_composer_alone_is_no_submit() {
    let mut redraw = World::new();
    redraw.enter = Enter::Newline;
    redraw.hook = false;
    redraw.before_job.push((
        1,
        Box::new(|w| {
            w.human.push("x\r".to_string());
            w.stale = Some(String::new());
            w.at.push((Duration::from_millis(400), Box::new(|w| w.stale = None)));
        }),
    ));
    let (outcome, _) = run(&mut redraw);
    assert_ne!(outcome, Ok(Outcome::Submitted));
    assert_eq!(redraw.pane.last().map(String::as_str), Some("x\r"));

    let mut dropped = World::new();
    dropped.enter = Enter::Drop;
    dropped.hook = false;
    let (outcome, _) = run(&mut dropped);
    assert_eq!(outcome, Ok(Outcome::Withdrawn));

    let mut stash = World::new();
    stash.enter = Enter::Drop;
    stash.hook = false;
    stash.before_job.push((
        1,
        Box::new(|w| {
            w.dialog = Some("hooks");
            w.at.push((Duration::from_millis(300), Box::new(|w| w.dialog = None)));
        }),
    ));
    let (outcome, _) = run(&mut stash);
    assert_ne!(outcome, Ok(Outcome::Submitted));

    let mut no_hook = World::new();
    no_hook.hook = false;
    no_hook.caps.transcript_route = false;
    let (outcome, _) = run(&mut no_hook);
    assert_ne!(
        outcome,
        Ok(Outcome::Submitted),
        "no submit without the token or a transcript echo"
    );
}

/// Scenario: Transcript negatives: the model quotes the line in an assistant message; a history repaint shows an older attempt's user message; the user message arrives in two pieces and only the first is drawn; the composer still holds the line while a transcript-looking row shows it. None is Submitted. The positive control, a whole user message with the composer empty, is.
#[spec("inbox/doorbell/047")]
#[test]
fn doorbell_047_only_a_whole_user_message_of_the_line_is_a_transcript_echo() {
    let scenario = |change: LineChange| {
        let mut world = World::new();
        world.hook = false;
        world.enter = Enter::Drop;
        world.caps.delete_key = false;
        world.before_job.push((
            1,
            Box::new(move |w| {
                let line = w.composer.clone();
                change(w, &line);
            }),
        ));
        run(&mut world).0
    };
    // An assistant's quote is not a user cell: nothing is added to them.
    assert_ne!(scenario(Box::new(|_, _| {})), Ok(Outcome::Submitted));
    assert_ne!(
        scenario(Box::new(|w, _| w
            .user_cells
            .push(RingToken::parse("abcdfg").unwrap().line(ID)))),
        Ok(Outcome::Submitted),
        "an older attempt's user message"
    );
    assert_ne!(
        scenario(Box::new(|w, line| w
            .user_cells
            .push(line[..20].to_string()))),
        Ok(Outcome::Submitted),
        "half a user message"
    );
    assert_ne!(
        scenario(Box::new(|w, line| w.user_cells.push(line.to_string()))),
        Ok(Outcome::Submitted),
        "the composer still holds the line"
    );
    assert_eq!(
        scenario(Box::new(|w, line| {
            w.user_cells.push(line.to_string());
            w.composer.clear();
        })),
        Ok(Outcome::Submitted),
        "the positive control"
    );
}

/// Scenario: Staged typing with each write near its cap and a layout edge between the stages that stays dirty a while, then an agent that never draws the whole line. The writer is released by the deadline, Unresolved with the cell suspended, and no byte goes in after it.
#[spec("inbox/doorbell/049")]
#[test]
fn doorbell_049_one_deadline_covers_every_phase() {
    let mut world = World::new();
    world.caps.staged_head = Some(9);
    world.latency = Duration::from_millis(190);
    // The edge lands with the head; the state is Clean again only at 1.4 s,
    // when the head's echo is evaluated again at the new epoch.
    world.before_job.push((
        0,
        Box::new(|w| {
            w.clean = false;
            w.epoch = 2;
            w.at.push((Duration::from_millis(1400), Box::new(|w| w.clean = true)));
        }),
    ));
    // The rest is typed and the agent never draws it.
    world
        .before_job
        .push((1, Box::new(|w| w.stale = Some(w.composer.clone()))));
    let start = world.now;
    let (outcome, line) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::Deadline)));
    assert_eq!(
        world.pane,
        vec![line[..9].to_string(), line[9..].to_string()]
    );
    assert!(world.released_at.unwrap() <= start + TXN_MAX);
    assert!(world.cell_at_release.unwrap().suspended());
    assert_eq!(crs(&world), 0);
}

/// Scenario: The line's job never starts before the deadline; separately, it starts and stalls with a human frame waiting. The first is taken back and nothing is written; the second ends Unresolved at the deadline and its late bytes land before the human frame. A writer that keeps every job stalled still releases the transaction by the deadline.
#[spec("inbox/doorbell/049")]
#[test]
fn doorbell_049_expiry_before_and_after_the_first_byte() {
    let mut queued = World::new();
    queued.writes.push_back(Write::NeverStarts);
    let start = queued.now;
    let (outcome, _) = run(&mut queued);
    assert_eq!(outcome, Ok(Outcome::NothingWritten(NotTyped::Deadline)));
    assert!(queued.pane.is_empty());
    assert!(queued.released_at.unwrap() <= start + TXN_MAX);
    assert!(matches!(
        queued.cell.load(),
        Latch::Resolved(1, Resolution::NotTyped)
    ));

    let mut started = World::new();
    started.writes.push_back(Write::Stall);
    started
        .before_job
        .push((0, Box::new(|w| w.human.push("typed".to_string()))));
    let (outcome, line) = run(&mut started);
    assert!(matches!(outcome, Ok(Outcome::Unresolved(_))));
    assert_eq!(
        started.pane,
        vec![format!("{line} (late)"), "typed".to_string()]
    );
}

/// Scenario: The seam's locks stay contended. Its retry loop never tries at or after the deadline and enqueues nothing; a transaction whose Enter seam never gets its locks is released by the deadline with its cell already suspended.
#[spec("inbox/doorbell/049")]
#[test]
fn doorbell_049_a_lock_that_stays_contended_enqueues_nothing_after_the_deadline() {
    let start = Instant::now();
    let deadline = start + TXN_MAX;
    let mut tries = Vec::new();
    assert_eq!(
        seam_until(start, deadline, |at| {
            tries.push(at);
            false
        }),
        None
    );
    assert!(tries.iter().all(|at| *at < deadline));
    assert!(
        tries.len() > 2_000,
        "retried every millisecond: {}",
        tries.len()
    );

    let mut world = World::new();
    world
        .at
        .push((Duration::from_millis(1), Box::new(|w| w.contended = true)));
    let (outcome, _) = run(&mut world);
    assert!(
        matches!(outcome, Ok(Outcome::Unresolved(Cause::Deadline))),
        "{outcome:?}"
    );
    assert_eq!(crs(&world), 0);
    assert_eq!(
        world.cell_at_release,
        Some(Latch::Suspended(1, Cause::Deadline))
    );
}

/// Scenario: The seam's ledger checks. A human frame admitted after the attempt's admission and before its first byte moves the watermark, and the attempt ends with nothing written. One admitted after the line is typed waits and cancels nothing: the Enter still goes and the human frame follows the release. A job mark that moves after the first byte (a ticket that reached the PTY around the held writer) stops everything: no Enter, Unresolved.
#[spec("inbox/doorbell/049")]
#[test]
fn doorbell_049_the_seam_checks_the_watermark_before_the_first_byte_and_the_job_mark_after() {
    let mut before = World::new();
    before.after_admission = Some(Box::new(|w| w.human_types("early")));
    let (outcome, _) = run(&mut before);
    assert_eq!(outcome, Ok(Outcome::NothingWritten(NotTyped::HumanInput)));
    assert_eq!(before.pane, vec!["early".to_string()]);
    assert!(matches!(
        before.cell.load(),
        Latch::Resolved(1, Resolution::NotTyped)
    ));

    let mut after = World::new();
    after.at.push((
        Duration::from_millis(50),
        Box::new(|w| w.human_types("late")),
    ));
    let (outcome, line) = run(&mut after);
    assert_eq!(outcome, Ok(Outcome::Submitted));
    assert_eq!(after.pane, vec![line, "<CR>".into(), "late".into()]);

    let mut bypass = World::new();
    bypass
        .at
        .push((Duration::from_millis(50), Box::new(|w| w.job_mark += 1)));
    let (outcome, _) = run(&mut bypass);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::EchoMismatch)));
    assert_eq!(crs(&bypass), 0);
    assert!(
        !bypass.pane.iter().any(|w| w.starts_with("<DEL")),
        "nothing more is enqueued"
    );
}

/// Scenario: A resize lands while the deck waits for its echo: the frame goes dirty, then a Clean frame at the new width shows the line, and the deck presses Enter on that one. One that stays dirty to the deadline ends Unresolved for the layout, and no delete is ever sent on a frame from before the edge.
#[spec("inbox/doorbell/044")]
#[test]
fn doorbell_044_an_edge_during_the_transaction_waits_for_a_clean_frame() {
    let mut world = World::new();
    world.before_job.push((
        0,
        Box::new(|w| {
            w.clean = false;
            w.epoch = 2;
            w.at.push((Duration::from_millis(80), Box::new(|w| w.clean = true)));
        }),
    ));
    let (outcome, _) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Submitted));
    assert_eq!(crs(&world), 1);

    let mut dirty = World::new();
    dirty.before_job.push((
        0,
        Box::new(|w| {
            w.clean = false;
            w.epoch = 2;
        }),
    ));
    let (outcome, _) = run(&mut dirty);
    assert_eq!(outcome, Ok(Outcome::Unresolved(Cause::Layout)));
    assert!(
        !dirty
            .pane
            .iter()
            .any(|w| w.starts_with("<DEL") || w == "<CR>")
    );

    let mut withdrawing = World::new();
    withdrawing.enter = Enter::Drop;
    withdrawing.hook = false;
    withdrawing.before_job.push((
        1,
        Box::new(|w| {
            w.at.push((
                Duration::from_millis(500),
                Box::new(|w| {
                    w.clean = false;
                    w.epoch = 3;
                }),
            ));
            w.at.push((Duration::from_millis(1400), Box::new(|w| w.clean = true)));
        }),
    ));
    let (outcome, _) = run(&mut withdrawing);
    assert_eq!(outcome, Ok(Outcome::Withdrawn));
}

/// Scenario: Walk one generation's latch cell through every move of the transition table: an attempt opens, ends with nothing written, a later attempt suspends and is resolved late by exact evidence, another is resolved by the user, withdrawn and submitted; each move names its attempt, and a move naming another attempt, a backward move and anything after retirement are refused. Retirement between an attempt's allocation and its Open move leaves it writing nothing.
#[spec("inbox/doorbell/046")]
#[test]
fn doorbell_046_the_latch_cell_moves_only_forward_on_its_own_attempt() {
    let cell = LatchCell::default();
    assert_eq!(cell.load(), Latch::Resolved(0, Resolution::NotTyped));
    assert_eq!(project(cell.load()), None);
    assert!(cell.open(1).is_ok());
    assert!(cell.open(1).is_err(), "an attempt opens once");
    assert!(cell.not_typed(2).is_err(), "another attempt's move");
    assert!(cell.not_typed(1).is_ok());
    assert!(cell.open(2).is_ok());
    assert!(cell.suspend(2, Cause::Deadline).is_ok());
    assert_eq!(project(cell.load()), Some(Shown::UncertainInput));
    assert!(
        cell.suspend(2, Cause::Layout).is_err(),
        "Suspended(a) is not re-suspended"
    );
    assert!(
        cell.exact_evidence(1).is_err(),
        "an older attempt's evidence"
    );
    assert!(cell.exact_evidence(2).is_ok());
    assert_eq!(cell.load(), Latch::Resolved(2, Resolution::LateExact));
    assert!(
        cell.suspend(2, Cause::Deadline).is_err(),
        "Resolved(a) to Suspended(a) is backward"
    );
    assert!(cell.open(3).is_ok());
    assert!(cell.suspend(3, Cause::DialogAfterSubmit).is_ok());
    assert_eq!(project(cell.load()), Some(Shown::DialogDuringRing));
    assert!(cell.user_cleared().is_ok());
    assert_eq!(cell.load(), Latch::Resolved(3, Resolution::User));
    assert!(cell.user_cleared().is_err(), "nothing suspended");
    assert!(cell.open(4).is_ok());
    assert!(cell.resolve(4, Resolution::Withdrawn).is_ok());
    assert!(cell.open(5).is_ok());
    assert!(cell.exact_evidence(5).is_ok());
    assert_eq!(cell.load(), Latch::Resolved(5, Resolution::Submitted));
    assert!(cell.open(4).is_err(), "an attempt never goes back");

    // Retirement between allocation and Open: absorbing, by its own
    // predicate, though Suspended(a+1) would compare above it.
    let counter = AttemptCounter::default();
    let a = counter.allocate().unwrap();
    cell.retire();
    assert!(cell.open(a + 10).is_err());
    assert!(cell.suspend(a + 10, Cause::Deadline).is_err());
    assert_eq!(cell.load(), Latch::Retired);
    let mut world = World::new();
    world.cell.retire();
    let (outcome, _) = run(&mut world);
    assert_eq!(outcome, Err(NotTyped::Retired));
    assert!(world.pane.is_empty());
}

/// Scenario: A generation's attempt numbers start just below the reserved maximum: the last number is used once, then admission says attempt_exhausted, and no number repeats.
#[spec("inbox/doorbell/046")]
#[test]
fn doorbell_046_attempt_numbers_run_out_instead_of_wrapping() {
    let counter = AttemptCounter::starting_at(u32::MAX - 2);
    assert_eq!(counter.allocate(), Some(u32::MAX - 2));
    assert_eq!(counter.allocate(), Some(u32::MAX - 1));
    assert_eq!(counter.allocate(), None);
    assert_eq!(counter.allocate(), None);
    let mut world = World::new();
    world.counter = Arc::new(AttemptCounter::starting_at(u32::MAX));
    let (outcome, _) = run(&mut world);
    assert_eq!(outcome, Err(NotTyped::Exhausted));
    assert!(world.pane.is_empty());
}

/// Scenario: Exact evidence for the attempt reaches the hook arm just as the deadline fires, before the suspension's compare-and-swap. The arm resolves the open attempt on receipt, the suspension then fails, and the transaction reports Submitted with nothing suspended or shown.
#[spec("inbox/doorbell/046")]
#[test]
fn doorbell_046_evidence_before_the_suspension_wins() {
    let cell = LatchCell::default();
    let ring = AttemptRing::default();
    let token = RingToken::parse("bxdmpw").unwrap();
    ring.record(1, token);
    cell.open(1).unwrap();
    assert!(hook_evidence(&cell, &ring, token, true).is_some());
    assert_eq!(
        cell.suspend(1, Cause::Deadline),
        Err(Latch::Resolved(1, Resolution::Submitted))
    );
    assert_eq!(project(cell.load()), None);

    // Through a transaction: its Enter's seam stays contended, and the
    // agent's report of the line (a prompt the user submitted, say) reaches
    // the hook arm a millisecond before the deadline, while the cell is open.
    let mut world = World::new();
    world
        .at
        .push((Duration::from_millis(1), Box::new(|w| w.contended = true)));
    world.at.push((
        TXN_MAX - Duration::from_millis(1),
        Box::new(|w| {
            let token = RingToken::parse(&w.composer[1..7]).unwrap();
            assert!(matches!(w.cell.load(), Latch::Open(1)));
            hook_evidence(&w.cell, &w.ring, token, true);
        }),
    ));
    let (outcome, _) = run(&mut world);
    assert_eq!(outcome, Ok(Outcome::Submitted), "{:?}", world.cell.load());
}

/// Scenario: Record 65 attempts in the ring: the 65th evicts the first's slot, so late evidence for the first token is a miss that moves nothing, and an entry copied before the eviction cannot move the cell on a later attempt. Late exact evidence for an older attempt's token does nothing; a report with other text never moves the cell. Each entry is one packed store.
#[spec("inbox/doorbell/046")]
#[test]
fn doorbell_046_the_ring_correlates_the_last_64_attempts_only() {
    let ring = AttemptRing::default();
    let token = |n: u32| {
        RingToken::from_draws({
            let mut k = 0u32;
            move || {
                k += 1;
                ((n >> (4 * (k - 1))) & 0x0f) as u8
            }
        })
    };
    for n in 1..=65 {
        ring.record(n, token(n));
    }
    assert_eq!(ring.lookup(token(1)), None, "evicted");
    assert_eq!(ring.lookup(token(2)), Some(2));
    assert_eq!(ring.lookup(token(65)), Some(65));
    assert_eq!(ring_entry(65, token(65)) >> 32, 65);
    assert_eq!(ring_entry(65, token(65)) as u32, token(65).bits());

    let cell = LatchCell::default();
    cell.open(65).unwrap();
    cell.suspend(65, Cause::Deadline).unwrap();
    assert_eq!(hook_evidence(&cell, &ring, token(1), true), None);
    assert_eq!(
        cell.exact_evidence(1),
        Err(Latch::Suspended(65, Cause::Deadline)),
        "a copy from before the eviction"
    );
    assert_eq!(
        hook_evidence(&cell, &ring, token(64), true),
        None,
        "an older attempt's token"
    );
    assert_eq!(
        hook_evidence(&cell, &ring, token(65), false),
        None,
        "with other text"
    );
    assert!(cell.load().suspended());
    assert_eq!(
        hook_evidence(&cell, &ring, token(65), true),
        Some(Latch::Resolved(65, Resolution::LateExact))
    );
}

/// Scenario: The re-projection model, a request held back while the generation's state moves on: past a user clear, late exact evidence, an in-pause resolution, and a retirement with a successor's own suspension. Each run shows the state at the moment it runs and touches nothing for a generation no longer current. The positive control runs before the clear and shows the suspension, then the clear's own request shows it cleared, each broadcast once.
#[spec("inbox/doorbell/046")]
#[test]
fn doorbell_046_a_delayed_reprojection_shows_only_the_current_state() {
    let cell = LatchCell::default();
    let current = |_: &str| Some("a1".to_string());
    for resolve in [
        Box::new(|c: &LatchCell| c.user_cleared().map(|_| ()))
            as Box<dyn Fn(&LatchCell) -> Result<(), Latch>>,
        Box::new(|c: &LatchCell| c.exact_evidence(1).map(|_| ())),
        Box::new(|c: &LatchCell| c.resolve(1, Resolution::Withdrawn).map(|_| ())),
    ] {
        let cell = LatchCell::default();
        let mut publisher = Publisher::default();
        cell.open(1).unwrap();
        cell.suspend(1, Cause::EchoMismatch).unwrap();
        publisher.request("p", "a1");
        resolve(&cell).unwrap();
        publisher.request("p", "a1");
        assert_eq!(publisher.pending(), 1, "coalesced");
        publisher.run_next(current, |_| Some(&cell));
        assert_eq!(publisher.shown("p"), None);
        assert!(
            publisher.broadcasts.is_empty(),
            "never carried the suspension"
        );
    }

    // Retirement and a successor suspended in its own cell: the old request
    // does nothing, the successor's shows its own state.
    let old = LatchCell::default();
    let successor = LatchCell::default();
    old.open(1).unwrap();
    old.suspend(1, Cause::Deadline).unwrap();
    let mut publisher = Publisher::default();
    publisher.request("p", "a1");
    old.retire();
    successor.open(1).unwrap();
    successor.suspend(1, Cause::DialogAfterTyping).unwrap();
    publisher.request("p", "a2");
    let cells = |agent: &str| match agent {
        "a1" => Some(&old),
        _ => Some(&successor),
    };
    publisher.run_next(|_| Some("a2".to_string()), cells);
    assert!(
        publisher.broadcasts.is_empty(),
        "the old generation's request did nothing"
    );
    publisher.run_next(|_| Some("a2".to_string()), cells);
    assert_eq!(publisher.shown("p"), Some(Shown::DialogDuringRing));

    // A request for a generation no longer current touches nothing, even
    // while that generation's own cell is still suspended.
    let stale = LatchCell::default();
    stale.open(1).unwrap();
    stale.suspend(1, Cause::Deadline).unwrap();
    let clear = LatchCell::default();
    let mut publisher = Publisher::default();
    publisher.request("p", "a1");
    publisher.run_next(
        |_| Some("a2".to_string()),
        |agent: &str| match agent {
            "a1" => Some(&stale),
            _ => Some(&clear),
        },
    );
    assert_eq!(publisher.shown("p"), None);
    assert!(publisher.broadcasts.is_empty());

    // Positive control.
    let mut publisher = Publisher::default();
    cell.open(1).unwrap();
    cell.suspend(1, Cause::EchoMismatch).unwrap();
    publisher.request("p", "a1");
    publisher.run_next(current, |_| Some(&cell));
    assert_eq!(publisher.shown("p"), Some(Shown::UncertainInput));
    cell.user_cleared().unwrap();
    publisher.request("p", "a1");
    publisher.run_next(current, |_| Some(&cell));
    publisher.request("p", "a1");
    publisher.run_next(current, |_| Some(&cell));
    assert_eq!(
        publisher.broadcasts,
        vec![
            ("p".to_string(), Some(Shown::UncertainInput)),
            ("p".to_string(), None)
        ]
    );
}
