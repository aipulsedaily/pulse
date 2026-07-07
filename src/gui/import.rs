//! Discover existing Claude Code sessions under ~/.claude/projects so the
//! user's 20 loose PowerShell windows can be adopted into folders.

use std::path::PathBuf;
use std::time::SystemTime;
use uuid::Uuid;

pub struct FoundSession {
    pub session_id: Uuid,
    pub cwd: PathBuf,
    pub project: String,
    pub modified: SystemTime,
    pub preview: String,
}

pub fn scan() -> Vec<FoundSession> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let projects = home.join(".claude").join("projects");
    let mut sessions = Vec::new();

    let Ok(project_dirs) = std::fs::read_dir(&projects) else {
        return Vec::new();
    };
    for project_dir in project_dirs.flatten() {
        let project_name = project_dir.file_name().to_string_lossy().to_string();
        let Ok(files) = std::fs::read_dir(project_dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(session_id) = Uuid::parse_str(stem) else {
                continue;
            };
            let Ok(meta) = file.metadata() else { continue };
            if meta.len() < 200 {
                continue; // empty / aborted session
            }
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let (cwd, preview) = peek(&path);
            let Some(cwd) = cwd else { continue };
            sessions.push(FoundSession {
                session_id,
                cwd,
                project: project_name.clone(),
                modified,
                preview,
            });
        }
    }
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    sessions
}

/// Read the head of the jsonl for the session cwd and a human preview
/// (summary line or first user message). Files can be 100+ MB — never read
/// more than the first chunk.
fn peek(path: &std::path::Path) -> (Option<PathBuf>, String) {
    use std::io::Read;
    let mut buf = vec![0u8; 128 * 1024];
    let n = match std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return (None, String::new()),
    };
    buf.truncate(n);
    let text = String::from_utf8_lossy(&buf);

    let mut cwd: Option<PathBuf> = None;
    let mut preview = String::new();
    for line in text.lines().take(60) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                cwd = Some(PathBuf::from(c));
            }
        }
        if preview.is_empty() {
            if let Some(s) = v.get("summary").and_then(|s| s.as_str()) {
                preview = s.to_string();
            } else if v.get("type").and_then(|t| t.as_str()) == Some("user") {
                if let Some(msg) = v.get("message") {
                    let content = msg.get("content");
                    let text = match content {
                        Some(serde_json::Value::String(s)) => Some(s.clone()),
                        Some(serde_json::Value::Array(items)) => items.iter().find_map(|i| {
                            i.get("text").and_then(|t| t.as_str()).map(String::from)
                        }),
                        _ => None,
                    };
                    if let Some(t) = text {
                        let t = t.trim();
                        if !t.is_empty() && !t.starts_with('<') {
                            preview = t.chars().take(80).collect();
                        }
                    }
                }
            }
        }
        if cwd.is_some() && !preview.is_empty() {
            break;
        }
    }
    (cwd, preview)
}
