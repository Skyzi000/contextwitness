#![deny(unsafe_op_in_unsafe_fn)]
#![windows_subsystem = "windows"]
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
    // Attaching resets every standard handle to the console, silently stealing `status > file`
    // away from the file, so whatever was inherited as a redirection is put back afterwards.
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    unsafe {
        let inherited = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|id| (id, GetStdHandle(id)));
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            for (id, handle) in inherited {
                if let Ok(handle) = handle
                    && !handle.is_invalid()
                {
                    let _ = SetStdHandle(id, handle);
                }
            }
        }
    }
    cli::main();
}
