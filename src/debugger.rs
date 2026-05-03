/// Debugger integration — drives lldb via subprocess pipes.
///
/// Program stdout is separated from lldb output by redirecting the debuggee's
/// stdout to a temp file via `process launch --stdout`. A reader thread tails
/// that file into a dedicated channel.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as IoRead, Seek, SeekFrom, Write as IoWrite};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

#[derive(Debug, Clone)]
pub enum DebugState {
    Idle,
    Running,
    Paused { file: String, line: usize },
    Exited { code: i32 },
}

#[derive(Debug, Clone)]
pub enum DebugEvent {
    Stopped {
        file: String,
        line: usize,
    },
    Variables(Vec<(String, String, VarType)>),
    /// Program output (from the debuggee's stdout, not lldb).
    ProgramOutput(String),
    Exited {
        code: i32,
    },
}

/// Coarse Pascal-flavoured classification of an lldb variable's type, used
/// to drive the type-aware value editor in the watch window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarType {
    Integer,
    Real,
    Boolean,
    Char,
    String,
    Other,
}

impl VarType {
    pub fn label(self) -> &'static str {
        match self {
            VarType::Integer => "integer",
            VarType::Real => "real",
            VarType::Boolean => "boolean",
            VarType::Char => "char",
            VarType::String => "string",
            VarType::Other => "value",
        }
    }

    /// True when the IDE knows how to format a setter for `expr` and can
    /// validate user input — anything else is read-only in the watch.
    pub fn is_editable(self) -> bool {
        !matches!(self, VarType::Other | VarType::String)
    }
}

/// Map an lldb type string (the bit between parentheses, e.g. "long",
/// "char *", "double") to a coarse VarType.
pub fn classify_var_type(type_str: &str) -> VarType {
    let t = type_str.trim();
    if t == "char *" || t == "const char *" {
        return VarType::String;
    }
    if t == "char" || t == "signed char" {
        return VarType::Char;
    }
    if t == "bool" || t == "unsigned char" {
        return VarType::Boolean;
    }
    if t == "double" || t == "float" {
        return VarType::Real;
    }
    let int_types = [
        "long",
        "unsigned long",
        "long long",
        "unsigned long long",
        "int",
        "unsigned int",
        "short",
        "unsigned short",
        "i64",
        "i32",
        "i16",
        "i8",
        "u64",
        "u32",
        "u16",
        "u8",
    ];
    if int_types.contains(&t) {
        return VarType::Integer;
    }
    VarType::Other
}

/// The compiled program writes its output here via fprintf (see codegen.rs).
/// Resolved via bruto_lang::target so /tmp on Unix and %TEMP% on Windows
/// both work; the codegen embeds the same value into the program's IR.
fn console_file() -> String {
    bruto_lang::target::console_capture_path()
}

/// Per-variable metadata loaded from `<exe>.bruto-meta`.
#[derive(Debug, Clone)]
pub enum VarMeta {
    Enum(Vec<String>), // values in ordinal order
    Set,               // 4-word bitmask
    VariantRecord {
        // tag + cases
        tag_name: Option<String>,
        fixed_fields: Vec<(String, String)>, // (name, short type)
        cases: Vec<(Vec<i64>, Vec<(String, String)>)>,
    },
}

pub struct Debugger {
    pub state: DebugState,
    process: Option<Child>,
    stdin_tx: Option<std::process::ChildStdin>,
    /// Lines from lldb's own stdout (commands, frame info, etc.)
    lldb_rx: Option<mpsc::Receiver<String>>,
    /// Lines from the debuggee's redirected stdout
    program_rx: Option<mpsc::Receiver<String>>,
    /// Signal the program-output reader thread to stop
    stop_flag: Arc<AtomicBool>,
    source_file: String,
    breakpoint_ids: HashMap<usize, u32>,
    next_bp_id: u32,
    pending_var_request: bool,
    accumulated_lines: Vec<String>,
    /// Watch-window metadata loaded from <exe>.bruto-meta.
    var_meta: HashMap<String, VarMeta>,
}

impl Debugger {
    pub fn new() -> Self {
        Self {
            state: DebugState::Idle,
            process: None,
            stdin_tx: None,
            lldb_rx: None,
            program_rx: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            source_file: String::new(),
            breakpoint_ids: HashMap::new(),
            next_bp_id: 1,
            pending_var_request: false,
            accumulated_lines: Vec::new(),
            var_meta: HashMap::new(),
        }
    }

