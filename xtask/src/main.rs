//! xtask-style command-line tool for building this project.

#![allow(
    clippy::disallowed_methods,
    reason = "the build tool operates on the host by definition; it is not shell code"
)]

mod analyze;
#[cfg(unix)]
mod bash_tests;
mod check;
mod codemod;
mod vendor;
mod ci;
mod common;
mod generate;
mod test;

use anyhow::Result;
use clap::Parser;

/// Global options shared across all commands.
#[derive(Parser, Debug, Clone, Copy)]
pub struct GlobalArgs {
    /// Enable verbose output.
    #[clap(long, short = 'v', global = true)]
    pub verbose: bool,
}

#[derive(Parser)]
#[clap(name = "xtask", about = "Build automation tasks for brush")]
struct CommandLineArgs {
    #[clap(flatten)]
    global: GlobalArgs,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Parser)]
enum Command {
    /// Run analysis tasks (benchmarks, public API diffing).
    #[clap(subcommand)]
    Analyze(analyze::AnalyzeCommand),
    /// Run code quality checks.
    #[clap(subcommand)]
    Check(check::CheckCommand),
    /// Route a forked utility's filesystem access through the vfs (D4).
    Codemod(codemod::CodemodCommand),
    /// Vendor a uutils crate and route it through the vfs (D4).
    VendorFork(vendor::VendorCommand),
    /// Run CI workflows.
    #[clap(subcommand)]
    Ci(ci::CiCommand),
    /// Generate documentation, completions, and schemas.
    #[clap(subcommand)]
    Gen(generate::GenCommand),
    /// Run tests.
    Test(Box<test::TestCommand>),
}

fn main() -> Result<()> {
    let args = CommandLineArgs::parse();
    let verbose = args.global.verbose;

    match &args.command {
        Command::Analyze(cmd) => analyze::run(cmd, verbose),
        Command::Gen(cmd) => generate::run(cmd, verbose),
        Command::Check(cmd) => check::run(cmd, verbose),
        Command::Codemod(cmd) => codemod::run(cmd, verbose),
        Command::VendorFork(cmd) => vendor::run(cmd, verbose),
        Command::Test(cmd) => test::run(cmd, verbose),
        Command::Ci(cmd) => ci::run(cmd, verbose),
    }
}
