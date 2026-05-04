/// Custom command IDs for the Pascal IDE.
///
/// turbo-vision core commands (CM_QUIT, CM_OPEN, CM_SAVE, etc.) are reused directly.
/// These are IDE-specific commands in the safe range (400+).

/// Build the current Pascal source file (F9)
pub const CM_BUILD: u16 = 400;

/// Run the compiled executable (Ctrl+F9)
pub const CM_RUN: u16 = 401;

/// Build and then run (Shift+F9)
pub const CM_BUILD_RUN: u16 = 402;

/// Start or continue debugging (F5)
pub const CM_DEBUG_START: u16 = 410;

/// Stop the debugger (Shift+F5)
pub const CM_DEBUG_STOP: u16 = 411;

/// Step over current line (F8)
pub const CM_DEBUG_STEP_OVER: u16 = 412;

/// Step into function call (F7)
pub const CM_DEBUG_STEP_INTO: u16 = 413;

/// Continue execution to next breakpoint (F5 while paused)
pub const CM_DEBUG_CONTINUE: u16 = 414;

/// Toggle breakpoint on current/clicked line
pub const CM_TOGGLE_BREAKPOINT: u16 = 415;

/// Broadcast: debugger hit a breakpoint, payload = line number
pub const CM_DEBUG_HIT_BREAK: u16 = 420;

/// Broadcast: variable values updated
pub const CM_DEBUG_VARS_UPDATE: u16 = 421;

/// Broadcast: highlight execution line
pub const CM_HIGHLIGHT_LINE: u16 = 422;

/// Broadcast: debugger process exited
pub const CM_DEBUG_EXITED: u16 = 423;

/// Show About dialog
pub const CM_ABOUT: u16 = 430;

/// EditorWindow window close request (translated from CM_CLOSE by IdeEditorWindow
/// so the IDE can show a save prompt before removing the editor).
pub const CM_CLOSE_EDITOR: u16 = 440;

/// Re-open the Watches window (Window menu) after the user closed it.
pub const CM_SHOW_WATCHES: u16 = 441;

/// Re-open the Output window (Window menu) after the user closed it.
pub const CM_SHOW_OUTPUT: u16 = 442;

/// Show / re-open the Call Stack window (hidden by default).
pub const CM_SHOW_CALLSTACK: u16 = 443;
