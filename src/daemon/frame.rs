//! SLEEP freeze-frame sidecar (`journals/<id>.frame`).
//!
//! A sleep kill is not a freeze: claude's graceful exit handler runs on the
//! ConPTY console-close and WIPES the alt screen into the journal before EOF
//! (`?1049l` + resume hint + a full-screen erase), so no journal re-parse can
//! ever reconstruct the conversation frame — the information is destroyed at
//! the source. `Core::sleep_terminals` therefore captures the mirror's alt
//! grid BETWEEN the drain and the kill (the only moment the pre-wipe frame
//! exists) and persists it here; the dead-attach arm replays it over the
//! serialized scrollback underlay (`?1049h` + frame — live-TUI semantics).
//!
//! The frame is DECORATION over the journal source of truth: any failure on
//! either side (write error, cap exceeded, truncation, bit rot) degrades to
//! the pre-freeze behavior and must never block a sleep or a wake. A corrupt
//! file is removed on read so it can't be re-tried forever.
//!
//! Layout (little-endian):
//!   `"PFRZ"` (4) · version u8 · cols u16 · rows u16 · flags u8 (bit0 = alt)
//!   · payload len u32 · payload (plain VT bytes) · crc32 u32 (IEEE, over
//!   everything before the crc field).
//! The payload is plain VT bytes, so the format is effectively version-proof;
//! the version byte covers future header growth only.

use std::path::PathBuf;

use uuid::Uuid;

const MAGIC: &[u8; 4] = b"PFRZ";
const VERSION: u8 = 1;
const FLAG_ALT: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 2 + 2 + 1 + 4;

/// Hard payload cap (journal-tail parity; a typical 160×42 claude frame is
/// 5–60 KB). Over the cap the capture is skipped — fallback, never an error
/// that could block sleep.
pub const MAX_PAYLOAD: usize = 2 * 1024 * 1024;

/// A decoded freeze-frame.
pub struct Frame {
    /// Grid size the frame was captured at (clip-on-resize policy: the
    /// overlay replays at this geometry; foreign attachers clip, never
    /// re-flow — exactly like a live TUI before its repaint).
    pub cols: u16,
    pub rows: u16,
    /// Captured from the alternate screen (the only kind v1 writes).
    pub alt: bool,
    /// Plain VT bytes: replayed verbatim after `?1049h`.
    pub bytes: Vec<u8>,
}

pub fn path(id: Uuid) -> PathBuf {
    crate::state::journals_dir().join(format!("{id}.frame"))
}

/// IEEE CRC-32, byte-at-a-time table variant (pw1 LOW-2). The read path
/// runs INSIDE the attach journal-lock hold for asleep terminals (dead
/// session ⇒ no ingest contention, but conn-thread + boot-window time):
/// bitwise cost was measured at 6.3ms for the 2MB cap / 185µs at the 60KB
/// typical — the 256-entry table is ~8× faster with no dependency. The
/// table is const-built from the same reflected 0xEDB88320 polynomial, so
/// the checksum value is bit-identical to the bitwise implementation
/// (existing .frame sidecars keep verifying; pinned by `crc32_known_vector`).
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize];
    }
    !crc
}

/// Encode a frame. `None` when the payload exceeds the cap (the caller skips
/// the capture and logs — degrade, don't truncate).
pub fn encode(cols: u16, rows: u16, alt: bool, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len() + 4);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&cols.to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out.push(if alt { FLAG_ALT } else { 0 });
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    Some(out)
}

/// Decode a frame file's bytes. `None` on ANY mismatch — magic, version,
/// length, cap, trailing garbage, crc — so the reader can never crash or
/// serve a torn/corrupt frame.
pub fn decode(raw: &[u8]) -> Option<Frame> {
    if raw.len() < HEADER_LEN + 4 || &raw[0..4] != MAGIC || raw[4] != VERSION {
        return None;
    }
    let cols = u16::from_le_bytes([raw[5], raw[6]]);
    let rows = u16::from_le_bytes([raw[7], raw[8]]);
    let flags = raw[9];
    let len = u32::from_le_bytes([raw[10], raw[11], raw[12], raw[13]]) as usize;
    if len > MAX_PAYLOAD || raw.len() != HEADER_LEN + len + 4 {
        return None;
    }
    let body_end = HEADER_LEN + len;
    let stored = u32::from_le_bytes([
        raw[body_end],
        raw[body_end + 1],
        raw[body_end + 2],
        raw[body_end + 3],
    ]);
    if crc32(&raw[..body_end]) != stored {
        return None;
    }
    Some(Frame {
        cols,
        rows,
        alt: flags & FLAG_ALT != 0,
        bytes: raw[HEADER_LEN..body_end].to_vec(),
    })
}

