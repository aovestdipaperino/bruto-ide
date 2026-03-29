/// Debugger integration — drives lldb via subprocess pipes.
///
/// Program stdout is separated from lldb output by redirecting the debuggee's
/// stdout to a temp file via `process launch --stdout`. A reader thread tails
/// that file into a dedicated channel.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as IoRead, Seek, SeekFrom, Write as IoWrite};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    Stopped { file: String, line: usize },
    Variables(Vec<(String, String)>),
    /// Program output (from the debuggee's stdout, not lldb).
    ProgramOutput(String),
    Exited { code: i32 },
}

/// The compiled program writes its output here via fprintf (see codegen.rs).
const CONSOLE_FILE: &str = "/tmp/turbo_pascal_console.txt";

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

        // Truncate the console capture file (program writes here via fprintf)
        let _ = std::fs::write(CONSOLE_FILE, "");

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
                    Ok(l) => { if lldb_tx.send(l).is_err() { break; } }
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
        thread::spawn(move || {
            let mut pos: u64 = 0;
            let mut leftover = String::new();

            while !stop_flag.load(Ordering::Relaxed) {
                thread::sleep(std::time::Duration::from_millis(50));

                let Ok(mut file) = std::fs::File::open(CONSOLE_FILE) else {
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
                if n == 0 { continue; }
                pos += n as u64;

                let chunk = String::from_utf8_lossy(&buf[..n]);
                leftover.push_str(&chunk);

                while let Some(nl) = leftover.find('\n') {
                    let line = leftover[..nl].to_string();
                    leftover = leftover[nl + 1..].to_string();
                    if prog_tx.send(line).is_err() { return; }
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

        // Run the program (output goes to capture file via compiled-in fprintf)
        self.send_command("run")?;
        self.state = DebugState::Running;

        Ok(())
    }

    fn send_command(&mut self, cmd: &str) -> Result<(), String> {
        if let Some(ref mut stdin) = self.stdin_tx {
            writeln!(stdin, "{cmd}").map_err(|e| format!("write to lldb: {e}"))?;
            stdin.flush().map_err(|e| format!("flush lldb stdin: {e}"))?;
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

        for line in lldb_lines {
            self.accumulated_lines.push(line.clone());

            if !already_stopped && line.contains("stop reason =") {
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

            if !already_stopped
                && !line.contains("stop reason")
                && line.contains("frame #0")
            {
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
                    already_stopped = true;
                }
            }

            if self.pending_var_request {
                if let Some(var) = parse_variable_line(&line) {
                    events.push(DebugEvent::Variables(vec![var]));
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

fn parse_variable_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.starts_with('(') {
        if let Some(paren_end) = trimmed.find(") ") {
            let rest = &trimmed[paren_end + 2..];
            if let Some(eq_pos) = rest.find(" = ") {
                let name = rest[..eq_pos].trim().to_string();
                let value = rest[eq_pos + 3..].trim().to_string();
                return Some((name, value));
            }
        }
    }
    None
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
