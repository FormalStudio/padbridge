//! Where input goes: watching for the edge on this desktop, forwarding to the iPad while there.

use std::time::Duration;

use evdev::{EventSummary, InputEvent, KeyCode, RelativeAxisCode};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::bluez::Cmd;
use crate::gatt::Reports;
use crate::hid;
use crate::hypr::{Hypr, Monitor};
use crate::{debug, log};

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
}

#[derive(Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    /// Side of the desktop the iPad sits on.
    pub edge: Edge,
    /// Mouse counts to cross the iPad, used to tell where its pointer is.
    pub ipad_width: f64,
    pub sensitivity: f64,
    /// Counts past the edge before switching, which stops accidental crossings.
    pub edge_push: f64,
    /// The iPad's pointer acceleration lets the counted position reach its edge early, so going
    /// back takes a fast shove past it: this many counts within RETURN_WINDOW.
    pub return_push: f64,
    pub toggle: Vec<String>,
    pub name: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            edge: Edge::Left,
            ipad_width: 1800.0,
            sensitivity: 1.0,
            edge_push: 40.0,
            return_push: 300.0,
            toggle: ["KEY_LEFTCTRL", "KEY_LEFTALT", "KEY_I"].map(String::from).to_vec(),
            name: "g815".into(),
        }
    }
}

/// A slow drift past the counted edge never adds up to return_push within this.
const RETURN_WINDOW: Duration = Duration::from_millis(100);
/// Motion is summed and sent at most this often, a little under the iPad's connection interval.
const FLUSH_EVERY: Duration = Duration::from_millis(7);
const QUERY_EVERY: Duration = Duration::from_millis(8);

pub struct Bridge {
    cfg: Config,
    toggle: Vec<KeyCode>,
    hypr: Hypr,
    reports: Reports,
    grab: watch::Sender<bool>,
    wake: mpsc::UnboundedSender<Cmd>,
    le_up: bool,
    on_ipad: bool,
    pending_toggle: bool,
    /// Held keys and buttons in press order, which is the order the keyboard report lists them.
    keys: Vec<KeyCode>,
    mods: u8,
    buttons: u8,
    dx: f64,
    dy: f64,
    wheel: i32,
    pan: i32,
    pub flush_at: Option<Instant>,
    /// Where the iPad's pointer is by our count, 0 at the edge we arrived from.
    vx: f64,
    push: f64,
    push_at: Option<Instant>,
    last_query: Option<Instant>,
    monitors: Vec<Monitor>,
    return_y: Option<f64>,
    sent: u64,
    warned_down: bool,
}

impl Bridge {
    pub fn new(cfg: Config, reports: Reports, grab: watch::Sender<bool>, wake: mpsc::UnboundedSender<Cmd>) -> Self {
        let toggle = cfg
            .toggle
            .iter()
            .filter_map(|k| match k.parse::<KeyCode>() {
                Ok(code) => Some(code),
                Err(_) => {
                    log!("unknown toggle key {k}");
                    None
                }
            })
            .collect();
        Bridge {
            cfg,
            toggle,
            hypr: Hypr::default(),
            reports,
            grab,
            wake,
            le_up: false,
            on_ipad: false,
            pending_toggle: false,
            keys: Vec::new(),
            mods: 0,
            buttons: 0,
            dx: 0.0,
            dy: 0.0,
            wheel: 0,
            pan: 0,
            flush_at: None,
            vx: 0.0,
            push: 0.0,
            push_at: None,
            last_query: None,
            monitors: Vec::new(),
            return_y: None,
            sent: 0,
            warned_down: false,
        }
    }

    pub async fn refresh_monitors(&mut self) {
        if let Some(m) = self.hypr.monitors().await {
            self.monitors = m;
        }
    }

    pub async fn set_link(&mut self, up: bool) {
        self.le_up = up;
        if up {
            self.warned_down = false;
        } else if self.on_ipad {
            self.leave("input link down").await;
        }
    }

