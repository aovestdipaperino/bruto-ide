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
use crate::disasm_window::DisasmPanel;
use crate::heat::{LineProfile, text_hash};
use crate::ide_editor::{IdeEditorWindow, SharedIdeEditorWindow};
use crate::ide_file_editor::IdeFileEditor;
use crate::output_panel::OutputPanel;
use crate::profile_window::ProfilePanel;
use crate::watch_window::WatchPanel;
use bruto_lang::language::{BuildOptions, BuildPhase, BuildProfile, BuildResult, Language};

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
use turbo_vision::core::menu_data::{Menu, MenuItem, MenuItemBuilder};
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::core::state::State;
use turbo_vision::core::status_data::StatusItemBuilder;
use turbo_vision::views::View;
use turbo_vision::views::editor_traits::{Editor as _, ExternalState, FileEditor};
use turbo_vision::views::file_dialog::FileDialogBuilder;
use turbo_vision::views::group::GroupLike;
use turbo_vision::views::menu_bar::{MenuBar, SubMenu};
use turbo_vision::views::msgbox::{MsgBox, message_box};
use turbo_vision::views::shared::Shared;
use turbo_vision::views::status_line::StatusLine;
use crate::terminal_widget::TerminalWidget;
use turbo_vision::views::view::{ViewCore, dispatch_to_child};

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
    /// Compilation options the IDE starts with (Debug/Retail, optimization
    /// goal). The host typically loads these from its config file.
    pub build_options: BuildOptions,
    /// Invoked whenever the user confirms new options in Build → Options…,
    /// so the host can persist them.
    pub on_build_options_changed: Option<Box<dyn FnMut(&BuildOptions)>>,
}