    /// Load `<exe>.bruto-meta` (if present) into the variable metadata map.
    fn load_metadata(&mut self, exe_path: &str) {
        self.var_meta.clear();
        let path = format!("{exe_path}.bruto-meta");
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return;
        };
        for line in contents.lines() {
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() < 2 {
                continue;
            }
            let name = parts[0].to_string();
            let kind = parts[1];
            let extra = parts.get(2).copied().unwrap_or("");
            let meta = match kind {
                "enum" => VarMeta::Enum(extra.split(',').map(|s| s.to_string()).collect()),
                "set" => VarMeta::Set,
                "vrec" => parse_vrec(extra),
                _ => continue,
            };
            self.var_meta.insert(name, meta);
        }
    }

    /// Start lldb on the given executable, setting breakpoints for the given lines.
    pub fn start(
        &mut self,
        exe_path: &str,
        source_file: &str,
        breakpoint_lines: &[usize],
    ) -> Result<(), String> {
        self.source_file = source_file.to_string();
        self.accumulated_lines.clear();
        self.pending_var_request = false;
        self.stop_flag.store(false, Ordering::Relaxed);
        self.load_metadata(exe_path);

        // Truncate the console capture file (program writes here via fprintf)
        let _ = std::fs::write(console_file(), "");

        // Launch lldb — we only load the target, don't run yet
        let mut child = Command::new("lldb")
            .arg("--no-use-colors")
            .arg(exe_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start lldb: {e}"))?;

        let stdin = child.stdin.take().ok_or("failed to get lldb stdin")?;
        let stdout = child.stdout.take().ok_or("failed to get lldb stdout")?;
        let stderr = child.stderr.take();

        // Reader thread for lldb's stdout (commands + debugger output)
        let (lldb_tx, lldb_rx) = mpsc::channel();
        let lldb_tx2 = lldb_tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if lldb_tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        if let Some(stderr) = stderr {
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    if let Ok(l) = line {
                        let _ = lldb_tx2.send(format!("[stderr] {l}"));
                    }
                }
            });
        }

        // Reader thread that tails the console capture file.
        // The compiled program writes here via fprintf (see codegen.rs).
        let (prog_tx, prog_rx) = mpsc::channel();
        let stop_flag = Arc::clone(&self.stop_flag);
        let capture_path = console_file();
        thread::spawn(move || {
            let mut pos: u64 = 0;
            let mut leftover = String::new();

            while !stop_flag.load(Ordering::Relaxed) {
                thread::sleep(std::time::Duration::from_millis(50));

                let Ok(mut file) = std::fs::File::open(&capture_path) else {
                    continue;
                };

                let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
                if file_len <= pos {
                    continue;
                }

                if file.seek(SeekFrom::Start(pos)).is_err() {
                    continue;
                }

                let mut buf = vec![0u8; (file_len - pos) as usize];
                let Ok(n) = file.read(&mut buf) else { continue };
                if n == 0 {
                    continue;
                }
                pos += n as u64;

                let chunk = String::from_utf8_lossy(&buf[..n]);
                leftover.push_str(&chunk);

                while let Some(nl) = leftover.find('\n') {
                    let line = leftover[..nl].to_string();
                    leftover = leftover[nl + 1..].to_string();
                    if prog_tx.send(line).is_err() {
                        return;
                    }
                }
            }

            if !leftover.is_empty() {
                let _ = prog_tx.send(leftover);
            }
        });

        self.stdin_tx = Some(stdin);
        self.lldb_rx = Some(lldb_rx);
        self.program_rx = Some(prog_rx);
        self.process = Some(child);

        // Wait for lldb to initialize
        std::thread::sleep(std::time::Duration::from_millis(500));

        self.send_command("settings set auto-confirm true")?;

        // Set breakpoints
        let source_basename = std::path::Path::new(source_file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(source_file);

        for &line in breakpoint_lines {
            self.send_command(&format!(
                "breakpoint set --file {source_basename} --line {line}"
            ))?;
            self.breakpoint_ids.insert(line, self.next_bp_id);
            self.next_bp_id += 1;
        }

        if breakpoint_lines.is_empty() {
            self.send_command("breakpoint set --name main")?;
        }

        // Force lldb to process all breakpoint commands by sending a
        // synchronous "version" query and draining the output.
        self.send_command("version")?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        if let Some(ref rx) = self.lldb_rx {
            while rx.try_recv().is_ok() {}
        }

        // Run the program (output goes to capture file via compiled-in fprintf)
        self.send_command("run")?;
        self.state = DebugState::Running;

        Ok(())
    }

    fn send_command(&mut self, cmd: &str) -> Result<(), String> {
        if let Some(ref mut stdin) = self.stdin_tx {
            writeln!(stdin, "{cmd}").map_err(|e| format!("write to lldb: {e}"))?;
            stdin
                .flush()
                .map_err(|e| format!("flush lldb stdin: {e}"))?;
            Ok(())
        } else {
            Err("lldb not running".into())
        }
    }

    pub fn continue_exec(&mut self) -> Result<(), String> {
        self.state = DebugState::Running;
        self.send_command("continue")
    }

    pub fn step_over(&mut self) -> Result<(), String> {
        self.send_command("next")?;
        self.pending_var_request = true;
        Ok(())
    }

    pub fn step_into(&mut self) -> Result<(), String> {
        self.send_command("step")?;
        self.pending_var_request = true;
        Ok(())
    }

    /// Send `expr <name> = <expr>` to lldb to mutate a paused-program variable
    /// and request a fresh `frame variable` dump so the watch panel updates.
    /// `expr_value` must already be a valid C/C++ expression (e.g. `42`,
    /// `3.14`, `'A'`, `true`); the caller is responsible for quoting.
    pub fn set_variable(&mut self, name: &str, expr_value: &str) -> Result<(), String> {
        self.send_command(&format!("expr {name} = {expr_value}"))?;
        self.pending_var_request = true;
        self.send_command("frame variable")?;
        Ok(())
    }

    pub fn add_breakpoint(&mut self, line: usize) -> Result<(), String> {
        let source_basename = std::path::Path::new(&self.source_file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.source_file)
            .to_string();
        self.send_command(&format!(
            "breakpoint set --file {source_basename} --line {line}"
        ))?;
        self.breakpoint_ids.insert(line, self.next_bp_id);
        self.next_bp_id += 1;
        Ok(())
    }

    pub fn remove_breakpoint(&mut self, line: usize) -> Result<(), String> {
        if let Some(id) = self.breakpoint_ids.remove(&line) {
            self.send_command(&format!("breakpoint delete {id}"))?;
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(ref mut stdin) = self.stdin_tx {
            let _ = writeln!(stdin, "process kill");
            let _ = stdin.flush();
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = writeln!(stdin, "quit");
            let _ = stdin.flush();
        }
        self.stdin_tx = None;
        self.lldb_rx = None;
        self.program_rx = None;
        if let Some(ref mut child) = self.process {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.process = None;
        self.state = DebugState::Idle;
        self.breakpoint_ids.clear();
        self.next_bp_id = 1;
        self.accumulated_lines.clear();
    }

    /// Poll for debugger events (non-blocking).
    pub fn poll(&mut self) -> Vec<DebugEvent> {
        let mut events = Vec::new();

        // 1. Drain program output (clean, no filtering needed)
        if let Some(ref rx) = self.program_rx {
            while let Ok(line) = rx.try_recv() {
                events.push(DebugEvent::ProgramOutput(line));
            }
        }

        // 2. Drain lldb output and parse debugger events
        let mut lldb_lines = Vec::new();
        if let Some(ref rx) = self.lldb_rx {
            while let Ok(line) = rx.try_recv() {
                lldb_lines.push(line);
            }
        }

        let mut needs_var_request = false;
        let mut already_stopped = false;

        let mut needs_continue = false;

        for line in lldb_lines {
            self.accumulated_lines.push(line.clone());

            if !already_stopped && line.contains("stop reason =") {
                // Some lldb stops include the source in the same line as
                // the "stop reason". Most don't — the `frame #0` line
                // arrives separately right after. Try here first; the
                // frame-#0 branch below handles the common case.
                if let Some(loc) = self.parse_stop_location() {
                    self.state = DebugState::Paused {
                        file: loc.0.clone(),
                        line: loc.1,
                    };
                    events.push(DebugEvent::Stopped {
                        file: loc.0,
                        line: loc.1,
                    });
                    needs_var_request = true;
                    self.pending_var_request = true;
                    already_stopped = true;
                }
            }

            if !already_stopped && !line.contains("stop reason") && line.contains("frame #0") {
                if let Some(loc) = parse_frame_location(&line) {
                    self.state = DebugState::Paused {
                        file: loc.0.clone(),
                        line: loc.1,
                    };
                    events.push(DebugEvent::Stopped {
                        file: loc.0,
                        line: loc.1,
                    });
                    needs_var_request = true;
                    self.pending_var_request = true;
                    already_stopped = true;
                } else {
                    // `frame #0:` arrived without an `at file:line` suffix —
                    // we've stepped out of `main` into dyld's bootstrap
                    // assembly (or another non-user frame). Auto-continue
                    // so the program runs the rest of process teardown
                    // and exits naturally; the "exited with status"
                    // detector below catches the result.
                    needs_continue = true;
                    already_stopped = true;
                }
            }

            if self.pending_var_request {
                if let Some((name, mut value, type_str)) = parse_variable_line(&line) {
                    // Override with Pascal-aware metadata if we have it.
                    if let Some(meta) = self.var_meta.get(&name) {
                        if let Some(formatted) = format_with_meta(meta, &value) {
                            value = formatted;
                        }
                    }
                    let ty = classify_var_type(&type_str);
                    events.push(DebugEvent::Variables(vec![(name, value, ty)]));
                }
            }

            if line.contains("exited with status") {
                let code = parse_exit_code(&line).unwrap_or(0);
                self.state = DebugState::Exited { code };
                events.push(DebugEvent::Exited { code });
            }
        }

        if needs_var_request {
            let _ = self.send_command("frame variable");
        }

        if needs_continue {
            let _ = self.send_command("continue");
        }

        events
    }

    fn parse_stop_location(&self) -> Option<(String, usize)> {
        for line in self.accumulated_lines.iter().rev().take(15) {
            if let Some(loc) = parse_frame_location(line) {
                return Some(loc);
            }
        }
        None
    }

    pub fn is_running(&self) -> bool {
        !matches!(self.state, DebugState::Idle)
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.state, DebugState::Paused { .. })
    }
}