    fn at_edge(&self, x: f64, y: f64) -> bool {
        for m in &self.monitors {
            let (w, h) = m.size();
            if !(m.y <= y && y < m.y + h && m.x <= x && x < m.x + w + 1.0) {
                continue;
            }
            let mut others = self.monitors.iter().filter(|n| !std::ptr::eq(*n, m));
            return match self.cfg.edge {
                Edge::Right => x >= m.x + w - 1.0 && !others.any(|n| n.x == m.x + w),
                Edge::Left => x <= m.x && !others.any(|n| n.x + n.size().0 == m.x),
            };
        }
        false
    }

    fn held(&self, k: KeyCode) -> bool {
        self.keys.contains(&k)
    }

    fn press(&mut self, k: KeyCode, down: bool) {
        self.keys.retain(|&c| c != k);
        if down {
            self.keys.push(k);
        }
    }

    fn chord_pressed(&self, k: KeyCode, value: i32) -> bool {
        value == 1 && !self.toggle.is_empty() && self.toggle.contains(&k) && self.toggle.iter().all(|&t| t == k || self.held(t))
    }

    fn any_toggle_held(&self) -> bool {
        self.toggle.iter().any(|&t| self.held(t))
    }

    async fn enter(&mut self) {
        if self.on_ipad {
            return;
        }
        if !self.le_up {
            if !self.warned_down {
                log!("ipad input link is down, staying here");
            }
            self.warned_down = true;
            self.push = 0.0;
            let _ = self.wake.send(Cmd::Wake);
            return;
        }
        self.on_ipad = true;
        self.sent = 0;
        self.return_y = self.hypr.cursor().await.map(|(_, y)| y);
        self.grab.send_replace(true);
        // Pin the iPad's pointer to the edge we arrive from.
        let sign = if self.cfg.edge == Edge::Right { -1 } else { 1 };
        self.send_mouse(sign * 32767, 0, 0, 0).await;
        self.vx = 0.0;
        self.push = 0.0;
        self.push_at = None;
        log!("-> ipad");
    }

    async fn leave(&mut self, reason: &str) {
        if !self.on_ipad {
            return;
        }
        self.on_ipad = false;
        self.release_all().await;
        self.grab.send_replace(false);
        if let Some((x, y)) = self.hypr.cursor().await {
            let step = if self.cfg.edge == Edge::Right { -3.0 } else { 3.0 };
            self.hypr.warp(x + step, self.return_y.unwrap_or(y)).await;
        }
        self.push = 0.0;
        log!("<- linux ({reason})");
        debug!("sent {} mouse reports while on the iPad", self.sent);
    }

    pub async fn release_all(&mut self) {
        self.keys.clear();
        self.mods = 0;
        self.buttons = 0;
        self.dx = 0.0;
        self.dy = 0.0;
        self.wheel = 0;
        self.pan = 0;
        self.flush_at = None;
        let r = self.reports.clone();
        r.send(&r.keyboard, &[0; hid::KBD_LEN]).await;
        r.send(&r.mouse, &[0; hid::MOUSE_LEN]).await;
        r.send(&r.consumer, &[0; hid::CONSUMER_LEN]).await;
    }

    pub async fn input(&mut self, ev: InputEvent) {
        if self.on_ipad {
            self.forward(ev).await;
        } else {
            self.watch_here(ev).await;
        }
    }

    async fn watch_here(&mut self, ev: InputEvent) {
        match ev.destructure() {
            EventSummary::Key(_, k, value) => {
                if self.chord_pressed(k, value) {
                    self.pending_toggle = true;
                }
                match value {
                    0 => {
                        self.press(k, false);
                        if self.pending_toggle && !self.any_toggle_held() {
                            self.pending_toggle = false;
                            self.keys.clear();
                            self.enter().await;
                        }
                    }
                    1 => self.press(k, true),
                    _ => {}
                }
            }
            EventSummary::RelativeAxis(_, RelativeAxisCode::REL_X, v) => {
                let toward = if self.cfg.edge == Edge::Right { v > 0 } else { v < 0 };
                if !toward {
                    self.push = 0.0;
                    return;
                }
                let now = Instant::now();
                if self.push == 0.0 && self.last_query.is_some_and(|t| now - t < QUERY_EVERY) {
                    return;
                }
                self.last_query = Some(now);
                let Some((x, y)) = self.hypr.cursor().await else { return };
                if self.at_edge(x, y) {
                    self.push += v.unsigned_abs() as f64;
                    if self.push >= self.cfg.edge_push && self.keys.is_empty() {
                        self.enter().await;
                    }
                } else {
                    self.push = 0.0;
                }
            }
            _ => {}
        }
    }

