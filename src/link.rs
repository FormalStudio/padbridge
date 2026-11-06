//! The input link: Bluetooth HID over the classic link, the one the iPad's audio already uses.
//!
//! HID over GATT on LE never reconnected by itself here. The iPad is the LE central and only
//! comes back when it decides to, and once its audio link is up it opens GATT over the classic
//! link instead, which BlueZ offers for every GATT service and iOS will not run HID over. A
//! classic HID device reconnects the way headphones do: it pages the host and opens the control
//! and interrupt channels itself, so this side decides when input comes back.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, watch};
use tokio::time::{sleep, timeout};

use crate::{debug, hid, log};

const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_L2CAP: libc::c_int = 0;
const SOL_BLUETOOTH: libc::c_int = 274;
const BT_SECURITY: libc::c_int = 4;
const BT_SECURITY_MEDIUM: u8 = 2;

const PSM_CONTROL: u16 = 0x11;
const PSM_INTERRUPT: u16 = 0x13;

/// DATA | Input, the header of every report sent on the interrupt channel.
const DATA_INPUT: u8 = 0xA1;

#[repr(C)]
struct SockaddrL2 {
    family: libc::sa_family_t,
    psm: u16,
    bdaddr: [u8; 6],
    cid: u16,
    bdaddr_type: u8,
}

#[repr(C)]
struct BtSecurity {
    level: u8,
    key_size: u8,
}

fn parse_addr(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = s.split(':').map(|p| u8::from_str_radix(p, 16)).collect::<Result<_, _>>().ok()?;
    let mut b: [u8; 6] = parts.try_into().ok()?;
    // bdaddr_t is little-endian.
    b.reverse();
    Some(b)
}

type Chan = Arc<AsyncFd<OwnedFd>>;

async fn open(addr: [u8; 6], psm: u16) -> io::Result<Chan> {
    // SAFETY: plain socket calls on a descriptor we own from here on.
    let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, BTPROTO_L2CAP) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let sec = BtSecurity { level: BT_SECURITY_MEDIUM, key_size: 0 };
    unsafe {
        libc::setsockopt(fd.as_raw_fd(), SOL_BLUETOOTH, BT_SECURITY, &sec as *const _ as *const libc::c_void, size_of::<BtSecurity>() as u32);
    }
    let sa = SockaddrL2 { family: AF_BLUETOOTH as libc::sa_family_t, psm: psm.to_le(), bdaddr: addr, cid: 0, bdaddr_type: 0 };
    let rc = unsafe { libc::connect(fd.as_raw_fd(), &sa as *const _ as *const libc::sockaddr, size_of::<SockaddrL2>() as u32) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(err);
        }
    }
    // SAFETY: the descriptor is owned by the AsyncFd and closed with it.
    let afd = Arc::new(unsafe { AsyncFd::register(fd) }.map_err(|e| e.into_parts().1)?);
    // The connect completes when the socket turns writable; SO_ERROR says how.
    let _ = afd.writable().await?;
    let mut err: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as u32;
    unsafe {
        libc::getsockopt(afd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_ERROR, &mut err as *mut _ as *mut libc::c_void, &mut len);
    }
    if err != 0 {
        return Err(io::Error::from_raw_os_error(err));
    }
    Ok(afd)
}

async fn recv(c: &Chan, buf: &mut [u8]) -> io::Result<usize> {
    c.async_io(Interest::READABLE, |fd| {
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    })
    .await
}