impl Drop for Debugger {
    fn drop(&mut self) {
        self.stop();
    }
}

fn parse_frame_location(line: &str) -> Option<(String, usize)> {
    if let Some(at_pos) = line.find(" at ") {
        let rest = &line[at_pos + 4..];
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() >= 2 {
            let file = parts[0].trim().to_string();
            if let Ok(line_num) = parts[1].trim().parse::<usize>() {
                return Some((file, line_num));
            }
        }
    }
    None
}

fn parse_variable_line(line: &str) -> Option<(String, String, String)> {
    let trimmed = line.trim();
    if !trimmed.starts_with('(') {
        return None;
    }
    let paren_end = trimmed.find(") ")?;
    let type_str = &trimmed[1..paren_end];
    let rest = &trimmed[paren_end + 2..];
    let eq_pos = rest.find(" = ")?;
    let name = rest[..eq_pos].trim().to_string();
    let raw_value = rest[eq_pos + 3..].trim().to_string();

    // Skip internal runtime variables
    if name == "_capture" || name == "_end_bp" || name == "_bruto_capture_fp" {
        return None;
    }

    let value = format_variable_value(type_str, &raw_value);
    Some((name, value, type_str.to_string()))
}

/// Format a variable value for the watch window based on its lldb type.
fn format_variable_value(type_str: &str, raw: &str) -> String {
    // String (char *): extract quoted content
    // lldb shows: 0x100003f80 "Hello"
    if type_str == "char *" || type_str == "const char *" {
        if let Some(q_start) = raw.find('"') {
            if let Some(q_end) = raw.rfind('"') {
                if q_end > q_start {
                    return format!("'{}'", &raw[q_start + 1..q_end]);
                }
            }
        }
        // Null pointer
        if raw.trim() == "0x0000000000000000"
            || raw.trim() == "0x0"
            || raw.contains("nil")
            || raw.contains("NULL")
        {
            return "''".to_string();
        }
    }

    // Pointer types: show address or nil
    if type_str.ends_with('*') && !type_str.contains("char") {
        let addr = raw.trim();
        if addr == "0x0000000000000000"
            || addr == "0x0"
            || addr.contains("nil")
            || addr.contains("NULL")
        {
            return "nil".to_string();
        }
        return format!("^{raw}");
    }

    // Boolean (stored as i1 or bool)
    if type_str == "bool" || type_str == "unsigned char" {
        return match raw.as_ref() {
            "0" | "'\\0'" | "false" => "false".to_string(),
            "1" | "'\\x01'" | "true" => "true".to_string(),
            _ => raw.to_string(),
        };
    }

    // Char (i8 / signed char): show as character
    if type_str == "char" || type_str == "signed char" {
        // lldb may show: 65 'A' or just 65
        if let Some(q_start) = raw.find('\'') {
            if let Some(q_end) = raw.rfind('\'') {
                if q_end > q_start {
                    return raw[q_start..=q_end].to_string();
                }
            }
        }
        // Numeric — convert to char
        if let Ok(n) = raw.trim().parse::<i64>() {
            if (32..127).contains(&n) {
                return format!("'{}'", n as u8 as char);
            }
            return format!("#{n}");
        }
    }

    // Float (double / real)
    if type_str == "double" {
        // Trim trailing zeros for cleaner display
        if let Ok(f) = raw.parse::<f64>() {
            return format!("{:.10}", f)
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string();
        }
    }

    // Sets: stored as [4 x long] / [4 x i64]. Decode the 256-bit bitmask
    // and display as Pascal set literal.
    if (type_str == "long[4]" || type_str == "unsigned long[4]" || type_str == "i64[4]")
        && raw.starts_with('(')
    {
        if let Some(s) = decode_set_bitmask(raw) {
            return s;
        }
    }

    // Arrays: lldb shows ([0] = 1, [1] = 4, ...) — clean up index notation
    if type_str.contains('[') && raw.starts_with('(') {
        let mut cleaned = raw.to_string();
        // Remove [N] = prefixes, keep just values
        while let Some(start) = cleaned.find('[') {
            if let Some(end) = cleaned[start..].find("] = ") {
                cleaned = format!("{}{}", &cleaned[..start], &cleaned[start + end + 4..]);
            } else {
                break;
            }
        }
        return cleaned;
    }

    // Records/structs: lldb shows (field1 = val1, field2 = val2) — pass through
    // Already readable format

    raw.to_string()
}

