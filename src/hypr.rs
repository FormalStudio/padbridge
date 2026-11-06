//! Hyprland's request socket, found again whenever the compositor restarts.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::debug;

#[derive(Deserialize, Clone, Debug)]
pub struct Monitor {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
    #[serde(default)]
    pub transform: u32,
    #[serde(default)]
    pub disabled: bool,
}

impl Monitor {
    /// Size in layout coordinates.
    pub fn size(&self) -> (f64, f64) {
        let (w, h) = if self.transform % 2 == 1 { (self.height, self.width) } else { (self.width, self.height) };
        (w / self.scale, h / self.scale)
    }
}

#[derive(Deserialize)]
struct Pos {
    x: f64,
    y: f64,
}

#[derive(Default)]
pub struct Hypr {
    sock: Option<PathBuf>,
}

fn find() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc_getuid() })));
    let base = runtime.join("hypr");
    if let Some(sig) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let s = base.join(sig).join(".socket.sock");
        if s.exists() {
            return Some(s);
        }
    }
    // The session's environment goes stale when Hyprland restarts under it: take the newest.
    std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .map(|e| e.path().join(".socket.sock"))
        .filter(|s| s.exists())
        .max_by_key(|s| s.metadata().and_then(|m| m.modified()).ok())
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

impl Hypr {
    async fn req(&mut self, cmd: &str) -> Option<String> {
        if self.sock.is_none() {
            self.sock = find();
        }
        let sock = self.sock.clone()?;
        let res = timeout(Duration::from_millis(100), async {
            let mut s = UnixStream::connect(&sock).await?;
            s.write_all(cmd.as_bytes()).await?;
            let mut out = String::new();
            s.read_to_string(&mut out).await?;
            Ok::<_, std::io::Error>(out)
        })
        .await;
        match res {
            Ok(Ok(out)) => Some(out),
            Ok(Err(err)) => {
                debug!("hyprland {cmd}: {err}");
                self.sock = None;
                None
            }
            Err(_) => {
                debug!("hyprland {cmd}: timed out");
                None
            }
        }
    }

    pub async fn cursor(&mut self) -> Option<(f64, f64)> {
        let p: Pos = serde_json::from_str(&self.req("j/cursorpos").await?).ok()?;
        Some((p.x, p.y))
    }

    pub async fn monitors(&mut self) -> Option<Vec<Monitor>> {
        let all: Vec<Monitor> = serde_json::from_str(&self.req("j/monitors").await?).ok()?;
        Some(all.into_iter().filter(|m| !m.disabled).collect())
    }

    /// Lua is the only config format here, where `dispatch movecursor` is a Lua syntax error.
    pub async fn warp(&mut self, x: f64, y: f64) {
        let cmd = format!("eval hl.dispatch(hl.dsp.cursor.move({{ x = {}, y = {} }}))", x as i64, y as i64);
        if let Some(out) = self.req(&cmd).await.filter(|o| o.trim() != "ok") {
            debug!("hyprland warp: {}", out.trim());
        }
    }
}