    async fn forward(&mut self, ev: InputEvent) {
        match ev.destructure() {
            EventSummary::RelativeAxis(_, axis, v) => {
                let s = self.cfg.sensitivity;
                match axis {
                    RelativeAxisCode::REL_X => {
                        self.dx += v as f64 * s;
                        self.track_return(v as f64 * s).await;
                        if !self.on_ipad {
                            return;
                        }
                    }
                    RelativeAxisCode::REL_Y => self.dy += v as f64 * s,
                    RelativeAxisCode::REL_WHEEL => self.wheel += v,
                    RelativeAxisCode::REL_HWHEEL => self.pan += v,
                    _ => return,
                }
                if self.flush_at.is_none() {
                    self.flush_at = Some(Instant::now() + FLUSH_EVERY);
                }
            }
            EventSummary::Key(_, k, value) => {
                if let Some(bit) = hid::button(k) {
                    self.buttons = if value != 0 { self.buttons | bit } else { self.buttons & !bit };
                    self.press(k, value != 0);
                    return self.flush().await;
                }
                // The iPad does its own key repeat.
                if value == 2 {
                    return;
                }
                if self.chord_pressed(k, value) {
                    self.pending_toggle = true;
                }
                if let Some(bit) = hid::modifier(k) {
                    self.mods = if value != 0 { self.mods | bit } else { self.mods & !bit };
                }
                self.press(k, value != 0);
                let r = self.reports.clone();
                if let Some(usage) = hid::consumer(k) {
                    let usage = if value != 0 { usage } else { 0 };
                    r.send(&r.consumer, &usage.to_le_bytes()).await;
                } else {
                    r.send(&r.keyboard, &hid::keyboard(self.mods, &self.keys)).await;
                }
                if self.pending_toggle && self.keys.is_empty() {
                    self.pending_toggle = false;
                    self.leave("chord").await;
                }
            }
            _ => {}
        }
    }

    async fn track_return(&mut self, dx: f64) {
        let inward = if self.cfg.edge == Edge::Right { dx } else { -dx };
        if inward < 0.0 && self.vx <= 0.0 {
            let now = Instant::now();
            if self.push_at.is_none_or(|t| now - t > RETURN_WINDOW) {
                self.push = 0.0;
                self.push_at = Some(now);
            }
            self.push -= inward;
            if self.push >= self.cfg.return_push && self.keys.is_empty() && self.buttons == 0 {
                let took = self.push_at.map(|t| now - t).unwrap_or_default();
                debug!("shove of {:.0} counts in {:.3}s", self.push, took.as_secs_f64());
                self.leave("shove").await;
            }
            return;
        }
        self.push = 0.0;
        self.push_at = None;
        self.vx = (self.vx + inward).clamp(0.0, self.cfg.ipad_width);
    }

    pub async fn flush(&mut self) {
        self.flush_at = None;
        let (dx, dy) = (self.dx.trunc(), self.dy.trunc());
        self.dx -= dx;
        self.dy -= dy;
        let (wheel, pan) = (std::mem::take(&mut self.wheel), std::mem::take(&mut self.pan));
        self.send_mouse(dx as i32, dy as i32, wheel, pan).await;
    }

    async fn send_mouse(&mut self, dx: i32, dy: i32, wheel: i32, pan: i32) {
        if self.sent == 0 {
            debug!("first mouse report to the iPad");
        }
        self.sent += 1;
        let r = self.reports.clone();
        r.send(&r.mouse, &hid::mouse(self.buttons, dx, dy, wheel, pan)).await;
    }

    pub fn on_ipad(&self) -> bool {
        self.on_ipad
    }

    pub async fn shutdown(&mut self) {
        if self.on_ipad {
            self.leave("stopping").await;
        }
    }
}
