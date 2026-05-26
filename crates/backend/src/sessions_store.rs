//! On-disk persistence for chat sessions. One TOML file per session lives
//! under `sica_core::paths::sessions_dir()`. Mirrors the per-provider
//! pattern in `frontend/src/llm_providers.rs`.

use std::fs;
use std::io;

use sica_core::paths::sessions_dir;
use sica_core::session::Session;

/// Scan the sessions directory and parse every `*.toml` file. Files that
/// fail to parse are skipped silently so a single corrupt file can't
/// brick the panel. Returns sessions sorted by `created_at` ascending.
pub fn load_all() -> Vec<Session> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(session) = toml::from_str::<Session>(&text) {
            out.push(session);
        }
    }
    out.sort_by_key(|s| s.created_at);
    out
}

pub fn save(session: &Session) -> io::Result<()> {
    let dir = sessions_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.toml", session.id));
    let text = toml::to_string_pretty(session)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(path, text)
}

/// Best-effort delete. Missing file is not an error.
pub fn delete(id: u64) {
    let path = sessions_dir().join(format!("{id}.toml"));
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sica_core::message::{Message, Role};

    #[test]
    fn roundtrip_with_reasoning() {
        let s = Session {
            id: 42,
            title: "test".into(),
            created_at: 1_700_000_000,
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    content: "hello".into(),
                    reasoning: Some("thinking…".into()),
                    images: Vec::new(),
                },
            ],
        };
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert_eq!(back.id, 42);
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.messages[1].reasoning.as_deref(), Some("thinking…"));
    }

    #[test]
    fn roundtrip_without_reasoning() {
        let s = Session {
            id: 1,
            title: "t".into(),
            created_at: 0,
            messages: vec![Message::user("x")],
        };
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert!(back.messages[0].reasoning.is_none());
    }
}
