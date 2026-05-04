/// IDE runner — takes a Language implementation and runs the TUI.
///
/// The desktop hosts at most one persistent watch panel + one output panel,
/// plus zero or more editor windows. Editor windows are created on
/// File→New / File→Open and removed when the user clicks the close button.
/// All file operations route through the [`FileEditor`] trait on the
/// focused editor, looked up dynamically via [`focused_editor`].
use crate::callstack_window::CallStackPanel;
use crate::commands::*;
use crate::debugger::{DebugEvent, Debugger, VarType};
use crate::ide_editor::{IdeEditorWindow, SharedIdeEditorWindow};
use crate::ide_file_editor::IdeFileEditor;
use crate::output_panel::OutputPanel;
use crate::watch_window::WatchPanel;
use bruto_lang::language::{BuildPhase, BuildResult, Language};

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use turbo_vision::app::Application;
use turbo_vision::core::command::{
    CM_CLOSE, CM_COPY, CM_CUT, CM_NEW, CM_NO, CM_OPEN, CM_PASTE, CM_QUIT, CM_REDO, CM_SAVE,
    CM_SAVE_AS, CM_SELECT_ALL, CM_UNDO, CM_YES,
};
use turbo_vision::core::event::{
    Event, EventType, KB_ALT_X, KB_F2, KB_F3, KB_F5, KB_F7, KB_F8, KB_F9,
};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::menu_data::{Menu, MenuItem};
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::SF_CLOSED;
use turbo_vision::views::View;
use turbo_vision::views::editor_traits::{Editor as _, ExternalState, FileEditor};
use turbo_vision::views::file_dialog::FileDialogBuilder;
use turbo_vision::views::menu_bar::{MenuBar, SubMenu};
use turbo_vision::views::msgbox::{MF_CANCEL_BUTTON, MF_NO_BUTTON, MF_YES_BUTTON, message_box};
use turbo_vision::views::status_line::{StatusItem, StatusLine};
use turbo_vision::views::terminal_widget::TerminalWidget;

/// Host-application hooks that influence first-run behaviour. The IDE itself
/// stays agnostic of any config file format — the host owns persistence and
/// passes a callback that fires once after the About dialog is shown.
pub struct IdeOptions {
    /// When true, the IDE pops the About dialog once before the user
    /// interacts with anything else.
    pub show_about_on_start: bool,
    /// Invoked exactly once, immediately after the first-run About dialog
    /// is dismissed, so the host can flip its persistent flag to false.
    pub on_about_shown: Option<Box<dyn FnMut()>>,
    /// Body shown in the About dialog. The host should bake the application
    /// name + version into this string. Falls back to a generic
    /// "Bruto IDE" blurb when None.
    pub about_text: Option<String>,
    /// Invoked exactly once after the desktop has been drawn for the
    /// first time and after the optional first-run About dialog has been
    /// dismissed. The callback owns the `Application` so it can pop
    /// modal dialogs (update prompts, license confirmations, etc.). The
    /// framework knows nothing about its content.
    pub on_desktop_ready: Option<Box<dyn FnOnce(&mut Application)>>,
}

impl Default for IdeOptions {
    fn default() -> Self {
        Self {
            show_about_on_start: false,
            on_about_shown: None,
            about_text: None,
            on_desktop_ready: None,
        }
    }
}

struct IdeState {
    debugger: Debugger,
    watch_vars: Vec<(String, String, VarType)>,
    /// Frames produced by lldb's `bt` since the last stop. Pairs of
    /// `(index, display)`; replaced by index when the same frame arrives
    /// twice (lldb prints `frame #0` automatically on stop and again
    /// inside `bt` output). Cleared on `Stopped` and `Exited`.
    callstack_frames: Vec<(usize, String)>,
    /// Frame index that should render with the green highlight in the
    /// call-stack panel. `Some(0)` after every stop; updated to the
    /// clicked frame's index when the user navigates via the panel.
    current_frame_idx: Option<usize>,
    source_path: Option<String>,
    exe_path: Option<String>,
    console_capture_path: Option<String>,
    exec_line: Option<usize>,
    /// Editor whose breakpoints are being mirrored into the live lldb session.
    /// Set when Debug→Start succeeds, cleared on Stop/Exit. Tracking the
    /// specific editor (rather than always reading from `focused_editor`)
    /// means switching focus to a sibling buffer mid-session doesn't
    /// confuse the breakpoint sync.
    debug_editor: Option<Rc<RefCell<IdeEditorWindow>>>,
    /// Editor that currently owns the green "current statement" bar.
    /// On a real stop this is `debug_editor`; clicking a call-stack frame
    /// re-points it at the editor showing that frame's source so the bar
    /// can land in a unit file when the frame isn't in the main program.
    /// Cleared whenever the debug session ends.
    exec_editor: Option<Rc<RefCell<IdeEditorWindow>>>,
    /// Cached set of breakpoint lines we last pushed into lldb. Compared
    /// each tick against `debug_editor`'s gutter so user clicks on the
    /// gutter while the process is running translate into
    /// `breakpoint set` / `breakpoint delete` immediately.
    debug_synced_bps: std::collections::HashSet<usize>,
    /// Layout for each new editor window (full editor area). All editors
    /// stack on top of each other at this rect; the user can drag/resize.
    editor_bounds: Rect,
    /// Default save-as wildcard, e.g. `"*.pas"`.
    save_wildcard: String,
    /// Default title for an Untitled buffer, e.g. `"Untitled.pas"`.
    untitled_title: String,
    /// Layout used to (re-)spawn the Watches window.
    watch_bounds: Rect,
    /// Layout used to (re-)spawn the Output panel.
    output_bounds: Rect,
    /// Layout used to (re-)spawn the Call Stack window.
    callstack_bounds: Rect,
    /// `Some(id)` while the Watches window is on the desktop. Cleared each tick
    /// when `Desktop::contains_id` reports the user closed it.
    watch_win_id: Option<turbo_vision::views::view::ViewId>,
    /// Same idea for the Output panel.
    output_win_id: Option<turbo_vision::views::view::ViewId>,
    /// Same idea for the Call Stack window.
    callstack_win_id: Option<turbo_vision::views::view::ViewId>,
    /// Shared Watches model — survives close/re-open so the variable list
    /// persists.
    watch: Rc<RefCell<WatchPanel>>,
    /// Shared Output buffer — survives close/re-open so build / run history
    /// isn't lost.
    output_term: Rc<RefCell<TerminalWidget>>,
    /// Shared Call Stack model — survives close/re-open so frames persist.
    callstack: Rc<RefCell<CallStackPanel>>,
    /// Host-supplied About dialog body, taken from IdeOptions at startup.
    /// `None` means fall back to the generic Bruto IDE blurb.
    about_text: Option<String>,
}

/// Wrap a fresh Watches `Window` around the shared [`WatchPanel`] and add it
/// to the desktop. Returns the resulting `ViewId` so the caller can poll
/// presence and re-spawn after close. Pulled out so the boot path and
/// `CM_SHOW_WATCHES` use the same code.
fn install_watch_window(
    app: &mut Application,
    watch_bounds: Rect,
    watch: &Rc<RefCell<WatchPanel>>,
) -> turbo_vision::views::view::ViewId {
    // Reset the panel's bounds back to interior-relative before re-adding.
    // After the first install, Group::add() rewrote them to absolute window
    // coordinates; without this reset, the second install would offset the
    // (already-absolute) bounds by the new window's position and the panel
    // would render off-screen.
    let interior_w = watch_bounds.width() - 2;
    let interior_h = watch_bounds.height() - 2;
    watch
        .borrow_mut()
        .set_bounds(Rect::new(0, 0, interior_w, interior_h));

    let mut watch_win = turbo_vision::views::window::Window::new_with_type(
        watch_bounds,
        "Watches",
        turbo_vision::views::window::WindowPaletteType::Gray,
    );
    watch_win.add(Box::new(WatchView(Rc::clone(watch))));
    {
        use turbo_vision::core::state::SF_SHADOW;
        let state = watch_win.state();
        watch_win.set_state(state & !SF_SHADOW);
    }
    app.desktop.add(Box::new(watch_win))
}

