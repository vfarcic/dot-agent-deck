//! PRD #1666 part 3, round 5-wake (plan §2.2): whether the frame the deck
//! reads for an agent was drawn for the terminal the agent has now.
//!
//! A resize, or a nudge's one-column shrink and restore, is a **layout edge**:
//! bytes the agent wrote for the old size can still arrive after it, and
//! parsing them at the new size draws a screen the agent never drew. So each
//! profiled agent keeps an [`EvidenceState`] inside its bus lock, next to the
//! scrollback ring, and every edge is recorded at its exact place in the
//! stream (the ioctl runs under the same lock `push` takes).
//!
//! From an edge the state is **Dirty** and no parser runs. It waits for the
//! profile's full-redraw marker in the bytes pushed after the edge; a fresh
//! parser at the new geometry starts at the marker, with the terminal modes in
//! force there, and bytes before it are never parsed. The state turns
//! **Clean** only once a frame from that parser shows the profile's width
//! witness, a row the agent ties to the terminal's width, and a witness row
//! counts only once the stream has moved the cursor off it, so a PTY read
//! that ends inside a row drawn for a wider terminal is never taken for one
//! drawn for this width. The tested scope (`inbox/doorbell/044`): Claude
//! Code's rule and Codex's padded choice and turn rows drawn one or twenty
//! columns too wide stay pending whole, cut at any byte and pushed a byte at a
//! time, and Codex's start-up logo stays pending after a move between the two
//! widths that draw it alike. A row drawn in pieces with a cursor move at the
//! new edge (off the row, or back along it) is not always refused
//! (`src/doorbell_screen.rs`, on the padded row, says when); every measured
//! redraw drew its rows in one run. A generation starts the same way from its first byte at the spawn geometry,
//! needing the witness but no marker. A nudge whose restore fails leaves the
//! width unknown until the next successful resize.
//!
//! Nothing turns Dirty into Clean because time passed. An agent that redraws
//! nothing after an edge stays Dirty.
//!
//! The parser's grid is bounded like the release scanner's
//! ([`EVIDENCE_CELLS_MAX`]): a geometry over it, at the spawn or at an edge,
//! builds no parser and leaves the state [`EvidenceStatus::OverBudget`],
//! which recognises nothing, until an edge back within it. While Pending, the
//! witness is read straight off the parser's screen, with no copy of it.
//!
//! The parser's work is bounded too (Q25): each byte is priced in
//! [`crate::vt_work`]'s units before it is parsed, and the first byte that
//! would take one push past [`EVIDENCE_WORK_PER_PUSH`] is not parsed. The
//! parser is dropped there and the state is
//! [`EvidenceStatus::WorkOverBudget`], which recognises nothing, until the
//! next layout edge starts the evidence over.

use crate::doorbell_screen::{Grid, ScreenProfile, WidthsSeen, find_marker};

/// The most cells (rows × cols) the evidence parser's grid may hold: the
/// release scanner's bound ([`crate::release_scanner::SCANNER_CELLS_MAX`]).
pub const EVIDENCE_CELLS_MAX: u32 = crate::release_scanner::SCANNER_CELLS_MAX;

/// The most work, in [`crate::vt_work`] units, the evidence parser may do
/// inside one push (Q25): the release scanner's per-push bound
/// ([`crate::release_scanner::SCANNER_WORK_PER_PUSH`]), 2.9 ms of CPU at the
/// dearest cost per unit measured (`src/agent_pty_scanner_budget.rs`).
pub const EVIDENCE_WORK_PER_PUSH: u64 = crate::release_scanner::SCANNER_WORK_PER_PUSH;

fn within_budget(rows: u16, cols: u16) -> bool {
    u32::from(rows.max(1)) * u32::from(cols.max(1)) <= EVIDENCE_CELLS_MAX
}

/// What moved the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeReason {
    /// A real resize (`AgentPtyRegistry::resize`'s viewer policy).
    Resize,
    /// A repaint nudge's one-column shrink.
    NudgeShrink,
    /// A repaint nudge's restore.
    NudgeRestore,
    /// The generation's agent type was learned from its own hooks after it
    /// started, and with it the profile: nothing parsed before is trusted.
    ProfileLearned,
}

