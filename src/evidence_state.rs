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
//! witness, a row the agent ties to the terminal's width, which rejects a full
//! redraw the agent drew for the old width. A generation starts the same way
//! from its first byte at the spawn geometry, needing the witness but no
//! marker. A nudge whose restore fails leaves the width unknown until the next
//! successful resize.
//!
//! Nothing turns Dirty into Clean because time passed. An agent that redraws
//! nothing after an edge stays Dirty.

use crate::doorbell_screen::{Grid, ScreenProfile, find_marker};

/// What moved the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeReason {
    /// A real resize (`AgentPtyRegistry::resize`'s viewer policy).
    Resize,
    /// A repaint nudge's one-column shrink.
    NudgeShrink,
    /// A repaint nudge's restore.
    NudgeRestore,
}

impl EdgeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeReason::Resize => "resize",
            EdgeReason::NudgeShrink => "nudge_shrink",
            EdgeReason::NudgeRestore => "nudge_restore",
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
}

impl EvidenceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceStatus::Unprofiled => "unprofiled",
            EvidenceStatus::Pending { .. } => "pending",
            EvidenceStatus::Clean { .. } => "clean",
            EvidenceStatus::Dirty { .. } => "dirty",
            EvidenceStatus::GeometryUnknown { .. } => "geometry_unknown",
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

/// One agent's evidence state. See the module documentation.
pub struct EvidenceState {
    profile: Option<&'static ScreenProfile>,
    rows: u16,
    cols: u16,
    layout_epoch: u64,
    out_seq: u64,
    status: EvidenceStatus,
    parser: Option<vt100::Parser>,
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
            Some(_) => (
                EvidenceStatus::Pending {
                    since_seq: 0,
                    after_edge: None,
                },
                Some(crate::vt_grid::parser(rows, cols, 0)),
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
            tail: Vec::new(),
        }
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
            EvidenceStatus::Unprofiled | EvidenceStatus::GeometryUnknown { .. } => {
                self.out_seq += bytes.len() as u64;
                None
            }
            EvidenceStatus::Pending { since_seq, .. } => {
                self.feed(bytes);
                self.check_witness(profile, since_seq, None)
            }
            EvidenceStatus::Clean { .. } => {
                self.feed(bytes);
                None
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
                let mut parser = crate::vt_grid::parser(self.rows, self.cols, 0);
                parser.process(&modes_at(at.saturating_sub(carried)));
                parser.process(&window[at..]);
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

    fn feed(&mut self, bytes: &[u8]) {
        if let Some(parser) = &mut self.parser {
            parser.process(bytes);
        }
        self.out_seq += bytes.len() as u64;
    }

    fn check_witness(
        &mut self,
        profile: &ScreenProfile,
        since_seq: u64,
        marker_seq: Option<u64>,
    ) -> Option<Transition> {
        let parser = self.parser.as_ref()?;
        let frame = Grid::of(parser.screen(), self.out_seq);
        if (profile.width_witness)(&frame) {
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
        self.profile?;
        self.parser = None;
        self.tail.clear();
        self.transition(
            EvidenceStatus::Dirty {
                edge_seq: self.out_seq,
                reason,
            },
            None,
        )
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