/// Wrap a fresh "Call Stack" `Window` around the shared [`CallStackPanel`] and
/// add it to the desktop. Mirrors [`install_watch_window`].
fn install_callstack_window(
    app: &mut Application,
    callstack_bounds: Rect,
    callstack: &Rc<RefCell<CallStackPanel>>,
) -> turbo_vision::views::view::ViewId {
    let interior_w = callstack_bounds.width() - 2;
    let interior_h = callstack_bounds.height() - 2;
    callstack
        .borrow_mut()
        .set_bounds(Rect::new(0, 0, interior_w, interior_h));

    let mut win = turbo_vision::views::window::Window::new_with_type(
        callstack_bounds,
        "Call Stack",
        turbo_vision::views::window::WindowPaletteType::Gray,
    );
    win.add(Box::new(CallStackView(Rc::clone(callstack))));
    {
        use turbo_vision::core::state::SF_SHADOW;
        let state = win.state();
        win.set_state(state & !SF_SHADOW);
    }
    app.desktop.add(Box::new(win))
}

const OUTPUT_TEXT: Attr = Attr::new(TvColor::LightGray, TvColor::Black);
const CONSOLE_INFO: Attr = Attr::new(TvColor::Yellow, TvColor::Black);
const CONSOLE_ERR: Attr = Attr::new(TvColor::LightRed, TvColor::Black);
const SUCCESS: Attr = Attr::new(TvColor::LightGreen, TvColor::Black);
const ERROR: Attr = Attr::new(TvColor::LightRed, TvColor::Black);

fn append_output_line(panel: &mut TerminalWidget, text: &str, attr: Option<Attr>) {
    panel.append_line_colored(text.to_string(), attr.unwrap_or(OUTPUT_TEXT));
}

/// Run the IDE with the given language implementation.
pub fn run(language: Box<dyn Language>) -> turbo_vision::core::error::Result<()> {
    run_with_options(language, IdeOptions::default())
}

