//! Input events (client → host) and cursor/rumble messages (host → client).
//!
//! Input datagrams carry the newest event plus the few before it, so a lost
//! datagram is repaired by the next one without retransmission. The host
//! applies each event sequence number at most once.

/// Maximum events carried per input datagram (newest + history).
pub const INPUT_REDUNDANCY: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// Absolute position inside the video frame, 0..=65535 on each axis.
    MouseAbs { x: u16, y: u16 },
    /// Raw relative motion (games with a captured cursor).
    MouseRel { dx: i16, dy: i16 },
    /// 0 left, 1 right, 2 middle, 3 X1, 4 X2.
    MouseButton { button: u8, down: bool },
    /// Wheel delta in WHEEL_DELTA units (120 per notch).
    MouseWheel { delta: i16, horizontal: bool },
    /// Set-1 scancode; `extended` is the E0 prefix.
    Key {
        scancode: u16,
        extended: bool,
        down: bool,
    },
    /// Full XInput-style pad state (idempotent).
    Gamepad(GamepadState),
    /// Release every key and button the client may have left pressed
    /// (sent on focus loss).
    ReleaseAll,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamepadState {
    pub index: u8,
    /// XINPUT_GAMEPAD_* button bits.
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

/// A sequenced event as carried on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequenced {
    pub seq: u32,
    pub event: InputEvent,
}

const T_MOUSE_ABS: u8 = 1;
const T_MOUSE_REL: u8 = 2;
const T_MOUSE_BUTTON: u8 = 3;
const T_MOUSE_WHEEL: u8 = 4;
const T_KEY: u8 = 5;
const T_GAMEPAD: u8 = 6;
const T_RELEASE_ALL: u8 = 7;

