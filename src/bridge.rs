//! Where input goes: watching for the edge on this desktop, forwarding to the iPad while there.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use evdev::{EventSummary, InputEvent, KeyCode, RelativeAxisCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::bluez::Cmd;
use crate::link::Hid;
use crate::hid;
use crate::hypr::{Hypr, Monitor};
use crate::status::Status;
use crate::{debug, log};

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn parse(s: &str) -> Option<Edge> {
        Some(match s {
            "left" => Edge::Left,
            "right" => Edge::Right,
            "top" => Edge::Top,
            "bottom" => Edge::Bottom,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }

    /// The axis that crosses this edge.
    fn axis(self) -> RelativeAxisCode {
        match self {
            Edge::Left | Edge::Right => RelativeAxisCode::REL_X,
            Edge::Top | Edge::Bottom => RelativeAxisCode::REL_Y,
        }
    }

    /// The sign of motion on `axis` that heads toward this edge.
    fn outward(self) -> i32 {
        match self {
            Edge::Right | Edge::Bottom => 1,
            Edge::Left | Edge::Top => -1,
        }
    }
}

/// Where the iPad sits: an edge of this desktop and the stretch of it, 0 to 1 from its top or
/// left end, that leads onto the iPad, as macOS arranges a second display.
#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Debug)]
pub struct Position {
    pub edge: Edge,
    pub start: f64,
    pub end: f64,
}

impl Position {
    pub fn new(edge: Edge, start: f64, end: f64) -> Option<Position> {
        (0.0 <= start && start < end && end <= 1.0).then_some(Position { edge, start, end })
    }

    fn file() -> PathBuf {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
        state.join("padbridge/position.json")
    }

    /// The position the shell last set, or the config's edge, whole.
    pub fn load(edge: Edge) -> Position {
        std::fs::read_to_string(Self::file())
            .ok()
            .and_then(|s| serde_json::from_str::<Position>(&s).ok())
            .and_then(|p| Position::new(p.edge, p.start, p.end))
            .unwrap_or(Position { edge, start: 0.0, end: 1.0 })
    }

    fn save(&self) {
        let path = Self::file();
        let res = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, serde_json::to_string(self).unwrap_or_default()));
        if let Err(err) = res {
            log!("{}: {err}", path.display());
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    /// Side of the desktop the iPad sits on until the shell sets a position.
    pub edge: Edge,
    /// Mouse counts to cross the iPad, used to tell where its pointer is.
    pub ipad_width: f64,
    pub ipad_height: f64,
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
            ipad_height: 1260.0,
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
/// Counts the pointer is placed off the iPad's edge on arrival, clear of its edge gestures.
const EDGE_GAP: i32 = 16;
/// Reports of 127 counts in the arrival pin, enough to cross the iPad with its acceleration.
const PIN_STEPS: usize = 12;

pub struct Bridge {
    cfg: Config,
    toggle: Vec<KeyCode>,
    hypr: Hypr,
    hid: Hid,
    grab: watch::Sender<bool>,
    wake: mpsc::UnboundedSender<Cmd>,
    status: watch::Sender<Status>,
    pos: Position,
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
    /// How far into the iPad its pointer is by our count, 0 at the edge we arrived from.
    depth: f64,
    push: f64,
    /// When the current shove began, by the kernel's own event clock: when padbridge gets to
    /// the events can be late and bunched, which would make slow motion look like a shove.
    push_at: Option<SystemTime>,
    last_query: Option<Instant>,
    monitors: Vec<Monitor>,
    /// The cursor where it left this desktop, put back on return.
    left_at: Option<(f64, f64)>,
    sent: u64,
    warned_down: bool,
}

impl Bridge {
    pub fn new(cfg: Config, hid: Hid, grab: watch::Sender<bool>, wake: mpsc::UnboundedSender<Cmd>, status: watch::Sender<Status>) -> Self {
        let pos = status.borrow().position;
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
            hid,
            grab,
            wake,
            status,
            pos,
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
            depth: 0.0,
            push: 0.0,
            push_at: None,
            last_query: None,
            monitors: Vec::new(),
            left_at: None,
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
        let p = self.pos;
        for m in &self.monitors {
            let (w, h) = m.size();
            if !(m.y <= y && y < m.y + h + 1.0 && m.x <= x && x < m.x + w + 1.0) {
                continue;
            }
            let mut others = self.monitors.iter().filter(|n| !std::ptr::eq(*n, m));
            let (on, along) = match p.edge {
                Edge::Right => (x >= m.x + w - 1.0 && !others.any(|n| n.x == m.x + w), (y - m.y) / h),
                Edge::Left => (x <= m.x && !others.any(|n| n.x + n.size().0 == m.x), (y - m.y) / h),
                Edge::Bottom => (y >= m.y + h - 1.0 && !others.any(|n| n.y == m.y + h), (x - m.x) / w),
                Edge::Top => (y <= m.y && !others.any(|n| n.y + n.size().1 == m.y), (x - m.x) / w),
            };
            return on && p.start <= along && along <= p.end;
        }
        false
    }

    /// Moves the iPad to another edge or stretch, kept for the next start.
    pub async fn set_position(&mut self, pos: Position) {
        if pos == self.pos {
            return;
        }
        if self.on_ipad && pos.edge != self.pos.edge {
            self.leave("moved").await;
        }
        self.pos = pos;
        pos.save();
        self.status.send_modify(|s| s.position = pos);
        log!("ipad on the {} edge, {:.2} to {:.2}", pos.edge.name(), pos.start, pos.end);
    }

    pub async fn switch(&mut self) {
        if self.on_ipad {
            self.leave("switch").await;
        } else {
            self.enter().await;
        }
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
        self.status.send_modify(|s| s.active = true);
        self.sent = 0;
        self.left_at = self.hypr.cursor().await;
        self.grab.send_replace(true);
        // Pin the iPad's pointer to the edge we arrive from, in mouse-sized steps: one 32767
        // delta lands it on the opposite edge, where iPadOS starts its Stage Manager strip
        // gesture and the pointer stops following. Then step off the edge so the gesture on
        // this side does not start either.
        let sign = -self.pos.edge.outward();
        let step = |n: i32| match self.pos.edge.axis() {
            RelativeAxisCode::REL_X => hid::mouse(0, n, 0, 0, 0),
            _ => hid::mouse(0, 0, n, 0, 0),
        };
        for _ in 0..PIN_STEPS {
            self.hid.report(hid::MOUSE_ID, &step(sign * 127));
        }
        self.hid.report(hid::MOUSE_ID, &step(-sign * EDGE_GAP));
        self.depth = 0.0;
        self.push = 0.0;
        self.push_at = None;
        log!("-> ipad");
    }

    async fn leave(&mut self, reason: &str) {
        if !self.on_ipad {
            return;
        }
        self.on_ipad = false;
        self.status.send_modify(|s| s.active = false);
        self.release_all();
        self.grab.send_replace(false);
        // Back where it left, a few pixels in from the edge so it does not cross again.
        if let Some((x, y)) = self.hypr.cursor().await {
            let (lx, ly) = self.left_at.unwrap_or((x, y));
            let step = -3.0 * self.pos.edge.outward() as f64;
            match self.pos.edge.axis() {
                RelativeAxisCode::REL_X => self.hypr.warp(x + step, ly).await,
                _ => self.hypr.warp(lx, y + step).await,
            }
        }
        self.push = 0.0;
        log!("<- linux ({reason})");
        debug!("sent {} mouse reports while on the iPad", self.sent);
    }

    pub fn release_all(&mut self) {
        self.keys.clear();
        self.mods = 0;
        self.buttons = 0;
        self.dx = 0.0;
        self.dy = 0.0;
        self.wheel = 0;
        self.pan = 0;
        self.flush_at = None;
        self.hid.report(hid::KBD_ID, &[0; hid::KBD_LEN]);
        self.hid.motion(0, 0, 0, 0, 0);
        self.hid.report(hid::CONSUMER_ID, &[0; hid::CONSUMER_LEN]);
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
            EventSummary::RelativeAxis(_, axis, v) if axis == self.pos.edge.axis() => {
                let toward = v * self.pos.edge.outward() > 0;
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
                    RelativeAxisCode::REL_X => self.dx += v as f64 * s,
                    RelativeAxisCode::REL_Y => self.dy += v as f64 * s,
                    RelativeAxisCode::REL_WHEEL => self.wheel += v,
                    RelativeAxisCode::REL_HWHEEL => self.pan += v,
                    _ => return,
                }
                if axis == self.pos.edge.axis() {
                    self.track_return(v as f64 * s, ev.timestamp()).await;
                    if !self.on_ipad {
                        return;
                    }
                }
                if self.flush_at.is_none() {
                    self.flush_at = Some(Instant::now() + FLUSH_EVERY);
                }
            }
            EventSummary::Key(_, k, value) => {
                if let Some(bit) = hid::button(k) {
                    self.buttons = if value != 0 { self.buttons | bit } else { self.buttons & !bit };
                    self.press(k, value != 0);
                    return self.flush();
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
                if let Some(usage) = hid::consumer(k) {
                    let usage = if value != 0 { usage } else { 0 };
                    self.hid.report(hid::CONSUMER_ID, &usage.to_le_bytes());
                } else {
                    self.hid.report(hid::KBD_ID, &hid::keyboard(self.mods, &self.keys));
                }
                if self.pending_toggle && self.keys.is_empty() {
                    self.pending_toggle = false;
                    self.leave("chord").await;
                }
            }
            _ => {}
        }
    }

    /// `d` is motion on the axis crossing the edge.
    async fn track_return(&mut self, d: f64, now: SystemTime) {
        let since = |t: SystemTime| now.duration_since(t).unwrap_or_default();
        let inward = d * self.pos.edge.outward() as f64;
        if inward < 0.0 && self.depth <= 0.0 {
            if self.push_at.is_none_or(|t| since(t) > RETURN_WINDOW) {
                self.push = 0.0;
                self.push_at = Some(now);
            }
            self.push -= inward;
            if self.push >= self.cfg.return_push && self.keys.is_empty() && self.buttons == 0 {
                let took = self.push_at.map(since).unwrap_or_default();
                debug!("shove of {:.0} counts in {:.3}s", self.push, took.as_secs_f64());
                self.leave("shove").await;
            }
            return;
        }
        self.push = 0.0;
        self.push_at = None;
        let span = match self.pos.edge.axis() {
            RelativeAxisCode::REL_X => self.cfg.ipad_width,
            _ => self.cfg.ipad_height,
        };
        self.depth = (self.depth + inward).clamp(0.0, span);
    }

    pub fn flush(&mut self) {
        self.flush_at = None;
        let (dx, dy) = (self.dx.trunc(), self.dy.trunc());
        self.dx -= dx;
        self.dy -= dy;
        let (wheel, pan) = (std::mem::take(&mut self.wheel), std::mem::take(&mut self.pan));
        self.send_mouse(dx as i32, dy as i32, wheel, pan);
    }

    fn send_mouse(&mut self, dx: i32, dy: i32, wheel: i32, pan: i32) {
        if self.sent == 0 {
            debug!("first mouse report to the iPad");
        }
        self.sent += 1;
        self.hid.motion(self.buttons, dx, dy, wheel, pan);
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