/// Run the IDE with optional host-supplied first-run behaviour.
pub fn run_with_options(
    language: Box<dyn Language>,
    mut options: IdeOptions,
) -> turbo_vision::core::error::Result<()> {
    install_panic_log_hook();
    crate::trace_log::init_for_session();
    crate::trace_log!("ide startup; lang={}", language.name());
    let mut app = Application::new()?;
    let (width, height) = app.terminal.size();
    let w = width as i16;
    let h = height as i16;

    let menu_bar = build_menu_bar(w);
    app.set_menu_bar(menu_bar);

    let status_line = build_status_line(w, h);
    app.set_status_line(status_line);

    let desktop_top = 0;
    let desktop_bottom = h - 1;
    let desktop_h = desktop_bottom - desktop_top;

    let watch_width: i16 = 26;
    let output_height: i16 = (desktop_h / 4).max(5);
    let editor_right = w - watch_width;
    let editor_bottom = desktop_bottom - output_height;

    let editor_bounds = Rect::new(0, desktop_top, editor_right, editor_bottom);
    let save_wildcard = format!("*.{}", language.file_extension());
    let untitled_title = format!("Untitled.{}", language.file_extension());

    // ── Watch window (hidden at start; host can re-open via Window menu) ─
    let watch_bounds = Rect::new(editor_right, desktop_top, w, editor_bottom);
    let watch_interior_w = watch_bounds.width() - 2;
    let watch_interior_h = watch_bounds.height() - 2;
    let watch = Rc::new(RefCell::new(WatchPanel::new(Rect::new(
        0,
        0,
        watch_interior_w,
        watch_interior_h,
    ))));

    // ── Output buffer (hidden at start; survives close/re-open) ─────────
    let output_bounds = Rect::new(0, editor_bottom, w, desktop_bottom);
    let output_interior_w = output_bounds.width() - 2;
    let output_interior_h = output_bounds.height() - 2;
    let output_term = Rc::new(RefCell::new(TerminalWidget::new(Rect::new(
        0,
        0,
        output_interior_w,
        output_interior_h,
    ))));

    // ── Call Stack (hidden at start). Default bounds occupy the lower
    // half of the watch column; user can drag/resize once shown.
    let callstack_top = desktop_top + (editor_bottom - desktop_top) / 2;
    let callstack_bounds = Rect::new(editor_right, callstack_top, w, editor_bottom);
    let callstack_interior_w = callstack_bounds.width() - 2;
    let callstack_interior_h = callstack_bounds.height() - 2;
    let callstack = Rc::new(RefCell::new(CallStackPanel::new(Rect::new(
        0,
        0,
        callstack_interior_w,
        callstack_interior_h,
    ))));

    let mut ide = IdeState {
        debugger: Debugger::new(),
        watch_vars: Vec::new(),
        callstack_frames: Vec::new(),
        current_frame_idx: None,
        source_path: None,
        exe_path: None,
        console_capture_path: None,
        exec_line: None,
        debug_editor: None,
        exec_editor: None,
        debug_synced_bps: std::collections::HashSet::new(),
        editor_bounds,
        save_wildcard,
        untitled_title,
        watch_bounds,
        output_bounds,
        callstack_bounds,
        watch_win_id: None,
        output_win_id: None,
        callstack_win_id: None,
        watch: Rc::clone(&watch),
        output_term: Rc::clone(&output_term),
        callstack: Rc::clone(&callstack),
        about_text: options.about_text.take(),
    };

    // ── Event loop ───────────────────────────────────────
    app.running = true;
    let mut pending_about = options.show_about_on_start;
    while app.running {
        update_command_states(&mut app, &ide);
        update_status_hint(&mut app);
        app.terminal.force_full_redraw();
        app.desktop.draw(&mut app.terminal);
        if let Some(ref mut mb) = app.menu_bar {
            mb.draw(&mut app.terminal);
        }
        if let Some(ref mut sl) = app.status_line {
            sl.draw(&mut app.terminal);
        }
        let _ = app.terminal.flush();

        if pending_about {
            pending_about = false;
            show_about_dialog(&mut app, language.name(), ide.about_text.as_deref());
            if let Some(cb) = options.on_about_shown.as_mut() {
                cb();
            }
        }

        // Fire the desktop-ready hook on the first iteration that has a
        // drawn frame. We move the closure out so it runs at most once
        // even if the loop iterates many times.
        if let Some(cb) = options.on_desktop_ready.take() {
            cb(&mut app);
        }

        // Poll debugger
        if ide.debugger.is_running() {
            let events = ide.debugger.poll();
            for dbg_event in events {
                match dbg_event {
                    DebugEvent::Stopped { line, .. } => {
                        ide.exec_line = Some(line);
                        // After a real stop the green bar belongs on the
                        // debug target; clear any prior frame-click override.
                        ide.exec_editor = ide.debug_editor.clone();
                        // A new stop means the previous backtrace is stale;
                        // wipe it so the panel doesn't briefly show the old
                        // stack while `bt` output streams in.
                        ide.callstack_frames.clear();
                        // Frame #0 is the current PC after every stop; the
                        // panel highlight resets to it until the user clicks
                        // a different frame.
                        ide.current_frame_idx = Some(0);
                    }
                    DebugEvent::Frames(frames) => {
                        for (idx, display) in frames {
                            match ide
                                .callstack_frames
                                .iter()
                                .position(|(i, _)| *i == idx)
                            {
                                Some(pos) => ide.callstack_frames[pos] = (idx, display),
                                None => ide.callstack_frames.push((idx, display)),
                            }
                        }
                        ide.callstack_frames.sort_by_key(|(i, _)| *i);
                    }
                    DebugEvent::Variables(vars) => {
                        for (name, value, ty) in vars {
                            let mut found = false;
                            for entry in &mut ide.watch_vars {
                                if entry.0 == name {
                                    entry.1 = value.clone();
                                    entry.2 = ty;
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                ide.watch_vars.push((name, value, ty));
                            }
                        }
                    }
                    DebugEvent::ProgramOutput(line) => {
                        append_output_line(&mut output_term.borrow_mut(), &line, None);
                    }
                    DebugEvent::Exited { code } => {
                        crate::trace_log!("DebugEvent::Exited code={code}");
                        ide.exec_line = None;
                        ide.watch_vars.clear();
                        ide.callstack_frames.clear();
                        ide.current_frame_idx = None;
                        // Clear the highlight on the editor that was being
                        // debugged directly — the per-frame
                        // `set_current_exec_line` call only updates the
                        // *focused* editor, so if the user moved focus
                        // (e.g. clicked the output panel) the bar would
                        // otherwise linger after the program exits. Clear
                        // any frame-click override target too, in case the
                        // user jumped into a unit before the exit.
                        if let Some(ee) = ide.exec_editor.as_ref() {
                            ee.borrow_mut().set_current_exec_line(None);
                        }
                        if let Some(de) = ide.debug_editor.as_ref() {
                            de.borrow_mut().set_current_exec_line(None);
                        }
                        ide.exec_editor = None;
                        ide.debugger.stop();
                        ide.debug_editor = None;
                        ide.debug_synced_bps.clear();
                        crate::trace_log!(
                            "post-Exited cleanup done; debug_editor=None desktop_children={}",
                            app.desktop.child_count()
                        );
                        let color = if code == 0 { SUCCESS } else { ERROR };
                        append_output_line(
                            &mut output_term.borrow_mut(),
                            &format!("Process exited with code {}", code),
                            Some(color),
                        );
                    }
                }
            }
        }

        // Mirror gutter breakpoint toggles into the running lldb session
        // (no-op when the debugger isn't active).
        sync_debug_breakpoints(&mut ide);

        // Update watch and per-editor exec-line state.
        watch.borrow_mut().set_variables(ide.watch_vars.clone());
        {
            let mut cs = callstack.borrow_mut();
            cs.set_frames(ide.callstack_frames.clone());
            cs.set_current_idx(ide.current_frame_idx);
        }

        // Prefer the editor that's actually being debugged: the green
        // exec-line bar must keep tracking the program counter even
        // when focus has moved to the watch panel or output. Falling
        // back to the focused editor keeps a leftover bar from
        // sticking on an editor the user re-focuses outside a debug
        // session. `exec_editor` overrides the default while the user
        // is exploring a non-#0 frame — see `handle_callstack_jump`.
        let target = ide
            .exec_editor
            .as_ref()
            .or(ide.debug_editor.as_ref())
            .map(Rc::clone)
            .or_else(|| focused_editor(&mut app));
        if let Some(ed) = target {
            ed.borrow_mut().set_current_exec_line(ide.exec_line);

            if let Some(exec_line) = ide.exec_line {
                let editor_inner = ed.borrow().editor_rc();
                let editor = editor_inner.borrow();
                let delta_y = editor.get_delta().y.max(0) as usize;
                let visible_h = editor.bounds().height_clamped() as usize;
                drop(editor);
                if exec_line <= delta_y || exec_line > delta_y + visible_h {
                    editor_inner.borrow_mut().scroll_to_line(exec_line - 1);
                }
            }
        }

        // External-change polling: refresh clean buffers silently, prompt for dirty ones.
        poll_all_external_changes(&mut app);

        // Poll terminal events
        match app.terminal.poll_event(Duration::from_millis(30)) {
            Ok(Some(mut event)) => {
                if let Some(ref mut sl) = app.status_line {
                    sl.handle_event(&mut event);
                }

                if let Some(ref mut mb) = app.menu_bar {
                    mb.handle_event(&mut event);
                    if event.what == EventType::Keyboard || event.what == EventType::MouseUp {
                        if let Some(cmd) = mb.check_cascading_submenu(&mut app.terminal) {
                            if cmd != 0 {
                                event = Event::command(cmd);
                            }
                        }
                    }
                }

                if event.what == EventType::Keyboard {
                    match event.key_code {
                        // Menu items declare these as shortcuts but the menu bar only
                        // displays the labels; dispatch the commands ourselves.
                        KB_F2 => {
                            event = Event::command(CM_SAVE);
                        }
                        KB_F3 => {
                            event = Event::command(CM_OPEN);
                        }
                        KB_F9 => {
                            event = Event::command(CM_BUILD);
                        }
                        KB_F5 => {
                            event = Event::command(CM_DEBUG_START);
                        }
                        KB_F7 => {
                            if ide.debugger.is_running() {
                                let _ = ide.debugger.step_into();
                            }
                            event.clear();
                        }
                        KB_F8 => {
                            if ide.debugger.is_running() {
                                let _ = ide.debugger.step_over();
                            }
                            event.clear();
                        }
                        _ => {}
                    }
                }

                if event.what == EventType::Command {
                    let handled =
                        handle_command(event.command, &mut app, &language, &output_term, &mut ide);
                    if handled {
                        event.clear();
                    }
                }

                app.desktop.handle_event(&mut event);

                // Frame-generated commands (e.g. CM_CLOSE from a close-button click)
                // are produced during desktop dispatch, so re-run handle_command afterwards.
                if event.what == EventType::Command {
                    let handled =
                        handle_command(event.command, &mut app, &language, &output_term, &mut ide);
                    if handled {
                        event.clear();
                    }
                }

                // Sweep any windows that self-closed during dispatch (Window::auto_close).
                // Editors don't auto-close — they bubble CM_CLOSE up so confirm_close_focused_editor
                // can prompt save first; for them, close_focused_window does the SF_CLOSED + sweep.
                app.desktop.remove_closed_windows();

                // After the sweep, check whether the Watches / Output windows are
                // still on the desktop. If not (user clicked their close button),
                // forget the saved id so the Window menu re-enables their entries.
                if let Some(id) = ide.watch_win_id {
                    if !app.desktop.contains_id(id) {
                        ide.watch_win_id = None;
                    }
                }
                if let Some(id) = ide.output_win_id {
                    if !app.desktop.contains_id(id) {
                        ide.output_win_id = None;
                    }
                }
                if let Some(id) = ide.callstack_win_id {
                    if !app.desktop.contains_id(id) {
                        ide.callstack_win_id = None;
                    }
                }

                // Did the user just double-click a watch row? Open the
                // type-aware value editor and push the result into lldb.
                // Extract the row into a local *before* the call, so the
                // borrow_mut() RefMut is dropped — handle_watch_edit
                // re-borrows the same RefCell.
                let pending_watch_edit = watch.borrow_mut().take_pending_edit();
                if let Some(row) = pending_watch_edit {
                    handle_watch_edit(&mut app, &mut ide, row);
                }

                let pending_jump = callstack.borrow_mut().take_pending_jump();
                if let Some(row) = pending_jump {
                    handle_callstack_jump(&mut app, &mut ide, row);
                }
            }
            Ok(None) => {}
            Err(_) => {}
        }
    }

    Ok(())
}

fn handle_command(
    cmd: u16,
    app: &mut Application,
    language: &Box<dyn Language>,
    output_rc: &Rc<RefCell<TerminalWidget>>,
    ide: &mut IdeState,
) -> bool {
    match cmd {
        CM_QUIT => {
            // Walk all editors and prompt for unsaved changes; any cancel aborts quit.
            if !confirm_close_all_dirty_editors(app) {
                return true;
            }
            ide.debugger.stop();
            app.running = false;
            true
        }
        CM_CLOSE_EDITOR => {
            if !confirm_close_debug_editor(app, ide) {
                return true;
            }
            if !confirm_close_focused_editor(app) {
                return true;
            }
            close_focused_window(app);
            true
        }
        CM_CLOSE => {
            // Fallback for any window that opted out of auto_close and bubbles
            // CM_CLOSE up. Watch and Output use auto_close=true and never reach
            // here. Editor windows translate CM_CLOSE → CM_CLOSE_EDITOR before
            // it gets here, so this is mostly defensive.
            close_focused_window(app);
            true
        }
        CM_NEW => {
            new_editor_window(app, language, ide);
            true
        }
        CM_SHOW_WATCHES => {
            if ide.watch_win_id.is_none() {
                let id = install_watch_window(app, ide.watch_bounds, &ide.watch);
                ide.watch_win_id = Some(id);
            }
            true
        }
        CM_SHOW_OUTPUT => {
            if ide.output_win_id.is_none() {
                let panel = OutputPanel::with_terminal(
                    ide.output_bounds,
                    "Output",
                    Rc::clone(&ide.output_term),
                );
                let id = app.desktop.add(Box::new(panel));
                ide.output_win_id = Some(id);
            }
            true
        }
        CM_SHOW_CALLSTACK => {
            if ide.callstack_win_id.is_none() {
                let id = install_callstack_window(app, ide.callstack_bounds, &ide.callstack);
                ide.callstack_win_id = Some(id);
            }
            true
        }
        CM_UNDO => {
            if let Some(ed) = focused_editor(app) {
                ed.borrow_mut().undo();
            }
            true
        }
        CM_REDO => {
            if let Some(ed) = focused_editor(app) {
                ed.borrow_mut().redo();
            }
            true
        }
        CM_CUT => {
            if let Some(ed) = focused_editor(app) {
                let _ = ed.borrow_mut().cut();
            }
            true
        }
        CM_COPY => {
            if let Some(ed) = focused_editor(app) {
                let _ = ed.borrow_mut().copy();
            }
            true
        }
        CM_PASTE => {
            if let Some(ed) = focused_editor(app) {
                let _ = ed.borrow_mut().paste();
            }
            true
        }
        CM_SELECT_ALL => {
            if let Some(ed) = focused_editor(app) {
                ed.borrow_mut().select_all();
            }
            true
        }
        CM_OPEN => {
            handle_open(app, language, ide);
            true
        }
        CM_SAVE => {
            handle_save(app, ide);
            true
        }
        CM_SAVE_AS => {
            handle_save_as(app, ide);
            true
        }
        CM_BUILD => {
            handle_build(app, language, &mut output_rc.borrow_mut(), ide);
            true
        }
        CM_RUN => {
            handle_build(app, language, &mut output_rc.borrow_mut(), ide);
            if let Some(exe) = ide.exe_path.clone() {
                handle_run(&exe, &ide.console_capture_path, &mut output_rc.borrow_mut());
            }
            true
        }
        CM_DEBUG_START | CM_DEBUG_CONTINUE => {
            handle_debug_start_continue(app, language, &mut output_rc.borrow_mut(), ide);
            true
        }
        CM_DEBUG_STOP => {
            crate::trace_log!("CM_DEBUG_STOP requested");
            ide.debugger.stop();
            ide.exec_line = None;
            ide.exec_editor = None;
            ide.watch_vars.clear();
            ide.callstack_frames.clear();
            ide.current_frame_idx = None;
            ide.debug_editor = None;
            ide.debug_synced_bps.clear();
            append_output_line(
                &mut output_rc.borrow_mut(),
                "Debugger stopped.",
                Some(CONSOLE_INFO),
            );
            true
        }
        CM_DEBUG_STEP_OVER => {
            if ide.debugger.is_running() {
                let _ = ide.debugger.step_over();
            }
            true
        }
        CM_DEBUG_STEP_INTO => {
            if ide.debugger.is_running() {
                let _ = ide.debugger.step_into();
            }
            true
        }
        CM_ABOUT => {
            show_about_dialog(app, language.name(), ide.about_text.as_deref());
            true
        }
        _ => false,
    }
}

// ── Editor lifecycle ─────────────────────────────────────

/// Build a fresh `IdeEditorWindow` wired up with the language's highlighter.
fn make_editor(language: &Box<dyn Language>, ide: &IdeState) -> Rc<RefCell<IdeEditorWindow>> {
    let mut ide_win =
        IdeEditorWindow::new(ide.editor_bounds, &ide.untitled_title, &ide.save_wildcard);
    ide_win.set_highlighter(language.create_highlighter());
    Rc::new(RefCell::new(ide_win))
}

/// Add an editor wrapper to the desktop and give it focus.
fn install_editor(app: &mut Application, editor: Rc<RefCell<IdeEditorWindow>>) {
    crate::trace_log!(
        "install_editor: pre-add desktop_children={}",
        app.desktop.child_count()
    );
    let wrapper = SharedIdeEditorWindow(editor);
    app.desktop.add(Box::new(wrapper));
    // The newly added child is at the end; focus it via desktop's last index.
    let last = app.desktop.child_count().saturating_sub(1);
    if last < app.desktop.child_count() {
        // Clear focus on others, then mark new one as focused so handle_event routes there.
        for i in 0..app.desktop.child_count() {
            app.desktop.child_at_mut(i).set_focus(i == last);
        }
    }
    crate::trace_log!(
        "install_editor: post-add desktop_children={} focused_idx={last}",
        app.desktop.child_count()
    );
}

fn new_editor_window(app: &mut Application, language: &Box<dyn Language>, ide: &IdeState) {
    let editor = make_editor(language, ide);
    install_editor(app, editor);
}

fn handle_open(app: &mut Application, language: &Box<dyn Language>, ide: &IdeState) {
    crate::trace_log!(
        "handle_open: enter; desktop_children={} debug_editor={} debugger_running={}",
        app.desktop.child_count(),
        ide.debug_editor.is_some(),
        ide.debugger.is_running()
    );
    let bounds = centered_dialog_bounds(app);
    let mut dialog = FileDialogBuilder::new()
        .bounds(bounds)
        .title("Open File")
        .wildcard(ide.save_wildcard.clone())
        .button_label("~O~pen")
        .build();
    let Some(path) = dialog.execute(app) else {
        crate::trace_log!("handle_open: cancelled");
        return;
    };
    crate::trace_log!("handle_open: selected {}", path.display());

    // Already open? Focus that window instead of creating a duplicate.
    if let Some(idx) = find_editor_with_path(app, &path) {
        crate::trace_log!("handle_open: already open at idx {idx}; refocusing");
        for i in 0..app.desktop.child_count() {
            app.desktop.child_at_mut(i).set_focus(i == idx);
        }
        return;
    }

    let editor = make_editor(language, ide);
    if let Err(e) = editor.borrow_mut().load(path.clone()) {
        crate::trace_log!("handle_open: load failed: {e}");
        use turbo_vision::views::msgbox::message_box_error;
        message_box_error(app, &format!("Cannot open file:\n{e}"));
        return;
    }
    install_editor(app, editor);
    crate::trace_log!(
        "handle_open: installed; desktop_children={}",
        app.desktop.child_count()
    );
}

fn handle_save(app: &mut Application, ide: &IdeState) {
    let Some(editor) = focused_editor(app) else {
        return;
    };
    let has_path = editor.borrow().file_path().is_some();
    if has_path {
        if let Err(e) = editor.borrow_mut().save() {
            use turbo_vision::views::msgbox::message_box_error;
            message_box_error(app, &format!("Save failed:\n{e}"));
        }
    } else {
        save_focused_as(app, &editor, ide);
    }
}

fn handle_save_as(app: &mut Application, ide: &IdeState) {
    let Some(editor) = focused_editor(app) else {
        return;
    };
    save_focused_as(app, &editor, ide);
}

fn save_focused_as(app: &mut Application, editor: &Rc<RefCell<IdeEditorWindow>>, ide: &IdeState) {
    let bounds = centered_dialog_bounds(app);
    let mut dialog = FileDialogBuilder::new()
        .bounds(bounds)
        .title("Save As")
        .wildcard(ide.save_wildcard.clone())
        .button_label("~S~ave")
        .build();
    let Some(path) = dialog.execute(app) else {
        return;
    };

    if path.exists() {
        use turbo_vision::views::msgbox::confirmation_box_yes_no;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let answer = confirmation_box_yes_no(app, &format!("{name} already exists.\n\nOverwrite?"));
        if answer != CM_YES {
            return;
        }
    }

    if let Err(e) = editor.borrow_mut().save_as(path) {
        use turbo_vision::views::msgbox::message_box_error;
        message_box_error(app, &format!("Save failed:\n{e}"));
    }
}

// ── Build / Run / Debug ──────────────────────────────────

fn handle_build(
    app: &mut Application,
    language: &Box<dyn Language>,
    output: &mut TerminalWidget,
    ide: &mut IdeState,
) {
    let Some(editor) = focused_editor(app) else {
        append_output_line(
            output,
            "No active editor — open or create a file first.",
            Some(CONSOLE_INFO),
        );
        return;
    };
    let source = editor.borrow().editor_rc().borrow().get_text();
    let file_path = editor.borrow().file_path().map(std::path::PathBuf::from);
    crate::trace_log!(
        "handle_build: src_len={} file_path={:?} debugger_running={} debug_editor_present={}",
        source.len(),
        file_path,
        ide.debugger.is_running(),
        ide.debug_editor.is_some()
    );
    output.clear();
    append_output_line(output, "Building...", Some(CONSOLE_INFO));

    // Drop any prior error highlight before we start; we'll re-set it
    // below if this build fails too. Each build is the authoritative
    // signal — leaving a stale red bar around after a successful build
    // would mislead the user.
    editor.borrow_mut().set_build_error(None);

    let job = language.build_job_at(&source, file_path.as_deref());
    match run_build_with_progress(app, job) {
        Some(Ok(result)) => {
            crate::trace_log!("handle_build: ok exe={}", result.exe_path);
            ide.exe_path = Some(result.exe_path.clone());
            ide.source_path = Some(result.source_path);
            ide.console_capture_path = Some(result.console_capture_path);
            append_output_line(
                output,
                &format!("Build successful: {}", result.exe_path),
                Some(SUCCESS),
            );
        }
        Some(Err(e)) => {
            crate::trace_log!("handle_build: err {e}");
            if let Some(line) = extract_error_line(&e) {
                editor
                    .borrow_mut()
                    .set_build_error(Some((line, e.clone())));
            }
            append_output_line(output, &format!("Build error: {}", e), Some(ERROR));
            use turbo_vision::views::msgbox::message_box_error;
            message_box_error(app, &e);
        }
        None => {
            // User cancelled. Dropping the job already killed the linker
            // child via PascalBuildJob::drop.
            crate::trace_log!("handle_build: cancelled");
            append_output_line(output, "Build cancelled.", Some(CONSOLE_INFO));
        }
    }
}

/// Pull a 1-based line number out of a build-error string. Pascal's
/// parser emits `line N:col: message`; the linker / dsymutil errors
/// don't have a line at all, in which case this returns `None` and the
/// IDE just leaves the gutter clear.
fn extract_error_line(error: &str) -> Option<usize> {
    let pos = error.find("line ")?;
    let rest = &error[pos + "line ".len()..];
    let end = rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// Run a [`BuildJob`] inside a centered modal dialog. Each tick draws
/// the desktop / dialog, polls the job, and updates the visible phase
/// label. Returns `Some(Ok|Err)` on completion or `None` when the user
/// clicks Cancel — dropping the job kills any live child process.
fn run_build_with_progress(
    app: &mut Application,
    mut job: Box<dyn bruto_lang::language::BuildJob>,
) -> Option<Result<BuildResult, String>> {
    use turbo_vision::core::command::CM_CANCEL;
    use turbo_vision::core::state::SF_MODAL;
    use turbo_vision::views::button::Button;
    use turbo_vision::views::dialog::Dialog;

    let (tw, th) = app.terminal.size();
    let dw = 50i16.min(tw as i16 - 4);
    let dh = 7i16;
    let x = ((tw as i16) - dw) / 2;
    let y = ((th as i16) - dh) / 2;
    let bounds = Rect::new(x, y, x + dw, y + dh);

    let mut dialog = Dialog::new(bounds, "Build");

    let progress_text = Rc::new(RefCell::new("Compiling…".to_string()));
    dialog.add(Box::new(ProgressView::new(
        Rect::new(2, 2, dw - 2, 3),
        Rc::clone(&progress_text),
    )));

    let cancel_w = 12i16;
    let cancel_x = (dw - cancel_w) / 2;
    dialog.add(Box::new(Button::new(
        Rect::new(cancel_x, dh - 4, cancel_x + cancel_w, dh - 2),
        "~C~ancel",
        CM_CANCEL,
        true,
    )));

    let old_state = dialog.state();
    dialog.set_state(old_state | SF_MODAL);
    dialog.set_initial_focus();

    // Hide the terminal cursor for the duration so it doesn't blink in
    // the editor underneath — the dialog has no focusable text input
    // anyway, just the Cancel button.
    let _ = app.terminal.hide_cursor();

    let result = run_progress_loop(app, &mut dialog, &mut job, &progress_text);

    // Show cursor again before we hand back to the main event loop;
    // its update_cursor will reposition it to whatever's focused now.
    let _ = app.terminal.show_cursor(0, 0);
    result
}

fn run_progress_loop(
    app: &mut Application,
    dialog: &mut turbo_vision::views::dialog::Dialog,
    job: &mut Box<dyn bruto_lang::language::BuildJob>,
    progress_text: &Rc<RefCell<String>>,
) -> Option<Result<BuildResult, String>> {
    use turbo_vision::core::command::CM_CANCEL;
    let _ = CM_CANCEL; // silence unused

    loop {
        // Force-clear the back buffer so the dialog overlay doesn't
        // accidentally show a stale frame from before it opened.
        app.terminal.force_full_redraw();

        // Draw — desktop first (so the dialog overlays), then chrome,
        // then the dialog itself.
        app.desktop.draw(&mut app.terminal);
        if let Some(ref mut mb) = app.menu_bar {
            mb.draw(&mut app.terminal);
        }
        if let Some(ref mut sl) = app.status_line {
            sl.draw(&mut app.terminal);
        }
        dialog.draw(&mut app.terminal);
        let _ = app.terminal.flush();

        // Advance the build by one step.
        match job.poll() {
            BuildPhase::Pending(label) => {
                *progress_text.borrow_mut() = label;
            }
            BuildPhase::Done(r) => return Some(Ok(r)),
            BuildPhase::Failed(e) => return Some(Err(e)),
        }

        // Poll the terminal briefly so Cancel feels responsive without
        // starving the build (which we re-poll on the next iteration).
        // Events route ONLY to the dialog — the desktop is purely
        // visual while the modal is up.
        if let Ok(Some(mut event)) = app.terminal.poll_event(Duration::from_millis(30)) {
            dialog.handle_event(&mut event);
            if event.what == EventType::Command {
                dialog.handle_event(&mut event);
            }
            if dialog.get_end_state() != 0 {
                // Clicked Cancel — let `job` drop and kill the linker.
                return None;
            }
        }
    }
}

/// One-line view used by the build progress dialog. Reads its text
/// from a `Rc<RefCell<String>>` so the polling loop can update what's
/// shown without rebuilding the dialog.
struct ProgressView {
    bounds: Rect,
    state: turbo_vision::core::state::StateFlags,
    text: Rc<RefCell<String>>,
}

impl ProgressView {
    fn new(bounds: Rect, text: Rc<RefCell<String>>) -> Self {
        Self {
            bounds,
            state: 0,
            text,
        }
    }
}

impl turbo_vision::views::View for ProgressView {
    fn bounds(&self) -> Rect {
        self.bounds
    }
    fn set_bounds(&mut self, b: Rect) {
        self.bounds = b;
    }
    fn draw(&mut self, terminal: &mut turbo_vision::terminal::Terminal) {
        use turbo_vision::core::draw::DrawBuffer;
        use turbo_vision::views::view::write_line_to_terminal;

        let width = self.bounds.width_clamped() as usize;
        let attr = self.map_color(1);
        let mut buf = DrawBuffer::new(width);
        buf.move_char(0, ' ', attr, width);
        let txt = self.text.borrow();
        buf.move_str(0, &txt, attr);
        write_line_to_terminal(terminal, self.bounds.a.x, self.bounds.a.y, &buf);
    }
    fn handle_event(&mut self, _event: &mut Event) {}
    fn state(&self) -> turbo_vision::core::state::StateFlags {
        self.state
    }
    fn set_state(&mut self, s: turbo_vision::core::state::StateFlags) {
        self.state = s;
    }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> {
        None
    }
}

fn handle_run(exe_path: &str, console_capture_path: &Option<String>, output: &mut TerminalWidget) {
    output.clear();
    output.append_line_colored(format!("Running {}...", exe_path), CONSOLE_INFO);

    if let Some(capture_path) = console_capture_path {
        let _ = std::fs::write(capture_path, "");
    }

    let status = std::process::Command::new(exe_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if let Some(capture_path) = console_capture_path {
        if let Ok(contents) = std::fs::read_to_string(capture_path) {
            for line in contents.lines() {
                output.append_line_colored(line.to_string(), OUTPUT_TEXT);
            }
        }
    }

    match status {
        Ok(s) => {
            let code = s.code().unwrap_or(-1);
            let color = if code == 0 { SUCCESS } else { ERROR };
            output.append_line_colored(format!("Exit code: {}", code), color);
        }
        Err(e) => {
            output.append_line_colored(format!("Failed to run: {}", e), CONSOLE_ERR);
        }
    }
}

fn handle_debug_start_continue(
    app: &mut Application,
    language: &Box<dyn Language>,
    output: &mut TerminalWidget,
    ide: &mut IdeState,
) {
    if ide.debugger.is_running() {
        let _ = ide.debugger.continue_exec();
        ide.exec_line = None;
        return;
    }

    handle_build(app, language, output, ide);
    let Some(exe_path) = ide.exe_path.clone() else {
        append_output_line(output, "No executable to debug.", Some(ERROR));
        return;
    };

    let Some(editor) = focused_editor(app) else {
        append_output_line(output, "No active editor.", Some(ERROR));
        return;
    };

    // Snap breakpoints to valid executable lines
    let source = editor.borrow().editor_rc().borrow().get_text();
    let valid: Vec<usize> = language
        .valid_breakpoint_lines(&source)
        .into_iter()
        .collect();
    let line_count = source.lines().count();
    editor.borrow_mut().snap_breakpoints(&valid, line_count);

    let source_file = ide.source_path.clone().unwrap_or_default();
    let bp_lines = editor.borrow().breakpoint_lines();

    append_output_line(
        output,
        &format!("Starting debugger with {} breakpoint(s)...", bp_lines.len()),
        Some(CONSOLE_INFO),
    );
    output.clear();

    crate::trace_log!(
        "debugger.start: exe={exe_path} source={source_file} bps={}",
        bp_lines.len()
    );
    match ide.debugger.start(&exe_path, &source_file, &bp_lines) {
        Ok(()) => {
            // Remember which editor's breakpoints to mirror into lldb each
            // tick — see `sync_debug_breakpoints`.
            ide.debug_editor = Some(Rc::clone(&editor));
            ide.debug_synced_bps = bp_lines.iter().copied().collect();
            append_output_line(output, "Debugger started.", Some(SUCCESS));
            crate::trace_log!("debugger.start: ok");
        }
        Err(e) => {
            crate::trace_log!("debugger.start: err {e}");
            append_output_line(output, &format!("Debugger error: {}", e), Some(ERROR));
        }
    }
}

/// While the debugger is running, mirror the debugged editor's gutter
/// breakpoints into lldb so toggles via the gutter take effect without a
/// restart. Diffs against the last pushed set and issues only the
/// add/remove deltas. No-op when the debugger isn't running or no editor
/// is attached.
fn sync_debug_breakpoints(ide: &mut IdeState) {
    if !ide.debugger.is_running() {
        return;
    }
    let Some(ref editor) = ide.debug_editor else {
        return;
    };

    let desired: std::collections::HashSet<usize> =
        editor.borrow().breakpoint_lines().into_iter().collect();
    if desired == ide.debug_synced_bps {
        return;
    }

    let to_remove: Vec<usize> = ide.debug_synced_bps.difference(&desired).copied().collect();
    let to_add: Vec<usize> = desired.difference(&ide.debug_synced_bps).copied().collect();

    for line in &to_remove {
        let _ = ide.debugger.remove_breakpoint(*line);
    }
    for line in &to_add {
        let _ = ide.debugger.add_breakpoint(*line);
    }

    ide.debug_synced_bps = desired;
}

// ── Window/desktop navigation ────────────────────────────

/// Toggle command-set entries (the global enable/disable bitset that
/// `MenuBar` consults when drawing menu items) based on the current IDE
/// state. Called every tick; greys out commands that don't make sense
/// right now so the user gets immediate visual feedback.
///
/// Rules:
/// - Save / Save As / Build / Run / Close-Editor / Debug Start: an editor
///   window must be focused.
/// - Step Over / Step Into / Stop / Continue: the debugger must be running.
/// - Show Watches: the Watches window must be currently closed.
/// - Show Output:  the Output window must be currently closed.
fn update_command_states(app: &mut Application, ide: &IdeState) {
    use turbo_vision::core::command_set::{disable_command, enable_command};

    let focused = focused_editor(app);
    let editor_focused = focused.is_some();
    let (has_selection, can_undo, can_redo) = match focused.as_ref() {
        Some(e) => {
            let b = e.borrow();
            (b.has_selection(), b.can_undo(), b.can_redo())
        }
        None => (false, false, false),
    };
    let dbg_running = ide.debugger.is_running();
    let watch_open = ide.watch_win_id.is_some();
    let output_open = ide.output_win_id.is_some();
    let callstack_open = ide.callstack_win_id.is_some();

    let toggle = |cmd: u16, enabled: bool| {
        if enabled {
            enable_command(cmd);
        } else {
            disable_command(cmd);
        }
    };

    // Editor-bound commands
    toggle(CM_SAVE, editor_focused);
    toggle(CM_SAVE_AS, editor_focused);
    toggle(CM_BUILD, editor_focused);
    toggle(CM_RUN, editor_focused);
    toggle(CM_CLOSE_EDITOR, editor_focused);

    // Edit menu. Undo / Redo follow the focused editor's stack state so
    // they grey out at the ends of the history (and on a clean buffer).
    // Cut / Copy require a non-empty selection. Paste / Select All only
    // need an editor; we don't peek at the OS clipboard each tick to
    // avoid the per-frame arboard call.
    toggle(CM_UNDO, can_undo);
    toggle(CM_REDO, can_redo);
    toggle(CM_PASTE, editor_focused);
    toggle(CM_SELECT_ALL, editor_focused);
    toggle(CM_CUT, editor_focused && has_selection);
    toggle(CM_COPY, editor_focused && has_selection);

    // Debugger-bound commands.
    // CM_DEBUG_START is the same menu entry as continue (~S~tart / Continue);
    // the handler dispatches based on dbg_running, so we keep it enabled in
    // both states as long as there's an editor to operate on.
    toggle(CM_DEBUG_START, editor_focused);
    toggle(CM_DEBUG_CONTINUE, dbg_running);
    toggle(CM_DEBUG_STEP_OVER, dbg_running);
    toggle(CM_DEBUG_STEP_INTO, dbg_running);
    toggle(CM_DEBUG_STOP, dbg_running);

    // Window menu — only offer to re-open closed panels
    toggle(CM_SHOW_WATCHES, !watch_open);
    toggle(CM_SHOW_OUTPUT, !output_open);
    toggle(CM_SHOW_CALLSTACK, !callstack_open);
}

/// Show the latest build error in the status line whenever the caret
/// sits on the offending line of the focused editor; clear the hint
/// otherwise. Whitespace is flattened to fit on a single status row,
/// and the noisy `Parse error: line ` prefix from the build job is
/// stripped — the user already knows it's an error and what line
/// they're on, so a compact `<col>: <message>` is more useful.
fn update_status_hint(app: &mut Application) {
    let hint = focused_editor(app).and_then(|ed| {
        let edw = ed.borrow();
        let (err_line, message) = edw.build_error()?;
        let cursor_line = edw.editor_rc().borrow().cursor().y as usize + 1;
        if cursor_line != err_line {
            return None;
        }
        let flat = message.split_whitespace().collect::<Vec<_>>().join(" ");
        Some(format_status_error(&flat))
    });
    if let Some(ref mut sl) = app.status_line {
        sl.set_hint(hint);
    }
}

/// Strip the `Parse error: line ` (or just `line `) prefix from a build
/// error so the status hint shows the bare `<line>:<col>: <message>`.
/// Anything that doesn't carry the `line ` marker is returned as-is.
fn format_status_error(message: &str) -> String {
    match message.find("line ") {
        Some(i) => message[i + "line ".len()..].to_string(),
        None => message.to_string(),
    }
}

/// Find the first child of the desktop that is both a `SharedIdeEditorWindow`
/// wrapper and currently focused, and return its underlying `Rc`.
fn focused_editor(app: &mut Application) -> Option<Rc<RefCell<IdeEditorWindow>>> {
    for i in 0..app.desktop.child_count() {
        let child = app.desktop.child_at(i);
        if !child.is_focused() {
            continue;
        }
        if let Some(shared) = child.as_any().downcast_ref::<SharedIdeEditorWindow>() {
            return Some(Rc::clone(&shared.0));
        }
    }
    None
}

/// Return the desktop child index of an editor showing `path`, comparing
/// canonicalized paths so symlinks and relative segments don't cause misses.
fn find_editor_with_path(app: &mut Application, path: &Path) -> Option<usize> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for i in 0..app.desktop.child_count() {
        let Some(shared) = app
            .desktop
            .child_at(i)
            .as_any()
            .downcast_ref::<SharedIdeEditorWindow>()
        else {
            continue;
        };
        let Some(existing) = shared.0.borrow().file_path() else {
            continue;
        };
        let canonical = std::fs::canonicalize(&existing).unwrap_or(existing);
        if canonical == target {
            return Some(i);
        }
    }
    None
}

/// Run [`FileEditor::poll_external_changes`] on every editor on the desktop.
/// - `Modified` + clean buffer: silent reload.
/// - `Modified` + dirty buffer: prompt user to discard local changes.
/// - `Deleted`: clear file_path and mtime so subsequent saves go through Save As.
fn poll_all_external_changes(app: &mut Application) {
    // Collect Rcs first so we don't mutate `app` while iterating it.
    let mut editors: Vec<Rc<RefCell<IdeEditorWindow>>> = Vec::new();
    for i in 0..app.desktop.child_count() {
        if let Some(shared) = app
            .desktop
            .child_at(i)
            .as_any()
            .downcast_ref::<SharedIdeEditorWindow>()
        {
            editors.push(Rc::clone(&shared.0));
        }
    }

    for editor in editors {
        let state = editor.borrow().poll_external_changes();
        match state {
            ExternalState::Unchanged | ExternalState::NoFile => {}
            ExternalState::Modified => {
                let dirty = editor.borrow().is_dirty();
                if !dirty {
                    let _ = editor.borrow_mut().reload();
                } else {
                    use turbo_vision::views::msgbox::confirmation_box_yes_no;
                    let name = editor
                        .borrow()
                        .file_path()
                        .as_deref()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "file".to_string());
                    let answer = confirmation_box_yes_no(
                        app,
                        &format!("{name} changed on disk.\n\nReload and lose unsaved changes?"),
                    );
                    if answer == CM_YES {
                        let _ = editor.borrow_mut().reload();
                    } else {
                        // User chose to keep local edits — refresh mtime so we don't
                        // re-prompt on every tick. Best effort: a save() would do it,
                        // but here we just mark the buffer as fresh-relative-to-disk
                        // by reloading mtime via a dummy save_as round-trip would be
                        // wrong. Instead, leave it; the reload prompt remains until
                        // the user saves or accepts the reload.
                    }
                }
            }
            ExternalState::Deleted => {
                editor.borrow_mut().set_file_path(None);
            }
        }
    }
}

/// Mark the currently-focused desktop window as closed and remove it.
fn close_focused_window(app: &mut Application) {
    let count = app.desktop.child_count();
    crate::trace_log!("close_focused_window: pre desktop_children={count}");
    for i in 0..count {
        if app.desktop.child_at(i).is_focused() {
            let state = app.desktop.child_at(i).state();
            app.desktop.child_at_mut(i).set_state(state | SF_CLOSED);
            break;
        }
    }
    app.desktop.remove_closed_windows();
    crate::trace_log!(
        "close_focused_window: post desktop_children={}",
        app.desktop.child_count()
    );
}

/// If the focused editor is the active debug target, prompt the user; on
/// confirmation, stop the debugger so the close can proceed. Returns false
/// when the user cancels (the close should be aborted).
fn confirm_close_debug_editor(app: &mut Application, ide: &mut IdeState) -> bool {
    let Some(editor) = focused_editor(app) else {
        return true;
    };
    let is_debug_target = match &ide.debug_editor {
        Some(de) => ide.debugger.is_running() && Rc::ptr_eq(de, &editor),
        None => false,
    };
    if !is_debug_target {
        return true;
    }

    use turbo_vision::views::msgbox::confirmation_box_yes_no;
    let answer = confirmation_box_yes_no(
        app,
        "Closing this window will stop debugging.\n\nDo you want to close it?",
    );
    if answer != CM_YES {
        return false;
    }

    ide.debugger.stop();
    ide.exec_line = None;
    ide.exec_editor = None;
    ide.watch_vars.clear();
    ide.callstack_frames.clear();
    ide.current_frame_idx = None;
    ide.debug_editor = None;
    ide.debug_synced_bps.clear();
    true
}

/// If the focused editor is dirty, prompt save / discard / cancel. Returns true
/// when it's safe to remove the window (saved or discarded).
fn confirm_close_focused_editor(app: &mut Application) -> bool {
    let Some(editor) = focused_editor(app) else {
        return true;
    };
    if !editor.borrow().is_dirty() {
        return true;
    }

    let name = editor
        .borrow()
        .file_path()
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Untitled".to_string());

    let result = message_box(
        app,
        &format!("{name} has been modified.\n\nSave changes?"),
        MF_YES_BUTTON | MF_NO_BUTTON | MF_CANCEL_BUTTON,
    );
    match result {
        CM_YES => {
            // Save via FileEditor::save (or save_as if no path).
            let has_path = editor.borrow().file_path().is_some();
            if has_path {
                editor.borrow_mut().save().is_ok()
            } else {
                editor.borrow_mut().prompt_save_as(app)
            }
        }
        CM_NO => true,
        _ => false,
    }
}

/// On quit, walk every editor; for each dirty one, prompt save/discard/cancel.
/// Returns false if the user cancels at any prompt.
fn confirm_close_all_dirty_editors(app: &mut Application) -> bool {
    // Snapshot editor Rcs first so prompts don't mutate the iteration.
    let mut editors: Vec<Rc<RefCell<IdeEditorWindow>>> = Vec::new();
    for i in 0..app.desktop.child_count() {
        if let Some(shared) = app
            .desktop
            .child_at(i)
            .as_any()
            .downcast_ref::<SharedIdeEditorWindow>()
        {
            editors.push(Rc::clone(&shared.0));
        }
    }

    for editor in editors {
        if !editor.borrow().is_dirty() {
            continue;
        }

        let name = editor
            .borrow()
            .file_path()
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Untitled".to_string());

        let result = message_box(
            app,
            &format!("{name} has been modified.\n\nSave changes?"),
            MF_YES_BUTTON | MF_NO_BUTTON | MF_CANCEL_BUTTON,
        );
        match result {
            CM_YES => {
                let has_path = editor.borrow().file_path().is_some();
                let saved = if has_path {
                    editor.borrow_mut().save().is_ok()
                } else {
                    editor.borrow_mut().prompt_save_as(app)
                };
                if !saved {
                    return false;
                }
            }
            CM_NO => {}
            _ => return false,
        }
    }
    true
}

/// Open the type-aware editor for the watch row that was just double-clicked.
/// No-op (with feedback dialog) when the variable type isn't editable or the
/// debugger isn't paused. The dialog itself lives in `value_editor` so other
/// languages can reuse it.
fn handle_watch_edit(app: &mut Application, ide: &mut IdeState, row: usize) {
    use turbo_vision::views::msgbox::{message_box_error, message_box_ok};

    let entry = ide
        .watch
        .borrow()
        .variable_at(row)
        .map(|(n, v, t)| (n.clone(), v.clone(), *t));
    let Some((name, current, ty)) = entry else {
        return;
    };

    if !ide.debugger.is_paused() {
        message_box_ok(
            app,
            "The program must be paused at a breakpoint to set a value.",
        );
        return;
    }

    if !ty.is_editable() {
        message_box_ok(
            app,
            &format!(
                "Variables of type {} are read-only in the watch window.",
                ty.label(),
            ),
        );
        return;
    }

    let Some(new_value) = crate::value_editor::prompt_set_value(app, &name, ty, &current) else {
        return;
    };
    let Some(expr_value) = crate::value_editor::format_setter_expr(ty, &new_value) else {
        message_box_error(
            app,
            &format!("'{new_value}' is not a valid {} literal.", ty.label(),),
        );
        return;
    };

    if let Err(e) = ide.debugger.set_variable(&name, &expr_value) {
        message_box_error(app, &format!("lldb error: {e}"));
    }
}

/// Navigate the editor to the source location of the clicked call-stack frame
/// and move the green "current statement" bar to that line. Frames whose
/// display has no `at file:line` (e.g. dyld bootstrap) are silently
/// ignored — there's no source to navigate to. The editor is matched by
/// file basename, since lldb prints just the basename in `bt` output; if
/// no editor is currently showing that file, the call falls back to the
/// active `debug_editor` (we don't auto-open unit files for now).
fn handle_callstack_jump(app: &mut Application, ide: &mut IdeState, row: usize) {
    let frame = ide
        .callstack
        .borrow()
        .frame_at(row)
        .map(|(i, d)| (*i, d.clone()));
    let Some((idx, display)) = frame else {
        return;
    };
    let Some((file, line)) = crate::debugger::parse_frame_location(&display) else {
        return;
    };
    let Some(editor) = jump_to_source_line(app, ide, &file, line) else {
        return;
    };
    // If the previous bar was on a different editor, clear it there so two
    // green bars don't linger when frames span multiple files.
    if let Some(prev) = ide.exec_editor.as_ref() {
        if !Rc::ptr_eq(prev, &editor) {
            prev.borrow_mut().set_current_exec_line(None);
        }
    }
    ide.exec_editor = Some(editor);
    ide.exec_line = Some(line);
    ide.current_frame_idx = Some(idx);
}

/// Find the desktop editor whose file path's basename matches `basename`,
/// focus it, and scroll to (1-based) `line`. Returns the editor for the
/// caller to wire up downstream state (e.g. the green-bar override).
/// Falls back to `debug_editor` if no desktop editor matches.
fn jump_to_source_line(
    app: &mut Application,
    ide: &IdeState,
    basename: &str,
    line: usize,
) -> Option<Rc<RefCell<IdeEditorWindow>>> {
    let mut target_idx: Option<usize> = None;
    for i in 0..app.desktop.child_count() {
        let Some(shared) = app
            .desktop
            .child_at(i)
            .as_any()
            .downcast_ref::<SharedIdeEditorWindow>()
        else {
            continue;
        };
        let Some(path) = shared.0.borrow().file_path() else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == basename {
            target_idx = Some(i);
            break;
        }
    }

    let editor = match target_idx {
        Some(idx) => {
            for i in 0..app.desktop.child_count() {
                app.desktop.child_at_mut(i).set_focus(i == idx);
            }
            app.desktop
                .child_at(idx)
                .as_any()
                .downcast_ref::<SharedIdeEditorWindow>()
                .map(|s| Rc::clone(&s.0))?
        }
        None => Rc::clone(ide.debug_editor.as_ref()?),
    };

    let editor_inner = editor.borrow().editor_rc();
    editor_inner.borrow_mut().scroll_to_line(line.saturating_sub(1));
    Some(editor)
}

/// Install a process-wide panic hook that appends the panic message + a
/// short backtrace to `/tmp/bruto-ide-panic.log` before chaining to the
/// default hook. The IDE runs inside crossterm's alt-screen, so the
/// default stderr panic message is wiped when the app teardown switches
/// back to the primary screen — without this, panics look like silent
/// exits. Idempotent across repeat IDE invocations.
fn install_panic_log_hook() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/bruto-ide-panic.log")
            {
                let _ = writeln!(
                    f,
                    "[{}] {info}\nbacktrace:\n{}",
                    chrono_now(),
                    std::backtrace::Backtrace::force_capture(),
                );
            }
            default_hook(info);
        }));
    });
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| format!("{}s since epoch", d.as_secs()))
        .unwrap_or_else(|_| "unknown time".into())
}

