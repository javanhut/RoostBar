//! One bar per session. Two bars each reserve the top strip, and the windows
//! below end up pushed down by both: a gap between the bar and the panes.
//!
//! The bar is whoever holds `$XDG_RUNTIME_DIR/roostbar.lock`, for as long as
//! it runs; the kernel lets go of the lock when the process ends, however it
//! ends. A bar started while another holds it goes away again, unless it was
//! started with `--replace`, which asks the holder to quit and takes over:
//! that is how a freshly installed build replaces the one running.
//!
//! Only the bar that has the lock ever ends another bar, so two bars started
//! at the same moment cannot end each other and leave none. Once it has the
//! lock it ends any other bar of this user's it finds in `/proc` — a bar
//! from a build that predates the lock holds none, and would otherwise run
//! alongside. `roostbar vol …` and `roostbar bt …` are the same program
//! running a one-off command, not bars, and are left alone.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long a bar gets to quit after SIGTERM before it gets SIGKILL.
const GRACE: Duration = Duration::from_secs(2);

/// Become the only bar, or return `None` when this process should exit
/// because another bar is running and `replace` is false. The returned lock
/// must live as long as the bar does.
pub fn take_over(replace: bool) -> Option<Option<File>> {
    let Some(mut lock) = lock_file() else {
        // No runtime dir to lock in: the /proc sweep alone still leaves one.
        end_other_bars();
        return Some(None);
    };
    if lock.try_lock().is_err() {
        if !replace {
            return None;
        }
        let start = Instant::now();
        loop {
            if let Some(pid) = holder(&mut lock) {
                signal(
                    &[pid],
                    if start.elapsed() < GRACE {
                        "TERM"
                    } else {
                        "KILL"
                    },
                );
            }
            std::thread::sleep(Duration::from_millis(100));
            if lock.try_lock().is_ok() {
                break;
            }
            if start.elapsed() > GRACE * 3 {
                eprintln!("roostbar: the running bar would not quit; leaving it");
                return None;
            }
        }
    }
    end_other_bars();
    let _ = lock.set_len(0);
    let _ = lock.rewind();
    let _ = writeln!(lock, "{}", std::process::id());
    Some(Some(lock))
}

fn lock_file() -> Option<File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("roostbar.lock"))
        .ok()
}

/// The pid the running bar wrote into the lock file.
fn holder(lock: &mut File) -> Option<u32> {
    let mut text = String::new();
    lock.rewind().ok()?;
    lock.read_to_string(&mut text).ok()?;
    text.trim()
        .parse()
        .ok()
        .filter(|&pid| pid != std::process::id())
}

/// Ask every other bar of this user's to quit, and make sure it has.
fn end_other_bars() {
    let others = other_bars();
    if others.is_empty() {
        return;
    }
    signal(&others, "TERM");
    let start = Instant::now();
    while start.elapsed() < GRACE {
        if others.iter().all(|pid| !alive(*pid)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let left: Vec<u32> = others.into_iter().filter(|pid| alive(*pid)).collect();
    if !left.is_empty() {
        signal(&left, "KILL");
    }
}

fn other_bars() -> Vec<u32> {
    let me = std::process::id();
    let Ok(uid) = std::fs::metadata("/proc/self").map(|m| m.uid()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != me)
        .filter(|&pid| {
            std::fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == uid)
                && std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| is_bar(&c))
        })
        .collect()
}

/// Whether a `/proc/*/cmdline` is a bar: roostbar with no command, only
/// flags such as `--replace`.
fn is_bar(cmdline: &[u8]) -> bool {
    let mut args = cmdline.split(|&b| b == 0).filter(|a| !a.is_empty());
    let Some(exe) = args.next() else {
        return false;
    };
    let name = exe.rsplit(|&b| b == b'/').next().unwrap_or(exe);
    name == b"roostbar" && args.all(|a| a.starts_with(b"-"))
}

fn alive(pid: u32) -> bool {
    // A zombie keeps its /proc entry but no longer holds a surface.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z")
    })
}

fn signal(pids: &[u32], sig: &str) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .args(pids.iter().map(u32::to_string))
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bar_is_a_bar() {
        assert!(is_bar(b"/home/u/.local/bin/roostbar\0"));
        assert!(is_bar(b"roostbar\0--replace\0"));
        assert!(!is_bar(b"roostbar\0vol\0up\0"));
        assert!(!is_bar(b"/usr/bin/roostbar\0bt\0list\0"));
        assert!(!is_bar(b"/usr/bin/vim\0roostbar\0"));
        assert!(!is_bar(b""));
    }

    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id()));
    }
}