impl Default for IdeOptions {
    fn default() -> Self {
        Self {
            show_about_on_start: false,
            on_about_shown: None,
            about_text: None,
            on_desktop_ready: None,
            build_options: BuildOptions::default(),
            on_build_options_changed: None,
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
    /// Layout used to (re-)spawn the Disassembly window.
    disasm_bounds: Rect,
    /// Same idea as `callstack_win_id`, for the Disassembly window.
    disasm_win_id: Option<turbo_vision::views::view::ViewId>,
    /// Shared Disassembly model — survives close/re-open so the listing
    /// persists until the next build.
    disasm: Rc<RefCell<DisasmPanel>>,
    /// Editor that produced the binary currently loaded (exe/source/asm
    /// paths in this struct). Set at the end of every successful build —
    /// the Disassembly window's click-to-source and click-to-toggle
    /// actions always target this editor, since the compiled listing can
    /// only ever describe the one file that was actually built.
    built_editor: Option<Rc<RefCell<IdeEditorWindow>>>,
    /// Host-supplied About dialog body, taken from IdeOptions at startup.
    /// `None` means fall back to the generic Bruto IDE blurb.
    about_text: Option<String>,
    /// Layout used to (re-)spawn the Profile window.
    profile_bounds: Rect,
    /// `Some(id)` while the Profile window is on the desktop.
    profile_win_id: Option<turbo_vision::views::view::ViewId>,
    /// Shared Profile model — survives close/re-open.
    profile_panel: Rc<RefCell<ProfilePanel>>,
    /// Editor whose buffer the last profile run measured; jumps from the
    /// Profile window land here.
    profile_editor: Option<Rc<RefCell<IdeEditorWindow>>>,
    /// Options used by Build / Run. Debug sessions always force a Debug
    /// build so lldb has DWARF to work with.
    build_options: BuildOptions,
    /// Host hook fired after the user changes `build_options`.
    on_build_options_changed: Option<Box<dyn FnMut(&BuildOptions)>>,
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
    // Size the panel to the new window's interior before re-adding it; the
    // user may have resized the previous window before closing it.
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
    watch_win.add(Shared::new(Rc::clone(watch)));
    watch_win.set_state_flag(State::SHADOW, false);
    app.desktop.add(watch_win)
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
    win.add(Shared::new(Rc::clone(callstack)));
    win.set_state_flag(State::SHADOW, false);
    app.desktop.add(win)
}

/// Wrap a fresh "Profile" `Window` around the shared [`ProfilePanel`] and
/// add it to the desktop. Mirrors [`install_callstack_window`].
fn install_profile_window(
    app: &mut Application,
    bounds: Rect,
    panel: &Rc<RefCell<ProfilePanel>>,
) -> turbo_vision::views::view::ViewId {
    let interior_w = bounds.width() - 2;
    let interior_h = bounds.height() - 2;
    panel
        .borrow_mut()
        .set_bounds(Rect::new(0, 0, interior_w, interior_h));

    let mut win = turbo_vision::views::window::Window::new_with_type(
        bounds,
        "Profile",
        turbo_vision::views::window::WindowPaletteType::Gray,
    );
    win.add(Shared::new(Rc::clone(panel)));
    win.set_state_flag(State::SHADOW, false);
    app.desktop.add(win)
}

/// Wrap a fresh "Disassembly" `Window` around the shared [`DisasmPanel`]
/// and add it to the desktop. Mirrors [`install_callstack_window`].
fn install_disasm_window(
    app: &mut Application,
    disasm_bounds: Rect,
    disasm: &Rc<RefCell<DisasmPanel>>,
) -> turbo_vision::views::view::ViewId {
    let interior_w = disasm_bounds.width() - 2;
    let interior_h = disasm_bounds.height() - 2;
    disasm
        .borrow_mut()
        .set_bounds(Rect::new(0, 0, interior_w, interior_h));

    let mut win = turbo_vision::views::window::Window::new_with_type(
        disasm_bounds,
        "Disassembly",
        turbo_vision::views::window::WindowPaletteType::Gray,
    );
    win.add(Shared::new(Rc::clone(disasm)));
    win.set_state_flag(State::SHADOW, false);
    app.desktop.add(win)
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
    let (w, h) = app.terminal.size();

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

    // ── Profile window (hidden at start). Lower right, wider than the
    // call stack so the inclusive/exclusive columns fit; the user can
    // drag/resize it once shown.
    let profile_bounds = Rect::new((editor_right - 34).max(0), callstack_top, w, editor_bottom);
    let profile_panel = Rc::new(RefCell::new(ProfilePanel::new(Rect::new(
        0,
        0,
        profile_bounds.width() - 2,
        profile_bounds.height() - 2,
    ))));

    // ── Disassembly (hidden at start). Assembly lines run long, so this
    // defaults to the same full-width strip as Output rather than the
    // narrow watch column; the two overlap when both are open, same as
    // Watch/Call Stack already do — drag/resize as needed.
    let disasm_bounds = Rect::new(0, editor_bottom, w, desktop_bottom);
    let disasm_interior_w = disasm_bounds.width() - 2;
    let disasm_interior_h = disasm_bounds.height() - 2;
    let disasm = Rc::new(RefCell::new(DisasmPanel::new(Rect::new(
        0,
        0,
        disasm_interior_w,
        disasm_interior_h,
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
        disasm_bounds,
        disasm_win_id: None,
        disasm: Rc::clone(&disasm),
        built_editor: None,
        about_text: options.about_text.take(),
        profile_bounds,
        profile_win_id: None,
        profile_panel: Rc::clone(&profile_panel),
        profile_editor: None,
        build_options: options.build_options,
        on_build_options_changed: options.on_build_options_changed.take(),
    };

    // ── Event loop ───────────────────────────────────────
    app.running = true;
    let mut pending_about = options.show_about_on_start;
    while app.running {
        update_command_states(&mut app, &ide);
        update_status_hint(&mut app);
        app.terminal.force_full_redraw();
        // Each top-level view draws in its own space; `draw_view` pushes
        // the view's origin around the call (turbo-vision 3.0).
        app.terminal.draw_view(&mut app.desktop);
        if let Some(ref mut mb) = app.menu_bar {
            app.terminal.draw_view(mb);
        }
        if let Some(ref mut sl) = app.status_line {
            app.terminal.draw_view(sl);
        }
        let _ = app.terminal.flush();
        // The editor paints its own caret cell, so the hardware cursor must
        // stay hidden here; otherwise it trails each diff-render's cursor
        // moves and flickers across the screen on every redraw (most
        // visibly while the mouse streams move events). Re-hidden every
        // frame because modal dialogs (`execute_modal`) show it for their
        // input lines and don't hide it on the way out.
        let _ = app.terminal.hide_cursor();

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
                            match ide.callstack_frames.iter().position(|(i, _)| *i == idx) {
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
        {
            let bp_lines: std::collections::HashSet<usize> = ide
                .built_editor
                .as_ref()
                .map(|e| e.borrow().breakpoint_lines().into_iter().collect())
                .unwrap_or_default();
            let mut d = disasm.borrow_mut();
            d.set_breakpoint_lines(bp_lines);
            d.set_highlighted_line(ide.exec_line);
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
                    dispatch_to_child(sl, &mut event);
                }

                if let Some(ref mut mb) = app.menu_bar {
                    dispatch_to_child(mb, &mut event);
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
                            let shift = event
                                .key_modifiers
                                .contains(crossterm::event::KeyModifiers::SHIFT);
                            event = Event::command(if shift { CM_PROFILE } else { CM_BUILD });
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

                dispatch_to_child(&mut app.desktop, &mut event);

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
                let _ = app.desktop.remove_closed_windows();

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
                if let Some(id) = ide.profile_win_id
                    && !app.desktop.contains_id(id)
                {
                    ide.profile_win_id = None;
                }
                if let Some(ed) = ide.profile_editor.as_ref()
                    && !ed.borrow().has_line_profile()
                {
                    ide.profile_panel.borrow_mut().clear();
                    ide.profile_editor = None;
                }
                if let Some(id) = ide.disasm_win_id {
                    if !app.desktop.contains_id(id) {
                        ide.disasm_win_id = None;
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

                let profile_jump = profile_panel.borrow_mut().take_pending_jump();
                if let Some(line) = profile_jump {
                    handle_profile_jump(&mut app, &mut ide, line);
                }
                let profile_correlate = profile_panel.borrow_mut().take_pending_correlate();
                if let Some(line) = profile_correlate {
                    handle_profile_correlate(&mut ide, line);
                }

                let pending_disasm_jump = disasm.borrow_mut().take_pending_jump();
                if let Some(idx) = pending_disasm_jump {
                    handle_disasm_jump(&mut app, &mut ide, idx);
                }
                let pending_disasm_toggle = disasm.borrow_mut().take_pending_toggle();
                if let Some(idx) = pending_disasm_toggle {
                    handle_disasm_toggle_breakpoint(&mut ide, idx);
                }

                // A double-click in the built editor correlates that
                // source line with the Disassembly window's highlight.
                // Only `built_editor` is checked — that's the only editor
                // whose line numbers the loaded listing actually describes.
                if let Some(line) = ide
                    .built_editor
                    .as_ref()
                    .and_then(|e| e.borrow().take_pending_correlate_line())
                {
                    disasm.borrow_mut().set_correlate_line(Some(line));
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
                let id = app.desktop.add(panel);
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
        CM_SHOW_DISASSEMBLY => {
            if ide.disasm_win_id.is_none() {
                let id = install_disasm_window(app, ide.disasm_bounds, &ide.disasm);
                ide.disasm_win_id = Some(id);
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
            let opts = ide.build_options;
            handle_build(app, language, &mut output_rc.borrow_mut(), ide, opts);
            true
        }
        CM_BUILD_OPTIONS => {
            if let Some(new_opts) =
                crate::build_options_dialog::prompt_build_options(app, ide.build_options)
            {
                if new_opts != ide.build_options {
                    ide.build_options = new_opts;
                    if let Some(cb) = ide.on_build_options_changed.as_mut() {
                        cb(&new_opts);
                    }
                }
                append_output_line(
                    &mut output_rc.borrow_mut(),
                    &format!("Build options: {}", new_opts.describe()),
                    Some(CONSOLE_INFO),
                );
            }
            true
        }
        CM_RUN => {
            let opts = ide.build_options;
            handle_build(app, language, &mut output_rc.borrow_mut(), ide, opts);
            if let Some(exe) = ide.exe_path.clone() {
                handle_run(&exe, &ide.console_capture_path, &mut output_rc.borrow_mut());
            }
            true
        }
        CM_PROFILE => {
            handle_profile(app, language, &mut output_rc.borrow_mut(), ide);
            true
        }
        CM_SHOW_PROFILE => {
            if ide.profile_win_id.is_none() {
                let id = install_profile_window(app, ide.profile_bounds, &ide.profile_panel);
                ide.profile_win_id = Some(id);
            }
            true
        }
        CM_TOGGLE_PROFILE_COLUMN => {
            if let Some(ed) = focused_editor(app) {
                let mut ed = ed.borrow_mut();
                let on = ed.profile_column_visible();
                ed.set_profile_column_visible(!on);
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
    let wrapper: SharedIdeEditorWindow = Shared::new(editor);
    app.desktop.add(wrapper);
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
        .hidden_toggle(true)
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
        .hidden_toggle(true)
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
    build_options: BuildOptions,
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
    append_output_line(
        output,
        &format!("Building ({})...", build_options.describe()),
        Some(CONSOLE_INFO),
    );

    // Drop any prior error highlight before we start; we'll re-set it
    // below if this build fails too. Each build is the authoritative
    // signal — leaving a stale red bar around after a successful build
    // would mislead the user.
    editor.borrow_mut().set_build_error(None);
    // A new build makes the last profile stale.
    editor.borrow_mut().set_line_profile(None);
    ide.profile_panel.borrow_mut().clear();
    // Same reasoning for a stale double-click correlation — it points at
    // the old listing, which `ide.disasm.clear()` below is about to drop.
    editor.borrow().set_asm_correlate_line(None);

    // Forget the previous binary up front so a failed or cancelled build
    // can't leave Run / Debug pointing at an exe built with other options.
    ide.exe_path = None;
    ide.disasm.borrow_mut().clear();
    let job = language.build_job_with(&source, file_path.as_deref(), &build_options);
    match run_build_with_progress(app, job) {
        Some(Ok(result)) => {
            crate::trace_log!("handle_build: ok exe={}", result.exe_path);
            ide.exe_path = Some(result.exe_path.clone());
            ide.source_path = Some(result.source_path);
            ide.console_capture_path = Some(result.console_capture_path);
            ide.built_editor = Some(Rc::clone(&editor));
            let (asm_lines, asm_notice) = load_disasm(&result.asm_path, build_options.profile);
            ide.disasm
                .borrow_mut()
                .set_lines(asm_lines, &source, asm_notice);
            append_output_line(
                output,
                &format!("Build successful: {}", result.exe_path),
                Some(SUCCESS),
            );
        }
        Some(Err(e)) => {
            crate::trace_log!("handle_build: err {e}");
            if let Some(line) = extract_error_line(&e) {
                editor.borrow_mut().set_build_error(Some((line, e.clone())));
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

/// Read and parse the `.s` listing a successful build produced, pairing
/// it with a one-line notice when the listing has no (or only partial)
/// Pascal source mapping — Retail builds strip debug info before
/// emitting assembly, so `.loc` directives (and therefore click-to-source
/// / click-to-breakpoint in the Disassembly window) aren't available.
fn load_disasm(
    asm_path: &Option<String>,
    profile: bruto_lang::language::BuildProfile,
) -> (Vec<bruto_lang::disasm::AsmLine>, Option<String>) {
    let Some(path) = asm_path else {
        return (
            Vec::new(),
            Some("Assembly listing unavailable for this target.".to_string()),
        );
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return (
            Vec::new(),
            Some("Could not read the assembly listing.".to_string()),
        );
    };
    let lines = bruto_lang::disasm::parse(&text);
    let notice = (profile == BuildProfile::Retail)
        .then(|| "Retail build — no Pascal source mapping; click does nothing.".to_string());
    (lines, notice)
}

/// Navigate `ide.built_editor` (the editor that produced the currently
/// loaded binary — see [`IdeState::built_editor`]) to the Pascal source
/// line mapped to disassembly row `idx`, focusing that editor window.
/// No-op if the row has no mapping (Retail build, or compiler-generated
/// code with no source counterpart) or the editor isn't open any more.
fn handle_disasm_jump(app: &mut Application, ide: &mut IdeState, idx: usize) {
    let Some(line) = ide.disasm.borrow().source_line_at(idx) else {
        return;
    };
    let Some(editor) = ide.built_editor.clone() else {
        return;
    };
    for i in 0..app.desktop.child_count() {
        if let Some(shared) = app
            .desktop
            .child_at(i)
            .as_any()
            .downcast_ref::<SharedIdeEditorWindow>()
        {
            let is_target = Rc::ptr_eq(shared.inner(), &editor);
            app.desktop.child_at_mut(i).set_focus(is_target);
        }
    }
    let editor_inner = editor.borrow().editor_rc();
    editor_inner
        .borrow_mut()
        .scroll_to_line(line.saturating_sub(1));
}

/// Toggle a breakpoint on `ide.built_editor`'s gutter at the Pascal source
/// line mapped to disassembly row `idx` — the same mechanism as clicking
/// the editor's own gutter. If a debug session is running, the next tick's
/// `sync_debug_breakpoints` picks up the change automatically. No-op if
/// the row has no mapping or the editor isn't open any more.
fn handle_disasm_toggle_breakpoint(ide: &mut IdeState, idx: usize) {
    let Some(line) = ide.disasm.borrow().source_line_at(idx) else {
        return;
    };
    let Some(editor) = ide.built_editor.clone() else {
        return;
    };
    editor
        .borrow()
        .gutter_rc()
        .borrow_mut()
        .toggle_breakpoint(line);
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
    use turbo_vision::app::ModalTick;
    use turbo_vision::core::command::CM_CANCEL;
    use turbo_vision::views::button::Button;
    use turbo_vision::views::dialog::Dialog;

    let (tw, th) = app.terminal.size();
    let dw = 50i16.min(tw - 4);
    let dh = 7i16;
    let x = (tw - dw) / 2;
    let y = (th - dh) / 2;
    let bounds = Rect::new(x, y, x + dw, y + dh);

    let mut dialog = Dialog::new(bounds, "Build");

    let progress_text = Rc::new(RefCell::new("Compiling…".to_string()));
    dialog.add(ProgressView::new(
        Rect::new(2, 2, dw - 2, 3),
        Rc::clone(&progress_text),
    ));

    let cancel_w = 12i16;
    let cancel_x = (dw - cancel_w) / 2;
    dialog.add(Button::new(
        Rect::new(cancel_x, dh - 4, cancel_x + cancel_w, dh - 2),
        "~C~ancel",
        CM_CANCEL,
        true,
    ));

    dialog.set_state_flag(State::MODAL, true);
    dialog.set_initial_focus();

    // Hide the terminal cursor for the duration so it doesn't blink in
    // the editor underneath — the dialog has no focusable text input
    // anyway, just the Cancel button.
    let _ = app.terminal.hide_cursor();

    // `execute_modal` is the library's one modal loop: it draws the desktop
    // and the dialog, dispatches events to the dialog, and calls the tick
    // closure once per iteration so the build can advance between frames.
    // Events route ONLY to the dialog — the desktop is purely visual while
    // the modal is up. Clicking Cancel ends the loop through the dialog's
    // own CloseOn policy; `job` then drops and kills any live child process.
    let mut outcome: Option<Result<BuildResult, String>> = None;
    app.execute_modal(&mut dialog, |_app, _dialog| match job.poll() {
        BuildPhase::Pending(label) => {
            *progress_text.borrow_mut() = label;
            ModalTick::Continue
        }
        BuildPhase::Done(r) => {
            outcome = Some(Ok(r));
            ModalTick::End(CM_BUILD_DONE)
        }
        BuildPhase::Failed(e) => {
            outcome = Some(Err(e));
            ModalTick::End(CM_BUILD_DONE)
        }
    });

    // Leave the hardware cursor hidden: the main loop never positions it
    // (the editor paints its own caret cell), so showing it here parked a
    // visible cursor that every redraw then dragged across the screen.
    outcome
}

/// One-line view used by the build progress dialog. Reads its text
/// from a `Rc<RefCell<String>>` so the polling loop can update what's
/// shown without rebuilding the dialog.
struct ProgressView {
    core: ViewCore,
    text: Rc<RefCell<String>>,
}

impl ProgressView {
    fn new(bounds: Rect, text: Rc<RefCell<String>>) -> Self {
        Self {
            core: ViewCore::new(bounds),
            text,
        }
    }
}

impl turbo_vision::views::View for ProgressView {
    fn core(&self) -> &ViewCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut ViewCore {
        &mut self.core
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn draw(&mut self, terminal: &mut turbo_vision::terminal::Terminal) {
        use turbo_vision::core::draw::DrawBuffer;
        use turbo_vision::views::view::write_line_to_terminal;

        let width = self.extent().width_clamped() as usize;
        let attr = self.map_color(1);
        let mut buf = DrawBuffer::new(width);
        buf.move_char(0, ' ', attr, width);
        let txt = self.text.borrow();
        buf.move_str(0, &txt, attr);
        write_line_to_terminal(terminal, 0, 0, &buf);
    }
    fn handle_event(&mut self, _event: &mut Event) {}
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

/// Build with instrumentation, run to completion, then load the profile
/// into the editor's heat column and the Profile window.
fn handle_profile(
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
    if ide.debugger.is_running() {
        append_output_line(
            output,
            "Stop the debugger before profiling.",
            Some(CONSOLE_INFO),
        );
        return;
    }
    let source = editor.borrow().editor_rc().borrow().get_text();
    let file_path = editor.borrow().file_path().map(std::path::PathBuf::from);
    output.clear();
    append_output_line(output, "Building with profiler...", Some(CONSOLE_INFO));
    editor.borrow_mut().set_build_error(None);
    editor.borrow_mut().set_line_profile(None);

    let job = language.profile_job_at(&source, file_path.as_deref());
    let result = match run_build_with_progress(app, job) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            if let Some(line) = extract_error_line(&e) {
                editor.borrow_mut().set_build_error(Some((line, e.clone())));
            }
            append_output_line(output, &format!("Build error: {e}"), Some(ERROR));
            use turbo_vision::views::msgbox::message_box_error;
            message_box_error(app, &e);
            return;
        }
        None => {
            append_output_line(output, "Build cancelled.", Some(CONSOLE_INFO));
            return;
        }
    };

    let Some(prof_path) = result.profile_path.clone() else {
        append_output_line(
            output,
            "Profiling is not available for this language.",
            Some(ERROR),
        );
        return;
    };
    let _ = std::fs::remove_file(&prof_path);
    append_output_line(
        output,
        &format!("Running {} (profiled)...", result.exe_path),
        Some(CONSOLE_INFO),
    );
    let capture = Some(result.console_capture_path.clone());
    if let Some(c) = &capture {
        let _ = std::fs::write(c, "");
    }
    let status = std::process::Command::new(&result.exe_path)
        .env("BRUTO_PROF_OUT", &prof_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Some(c) = &capture
        && let Ok(contents) = std::fs::read_to_string(c)
    {
        for line in contents.lines() {
            output.append_line_colored(line.to_string(), OUTPUT_TEXT);
        }
    }
    let code = match status {
        Ok(s) => s.code().unwrap_or(-1),
        Err(e) => {
            output.append_line_colored(format!("Failed to run: {e}"), CONSOLE_ERR);
            return;
        }
    };
    if !std::path::Path::new(&prof_path).exists() {
        output.append_line_colored(
            format!("Program exited with code {code}; no profile written"),
            ERROR,
        );
        return;
    }
    output.append_line_colored(
        format!("Exit code: {code}"),
        if code == 0 { SUCCESS } else { ERROR },
    );

    let profile = match language.load_profile(&result) {
        Ok(p) => p,
        Err(e) => {
            use turbo_vision::views::msgbox::message_box_error;
            message_box_error(app, &format!("Could not read profile:\n{e}"));
            return;
        }
    };
    if profile.truncated {
        output.append_line_colored(
            "Warning: profile truncated (too many call sites or recursion too deep)".to_string(),
            CONSOLE_INFO,
        );
    }
    output.append_line_colored(
        format!(
            "Profile: {} nodes, {:.1} ms total",
            profile.nodes.len(),
            profile.elapsed_ns as f64 / 1e6
        ),
        SUCCESS,
    );

    editor.borrow_mut().set_line_profile(Some(LineProfile {
        lines: profile.line_totals(),
        total_ns: profile.elapsed_ns.max(1),
        visible: true,
        text_hash: text_hash(&source),
    }));
    if let Some(prev) = &ide.profile_editor
        && !Rc::ptr_eq(prev, &editor)
    {
        prev.borrow_mut().set_line_profile(None);
    }
    ide.profile_editor = Some(Rc::clone(&editor));
    ide.profile_panel.borrow_mut().set_profile(Some(profile));
    if ide.profile_win_id.is_none() {
        let id = install_profile_window(app, ide.profile_bounds, &ide.profile_panel);
        ide.profile_win_id = Some(id);
    }
}

/// Scroll the profiled editor to `line` (1-based) and focus it.
fn handle_profile_jump(app: &mut Application, ide: &mut IdeState, line: usize) {
    let Some(editor) = ide.profile_editor.clone() else {
        return;
    };
    for i in 0..app.desktop.child_count() {
        let is_target = app
            .desktop
            .child_at(i)
            .as_any()
            .downcast_ref::<SharedIdeEditorWindow>()
            .is_some_and(|s| Rc::ptr_eq(s.inner(), &editor));
        app.desktop.child_at_mut(i).set_focus(is_target);
    }
    editor
        .borrow()
        .editor_rc()
        .borrow_mut()
        .scroll_to_line(line.saturating_sub(1));
}

/// Give Pascal line `line` the cyan correlation highlight after a click
/// in the Profile tree — the same highlight a double-click on the source
/// line produces. The profiled editor is scrolled to the line without
/// taking focus from the Profile window, and the Disassembly window
/// follows when its listing was built from that same editor (its line
/// numbers describe no other file).
fn handle_profile_correlate(ide: &mut IdeState, line: usize) {
    let Some(editor) = ide.profile_editor.clone() else {
        return;
    };
    editor.borrow().set_asm_correlate_line(Some(line));
    editor
        .borrow()
        .editor_rc()
        .borrow_mut()
        .scroll_to_line(line.saturating_sub(1));
    if ide
        .built_editor
        .as_ref()
        .is_some_and(|b| Rc::ptr_eq(b, &editor))
    {
        ide.disasm.borrow_mut().set_correlate_line(Some(line));
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

    // lldb needs DWARF: debug sessions always use a Debug build, whatever
    // the Build Options say.
    let opts = ide.build_options.for_debugging();
    if opts != ide.build_options {
        append_output_line(
            output,
            "Retail build selected — debugging with a Debug build instead.",
            Some(CONSOLE_INFO),
        );
    }
    handle_build(app, language, output, ide, opts);
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
    let profile_open = ide.profile_win_id.is_some();
    let has_profile = focused
        .as_ref()
        .is_some_and(|e| e.borrow().has_line_profile());
    let disasm_open = ide.disasm_win_id.is_some();
    // Cleared at the start of every build and only set again on success —
    // exactly "there's a current build to show the assembly of".
    let has_build = ide.exe_path.is_some();

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
    toggle(CM_PROFILE, editor_focused && !dbg_running);
    toggle(CM_SHOW_PROFILE, !profile_open);
    toggle(CM_TOGGLE_PROFILE_COLUMN, has_profile);
    toggle(CM_SHOW_DISASSEMBLY, !disasm_open && has_build);
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
            return Some(Rc::clone(shared.inner()));
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
        let Some(existing) = shared.inner().borrow().file_path() else {
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
            editors.push(Rc::clone(shared.inner()));
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
            app.desktop
                .child_at_mut(i)
                .set_state_flag(State::CLOSED, true);
            break;
        }
    }
    let _ = app.desktop.remove_closed_windows();
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
        MsgBox::YES_BUTTON | MsgBox::NO_BUTTON | MsgBox::CANCEL_BUTTON,
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
            editors.push(Rc::clone(shared.inner()));
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
            MsgBox::YES_BUTTON | MsgBox::NO_BUTTON | MsgBox::CANCEL_BUTTON,
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
        let Some(path) = shared.inner().borrow().file_path() else {
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
                .map(|s| Rc::clone(s.inner()))?
        }
        None => Rc::clone(ide.debug_editor.as_ref()?),
    };

    let editor_inner = editor.borrow().editor_rc();
    editor_inner
        .borrow_mut()
        .scroll_to_line(line.saturating_sub(1));
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
    let dw = 64i16.min(tw - 4);
    let dh = 18i16.min(th - 4);
    let x = (tw - dw) / 2;
    let y = (th - dh) / 2;
    Rect::new(x, y, x + dw, y + dh)
}

// ── Menu and status bar ──────────────────────────────────

/// A menu entry with a bound key and the label shown beside it. `label` is
/// display only when `key` is 0 (the editor handles Ctrl+Z and friends
/// itself; the label just documents the binding).
fn item(
    text: &str,
    command: turbo_vision::core::command::CommandId,
    key: u16,
    label: &str,
) -> MenuItem {
    let mut b = MenuItemBuilder::new().text(text).command(command);
    if key != 0 {
        b = b.key_code(key);
    }
    if !label.is_empty() {
        b = b.shortcut(label);
    }
    b.build()
}

fn build_menu_bar(width: i16) -> MenuBar {
    let file_menu = Menu::from_items(vec![
        item("~N~ew", CM_NEW, 0, ""),
        item("~O~pen...", CM_OPEN, KB_F3, "F3"),
        item("~S~ave", CM_SAVE, KB_F2, "F2"),
        item("Save ~A~s...", CM_SAVE_AS, 0, ""),
        MenuItem::separator(),
        item("E~x~it", CM_QUIT, KB_ALT_X, "Alt-X"),
    ]);
    let edit_menu = Menu::from_items(vec![
        item("~U~ndo", CM_UNDO, 0, "Ctrl-Z"),
        item("~R~edo", CM_REDO, 0, "Ctrl-Y"),
        MenuItem::separator(),
        item("Cu~t~", CM_CUT, 0, "Ctrl-X"),
        item("~C~opy", CM_COPY, 0, "Ctrl-C"),
        item("~P~aste", CM_PASTE, 0, "Ctrl-V"),
        MenuItem::separator(),
        item("Select ~A~ll", CM_SELECT_ALL, 0, "Ctrl-A"),
    ]);
    let build_menu = Menu::from_items(vec![
        item("~B~uild", CM_BUILD, KB_F9, "F9"),
        item("~R~un", CM_RUN, 0, "Ctrl-F9"),
        MenuItem::separator(),
        item("~O~ptions...", CM_BUILD_OPTIONS, 0, ""),
    ]);
    let debug_menu = Menu::from_items(vec![
        item("~S~tart / Continue", CM_DEBUG_START, KB_F5, "F5"),
        item("Step ~O~ver", CM_DEBUG_STEP_OVER, KB_F8, "F8"),
        item("Step ~I~nto", CM_DEBUG_STEP_INTO, KB_F7, "F7"),
        MenuItem::separator(),
        item("Sto~p~", CM_DEBUG_STOP, 0, "Shift-F5"),
        MenuItem::separator(),
        item("~P~rofile", CM_PROFILE, 0, "Shift-F9"),
    ]);
    let window_menu = Menu::from_items(vec![
        item("~W~atches", CM_SHOW_WATCHES, 0, ""),
        item("~O~utput", CM_SHOW_OUTPUT, 0, ""),
        item("~C~all Stack", CM_SHOW_CALLSTACK, 0, ""),
        item("~P~rofile", CM_SHOW_PROFILE, 0, ""),
        item("Profile Colu~m~n", CM_TOGGLE_PROFILE_COLUMN, 0, ""),
        item("~D~isassembly", CM_SHOW_DISASSEMBLY, 0, ""),
    ]);
    let about_menu = Menu::from_items(vec![item("~A~bout...", CM_ABOUT, 0, "")]);

    let mut menu_bar = MenuBar::new(Rect::new(0, 0, width, 1));
    menu_bar.add_submenu(SubMenu::new("~F~ile", file_menu));
    menu_bar.add_submenu(SubMenu::new("~E~dit", edit_menu));
    menu_bar.add_submenu(SubMenu::new("~B~uild", build_menu));
    menu_bar.add_submenu(SubMenu::new("~D~ebug", debug_menu));
    menu_bar.add_submenu(SubMenu::new("~W~indows", window_menu));
    menu_bar.add_submenu(SubMenu::new("~H~elp", about_menu));
    menu_bar
}

fn status(
    text: &str,
    key: u16,
    command: turbo_vision::core::command::CommandId,
) -> turbo_vision::core::status_data::StatusItem {
    StatusItemBuilder::new()
        .text(text)
        .key_code(key)
        .command(command)
        .build()
}

fn build_status_line(width: i16, height: i16) -> StatusLine {
    StatusLine::new(
        Rect::new(0, height - 1, width, height),
        vec![
            status("~F5~ Debug", KB_F5, CM_DEBUG_START),
            status("~F7~ Step", KB_F7, CM_DEBUG_STEP_INTO),
            status("~F8~ Next", KB_F8, CM_DEBUG_STEP_OVER),
            status("~F9~ Build", KB_F9, CM_BUILD),
            status("~Alt-X~ Exit", KB_ALT_X, CM_QUIT),
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
