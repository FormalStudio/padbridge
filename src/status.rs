//! What padbridge is doing, published on the session bus for the shell, and the commands the
//! shell sends back.

use tokio::sync::{mpsc, watch};
use zbus::object_server::SignalEmitter;

use crate::bridge::{Edge, Position};
use crate::log;

pub const NAME: &str = "studio.formal.Padbridge";
pub const PATH: &str = "/studio/formal/Padbridge";

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    /// An iPad is paired, so there is something to connect to.
    pub paired: bool,
    /// The input link is up.
    pub link: bool,
    /// The iPad's audio plays here.
    pub audio: bool,
    /// The iPad's Bluetooth name, empty until BlueZ knows it.
    pub name: String,
    /// Input goes to the iPad right now.
    pub active: bool,
    pub position: Position,
}

impl Status {
    pub fn new(position: Position) -> Self {
        Status { paired: false, link: false, audio: false, name: String::new(), active: false, position }
    }

    fn link_state(&self) -> &'static str {
        match (self.paired, self.link) {
            (false, _) => "unpaired",
            (true, false) => "down",
            (true, true) => "up",
        }
    }
}

pub enum Ctl {
    SetPosition(Position),
    Switch,
    Reconnect,
}

struct Iface {
    status: Status,
    ctl: mpsc::UnboundedSender<Ctl>,
}

#[zbus::interface(name = "studio.formal.Padbridge1")]
impl Iface {
    /// `unpaired`, `down` or `up`.
    #[zbus(property)]
    fn link(&self) -> &str {
        self.status.link_state()
    }

    #[zbus(property)]
    fn active(&self) -> bool {
        self.status.active
    }

    #[zbus(property)]
    fn audio(&self) -> bool {
        self.status.audio
    }

    #[zbus(property)]
    fn name(&self) -> &str {
        &self.status.name
    }

    /// `left`, `right`, `top` or `bottom` of this desktop.
    #[zbus(property)]
    fn edge(&self) -> &str {
        self.status.position.edge.name()
    }

    /// Where along the edge the iPad starts, 0 to 1 from its top or left end.
    #[zbus(property)]
    fn start(&self) -> f64 {
        self.status.position.start
    }

    #[zbus(property)]
    fn end(&self) -> f64 {
        self.status.position.end
    }

    fn set_position(&self, edge: &str, start: f64, end: f64) -> zbus::fdo::Result<()> {
        let edge = Edge::parse(edge).ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("unknown edge {edge}")))?;
        let pos = Position::new(edge, start, end)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("start and end must satisfy 0 <= start < end <= 1".into()))?;
        self.send(Ctl::SetPosition(pos))
    }

    /// Sends input to the iPad, or back here when it is already there.
    fn switch(&self) -> zbus::fdo::Result<()> {
        self.send(Ctl::Switch)
    }

    fn reconnect(&self) -> zbus::fdo::Result<()> {
        self.send(Ctl::Reconnect)
    }
}

impl Iface {
    fn send(&self, c: Ctl) -> zbus::fdo::Result<()> {
        self.ctl.send(c).map_err(|_| zbus::fdo::Error::Failed("padbridge is stopping".into()))
    }

    async fn update(&mut self, new: Status, em: &SignalEmitter<'_>) -> zbus::Result<()> {
        let old = std::mem::replace(&mut self.status, new);
        let new = &self.status;
        if old.link_state() != new.link_state() {
            self.link_changed(em).await?;
        }
        if old.active != new.active {
            self.active_changed(em).await?;
        }
        if old.audio != new.audio {
            self.audio_changed(em).await?;
        }
        if old.name != new.name {
            self.name_changed(em).await?;
        }
        if old.position.edge != new.position.edge {
            self.edge_changed(em).await?;
        }
        if old.position.start != new.position.start {
            self.start_changed(em).await?;
        }
        if old.position.end != new.position.end {
            self.end_changed(em).await?;
        }
        Ok(())
    }
}

/// Owns the name on the session bus and keeps the properties in step with `status`. Without a
/// session bus padbridge still runs; the shell just cannot see it.
pub async fn serve(mut status: watch::Receiver<Status>, ctl: mpsc::UnboundedSender<Ctl>) {
    let iface = Iface { status: status.borrow_and_update().clone(), ctl };
    let built = async { zbus::connection::Builder::session()?.name(NAME)?.serve_at(PATH, iface)?.build().await }.await;
    let conn = match built {
        Ok(c) => c,
        Err(err) => return log!("session bus: {err}, the shell will not see padbridge"),
    };
    let Ok(iref) = conn.object_server().interface::<_, Iface>(PATH).await else { return };
    while status.changed().await.is_ok() {
        let new = status.borrow_and_update().clone();
        let em = iref.signal_emitter();
        if let Err(err) = iref.get_mut().await.update(new, em).await {
            log!("session bus: {err}");
        }
    }
}
