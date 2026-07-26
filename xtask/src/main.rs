//! Reproducible workflows for progeny.
//!
//! Everything that touches the filesystem or the network lives here rather than in the library:
//! the generator takes bytes and returns strings, which keeps generation deterministic and
//! trivially testable and leaves I/O to the callers that need it.

mod bench;
mod corpus;
mod layers;
mod paths;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    about = "Reproducible workflows for progeny",
    max_term_width = 100
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the conformance corpus.
    Corpus(corpus::Args),
    /// Measure what a generated crate costs to compile.
    BenchCompile(bench::Args),
    /// Check that the pipeline's module graph only ever points leftward.
    LintLayers(layers::Args),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Corpus(args) => corpus::run(&args),
        Command::BenchCompile(args) => bench::run(&args),
        Command::LintLayers(args) => layers::run(&args),
    }
}
