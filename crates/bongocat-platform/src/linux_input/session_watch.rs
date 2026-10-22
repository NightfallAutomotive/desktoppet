//! Session changes from systemd-logind that must reset the input state.
//!
//! A key held when the machine sleeps, when the screen locks, or when the user
//! switches to another VT or account is released somewhere this process never
//! sees, so its release edge never arrives (issue #47). logind announces each
//! of those moments: `Manager.PrepareForSleep` before sleep and after resume,
//! `Session.Lock`/`Unlock`, and the session's `Active` property for VT and
//! user switches. Each one becomes a reset on the input worker.
//!
//! The caller's session is found through `/org/freedesktop/login1/session/auto`,
//! which logind resolves from the bus connection's credentials; looking it up by
//! PID would fail inside a Flatpak, whose PID namespace differs from the host's.
//! The alias only answers method calls: signals and property changes are
//! emitted from the session's own path, so its `Id` is read through the alias
//! and the real path comes from `Manager.GetSession`. A system without logind keeps working without these
//! resets; the periodic reconciliation still clears what a release missed.

use bongocat_input::InputResetReason;
use futures_lite::{FutureExt, StreamExt};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_INTERFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_PATH: &str = "/org/freedesktop/login1/session/auto";
const SESSION_INTERFACE: &str = "org.freedesktop.login1.Session";
/// How often the watcher checks for a stop request between signals.
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// One logind notification, already reduced to the reset it requires.
fn reset_for(signal: &str) -> Option<InputResetReason> {
    match signal {
        "PrepareForSleep" => Some(InputResetReason::Sleep),
        "Lock" | "Unlock" | "Inactive" | "Active" => Some(InputResetReason::SessionLock),
        _ => None,
    }
}

pub(crate) struct SessionWatch {
    stop: Arc<AtomicBool>,
    resets: Receiver<InputResetReason>,
    worker: Option<JoinHandle<()>>,
}

impl SessionWatch {
    /// Start watching. The watcher thread gives up quietly when there is no
    /// system bus or no logind, which leaves [`Self::take_resets`] empty.
    pub(crate) fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, resets) = mpsc::channel();
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("bongocat-logind-watch".into())
            .spawn(move || {
                let _ = async_io::block_on(watch(&sender, &worker_stop));
            })
            .ok();
        Self {
            stop,
            resets,
            worker,
        }
    }

    /// Every reset logind asked for since the last call.
    pub(crate) fn take_resets(&self) -> Vec<InputResetReason> {
        self.resets.try_iter().collect()
    }
}

impl Drop for SessionWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum Notice {
    Signal(&'static str),
    Active(bool),
    Tick,
    Ended,
}

async fn watch(sender: &Sender<InputResetReason>, stop: &AtomicBool) -> zbus::Result<()> {
    let connection = zbus::Connection::system().await?;
    let manager = zbus::Proxy::new(&connection, LOGIND, MANAGER_PATH, MANAGER_INTERFACE).await?;
    let alias = zbus::Proxy::new(&connection, LOGIND, SESSION_PATH, SESSION_INTERFACE).await?;
    let id: String = alias.get_property("Id").await?;
    let path: zbus::zvariant::OwnedObjectPath = manager.call("GetSession", &(id,)).await?;
    let session = zbus::Proxy::new(&connection, LOGIND, path, SESSION_INTERFACE).await?;
    let mut sleep = manager.receive_signal("PrepareForSleep").await?;
    let mut lock = session.receive_signal("Lock").await?;
    let mut unlock = session.receive_signal("Unlock").await?;
    let mut active = session.receive_property_changed::<bool>("Active").await;
    // The property stream yields the current value first; only a change from
    // a known value is a session switch.
    let mut last_active: Option<bool> = None;
    while !stop.load(Ordering::Acquire) {
        let notice = async {
            sleep
                .next()
                .await
                .map_or(Notice::Ended, |_| Notice::Signal("PrepareForSleep"))
        }
        .or(async {
            lock.next()
                .await
                .map_or(Notice::Ended, |_| Notice::Signal("Lock"))
        })
        .or(async {
            unlock
                .next()
                .await
                .map_or(Notice::Ended, |_| Notice::Signal("Unlock"))
        })
        .or(async {
            match active.next().await {
                Some(change) => change.get().await.map_or(Notice::Tick, Notice::Active),
                None => Notice::Ended,
            }
        })
        .or(async {
            async_io::Timer::after(STOP_CHECK_INTERVAL).await;
            Notice::Tick
        })
        .await;
        let signal = match notice {
            Notice::Signal(signal) => signal,
            Notice::Active(now) => {
                let previous = last_active.replace(now);
                match previous {
                    Some(previous) if previous != now => {
                        if now {
                            "Active"
                        } else {
                            "Inactive"
                        }
                    }
                    _ => continue,
                }
            }
            Notice::Tick => continue,
            Notice::Ended => return Ok(()),
        };
        if let Some(reason) = reset_for(signal)
            && sender.send(reason).is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_session_change_resets_and_sleep_is_its_own_reason() {
        assert_eq!(reset_for("PrepareForSleep"), Some(InputResetReason::Sleep));
        for signal in ["Lock", "Unlock", "Inactive", "Active"] {
            assert_eq!(reset_for(signal), Some(InputResetReason::SessionLock));
        }
        assert_eq!(reset_for("SomethingElse"), None);
    }

    /// On a machine with logind the watcher subscribes and keeps listening;
    /// a subscription error would end its thread at once.
    #[test]
    #[ignore = "needs a running systemd-logind and a login session"]
    fn the_watch_subscribes_to_logind() {
        let watch = SessionWatch::start();
        thread::sleep(Duration::from_millis(500));
        assert!(
            watch
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished()),
            "the logind watcher ended instead of listening"
        );
        assert!(watch.take_resets().is_empty(), "nothing happened yet");
    }

    #[test]
    fn the_watch_stops_promptly_with_or_without_logind() {
        let watch = SessionWatch::start();
        let started = std::time::Instant::now();
        drop(watch);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stopping took {:?}",
            started.elapsed()
        );
    }
}
