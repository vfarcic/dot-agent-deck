//! PRD #1666 part 3, round 5-wake (plan §4.6): reading an agent's release off
//! its start-up header.
//!
//! The doorbell's admission decides per release ([`crate::doorbell_activation`]),
//! so each generation needs its release. No hook or wrapper reports it today;
//! the agent's start-up screen shows it:
//! Codex draws `>_ OpenAI Codex (v0.164.0)`, Claude Code draws
//! `Claude Code` and `v2.1.310` beside its logo.
//!
//! A [`ReleaseScanner`] is a `vt100` parser of its own at the pane's size,
//! with no scrollback, fed every byte of the pane's output from the
//! generation's first byte. **After every byte** it checks the row the cursor
//! is on, and only that row, against each [`HeaderRecogniser`]: the anchor
//! cell first, the rest of the span only on an anchor hit. The parser keeps
//! its state across calls, so the grid states the scanner passes through, and
//! with them what it latches, depend on the bytes alone and never on how the
//! PTY reader split them.
//!
//! Why the cursor's row is enough, operation by operation, for what `vt100`
//! 0.18.1 implements: a printed character lands on the cursor's row (a wrap
//! moves the cursor first); insert, delete and erase character and
//! erase-in-line change only the cursor's row; erase-in-display changes the
//! cursor's row in part and blanks other rows whole; scrolls and line moves
//! move whole rows without changing their cells; switching screens shows rows
//! that were checked when they were printed. So every change to a row's cells
//! is followed by a check of that row while the cursor is on it, and a row
//! blanked whole cannot match. A recogniser also names the one row every
//! measured launch draws its header on: a row moved there whole (a scroll, an
//! inserted line) is not checked again, so a header-like text drawn on
//! another row and moved is missed rather than read, and the header itself is
//! read when it is drawn, on its row.
//! The scanner works to a budget ([`SCANNER_WORK_PER_PUSH`],
//! [`SCANNER_WORK_TOTAL`]) counted in [`crate::vt_work`]'s units before each
//! byte is parsed, so a pane that prints whole-grid operations without pause
//! at a very large size costs its push at most the per-push limit: the
//! scanner is dropped at the first byte that would pass a limit, and a
//! generation not yet latched reads as `scanner_budget`.
//!
//! Not seen: a repeat (`CSI b`) that prints across a wrap writes several rows
//! in one byte and only the row the cursor ends on is checked (none of the
//! 372 measured traces uses one); a resize that truncates a span. Either way
//! the header can be missed and the release stays unknown; a release is
//! latched only from a complete match on its row and column.

use semver::Version;

use crate::event::AgentType;

/// The most cells (rows × columns) a scanner's grid may hold. A `vt100`
/// 0.18.1 cell is 32 bytes (asserted below), so one grid's cells take at most
/// 8 MiB, and a pane's at most 16 MiB with the alternate screen. That is cell
/// storage, not a measured heap ceiling.
pub const SCANNER_CELLS_MAX: u32 = 1 << 18;

// `vt100` asserts this itself (`cell.rs`); repeating it here makes a crate
// update that changes the cell size fail this build, where the budget above
// is stated in it.
const _: () = assert!(std::mem::size_of::<vt100::Cell>() == 32);

/// How the end of a header's version is recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionEnd {
    /// The cell after the version holds this character.
    Char(char),
    /// The cell after the version is blank and the cursor is not on it: the
    /// agent has moved on from writing the version (Claude Code ends its
    /// version with a carriage return), so a version still being written,
    /// `2.1.3` on the way to `2.1.310`, does not match.
    BlankPassed,
}

/// One measured start-up header: `text` at `col` on `row`, then a version
/// `MAJOR.MINOR.PATCH` (digits only: a pre-release tag does not match), then
/// `end`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderRecogniser {
    pub agent: AgentType,
    /// The row the header is drawn on. Every measured launch draws it on the
    /// same row, so a header-like text elsewhere is not one.
    pub row: u16,
    /// The cell column of `text`'s first character.
    pub col: u16,
    /// Everything from the anchor to the version, blanks included.
    pub text: &'static str,
    pub end: VersionEnd,
}

/// Codex's header, measured on 0.160.0 and 0.164.0 at 50 × 200 and 24 × 80,
/// and through the deck's wrapper and `devbox run codex-big`:
/// `  >_ OpenAI Codex (v0.164.0)`, `OpenAI` at row 1, column 5.
pub const CODEX_HEADER: HeaderRecogniser = HeaderRecogniser {
    agent: AgentType::Codex,
    row: 1,
    col: 5,
    text: "OpenAI Codex (v",
    end: VersionEnd::Char(')'),
};

