//! Everything BlueZ: the adapter, the HID service record, the iPad's audio link and pairing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Instant, interval, timeout};
use zbus::message::Type as MsgType;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use crate::status::Status;
use crate::{debug, hid, link, log};

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const BREDR: &str = "org.bluez.Bearer.BREDR1";
const A2DP_SOURCE: &str = "0000110a-0000-1000-8000-00805f9b34fb";

const AGENT_PATH: &str = "/es/canarycoders/padbridge/agent";
const PROFILE_PATH: &str = "/es/canarycoders/padbridge/hid";

const AUDIO_TRIES: u32 = 5;

/// Address of the paired iPad, written at pairing.
pub fn ipad_file() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    state.join("padbridge/ipad")
}

fn ipad_addr() -> Option<String> {
    std::fs::read_to_string(ipad_file()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// Just Works pairing, only accepted while the pairing window is open.
struct Agent {
    pairing: Arc<AtomicBool>,
}

impl Agent {
    async fn accept(&self, conn: &Connection, device: &ObjectPath<'_>) -> Result<(), AgentError> {
        if !self.pairing.load(Ordering::SeqCst) {
            return Err(AgentError::Rejected("pairing window closed".into()));
        }
        log!("accepting pairing from {device}");
        let props = props(conn, device.to_owned().into()).await?;
        props.set(DEVICE.try_into().unwrap(), "Trusted", Value::from(true)).await.map_err(zbus::Error::from)?;
        let addr: String = props
            .get(DEVICE.try_into().unwrap(), "Address")
            .await
            .map_err(zbus::Error::from)?
            .try_into()
            .map_err(zbus::Error::from)?;
        let file = ipad_file();
        let written = std::fs::create_dir_all(file.parent().unwrap()).and_then(|_| std::fs::write(&file, format!("{addr}\n")));
        if let Err(err) = written {
            log!("cannot write {}: {err}", file.display());
        }
        Ok(())
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    async fn request_confirmation(
        &self,
        device: ObjectPath<'_>,
        _passkey: u32,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        self.accept(conn, &device).await
    }
    async fn request_authorization(
        &self,
        device: ObjectPath<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), AgentError> {
        self.accept(conn, &device).await
    }
    fn authorize_service(&self, _device: ObjectPath<'_>, _uuid: String) {}
    fn display_passkey(&self, _device: ObjectPath<'_>, passkey: u32, _entered: u16) {
        log!("passkey {passkey:06}");
    }
    fn release(&self) {}
    fn cancel(&self) {}
}

/// Carries the HID service record. Its own channel is never used: the iPad's HID host is
/// reached by connecting out to it.
struct Profile;

#[zbus::interface(name = "org.bluez.Profile1")]
impl Profile {
    fn new_connection(&self, device: ObjectPath<'_>, _fd: zbus::zvariant::OwnedFd, _options: HashMap<String, OwnedValue>) {
        debug!("unused profile channel from {device}");
    }
    fn request_disconnection(&self, _device: ObjectPath<'_>) {}
    fn release(&self) {}
}

async fn props(conn: &Connection, path: OwnedObjectPath) -> zbus::Result<zbus::fdo::PropertiesProxy<'static>> {
    zbus::fdo::PropertiesProxy::builder(conn).destination(BLUEZ)?.path(path)?.build().await
}

type Objects = HashMap<OwnedObjectPath, HashMap<zbus::names::OwnedInterfaceName, HashMap<String, OwnedValue>>>;

async fn managed_objects(conn: &Connection) -> zbus::Result<Objects> {
    let om = zbus::fdo::ObjectManagerProxy::builder(conn).destination(BLUEZ)?.path("/")?.build().await?;
    Ok(om.get_managed_objects().await?)
}

fn flag(ifaces: &HashMap<zbus::names::OwnedInterfaceName, HashMap<String, OwnedValue>>, iface: &str, prop: &str) -> bool {
    ifaces
        .iter()
        .find(|(name, _)| name.as_str() == iface)
        .and_then(|(_, p)| p.get(prop))
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

fn already(err: &zbus::Error) -> bool {
    matches!(err, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.bluez.Error.AlreadyExists")
}

pub enum Cmd {
    /// The user tried to go to the iPad while its input link is down.
    Wake,
    /// Asked for from the shell: bring the input link up and give audio fresh tries.
    Reconnect,
}

pub struct Bt {
    conn: Connection,
    name: String,
    adapter: Option<OwnedObjectPath>,
    ipad: Option<OwnedObjectPath>,
    ipad_tx: watch::Sender<Option<String>>,
    kick: Arc<Notify>,
    hid_up: bool,
    bredr: bool,
    pairing: Arc<AtomicBool>,
    pair_until: Option<Instant>,
    audio_at: Option<Instant>,
    audio_tries: u32,
    audio_busy: bool,
    status: watch::Sender<Status>,
}

impl Bt {
    pub async fn new(
        conn: Connection,
        name: String,
        pair_seconds: u64,
        ipad_tx: watch::Sender<Option<String>>,
        kick: Arc<Notify>,
        status: watch::Sender<Status>,
    ) -> zbus::Result<Self> {
        let pairing = Arc::new(AtomicBool::new(pair_seconds > 0));
        let server = conn.object_server();
        server.at(AGENT_PATH, Agent { pairing: pairing.clone() }).await?;
        server.at(PROFILE_PATH, Profile).await?;
        let now = Instant::now();
        ipad_tx.send_replace(ipad_addr());
        Ok(Bt {
            conn,
            name,
            adapter: None,
            ipad: None,
            ipad_tx,
            kick,
            hid_up: false,
            bredr: false,
            pair_until: (pair_seconds > 0).then(|| now + Duration::from_secs(pair_seconds)),
            pairing,
            audio_at: None,
            audio_tries: 0,
            audio_busy: false,
            status,
        })
    }

    pub async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>, mut hid: watch::Receiver<bool>) -> zbus::Result<()> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let mut owners = dbus.receive_name_owner_changed_with_args(&[(0, BLUEZ)]).await?;
        let rule = MatchRule::builder()
            .msg_type(MsgType::Signal)
            .sender(BLUEZ)?
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .build();
        let mut changes = MessageStream::for_match_rule(rule, &self.conn, Some(64)).await?;
        let (audio_tx, mut audio_rx) = mpsc::unbounded_channel::<Result<(), String>>();
        let mut tick = interval(Duration::from_secs(1));
        let mut ticks: u64 = 0;
        loop {
            tokio::select! {
                Some(cmd) = cmds.recv() => match cmd {
                    Cmd::Wake => self.kick.notify_one(),
                    Cmd::Reconnect => {
                        log!("reconnect asked for");
                        self.kick.notify_one();
                        self.audio_tries = 0;
                        if self.hid_up && !self.audio_busy {
                            self.audio_at = Some(Instant::now());
                        }
                    }
                },
                Ok(()) = hid.changed() => {
                    self.hid_up = *hid.borrow_and_update();
                    if self.hid_up {
                        self.audio_tries = 0;
                        if !self.bredr_audio().await {
                            self.audio_at = Some(Instant::now() + Duration::from_millis(1500));
                        }
                    } else {
                        self.audio_at = None;
                    }
                }
                Some(_) = owners.next() => {
                    log!("bluetoothd restarted, registering again");
                    self.adapter = None;
                    self.ipad = None;
                }
                Some(Ok(msg)) = changes.next() => self.on_changed(&msg),
                Some(res) = audio_rx.recv() => self.audio_done(res),
                _ = tick.tick() => {
                    ticks += 1;
                    self.tick(ticks, &audio_tx).await;
                }
            }
        }
    }

    async fn tick(&mut self, ticks: u64, audio_tx: &mpsc::UnboundedSender<Result<(), String>>) {
        if self.adapter.is_none() {
            self.setup().await;
        }
        // The signals carry every change; this is the net for one missed across a restart.
        if self.adapter.is_some() && (self.ipad.is_none() || ticks.is_multiple_of(5)) {
            self.refresh().await;
        }
        if self.pair_until.is_some_and(|t| Instant::now() >= t) {
            self.close_pairing().await;
        }
        if self.audio_at.is_some_and(|t| Instant::now() >= t) {
            self.audio_at = None;
            self.connect_audio(audio_tx);
        }
    }

    async fn setup(&mut self) {
        let objs = match managed_objects(&self.conn).await {
            Ok(o) => o,
            Err(err) => return debug!("bluez: {err}"),
        };
        let Some(adapter) = objs
            .iter()
            .find(|(_, i)| i.keys().any(|k| k.as_str() == ADAPTER))
            .map(|(p, _)| p.clone())
        else {
            return debug!("no Bluetooth adapter yet");
        };
        let root = OwnedObjectPath::try_from("/org/bluez").unwrap();
        let profile = ObjectPath::try_from(PROFILE_PATH).unwrap();
        let opts: HashMap<&str, Value> = HashMap::from([
            ("ServiceRecord", Value::from(link::sdp_record(&self.name, hid::REPORT_MAP))),
            ("Role", Value::from("server")),
            ("RequireAuthentication", Value::from(true)),
            ("RequireAuthorization", Value::from(false)),
            ("AutoConnect", Value::from(false)),
            // A channel of its own that nothing connects to: the record names PSMs 17 and 19,
            // which bluetoothd's input plugin holds.
            ("PSM", Value::from(0x1011u16)),
        ]);
        match self.call(&root, "org.bluez.ProfileManager1", "RegisterProfile", &(&profile, link::PROFILE_UUID, opts)).await {
            Ok(()) => log!("hid service record registered on {adapter}"),
            Err(err) if already(&err) => {}
            Err(err) => return log!("hid service record failed: {err}"),
        }
        self.adapter = Some(adapter.clone());
        if self.pairing.load(Ordering::SeqCst) {
            self.open_pairing(&adapter).await;
        }
        self.kick.notify_one();
    }

    async fn open_pairing(&mut self, adapter: &OwnedObjectPath) {
        let agent = ObjectPath::try_from(AGENT_PATH).unwrap();
        let root = OwnedObjectPath::try_from("/org/bluez").unwrap();
        let reg = self.call(&root, "org.bluez.AgentManager1", "RegisterAgent", &(&agent, "NoInputNoOutput")).await;
        if let Err(err) = &reg
            && !already(err)
        {
            return log!("agent registration failed: {err}");
        }
        if let Err(err) = self.call(&root, "org.bluez.AgentManager1", "RequestDefaultAgent", &(&agent,)).await {
            log!("default agent: {err}");
        }
        let left = self.pair_until.map(|t| t.saturating_duration_since(Instant::now()).as_secs()).unwrap_or(0);
        if let Ok(p) = props(&self.conn, adapter.clone()).await {
            let iface = || ADAPTER.try_into().unwrap();
            for (key, value) in [
                ("Pairable", Value::from(true)),
                ("DiscoverableTimeout", Value::from(left as u32)),
                ("Discoverable", Value::from(true)),
            ] {
                if let Err(err) = p.set(iface(), key, value).await {
                    log!("{key}: {err}");
                }
            }
        }
        log!("pairing open for {left}s: on the iPad, Settings > Bluetooth > {}", self.name);
    }

    async fn close_pairing(&mut self) {
        self.pair_until = None;
        self.pairing.store(false, Ordering::SeqCst);
        let root = OwnedObjectPath::try_from("/org/bluez").unwrap();
        let agent = ObjectPath::try_from(AGENT_PATH).unwrap();
        let _ = self.call(&root, "org.bluez.AgentManager1", "UnregisterAgent", &(&agent,)).await;
        if let Some(adapter) = self.adapter.clone()
            && let Ok(p) = props(&self.conn, adapter).await
        {
            let _ = p.set(ADAPTER.try_into().unwrap(), "Discoverable", Value::from(false)).await;
        }
        self.ipad_tx.send_replace(ipad_addr());
        log!("pairing closed");
    }

    async fn call<B>(&self, path: &OwnedObjectPath, iface: &str, method: &str, body: &B) -> zbus::Result<()>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let call = self.conn.call_method(Some(BLUEZ), path.as_str(), Some(iface), method, body);
        match timeout(Duration::from_secs(10), call).await {
            Ok(r) => r.map(|_| ()),
            Err(_) => Err(zbus::Error::Failure(format!("{method} timed out"))),
        }
    }

    async fn refresh(&mut self) {
        let addr = ipad_addr();
        self.ipad_tx.send_if_modified(|a| {
            let changed = *a != addr;
            *a = addr.clone();
            changed
        });
        let Some(addr) = addr else {
            self.status.send_if_modified(|s| {
                let was = (s.paired, s.audio);
                (s.paired, s.audio) = (false, false);
                s.name.clear();
                was != (false, false)
            });
            return;
        };
        let objs = match managed_objects(&self.conn).await {
            Ok(o) => o,
            Err(err) => return debug!("bluez: {err}"),
        };
        let found = objs.iter().find(|(_, i)| {
            i.iter().any(|(n, p)| {
                n.as_str() == DEVICE && p.get("Address").and_then(|v| <&str>::try_from(v).ok()) == Some(addr.as_str())
            })
        });
        let Some((path, ifaces)) = found else {
            self.ipad = None;
            return;
        };
        if self.ipad.as_ref() != Some(path) {
            debug!("ipad is {path}");
            self.ipad = Some(path.clone());
        }
        let bredr = flag(ifaces, BREDR, "Connected");
        self.set_bredr(bredr);
        let name = ifaces
            .iter()
            .find(|(n, _)| n.as_str() == DEVICE)
            .and_then(|(_, p)| p.get("Alias"))
            .and_then(|v| <&str>::try_from(v).ok())
            .unwrap_or_default()
            .to_string();
        let audio = self.bredr_audio().await;
        self.status.send_if_modified(|s| {
            let changed = !s.paired || s.audio != audio || s.name != name;
            (s.paired, s.audio, s.name) = (true, audio, name);
            changed
        });
        // Audio that dropped while input stayed up comes back too, a few tries per input link.
        if self.hid_up && self.audio_at.is_none() && !self.audio_busy && self.audio_tries < AUDIO_TRIES && !audio {
            self.audio_at = Some(Instant::now());
        }
    }

    /// Whether the A2DP stream is up, which is what `bredr` stands for once input rides the
    /// same link: the link itself is up whenever input is.
    async fn bredr_audio(&self) -> bool {
        let Some(path) = self.ipad.clone() else { return false };
        let Ok(objs) = managed_objects(&self.conn).await else { return false };
        objs.iter().any(|(p, i)| {
            p.as_str().starts_with(path.as_str()) && i.keys().any(|k| k.as_str() == "org.bluez.MediaTransport1")
        })
    }

    fn on_changed(&mut self, msg: &zbus::Message) {
        let header = msg.header();
        let Some(path) = header.path() else { return };
        let Ok((iface, changed, _)) = msg.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        else {
            return;
        };
        if self.ipad.as_ref().is_some_and(|p| p.as_str() == path.as_str())
            && iface == BREDR
            && let Some(up) = changed.get("Connected").and_then(|v| bool::try_from(v).ok())
        {
            self.set_bredr(up);
        }
        if self.adapter.as_ref().is_some_and(|p| p.as_str() == path.as_str())
            && iface == ADAPTER
            && changed.get("Powered").and_then(|v| bool::try_from(v).ok()) == Some(true)
        {
            log!("adapter powered on");
            self.kick.notify_one();
        }
    }

    fn set_bredr(&mut self, up: bool) {
        if up == self.bredr {
            return;
        }
        self.bredr = up;
        debug!("classic link {}", if up { "up" } else { "down" });
        if up {
            // The iPad came back on its own, for audio: bring input up over the same link now.
            self.kick.notify_one();
        }
    }

    /// iPadOS only sends audio over a link the other side opens, as headphones do.
    fn connect_audio(&mut self, tx: &mpsc::UnboundedSender<Result<(), String>>) {
        let Some(path) = self.ipad.clone() else { return };
        if !self.hid_up || self.audio_busy {
            return;
        }
        self.audio_busy = true;
        let conn = self.conn.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let call = conn.call_method(Some(BLUEZ), path.as_str(), Some(DEVICE), "ConnectProfile", &(A2DP_SOURCE,));
            let res = match timeout(Duration::from_secs(30), call).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(zbus::Error::MethodError(name, _, _))) if name.as_str() == "org.bluez.Error.AlreadyConnected" => Ok(()),
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("timed out".into()),
            };
            let _ = tx.send(res);
        });
    }

    fn audio_done(&mut self, res: Result<(), String>) {
        self.audio_busy = false;
        match res {
            Ok(()) => {
                log!("audio link up");
                self.status.send_if_modified(|s| !std::mem::replace(&mut s.audio, true));
            }
            Err(err) => {
                self.audio_tries += 1;
                log!("audio link: {err}");
                if self.hid_up && self.audio_tries < AUDIO_TRIES {
                    self.audio_at = Some(Instant::now() + Duration::from_secs(5 << (self.audio_tries - 1)));
                }
            }
        }
    }
}
