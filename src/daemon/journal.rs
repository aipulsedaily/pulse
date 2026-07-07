//! Append-only per-terminal output journal.
//!
//! Every byte a terminal emits is appended here as it happens, so scrollback
//! survives daemon restarts and reboots. On attach, the tail is replayed into
//! the client's VT parser to reconstruct the screen exactly.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Instant;
use uuid::Uuid;

use crate::state::journals_dir;

/// Compact once the journal grows past this.
const MAX_LEN: u64 = 8 * 1024 * 1024;
/// After compaction, keep roughly this much tail.
const COMPACT_KEEP: u64 = 4 * 1024 * 1024;
/// How much tail to replay to a newly attached client.
const REPLAY_MAX: u64 = 2 * 1024 * 1024;

// NOTE (perf-wave-1): a write-behind buffer here (batch appends, flush on the
// 250ms tick) was tried and REVERTED: it broke the crash-durability contract
// probe `compact_crash` pins — a hard TerminateProcess must not lose bytes
// the daemon already ingested, and write()-per-append puts them in the OS
// page cache where a process kill can't touch them. The reader→ingest
// pipeline (session.rs) already coalesces appends into ≤64KiB batches, so
// per-append write() costs ~57ms per 50MB flood — not worth the contract.

pub struct Journal {
    path: PathBuf,
    file: File,
    len: u64,
    /// Total bytes ever dropped from the head by compaction, so
    /// `absolute_len()` is a monotonic stream offset since journal birth.
    /// Block records key on these offsets and never need rewriting when the
    /// file is compacted. Survives daemon restarts via the blocks sidecar
    /// (NOT the journal file); a missing sidecar means base=0, which is safe:
    /// pre-feature journals have no block records to misalign.
    base: u64,
    /// Bytes appended since the last fsync; the flush thread syncs only when set.
    dirty: bool,
    /// A write failed since the last time the error was reported.
    new_error: bool,
    /// A sync failure was already logged (log once per failure run — a dying
    /// disk would otherwise spam every 2s tick). Reset by the next success.
    sync_err_logged: bool,
    /// When the most recent append happened (drives the burst-end flush).
    last_append: Instant,
    /// When the journal was last fsync'd (bounds sustained-output exposure).
    last_sync: Instant,
}

fn journal_path(id: Uuid) -> PathBuf {
    journals_dir().join(format!("{id}.log"))
}

impl Journal {
    pub fn open(id: Uuid, base: u64) -> anyhow::Result<Self> {
        std::fs::create_dir_all(journals_dir())?;
        Self::open_path(journal_path(id), base)
    }

