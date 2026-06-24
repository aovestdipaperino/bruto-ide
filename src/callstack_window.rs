/// Call-stack window — displays `bt` frames during debugging.
/// Mirrors the watch panel's layout (gray dialog palette).
use turbo_vision::core::draw::DrawBuffer;
use turbo_vision::core::event::{Event, EventType, MB_LEFT_BUTTON};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::StateFlags;
use turbo_vision::terminal::Terminal;
use turbo_vision::views::view::{View, write_line_to_terminal};

const TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);
const IDX_ATTR: Attr = Attr::new(TvColor::Blue, TvColor::LightGray);
const BG_ATTR: Attr = Attr::new(TvColor::Black, TvColor::LightGray);
// Mirrors the editor's "current statement" overlay: green background so the
// active frame is obvious at a glance.
const HL_TEXT_ATTR: Attr = Attr::new(TvColor::Black, TvColor::Green);
const HL_IDX_ATTR: Attr = Attr::new(TvColor::Blue, TvColor::Green);
const HL_BG_ATTR: Attr = Attr::new(TvColor::Black, TvColor::Green);

pub struct CallStackPanel {
    bounds: Rect,
    state: StateFlags,
    frames: Vec<(usize, String)>,
    /// Frame index whose row should be drawn with the green highlight —
    /// either `#0` (the program counter just stopped here) or whichever
    /// frame the user last clicked. `None` when no debug session is
    /// active. Compared against `frame.index`, not row position, so it
    /// stays correct even if filtering shifts row layout.
    current_idx: Option<usize>,
    /// Set when a frame row is single-clicked. The IDE event loop drains
    /// this each tick (via [`take_pending_jump`]) and navigates the
    /// corresponding editor to the frame's source location.
    pending_jump: Option<usize>,
}

impl CallStackPanel {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            state: 0,
            frames: Vec::new(),
            current_idx: None,
            pending_jump: None,
        }
    }

    pub fn set_frames(&mut self, frames: Vec<(usize, String)>) {
        self.frames = frames;
    }

    pub fn set_current_idx(&mut self, idx: Option<usize>) {
        self.current_idx = idx;
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.current_idx = None;
    }

    /// Look up the frame shown on `row`, returning `(index, display)` if
    /// the row currently holds one.
    pub fn frame_at(&self, row: usize) -> Option<&(usize, String)> {
        self.frames.get(row)
    }

    /// Drain the latest click target. Called by the IDE event loop.
    pub fn take_pending_jump(&mut self) -> Option<usize> {
        self.pending_jump.take()
    }
}

impl View for CallStackPanel {
    fn bounds(&self) -> Rect {
        self.bounds
    }
    fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
    }

    fn draw(&mut self, terminal: &mut Terminal) {
        let width = self.bounds.width_clamped() as usize;
        let height = self.bounds.height_clamped() as usize;

        for row in 0..height {
            let mut buf = DrawBuffer::new(width);

            if row < self.frames.len() {
                let (idx, display) = &self.frames[row];
                let highlighted = self.current_idx == Some(*idx);
                let (bg_attr, idx_attr, text_attr) = if highlighted {
                    (HL_BG_ATTR, HL_IDX_ATTR, HL_TEXT_ATTR)
                } else {
                    (BG_ATTR, IDX_ATTR, TEXT_ATTR)
                };
                buf.move_char(0, ' ', bg_attr, width);
                let prefix = format!("#{idx} ");
                buf.move_str(0, &prefix, idx_attr);
                buf.move_str(prefix.len(), display, text_attr);
            } else {
                buf.move_char(0, ' ', BG_ATTR, width);
            }

            write_line_to_terminal(
                terminal,
                self.bounds.a.x,
                self.bounds.a.y + row as i16,
                &buf,
            );
        }
    }

    fn handle_event(&mut self, event: &mut Event) {
        if event.what == EventType::MouseDown && (event.mouse.buttons & MB_LEFT_BUTTON != 0) {
            let mx = event.mouse.pos.x;
            let my = event.mouse.pos.y;
            if mx >= self.bounds.a.x
                && mx < self.bounds.b.x
                && my >= self.bounds.a.y
                && my < self.bounds.b.y
            {
                let row = (my - self.bounds.a.y) as usize;
                if row < self.frames.len() {
                    self.pending_jump = Some(row);
                    event.clear();
                }
            }
        }
    }

    fn state(&self) -> StateFlags {
        self.state
    }
    fn set_state(&mut self, state: StateFlags) {
        self.state = state;
    }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> {
        None
    }
}
