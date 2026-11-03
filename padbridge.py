#!/usr/bin/python3.13
"""Share this machine's mouse and keyboard with an iPad by acting as a BLE HID device.

Push the pointer past the configured edge of the Hyprland desktop and input goes to
the iPad; push back past the iPad's opposite edge (or press the toggle chord) and it
comes home.
"""

import argparse
import json
import os
import signal
import socket
import sys
import time
import tomllib
from pathlib import Path

import dbus
import dbus.mainloop.glib
import dbus.service
import evdev
from evdev import ecodes as e
from gi.repository import GLib, GLibUnix

BLUEZ = "org.bluez"
GATT_MANAGER = "org.bluez.GattManager1"
AD_MANAGER = "org.bluez.LEAdvertisingManager1"
AGENT_MANAGER = "org.bluez.AgentManager1"
OM = "org.freedesktop.DBus.ObjectManager"
PROPS = "org.freedesktop.DBus.Properties"
GATT_SERVICE = "org.bluez.GattService1"
GATT_CHRC = "org.bluez.GattCharacteristic1"
GATT_DESC = "org.bluez.GattDescriptor1"

APP_PATH = "/es/canarycoders/padbridge"

KBD_ID, MOUSE_ID, CONSUMER_ID = 1, 2, 3

# fmt: off
REPORT_MAP = bytes([
    # Keyboard: modifiers, reserved, 6 keys
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, KBD_ID,
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x05, 0x07, 0x19, 0x00, 0x29, 0xFF, 0x81, 0x00,
    0xC0,
    # Mouse: 5 buttons, 16-bit X/Y, wheel, horizontal pan
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, MOUSE_ID, 0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x06,
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0xC0, 0xC0,
    # Consumer control: one 16-bit usage
    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, CONSUMER_ID,
    0x15, 0x00, 0x26, 0xFF, 0x03, 0x19, 0x00, 0x2A, 0xFF, 0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00,
    0xC0,
])

MODIFIERS = {
    e.KEY_LEFTCTRL: 0x01, e.KEY_LEFTSHIFT: 0x02, e.KEY_LEFTALT: 0x04, e.KEY_LEFTMETA: 0x08,
    e.KEY_RIGHTCTRL: 0x10, e.KEY_RIGHTSHIFT: 0x20, e.KEY_RIGHTALT: 0x40, e.KEY_RIGHTMETA: 0x80,
}

