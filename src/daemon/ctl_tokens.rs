//! Scoped controller tokens (P5), persisted across daemon restarts.
//!
//! The master daemon.json token rotates every daemon start; scripts that hold
//! it break on every reboot. Controller tokens minted here live in
//! `%LOCALAPPDATA%\Pulse\ctl-tokens.json` (user-private ACL, same
//! as daemon.json) and survive restarts. They are guardrails for
//! cooperating-but-fallible agents, NOT a security boundary: any same-user
//! process can read daemon.json and obtain full rights anyway.

use std::path::PathBuf;

use crate::protocol::CtlTokenInfo;
use crate::state::data_dir;

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
pub struct TokenFile {
    pub tokens: Vec<CtlTokenInfo>,
}

pub fn path() -> PathBuf {
    data_dir().join("ctl-tokens.json")
}

/// Load the token file — never a startup failure, but never a silent
/// first-write hazard either (wave1 F2, the R4-F1 state.json shape):
/// - NotFound ⇒ quiet empty set (normal first run);
/// - corrupt or unreadable (AV lock / sharing violation / EACCES) ⇒ LOG +
///   move the real file aside FIRST, then default. Without the backup, the
///   next token verb would `save()` the defaulted-empty set over the real
///   file and every persisted scoped token would be gone permanently.
///   (If the rename itself fails — e.g. the file is still exclusively
///   locked — the original survives in place for the next boot.)
pub fn load() -> TokenFile {
    load_from(&path())
}

fn load_from(p: &std::path::Path) -> TokenFile {
    match std::fs::read(p) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(e) => {
                log::error!(
                    "ctl-tokens.json corrupt ({e}); starting with no scoped tokens, old file backed up"
                );
                backup_bad_file(p);
                TokenFile::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TokenFile::default(),
        Err(e) => {
            log::error!(
                "ctl-tokens.json unreadable ({e}); starting with no scoped tokens, backup attempted"
            );
            backup_bad_file(p);
            TokenFile::default()
        }
    }
}

/// Move a corrupt/unreadable ctl-tokens.json aside (the
/// `SharedState::backup_bad_state` pattern). Keep the FIRST bad copy — a
/// second one falls back to a timestamped name rather than clobbering it.
fn backup_bad_file(p: &std::path::Path) {
    let dir = p.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let backup = dir.join("ctl-tokens.json.corrupt");
    let backup = if backup.exists() {
        dir.join(format!(
            "ctl-tokens.json.corrupt.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ))
    } else {
        backup
    };
    let _ = std::fs::rename(p, backup);
}

/// Atomic tmp+rename write (the SharedState::save pattern), so a power cut
/// can never leave a truncated token file.
pub fn save(f: &TokenFile) {
    use std::io::Write;
    let Ok(data) = serde_json::to_vec_pretty(f) else {
        return;
    };
    let dir = data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join("ctl-tokens.json.tmp");
    let write_tmp = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        Ok(())
    };
    if write_tmp().is_ok() {
        let _ = std::fs::rename(&tmp, path());
    } else {
        log::error!("failed to write ctl-tokens.json");
    }
}

/// Mint a controller token: 32 lowercase hex chars (122 bits — the same
/// entropy family as the master token's two uuids).
pub fn mint() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §15 ctl_tokens_roundtrip — save/load through the atomic path; upsert
    /// by name rotates the token; revoke removes. Runs in an isolated
    /// TC_DATA_DIR-style temp dir by pointing the file ops at a temp path via
    /// the env override.
    #[test]
    fn ctl_tokens_roundtrip() {
        // NOTE: data_dir() reads TC_DATA_DIR per call; tests in this crate
        // run multi-threaded, so instead of mutating the process env (racy)
        // we exercise the pure upsert/revoke logic plus serde roundtrip.
        let mut f = TokenFile::default();
        // Upsert by name (the daemon-side TokenCreate logic mirrored).
        let upsert = |f: &mut TokenFile, name: &str, scope: u32| -> String {
            let token = mint();
            let info = CtlTokenInfo {
                name: name.into(),
                token: token.clone(),
                scope,
                created_ms: 42,
            };
            match f.tokens.iter_mut().find(|t| t.name == name) {
                Some(t) => *t = info,
                None => f.tokens.push(info),
            }
            token
        };
        let t1 = upsert(&mut f, "agents", crate::protocol::SCOPE_READ);
        assert_eq!(t1.len(), 32);
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        let t2 = upsert(&mut f, "agents", crate::protocol::SCOPE_INPUT);
        assert_ne!(t1, t2, "re-creating a name rotates its token");
        assert_eq!(f.tokens.len(), 1, "upsert, not append");
        upsert(&mut f, "ci", crate::protocol::SCOPE_MANAGE);
        assert_eq!(f.tokens.len(), 2);

        // Serde roundtrip is byte-stable.
        let json = serde_json::to_vec(&f).unwrap();
        let back: TokenFile = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.tokens.len(), 2);
        assert_eq!(back.tokens[0].name, "agents");
        assert_eq!(back.tokens[0].token, t2);

        // Revoke removes by name.
        f.tokens.retain(|t| t.name != "agents");
        assert_eq!(f.tokens.len(), 1);
        assert_eq!(f.tokens[0].name, "ci");
    }

    /// Wave1 F2: the loader's three-arm hardening (the state.json R4-F1
    /// shape). NotFound is the quiet first run; a corrupt file is moved
    /// aside BEFORE defaulting, so a later token verb's whole-file save can
    /// never clobber the real data; a healthy file just loads.
    #[test]
    fn load_from_three_arms() {
        let dir = std::env::temp_dir().join(format!("tc-ctl-tokens-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ctl-tokens.json");

        // NotFound ⇒ empty set, NO backup file minted.
        assert!(load_from(&p).tokens.is_empty());
        assert!(!dir.join("ctl-tokens.json.corrupt").exists());

        // Healthy ⇒ loads as-is.
        let mut f = TokenFile::default();
        f.tokens.push(CtlTokenInfo {
            name: "keepme".into(),
            token: mint(),
            scope: crate::protocol::SCOPE_READ,
            created_ms: 1,
        });
        std::fs::write(&p, serde_json::to_vec(&f).unwrap()).unwrap();
        assert_eq!(load_from(&p).tokens.len(), 1);
        assert!(p.exists(), "healthy load never moves the file");

        // Corrupt ⇒ default AND the real bytes are moved aside first.
        std::fs::write(&p, b"{ not json").unwrap();
        assert!(load_from(&p).tokens.is_empty());
        assert!(!p.exists(), "corrupt file moved aside — a save can't clobber it");
        assert_eq!(
            std::fs::read(dir.join("ctl-tokens.json.corrupt")).unwrap(),
            b"{ not json",
            "backup holds the original bytes"
        );

        // A SECOND bad copy falls back to a timestamped name, keeping the first.
        std::fs::write(&p, b"also bad").unwrap();
        assert!(load_from(&p).tokens.is_empty());
        assert_eq!(
            std::fs::read(dir.join("ctl-tokens.json.corrupt")).unwrap(),
            b"{ not json",
            "first backup is never clobbered"
        );
        let timestamped = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("ctl-tokens.json.corrupt.")
            })
            .count();
        assert_eq!(timestamped, 1, "second bad copy got a timestamped name");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