/// Atomically persist a frame (tmp + rename — a crash mid-write leaves at
/// worst a `.tmp` corpse the orphan reap collects; the visible file is always
/// whole). Errors bubble up for the caller to LOG AND IGNORE: capture must
/// never block or fail a sleep.
pub fn write(id: Uuid, cols: u16, rows: u16, alt: bool, payload: &[u8]) -> anyhow::Result<()> {
    let encoded = encode(cols, rows, alt, payload).ok_or_else(|| {
        anyhow::anyhow!(
            "frame payload {} bytes exceeds the {}MB cap",
            payload.len(),
            MAX_PAYLOAD / (1024 * 1024)
        )
    })?;
    let dst = path(id);
    let tmp = dst.with_extension("frame.tmp");
    std::fs::write(&tmp, &encoded)?;
    std::fs::rename(&tmp, &dst)?;
    Ok(())
}

/// Read + validate the frame for `id`. Missing file = `None` silently; a
/// present-but-invalid file is REMOVED (logged) so corruption degrades once,
/// not on every attach.
pub fn read(id: Uuid) -> Option<Frame> {
    let p = path(id);
    let raw = std::fs::read(&p).ok()?;
    match decode(&raw) {
        Some(f) => Some(f),
        None => {
            log::warn!(
                "terminal {id}: freeze-frame sidecar invalid ({} bytes) — removed, falling back to journal reconstruction",
                raw.len()
            );
            let _ = std::fs::remove_file(&p);
            None
        }
    }
}

/// Best-effort removal (wake success, terminal delete). Missing is fine.
pub fn remove(id: Uuid) {
    let _ = std::fs::remove_file(path(id));
    let _ = std::fs::remove_file(path(id).with_extension("frame.tmp"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pw1 LOW-2: the table variant must be bit-identical to the bitwise
    /// implementation it replaced — existing on-disk .frame sidecars keep
    /// verifying across the upgrade. Pinned against the standard IEEE
    /// CRC-32 check vector and a reference bitwise computation over
    /// VT-shaped bytes.
    #[test]
    fn crc32_known_vector() {
        // The canonical IEEE 802.3 check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);

        fn bitwise(data: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    let mask = (crc & 1).wrapping_neg();
                    crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                }
            }
            !crc
        }
        let vt = b"\x1b[?1049h\x1b[2J\x1b[5;3H\x1b[38;5;153mfrozen frame\x1b[0m\xff\x00\x7f";
        assert_eq!(crc32(vt), bitwise(vt));
        let big: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(crc32(&big), bitwise(&big));
    }

    #[test]
    fn roundtrip_preserves_geometry_and_bytes() {
        let payload = b"\x1b[0m\x1b[H\x1b[0;1;38;5;153mclaude says hi\x1b[0m\r\nrow two";
        let enc = encode(158, 51, true, payload).expect("under cap");
        let f = decode(&enc).expect("roundtrip");
        assert_eq!((f.cols, f.rows, f.alt), (158, 51, true));
        assert_eq!(f.bytes, payload);
    }

    #[test]
    fn corrupt_frames_decode_to_none() {
        let enc = encode(80, 24, true, b"frame body").unwrap();
        // Flipped payload bit → crc mismatch.
        let mut flipped = enc.clone();
        flipped[HEADER_LEN + 2] ^= 0x40;
        assert!(decode(&flipped).is_none(), "crc must catch a flipped bit");
        // Truncation (torn write).
        assert!(decode(&enc[..enc.len() - 3]).is_none(), "truncated");
        // Trailing garbage (concatenated/partial rewrite).
        let mut long = enc.clone();
        long.extend_from_slice(b"junk");
        assert!(decode(&long).is_none(), "trailing garbage");
        // Wrong magic / version.
        let mut bad_magic = enc.clone();
        bad_magic[0] = b'X';
        assert!(decode(&bad_magic).is_none(), "magic");
        let mut bad_ver = enc.clone();
        bad_ver[4] = 99;
        // Version flips participate in the crc, so recompute to isolate the
        // version check itself.
        let body_end = bad_ver.len() - 4;
        let crc = crc32(&bad_ver[..body_end]).to_le_bytes();
        bad_ver[body_end..].copy_from_slice(&crc);
        assert!(decode(&bad_ver).is_none(), "future version refused");
        // Absurd length field.
        assert!(decode(b"PFRZ").is_none(), "short reject");
    }

    #[test]
    fn cap_is_enforced_on_both_sides() {
        assert!(encode(2, 2, true, &vec![0u8; MAX_PAYLOAD + 1]).is_none());
        // A forged header claiming an over-cap length is refused even if the
        // buffer were somehow that large.
        let mut forged = Vec::new();
        forged.extend_from_slice(MAGIC);
        forged.push(VERSION);
        forged.extend_from_slice(&2u16.to_le_bytes());
        forged.extend_from_slice(&2u16.to_le_bytes());
        forged.push(FLAG_ALT);
        forged.extend_from_slice(&((MAX_PAYLOAD as u32) + 1).to_le_bytes());
        forged.extend_from_slice(&[0u8; 64]);
        assert!(decode(&forged).is_none());
    }

    #[test]
    fn crc32_is_ieee() {
        // The classic check value: crc32("123456789") = 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