/// Parse the body of a `name|vrec|<body>` metadata line into a VarMeta.
fn parse_vrec(body: &str) -> VarMeta {
    let mut tag_name: Option<String> = None;
    let mut fixed: Vec<(String, String)> = Vec::new();
    let mut cases: Vec<(Vec<i64>, Vec<(String, String)>)> = Vec::new();
    for part in body.split(';') {
        if part.is_empty() {
            continue;
        }
        if let Some(rest) = part.strip_prefix("__tag=") {
            tag_name = Some(rest.to_string());
        } else if let Some(rest) = part.strip_prefix("__case[") {
            // [vals]=field=type,field=type
            if let Some(end) = rest.find("]=") {
                let vals_str = &rest[..end];
                let fields_str = &rest[end + 2..];
                let vals: Vec<i64> = vals_str.split(',').filter_map(|v| v.parse().ok()).collect();
                let fs: Vec<(String, String)> = fields_str
                    .split(',')
                    .filter_map(|f| {
                        let (n, t) = f.split_once('=')?;
                        Some((n.to_string(), t.to_string()))
                    })
                    .collect();
                cases.push((vals, fs));
            }
        } else if let Some((n, t)) = part.split_once('=') {
            fixed.push((n.to_string(), t.to_string()));
        }
    }
    VarMeta::VariantRecord {
        tag_name,
        fixed_fields: fixed,
        cases,
    }
}

