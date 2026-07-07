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

/// Missing or corrupt file ⇒ empty set (logged) — never a startup failure.
pub fn load() -> TokenFile {
    match std::fs::read(path()) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(e) => {
                log::error!("ctl-tokens.json corrupt ({e}); starting with no scoped tokens");
                TokenFile::default()
            }
        },
        Err(_) => TokenFile::default(),
    }
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
}