/// Claude Code's header, measured on 2.1.310: `Claude Code` at row 1,
/// column 11 beside the logo, one blank, then `v2.1.310` at column 23, ended
/// by a carriage return.
pub const CLAUDE_HEADER: HeaderRecogniser = HeaderRecogniser {
    agent: AgentType::ClaudeCode,
    row: 1,
    col: 11,
    text: "Claude Code v",
    end: VersionEnd::BlankPassed,
};

/// Every header the deck recognises.
pub const HEADER_RECOGNISERS: &[HeaderRecogniser] = &[CODEX_HEADER, CLAUDE_HEADER];

/// A cell's text, with an untouched cell read as a blank.
fn cell_char(screen: &vt100::Screen, row: u16, col: u16) -> Option<char> {
    let cell = screen.cell(row, col)?;
    if cell.is_wide_continuation() {
        return None;
    }
    let mut chars = cell.contents().chars();
    match (chars.next(), chars.next()) {
        (None, _) => Some(' '),
        (Some(c), None) => Some(c),
        _ => None,
    }
}

impl HeaderRecogniser {
    /// The release this header shows on `row`, if the whole pattern is there.
    pub fn read(&self, screen: &vt100::Screen, row: u16) -> Option<Version> {
        if row != self.row {
            return None;
        }
        let mut text = self.text.chars();
        let first = text.next()?;
        if cell_char(screen, row, self.col)? != first {
            return None;
        }
        let mut col = self.col + 1;
        for expected in text {
            if cell_char(screen, row, col)? != expected {
                return None;
            }
            col += 1;
        }
        // MAJOR.MINOR.PATCH, digits only.
        let mut version = String::new();
        let mut dots = 0;
        let mut digits_in_part = 0;
        loop {
            let c = cell_char(screen, row, col)?;
            if c.is_ascii_digit() {
                digits_in_part += 1;
            } else if c == '.' && digits_in_part > 0 && dots < 2 {
                dots += 1;
                digits_in_part = 0;
            } else {
                break;
            }
            version.push(c);
            col += 1;
        }
        if dots != 2 || digits_in_part == 0 {
            return None;
        }
        let end = cell_char(screen, row, col)?;
        let ended = match self.end {
            VersionEnd::Char(c) => end == c,
            VersionEnd::BlankPassed => {
                let (cursor_row, cursor_col) = screen.cursor_position();
                end == ' ' && !(cursor_row == row && cursor_col == col)
            }
        };
        if !ended {
            return None;
        }
        Version::parse(&version).ok()
    }
}

/// What the scanner has latched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanLatch {
    /// No header seen yet.
    Pending,
    /// One release of `agent` seen, completed at stream offset `at` (bytes
    /// from the generation's first byte, the completing byte included).
    Latched {
        agent: AgentType,
        release: Version,
        at: u64,
    },
    /// A second, different header seen after the first: another release, or
    /// another agent's header.
    Conflict {
        first: (AgentType, Version),
        second: (AgentType, Version),
        at: u64,
    },
}

/// The grid is over [`SCANNER_CELLS_MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverBudget {
    pub rows: u16,
    pub cols: u16,
}

/// The most work, in [`crate::vt_work`] units, the scanner may do for one
/// generation (Q25): 48 ms of CPU at the dearest cost per unit measured
/// (0.57 ns, `src/agent_pty_scanner_budget.rs`, on the recorded launches and
/// fifteen pathological traces). The most any recorded launch uses, at
/// 50 × 200 or 80 × 300, is 12.6 million.
pub const SCANNER_WORK_TOTAL: u64 = 85_000_000;

/// The most work the scanner may do inside one push (Q25): 2.9 ms of CPU at
/// the same cost per unit, so that with the evidence parser's bound
/// ([`crate::evidence_state::EVIDENCE_WORK_PER_PUSH`]) a push's added lock
/// hold stays under plan §4.6's 10 ms with room for a slower machine. The
/// largest push of any recorded launch uses 0.56 million.
pub const SCANNER_WORK_PER_PUSH: u64 = 5_000_000;

/// Which of the scanner's work limits a byte would have passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkLimit {
    /// [`SCANNER_WORK_PER_PUSH`].
    Push,
    /// [`SCANNER_WORK_TOTAL`].
    Total,
}