/// Apply Pascal-aware formatting to an lldb-formatted value.
fn format_with_meta(meta: &VarMeta, raw: &str) -> Option<String> {
    match meta {
        VarMeta::Enum(values) => {
            // raw is an integer like "2" — map to value name.
            let n: i64 = raw.trim().parse().ok()?;
            if (0..values.len() as i64).contains(&n) {
                Some(format!("{} ({n})", values[n as usize]))
            } else {
                None
            }
        }
        VarMeta::Set => {
            // raw might already be `[...]` from decode_set_bitmask, or a struct dump.
            if raw.starts_with('[') {
                return Some(raw.to_string());
            }
            decode_set_bitmask(raw)
        }
        VarMeta::VariantRecord {
            tag_name,
            fixed_fields,
            cases,
        } => {
            // raw is a struct dump like `(color = 7, kind = 1, radius = 10,
            // width = 10, height = 5)` — every variant's fields appear because
            // codegen emits them at overlapping offsets in DWARF. We pick the
            // active variant by tag value and drop the rest.
            let inner = raw.trim().trim_start_matches('(').trim_end_matches(')');
            let entries = split_top_level(inner);
            let mut field_values: Vec<(String, String)> = Vec::new();
            for entry in &entries {
                if let Some((n, v)) = entry.trim().split_once('=') {
                    field_values.push((n.trim().to_string(), v.trim().to_string()));
                }
            }
            let tag_value: Option<i64> = tag_name.as_ref().and_then(|tn| {
                field_values
                    .iter()
                    .find(|(n, _)| n == tn)
                    .and_then(|(_, v)| v.parse().ok())
            });

            let active_variant_fields: Vec<&str> = match tag_value
                .and_then(|tv| cases.iter().find(|(vs, _)| vs.contains(&tv)))
            {
                Some((_, fs)) => fs.iter().map(|(n, _)| n.as_str()).collect(),
                None => Vec::new(),
            };

            // Names of fields belonging to *some other* variant — we drop these
            // from the watch output so the user only sees the active case.
            let inactive_variant_fields: std::collections::HashSet<&str> = cases
                .iter()
                .flat_map(|(_, fs)| fs.iter().map(|(n, _)| n.as_str()))
                .filter(|n| !active_variant_fields.contains(n))
                .collect();

            let mut keep: Vec<String> = Vec::new();
            for (fname, fval) in &field_values {
                if inactive_variant_fields.contains(fname.as_str()) {
                    continue;
                }
                // Show fixed fields, the tag, and active-variant fields.
                let is_fixed = fixed_fields.iter().any(|(n, _)| n == fname);
                let is_tag = tag_name.as_ref().is_some_and(|t| t == fname);
                let is_active = active_variant_fields.iter().any(|n| n == fname);
                if is_fixed || is_tag || is_active {
                    keep.push(format!("{fname}={fval}"));
                }
            }
            Some(format!("({})", keep.join(", ")))
        }
    }
}

