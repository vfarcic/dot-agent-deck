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
//! time. A row drawn in pieces with a cursor move at the
//! new edge (off the row, or back along it) is not always refused
//! (`src/doorbell_screen.rs`, on the padded row, says when); every measured
//! redraw drew its rows in one run. A generation starts the same way from its first byte at the spawn geometry,
//! needing the witness but no marker. A nudge whose restore fails leaves the
//! width unknown until the next successful resize.
//!
//! **Two widths that draw alike** (issue #2124). Codex's start-up logo sits
//! in the same place at the widths `2k` and `2k + 1`, so once the terminal
//! has had both (a nudge's one-column shrink of an odd width is enough) the
//! logo cannot say which of the two a frame was drawn for. It then counts as
//! the profile's *pair witness*, and only while the bytes read the same at
//! both widths: alongside the parser, a twin parser at the pair's other width
//! reads every byte the parser reads from the same marker, and after each
//! push the two screens are compared ([`readings_agree`]: every cell of the
//! columns both widths have, the row wraps and the cursor, and nothing in the
//! wider one's last column). While they agree, the screen the parser holds is
//! the one the agent drew whichever of the two widths it drew for, since a
//! terminal of either width would show it the same. The first push after
//! which they differ ends the pair witness until the next layout edge: a
//! Pending state stays Pending, and a state Clean on the pair witness
//! returns to Pending unless a width witness shows by then
//! (`inbox/doorbell/044`). Frames are read only between pushes, so readings
//! that differ only inside a push are never read.
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