impl WorkLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkLimit::Push => "push",
            WorkLimit::Total => "total",
        }
    }
}

/// A generation's own header parser. See the module documentation.
pub struct ReleaseScanner {
    parser: crate::vt_work::MeteredParser,
    recognisers: &'static [HeaderRecogniser],
    fed: u64,
    latch: ScanLatch,
    /// The limit a byte would have passed. Set once; the scanner is fed
    /// nothing after it.
    over: Option<WorkLimit>,
}

fn within_budget(rows: u16, cols: u16) -> Result<(), OverBudget> {
    if u32::from(rows.max(1)) * u32::from(cols.max(1)) > SCANNER_CELLS_MAX {
        Err(OverBudget { rows, cols })
    } else {
        Ok(())
    }
}

impl ReleaseScanner {
    /// A scanner at the spawn geometry, or none when the grid would be over
    /// [`SCANNER_CELLS_MAX`].
    pub fn new(
        rows: u16,
        cols: u16,
        recognisers: &'static [HeaderRecogniser],
    ) -> Result<Self, OverBudget> {
        within_budget(rows, cols)?;
        Ok(Self {
            parser: crate::vt_work::MeteredParser::new(rows, cols, 0),
            recognisers,
            fed: 0,
            latch: ScanLatch::Pending,
            over: None,
        })
    }

    /// Feed one push's bytes in stream order, checking the cursor's row after
    /// each. Returns whether the latch changed.
    ///
    /// Each byte is priced before it is parsed ([`crate::vt_work`]). The
    /// first byte that would take this push past [`SCANNER_WORK_PER_PUSH`],
    /// or the generation past [`SCANNER_WORK_TOTAL`], is not parsed, and
    /// neither is anything after it, in this push or a later one
    /// ([`Self::over_budget`]); the latch keeps what it had.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let before = self.latch.clone();
        if self.over.is_some() {
            return false;
        }
        let push_limit = self.parser.spent().saturating_add(SCANNER_WORK_PER_PUSH);
        let (limit, which) = if push_limit < SCANNER_WORK_TOTAL {
            (push_limit, WorkLimit::Push)
        } else {
            (SCANNER_WORK_TOTAL, WorkLimit::Total)
        };
        for &byte in bytes {
            if !self.parser.feed_byte(byte, limit) {
                self.over = Some(which);
                break;
            }
            self.fed += 1;
            self.check();
        }
        self.latch != before
    }

    /// The work limit a byte would have passed, once one has.
    pub fn over_budget(&self) -> Option<WorkLimit> {
        self.over
    }

    /// The work done so far, in [`crate::vt_work`] units.
    pub fn work(&self) -> u64 {
        self.parser.spent()
    }

    fn check(&mut self) {
        if matches!(self.latch, ScanLatch::Conflict { .. }) {
            return;
        }
        let screen = self.parser.screen();
        let (row, _) = screen.cursor_position();
        for recogniser in self.recognisers {
            let Some(release) = recogniser.read(screen, row) else {
                continue;
            };
            match &self.latch {
                ScanLatch::Pending => {
                    self.latch = ScanLatch::Latched {
                        agent: recogniser.agent.clone(),
                        release,
                        at: self.fed,
                    };
                }
                ScanLatch::Latched {
                    agent,
                    release: first,
                    ..
                } if *agent != recogniser.agent || *first != release => {
                    self.latch = ScanLatch::Conflict {
                        first: (agent.clone(), first.clone()),
                        second: (recogniser.agent.clone(), release),
                        at: self.fed,
                    };
                    return;
                }
                _ => {}
            }
        }
    }

    /// Resize the grid at this place in the stream, or report the new size
    /// over budget (the caller then drops the scanner).
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), OverBudget> {
        within_budget(rows, cols)?;
        crate::vt_grid::set_size(self.parser.screen_mut(), rows, cols);
        Ok(())
    }

    pub fn latch(&self) -> &ScanLatch {
        &self.latch
    }

    /// Bytes fed so far.
    pub fn fed(&self) -> u64 {
        self.fed
    }

    /// The rows of scrollback the parser keeps: none.
    pub fn scrollback_capacity(&self) -> usize {
        self.parser.screen().scrollback_capacity()
    }
}

/// The reference the scanner must equal: `trace` fed one byte per call at
/// `rows` × `cols`.
pub fn latch_byte_at_a_time(trace: &[u8], rows: u16, cols: u16) -> Option<ScanLatch> {
    let mut scanner = ReleaseScanner::new(rows, cols, HEADER_RECOGNISERS).ok()?;
    for byte in trace {
        scanner.feed(std::slice::from_ref(byte));
    }
    Some(scanner.latch().clone())
}

