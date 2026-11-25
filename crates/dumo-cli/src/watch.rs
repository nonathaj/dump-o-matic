//! The `watch` command: rip every disc that goes into a drive, one after another.
//!
//! For working through a box set: put a disc in, and it is ripped and the tray opened
//! for the next, until the command is stopped. It only rips. Identifying and filing
//! stay separate, because a video disc is never identified well enough to file
//! unattended, and the rips wait in staging for that like any other.
//!
//! A disc is ripped once per insertion. One whose rip failed is left in the drive, and
//! is not tried again until it is taken out and put back, so a bad disc cannot loop.

use crate::rip::{self, RipArgs};
use anyhow::{Context, Result};
use dumo_core::config::Config;
use dumo_core::job::{Job, JobStage};
use dumo_core::TrayState;
use std::path::PathBuf;
use std::time::Duration;

/// How often the tray is asked about.
const POLL: Duration = Duration::from_secs(2);

/// Consecutive polls a disc must be ready for before it is ripped. A disc going in
/// reports ready while the drive is still settling it, and the probe that starts a rip
/// would meet a half-read table of contents.
const SETTLE_POLLS: u32 = 3;

pub struct WatchArgs {
    pub device: Option<String>,
    pub min_length: u32,
    pub keep_play_all: bool,
    pub config_file: Option<PathBuf>,
}

pub fn run(args: WatchArgs) -> Result<()> {
    let device = match args.device.clone() {
        Some(d) => d,
        None => dumo_drives::enumerate_drives()
            .context("enumerating drives")?
            .first()
            .map(|d| d.path.clone())
            .context("no optical drives found")?,
    };
    // Fail now, not at the first disc, if there is nowhere to rip to.
    let cfg = Config::load(args.config_file.as_deref())?;

    println!("Watching {device} for discs. Each is ripped, and the tray opened when it is done.");
    println!("Identify and file the rips as usual. Ctrl-C to stop.");

    let mut watcher = Watcher::default();
    let mut unreadable = false;
    let mut ripped = 0u32;
    loop {
        let tray = match dumo_drives::tray_state(&device) {
            Ok(t) => {
                unreadable = false;
                t
            }
            Err(e) => {
                // Say so once, not every two seconds; a drive that comes back is used.
                if !unreadable {
                    eprintln!("[{}] Cannot ask {device} about its tray: {e}", clock());
                    unreadable = true;
                }
                TrayState::NotReady
            }
        };

        if watcher.see(tray) == Next::Rip {
            println!();
            println!("[{}] Disc in {device}; ripping it.", clock());
            let outcome = rip::run(RipArgs {
                device: Some(device.clone()),
                min_length: args.min_length,
                assume_yes: true,
                keep_play_all: args.keep_play_all,
                eject: true,
                config_file: args.config_file.clone(),
                ..RipArgs::default()
            });
            match outcome {
                Ok(Some(id)) if ripped_cleanly(&cfg, &id) => {
                    ripped += 1;
                    println!("[{}] Ripped {id} ({ripped} this session).", clock());
                }
                Ok(Some(id)) => println!(
                    "[{}] {id} did not rip cleanly; the disc is left in. \
                     Take it out and put it back to try again.",
                    clock()
                ),
                Ok(None) => println!(
                    "[{}] Nothing was ripped; the disc is left in. \
                     Take it out and put it back to try again.",
                    clock()
                ),
                Err(e) => eprintln!(
                    "[{}] The rip failed: {e:#}. The disc is left in; \
                     take it out and put it back to try again.",
                    clock()
                ),
            }
            println!("[{}] Waiting for the next disc in {device} ...", clock());
        }
        std::thread::sleep(POLL);
    }
}

fn ripped_cleanly(cfg: &Config, id: &str) -> bool {
    Job::load(&cfg.staging.job_dir(id))
        .map(|j| j.stage == JobStage::Ripped)
        .unwrap_or(false)
}

/// The local time of day, for telling one disc's lines from the next in a long session.
fn clock() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return "--:--:--".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[derive(Debug, PartialEq, Eq)]
enum Next {
    Wait,
    Rip,
}

/// Decides, from what the tray says poll by poll, when a disc should be ripped.
#[derive(Default)]
struct Watcher {
    /// Consecutive polls the disc has been ready for.
    ready: u32,
    /// The disc in the drive has had its rip, well or badly. Cleared only when the
    /// drive is opened or emptied, so each insertion is ripped once.
    done: bool,
}

impl Watcher {
    fn see(&mut self, tray: TrayState) -> Next {
        match tray {
            TrayState::DiscOk | TrayState::DiscUnreadable => {
                self.ready += 1;
                if !self.done && self.ready >= SETTLE_POLLS {
                    self.done = true;
                    return Next::Rip;
                }
            }
            // Spinning up, or spinning down after a rip: the same disc, still in.
            TrayState::NotReady => self.ready = 0,
            TrayState::NoDisc | TrayState::TrayOpen => {
                self.ready = 0;
                self.done = false;
            }
        }
        Next::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TrayState::*;

    fn rips(w: &mut Watcher, polls: &[TrayState]) -> usize {
        polls.iter().filter(|t| w.see(**t) == Next::Rip).count()
    }

    #[test]
    fn a_disc_is_ripped_once_it_has_settled() {
        let mut w = Watcher::default();
        assert_eq!(rips(&mut w, &[TrayOpen, NotReady, NotReady, DiscOk, DiscOk]), 0);
        assert_eq!(w.see(DiscOk), Next::Rip);
    }

    #[test]
    fn a_disc_is_not_ripped_twice_while_it_stays_in() {
        let mut w = Watcher::default();
        assert_eq!(rips(&mut w, &[DiscOk; 3]), 1);
        // A failed rip leaves it in; the drive spins it down and up again.
        assert_eq!(rips(&mut w, &[DiscOk, NotReady, NotReady, DiscOk, DiscOk, DiscOk, DiscOk]), 0);
    }

    #[test]
    fn the_next_disc_is_ripped_after_the_tray_opens() {
        let mut w = Watcher::default();
        let session = [DiscOk, DiscOk, DiscOk, TrayOpen, TrayOpen, NotReady, DiscOk, DiscOk, DiscOk];
        assert_eq!(rips(&mut w, &session), 2);
    }

    #[test]
    fn a_disc_that_flickers_ready_is_not_ripped_until_it_holds() {
        let mut w = Watcher::default();
        assert_eq!(rips(&mut w, &[DiscOk, DiscOk, NotReady, DiscOk, DiscOk, NotReady]), 0);
    }

    #[test]
    fn a_disc_already_in_when_the_watch_starts_is_ripped() {
        let mut w = Watcher::default();
        assert_eq!(rips(&mut w, &[DiscOk; 5]), 1);
    }
}