# evdev keycode to HID keyboard usage (inverse of the kernel's hid-input table)
KEYS = {
    e.KEY_A: 0x04, e.KEY_B: 0x05, e.KEY_C: 0x06, e.KEY_D: 0x07, e.KEY_E: 0x08, e.KEY_F: 0x09,
    e.KEY_G: 0x0A, e.KEY_H: 0x0B, e.KEY_I: 0x0C, e.KEY_J: 0x0D, e.KEY_K: 0x0E, e.KEY_L: 0x0F,
    e.KEY_M: 0x10, e.KEY_N: 0x11, e.KEY_O: 0x12, e.KEY_P: 0x13, e.KEY_Q: 0x14, e.KEY_R: 0x15,
    e.KEY_S: 0x16, e.KEY_T: 0x17, e.KEY_U: 0x18, e.KEY_V: 0x19, e.KEY_W: 0x1A, e.KEY_X: 0x1B,
    e.KEY_Y: 0x1C, e.KEY_Z: 0x1D,
    e.KEY_1: 0x1E, e.KEY_2: 0x1F, e.KEY_3: 0x20, e.KEY_4: 0x21, e.KEY_5: 0x22,
    e.KEY_6: 0x23, e.KEY_7: 0x24, e.KEY_8: 0x25, e.KEY_9: 0x26, e.KEY_0: 0x27,
    e.KEY_ENTER: 0x28, e.KEY_ESC: 0x29, e.KEY_BACKSPACE: 0x2A, e.KEY_TAB: 0x2B, e.KEY_SPACE: 0x2C,
    e.KEY_MINUS: 0x2D, e.KEY_EQUAL: 0x2E, e.KEY_LEFTBRACE: 0x2F, e.KEY_RIGHTBRACE: 0x30,
    e.KEY_BACKSLASH: 0x31, e.KEY_SEMICOLON: 0x33, e.KEY_APOSTROPHE: 0x34, e.KEY_GRAVE: 0x35,
    e.KEY_COMMA: 0x36, e.KEY_DOT: 0x37, e.KEY_SLASH: 0x38, e.KEY_CAPSLOCK: 0x39,
    e.KEY_F1: 0x3A, e.KEY_F2: 0x3B, e.KEY_F3: 0x3C, e.KEY_F4: 0x3D, e.KEY_F5: 0x3E, e.KEY_F6: 0x3F,
    e.KEY_F7: 0x40, e.KEY_F8: 0x41, e.KEY_F9: 0x42, e.KEY_F10: 0x43, e.KEY_F11: 0x44, e.KEY_F12: 0x45,
    e.KEY_SYSRQ: 0x46, e.KEY_SCROLLLOCK: 0x47, e.KEY_PAUSE: 0x48, e.KEY_INSERT: 0x49,
    e.KEY_HOME: 0x4A, e.KEY_PAGEUP: 0x4B, e.KEY_DELETE: 0x4C, e.KEY_END: 0x4D, e.KEY_PAGEDOWN: 0x4E,
    e.KEY_RIGHT: 0x4F, e.KEY_LEFT: 0x50, e.KEY_DOWN: 0x51, e.KEY_UP: 0x52,
    e.KEY_NUMLOCK: 0x53, e.KEY_KPSLASH: 0x54, e.KEY_KPASTERISK: 0x55, e.KEY_KPMINUS: 0x56,
    e.KEY_KPPLUS: 0x57, e.KEY_KPENTER: 0x58, e.KEY_KP1: 0x59, e.KEY_KP2: 0x5A, e.KEY_KP3: 0x5B,
    e.KEY_KP4: 0x5C, e.KEY_KP5: 0x5D, e.KEY_KP6: 0x5E, e.KEY_KP7: 0x5F, e.KEY_KP8: 0x60,
    e.KEY_KP9: 0x61, e.KEY_KP0: 0x62, e.KEY_KPDOT: 0x63, e.KEY_102ND: 0x64, e.KEY_COMPOSE: 0x65,
    e.KEY_KPEQUAL: 0x67, e.KEY_F13: 0x68, e.KEY_F14: 0x69, e.KEY_F15: 0x6A,
    e.KEY_RO: 0x87, e.KEY_YEN: 0x89,
}

CONSUMER = {
    e.KEY_PLAYPAUSE: 0x00CD, e.KEY_NEXTSONG: 0x00B5, e.KEY_PREVIOUSSONG: 0x00B6,
    e.KEY_MUTE: 0x00E2, e.KEY_VOLUMEUP: 0x00E9, e.KEY_VOLUMEDOWN: 0x00EA,
    e.KEY_BRIGHTNESSUP: 0x006F, e.KEY_BRIGHTNESSDOWN: 0x0070,
}

BUTTONS = {e.BTN_LEFT: 0x01, e.BTN_RIGHT: 0x02, e.BTN_MIDDLE: 0x04, e.BTN_SIDE: 0x08, e.BTN_EXTRA: 0x10}
# fmt: on

DEFAULTS = {
    "edge": "left",  # side of the desktop the iPad sits on
    "ipad_width": 1800,  # mouse counts to cross the iPad, used to detect the return edge
    "sensitivity": 1.0,
    "edge_push": 40,  # counts past the edge before switching, stops accidental crossings
    "toggle": ["KEY_LEFTCTRL", "KEY_LEFTALT", "KEY_I"],
    "name": "g815",
}


def log(*a):
    print(*a, flush=True)


