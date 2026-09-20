use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "rust-refactor")]
#[command(about = "A semantic Rust refactoring CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Inline(InlineCommand),
    ToOop(ToOopCommand),
    ToOopStats(ToOopStatsCommand),
    RemoveFunction(RemoveFunctionCommand),
    SimplifyWrapper(SimplifyWrapperCommand),
}

#[derive(Debug, Args)]
pub struct SimplifyWrapperCommand {
    /// Rust source file containing the free function, relative to the workspace.
    #[arg(long)]
    pub file: Utf8PathBuf,
    /// One-based line of the function name.
    #[arg(long)]
    pub line: usize,
    /// One-based column of the function name.
    #[arg(long)]
    pub column: usize,
    /// Validate and print edits without changing source files.
    #[arg(long, conflicts_with = "write")]
    pub dry_run: bool,
    /// Apply edits and run cargo check; restore edited files on failure.
    #[arg(long)]
    pub write: bool,
    /// Scan source without rust-analyzer; requires a unique function name in the workspace.
    #[arg(long)]
    pub fast: bool,
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Output format for the result and diagnostics.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct RemoveFunctionCommand {
    /// Rust source file containing the free function, relative to the workspace.
    #[arg(long)]
    pub file: Utf8PathBuf,
    /// One-based line of the function name.
    #[arg(long)]
    pub line: usize,
    /// One-based column of the function name.
    #[arg(long)]
    pub column: usize,
    /// Validate and print edits without changing source files.
    #[arg(long, conflicts_with = "write")]
    pub dry_run: bool,
    /// Apply edits and run cargo check; restore edited files on failure.
    #[arg(long)]
    pub write: bool,
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Output format for the result and diagnostics.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct ToOopStatsCommand {
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Only report functions whose first parameter uses this struct.
    #[arg(long = "struct")]
    pub struct_name: Option<String>,
    /// Output format for the grouped table.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct ToOopCommand {
    /// Refactor every direct-receiver free function for this struct in one batch.
    #[arg(long = "struct", conflicts_with_all = ["file", "selection"])]
    pub struct_name: Option<String>,
    /// Rust source file containing selected functions, relative to the workspace.
    #[arg(long, requires_all = ["line", "column"], conflicts_with = "selection")]
    pub file: Option<Utf8PathBuf>,
    /// One-based line of a function name. Repeat to select functions in --file.
    #[arg(long, requires = "file")]
    pub line: Vec<usize>,
    /// One-based column shared by the --line selections.
    #[arg(long, requires = "file")]
    pub column: Option<usize>,
    /// Select a function as FILE:LINE:COLUMN. Repeat for multiple files or columns.
    #[arg(
        long = "selection",
        value_name = "FILE:LINE:COLUMN",
        conflicts_with = "file"
    )]
    pub selection: Vec<String>,
    /// Validate and print the full edit plan without changing source files.
    #[arg(long, conflicts_with = "write")]
    pub dry_run: bool,
    /// Apply the plan and format touched files; normal mode also runs cargo check.
    #[arg(long)]
    pub write: bool,
    /// Run cargo check after --fast --write (normal mode always checks).
    #[arg(long, requires = "write")]
    pub check: bool,
    /// Use syntax analysis and in-memory preview; --write skips cargo check unless --check.
    #[arg(long)]
    pub fast: bool,
    /// Analyze and check all Cargo features.
    #[arg(long)]
    pub all_features: bool,
    /// Analyze and check the selected compilation target.
    #[arg(long, requires = "write")]
    pub target: Option<String>,
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Output format for the result and diagnostics.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct InlineCommand {
    /// Show the planned refactor without writing files.
    #[arg(long, conflicts_with = "write")]
    pub dry_run: bool,

    /// Apply the planned refactor.
    #[arg(long)]
    pub write: bool,

    /// Run verification commands after applying edits.
    #[arg(long, requires = "write")]
    pub check: bool,

    /// Run cargo test after applying edits.
    #[arg(long, requires = "write")]
    pub test: bool,

    /// Pass --all-features to cargo check/test verification commands.
    #[arg(long, requires = "write")]
    pub all_features: bool,

    /// Pass --target to cargo check/test verification commands.
    #[arg(long, requires = "write")]
    pub target: Option<String>,

    /// Keep edited files if verification fails.
    #[arg(long, requires = "write")]
    pub keep_broken: bool,

    /// Cargo manifest path for the workspace to refactor.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
}
