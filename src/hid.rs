use evdev::KeyCode as K;

pub const KBD_ID: u8 = 1;
pub const MOUSE_ID: u8 = 2;
pub const CONSUMER_ID: u8 = 3;

pub const KBD_LEN: usize = 8;
pub const MOUSE_LEN: usize = 5;
pub const CONSUMER_LEN: usize = 2;

#[rustfmt::skip]
pub const REPORT_MAP: &[u8] = &[
    // Keyboard: modifiers, reserved, 6 keys
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, KBD_ID,
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x05, 0x07, 0x19, 0x00, 0x29, 0xFF, 0x81, 0x00,
    0xC0,
    // Mouse: 5 buttons, 8-bit X/Y, wheel, horizontal pan. The first three bytes are the boot
    // mouse layout, which is how the iPad reads them when it has no descriptor for this device:
    // it took 16-bit X's high byte for Y and never saw Y at all.
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, MOUSE_ID, 0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0xC0, 0xC0,
    // Consumer control: one 16-bit usage
    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, CONSUMER_ID,
    0x15, 0x00, 0x26, 0xFF, 0x03, 0x19, 0x00, 0x2A, 0xFF, 0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00,
    0xC0,
];

pub fn modifier(k: K) -> Option<u8> {
    Some(match k {
        K::KEY_LEFTCTRL => 0x01,
        K::KEY_LEFTSHIFT => 0x02,
        K::KEY_LEFTALT => 0x04,
        K::KEY_LEFTMETA => 0x08,
        K::KEY_RIGHTCTRL => 0x10,
        K::KEY_RIGHTSHIFT => 0x20,
        K::KEY_RIGHTALT => 0x40,
        K::KEY_RIGHTMETA => 0x80,
        _ => return None,
    })
}

pub fn button(k: K) -> Option<u8> {
    Some(match k {
        K::BTN_LEFT => 0x01,
        K::BTN_RIGHT => 0x02,
        K::BTN_MIDDLE => 0x04,
        K::BTN_SIDE => 0x08,
        K::BTN_EXTRA => 0x10,
        _ => return None,
    })
}

pub fn consumer(k: K) -> Option<u16> {
    Some(match k {
        K::KEY_PLAYPAUSE => 0x00CD,
        K::KEY_NEXTSONG => 0x00B5,
        K::KEY_PREVIOUSSONG => 0x00B6,
        K::KEY_MUTE => 0x00E2,
        K::KEY_VOLUMEUP => 0x00E9,
        K::KEY_VOLUMEDOWN => 0x00EA,
        K::KEY_BRIGHTNESSUP => 0x006F,
        K::KEY_BRIGHTNESSDOWN => 0x0070,
        _ => return None,
    })
}