impl InputEvent {
    fn write(&self, out: &mut Vec<u8>) {
        match *self {
            InputEvent::MouseAbs { x, y } => {
                out.push(T_MOUSE_ABS);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            InputEvent::MouseRel { dx, dy } => {
                out.push(T_MOUSE_REL);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
            }
            InputEvent::MouseButton { button, down } => {
                out.extend_from_slice(&[T_MOUSE_BUTTON, button, down as u8]);
            }
            InputEvent::MouseWheel { delta, horizontal } => {
                out.push(T_MOUSE_WHEEL);
                out.extend_from_slice(&delta.to_le_bytes());
                out.push(horizontal as u8);
            }
            InputEvent::Key {
                scancode,
                extended,
                down,
            } => {
                out.push(T_KEY);
                out.extend_from_slice(&scancode.to_le_bytes());
                out.push(extended as u8 | (down as u8) << 1);
            }
            InputEvent::Gamepad(g) => {
                out.extend_from_slice(&[T_GAMEPAD, g.index]);
                out.extend_from_slice(&g.buttons.to_le_bytes());
                out.extend_from_slice(&[g.left_trigger, g.right_trigger]);
                for v in [g.lx, g.ly, g.rx, g.ry] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            InputEvent::ReleaseAll => out.push(T_RELEASE_ALL),
        }
    }

    fn read(b: &[u8]) -> Option<(InputEvent, usize)> {
        let u16_at = |i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
        let i16_at = |i: usize| u16_at(i).map(|v| v as i16);
        Some(match *b.first()? {
            T_MOUSE_ABS => (
                InputEvent::MouseAbs {
                    x: u16_at(1)?,
                    y: u16_at(3)?,
                },
                5,
            ),
            T_MOUSE_REL => (
                InputEvent::MouseRel {
                    dx: i16_at(1)?,
                    dy: i16_at(3)?,
                },
                5,
            ),
            T_MOUSE_BUTTON => (
                InputEvent::MouseButton {
                    button: *b.get(1)?,
                    down: *b.get(2)? != 0,
                },
                3,
            ),
            T_MOUSE_WHEEL => (
                InputEvent::MouseWheel {
                    delta: i16_at(1)?,
                    horizontal: *b.get(3)? != 0,
                },
                4,
            ),
            T_KEY => {
                let f = *b.get(3)?;
                (
                    InputEvent::Key {
                        scancode: u16_at(1)?,
                        extended: f & 1 != 0,
                        down: f & 2 != 0,
                    },
                    4,
                )
            }
            T_GAMEPAD => (
                InputEvent::Gamepad(GamepadState {
                    index: *b.get(1)?,
                    buttons: u16_at(2)?,
                    left_trigger: *b.get(4)?,
                    right_trigger: *b.get(5)?,
                    lx: i16_at(6)?,
                    ly: i16_at(8)?,
                    rx: i16_at(10)?,
                    ry: i16_at(12)?,
                }),
                14,
            ),
            T_RELEASE_ALL => (InputEvent::ReleaseAll, 1),
            _ => return None,
        })
    }
}

/// Payload of a `Channel::Input` datagram: `count u8`, then per event
/// `seq u32` + encoded event, newest first.
pub fn encode_input(events: &[Sequenced], out: &mut Vec<u8>) {
    out.push(events.len() as u8);
    for e in events {
        out.extend_from_slice(&e.seq.to_le_bytes());
        e.event.write(out);
    }
}

pub fn decode_input(mut b: &[u8], out: &mut Vec<Sequenced>) -> Option<()> {
    let count = *b.first()? as usize;
    b = &b[1..];
    for _ in 0..count {
        let seq = u32::from_le_bytes(b.get(..4)?.try_into().ok()?);
        let (event, n) = InputEvent::read(&b[4..])?;
        out.push(Sequenced { seq, event });
        b = &b[4 + n..];
    }
    Some(())
}

/// Host → client cursor state (`Channel::Cursor`, message type 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorState {
    pub visible: bool,
    /// Identifies the current shape; the shape itself is sent separately.
    pub shape_id: u32,
    /// Position inside the video frame, 0..=65535.
    pub x: u16,
    pub y: u16,
}

/// How cursor shape `data` is laid out (mirrors DXGI pointer shape types,
/// so the client can rebuild a native cursor that behaves identically,
/// including screen inversion).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    /// `w*h` BGRA pixels with straight alpha.
    Color = 0,
    /// 1bpp AND mask rows followed by 1bpp XOR mask rows (`2*h` rows of
    /// `mono_pitch(w)` bytes): AND=1,XOR=1 inverts the screen.
    Monochrome = 1,
    /// `w*h` BGRA XOR colors (alpha ignored) followed by an `h`-row 1bpp AND mask.
    MaskedColor = 2,
}

/// Row pitch of a 1bpp mask, WORD-aligned as `CreateBitmap` requires.
pub fn mono_pitch(width: u16) -> usize {
    (width as usize).div_ceil(16) * 2
}

impl CursorKind {
    pub fn data_len(self, width: u16, height: u16) -> usize {
        let (w, h) = (width as usize, height as usize);
        match self {
            CursorKind::Color => w * h * 4,
            CursorKind::Monochrome => 2 * h * mono_pitch(width),
            CursorKind::MaskedColor => w * h * 4 + h * mono_pitch(width),
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => CursorKind::Color,
            1 => CursorKind::Monochrome,
            2 => CursorKind::MaskedColor,
            _ => return None,
        })
    }
}

/// Cursor shape (`Channel::Cursor`, message type 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub shape_id: u32,
    pub kind: CursorKind,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub data: Vec<u8>,
}

pub const CURSOR_STATE: u8 = 1;
pub const CURSOR_SHAPE: u8 = 2;

