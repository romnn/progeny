//! The standalone binary.

use std::process::ExitCode;

use progeny_cli::Invocation;

fn main() -> ExitCode {
    progeny_cli::main(Invocation::Standalone, std::env::args())
}
