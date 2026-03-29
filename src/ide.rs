/// IDE runner — takes a Language implementation and runs the TUI.

use crate::commands::*;
use crate::debugger::{DebugEvent, Debugger};
use crate::gutter::BreakpointGutter;
use crate::ide_editor::IdeEditorWindow;
use crate::language::Language;
use crate::output_panel::OutputPanel;
use crate::watch_window::WatchPanel;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use turbo_vision::app::Application;
use turbo_vision::core::command::{CM_NEW, CM_OPEN, CM_QUIT, CM_SAVE, CM_SAVE_AS};
use turbo_vision::core::event::{Event, EventType, KB_F2, KB_F3, KB_F5, KB_F7, KB_F8, KB_F9};
use turbo_vision::core::geometry::Rect;
use turbo_vision::core::menu_data::{Menu, MenuItem};
use turbo_vision::core::palette::{Attr, TvColor};
use turbo_vision::views::menu_bar::{MenuBar, SubMenu};
use turbo_vision::views::status_line::{StatusItem, StatusLine};
use turbo_vision::views::terminal_widget::TerminalWidget;
use turbo_vision::views::View;

struct IdeState {
    debugger: Debugger,
    watch_vars: Vec<(String, String)>,
    source_path: Option<String>,
    exe_path: Option<String>,
    console_capture_path: Option<String>,
    exec_line: Option<usize>,
}

impl IdeState {
    fn new() -> Self {
        Self {
            debugger: Debugger::new(),
            watch_vars: Vec::new(),
            source_path: None,
            exe_path: None,
            console_capture_path: None,
            exec_line: None,
        }
    }
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

    // ── Editor window ────────────────────────────────────
    let title = format!("Untitled.{}", language.file_extension());
    let editor_bounds = Rect::new(0, desktop_top, editor_right, editor_bottom);
    let ide_win = IdeEditorWindow::new(editor_bounds, &title);
    ide_win.set_highlighter(language.create_highlighter());
    ide_win.set_text(language.sample_program());
    let editor_rc = ide_win.editor_rc();
    let gutter_rc = ide_win.gutter_rc();
    app.desktop.add(Box::new(ide_win));

    // ── Watch dialog ─────────────────────────────────────
    let watch_bounds = Rect::new(editor_right, desktop_top - 1, w, editor_bottom - 1);
    let watch_interior_w = watch_bounds.width() - 2;
    let watch_interior_h = watch_bounds.height() - 2;
    let watch = Rc::new(RefCell::new(WatchPanel::new(
        Rect::new(0, 0, watch_interior_w, watch_interior_h),
    )));
    let mut watch_dlg = turbo_vision::views::dialog::Dialog::new(watch_bounds, "Watches");
    watch_dlg.add(Box::new(WatchView(Rc::clone(&watch))));
    {
        use turbo_vision::core::state::SF_SHADOW;
        let state = watch_dlg.state();
        watch_dlg.set_state(state & !SF_SHADOW);
    }
    app.desktop.add(Box::new(watch_dlg));

    // ── Output dialog ────────────────────────────────────
    let output_bounds = Rect::new(0, editor_bottom, w, desktop_bottom);
    let output_panel = OutputPanel::new(output_bounds, "Output");
    let output_term = output_panel.terminal_rc();
    output_term.borrow_mut().append_line_colored(
        format!("Bruto IDE ready ({}).  Press F9 to build.", language.name()),
        OUTPUT_TEXT,
    );
    app.desktop.add(Box::new(output_panel));

    let mut ide = IdeState::new();