class Hyprland:
    def __init__(self):
        runtime = os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
        sig = os.environ.get("HYPRLAND_INSTANCE_SIGNATURE")
        base = Path(runtime) / "hypr"
        if not sig:
            sig = max(base.iterdir(), key=lambda p: p.stat().st_mtime).name
        self.sock = str(base / sig / ".socket.sock")

    def _req(self, cmd):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.connect(self.sock)
            s.sendall(cmd.encode())
            out = b""
            while chunk := s.recv(65536):
                out += chunk
        return out.decode()

    def cursor(self):
        p = json.loads(self._req("j/cursorpos"))
        return p["x"], p["y"]

    def monitors(self):
        return [m for m in json.loads(self._req("j/monitors")) if not m.get("disabled")]

    def warp(self, x, y):
        self._req(f"dispatch movecursor {int(x)} {int(y)}")


# --- BlueZ GATT application -------------------------------------------------


class Application(dbus.service.Object):
    def __init__(self, bus):
        self.path = APP_PATH
        self.services = []
        super().__init__(bus, self.path)

    @dbus.service.method(OM, out_signature="a{oa{sa{sv}}}")
    def GetManagedObjects(self):
        out = {}
        for s in self.services:
            out[s.get_path()] = s.get_properties()
            for c in s.characteristics:
                out[c.get_path()] = c.get_properties()
                for d in c.descriptors:
                    out[d.get_path()] = d.get_properties()
        return out


class Service(dbus.service.Object):
    def __init__(self, bus, index, uuid):
        self.path = f"{APP_PATH}/service{index}"
        self.uuid = uuid
        self.characteristics = []
        self.bus = bus
        super().__init__(bus, self.path)

    def get_path(self):
        return dbus.ObjectPath(self.path)

    def get_properties(self):
        return {
            GATT_SERVICE: {
                "UUID": self.uuid,
                "Primary": True,
                "Characteristics": dbus.Array([c.get_path() for c in self.characteristics], signature="o"),
            }
        }

    def add(self, uuid, flags, value=b""):
        c = Characteristic(self.bus, len(self.characteristics), self, uuid, flags, value)
        self.characteristics.append(c)
        return c


class Characteristic(dbus.service.Object):
    def __init__(self, bus, index, service, uuid, flags, value):
        self.path = f"{service.path}/char{index}"
        self.service = service
        self.uuid = uuid
        self.flags = flags
        self.value = bytes(value)
        self.descriptors = []
        self.notifying = False
        self.bus = bus
        super().__init__(bus, self.path)

    def get_path(self):
        return dbus.ObjectPath(self.path)

    def get_properties(self):
        return {
            GATT_CHRC: {
                "Service": self.service.get_path(),
                "UUID": self.uuid,
                "Flags": self.flags,
                "Descriptors": dbus.Array([d.get_path() for d in self.descriptors], signature="o"),
            }
        }

    def add_descriptor(self, uuid, flags, value):
        d = Descriptor(self.bus, len(self.descriptors), self, uuid, flags, value)
        self.descriptors.append(d)
        return d

    @dbus.service.method(GATT_CHRC, in_signature="a{sv}", out_signature="ay")
    def ReadValue(self, options):
        off = int(options.get("offset", 0))
        return dbus.Array(self.value[off:], signature="y")

    @dbus.service.method(GATT_CHRC, in_signature="aya{sv}")
    def WriteValue(self, value, options):
        self.value = bytes(value)

    @dbus.service.method(GATT_CHRC)
    def StartNotify(self):
        self.notifying = True
        log(f"notify on {self.uuid} {self.path}")

    @dbus.service.method(GATT_CHRC)
    def StopNotify(self):
        self.notifying = False

    @dbus.service.signal(PROPS, signature="sa{sv}as")
    def PropertiesChanged(self, interface, changed, invalidated):
        pass

    def notify(self, value):
        self.value = bytes(value)
        if self.notifying:
            self.PropertiesChanged(GATT_CHRC, {"Value": dbus.Array(self.value, signature="y")}, [])


