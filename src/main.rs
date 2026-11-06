//! Share this machine's mouse and keyboard with an iPad by acting as a BLE HID device.
//!
//! Push the pointer past the configured edge of the Hyprland desktop and input goes to the iPad;
//! shove it back past the iPad's opposite edge (or press the toggle chord) and it comes home.

mod bluez;
mod bridge;
mod hid;
mod hypr;
mod input;
mod link;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, sleep_until};

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { println!($($t)*) };
}

/// journald reads the `<7>` prefix as debug priority.
#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { println!("<7>{}", format_args!($($t)*)) };
}

const USAGE: &str = "usage: padbridge [--config PATH] [--pair [SECONDS]] | --no-sniff";

struct Args {
    config: PathBuf,
    pair: u64,
}

fn args() -> Args {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut a = Args { config: home.join(".config/padbridge/config.toml"), pair: 0 };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => a.config = it.next().map(PathBuf::from).unwrap_or_else(|| die(USAGE)),
            "--pair" => {
                a.pair = match it.peek().and_then(|s| s.parse().ok()) {
                    Some(n) => {
                        it.next();
                        n
                    }
                    None => 120,
                }
            }
            "--no-sniff" => match link::no_sniff(0) {
                Ok(()) => std::process::exit(0),
                Err(err) => die(&format!("hci0 link policy: {err}")),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => die(USAGE),
        }
    }
    a
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}

fn config(path: &PathBuf) -> bridge::Config {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|err| die(&format!("{}: {err}", path.display()))),
        Err(_) => bridge::Config::default(),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = args();
    let cfg = config(&args.config);
    if let Err(err) = run(cfg, args.pair).await {
        log!("padbridge: {err}");
        std::process::exit(1);
    }
}

async fn run(cfg: bridge::Config, pair: u64) -> zbus::Result<()> {
    let conn = zbus::Connection::system().await?;

    let (up_tx, mut up_rx) = watch::channel(false);
    let (ipad_tx, ipad_rx) = watch::channel(None);
    let link = link::Link::new(up_tx);
    let hid = link.hid.clone();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let bt = bluez::Bt::new(conn.clone(), cfg.name.clone(), pair, ipad_tx, link.kick.clone()).await?;
    let mut bt_task = tokio::spawn(bt.run(cmd_rx, up_rx.clone()));
    tokio::spawn(link.run(ipad_rx));

    let (grab, _) = watch::channel(false);
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let mut open = HashSet::new();
    let mut bridge = bridge::Bridge::new(cfg, hid, grab.clone(), cmd_tx);
    bridge.refresh_monitors().await;

    let mut scan = interval(Duration::from_secs(2));
    let mut monitors = interval(Duration::from_secs(5));
    let mut term = signal(SignalKind::terminate()).map_err(|e| zbus::Error::Failure(e.to_string()))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| zbus::Error::Failure(e.to_string()))?;

    loop {
        let flush = bridge.flush_at;
        tokio::select! {
            biased;
            _ = term.recv() => break,
            _ = int.recv() => break,
            r = &mut bt_task => {
                bridge.shutdown().await;
                return match r {
                    Ok(Err(err)) => Err(err),
                    _ => Err(zbus::Error::Failure("bluetooth task ended".into())),
                };
            }
            _ = async { sleep_until(flush.unwrap()).await }, if flush.is_some() => bridge.flush().await,
            Some(ev) = ev_rx.recv() => match ev {
                input::Event::Input(ev) => bridge.input(ev).await,
                input::Event::Gone(path) => { open.remove(&path); }
            },
            Ok(()) = up_rx.changed() => {
                let up = *up_rx.borrow_and_update();
                bridge.set_link(up).await;
            }
            _ = scan.tick() => input::scan(&mut open, &grab, &ev_tx),
            _ = monitors.tick() => if !bridge.on_ipad() { bridge.refresh_monitors().await },
        }
    }
    bridge.shutdown().await;
    Ok(())
}