    /// `open` with an explicit path — the id→path mapping factored out so
    /// unit tests can run against a temp file instead of the data dir.
    fn open_path(path: PathBuf, base: u64) -> anyhow::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            len,
            base,
            dirty: false,
            new_error: false,
            sync_err_logged: false,
            last_append: Instant::now(),
            last_sync: Instant::now(),
        })
    }

    /// Absolute stream offset of the next appended byte: monotonic since the
    /// journal's birth, unaffected by compaction.
    pub fn absolute_len(&self) -> u64 {
        self.base + self.len
    }

    /// Appends `bytes`; returns `Some(new base)` when the append triggered a
    /// compaction, so the caller can evict block records that now point
    /// before the file's head (forwarded AFTER the journal lock is released —
    /// the blocks lock is a leaf).
    pub fn append(&mut self, bytes: &[u8]) -> Option<u64> {
        match self.file.write_all(bytes) {
            Ok(()) => {
                self.len += bytes.len() as u64;
                self.dirty = true;
                self.last_append = Instant::now();
            }
            Err(e) => {
                log::error!("journal append failed for {:?}: {e}", self.path);
                self.new_error = true;
                // R4-F5: write_all may have written a PREFIX of the batch
                // (it loops in chunks; disk-full mid-loop is the realistic
                // trigger), leaving the file LONGER than `base + len` says —
                // every later block record's start_off would then point
                // before its true position until the next compaction
                // resyncs. Resync `len` to the file now instead.
                if let Ok(meta) = self.file.metadata() {
                    if meta.len() != self.len {
                        self.len = meta.len();
                        // Those prefix bytes are real appended output:
                        // they need the same fsync coverage.
                        self.dirty = true;
                        self.last_append = Instant::now();
                    }
                }
            }
        }
        if self.len > MAX_LEN {
            match self.compact() {
                Ok(()) => return Some(self.base),
                Err(e) => {
                    log::error!("journal compact failed for {:?}: {e}", self.path);
                    self.new_error = true;
                }
            }
        }
        None
    }

    /// Take the pending write-error flag (reported at most once per failure run).
    pub fn take_new_error(&mut self) -> bool {
        std::mem::take(&mut self.new_error)
    }

    /// Force the OS to flush this journal to disk.
    pub fn sync(&mut self) {
        // C5: a silently failing fsync is indistinguishable from the
        // crash-atomicity bugs compact_crash exists to prevent — log it
        // (once per failure run), but never toast: the append path already
        // owns the user-facing disk-full banner.
        match self.file.sync_data() {
            Ok(()) => {
                self.sync_err_logged = false;
                // R4-F4: clear dirty ONLY on success, like finish_tick_sync.
                // on_exit's dying-tail flush is the caller that matters: a
                // dead terminal appends nothing further, so a cleared-but-
                // unsynced dirty flag would end retries forever and leave the
                // tail page-cache-only.
                self.dirty = false;
            }
            Err(e) => {
                if !self.sync_err_logged {
                    log::warn!("journal fsync failed for {:?}: {e}", self.path);
                    self.sync_err_logged = true;
                }
            }
        }
        self.last_sync = Instant::now();
    }

    /// Power-loss-grade flush policy, first half (flush-tick thread): under
    /// the journal lock, decide due-ness — fsync a dirty journal once its
    /// output burst has ended (idle ≥500ms) or, for sustained output that
    /// never idles, at least every 2s — and hand back a duplicated handle so
    /// the fsync itself runs OUTSIDE the lock (ingest must not wait out a
    /// slow disk's fsync). `dirty` clears now: appends racing the
    /// out-of-lock sync re-mark it, and the journal only appends, so a
    /// handle taken here covers every byte appended up to this point — the
    /// power-loss exposure bound is unchanged. A compaction racing the sync
    /// swaps `self.file`, leaving the dup pointing at the pre-compaction
    /// file — harmless (compact() sync_all'd the replacement before the
    /// rename). Callers pass the result to `finish_tick_sync`.
    pub fn begin_tick_sync(&mut self) -> Option<File> {
        if !self.dirty {
            return None;
        }
        let now = Instant::now();
        let burst_ended = now.duration_since(self.last_append) >= std::time::Duration::from_millis(500);
        let aged = now.duration_since(self.last_sync) >= std::time::Duration::from_secs(2);
        if !(burst_ended || aged) {
            return None;
        }
        match self.file.try_clone() {
            Ok(f) => {
                self.dirty = false;
                self.last_sync = now;
                Some(f)
            }
            Err(_) => {
                // Degrade: sync under the lock (the pre-r3 behavior).
                self.sync();
                None
            }
        }
    }

    /// Second half: record the out-of-lock fsync result. Failure re-marks
    /// dirty (the next tick retries) and keeps the log-once-per-failure-run
    /// discipline `sync()` uses. Takes the io::Result itself (R4-H4) so the
    /// log line can distinguish disk-full from handle-invalid.
    pub fn finish_tick_sync(&mut self, res: &std::io::Result<()>) {
        match res {
            Ok(()) => self.sync_err_logged = false,
            Err(e) => {
                self.dirty = true;
                if !self.sync_err_logged {
                    log::warn!("journal fsync failed for {:?}: {e}", self.path);
                    self.sync_err_logged = true;
                }
            }
        }
    }

    /// Keep only the tail, cut at a line boundary so the VT stream restarts
    /// at a clean point.
    fn compact(&mut self) -> anyhow::Result<()> {
        // Read ONLY the tail being kept: this runs on the pty-ingest thread
        // with the journal lock held, and the file is >8MB here — reading
        // all of it doubled both the IO and the transient allocation for
        // bytes that were about to be dropped. `skip` is measured against
        // the file's own length so the kept range is exact even if len
        // drifted. Byte-identical to the old full-read cut: the boundary
        // scan starts at the same absolute position.
        let mut src = File::open(&self.path)?;
        let file_len = src.metadata()?.len();
        let skip = file_len.saturating_sub(COMPACT_KEEP);
        src.seek(SeekFrom::Start(skip))?;
        let mut data = Vec::new();
        src.read_to_end(&mut data)?;
        // Advance to just past the tail's first '\n' (NOT via
        // cut_at_line_boundary — its at==0 early-return means "keep the
        // whole file", which only applies when nothing was skipped). No
        // newline in the tail ⇒ keep it whole, exactly like the old cut.
        let start = if skip == 0 {
            0
        } else {
            data.iter()
                .position(|&b| b == b'\n')
                .map(|p| p + 1)
                .unwrap_or(0)
        };
        let tmp = self.path.with_extension("log.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&data[start..])?;
            // The rename below only swaps the name; the DATA must be durable
            // before the journal name points at it, or a power cut in the next
            // couple of seconds leaves a truncated/empty journal behind an
            // already-committed swap.
            f.sync_all()?;
        }
        // Atomic swap: rename-over the live journal (MoveFileExW/POSIX-rename
        // replaces an existing destination). Never remove_file first — a crash
        // between remove and rename would leave NO journal at all, and .log.tmp
        // is never recovered by open(). Either the rename committed (compacted
        // tail, fsynced above) or it didn't (full pre-compaction journal): the
        // name always resolves to a complete file.
        std::fs::rename(&tmp, &self.path)?;
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.len = self.file.metadata()?.len();
        // The cut bytes leave the file but not the stream's coordinate space.
        self.base += skip + start as u64;
        Ok(())
    }

    /// Bytes for the absolute-offset range [abs_start, abs_end), clamped to
    /// what the file still holds (compaction may have cut the head) and to
    /// `max` bytes. Returns (bytes, clipped) where clipped = the head was cut
    /// or `max` was hit. Opens a FRESH handle (same pattern as `tail()`): the
    /// append handle's position/mode must never be disturbed under concurrent
    /// appends. Callers hold the journal lock, so `base`/`len` are stable
    /// across the read.
    pub fn read_range(&self, abs_start: u64, abs_end: u64, max: usize) -> (Vec<u8>, bool) {
        let mut clipped = abs_start < self.base;
        let start = abs_start.saturating_sub(self.base).min(self.len);
        let end = abs_end.saturating_sub(self.base).min(self.len);
        if end <= start {
            return (Vec::new(), clipped);
        }
        let mut take = (end - start) as usize;
        if take > max {
            take = max;
            clipped = true;
        }
        let Ok(mut f) = File::open(&self.path) else {
            return (Vec::new(), clipped);
        };
        if f.seek(SeekFrom::Start(start)).is_err() {
            return (Vec::new(), clipped);
        }
        let mut buf = vec![0u8; take];
        let mut filled = 0usize;
        while filled < take {
            match f.read(&mut buf[filled..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => filled += n,
            }
        }
        buf.truncate(filled);
        if filled < take {
            clipped = true; // mid-loop read error: the caller must not
                            // assume the whole requested range was returned
        }
        (buf, clipped)
    }

    /// The replay tail for a newly attached client.
    pub fn tail(&self) -> Vec<u8> {
        let mut f = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let from = len.saturating_sub(REPLAY_MAX);
        if f.seek(SeekFrom::Start(from)).is_err() {
            return Vec::new();
        }
        let mut data = Vec::new();
        if f.read_to_end(&mut data).is_err() {
            return Vec::new();
        }
        if from > 0 {
            // `from` is a fixed byte offset — essentially always mid-line
            // (often mid-escape). Drop the partial first line so the replay
            // parser starts at a clean point, exactly like compact()'s scan
            // (r2-F3: the old cut_at_line_boundary(&data, 0) early-returned
            // 0 unconditionally, so nothing was ever dropped).
            drop_partial_first_line(&mut data);
        }
        data
    }

    pub fn delete(id: Uuid) {
        let _ = std::fs::remove_file(journal_path(id));
    }
}