class Descriptor(dbus.service.Object):
    def __init__(self, bus, index, chrc, uuid, flags, value):
        self.path = f"{chrc.path}/desc{index}"
        self.chrc = chrc
        self.uuid = uuid
        self.flags = flags
        self.value = bytes(value)
        super().__init__(bus, self.path)

    def get_path(self):
        return dbus.ObjectPath(self.path)

    def get_properties(self):
        return {GATT_DESC: {"Characteristic": self.chrc.get_path(), "UUID": self.uuid, "Flags": self.flags}}

    @dbus.service.method(GATT_DESC, in_signature="a{sv}", out_signature="ay")
    def ReadValue(self, options):
        return dbus.Array(self.value, signature="y")


class Advertisement(dbus.service.Object):
    def __init__(self, bus, name, discoverable):
        self.path = f"{APP_PATH}/advertisement0"
        self.name = name
        self.discoverable = discoverable
        super().__init__(bus, self.path)

    @dbus.service.method(PROPS, in_signature="s", out_signature="a{sv}")
    def GetAll(self, interface):
        return {
            "Type": "peripheral",
            "ServiceUUIDs": dbus.Array(["1812"], signature="s"),
            "Appearance": dbus.UInt16(0x03C0),
            "LocalName": self.name,
            "Discoverable": dbus.Boolean(self.discoverable),
        }

    @dbus.service.method("org.bluez.LEAdvertisement1")
    def Release(self):
        pass


class Agent(dbus.service.Object):
    """Just Works pairing, only accepted while the pairing window is open."""

    def __init__(self, bus, bridge):
        self.bridge = bridge
        super().__init__(bus, f"{APP_PATH}/agent")

    def _check(self, device):
        if not self.bridge.pairing:
            raise dbus.DBusException("pairing window closed", name="org.bluez.Error.Rejected")
        log(f"accepting pairing from {device}")

    @dbus.service.method("org.bluez.Agent1", in_signature="ou")
    def RequestConfirmation(self, device, passkey):
        self._check(device)

    @dbus.service.method("org.bluez.Agent1", in_signature="o")
    def RequestAuthorization(self, device):
        self._check(device)

    @dbus.service.method("org.bluez.Agent1", in_signature="os")
    def AuthorizeService(self, device, uuid):
        pass

    @dbus.service.method("org.bluez.Agent1", in_signature="ouq")
    def DisplayPasskey(self, device, passkey, entered):
        log(f"passkey {passkey:06d}")

    @dbus.service.method("org.bluez.Agent1")
    def Release(self):
        pass

    @dbus.service.method("org.bluez.Agent1")
    def Cancel(self):
        pass


def build_gatt(bus):
    app = Application(bus)
    hid = Service(bus, 0, "1812")
    hid.add("2A4A", ["read"], [0x11, 0x01, 0x00, 0x02])  # HID 1.11, normally connectable
    hid.add("2A4B", ["read", "encrypt-read"], REPORT_MAP)
    hid.add("2A4C", ["write-without-response"], [0])
    hid.add("2A4E", ["read", "write-without-response"], [1])  # report protocol
    reports = {}
    for rid, size in ((KBD_ID, 8), (MOUSE_ID, 7), (CONSUMER_ID, 2)):
        c = hid.add("2A4D", ["read", "notify", "encrypt-read", "encrypt-notify"], bytes(size))
        c.add_descriptor("2908", ["read"], [rid, 0x01])  # report id, input report
        reports[rid] = c
    battery = Service(bus, 1, "180F")
    battery.add("2A19", ["read", "notify"], [100])
    app.services = [hid, battery]
    return app, reports


# --- input capture ----------------------------------------------------------


