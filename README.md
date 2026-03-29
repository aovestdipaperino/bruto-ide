# Bruto IDE

A pluggable TUI IDE framework built with [Turbo Vision for Rust](https://github.com/aovestdipaperino/turbo-vision-4-rust).

Bruto IDE provides the complete shell for a text-mode development environment: editor with breakpoint gutter, lldb-based debugger, variable watch panel, output panel, menus, and keyboard shortcuts. It knows nothing about any specific programming language. You plug in a language by implementing a single trait.

## The Language Trait

```rust
pub trait Language {
    fn name(&self) -> &str;
    fn file_extension(&self) -> &str;
    fn sample_program(&self) -> &str;
    fn create_highlighter(&self) -> Box<dyn SyntaxHighlighter>;
    fn build(&self, source: &str) -> Result<BuildResult, String>;
}
```

Implement these five methods and call `bruto_ide::ide::run(Box::new(YourLanguage))`. The IDE handles everything else.

## What the IDE Provides

- Syntax-highlighted editor (via turbo-vision's `Editor`)
- Single-column breakpoint gutter (click to toggle red square markers)
- lldb-based debugger: start/continue (F5), step over (F8), step into (F7), stop (Shift+F5)
- Execution line highlighting during debugging
- Variable watch panel showing local variables
- Output panel for build messages and program console output
- Menu bar (File, Build, Debug, Help) with keyboard shortcuts
- About dialog showing the language name

## Usage

Add `bruto-ide` as a dependency, implement `Language`, and create a binary:

```rust
fn main() -> turbo_vision::core::error::Result<()> {
    bruto_ide::ide::run(Box::new(my_lang::MyLanguage))
}
```

See [bruto-pascal-lang](https://github.com/aovestdipaperino/bruto-pascal-lang) for a complete implementation and [bruto-pascal](https://github.com/aovestdipaperino/bruto-pascal) for the binary that wires them together.

## License

MIT
