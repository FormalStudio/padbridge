//! Everything BlueZ: the adapter, our registrations, the iPad's two links and the pairing agent.
//!
//! The iPad is the LE central. It reconnects to a bonded HID device only when it catches that
//! device advertising, and its background scan has a low duty cycle, so how fast we advertise
//! decides how quickly input comes back. BlueZ's default interval is 1.28s, and this controller
//! skips advertising events while it serves A2DP, which left the link down for hours. So while
//! the link is down we advertise every 20 to 30ms for a while (after a drop, at start, and
//! whenever the user tries to switch), then every ~150ms, the intervals Apple's accessory
//! guidelines give. While it is up we stop, which gives the radio back to the link itself.
//!
//! Connecting out to the iPad (Bearer.LE1.Connect) never works: the iPad does not advertise
//! connectably, so the kernel's accept-list scan waits out its 30s every time, and each attempt
//! toggles our advertising sets off and on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, interval, timeout};
use zbus::message::Type as MsgType;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use crate::gatt::APP_PATH;
use crate::{debug, log};

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const LE: &str = "org.bluez.Bearer.LE1";
const BREDR: &str = "org.bluez.Bearer.BREDR1";
const A2DP_SOURCE: &str = "0000110a-0000-1000-8000-00805f9b34fb";

const ADV_PATH: &str = "/es/canarycoders/padbridge/advertisement0";
const AGENT_PATH: &str = "/es/canarycoders/padbridge/agent";

const FAST_FOR: Duration = Duration::from_secs(30);
const AUDIO_TRIES: u32 = 5;

/// Address of the paired iPad, written at pairing. Its audio link is brought up whenever its input
/// link is.
pub fn ipad_file() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    state.join("padbridge/ipad")
}