impl EdgeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeReason::Resize => "resize",
            EdgeReason::NudgeShrink => "nudge_shrink",
            EdgeReason::NudgeRestore => "nudge_restore",
            EdgeReason::ProfileLearned => "profile_learned",
        }
    }
}

/// Where the evidence stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceStatus {
    /// The agent has no measured screen profile: nothing is tracked.
    Unprofiled,
    /// A parser runs, from the generation's first byte (`after_edge: None`)
    /// or from a redraw marker after an edge, and no frame has shown the
    /// width witness yet.
    Pending {
        since_seq: u64,
        after_edge: Option<u64>,
    },
    /// The parser's frames are the agent's own for this width, since
    /// `since_seq`.
    Clean { since_seq: u64 },
    /// A layout edge at `edge_seq`; waiting for the redraw marker.
    Dirty { edge_seq: u64, reason: EdgeReason },
    /// A nudge's restore failed at `since_seq`: the terminal's width is not
    /// known until the next successful resize.
    GeometryUnknown { since_seq: u64 },
    /// The geometry in force since `since_seq` is over
    /// [`EVIDENCE_CELLS_MAX`]: no parser runs and nothing is recognised until
    /// an edge back within it.
    OverBudget { since_seq: u64 },
    /// A byte at `since_seq` would have taken one push past
    /// [`EVIDENCE_WORK_PER_PUSH`]: the parser was dropped there, and nothing
    /// is recognised until the next layout edge.
    WorkOverBudget { since_seq: u64 },
}

impl EvidenceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceStatus::Unprofiled => "unprofiled",
            EvidenceStatus::Pending { .. } => "pending",
            EvidenceStatus::Clean { .. } => "clean",
            EvidenceStatus::Dirty { .. } => "dirty",
            EvidenceStatus::GeometryUnknown { .. } => "geometry_unknown",
            EvidenceStatus::OverBudget { .. } => "geometry_over_budget",
            EvidenceStatus::WorkOverBudget { .. } => "work_over_budget",
        }
    }
}

/// A change of [`EvidenceStatus`], for the `pane layout` log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub from: EvidenceStatus,
    pub to: EvidenceStatus,
    pub layout_epoch: u64,
    pub out_seq: u64,
    /// The stream offset of the redraw marker that started the parser, when
    /// this transition began from one.
    pub marker_seq: Option<u64>,
}

/// Where a Clean frame stood when it was read (R-D-U2-01): its layout epoch
/// and its place in the output. A layout edge or an unknown geometry moves
/// the epoch and invalidates the frame without moving the output position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMark {
    pub layout_epoch: u64,
    pub out_seq: u64,
}

/// One agent's evidence state. See the module documentation.
pub struct EvidenceState {
    profile: Option<&'static ScreenProfile>,
    rows: u16,
    cols: u16,
    layout_epoch: u64,
    out_seq: u64,
    status: EvidenceStatus,
    parser: Option<crate::vt_work::MeteredParser>,
    /// Every width the terminal has had, for the witness.
    widths: WidthsSeen,
    /// While Dirty: the last bytes pushed, one fewer than the marker, so a
    /// marker split across two pushes is still found.
    tail: Vec<u8>,
}

impl std::fmt::Debug for EvidenceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvidenceState")
            .field("profile", &self.profile.map(|p| p.id))
            .field("geometry", &(self.rows, self.cols))
            .field("layout_epoch", &self.layout_epoch)
            .field("out_seq", &self.out_seq)
            .field("status", &self.status)
            .finish()
    }
}

impl EvidenceState {
    /// The state of a generation starting at `rows` × `cols`.
    pub fn new(profile: Option<&'static ScreenProfile>, rows: u16, cols: u16) -> Self {
        let (status, parser) = match profile {
            Some(_) if !within_budget(rows, cols) => {
                (EvidenceStatus::OverBudget { since_seq: 0 }, None)
            }
            Some(_) => (
                EvidenceStatus::Pending {
                    since_seq: 0,
                    after_edge: None,
                },
                Some(crate::vt_work::MeteredParser::new(rows, cols, 0)),
            ),
            None => (EvidenceStatus::Unprofiled, None),
        };
        Self {
            profile,
            rows,
            cols,
            layout_epoch: 0,
            out_seq: 0,
            status,
            parser,
            widths: WidthsSeen::of([cols]),
            tail: Vec::new(),
        }
    }