/// How many bytes of a generation's output the header window covers.
pub const RELEASE_WINDOW_BYTES: u64 = 262_144;

/// Why a generation's header window closed, in the words the `pane release`
/// log line uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowClose {
    /// The generation's first `UserPromptSubmit` hook.
    FirstPrompt,
    /// [`RELEASE_WINDOW_BYTES`] fed.
    ByteLimit,
    /// The PTY reader ended.
    AgentExit,
    /// The generation was retired.
    Retired,
    /// The pane's geometry went over [`SCANNER_CELLS_MAX`].
    Geometry,
    /// A byte would have taken the scanner past one of its work limits
    /// (Q25).
    WorkBudget,
}

impl WindowClose {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowClose::FirstPrompt => "closed at first prompt",
            WindowClose::ByteLimit => "closed at byte limit",
            WindowClose::AgentExit => "closed at agent exit",
            WindowClose::Retired => "closed at retirement",
            WindowClose::Geometry => "scanner dropped: geometry over budget",
            WindowClose::WorkBudget => "scanner dropped: work over budget",
        }
    }
}

/// A change to a generation's release capture, for the `pane release` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureEvent {
    Latched(ScanLatch),
    Closed {
        reason: WindowClose,
        latch: ScanLatch,
        /// What the scanner cost the generation while its window was open,
        /// for the `release scanner timing` line.
        cost: ScanCost,
    },
}

/// What a generation's scanner cost while its window was open (plan §9.1):
/// the time each push spent in the scanner, which is time the push holds the
/// pane's output lock for, and the scanner's own CPU time. Resizes are kept
/// apart and charged to no push.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanCost {
    /// Pushes the scanner was fed.
    pub pushes: u64,
    /// Wall time inside the scanner, over every push.
    pub push_ns_total: u64,
    /// The longest single push inside the scanner.
    pub push_ns_max: u64,
    /// The scanner's CPU time (`CLOCK_THREAD_CPUTIME_ID` around its calls),
    /// summed; 0 where the platform has no thread clock.
    pub cpu_ns: u64,
    pub resizes: u64,
    /// The longest resize of the scanner's grid.
    pub resize_ns_max: u64,
    /// The scanner's work, in [`crate::vt_work`] units, over every push.
    pub work: u64,
    /// The most work any one push gave it.
    pub push_work_max: u64,
    /// The work limit a byte would have passed, if one did.
    pub over: Option<WorkLimit>,
}

/// The calling thread's CPU time, in nanoseconds.
#[cfg(unix)]
pub fn thread_cpu_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a stack-allocated `timespec` the call writes into.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (rc == 0).then(|| ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

/// No thread clock off Unix: the CPU figure reads 0.
#[cfg(not(unix))]
pub fn thread_cpu_ns() -> Option<u64> {
    None
}

/// A generation's release, latched from its start-up header, and the scanner
/// that reads it while the header window is open (plan §4.6). The latch lives
/// as long as the generation; the scanner is dropped at the first of the
/// window's close (first prompt, byte limit), the generation's retirement,
/// the end of its PTY reader, a geometry over [`SCANNER_CELLS_MAX`], or a byte
/// over its work budget (Q25).
pub struct ReleaseCapture {
    scanner: Option<ReleaseScanner>,
    latch: ScanLatch,
    closed: Option<WindowClose>,
    cost: ScanCost,
    /// The last push's wall and CPU time inside the scanner, while it was
    /// alive: what the budget measurement reads push by push.
    last_push: Option<(u64, u64)>,
}

impl std::fmt::Debug for ReleaseCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseCapture")
            .field("scanner", &self.scanner.is_some())
            .field("latch", &self.latch)
            .field("closed", &self.closed)
            .field("cost", &self.cost)
            .finish()
    }
}

impl ReleaseCapture {
    /// A capture for a generation spawned at `rows` × `cols`.
    pub fn start(rows: u16, cols: u16) -> Self {
        match ReleaseScanner::new(rows, cols, HEADER_RECOGNISERS) {
            Ok(scanner) => Self {
                scanner: Some(scanner),
                latch: ScanLatch::Pending,
                closed: None,
                cost: ScanCost::default(),
                last_push: None,
            },
            Err(_) => Self {
                scanner: None,
                latch: ScanLatch::Pending,
                closed: Some(WindowClose::Geometry),
                cost: ScanCost::default(),
                last_push: None,
            },
        }
    }

