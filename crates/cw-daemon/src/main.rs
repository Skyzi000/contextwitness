#![deny(unsafe_op_in_unsafe_fn)]
//! The ContextWitness daemon entry point.

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
