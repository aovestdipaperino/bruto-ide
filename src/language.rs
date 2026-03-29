/// The Language trait abstracts a programming language for the IDE.
///
/// Implement this trait to add support for a new language. The IDE calls
/// these methods to get syntax highlighting, build executables, and
/// display language-specific information.

/// Result of a successful build.
pub struct BuildResult {
    /// Path to the compiled executable.
    pub exe_path: String,
    /// Path to the source file on disk (needed for DWARF debug info resolution).
    pub source_path: String,
    /// Path to the console output capture file (program writes here via compiled-in code).
    pub console_capture_path: String,
}

pub trait Language {
    /// Display name shown in the About dialog (e.g. "Mini-Pascal").
    fn name(&self) -> &str;

    /// File extension without the leading dot (e.g. "pas").
    fn file_extension(&self) -> &str;

    /// Sample program loaded into the editor on startup.
    fn sample_program(&self) -> &str;

    /// Create a syntax highlighter for the turbo-vision Editor.
    fn create_highlighter(&self) -> Box<dyn turbo_vision::views::syntax::SyntaxHighlighter>;

    /// Compile source text into a native executable with debug info.
    ///
    /// The implementation must:
    /// 1. Save the source to a file on disk (so lldb can resolve DWARF references)
    /// 2. Parse and compile the source
    /// 3. Emit an executable with DWARF debug metadata
    /// 4. Return paths to the executable, source file, and console capture file
    fn build(&self, source: &str) -> Result<BuildResult, String>;
}
