//! Keyboards and mice, each read on a task of its own and grabbed while input goes to the iPad.

use std::collections::HashSet;
use std::path::PathBuf;

use evdev::{Device, InputEvent, KeyCode, RelativeAxisCode};
use tokio::sync::{mpsc, watch};

use crate::log;

pub enum Event {
    Input(InputEvent),
    Gone(PathBuf),
}

fn wanted(d: &Device) -> bool {
    if d.name().is_some_and(|n| n.contains("padbridge")) {
        return false;
    }
    let mouse = d.supported_relative_axes().is_some_and(|a| a.contains(RelativeAxisCode::REL_X));
    let keys = d.supported_keys();
    let keyboard = keys.is_some_and(|k| k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_SPACE));
    mouse || keyboard
}

/// Starts a reader for every wanted device not already in `open`.
pub fn scan(open: &mut HashSet<PathBuf>, grab: &watch::Sender<bool>, tx: &mpsc::UnboundedSender<Event>) {
    for (path, dev) in evdev::enumerate() {
        if open.contains(&path) || !wanted(&dev) {
            continue;
        }
        let name = dev.name().unwrap_or("?").to_string();
        let mut stream = match dev.into_event_stream() {
            Ok(s) => s,
            Err(err) => {
                log!("cannot read {name} ({}): {err}", path.display());
                continue;
            }
        };
        log!("watching {name} ({})", path.display());
        open.insert(path.clone());
        let mut grab = grab.subscribe();
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut grabbed = false;
            loop {
                let want = *grab.borrow_and_update();
                if want != grabbed {
                    let r = if want { stream.device_mut().grab() } else { stream.device_mut().ungrab() };
                    match r {
                        Ok(()) => grabbed = want,
                        Err(err) => {
                            log!("{} {name}: {err}", if want { "grab" } else { "ungrab" });
                            // Only try once per switch.
                            grabbed = want;
                        }
                    }
                }
                tokio::select! {
                    ev = stream.next_event() => match ev {
                        Ok(ev) => {
                            if tx.send(Event::Input(ev)).is_err() {
                                return;
                            }
                        }
                        Err(err) => {
                            log!("lost {name} ({}): {err}", path.display());
                            let _ = tx.send(Event::Gone(path));
                            return;
                        }
                    },
                    r = grab.changed() => if r.is_err() { return },
                }
            }
        });
    }
}