class Bridge:
    def __init__(self, cfg, bus, adapter, pair_seconds):
        self.cfg = cfg
        self.bus = bus
        self.adapter = adapter
        self.hypr = Hyprland()
        self.app, self.reports = build_gatt(bus)
        self.on_ipad = False
        self.pending_toggle = False
        self.devices = {}  # path -> (InputDevice, watch id)
        self.keys = set()
        self.mods = 0
        self.buttons = 0
        self.dx = self.dy = 0
        self.wheel = self.pan = 0
        self.flush_pending = False
        self.vx = 0.0
        self.edge_push = 0
        self.last_query = 0.0
        self.monitors = []
        self.toggle = {e.ecodes[k] for k in cfg["toggle"]}
        self.pairing = pair_seconds > 0
        self.pair_seconds = pair_seconds

    # bluetooth

    def start_bluetooth(self):
        gm = dbus.Interface(self.bus.get_object(BLUEZ, self.adapter), GATT_MANAGER)
        gm.RegisterApplication(self.app.path, {}, reply_handler=lambda: log("gatt registered"),
                               error_handler=self.fatal)
        self.adv = Advertisement(self.bus, self.cfg["name"], self.pairing)
        am = dbus.Interface(self.bus.get_object(BLUEZ, self.adapter), AD_MANAGER)
        am.RegisterAdvertisement(self.adv.path, {}, reply_handler=lambda: log("advertising"),
                                 error_handler=self.fatal)
        if self.pairing:
            self.agent = Agent(self.bus, self)
            agm = dbus.Interface(self.bus.get_object(BLUEZ, "/org/bluez"), AGENT_MANAGER)
            agm.RegisterAgent(self.agent._object_path, "NoInputNoOutput")
            agm.RequestDefaultAgent(self.agent._object_path)
            props = dbus.Interface(self.bus.get_object(BLUEZ, self.adapter), PROPS)
            props.Set("org.bluez.Adapter1", "Pairable", True)
            log(f"pairing open for {self.pair_seconds}s: on the iPad, Settings > Bluetooth > {self.cfg['name']}")
            GLib.timeout_add_seconds(self.pair_seconds, self.close_pairing)

    def close_pairing(self):
        self.pairing = False
        agm = dbus.Interface(self.bus.get_object(BLUEZ, "/org/bluez"), AGENT_MANAGER)
        agm.UnregisterAgent(self.agent._object_path)
        log("pairing closed")
        return False

    def fatal(self, err):
        log(f"bluez error: {err}")
        GLib.idle_add(lambda: sys.exit(1))

    # devices

    def scan(self):
        for path in evdev.list_devices():
            if path in self.devices:
                continue
            try:
                d = evdev.InputDevice(path)
            except OSError:
                continue
            caps = d.capabilities()
            keys = set(caps.get(e.EV_KEY, []))
            is_mouse = e.REL_X in caps.get(e.EV_REL, [])
            is_kbd = e.KEY_A in keys and e.KEY_SPACE in keys
            if not (is_mouse or is_kbd) or "padbridge" in d.name:
                d.close()
                continue
            wid = GLib.io_add_watch(d.fd, GLib.IO_IN | GLib.IO_ERR | GLib.IO_HUP, self.on_input, path)
            self.devices[path] = (d, wid)
            if self.on_ipad:
                self.grab(d)
            log(f"watching {d.name} ({path})")
        return True

    def drop(self, path):
        d, wid = self.devices.pop(path)
        GLib.source_remove(wid)
        try:
            d.close()
        except OSError:
            pass
        log(f"lost {path}")

    def grab(self, d):
        try:
            d.grab()
        except OSError as err:
            log(f"grab {d.path}: {err}")

    def ungrab(self, d):
        try:
            d.ungrab()
        except OSError:
            pass

    # switching

    def refresh_monitors(self):
        self.monitors = self.hypr.monitors()
        return True

    def at_edge(self, x, y):
        side = self.cfg["edge"]
        for m in self.monitors:
            w, h = m["width"] / m["scale"], m["height"] / m["scale"]
            if not (m["y"] <= y < m["y"] + h and m["x"] <= x < m["x"] + w + 1):
                continue
            neighbours = [n for n in self.monitors if n is not m]
            if side == "right" and x >= m["x"] + w - 1:
                return not any(n["x"] == m["x"] + w for n in neighbours)
            if side == "left" and x <= m["x"]:
                return not any(n["x"] + n["width"] / n["scale"] == m["x"] for n in neighbours)
        return False

    def enter_ipad(self):
        if self.on_ipad:
            return
        self.on_ipad = True
        self.return_y = self.hypr.cursor()[1]
        for d, _ in self.devices.values():
            self.grab(d)
        # Pin the iPad pointer to the edge we arrive from.
        sign = -1 if self.cfg["edge"] == "right" else 1
        self.send_mouse(sign * 32767, 0)
        self.vx = 0.0
        self.edge_push = 0
        log("-> ipad")

    def leave_ipad(self):
        if not self.on_ipad:
            return
        self.on_ipad = False
        self.release_all()
        for d, _ in self.devices.values():
            self.ungrab(d)
        try:
            x, _ = self.hypr.cursor()
            self.hypr.warp(x + (-3 if self.cfg["edge"] == "right" else 3), self.return_y)
        except OSError:
            pass
        self.edge_push = 0
        log("<- linux")

    def release_all(self):
        self.keys.clear()
        self.mods = 0
        self.buttons = 0
        self.reports[KBD_ID].notify(bytes(8))
        self.reports[MOUSE_ID].notify(bytes(7))
        self.reports[CONSUMER_ID].notify(bytes(2))

    # event handling

    def on_input(self, fd, cond, path):
        if path not in self.devices:
            return False
        d, _ = self.devices[path]
        try:
            events = list(d.read())
        except OSError:
            self.drop(path)
            return False
        for ev in events:
            if self.on_ipad:
                self.forward(ev)
            else:
                self.watch_linux(ev)
        return True

    def chord_pressed(self, ev):
        return ev.type == e.EV_KEY and ev.value == 1 and ev.code in self.toggle and self.toggle <= self.keys | {ev.code}

    def watch_linux(self, ev):
        if ev.type == e.EV_KEY:
            if self.chord_pressed(ev):
                self.pending_toggle = True
            if ev.value:
                self.keys.add(ev.code)
            else:
                self.keys.discard(ev.code)
                if self.pending_toggle and not (self.keys & self.toggle):
                    self.pending_toggle = False
                    self.keys.clear()
                    self.enter_ipad()
            return
        if ev.type != e.EV_REL or ev.code != e.REL_X:
            return
        toward = ev.value > 0 if self.cfg["edge"] == "right" else ev.value < 0
        if not toward:
            self.edge_push = 0
            return
        now = time.monotonic()
        if now - self.last_query < 0.008 and self.edge_push == 0:
            return
        self.last_query = now
        x, y = self.hypr.cursor()
        if self.at_edge(x, y):
            self.edge_push += abs(ev.value)
            if self.edge_push >= self.cfg["edge_push"] and not self.keys:
                self.enter_ipad()
        else:
            self.edge_push = 0

    def forward(self, ev):
        if ev.type == e.EV_REL:
            s = self.cfg["sensitivity"]
            if ev.code == e.REL_X:
                self.dx += ev.value * s
                self.track_return(ev.value * s)
            elif ev.code == e.REL_Y:
                self.dy += ev.value * s
            elif ev.code == e.REL_WHEEL:
                self.wheel += ev.value
            elif ev.code == e.REL_HWHEEL:
                self.pan += ev.value
            self.schedule_flush()
        elif ev.type == e.EV_KEY:
            if ev.code in BUTTONS:
                bit = BUTTONS[ev.code]
                self.buttons = self.buttons | bit if ev.value else self.buttons & ~bit
                self.flush()
                return
            if ev.value == 2:
                return  # the iPad does its own key repeat
            if self.chord_pressed(ev):
                self.pending_toggle = True
            if ev.code in MODIFIERS:
                bit = MODIFIERS[ev.code]
                self.mods = self.mods | bit if ev.value else self.mods & ~bit
            if ev.value:
                self.keys.add(ev.code)
            else:
                self.keys.discard(ev.code)
            if ev.code in CONSUMER:
                usage = CONSUMER[ev.code] if ev.value else 0
                self.reports[CONSUMER_ID].notify(usage.to_bytes(2, "little"))
                return
            self.send_keys()
            if self.pending_toggle and not self.keys:
                self.pending_toggle = False
                self.leave_ipad()

    def track_return(self, dx):
        inward = dx if self.cfg["edge"] == "right" else -dx
        if inward < 0 and self.vx <= 0:
            self.edge_push -= inward
            if self.edge_push >= self.cfg["edge_push"] and not self.keys and not self.buttons:
                GLib.idle_add(self.leave_ipad)
            return
        self.edge_push = 0
        self.vx = min(self.cfg["ipad_width"], max(0.0, self.vx + inward))

    def send_keys(self):
        usages = [KEYS[k] for k in self.keys if k in KEYS][:6]
        self.reports[KBD_ID].notify(bytes([self.mods, 0] + usages + [0] * (6 - len(usages))))

    def schedule_flush(self):
        if not self.flush_pending:
            self.flush_pending = True
            GLib.timeout_add(7, self.flush)

    def flush(self):
        self.flush_pending = False
        dx, dy = int(self.dx), int(self.dy)
        self.dx -= dx
        self.dy -= dy
        self.send_mouse(dx, dy, self.wheel, self.pan)
        self.wheel = self.pan = 0
        return False

    def send_mouse(self, dx, dy, wheel=0, pan=0):
        clamp = lambda v, lo, hi: max(lo, min(hi, v))
        dx, dy = clamp(dx, -32767, 32767), clamp(dy, -32767, 32767)
        self.reports[MOUSE_ID].notify(
            bytes([self.buttons])
            + dx.to_bytes(2, "little", signed=True)
            + dy.to_bytes(2, "little", signed=True)
            + clamp(wheel, -127, 127).to_bytes(1, "little", signed=True)
            + clamp(pan, -127, 127).to_bytes(1, "little", signed=True)
        )

    def shutdown(self):
        for d, _ in self.devices.values():
            self.ungrab(d)


