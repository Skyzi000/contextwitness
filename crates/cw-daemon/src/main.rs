#![deny(unsafe_op_in_unsafe_fn)]
//! The ContextWitness daemon entry point.
//!
//! A console binary, so shells wait for the interactive commands and share their console with
//! them. The manifest's detached console allocation policy (Windows 11 24H2+) keeps the logon
//! autostart windowless.

mod autostart;
mod capture;
mod cli;
mod delivery;
mod episodes;
mod logging;
mod maintenance;
mod tray;

fn main() {
    cli::main();
}