/// Split `inner` on commas at depth 0 (ignoring those inside parens).
fn split_top_level(inner: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' | '{' | '[' => {
                depth += 1;
                buf.push(c);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                buf.push(c);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut buf));
            }
            _ => buf.push(c),
        }
    }
    if !buf.is_empty() {
        parts.push(buf);
    }
    parts
}

/// Decode a 4-word set bitmask from lldb output like
/// `([0] = 0x000000000000000a, [1] = 0, [2] = 0, [3] = 0)` into `[1, 3]`.
fn decode_set_bitmask(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('(').trim_end_matches(')');
    let mut words: [u64; 4] = [0; 4];
    let mut count = 0;
    for part in trimmed.split(',') {
        let after_eq = part.split('=').nth(1)?.trim();
        let val: u64 = if let Some(rest) = after_eq.strip_prefix("0x") {
            u64::from_str_radix(rest.trim(), 16).ok()?
        } else {
            after_eq.parse::<i64>().ok()? as u64
        };
        if count < 4 {
            words[count] = val;
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    // Decode into ordinals.
    let mut ordinals: Vec<u32> = Vec::new();
    for (i, &w) in words.iter().enumerate() {
        if w == 0 {
            continue;
        }
        for b in 0..64u32 {
            if (w >> b) & 1 == 1 {
                ordinals.push((i as u32) * 64 + b);
            }
        }
    }
    if ordinals.is_empty() {
        return Some("[]".to_string());
    }
    // Compress consecutive ranges.
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ordinals.len() {
        let start = ordinals[i];
        let mut end = start;
        while i + 1 < ordinals.len() && ordinals[i + 1] == end + 1 {
            i += 1;
            end = ordinals[i];
        }
        if end == start {
            parts.push(start.to_string());
        } else if end == start + 1 {
            parts.push(format!("{start},{end}"));
        } else {
            parts.push(format!("{start}..{end}"));
        }
        i += 1;
    }
    Some(format!("[{}]", parts.join(", ")))
}

fn parse_exit_code(line: &str) -> Option<i32> {
    if let Some(pos) = line.find("status = ") {
        let rest = &line[pos + 9..];
        let num_str: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        num_str.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_integer_variable() {
        let r = parse_variable_line("(long) x = 42");
        assert_eq!(r, Some(("x".into(), "42".into(), "long".into())));
    }

    #[test]
    fn parse_string_variable() {
        let r = parse_variable_line(r#"(char *) msg = 0x0000000100000acb "Hello""#);
        assert_eq!(r, Some(("msg".into(), "'Hello'".into(), "char *".into())));
    }

    #[test]
    fn parse_double_variable() {
        let r = parse_variable_line("(double) r = 3.1400000000000001");
        assert_eq!(r, Some(("r".into(), "3.14".into(), "double".into())));
    }

    #[test]
    fn parse_pointer_variable() {
        let r = parse_variable_line("(long *) p = 0x0000600001234000");
        assert_eq!(
            r,
            Some(("p".into(), "^0x0000600001234000".into(), "long *".into()))
        );
    }

    #[test]
    fn parse_null_pointer() {
        let r = parse_variable_line("(long *) p = 0x0000000000000000");
        assert_eq!(r, Some(("p".into(), "nil".into(), "long *".into())));
    }

    #[test]
    fn classify_basic_types() {
        assert_eq!(classify_var_type("long"), VarType::Integer);
        assert_eq!(classify_var_type("double"), VarType::Real);
        assert_eq!(classify_var_type("bool"), VarType::Boolean);
        assert_eq!(classify_var_type("char"), VarType::Char);
        assert_eq!(classify_var_type("char *"), VarType::String);
        assert_eq!(classify_var_type("MyEnum"), VarType::Other);
    }

    #[test]
    fn skip_internal_variable() {
        assert_eq!(
            parse_variable_line("(void *) _capture = 0x0000000100000000"),
            None
        );
        assert_eq!(parse_variable_line("(long) _end_bp = 0"), None);
    }

    #[test]
    fn parse_non_variable_line() {
        assert_eq!(parse_variable_line("Process 1234 stopped"), None);
        assert_eq!(parse_variable_line("(lldb) frame variable"), None);
    }

    #[test]
    fn variant_record_filters_to_active_case() {
        let meta = VarMeta::VariantRecord {
            tag_name: Some("kind".into()),
            fixed_fields: vec![("color".into(), "long".into())],
            cases: vec![
                (vec![0], vec![("radius".into(), "double".into())]),
                (
                    vec![1],
                    vec![
                        ("width".into(), "double".into()),
                        ("height".into(), "double".into()),
                    ],
                ),
            ],
        };
        // Tag = 1 → only color, kind, width, height should remain.
        let raw = "(color = 7, kind = 1, radius = 10, width = 10, height = 5)";
        let formatted = format_with_meta(&meta, raw).unwrap();
        assert!(formatted.contains("color=7"));
        assert!(formatted.contains("kind=1"));
        assert!(formatted.contains("width=10"));
        assert!(formatted.contains("height=5"));
        assert!(!formatted.contains("radius"));
    }

    #[test]
    fn variant_record_unknown_tag_drops_all_variant_fields() {
        let meta = VarMeta::VariantRecord {
            tag_name: Some("kind".into()),
            fixed_fields: vec![("color".into(), "long".into())],
            cases: vec![
                (vec![0], vec![("radius".into(), "double".into())]),
                (vec![1], vec![("width".into(), "double".into())]),
            ],
        };
        let raw = "(color = 7, kind = 99, radius = 0, width = 0)";
        let formatted = format_with_meta(&meta, raw).unwrap();
        assert!(formatted.contains("color=7"));
        assert!(formatted.contains("kind=99"));
        assert!(!formatted.contains("radius"));
        assert!(!formatted.contains("width"));
    }
}