    // ── Event loop ───────────────────────────────────────
    app.running = true;
    while app.running {
        app.terminal.force_full_redraw();
        app.desktop.draw(&mut app.terminal);
        if let Some(ref mut mb) = app.menu_bar {
            mb.draw(&mut app.terminal);
        }
        if let Some(ref mut sl) = app.status_line {
            sl.draw(&mut app.terminal);
        }
        let _ = app.terminal.flush();

        watch.borrow_mut().set_variables(ide.watch_vars.clone());
        gutter_rc.borrow_mut().set_current_exec_line(ide.exec_line);

        // Poll debugger
        if ide.debugger.is_running() {
            let events = ide.debugger.poll();
            for dbg_event in events {
                match dbg_event {
                    DebugEvent::Stopped { line, .. } => {
                        ide.exec_line = Some(line);
                    }
                    DebugEvent::Variables(vars) => {
                        for (name, value) in vars {
                            let mut found = false;
                            for (n, v) in &mut ide.watch_vars {
                                if *n == name {
                                    *v = value.clone();
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                ide.watch_vars.push((name, value));
                            }
                        }
                    }
                    DebugEvent::ProgramOutput(line) => {
                        append_output_line(&mut output_term.borrow_mut(), &line, None);
                    }
                    DebugEvent::Exited { code } => {
                        ide.exec_line = None;
                        ide.watch_vars.clear();
                        ide.debugger.stop();
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
                        KB_F9 => {
                            handle_build(&language, &editor_rc, &mut output_term.borrow_mut(), &mut ide);
                            event.clear();
                        }
                        KB_F5 => {
                            handle_debug_start_continue(
                                &language, &editor_rc, &gutter_rc, &mut output_term.borrow_mut(), &mut ide,
                            );
                            event.clear();
                        }
                        KB_F7 => {
                            if ide.debugger.is_running() { let _ = ide.debugger.step_into(); }
                            event.clear();
                        }
                        KB_F8 => {
                            if ide.debugger.is_running() { let _ = ide.debugger.step_over(); }
                            event.clear();
                        }
                        _ => {}
                    }
                }

                if event.what == EventType::Command {
                    let handled = handle_command(
                        event.command, &mut app, &language, &editor_rc, &gutter_rc, &output_term, &mut ide,
                    );
                    if handled { event.clear(); }
                }

                app.desktop.handle_event(&mut event);
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
    editor_rc: &Rc<RefCell<turbo_vision::views::editor::Editor>>,
    gutter: &Rc<RefCell<BreakpointGutter>>,
    output_rc: &Rc<RefCell<TerminalWidget>>,
    ide: &mut IdeState,
) -> bool {
    match cmd {
        CM_QUIT => { ide.debugger.stop(); app.running = false; true }
        CM_BUILD => { handle_build(language, editor_rc, &mut output_rc.borrow_mut(), ide); true }
        CM_RUN => {
            handle_build(language, editor_rc, &mut output_rc.borrow_mut(), ide);
            if let Some(exe) = ide.exe_path.clone() {
                handle_run(&exe, &ide.console_capture_path, &mut output_rc.borrow_mut());
            }
            true
        }
        CM_DEBUG_START | CM_DEBUG_CONTINUE => {
            handle_debug_start_continue(language, editor_rc, gutter, &mut output_rc.borrow_mut(), ide);
            true
        }
        CM_DEBUG_STOP => {
            ide.debugger.stop(); ide.exec_line = None; ide.watch_vars.clear();
            append_output_line(&mut output_rc.borrow_mut(), "Debugger stopped.", Some(CONSOLE_INFO));
            true
        }
        CM_DEBUG_STEP_OVER => { if ide.debugger.is_running() { let _ = ide.debugger.step_over(); } true }
        CM_DEBUG_STEP_INTO => { if ide.debugger.is_running() { let _ = ide.debugger.step_into(); } true }
        CM_ABOUT => {
            use turbo_vision::views::msgbox::message_box_ok;
            message_box_ok(app, &format!(
                "Bruto IDE\n\nVersion 0.1.0\n\nLanguage: {}\n\nBuilt with Turbo Vision for Rust\n\n(c) 2026 Enzo Lombardi",
                language.name(),
            ));
            true
        }
        _ => false,
    }
}

fn handle_build(
    language: &Box<dyn Language>,
    editor_rc: &Rc<RefCell<turbo_vision::views::editor::Editor>>,
    output: &mut TerminalWidget,
    ide: &mut IdeState,
) {
    let source = editor_rc.borrow().get_text();
    output.clear();
    append_output_line(output, "Building...", Some(CONSOLE_INFO));

    match language.build(&source) {
        Ok(result) => {
            ide.exe_path = Some(result.exe_path.clone());
            ide.source_path = Some(result.source_path);
            ide.console_capture_path = Some(result.console_capture_path);
            append_output_line(output, &format!("Build successful: {}", result.exe_path), Some(SUCCESS));
        }
        Err(e) => {
            append_output_line(output, &format!("Build error: {}", e), Some(ERROR));
        }
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
    language: &Box<dyn Language>,
    editor_rc: &Rc<RefCell<turbo_vision::views::editor::Editor>>,
    gutter: &Rc<RefCell<BreakpointGutter>>,
    output: &mut TerminalWidget,
    ide: &mut IdeState,
) {
    if ide.debugger.is_running() {
        let _ = ide.debugger.continue_exec();
        ide.exec_line = None;
        return;
    }

    handle_build(language, editor_rc, output, ide);
    let Some(exe_path) = ide.exe_path.clone() else {
        append_output_line(output, "No executable to debug.", Some(ERROR));
        return;
    };

    let source_file = ide.source_path.clone().unwrap_or_default();
    let bp_lines = gutter.borrow().breakpoint_lines();

    append_output_line(
        output,
        &format!("Starting debugger with {} breakpoint(s)...", bp_lines.len()),
        Some(CONSOLE_INFO),
    );
    output.clear();

    match ide.debugger.start(&exe_path, &source_file, &bp_lines) {
        Ok(()) => append_output_line(output, "Debugger started.", Some(SUCCESS)),
        Err(e) => append_output_line(output, &format!("Debugger error: {}", e), Some(ERROR)),
    }
}

// ── View wrapper ─────────────────────────────────────────

struct WatchView(Rc<RefCell<WatchPanel>>);

impl View for WatchView {
    fn bounds(&self) -> Rect { self.0.borrow().bounds() }
    fn set_bounds(&mut self, b: Rect) { self.0.borrow_mut().set_bounds(b); }
    fn draw(&mut self, t: &mut turbo_vision::terminal::Terminal) { self.0.borrow_mut().draw(t); }
    fn handle_event(&mut self, e: &mut Event) { self.0.borrow_mut().handle_event(e); }
    fn state(&self) -> turbo_vision::core::state::StateFlags { self.0.borrow().state() }
    fn set_state(&mut self, s: turbo_vision::core::state::StateFlags) { self.0.borrow_mut().set_state(s); }
    fn get_palette(&self) -> Option<turbo_vision::core::palette::Palette> { None }
}

// ── Menu and status bar ──────────────────────────────────

fn build_menu_bar(width: i16) -> MenuBar {
    let file_menu = Menu::from_items(vec![
        MenuItem::with_shortcut("~N~ew", CM_NEW, 0, "", 0),
        MenuItem::with_shortcut("~O~pen...", CM_OPEN, KB_F3, "F3", 0),
        MenuItem::with_shortcut("~S~ave", CM_SAVE, KB_F2, "F2", 0),
        MenuItem::with_shortcut("Save ~A~s...", CM_SAVE_AS, 0, "", 0),
        MenuItem::separator(),
        MenuItem::with_shortcut("E~x~it", CM_QUIT, 0x012D, "Alt-X", 0),
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
    let about_menu = Menu::from_items(vec![
        MenuItem::with_shortcut("~A~bout...", CM_ABOUT, 0, "", 0),
    ]);

    let mut menu_bar = MenuBar::new(Rect::new(0, 0, width, 1));
    menu_bar.add_submenu(SubMenu::new("~F~ile", file_menu));
    menu_bar.add_submenu(SubMenu::new("~B~uild", build_menu));
    menu_bar.add_submenu(SubMenu::new("~D~ebug", debug_menu));
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
            StatusItem::new("~Alt-X~ Exit", 0x012D, CM_QUIT),
        ],
    )
}