/// Advance past the first '\n' (keep everything when there is none — one
/// giant line beats an empty replay). Only call when the buffer is known to
/// start mid-line.
fn drop_partial_first_line(data: &mut Vec<u8>) {
    let cut = data
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    data.drain(..cut);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// R4-T4: the begin_tick_sync / finish_tick_sync dirty discipline. A
    /// forgotten re-mark on failure silently voids the ≤600ms power-loss
    /// bound — the retry guarantee is the whole point of the two-phase shape.
    #[test]
    fn tick_sync_dirty_discipline() {
        let dir = std::env::temp_dir().join(format!("tc-journal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = Journal::open_path(dir.join("t.log"), 0).unwrap();

        // Clean journal ⇒ nothing due.
        assert!(j.begin_tick_sync().is_none(), "clean journal must not sync");

        // Append marks dirty, but a mid-burst journal is not due yet.
        j.append(b"hello");
        assert!(j.dirty);
        assert!(j.begin_tick_sync().is_none(), "mid-burst (<500ms idle) not due");

        // Sustained output that never idles is still due at the 2s cap.
        j.last_sync = Instant::now() - Duration::from_secs(3);
        let h = j.begin_tick_sync().expect("aged journal is due");
        assert!(!j.dirty, "dirty clears at begin (racing appends re-mark it)");
        drop(h);

        // Failure RE-MARKS dirty — the retry guarantee — and logs once.
        j.dirty = false;
        j.finish_tick_sync(&Err(std::io::Error::other("synthetic")));
        assert!(j.dirty, "failed fsync must re-mark dirty");
        assert!(j.sync_err_logged);

        // Next tick retries (burst over) and success resets the log-once run.
        j.last_append = Instant::now() - Duration::from_millis(600);
        let h = j.begin_tick_sync().expect("burst-ended dirty journal is due");
        let res = h.sync_data();
        assert!(res.is_ok());
        j.finish_tick_sync(&res);
        assert!(!j.dirty);
        assert!(!j.sync_err_logged, "success resets the log-once discipline");

        // R4-F4 sibling: sync() clears dirty on its Ok arm.
        j.append(b"tail");
        assert!(j.dirty);
        j.sync();
        assert!(!j.dirty, "successful sync() clears dirty");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// r2-F3: a >REPLAY_MAX journal's replay tail must start on a line
    /// boundary — the fixed-offset cut lands mid-line/mid-escape otherwise.
    #[test]
    fn tail_cut_drops_the_partial_first_line() {
        let mut d = b"\x1b[31mtruncated escape tail\r\nclean line\r\n".to_vec();
        drop_partial_first_line(&mut d);
        assert_eq!(d, b"clean line\r\n");
        // No newline at all: keep everything.
        let mut d = b"one giant line with no newline".to_vec();
        drop_partial_first_line(&mut d);
        assert_eq!(d, b"one giant line with no newline");
        // Newline-first: drops just that empty head.
        let mut d = b"\nrest".to_vec();
        drop_partial_first_line(&mut d);
        assert_eq!(d, b"rest");
    }
}