    /// A capture with no scanner and no window: what the budget measurement
    /// compares a scanning push against.
    pub fn disabled() -> Self {
        Self {
            scanner: None,
            latch: ScanLatch::Pending,
            closed: None,
            cost: ScanCost::default(),
            last_push: None,
        }
    }

    /// Bytes the agent wrote, in stream order: fed to the scanner up to the
    /// window's byte limit.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<CaptureEvent> {
        let mut events = Vec::new();
        self.last_push = None;
        let Some(scanner) = &mut self.scanner else {
            return events;
        };
        let room = RELEASE_WINDOW_BYTES.saturating_sub(scanner.fed()) as usize;
        let take = room.min(bytes.len());
        let started = std::time::Instant::now();
        let cpu_before = thread_cpu_ns();
        let work_before = scanner.work();
        let changed = scanner.feed(&bytes[..take]);
        let cpu = thread_cpu_ns()
            .zip(cpu_before)
            .map_or(0, |(after, before)| after.saturating_sub(before));
        let wall = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let work = scanner.work() - work_before;
        self.cost.pushes += 1;
        self.cost.push_ns_total = self.cost.push_ns_total.saturating_add(wall);
        self.cost.push_ns_max = self.cost.push_ns_max.max(wall);
        self.cost.cpu_ns = self.cost.cpu_ns.saturating_add(cpu);
        self.cost.work = scanner.work();
        self.cost.push_work_max = self.cost.push_work_max.max(work);
        self.cost.over = scanner.over_budget();
        self.last_push = Some((wall, cpu));
        if changed {
            self.latch = scanner.latch().clone();
            events.push(CaptureEvent::Latched(self.latch.clone()));
        }
        if scanner.over_budget().is_some() {
            events.extend(self.close(WindowClose::WorkBudget));
        } else if scanner.fed() >= RELEASE_WINDOW_BYTES {
            events.extend(self.close(WindowClose::ByteLimit));
        }
        events
    }

    /// A layout edge to `rows` × `cols` at this place in the stream.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Option<CaptureEvent> {
        let scanner = self.scanner.as_mut()?;
        let started = std::time::Instant::now();
        let over = scanner.resize(rows, cols).is_err();
        let wall = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.cost.resizes += 1;
        self.cost.resize_ns_max = self.cost.resize_ns_max.max(wall);
        if over {
            return self.close(WindowClose::Geometry);
        }
        None
    }

    /// Close the window: the scanner is dropped; the latch stays.
    pub fn close(&mut self, reason: WindowClose) -> Option<CaptureEvent> {
        self.scanner.take()?;
        self.closed = Some(reason);
        Some(CaptureEvent::Closed {
            reason,
            latch: self.latch.clone(),
            cost: self.cost,
        })
    }

    /// What the scanner has cost the generation so far.
    pub fn cost(&self) -> ScanCost {
        self.cost
    }

    /// The last push's wall and CPU nanoseconds inside the scanner, if the
    /// scanner was alive for it.
    pub fn last_push(&self) -> Option<(u64, u64)> {
        self.last_push
    }

    pub fn latch(&self) -> &ScanLatch {
        &self.latch
    }

    pub fn closed(&self) -> Option<WindowClose> {
        self.closed
    }

    pub fn scanner_alive(&self) -> bool {
        self.scanner.is_some()
    }

    /// The live scanner's scrollback rows, while it is alive.
    pub fn scanner_scrollback(&self) -> Option<usize> {
        self.scanner
            .as_ref()
            .map(ReleaseScanner::scrollback_capacity)
    }

    /// The generation's release for the activation table, as `agent`'s: a
    /// header of another agent is no release of this one.
    pub fn latched_for(&self, agent: &AgentType) -> crate::doorbell_activation::LatchedRelease {
        use crate::doorbell_activation::LatchedRelease;
        match &self.latch {
            ScanLatch::Latched {
                agent: seen,
                release,
                ..
            } if seen == agent => LatchedRelease::Known(release.clone()),
            ScanLatch::Conflict { .. } => LatchedRelease::Conflict,
            ScanLatch::Pending if self.closed == Some(WindowClose::Geometry) => {
                LatchedRelease::ScannerGeometry
            }
            ScanLatch::Pending if self.closed == Some(WindowClose::WorkBudget) => {
                LatchedRelease::ScannerBudget
            }
            _ => LatchedRelease::Unseen,
        }
    }
}
