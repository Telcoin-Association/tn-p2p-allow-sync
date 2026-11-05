//! Entry point of `tn-p2p-allow-sync`; [`tn_p2p_allow_sync::cli::run`] does the work.

use std::process::ExitCode;

use tn_p2p_allow_sync::cli;

fn main() -> ExitCode {
    ExitCode::from(cli::run(std::env::args_os()).code())
}