/// Whether two parsers that read the same bytes at the two widths of a pair
/// (`2k` and `2k + 1`) hold the same screen: the same cell (character,
/// attributes, wide halves) in every column both widths have, the same row
/// wraps, the same cursor position, and nothing in the wider one's last
/// column, which the narrower one does not have.
fn readings_agree(a: &vt100::Screen, b: &vt100::Screen) -> bool {
    let (rows, a_cols) = crate::vt_grid::size(a);
    let (b_rows, b_cols) = crate::vt_grid::size(b);
    if rows != b_rows || a.cursor_position() != b.cursor_position() {
        return false;
    }
    let shared = a_cols.min(b_cols);
    let (wide, wide_cols) = if a_cols > b_cols {
        (a, a_cols)
    } else {
        (b, b_cols)
    };
    (0..rows).all(|r| {
        a.row_wrapped(r) == b.row_wrapped(r)
            && (0..shared).all(|c| a.cell(r, c) == b.cell(r, c))
            && (wide_cols == shared
                || wide
                    .cell(r, wide_cols - 1)
                    .is_none_or(|c| !c.has_contents() && !c.is_wide_continuation()))
    })
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
    /// While the parser runs at a width whose pair partner the terminal has
    /// also had (`2k` and `2k + 1`): whether the twin, reading the same bytes
    /// at the partner width, has held the same screen as the parser after
    /// every push since both started ([`readings_agree`]), the condition
    /// under which the profile's pair witness counts. `None` when the pair is
    /// not in question, or once the state is Clean on a width witness.
    pair_agree: Option<bool>,
    /// The twin parser at the pair partner's width, while `pair_agree` is
    /// `Some(true)`.
    twin: Option<crate::vt_work::MeteredParser>,
    /// The Pending status a Clean resting on the pair witness came from:
    /// the state returns to it once the twin's reading differs and no width
    /// witness shows. `None` when Clean rests on a width witness.
    pair_held: Option<EvidenceStatus>,
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
            pair_agree: None,
            twin: None,
            pair_held: None,
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

    /// The width now and every width the terminal has had, ascending: what
    /// a width witness reads ([`WidthsSeen`]), for a diagnosis line.
    pub fn widths(&self) -> (u16, Vec<u16>) {
        (self.cols, self.widths.list())
    }

    /// Whether the state is Clean on the profile's pair witness, which
    /// holds only while the twin reads the same screen.
    pub fn pair_held(&self) -> bool {
        self.pair_held.is_some()
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
            EvidenceStatus::Clean { .. } => {
                if let Some(over) = self.feed(bytes) {
                    return over;
                }
                self.recheck_pair(profile)
            }
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
                self.start_twin(modes.iter().chain(&window[at..]));
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
        let Some(refused) = bytes.iter().position(|&b| !parser.feed_byte(b, limit)) else {
            self.feed_twin(bytes);
            return None;
        };
        self.parser = None;
        self.twin = None;
        self.pair_agree = None;
        self.pair_held = None;
        Some(self.transition(
            EvidenceStatus::WorkOverBudget {
                since_seq: base + refused as u64,
            },
            None,
        ))
    }

    /// Start the twin at the pair partner's width, when the terminal has had
    /// both widths of the current one's pair, and feed it what the parser
    /// was just started on.
    fn start_twin<'a>(&mut self, bytes: impl IntoIterator<Item = &'a u8>) {
        self.twin = None;
        self.pair_held = None;
        let partner = self.cols ^ 1;
        self.pair_agree = self.widths.had(partner).then_some(true);
        if self.pair_agree.is_none() {
            return;
        }
        if partner == 0 || !within_budget(self.rows, partner) {
            self.pair_agree = Some(false);
            return;
        }
        self.twin = Some(crate::vt_work::MeteredParser::new(self.rows, partner, 0));
        self.feed_twin(bytes);
    }

    /// Feed the twin the bytes the parser just took, then compare the two
    /// readings: the first push after which they differ, or the twin's work
    /// bound refuses a byte, drops the twin and spends the pair witness until
    /// the next edge. Frames are read between pushes, so a reading that
    /// differs only inside a push is never read.
    fn feed_twin<'a>(&mut self, bytes: impl IntoIterator<Item = &'a u8>) {
        let Some(twin) = self.twin.as_mut() else {
            return;
        };
        let limit = twin.spent().saturating_add(EVIDENCE_WORK_PER_PUSH);
        let refused = bytes.into_iter().any(|&b| !twin.feed_byte(b, limit));
        let agree = !refused
            && self
                .parser
                .as_ref()
                .is_some_and(|parser| readings_agree(parser.screen(), twin.screen()));
        if !agree {
            self.twin = None;
            self.pair_agree = Some(false);
        }
    }

    fn check_witness(
        &mut self,
        profile: &ScreenProfile,
        since_seq: u64,
        marker_seq: Option<u64>,
    ) -> Option<Transition> {
        let parser = self.parser.as_ref()?;
        if (profile.width_witness)(parser.screen(), &self.widths) {
            self.pair_agree = None;
            self.twin = None;
            self.pair_held = None;
            return self.transition(EvidenceStatus::Clean { since_seq }, marker_seq);
        }
        if self.pair_agree == Some(true) && (profile.pair_witness)(parser.screen()) {
            self.pair_held = Some(self.status);
            return self.transition(EvidenceStatus::Clean { since_seq }, marker_seq);
        }
        None
    }

    /// After a push to a state Clean on the pair witness: a width witness
    /// now makes it Clean on that witness, dropping the twin; otherwise it
    /// stays Clean while the two readings agree, and once they have differed
    /// the frame no longer reads the same for both widths of the pair, so it
    /// returns to the Pending it came from.
    fn recheck_pair(&mut self, profile: &ScreenProfile) -> Option<Transition> {
        let pending = self.pair_held?;
        let parser = self.parser.as_ref()?;
        if (profile.width_witness)(parser.screen(), &self.widths) {
            self.pair_held = None;
            self.pair_agree = None;
            self.twin = None;
            return None;
        }
        if self.pair_agree == Some(true) {
            return None;
        }
        self.pair_held = None;
        self.transition(pending, None)
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
        self.pair_agree = None;
        self.twin = None;
        self.pair_held = None;
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
        self.pair_agree = None;
        self.twin = None;
        self.pair_held = None;
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
        self.pair_agree = None;
        self.twin = None;
        self.pair_held = None;
        self.tail.clear();
        self.transition(
            EvidenceStatus::GeometryUnknown {
                since_seq: self.out_seq,
            },
            None,
        )
    }
}
