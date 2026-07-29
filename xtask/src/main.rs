//! Reproducible workflows for progeny.
//!
//! Everything that touches the filesystem or the network lives here rather than in the library:
//! the generator takes bytes and returns strings, which keeps generation deterministic and
//! trivially testable and leaves I/O to the callers that need it.

mod bench;
mod corpus;
mod differential;
mod example;
mod generated;
mod layers;
mod paths;
mod payloads;
mod probe;
mod runtime;
mod snapshot;

use clap::{Parser, Subcommand};
use color_eyre::eyre;

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
    /// Measure the runtime cost of the generated serde strategies.
    BenchRuntime(runtime::Args),
    /// Check that the pipeline's module graph only ever points leftward.
    LintLayers(layers::Args),
    /// Re-record every document's diagnostics snapshot.
    RegenSnapshots(corpus::Args),
    /// Assert the derive and the hand-written serde path agree, and count function bodies.
    Differential(differential::Args),
    /// Round-trip every example payload the corpus documents carry through the generated types.
    Payloads(payloads::Args),
    /// Generate one description's client and server and make them talk to each other.
    Example(example::Args),
    /// The same, generated for any document: every servable operation, driven over a socket.
    Probe(probe::Args),
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    match cli.command {
        Command::Corpus(args) => corpus::run(&args),
        Command::BenchCompile(args) => bench::run(&args),
        Command::BenchRuntime(args) => runtime::run(&args),
        Command::LintLayers(args) => layers::run(&args),
        Command::Differential(args) => differential::run(&args),
        Command::Payloads(args) => payloads::run(&args),
        Command::Example(args) => example::run(&args),
        Command::Probe(args) => probe::run(&args),
        // The same run as `corpus`, writing what it would otherwise compare against.
        Command::RegenSnapshots(mut args) => {
            args.write_snapshots = true;
            corpus::run(&args)
        }
    }
}
