use anyhow::Result;
use rust_refactor::cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse_args();

    match cli.command {
        Command::Inline(command) => rust_refactor::refactors::inline_function::run(command),
        Command::ToOop(command) => {
            let code = rust_refactor::refactors::to_oop::run(command)?;
            std::process::exit(code);
        }
        Command::ToOopStats(command) => rust_refactor::refactors::oop_stats::run(command),
        Command::RemoveFunction(command) => {
            let code = rust_refactor::refactors::remove_function::run(command)?;
            std::process::exit(code);
        }
        Command::SimplifyWrapper(command) => {
            let code = rust_refactor::refactors::simplify_wrapper::run(command)?;
            std::process::exit(code);
        }
    }
}