fn show_about_dialog(app: &mut Application, language_name: &str, override_text: Option<&str>) {
    use turbo_vision::views::msgbox::message_box_ok;
    let body: String = override_text.map(str::to_string).unwrap_or_else(|| {
        format!("Bruto IDE\n\nLanguage: {language_name}\n\n(c) 2026 Enzo Lombardi",)
    });
    message_box_ok(app, &body);
}

fn centered_dialog_bounds(app: &Application) -> Rect {
    let (tw, th) = app.terminal.size();
    let dw = 64i16.min(tw as i16 - 4);
    let dh = 18i16.min(th as i16 - 4);
    let x = ((tw as i16) - dw) / 2;
    let y = ((th as i16) - dh) / 2;
    Rect::new(x, y, x + dw, y + dh)
}

// ── View wrapper ─────────────────────────────────────────

struct WatchView(Rc<RefCell<WatchPanel>>);

impl View for WatchView {
    fn bounds(&self) -> Rect {
        self.0.borrow().bounds()
    }
    fn set_bounds(&mut self, b: Rect) {
        self.0.borrow_mut().set_bounds(b);
    }
    fn draw(&mut self, t: &mut turbo_vision::terminal::Terminal) {
        self.0.borrow_mut().draw(t);
    }
    fn handle_event(&mut self, e: &mut Event) {
        self.0.borrow_mut().handle_event(e);
    }
    fn state(&self) -> turbo_vision::core::state::StateFlags {
        self.0.borrow().state()
    }
    fn set_state(&mut self, s: turbo_vision::core::state::StateFlags) {
        self.0.borrow_mut().set_state(s);
    }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> {
        None
    }
}

