//! The same shell, reached as `cargo progeny`.
//!
//! cargo execs a subcommand as `cargo-progeny progeny <arguments>`, so the only difference from the
//! standalone binary is the repeated subcommand name — which [`Invocation`] strips — and the usage
//! text, which has to name the invocation a reader will actually type.

use std::process::ExitCode;

use progeny_cli::Invocation;

fn main() -> ExitCode {
    progeny_cli::main(Invocation::CargoSubcommand, std::env::args())
}