def find_adapter(bus):
    objs = dbus.Interface(bus.get_object(BLUEZ, "/"), OM).GetManagedObjects()
    for path, ifaces in objs.items():
        if GATT_MANAGER in ifaces and AD_MANAGER in ifaces:
            return path
    sys.exit("no BLE adapter with GATT and advertising support")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", default=os.path.expanduser("~/.config/padbridge/config.toml"))
    ap.add_argument("--pair", type=int, nargs="?", const=120, default=0, metavar="SECONDS",
                    help="accept a new pairing for this many seconds")
    args = ap.parse_args()

    cfg = dict(DEFAULTS)
    if os.path.exists(args.config):
        with open(args.config, "rb") as f:
            cfg.update(tomllib.load(f))

    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SystemBus()
    bridge = Bridge(cfg, bus, find_adapter(bus), args.pair)
    bridge.start_bluetooth()
    bridge.refresh_monitors()
    bridge.scan()
    GLib.timeout_add_seconds(2, bridge.scan)
    GLib.timeout_add_seconds(5, bridge.refresh_monitors)

    loop = GLib.MainLoop()

    def stop(*_):
        bridge.shutdown()
        loop.quit()

    GLibUnix.signal_add(GLib.PRIORITY_DEFAULT, signal.SIGINT, stop)
    GLibUnix.signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, stop)
    try:
        loop.run()
    finally:
        bridge.shutdown()


if __name__ == "__main__":
    main()
