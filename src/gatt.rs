//! The GATT application BlueZ serves for us: HID over GATT plus a battery service.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::Connection;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::hid;

pub const APP_PATH: &str = "/es/canarycoders/padbridge";
const CHRC: &str = "org.bluez.GattCharacteristic1";

fn path(s: String) -> OwnedObjectPath {
    OwnedObjectPath::try_from(s).expect("static object path")
}

struct Service {
    uuid: &'static str,
    chars: Vec<OwnedObjectPath>,
}

#[zbus::interface(name = "org.bluez.GattService1")]
impl Service {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        self.uuid
    }
    #[zbus(property)]
    fn primary(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn characteristics(&self) -> Vec<OwnedObjectPath> {
        self.chars.clone()
    }
}

struct Characteristic {
    uuid: &'static str,
    service: OwnedObjectPath,
    flags: &'static [&'static str],
    descriptors: Vec<OwnedObjectPath>,
    value: Arc<Mutex<Vec<u8>>>,
}

fn offset(options: &HashMap<String, OwnedValue>) -> usize {
    options.get("offset").and_then(|v| u16::try_from(v).ok()).unwrap_or(0) as usize
}

#[zbus::interface(name = "org.bluez.GattCharacteristic1")]
impl Characteristic {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        self.uuid
    }
    #[zbus(property)]
    fn service(&self) -> OwnedObjectPath {
        self.service.clone()
    }
    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        self.flags.iter().map(|s| s.to_string()).collect()
    }
    #[zbus(property)]
    fn descriptors(&self) -> Vec<OwnedObjectPath> {
        self.descriptors.clone()
    }

    fn read_value(&self, options: HashMap<String, OwnedValue>) -> Vec<u8> {
        let v = self.value.lock().unwrap();
        v.get(offset(&options).min(v.len())..).unwrap_or_default().to_vec()
    }

    fn write_value(&self, value: Vec<u8>, _options: HashMap<String, OwnedValue>) {
        *self.value.lock().unwrap() = value;
    }

    fn start_notify(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) {
        crate::log!("notify on {} {}", self.uuid, e.path());
    }

    fn stop_notify(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) {
        crate::debug!("notify off {} {}", self.uuid, e.path());
    }
}

struct Descriptor {
    uuid: &'static str,
    chrc: OwnedObjectPath,
    value: Vec<u8>,
}

#[zbus::interface(name = "org.bluez.GattDescriptor1")]
impl Descriptor {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> &str {
        self.uuid
    }
    #[zbus(property)]
    fn characteristic(&self) -> OwnedObjectPath {
        self.chrc.clone()
    }
    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["read".into()]
    }

    fn read_value(&self, _options: HashMap<String, OwnedValue>) -> Vec<u8> {
        self.value.clone()
    }
}

/// One input report characteristic. BlueZ only calls StartNotify for the first subscriber and
/// keeps a bonded device's subscription across disconnects without calling StopNotify, so it
/// decides who a notification reaches.
#[derive(Clone)]
pub struct Report {
    path: OwnedObjectPath,
    value: Arc<Mutex<Vec<u8>>>,
}

#[derive(Clone)]
pub struct Reports {
    conn: Connection,
    pub keyboard: Report,
    pub mouse: Report,
    pub consumer: Report,
}

impl Reports {
    pub async fn send(&self, r: &Report, bytes: &[u8]) {
        *r.value.lock().unwrap() = bytes.to_vec();
        let changed = HashMap::from([("Value", Value::from(bytes.to_vec()))]);
        let body = (CHRC, changed, Vec::<&str>::new());
        if let Err(err) = self
            .conn
            .emit_signal(None::<&str>, &r.path, "org.freedesktop.DBus.Properties", "PropertiesChanged", &body)
            .await
        {
            crate::log!("notify failed: {err}");
        }
    }
}

struct Builder<'a> {
    conn: &'a Connection,
}

impl Builder<'_> {
    async fn chrc(
        &self,
        service: &str,
        index: usize,
        uuid: &'static str,
        flags: &'static [&'static str],
        value: &[u8],
        descriptor: Option<Vec<u8>>,
    ) -> zbus::Result<Report> {
        let p = path(format!("{service}/char{index}"));
        let mut descriptors = Vec::new();
        if let Some(v) = descriptor {
            let d = path(format!("{}/desc0", p.as_str()));
            // 0x2908 is the Report Reference descriptor: report id and type.
            let desc = Descriptor { uuid: "2908", chrc: p.clone(), value: v };
            self.conn.object_server().at(&d, desc).await?;
            descriptors.push(d);
        }
        let value = Arc::new(Mutex::new(value.to_vec()));
        let c = Characteristic { uuid, service: path(service.into()), flags, descriptors, value: value.clone() };
        self.conn.object_server().at(&p, c).await?;
        Ok(Report { path: p, value })
    }
}

pub async fn serve(conn: &Connection) -> zbus::Result<Reports> {
    let server = conn.object_server();
    server.at(APP_PATH, zbus::fdo::ObjectManager).await?;
    let b = Builder { conn };

    let hid_path = format!("{APP_PATH}/service0");
    const REPORT: &[&str] = &["read", "notify", "encrypt-read", "encrypt-notify"];
    // HID 1.11, normally connectable.
    b.chrc(&hid_path, 0, "2A4A", &["read"], &[0x11, 0x01, 0x00, 0x02], None).await?;
    b.chrc(&hid_path, 1, "2A4B", &["read", "encrypt-read"], hid::REPORT_MAP, None).await?;
    b.chrc(&hid_path, 2, "2A4C", &["write-without-response"], &[0], None).await?;
    // Report protocol.
    b.chrc(&hid_path, 3, "2A4E", &["read", "write-without-response"], &[1], None).await?;
    let keyboard = b.chrc(&hid_path, 4, "2A4D", REPORT, &[0; hid::KBD_LEN], Some(vec![hid::KBD_ID, 1])).await?;
    let mouse = b.chrc(&hid_path, 5, "2A4D", REPORT, &[0; hid::MOUSE_LEN], Some(vec![hid::MOUSE_ID, 1])).await?;
    let consumer =
        b.chrc(&hid_path, 6, "2A4D", REPORT, &[0; hid::CONSUMER_LEN], Some(vec![hid::CONSUMER_ID, 1])).await?;
    let chars = (0..7).map(|i| path(format!("{hid_path}/char{i}"))).collect();
    server.at(hid_path.as_str(), Service { uuid: "1812", chars }).await?;

    let bat_path = format!("{APP_PATH}/service1");
    b.chrc(&bat_path, 0, "2A19", &["read", "notify"], &[100], None).await?;
    server.at(bat_path.as_str(), Service { uuid: "180F", chars: vec![path(format!("{bat_path}/char0"))] }).await?;

    Ok(Reports { conn: conn.clone(), keyboard, mouse, consumer })
}
