//! The input link: Bluetooth HID over the classic link, the one the iPad's audio already uses.
//!
//! HID over GATT on LE never reconnected by itself here. The iPad is the LE central and only
//! comes back when it decides to, and once its audio link is up it opens GATT over the classic
//! link instead, which BlueZ offers for every GATT service and iOS will not run HID over. A
//! classic HID device reconnects the way headphones do: it pages the host and opens the control
//! and interrupt channels itself, so this side decides when input comes back.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, watch};
use tokio::time::{sleep, timeout};

use crate::{debug, log};

const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_L2CAP: libc::c_int = 0;
const BTPROTO_HCI: libc::c_int = 1;
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

#[repr(C)]
struct SockaddrHci {
    family: libc::sa_family_t,
    dev: u16,
    channel: u16,
}

/// HCI Write Default Link Policy Settings: role switch allowed, sniff, hold and park refused.
const WRITE_DEFAULT_LINK_POLICY: [u8; 6] = [0x01, 0x0f, 0x08, 0x02, 0x01, 0x00];

/// Has the adapter refuse sniff mode on new links. The iPad parks an idle link in sniff, where
/// this side may send one report per sniff interval (~38ms here) and the motion queued behind
/// that arrives late. The adapter takes its default back on every power up, so this runs after
/// bluetoothd starts, waiting for hci0 to come up. Needs CAP_NET_RAW.
pub fn no_sniff(dev: u16) -> io::Result<()> {
    let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_RAW | libc::SOCK_CLOEXEC, BTPROTO_HCI) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let sa = SockaddrHci { family: AF_BLUETOOTH as libc::sa_family_t, dev, channel: 0 };
    if unsafe { libc::bind(fd.as_raw_fd(), &sa as *const _ as *const libc::sockaddr, size_of::<SockaddrHci>() as u32) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut tries = 0;
    loop {
        let cmd = &WRITE_DEFAULT_LINK_POLICY;
        let n = unsafe { libc::write(fd.as_raw_fd(), cmd.as_ptr() as *const libc::c_void, cmd.len()) };
        if n >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        tries += 1;
        if err.raw_os_error() != Some(libc::ENETDOWN) || tries == 60 {
            return Err(err);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
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

/// Where reports go: the interrupt channel while the link is up.
#[derive(Clone, Default)]
pub struct Hid {
    chan: Arc<Mutex<Option<Chan>>>,
}

impl Hid {
    /// Sends one input report. A report that cannot go out within 50ms is dropped, so a
    /// congested radio never stalls the input loop.
    pub async fn send(&self, report_id: u8, data: &[u8]) {
        let Some(c) = self.chan.lock().unwrap().clone() else { return };
        let mut msg = Vec::with_capacity(data.len() + 2);
        msg.extend_from_slice(&[DATA_INPUT, report_id]);
        msg.extend_from_slice(data);
        match timeout(Duration::from_millis(50), send(&c, &msg)).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => debug!("report: {err}"),
            Err(_) => debug!("report dropped, link congested"),
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
                    *self.hid.chan.lock().unwrap() = Some(interrupt.clone());
                    self.up.send_replace(true);
                    log!("input link up");
                    let why = serve(&control, &interrupt).await;
                    *self.hid.chan.lock().unwrap() = None;
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
    fn sets_succeed_and_gets_are_refused() {
        assert_eq!(handshake(0x70), Some(0x00));
        assert_eq!(handshake(0x41), Some(0x03));
        assert_eq!(handshake(0x10), None);
    }
}
