//! `IdeFileEditor` trait — adds syntax highlighting and breakpoint support
//! to a [`FileEditor`].

use turbo_vision::views::editor_traits::FileEditor;
use turbo_vision::views::syntax::SyntaxHighlighter;

pub trait IdeFileEditor: FileEditor {
    fn set_highlighter(&mut self, highlighter: Box<dyn SyntaxHighlighter>);

    fn toggle_breakpoint(&mut self, line: usize);
    fn clear_breakpoints(&mut self);
    fn breakpoint_lines(&self) -> Vec<usize>;
    fn snap_breakpoints(&mut self, valid_lines: &[usize], total_lines: usize);

    fn current_exec_line(&self) -> Option<usize>;
    fn set_current_exec_line(&mut self, line: Option<usize>);

    /// Last build error attached to this buffer: `(line, message)`. The
    /// line drives the red row overlay and the message is what the IDE
    /// displays in the status bar when the caret sits on that line.
    fn build_error(&self) -> Option<(usize, String)>;
    /// Replace (or clear with `None`) the build error. The IDE clears
    /// this at the start of every build attempt and sets it again on
    /// failure.
    fn set_build_error(&mut self, err: Option<(usize, String)>);
}