async fn send(c: &Chan, data: &[u8]) -> io::Result<()> {
    c.async_io(Interest::WRITABLE, |fd| {
        let n = unsafe { libc::send(fd.as_raw_fd(), data.as_ptr() as *const libc::c_void, data.len(), libc::MSG_NOSIGNAL) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    })
    .await
}

/// Answers the host's HID control requests: every SET is acknowledged, every GET refused.
fn handshake(msg: u8) -> Option<u8> {
    match msg >> 4 {
        // GET_REPORT, GET_PROTOCOL, GET_IDLE: ERR_UNSUPPORTED_REQUEST.
        0x4 | 0x6 | 0x8 => Some(0x03),
        // SET_REPORT, SET_PROTOCOL, SET_IDLE: SUCCESSFUL.
        0x5 | 0x7 | 0x9 => Some(0x00),
        _ => None,
    }
}

/// Motion not yet sent, summed while the radio is busy so it leaves as one report.
#[derive(Default)]
struct Motion {
    buttons: u8,
    dx: i32,
    dy: i32,
    wheel: i32,
    pan: i32,
    dirty: bool,
}

impl Motion {
    /// Takes one report's worth, leaving whatever does not fit its fields for the next.
    fn take(&mut self) -> Vec<u8> {
        fn part(v: &mut i32, max: i32) -> i32 {
            let p = (*v).clamp(-max, max);
            *v -= p;
            p
        }
        let (dx, dy) = (part(&mut self.dx, 32767), part(&mut self.dy, 32767));
        let (wheel, pan) = (part(&mut self.wheel, 127), part(&mut self.pan, 127));
        self.dirty = self.dx != 0 || self.dy != 0 || self.wheel != 0 || self.pan != 0;
        message(hid::MOUSE_ID, &hid::mouse(self.buttons, dx, dy, wheel, pan))
    }
}

#[derive(Default)]
struct Out {
    up: bool,
    /// Keyboard, consumer and button changes, each of which has to arrive.
    queue: VecDeque<Vec<u8>>,
    motion: Motion,
}

fn message(report_id: u8, data: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(data.len() + 2);
    msg.extend_from_slice(&[DATA_INPUT, report_id]);
    msg.extend_from_slice(data);
    msg
}

/// Where reports go. Nothing here waits on the radio: a sender task drains it while the link
/// is up, and motion that piles up while the radio is busy is merged rather than queued, so a
/// stall never comes out as a burst of old movement.
#[derive(Clone, Default)]
pub struct Hid {
    out: Arc<Mutex<Out>>,
    ready: Arc<Notify>,
}

impl Hid {
    pub fn report(&self, report_id: u8, data: &[u8]) {
        let mut o = self.out.lock().unwrap();
        if !o.up {
            return;
        }
        // Motion before this report has to arrive before it.
        while o.motion.dirty {
            let m = o.motion.take();
            o.queue.push_back(m);
        }
        o.queue.push_back(message(report_id, data));
        drop(o);
        self.ready.notify_one();
    }

    pub fn motion(&self, buttons: u8, dx: i32, dy: i32, wheel: i32, pan: i32) {
        let mut o = self.out.lock().unwrap();
        if !o.up {
            return;
        }
        // A press and its release merged into one report would be a lost click.
        if buttons != o.motion.buttons {
            while o.motion.dirty {
                let m = o.motion.take();
                o.queue.push_back(m);
            }
            o.motion.buttons = buttons;
            o.queue.push_back(message(hid::MOUSE_ID, &hid::mouse(buttons, dx, dy, wheel, pan)));
        } else {
            let m = &mut o.motion;
            m.dx = m.dx.saturating_add(dx);
            m.dy = m.dy.saturating_add(dy);
            m.wheel = m.wheel.saturating_add(wheel);
            m.pan = m.pan.saturating_add(pan);
            m.dirty = true;
        }
        drop(o);
        self.ready.notify_one();
    }

    fn set_up(&self, up: bool) {
        let mut o = self.out.lock().unwrap();
        *o = Out { up, ..Out::default() };
    }

    fn next(&self) -> Option<Vec<u8>> {
        let mut o = self.out.lock().unwrap();
        if let Some(m) = o.queue.pop_front() {
            return Some(m);
        }
        o.motion.dirty.then(|| o.motion.take())
    }

    /// Sends whatever is pending, one report each time the socket can take one.
    async fn pump(&self, c: &Chan) -> String {
        loop {
            match self.next() {
                Some(msg) => {
                    if let Err(err) = send(c, &msg).await {
                        return err.to_string();
                    }
                }
                None => self.ready.notified().await,
            }
        }
    }
}

/// Keeps the two channels open to the paired iPad, reconnecting whenever they drop.
pub struct Link {
    pub hid: Hid,
    /// Wakes the loop for an attempt now: the user wants the iPad, or its audio link came up.
    pub kick: Arc<Notify>,
    up: watch::Sender<bool>,
}

impl Link {
    pub fn new(up: watch::Sender<bool>) -> Self {
        Link { hid: Hid::default(), kick: Arc::new(Notify::new()), up }
    }

    pub async fn run(self, mut ipad: watch::Receiver<Option<String>>) {
        let mut backoff = Duration::from_secs(2);
        loop {
            let addr = ipad.borrow_and_update().clone();
            let Some(addr) = addr.as_deref().and_then(parse_addr) else {
                if ipad.changed().await.is_err() {
                    return;
                }
                continue;
            };
            match self.connect(addr).await {
                Ok((control, interrupt)) => {
                    backoff = Duration::from_secs(2);
                    self.hid.set_up(true);
                    self.up.send_replace(true);
                    log!("input link up");
                    let why = tokio::select! {
                        why = serve(&control, &interrupt) => why,
                        why = self.hid.pump(&interrupt) => why,
                    };
                    self.hid.set_up(false);
                    self.up.send_replace(false);
                    log!("input link down: {why}");
                }
                Err(err) => {
                    debug!("input link: {err}, next try in {}s", backoff.as_secs());
                    tokio::select! {
                        _ = sleep(backoff) => {}
                        _ = self.kick.notified() => {}
                        _ = ipad.changed() => {}
                    }
                    // Paging an iPad that is not there takes air time from the audio links.
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn connect(&self, addr: [u8; 6]) -> io::Result<(Chan, Chan)> {
        let attempt = async {
            let control = open(addr, PSM_CONTROL).await?;
            let interrupt = open(addr, PSM_INTERRUPT).await?;
            // The smallest send buffer the kernel allows, so the socket stops taking reports
            // as soon as the radio falls behind and the rest merge in Motion instead.
            let one: libc::c_int = 1;
            unsafe {
                libc::setsockopt(interrupt.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, &one as *const _ as *const libc::c_void, size_of::<libc::c_int>() as u32);
            }
            Ok::<_, io::Error>((control, interrupt))
        };
        timeout(Duration::from_secs(15), attempt).await.unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
    }
}

/// Runs until either channel closes, answering the host's control requests.
async fn serve(control: &Chan, interrupt: &Chan) -> String {
    let mut cbuf = [0u8; 64];
    let mut ibuf = [0u8; 64];
    loop {
        tokio::select! {
            r = recv(control, &mut cbuf) => match r {
                Ok(0) => return "closed by the iPad".into(),
                Ok(_) => {
                    // HID_CONTROL VIRTUAL_CABLE_UNPLUG ends the bond's input side.
                    if cbuf[0] == 0x15 {
                        return "unplugged by the iPad".into();
                    }
                    if let Some(code) = handshake(cbuf[0]) && let Err(err) = send(control, &[code]).await {
                        return err.to_string();
                    }
                }
                Err(err) => return err.to_string(),
            },
            // Output reports (keyboard LEDs) arrive here; nothing to do with them.
            r = recv(interrupt, &mut ibuf) => match r {
                Ok(0) => return "closed by the iPad".into(),
                Ok(_) => {}
                Err(err) => return err.to_string(),
            },
        }
    }
}

pub const PROFILE_UUID: &str = "8e1f0cf7-508f-4875-b62c-fbb67fd34812";

/// The SDP record the iPad reads to learn this is a keyboard and mouse, and its descriptor.
pub fn sdp_record(name: &str, report_map: &[u8]) -> String {
    let hex: String = report_map.iter().map(|b| format!("{b:02x}")).collect();
    let name = name.replace(['&', '<', '>', '"'], "");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<record>
 <attribute id="0x0001"><sequence><uuid value="0x1124"/></sequence></attribute>
 <attribute id="0x0004"><sequence><sequence><uuid value="0x0100"/><uint16 value="0x0011"/></sequence><sequence><uuid value="0x0011"/></sequence></sequence></attribute>
 <attribute id="0x0005"><sequence><uuid value="0x1002"/></sequence></attribute>
 <attribute id="0x0006"><sequence><uint16 value="0x656e"/><uint16 value="0x006a"/><uint16 value="0x0100"/></sequence></attribute>
 <attribute id="0x0009"><sequence><sequence><uuid value="0x1124"/><uint16 value="0x0101"/></sequence></sequence></attribute>
 <attribute id="0x000d"><sequence><sequence><sequence><uuid value="0x0100"/><uint16 value="0x0013"/></sequence><sequence><uuid value="0x0011"/></sequence></sequence></sequence></attribute>
 <attribute id="0x0100"><text value="{name}"/></attribute>
 <attribute id="0x0101"><text value="Keyboard and mouse"/></attribute>
 <attribute id="0x0102"><text value="FormalStudio"/></attribute>
 <attribute id="0x0200"><uint16 value="0x0100"/></attribute>
 <attribute id="0x0201"><uint16 value="0x0111"/></attribute>
 <attribute id="0x0202"><uint8 value="0xc0"/></attribute>
 <attribute id="0x0203"><uint8 value="0x00"/></attribute>
 <attribute id="0x0204"><boolean value="false"/></attribute>
 <attribute id="0x0205"><boolean value="true"/></attribute>
 <attribute id="0x0206"><sequence><sequence><uint8 value="0x22"/><text encoding="hex" value="{hex}"/></sequence></sequence></attribute>
 <attribute id="0x0207"><sequence><sequence><uint16 value="0x0409"/><uint16 value="0x0100"/></sequence></sequence></attribute>
 <attribute id="0x020b"><uint16 value="0x0100"/></attribute>
 <attribute id="0x020c"><uint16 value="0x0c80"/></attribute>
 <attribute id="0x020d"><boolean value="false"/></attribute>
 <attribute id="0x020e"><boolean value="false"/></attribute>
</record>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_is_little_endian() {
        assert_eq!(parse_addr("C8:89:F3:45:A7:AA"), Some([0xAA, 0xA7, 0x45, 0xF3, 0x89, 0xC8]));
        assert_eq!(parse_addr("nope"), None);
    }

    #[test]
    fn motion_merges_but_clicks_do_not() {
        let h = Hid::default();
        h.set_up(true);
        h.motion(0, 3, 1, 0, 0);
        h.motion(0, 4, -1, 0, 0);
        h.motion(1, 0, 0, 0, 0);
        h.motion(0, 0, 0, 0, 0);
        h.motion(0, 40000, 0, 0, 0);
        let sent: Vec<_> = std::iter::from_fn(|| h.next()).collect();
        assert_eq!(sent, vec![
            message(hid::MOUSE_ID, &hid::mouse(0, 7, 0, 0, 0)),
            message(hid::MOUSE_ID, &hid::mouse(1, 0, 0, 0, 0)),
            message(hid::MOUSE_ID, &hid::mouse(0, 0, 0, 0, 0)),
            message(hid::MOUSE_ID, &hid::mouse(0, 32767, 0, 0, 0)),
            message(hid::MOUSE_ID, &hid::mouse(0, 40000 - 32767, 0, 0, 0)),
        ]);
    }

    #[test]
    fn sets_succeed_and_gets_are_refused() {
        assert_eq!(handshake(0x70), Some(0x00));
        assert_eq!(handshake(0x41), Some(0x03));
        assert_eq!(handshake(0x10), None);
    }
}
