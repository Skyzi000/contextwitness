use std::io;

use crate::select::{Process, Table};

const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
const SYSTEM_PROCESS_INFORMATION: u32 = 5;
const SYSTEM_BASIC_PROCESS_INFORMATION: u32 = 252;
const PROCESS_SEQUENCE_NUMBER: u32 = 92;
const CURRENT_PROCESS: isize = -1;

const _: () = assert!(size_of::<usize>() == 8);

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(class: u32, info: *mut u64, len: u32, returned: *mut u32) -> i32;
    fn NtQueryInformationProcess(
        process: isize,
        class: u32,
        info: *mut u64,
        len: u32,
        returned: *mut u32,
    ) -> i32;
}

#[derive(Clone, Copy)]
struct Layout {
    class: u32,
    pid: usize,
    parent: usize,
    created: usize,
    name: usize,
}

/// Windows has this class from build 26100.4770.
const BASIC: Layout = Layout {
    class: SYSTEM_BASIC_PROCESS_INFORMATION,
    pid: 8,
    parent: 16,
    created: 24,
    name: 32,
};

/// `created` is the creation time, which lies in a field the documentation marks reserved.
const FULL: Layout = Layout {
    class: SYSTEM_PROCESS_INFORMATION,
    pid: 80,
    parent: 88,
    created: 32,
    name: 56,
};

pub(crate) struct Processes {
    layout: Layout,
    buffer: Vec<u64>,
}

impl Processes {
    /// Settles on one list and stays with it: the two count `created` differently.
    pub(crate) fn new() -> io::Result<Self> {
        let mut full = Self::with(FULL);
        let by_full = full.table()?;
        if !describes_this_process(&by_full) {
            return Err(invalid("it does not describe this process"));
        }
        let mut basic = Self::with(BASIC);
        if basic
            .table()
            .is_ok_and(|by_basic| confirmed(&by_basic, &by_full))
        {
            return Ok(basic);
        }
        tracing::info!("the lighter process list is not usable here; reading the full one");
        Ok(full)
    }

    fn with(layout: Layout) -> Self {
        Self {
            layout,
            buffer: vec![0; 1 << 14],
        }
    }