fn ipad_addr() -> Option<String> {
    std::fs::read_to_string(ipad_file()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rate {
    Fast,
    Slow,
}

impl Rate {
    /// Milliseconds, the unit of LEAdvertisement1's MinInterval and MaxInterval.
    fn interval(self) -> (u32, u32) {
        match self {
            Rate::Fast => (20, 30),
            Rate::Slow => (150, 160),
        }
    }
}

struct Advertisement {
    name: String,
    discoverable: bool,
    rate: Rate,
}

#[zbus::interface(name = "org.bluez.LEAdvertisement1")]
impl Advertisement {
    #[zbus(property, name = "Type")]
    fn kind(&self) -> &str {
        "peripheral"
    }
    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        vec!["1812".into()]
    }
    #[zbus(property)]
    fn appearance(&self) -> u16 {
        0x03C0
    }
    #[zbus(property)]
    fn local_name(&self) -> &str {
        &self.name
    }
    #[zbus(property)]
    fn discoverable(&self) -> bool {
        self.discoverable
    }
    #[zbus(property)]
    fn min_interval(&self) -> u32 {
        self.rate.interval().0
    }
    #[zbus(property)]
    fn max_interval(&self) -> u32 {
        self.rate.interval().1
    }
    fn release(&self) {
        debug!("advertisement released");
    }
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
        let addr: String = props.get(DEVICE.try_into().unwrap(), "Address").await.map_err(zbus::Error::from)?.try_into()
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
}

pub struct Bt {
    conn: Connection,
    name: String,
    adapter: Option<OwnedObjectPath>,
    /// What we last registered: None while nothing is.
    adv: Option<(Rate, bool)>,
    adv_retry: Instant,
    ipad: Option<OwnedObjectPath>,
    le: bool,
    bredr: bool,
    fast_until: Instant,
    pairing: Arc<AtomicBool>,
    pair_until: Option<Instant>,
    audio_at: Option<Instant>,
    audio_tries: u32,
    audio_busy: bool,
    le_tx: watch::Sender<bool>,
}

impl Bt {
    pub async fn new(conn: Connection, name: String, pair_seconds: u64, le_tx: watch::Sender<bool>) -> zbus::Result<Self> {
        let pairing = Arc::new(AtomicBool::new(pair_seconds > 0));
        let server = conn.object_server();
        server.at(ADV_PATH, Advertisement { name: name.clone(), discoverable: false, rate: Rate::Fast }).await?;
        server.at(AGENT_PATH, Agent { pairing: pairing.clone() }).await?;
        let now = Instant::now();
        Ok(Bt {
            conn,
            name,
            adapter: None,
            adv: None,
            adv_retry: now,
            ipad: None,
            le: false,
            bredr: false,
            fast_until: now + FAST_FOR,
            pair_until: (pair_seconds > 0).then(|| now + Duration::from_secs(pair_seconds)),
            pairing,
            audio_at: None,
            audio_tries: 0,
            audio_busy: false,
            le_tx,
        })
    }

    pub async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) -> zbus::Result<()> {
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
                    Cmd::Wake => {
                        if Instant::now() >= self.fast_until {
                            log!("input link down, advertising fast for the iPad");
                        }
                        self.fast_until = Instant::now() + FAST_FOR;
                        self.reconcile_adv().await;
                    }
                },
                Some(_) = owners.next() => {
                    log!("bluetoothd restarted, registering again");
                    self.forget_registrations();
                    self.set_le(false);
                }
                Some(Ok(msg)) = changes.next() => self.on_changed(&msg).await,
                Some(res) = audio_rx.recv() => self.audio_done(res),
                _ = tick.tick() => {
                    ticks += 1;
                    self.tick(ticks, &audio_tx).await;
                }
            }
        }
    }

    fn forget_registrations(&mut self) {
        self.adapter = None;
        self.adv = None;
        self.ipad = None;
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
        self.reconcile_adv().await;
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
            .find(|(_, i)| {
                let has = |n: &str| i.keys().any(|k| k.as_str() == n);
                has("org.bluez.GattManager1") && has("org.bluez.LEAdvertisingManager1")
            })
            .map(|(p, _)| p.clone())
        else {
            return debug!("no BLE adapter with GATT and advertising support yet");
        };
        let app = ObjectPath::try_from(APP_PATH).unwrap();
        let opts: HashMap<&str, Value> = HashMap::new();
        match self.call(&adapter, "org.bluez.GattManager1", "RegisterApplication", &(app, opts)).await {
            Ok(()) => log!("gatt registered on {adapter}"),
            Err(err) if already(&err) => {}
            Err(err) => return log!("gatt registration failed: {err}"),
        }
        self.adapter = Some(adapter.clone());
        self.adv = None;
        if self.pairing.load(Ordering::SeqCst) {
            self.open_pairing(&adapter).await;
        }
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
        if let Ok(p) = props(&self.conn, adapter.clone()).await
            && let Err(err) = p.set(ADAPTER.try_into().unwrap(), "Pairable", Value::from(true)).await
        {
            log!("pairable: {err}");
        }
        let left = self.pair_until.map(|t| t.saturating_duration_since(Instant::now()).as_secs()).unwrap_or(0);
        log!("pairing open for {left}s: on the iPad, Settings > Bluetooth > {}", self.name);
    }

    async fn close_pairing(&mut self) {
        self.pair_until = None;
        self.pairing.store(false, Ordering::SeqCst);
        let root = OwnedObjectPath::try_from("/org/bluez").unwrap();
        let agent = ObjectPath::try_from(AGENT_PATH).unwrap();
        let _ = self.call(&root, "org.bluez.AgentManager1", "UnregisterAgent", &(&agent,)).await;
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
        let Some(addr) = ipad_addr() else { return };
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
            return self.set_le(false);
        };
        if self.ipad.as_ref() != Some(path) {
            debug!("ipad is {path}");
            self.ipad = Some(path.clone());
        }
        let (le, bredr) = (flag(ifaces, LE, "Connected"), flag(ifaces, BREDR, "Connected"));
        self.set_le(le);
        self.set_bredr(bredr);
    }

    async fn on_changed(&mut self, msg: &zbus::Message) {
        let header = msg.header();
        let Some(path) = header.path() else { return };
        let Ok((iface, changed, _)) = msg.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        else {
            return;
        };
        let connected = changed.get("Connected").and_then(|v| bool::try_from(v).ok());
        if self.ipad.as_ref().is_some_and(|p| p.as_str() == path.as_str()) {
            match (iface.as_str(), connected) {
                (LE, Some(up)) => self.set_le(up),
                (BREDR, Some(up)) => self.set_bredr(up),
                _ => {}
            }
        } else if iface == DEVICE && connected == Some(true) && self.ipad.is_none() {
            // A first connection right after pairing, before the address file was read.
            self.refresh().await;
        }
        if self.adapter.as_ref().is_some_and(|p| p.as_str() == path.as_str())
            && iface == ADAPTER
            && let Some(powered) = changed.get("Powered").and_then(|v| bool::try_from(v).ok())
        {
            log!("adapter powered {}", if powered { "on" } else { "off" });
            // bluetoothd drops advertising instances with the power, so ours goes again.
            self.adv = None;
            if !powered {
                self.set_le(false);
                self.set_bredr(false);
            } else {
                self.fast_until = Instant::now() + FAST_FOR;
            }
            self.reconcile_adv().await;
        }
    }

    fn set_le(&mut self, up: bool) {
        if up == self.le {
            return;
        }
        self.le = up;
        log!("input link {}", if up { "up" } else { "down" });
        self.le_tx.send_replace(up);
        if up {
            self.audio_tries = 0;
            if !self.bredr {
                self.audio_at = Some(Instant::now() + Duration::from_millis(1500));
            }
        } else {
            self.fast_until = Instant::now() + FAST_FOR;
            self.audio_at = None;
        }
    }

    fn set_bredr(&mut self, up: bool) {
        if up == self.bredr {
            return;
        }
        self.bredr = up;
        log!("audio link {}", if up { "up" } else { "down" });
        if up {
            self.audio_at = None;
            self.audio_tries = 0;
        } else if self.le {
            self.audio_at = Some(Instant::now() + Duration::from_secs(3));
        }
    }

    /// iPadOS only sends audio over a classic link the other side opens, as headphones do.
    fn connect_audio(&mut self, tx: &mpsc::UnboundedSender<Result<(), String>>) {
        let Some(path) = self.ipad.clone() else { return };
        if !self.le || self.bredr || self.audio_busy {
            return;
        }
        self.audio_busy = true;
        let conn = self.conn.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let call = conn.call_method(Some(BLUEZ), path.as_str(), Some(DEVICE), "ConnectProfile", &(A2DP_SOURCE,));
            let res = match timeout(Duration::from_secs(30), call).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("timed out".into()),
            };
            let _ = tx.send(res);
        });
    }

    fn audio_done(&mut self, res: Result<(), String>) {
        self.audio_busy = false;
        match res {
            Ok(()) => debug!("audio profile connected"),
            Err(err) => {
                self.audio_tries += 1;
                log!("audio link: {err}");
                if self.le && !self.bredr && self.audio_tries < AUDIO_TRIES {
                    self.audio_at = Some(Instant::now() + Duration::from_secs(5 << (self.audio_tries - 1)));
                }
            }
        }
    }

    async fn reconcile_adv(&mut self) {
        let Some(adapter) = self.adapter.clone() else { return };
        let pairing = self.pairing.load(Ordering::SeqCst);
        let want = if self.le && !pairing {
            None
        } else if pairing || Instant::now() < self.fast_until {
            Some((Rate::Fast, pairing))
        } else {
            Some((Rate::Slow, pairing))
        };
        if want == self.adv || Instant::now() < self.adv_retry {
            return;
        }
        let adv = ObjectPath::try_from(ADV_PATH).unwrap();
        const MANAGER: &str = "org.bluez.LEAdvertisingManager1";
        if self.adv.take().is_some()
            && let Err(err) = self.call(&adapter, MANAGER, "UnregisterAdvertisement", &(&adv,)).await
        {
            debug!("unregister advertisement: {err}");
        }
        let Some((rate, discoverable)) = want else {
            return debug!("advertising stopped");
        };
        if let Ok(iface) = self.conn.object_server().interface::<_, Advertisement>(ADV_PATH).await {
            let mut a = iface.get_mut().await;
            a.rate = rate;
            a.discoverable = discoverable;
        }
        let opts = || HashMap::<&str, Value>::new();
        let mut res = self.call(&adapter, MANAGER, "RegisterAdvertisement", &(&adv, opts())).await;
        if res.as_ref().is_err_and(already) {
            // Still registered from before an adapter power cycle, with the old interval.
            let _ = self.call(&adapter, MANAGER, "UnregisterAdvertisement", &(&adv,)).await;
            res = self.call(&adapter, MANAGER, "RegisterAdvertisement", &(&adv, opts())).await;
        }
        match res {
            Ok(()) => {
                let (lo, hi) = rate.interval();
                debug!("advertising every {lo} to {hi}ms{}", if discoverable { ", discoverable" } else { "" });
                self.adv = want;
            }
            Err(err) => {
                log!("advertising failed: {err}");
                self.adv_retry = Instant::now() + Duration::from_secs(10);
            }
        }
    }
}