    /// The cells the evidence parser holds now (0 with none running).
    pub fn parser_cells(&self) -> u32 {
        self.parser.as_ref().map_or(0, |parser| {
            let (rows, cols) = crate::vt_grid::size(parser.screen());
            u32::from(rows) * u32::from(cols)
        })
    }

    pub fn status(&self) -> EvidenceStatus {
        self.status
    }

    pub fn layout_epoch(&self) -> u64 {
        self.layout_epoch
    }

    pub fn out_seq(&self) -> u64 {
        self.out_seq
    }

    pub fn geometry(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// The current frame, read losslessly, while the state is Clean, with
    /// where it stands (R-D-U2-01): its layout epoch and output position,
    /// read with it.
    pub fn clean_frame_marked(&self) -> Option<(Grid, FrameMark)> {
        let grid = self.clean_frame()?;
        Some((
            grid,
            FrameMark {
                layout_epoch: self.layout_epoch,
                out_seq: self.out_seq,
            },
        ))
    }

    /// Whether the Clean frame `mark` was read from still stands
    /// (R-D-U2-01): the state is Clean now, in the same layout epoch, with
    /// no output taken since. Comparing the output position alone misses a
    /// layout edge or an unknown geometry, which invalidate the frame
    /// without moving it. [`Self::adopt`] moves the layout epoch too, but
    /// never under a mark: only a state with a profile is ever Clean, and
    /// `adopt` changes only one with none.
    pub fn frame_unchanged(&self, mark: FrameMark) -> bool {
        matches!(self.status, EvidenceStatus::Clean { .. })
            && self.parser.is_some()
            && self.layout_epoch == mark.layout_epoch
            && self.out_seq == mark.out_seq
    }

    /// The current frame, read losslessly, while the state is Clean.
    pub fn clean_frame(&self) -> Option<Grid> {
        match (self.status, &self.parser) {
            (EvidenceStatus::Clean { .. }, Some(parser)) => {
                Some(Grid::of(parser.screen(), self.out_seq))
            }
            _ => None,
        }
    }

    fn transition(&mut self, to: EvidenceStatus, marker_seq: Option<u64>) -> Option<Transition> {
        let from = self.status;
        self.status = to;
        (from != to).then_some(Transition {
            from,
            to,
            layout_epoch: self.layout_epoch,
            out_seq: self.out_seq,
            marker_seq,
        })
    }

    /// Bytes the agent wrote, in stream order. `modes_at` gives the terminal
    /// modes preamble in force just before the byte at the given index of
    /// `bytes`, for a parser that starts at a redraw marker there.
    pub fn push(
        &mut self,
        bytes: &[u8],
        modes_at: impl FnOnce(usize) -> Vec<u8>,
    ) -> Option<Transition> {
        let Some(profile) = self.profile else {
            self.out_seq += bytes.len() as u64;
            return None;
        };
        match self.status {
            EvidenceStatus::Unprofiled
            | EvidenceStatus::GeometryUnknown { .. }
            | EvidenceStatus::OverBudget { .. }
            | EvidenceStatus::WorkOverBudget { .. } => {
                self.out_seq += bytes.len() as u64;
                None
            }
            EvidenceStatus::Pending { since_seq, .. } => {
                if let Some(over) = self.feed(bytes) {
                    return over;
                }
                self.check_witness(profile, since_seq, None)
            }
            EvidenceStatus::Clean { .. } => self.feed(bytes).flatten(),
            EvidenceStatus::Dirty { .. } => {
                let marker = profile.redraw_marker;
                let mut window = std::mem::take(&mut self.tail);
                let carried = window.len();
                window.extend_from_slice(bytes);
                let Some(at) = find_marker(&window, marker) else {
                    let keep = marker.len().saturating_sub(1).min(window.len());
                    self.tail = window[window.len() - keep..].to_vec();
                    self.out_seq += bytes.len() as u64;
                    return None;
                };
                // The marker can begin in the carried tail; the parser starts
                // at its first byte either way.
                let base = self.out_seq;
                let marker_seq = base - carried as u64 + at as u64;
                let mut parser = crate::vt_work::MeteredParser::new(self.rows, self.cols, 0);
                let modes = modes_at(at.saturating_sub(carried));
                let fed = modes
                    .iter()
                    .chain(&window[at..])
                    .position(|&b| !parser.feed_byte(b, EVIDENCE_WORK_PER_PUSH));
                if let Some(refused) = fed {
                    // The refused byte's place in the stream: the preamble
                    // is the deck's own and has none, so it is charged to
                    // the marker.
                    let past_marker = refused.saturating_sub(modes.len()) as u64;
                    self.out_seq = base + bytes.len() as u64;
                    return self.transition(
                        EvidenceStatus::WorkOverBudget {
                            since_seq: marker_seq + past_marker,
                        },
                        Some(marker_seq),
                    );
                }
                self.parser = Some(parser);
                self.out_seq = base + bytes.len() as u64;
                let edge_seq = match self.status {
                    EvidenceStatus::Dirty { edge_seq, .. } => edge_seq,
                    _ => base,
                };
                let first = self.transition(
                    EvidenceStatus::Pending {
                        since_seq: marker_seq,
                        after_edge: Some(edge_seq),
                    },
                    Some(marker_seq),
                );
                let second = self.check_witness(profile, marker_seq, Some(marker_seq));
                second.or(first)
            }
        }
    }

    /// Feed one push to the running parser. `Some` when a byte would have
    /// taken the push past [`EVIDENCE_WORK_PER_PUSH`]: the parser is dropped
    /// and the state has moved to [`EvidenceStatus::WorkOverBudget`], with
    /// the transition if there was one.
    fn feed(&mut self, bytes: &[u8]) -> Option<Option<Transition>> {
        let base = self.out_seq;
        self.out_seq += bytes.len() as u64;
        let parser = self.parser.as_mut()?;
        let limit = parser.spent().saturating_add(EVIDENCE_WORK_PER_PUSH);
        let refused = bytes.iter().position(|&b| !parser.feed_byte(b, limit))?;
        self.parser = None;
        Some(self.transition(
            EvidenceStatus::WorkOverBudget {
                since_seq: base + refused as u64,
            },
            None,
        ))
    }

    fn check_witness(
        &mut self,
        profile: &ScreenProfile,
        since_seq: u64,
        marker_seq: Option<u64>,
    ) -> Option<Transition> {
        let parser = self.parser.as_ref()?;
        if (profile.width_witness)(parser.screen(), &self.widths) {
            return self.transition(EvidenceStatus::Clean { since_seq }, marker_seq);
        }
        None
    }

    /// A successful layout ioctl to `rows` × `cols`, at this place in the
    /// stream.
    pub fn edge(&mut self, reason: EdgeReason, rows: u16, cols: u16) -> Option<Transition> {
        self.rows = rows;
        self.cols = cols;
        self.layout_epoch += 1;
        self.widths.record(cols);
        self.profile?;
        self.parser = None;
        self.tail.clear();
        let to = if within_budget(rows, cols) {
            EvidenceStatus::Dirty {
                edge_seq: self.out_seq,
                reason,
            }
        } else {
            EvidenceStatus::OverBudget {
                since_seq: self.out_seq,
            }
        };
        self.transition(to, None)
    }

    /// The generation's profile, learned after it started (from the agent
    /// type its own hooks reported): an unprofiled state takes it up as a
    /// layout edge, Dirty until the profile's redraw marker, since none of
    /// what was drawn before was parsed. A state that has a profile keeps it.
    pub fn adopt(&mut self, profile: &'static ScreenProfile) -> Option<Transition> {
        if self.profile.is_some() {
            return None;
        }
        self.profile = Some(profile);
        self.layout_epoch += 1;
        self.parser = None;
        self.tail.clear();
        let to = if within_budget(self.rows, self.cols) {
            EvidenceStatus::Dirty {
                edge_seq: self.out_seq,
                reason: EdgeReason::ProfileLearned,
            }
        } else {
            EvidenceStatus::OverBudget {
                since_seq: self.out_seq,
            }
        };
        self.transition(to, None)
    }

    /// The profile the state tracks, if any.
    pub fn profile(&self) -> Option<&'static ScreenProfile> {
        self.profile
    }

    /// A nudge's restore ioctl failed: the width is unknown until the next
    /// successful one.
    pub fn geometry_unknown(&mut self) -> Option<Transition> {
        self.layout_epoch += 1;
        self.profile?;
        self.parser = None;
        self.tail.clear();
        self.transition(
            EvidenceStatus::GeometryUnknown {
                since_seq: self.out_seq,
            },
            None,
        )
    }
}