    pub(crate) fn table(&mut self) -> io::Result<Table> {
        let filled = loop {
            let mut needed = 0u32;
            let status = unsafe {
                NtQuerySystemInformation(
                    self.layout.class,
                    self.buffer.as_mut_ptr(),
                    (self.buffer.len() * 8) as u32,
                    &mut needed,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH {
                self.buffer.resize(needed as usize / 8 + (1 << 12), 0);
            } else if status < 0 {
                return Err(io::Error::other(format!(
                    "NtQuerySystemInformation({}) failed with status {status:#010x}",
                    self.layout.class
                )));
            } else {
                break (needed as usize).min(self.buffer.len() * 8);
            }
        };

        let layout = self.layout;
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast(), filled) };
        let read = |at: usize| -> io::Result<u64> {
            bytes
                .get(at..at + 8)
                .and_then(|field| field.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(|| invalid("an entry reaches past its end"))
        };

        let mut table = Table::new();
        let mut entry = 0usize;
        loop {
            let name_bytes = (read(entry + layout.name)? & 0xFFFF) as usize;
            let name_at = (read(entry + layout.name + 8)? as usize)
                .wrapping_sub(self.buffer.as_ptr() as usize);
            let exe = if name_bytes == 0 {
                String::new()
            } else {
                let name = bytes
                    .get(name_at..name_at.wrapping_add(name_bytes))
                    .ok_or_else(|| invalid("a name lies outside it"))?;
                let units: Vec<u16> = name
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|unit| u16::from_le_bytes(*unit))
                    .collect();
                String::from_utf16_lossy(&units)
            };
            table.insert(
                read(entry + layout.pid)? as u32,
                Process {
                    parent: read(entry + layout.parent)? as u32,
                    created: read(entry + layout.created)?,
                    exe,
                },
            );
            let next = (read(entry)? & 0xFFFF_FFFF) as usize;
            if next == 0 {
                return Ok(table);
            }
            entry += next;
        }
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::other(format!("the system's process list is unusable: {what}"))
}

fn confirmed(basic: &Table, full: &Table) -> bool {
    let own_pid = std::process::id();
    let (Some(own), Some(by_full)) = (basic.get(&own_pid), full.get(&own_pid)) else {
        return false;
    };
    let mut sequence = 0u64;
    let status = unsafe {
        NtQueryInformationProcess(
            CURRENT_PROCESS,
            PROCESS_SEQUENCE_NUMBER,
            &mut sequence,
            size_of::<u64>() as u32,
            std::ptr::null_mut(),
        )
    };
    describes_this_process(basic)
        && own.parent == by_full.parent
        && status >= 0
        && own.created == sequence
}

/// Microsoft documents these structures as subject to change between releases of Windows, so a
/// layout is trusted only once it has shown this process under its own name.
fn describes_this_process(table: &Table) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    table
        .get(&std::process::id())
        .is_some_and(|own| own.exe.eq_ignore_ascii_case(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(layout: Layout) -> Processes {
        Processes::with(layout)
    }

    fn basic_is_confirmed() -> bool {
        let full = with(FULL).table().expect("the full list reads");
        match with(BASIC).table() {
            Ok(basic) if confirmed(&basic, &full) => true,
            Ok(_) => {
                println!("the basic list is answered here but not confirmed");
                false
            }
            Err(error) => {
                println!("the basic list is not answered here: {error}");
                false
            }
        }
    }

    #[test]
    fn the_full_list_describes_this_process() {
        let table = with(FULL).table().expect("the full list reads");
        assert!(describes_this_process(&table));
    }

    #[test]
    fn the_basic_list_is_chosen_exactly_when_it_is_confirmed() {
        let confirmed = basic_is_confirmed();
        let mut chosen = Processes::new().expect("a process list is readable");
        assert_eq!(
            chosen.layout.class,
            if confirmed { BASIC.class } else { FULL.class }
        );
        let table = chosen.table().expect("the chosen list reads again");
        assert!(describes_this_process(&table));
        println!(
            "checked: class {} is chosen and describes this process",
            chosen.layout.class
        );
    }

    #[test]
    fn both_lists_name_and_parent_the_same_processes_alike() {
        if !basic_is_confirmed() {
            println!("skipped: the two lists are compared only where the basic one is used");
            return;
        }
        let before = with(BASIC).table().expect("the basic list reads");
        let full = with(FULL).table().expect("the full list reads");
        let after = with(BASIC)
            .table()
            .expect("the basic list reads a second time");

        let mut compared = 0;
        for (pid, by_basic) in &after {
            let unchanged = before.get(pid).is_some_and(|earlier| {
                (earlier.created, earlier.parent, &earlier.exe)
                    == (by_basic.created, by_basic.parent, &by_basic.exe)
            });
            if !unchanged {
                continue;
            }
            let Some(by_full) = full.get(pid) else {
                println!("pid {pid} is in the basic list only");
                continue;
            };
            assert_eq!(
                (by_full.parent, &by_full.exe),
                (by_basic.parent, &by_basic.exe),
                "pid {pid}"
            );
            compared += 1;
        }
        assert!(
            compared > 0,
            "this process at least is in all three readings"
        );
        println!("checked: {compared} processes are named and parented alike in both lists");
    }

    #[test]
    fn a_process_whose_parent_has_exited_is_confirmed_all_the_same() {
        if !basic_is_confirmed() {
            println!("skipped: nothing is confirmed here with or without the parent");
            return;
        }
        let full = with(FULL).table().expect("the full list reads");
        let mut basic = with(BASIC).table().expect("the basic list reads");
        let parent = basic[&std::process::id()].parent;
        basic.remove(&parent);
        assert!(confirmed(&basic, &full));
        println!("checked: confirmed without the parent's entry");
    }

    #[test]
    fn a_creation_time_earlier_than_the_system_process_does_not_refuse_the_full_list() {
        let mut full = with(FULL).table().expect("the full list reads");
        for process in full.values_mut() {
            process.created = u64::MAX - process.created;
        }
        assert!(
            describes_this_process(&full),
            "a clock stepped back after boot leaves this process older than its elders"
        );
    }

    #[test]
    fn the_basic_list_read_with_its_fields_crossed_is_not_confirmed() {
        println!(
            "the basic list read straight is confirmed here: {}",
            basic_is_confirmed()
        );
        let full = with(FULL).table().expect("the full list reads");
        let offsets = [0, BASIC.pid, BASIC.parent, BASIC.created, BASIC.name, 40];
        let mut refused = 0;
        for pid in offsets {
            for parent in offsets {
                for created in offsets {
                    if (pid, parent, created) == (BASIC.pid, BASIC.parent, BASIC.created) {
                        continue;
                    }
                    let mut crossed = with(Layout {
                        pid,
                        parent,
                        created,
                        ..BASIC
                    });
                    assert!(
                        !crossed.table().is_ok_and(|table| confirmed(&table, &full)),
                        "pid at {pid}, parent at {parent}, created at {created}"
                    );
                    refused += 1;
                }
            }
        }
        assert_eq!(refused, 6 * 6 * 6 - 1);
    }

    #[test]
    fn a_list_read_in_the_wrong_layout_is_an_error_or_does_not_describe_this_process() {
        let mut crossed = with(Layout {
            class: FULL.class,
            ..BASIC
        });
        assert!(
            !crossed
                .table()
                .is_ok_and(|table| describes_this_process(&table))
        );
    }
}