struct CallStackView(Rc<RefCell<CallStackPanel>>);

impl View for CallStackView {
    fn bounds(&self) -> Rect {
        self.0.borrow().bounds()
    }
    fn set_bounds(&mut self, b: Rect) {
        self.0.borrow_mut().set_bounds(b);
    }
    fn draw(&mut self, t: &mut turbo_vision::terminal::Terminal) {
        self.0.borrow_mut().draw(t);
    }
    fn handle_event(&mut self, e: &mut Event) {
        self.0.borrow_mut().handle_event(e);
    }
    fn state(&self) -> turbo_vision::core::state::StateFlags {
        self.0.borrow().state()
    }
    fn set_state(&mut self, s: turbo_vision::core::state::StateFlags) {
        self.0.borrow_mut().set_state(s);
    }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> {
        None
    }
}

// ── Menu and status bar ──────────────────────────────────

fn build_menu_bar(width: i16) -> MenuBar {
    let file_menu = Menu::from_items(vec![
        MenuItem::with_shortcut("~N~ew", CM_NEW, 0, "", 0),
        MenuItem::with_shortcut("~O~pen...", CM_OPEN, KB_F3, "F3", 0),
        MenuItem::with_shortcut("~S~ave", CM_SAVE, KB_F2, "F2", 0),
        MenuItem::with_shortcut("Save ~A~s...", CM_SAVE_AS, 0, "", 0),
        MenuItem::separator(),
        MenuItem::with_shortcut("E~x~it", CM_QUIT, KB_ALT_X, "Alt-X", 0),
    ]);
    let edit_menu = Menu::from_items(vec![
        MenuItem::with_shortcut("~U~ndo", CM_UNDO, 0, "Ctrl-Z", 0),
        MenuItem::with_shortcut("~R~edo", CM_REDO, 0, "Ctrl-Y", 0),
        MenuItem::separator(),
        MenuItem::with_shortcut("Cu~t~", CM_CUT, 0, "Ctrl-X", 0),
        MenuItem::with_shortcut("~C~opy", CM_COPY, 0, "Ctrl-C", 0),
        MenuItem::with_shortcut("~P~aste", CM_PASTE, 0, "Ctrl-V", 0),
        MenuItem::separator(),
        MenuItem::with_shortcut("Select ~A~ll", CM_SELECT_ALL, 0, "Ctrl-A", 0),
    ]);
    let build_menu = Menu::from_items(vec![
        MenuItem::with_shortcut("~B~uild", CM_BUILD, KB_F9, "F9", 0),
        MenuItem::with_shortcut("~R~un", CM_RUN, 0, "Ctrl-F9", 0),
    ]);
    let debug_menu = Menu::from_items(vec![
        MenuItem::with_shortcut("~S~tart / Continue", CM_DEBUG_START, KB_F5, "F5", 0),
        MenuItem::with_shortcut("Step ~O~ver", CM_DEBUG_STEP_OVER, KB_F8, "F8", 0),
        MenuItem::with_shortcut("Step ~I~nto", CM_DEBUG_STEP_INTO, KB_F7, "F7", 0),
        MenuItem::separator(),
        MenuItem::with_shortcut("Sto~p~", CM_DEBUG_STOP, 0, "Shift-F5", 0),
    ]);
    let window_menu = Menu::from_items(vec![
        MenuItem::with_shortcut("~W~atches", CM_SHOW_WATCHES, 0, "", 0),
        MenuItem::with_shortcut("~O~utput", CM_SHOW_OUTPUT, 0, "", 0),
        MenuItem::with_shortcut("~C~all Stack", CM_SHOW_CALLSTACK, 0, "", 0),
    ]);
    let about_menu = Menu::from_items(vec![MenuItem::with_shortcut(
        "~A~bout...",
        CM_ABOUT,
        0,
        "",
        0,
    )]);

    let mut menu_bar = MenuBar::new(Rect::new(0, 0, width, 1));
    menu_bar.add_submenu(SubMenu::new("~F~ile", file_menu));
    menu_bar.add_submenu(SubMenu::new("~E~dit", edit_menu));
    menu_bar.add_submenu(SubMenu::new("~B~uild", build_menu));
    menu_bar.add_submenu(SubMenu::new("~D~ebug", debug_menu));
    menu_bar.add_submenu(SubMenu::new("~W~indows", window_menu));
    menu_bar.add_submenu(SubMenu::new("~H~elp", about_menu));
    menu_bar
}