impl CursorState {
    pub fn encode(&self) -> [u8; 10] {
        let mut d = [0u8; 10];
        d[0] = CURSOR_STATE;
        d[1] = self.visible as u8;
        d[2..6].copy_from_slice(&self.shape_id.to_le_bytes());
        d[6..8].copy_from_slice(&self.x.to_le_bytes());
        d[8..10].copy_from_slice(&self.y.to_le_bytes());
        d
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < 10 || b[0] != CURSOR_STATE {
            return None;
        }
        Some(CursorState {
            visible: b[1] != 0,
            shape_id: u32::from_le_bytes(b[2..6].try_into().ok()?),
            x: u16::from_le_bytes([b[6], b[7]]),
            y: u16::from_le_bytes([b[8], b[9]]),
        })
    }
}

impl CursorShape {
    pub fn encode(&self) -> Vec<u8> {
        let mut d = Vec::with_capacity(14 + self.data.len());
        d.push(CURSOR_SHAPE);
        d.extend_from_slice(&self.shape_id.to_le_bytes());
        d.push(self.kind as u8);
        for v in [self.width, self.height, self.hot_x, self.hot_y] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&self.data);
        d
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < 14 || b[0] != CURSOR_SHAPE {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let kind = CursorKind::from_u8(b[5])?;
        let (width, height) = (u16_at(6), u16_at(8));
        let data = b[14..].to_vec();
        if data.len() != kind.data_len(width, height) {
            return None;
        }
        Some(CursorShape {
            shape_id: u32::from_le_bytes(b[1..5].try_into().ok()?),
            kind,
            width,
            height,
            hot_x: u16_at(10),
            hot_y: u16_at(12),
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_round_trip() {
        let events = [
            InputEvent::MouseAbs { x: 1, y: 65535 },
            InputEvent::MouseRel { dx: -5, dy: 300 },
            InputEvent::MouseButton {
                button: 2,
                down: true,
            },
            InputEvent::MouseWheel {
                delta: -120,
                horizontal: true,
            },
            InputEvent::Key {
                scancode: 0x1d,
                extended: true,
                down: false,
            },
            InputEvent::Gamepad(GamepadState {
                index: 3,
                buttons: 0xf00f,
                left_trigger: 7,
                right_trigger: 255,
                lx: -32768,
                ly: 32767,
                rx: 1,
                ry: -1,
            }),
            InputEvent::ReleaseAll,
        ];
        let seq: Vec<_> = events
            .iter()
            .enumerate()
            .map(|(i, &e)| Sequenced {
                seq: 100 + i as u32,
                event: e,
            })
            .collect();
        let mut buf = Vec::new();
        encode_input(&seq, &mut buf);
        let mut out = Vec::new();
        decode_input(&buf, &mut out).unwrap();
        assert_eq!(out, seq);
    }

    #[test]
    fn truncated_input_is_rejected() {
        let mut buf = Vec::new();
        encode_input(
            &[Sequenced {
                seq: 1,
                event: InputEvent::MouseAbs { x: 1, y: 2 },
            }],
            &mut buf,
        );
        let mut out = Vec::new();
        assert!(decode_input(&buf[..buf.len() - 1], &mut out).is_none());
    }

    #[test]
    fn cursor_round_trip() {
        let s = CursorState {
            visible: true,
            shape_id: 9,
            x: 100,
            y: 200,
        };
        assert_eq!(CursorState::decode(&s.encode()), Some(s));
        let shape = CursorShape {
            shape_id: 9,
            kind: CursorKind::Color,
            width: 2,
            height: 1,
            hot_x: 1,
            hot_y: 0,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        assert_eq!(CursorShape::decode(&shape.encode()), Some(shape));
        let mono = CursorShape {
            shape_id: 10,
            kind: CursorKind::Monochrome,
            width: 32,
            height: 32,
            hot_x: 0,
            hot_y: 0,
            data: vec![0xAA; CursorKind::Monochrome.data_len(32, 32)],
        };
        assert_eq!(mono.data.len(), 2 * 32 * 4);
        assert_eq!(CursorShape::decode(&mono.encode()), Some(mono));
    }
}
