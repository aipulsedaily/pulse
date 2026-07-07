//! Campbell palette shared by the GUI renderer and the daemon's query
//! responder, so OSC color reports always match what is drawn.

pub const NORMAL: [(u8, u8, u8); 8] = [
    (0x0c, 0x0c, 0x0c),
    (0xc5, 0x0f, 0x1f),
    (0x13, 0xa1, 0x0e),
    (0xc1, 0x9c, 0x00),
    (0x30, 0x78, 0xda),
    (0x88, 0x17, 0x98),
    (0x3a, 0x96, 0xdd),
    (0xcc, 0xcc, 0xcc),
];

pub const BRIGHT: [(u8, u8, u8); 8] = [
    (0x76, 0x76, 0x76),
    (0xe7, 0x48, 0x56),
    (0x16, 0xc6, 0x0c),
    (0xf9, 0xf1, 0xa5),
    (0x3b, 0x78, 0xff),
    (0xb4, 0x00, 0x9e),
    (0x61, 0xd6, 0xd6),
    (0xf2, 0xf2, 0xf2),
];

pub const FOREGROUND: (u8, u8, u8) = (0xcc, 0xcc, 0xcc);
// Warp terminal surface (D12). ANSI palette above is unchanged; only the
// default background differs, so OSC background queries report this.
pub const BACKGROUND: (u8, u8, u8) = (0x0c, 0x0e, 0x13);

fn ansi256(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let v = (index - 232) * 10 + 8;
        return (v, v, v);
    }
    let i = index - 16;
    let (r, g, b) = (i / 36, (i / 6) % 6, i % 6);
    let ch = |x: u8| if x == 0 { 0 } else { x * 40 + 55 };
    (ch(r), ch(g), ch(b))
}

/// RGB for OSC color queries (indices 256=fg, 257=bg, 258=cursor).
pub fn query_rgb(index: usize) -> (u8, u8, u8) {
    match index {
        256 | 258 => FOREGROUND,
        257 => BACKGROUND,
        0..=7 => NORMAL[index],
        8..=15 => BRIGHT[index - 8],
        16..=255 => ansi256(index as u8),
        _ => BACKGROUND,
    }
}