fn build_status_line(width: i16, height: i16) -> StatusLine {
    StatusLine::new(
        Rect::new(0, height - 1, width, height),
        vec![
            StatusItem::new("~F5~ Debug", KB_F5, CM_DEBUG_START),
            StatusItem::new("~F7~ Step", KB_F7, CM_DEBUG_STEP_INTO),
            StatusItem::new("~F8~ Next", KB_F8, CM_DEBUG_STEP_OVER),
            StatusItem::new("~F9~ Build", KB_F9, CM_BUILD),
            StatusItem::new("~Alt-X~ Exit", KB_ALT_X, CM_QUIT),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::{extract_error_line, format_status_error};

    #[test]
    fn status_strips_parse_error_prefix() {
        assert_eq!(
            format_status_error("Parse error: line 6:1: expected ';', found 'var'"),
            "6:1: expected ';', found 'var'"
        );
    }

    #[test]
    fn status_strips_bare_line_prefix() {
        assert_eq!(
            format_status_error("line 12:5: unexpected token: BEGIN"),
            "12:5: unexpected token: BEGIN"
        );
    }

    #[test]
    fn status_passes_through_message_without_line_marker() {
        assert_eq!(
            format_status_error("ld: framework not found"),
            "ld: framework not found"
        );
    }

    #[test]
    fn extracts_pascal_parser_line() {
        assert_eq!(
            extract_error_line("line 12:5: unexpected token: BEGIN"),
            Some(12)
        );
    }

    #[test]
    fn extracts_when_prefixed_with_filename() {
        assert_eq!(
            extract_error_line("demo.pas: line 7:1: missing semicolon"),
            Some(7)
        );
    }

    #[test]
    fn returns_none_when_no_line_in_message() {
        assert_eq!(extract_error_line("ld: framework not found"), None);
        assert_eq!(extract_error_line("line abc: bogus"), None);
    }
}
