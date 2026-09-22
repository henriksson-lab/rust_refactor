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
    ConstantsToEnum(ConstantsToEnumCommand),
    ConstantsToEnumCsv(ConstantsToEnumCsvCommand),
    ConstantsToEnumStats(ConstantsToEnumStatsCommand),
    EnumHoist(EnumHoistCommand),
    EnumHoistStats(EnumHoistStatsCommand),
    Inline(InlineCommand),
    OutParamStats(OutParamStatsCommand),
    ToOop(ToOopCommand),
    ToOopStats(ToOopStatsCommand),
    RemoveFunction(RemoveFunctionCommand),
    ReturnStats(ReturnStatsCommand),
    SimplifyWrapper(SimplifyWrapperCommand),
}

#[derive(Debug, Args)]
pub struct OutParamStatsCommand {
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Output format for the output-parameter inventory.
    #[arg(long, value_enum, default_value_t = ReturnStatsFormat::Text)]
    pub format: ReturnStatsFormat,
}

#[derive(Debug, Args)]
pub struct ReturnStatsCommand {
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Only emit functions for which an Option or Result rewrite is suggested.
    #[arg(long)]
    pub candidates_only: bool,
    /// Output format for the return-value inventory.
    #[arg(long, value_enum, default_value_t = ReturnStatsFormat::Text)]
    pub format: ReturnStatsFormat,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ReturnStatsFormat {
    Text,
    Json,
    Csv,
}

#[derive(Debug, Args)]
pub struct EnumHoistStatsCommand {
    /// File containing the enum introduced by constants-to-enum.
    #[arg(long)]
    pub enum_file: Utf8PathBuf,
    /// Enum whose repeated from_raw conversions should be reported.
    #[arg(long)]
    pub enum_name: String,
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Analyze all Cargo features.
    #[arg(long)]
    pub all_features: bool,
    /// Analyze one compilation target.
    #[arg(long)]
    pub target: Option<String>,
    /// Output format for the candidate report.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct EnumHoistCommand {
    /// File containing the enum introduced by constants-to-enum.
    #[arg(long)]
    pub enum_file: Utf8PathBuf,
    /// Name of the enum to propagate through typed value flow.
    #[arg(long)]
    pub enum_name: String,
    /// Enum path emitted at rewritten sites; defaults to the enum name.
    #[arg(long)]
    pub enum_path: Option<String>,
    /// Seed a parameter as FILE:LINE:COLUMN. Repeat to combine value flows.
    #[arg(long = "parameter", value_name = "FILE:LINE:COLUMN")]
    pub parameters: Vec<String>,
    /// Seed a named struct field as FILE:LINE:COLUMN. Repeat as needed.
    #[arg(long = "field", value_name = "FILE:LINE:COLUMN")]
    pub fields: Vec<String>,
    /// Seed a simple local binding as FILE:LINE:COLUMN. Repeat as needed.
    #[arg(long = "local", value_name = "FILE:LINE:COLUMN")]
    pub locals: Vec<String>,
    /// Seed a function return at its function name as FILE:LINE:COLUMN.
    #[arg(long = "return", value_name = "FILE:LINE:COLUMN")]
    pub returns: Vec<String>,
    /// Replace compatibility constants with Enum::Variant.to_raw() and remove their declarations.
    #[arg(long)]
    pub remove_aliases: bool,
    /// Also remove aliases for FILE=ENUM. Repeat to batch cleanup in one workspace load.
    #[arg(long = "remove-aliases-from", value_name = "FILE=ENUM")]
    pub remove_aliases_from: Vec<String>,
    /// Validate and print the complete plan without changing files.
    #[arg(long, conflicts_with = "write")]
    pub dry_run: bool,
    /// Apply atomically, format touched files, and run cargo check.
    #[arg(long)]
    pub write: bool,
    /// Analyze and check all Cargo features.
    #[arg(long)]
    pub all_features: bool,
    /// Analyze and check one compilation target.
    #[arg(long)]
    pub target: Option<String>,
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Output format for the plan and diagnostics.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct ConstantsToEnumCsvCommand {
    /// Edited CSV produced by constants-to-enum-stats.
    #[arg(long)]
    pub table: Utf8PathBuf,
    /// Value from the proposed_enum_group column to apply.
    #[arg(long)]
    pub group: String,
    /// Enum path emitted at cross-module match sites.
    #[arg(long)]
    pub enum_path: Option<String>,
    /// Override enum visibility; otherwise all constants must agree.
    #[arg(long, value_enum)]
    pub visibility: Option<EnumVisibility>,
    /// Validate and print edits without changing files.
    #[arg(long, conflicts_with = "write")]
    pub dry_run: bool,
    /// Apply edits, format touched files, and run cargo check.
    #[arg(long)]
    pub write: bool,
    /// Analyze and check all Cargo features.
    #[arg(long)]
    pub all_features: bool,
    /// Analyze and check one compilation target.
    #[arg(long)]
    pub target: Option<String>,
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Output format for the plan and diagnostics.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct ConstantsToEnumStatsCommand {
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Hide groups with fewer than this many distinct constants.
    #[arg(long, default_value_t = 2)]
    pub min_constants: usize,
    /// Output format for the grouped report.
    #[arg(long, value_enum, default_value_t = ConstantsStatsFormat::Text)]
    pub format: ConstantsStatsFormat,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ConstantsStatsFormat {
    Text,
    Json,
    Csv,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum EnumVisibility {
    Private,
    PubCrate,
    Pub,
}

#[derive(Debug, Args)]
pub struct ConstantsToEnumCommand {
    /// File containing the constants and receiving the enum.
    #[arg(long)]
    pub file: Utf8PathBuf,
    /// Name of the generated enum.
    #[arg(long)]
    pub enum_name: String,
    /// Select a constant and variant as CONSTANT=Variant. Repeat for the family.
    #[arg(long = "constant", value_name = "CONSTANT=VARIANT", required = true)]
    pub constants: Vec<String>,
    /// Select a match as FILE:LINE:COLUMN. Repeat to convert several matches.
    #[arg(long = "match", value_name = "FILE:LINE:COLUMN")]
    pub matches: Vec<String>,
    /// Select an == or != comparison as FILE:LINE:COLUMN. Repeat as needed.
    #[arg(long = "comparison", value_name = "FILE:LINE:COLUMN")]
    pub comparisons: Vec<String>,
    /// Enum path emitted at match sites; defaults to the enum name.
    #[arg(long)]
    pub enum_path: Option<String>,
    /// Override enum visibility; otherwise all constants must agree.
    #[arg(long, value_enum)]
    pub visibility: Option<EnumVisibility>,
    /// Validate and print edits without changing files.
    #[arg(long, conflicts_with = "write")]
    pub dry_run: bool,
    /// Apply edits, format touched files, and run cargo check.
    #[arg(long)]
    pub write: bool,
    /// Analyze and check all Cargo features.
    #[arg(long)]
    pub all_features: bool,
    /// Analyze and check one compilation target.
    #[arg(long)]
    pub target: Option<String>,
    /// Cargo manifest path for the workspace.
    #[arg(long)]
    pub manifest_path: Option<Utf8PathBuf>,
    /// Output format for the plan and diagnostics.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
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