/// evdev keycode to HID keyboard usage, the inverse of the kernel's hid-input table.
pub fn usage(k: K) -> Option<u8> {
    let c = k.code();
    // KEY_A..KEY_Z are not contiguous in evdev, so letters go through the table below.
    Some(match k {
        K::KEY_A => 0x04, K::KEY_B => 0x05, K::KEY_C => 0x06, K::KEY_D => 0x07, K::KEY_E => 0x08,
        K::KEY_F => 0x09, K::KEY_G => 0x0A, K::KEY_H => 0x0B, K::KEY_I => 0x0C, K::KEY_J => 0x0D,
        K::KEY_K => 0x0E, K::KEY_L => 0x0F, K::KEY_M => 0x10, K::KEY_N => 0x11, K::KEY_O => 0x12,
        K::KEY_P => 0x13, K::KEY_Q => 0x14, K::KEY_R => 0x15, K::KEY_S => 0x16, K::KEY_T => 0x17,
        K::KEY_U => 0x18, K::KEY_V => 0x19, K::KEY_W => 0x1A, K::KEY_X => 0x1B, K::KEY_Y => 0x1C,
        K::KEY_Z => 0x1D,
        // KEY_1..KEY_0 are 2..11, usages 0x1E..0x27 in the same order.
        _ if (K::KEY_1.code()..=K::KEY_0.code()).contains(&c) => 0x1E + (c - K::KEY_1.code()) as u8,
        // KEY_F1..KEY_F10 are 59..68.
        _ if (K::KEY_F1.code()..=K::KEY_F10.code()).contains(&c) => 0x3A + (c - K::KEY_F1.code()) as u8,
        K::KEY_ENTER => 0x28, K::KEY_ESC => 0x29, K::KEY_BACKSPACE => 0x2A, K::KEY_TAB => 0x2B,
        K::KEY_SPACE => 0x2C, K::KEY_MINUS => 0x2D, K::KEY_EQUAL => 0x2E, K::KEY_LEFTBRACE => 0x2F,
        K::KEY_RIGHTBRACE => 0x30, K::KEY_BACKSLASH => 0x31, K::KEY_SEMICOLON => 0x33,
        K::KEY_APOSTROPHE => 0x34, K::KEY_GRAVE => 0x35, K::KEY_COMMA => 0x36, K::KEY_DOT => 0x37,
        K::KEY_SLASH => 0x38, K::KEY_CAPSLOCK => 0x39, K::KEY_F11 => 0x44, K::KEY_F12 => 0x45,
        K::KEY_SYSRQ => 0x46, K::KEY_SCROLLLOCK => 0x47, K::KEY_PAUSE => 0x48, K::KEY_INSERT => 0x49,
        K::KEY_HOME => 0x4A, K::KEY_PAGEUP => 0x4B, K::KEY_DELETE => 0x4C, K::KEY_END => 0x4D,
        K::KEY_PAGEDOWN => 0x4E, K::KEY_RIGHT => 0x4F, K::KEY_LEFT => 0x50, K::KEY_DOWN => 0x51,
        K::KEY_UP => 0x52, K::KEY_NUMLOCK => 0x53, K::KEY_KPSLASH => 0x54, K::KEY_KPASTERISK => 0x55,
        K::KEY_KPMINUS => 0x56, K::KEY_KPPLUS => 0x57, K::KEY_KPENTER => 0x58, K::KEY_KP1 => 0x59,
        K::KEY_KP2 => 0x5A, K::KEY_KP3 => 0x5B, K::KEY_KP4 => 0x5C, K::KEY_KP5 => 0x5D,
        K::KEY_KP6 => 0x5E, K::KEY_KP7 => 0x5F, K::KEY_KP8 => 0x60, K::KEY_KP9 => 0x61,
        K::KEY_KP0 => 0x62, K::KEY_KPDOT => 0x63, K::KEY_102ND => 0x64, K::KEY_COMPOSE => 0x65,
        K::KEY_KPEQUAL => 0x67, K::KEY_F13 => 0x68, K::KEY_F14 => 0x69, K::KEY_F15 => 0x6A,
        K::KEY_RO => 0x87, K::KEY_YEN => 0x89,
        _ => return None,
    })
}

pub fn keyboard(mods: u8, keys: &[K]) -> [u8; KBD_LEN] {
    let mut r = [0u8; KBD_LEN];
    r[0] = mods;
    for (slot, u) in r[2..].iter_mut().zip(keys.iter().filter_map(|&k| usage(k))) {
        *slot = u;
    }
    r
}

pub fn mouse(buttons: u8, dx: i32, dy: i32, wheel: i32, pan: i32) -> [u8; MOUSE_LEN] {
    let b = |v: i32| v.clamp(-127, 127) as i8 as u8;
    [buttons, b(dx), b(dy), b(wheel), b(pan)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_and_function_keys() {
        assert_eq!(usage(K::KEY_1), Some(0x1E));
        assert_eq!(usage(K::KEY_0), Some(0x27));
        assert_eq!(usage(K::KEY_F1), Some(0x3A));
        assert_eq!(usage(K::KEY_F10), Some(0x43));
        assert_eq!(usage(K::KEY_F11), Some(0x44));
    }

    #[test]
    fn keyboard_report_keeps_press_order_and_six_keys() {
        let keys = [K::KEY_B, K::KEY_A, K::KEY_LEFTSHIFT, K::KEY_C, K::KEY_D, K::KEY_E, K::KEY_F, K::KEY_G];
        assert_eq!(keyboard(0x02, &keys), [0x02, 0, 0x05, 0x04, 0x06, 0x07, 0x08, 0x09]);
    }

    #[test]
    fn mouse_report_clamps() {
        assert_eq!(mouse(1, -400, 2, 300, -300), [1, 0x81, 2, 127, 0x81]);
    }
}
