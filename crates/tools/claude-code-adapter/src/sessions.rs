//! The adapter's session map, and the check of it against Claude Code's own
//! state.
//!
//! The adapter owns a stable session id and maps it to the id Claude Code
//! uses. The map lives in the config directory, so a later adapter process on
//! the same home finds it. Claude Code keeps each conversation as a transcript
//! at `<config dir>/projects/<working-directory-key>/<claude session id>.jsonl`.
//! A session is only reported resumable when its transcript is still there,
//! at least as long as when the last turn ended, and ends on a whole line.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use crate::invocation::SessionSettings;

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub claude_session_id: String,
    pub settings: SessionSettings,
    /// Turns after which a transcript existed.
    pub turns_completed: u64,
    /// The transcript's length when the last turn ended.
    pub transcript_bytes: u64,
    /// Set when a turn starts and cleared when it ends. A record found with
    /// this set belongs to a turn whose outcome nobody saw.
    pub turn_in_flight: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The map has no such session.
    Unknown,
    /// The map has it but Claude Code's state does not back it.
    Lost(String),
    /// A turn was running when the previous adapter process ended.
    Interrupted,
    Intact,
}

pub struct SessionMap {
    home: PathBuf,
    path: PathBuf,
    guard: Mutex<()>,
}

impl SessionMap {
    pub fn new(home: PathBuf, path: PathBuf) -> Self {
        Self {
            home,
            path,
            guard: Mutex::new(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.guard.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn load(&self) -> BTreeMap<String, Record> {
        // An unreadable or damaged map reads as empty: every session then
        // answers `lost`, which is the honest answer.
        let Ok(bytes) = fs::read(&self.path) else {
            return BTreeMap::new();
        };
        let Ok(Value::Object(sessions)) = serde_json::from_slice::<Value>(&bytes)
            .map(|value| value.get("sessions").cloned().unwrap_or(Value::Null))
        else {
            return BTreeMap::new();
        };
        sessions
            .into_iter()
            .filter_map(|(id, value)| Some((id, record_from_json(&value)?)))
            .collect()
    }

    fn store(&self, sessions: &BTreeMap<String, Record>) -> std::io::Result<()> {
        let directory = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(directory)?;
        let document = json!({
            "sessions": sessions
                .iter()
                .map(|(id, record)| (id.clone(), record_to_json(record)))
                .collect::<serde_json::Map<_, _>>()
        });
        let temporary = self.path.with_extension("json.tmp");
        let mut file = fs::File::create(&temporary)?;
        file.write_all(&serde_json::to_vec(&document).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)
    }

    pub fn insert(&self, id: &str, record: Record) -> std::io::Result<()> {
        let _guard = self.lock();
        let mut sessions = self.load();
        sessions.insert(id.to_owned(), record);
        self.store(&sessions)
    }

    pub fn get(&self, id: &str) -> Option<Record> {
        let _guard = self.lock();
        self.load().remove(id)
    }

    /// Change one record in place. `false` when the session is not mapped.
    pub fn update(&self, id: &str, change: impl FnOnce(&mut Record)) -> std::io::Result<bool> {
        let _guard = self.lock();
        let mut sessions = self.load();
        let Some(record) = sessions.get_mut(id) else {
            return Ok(false);
        };
        change(record);
        self.store(&sessions)?;
        Ok(true)
    }

    /// Where Claude Code keeps this conversation, and how long it is.
    pub fn transcript(&self, claude_session_id: &str) -> Option<(PathBuf, u64)> {
        let name = format!("{claude_session_id}.jsonl");
        for entry in fs::read_dir(self.home.join("projects")).ok()?.flatten() {
            let candidate = entry.path().join(&name);
            if let Ok(metadata) = fs::metadata(&candidate)
                && metadata.is_file()
            {
                return Some((candidate, metadata.len()));
            }
        }
        None
    }

    /// Check the map's claim about a session against Claude Code's state.
    pub fn verify(&self, id: &str) -> Verdict {
        let Some(record) = self.get(id) else {
            return Verdict::Unknown;
        };
        if record.turn_in_flight {
            return Verdict::Interrupted;
        }
        if record.turns_completed == 0 {
            // Nothing has been said yet, so there is nothing to lose.
            return Verdict::Intact;
        }
        let Some((path, length)) = self.transcript(&record.claude_session_id) else {
            return Verdict::Lost("claude code has no transcript for the session".to_owned());
        };
        if length < record.transcript_bytes {
            return Verdict::Lost(
                "the transcript is shorter than when the last turn ended".to_owned(),
            );
        }
        if !ends_on_a_whole_line(&path) {
            return Verdict::Lost("the transcript ends in a torn line".to_owned());
        }
        Verdict::Intact
    }
}

fn ends_on_a_whole_line(path: &Path) -> bool {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let Ok(length) = file.seek(SeekFrom::End(0)) else {
        return false;
    };
    if length == 0 {
        return false;
    }
    let mut last = [0u8; 1];
    file.seek(SeekFrom::End(-1)).is_ok() && file.read_exact(&mut last).is_ok() && last[0] == b'\n'
}

fn record_to_json(record: &Record) -> Value {
    json!({
        "claude_session_id": record.claude_session_id,
        "read_only": record.settings.read_only,
        "model": record.settings.model,
        "effort": record.settings.effort,
        "tools": record.settings.tools,
        "system_prompt": record.settings.system_prompt,
        "turns_completed": record.turns_completed,
        "transcript_bytes": record.transcript_bytes,
        "turn_in_flight": record.turn_in_flight,
    })
}

fn record_from_json(value: &Value) -> Option<Record> {
    let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(Record {
        claude_session_id: text("claude_session_id")?,
        settings: SessionSettings {
            read_only: value.get("read_only")?.as_bool()?,
            model: text("model"),
            effort: text("effort"),
            tools: value
                .get("tools")?
                .as_array()?
                .iter()
                .map(|tool| tool.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?,
            system_prompt: text("system_prompt"),
        },
        turns_completed: value.get("turns_completed")?.as_u64()?,
        transcript_bytes: value.get("transcript_bytes")?.as_u64()?,
        turn_in_flight: value.get("turn_in_flight")?.as_bool()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "cc-adapter-sessions-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn record(turns: u64, bytes: u64) -> Record {
        Record {
            claude_session_id: "0b6f3c1e".into(),
            settings: SessionSettings {
                read_only: false,
                model: Some("m".into()),
                effort: None,
                tools: vec!["Agent".into()],
                system_prompt: None,
            },
            turns_completed: turns,
            transcript_bytes: bytes,
            turn_in_flight: false,
        }
    }

    fn map_in(home: &Path) -> SessionMap {
        SessionMap::new(home.to_owned(), home.join("adapter/sessions.json"))
    }

    fn write_transcript(home: &Path, contents: &str) {
        let directory = home.join("projects/-work-task");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("0b6f3c1e.jsonl"), contents).unwrap();
    }

    #[test]
    fn the_map_survives_a_new_process_and_unknown_ids_are_unknown() {
        let home = scratch("persist");
        map_in(&home).insert("cc-1", record(0, 0)).unwrap();
        let later = map_in(&home);
        assert_eq!(later.get("cc-1"), Some(record(0, 0)));
        assert_eq!(later.verify("cc-1"), Verdict::Intact);
        assert_eq!(later.verify("cc-2"), Verdict::Unknown);
    }

    #[test]
    fn a_session_with_history_is_intact_only_while_its_transcript_backs_it() {
        let home = scratch("verify");
        let map = map_in(&home);
        map.insert("cc-1", record(2, 16)).unwrap();
        assert!(matches!(map.verify("cc-1"), Verdict::Lost(why) if why.contains("no transcript")));
        write_transcript(&home, "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(map.verify("cc-1"), Verdict::Intact);
        write_transcript(&home, "{\"a\":1}\n");
        assert!(matches!(map.verify("cc-1"), Verdict::Lost(why) if why.contains("shorter")));
        write_transcript(&home, "{\"a\":1}\n{\"b\":2}\n{\"c\"");
        assert!(matches!(map.verify("cc-1"), Verdict::Lost(why) if why.contains("torn")));
    }

    #[test]
    fn a_turn_left_in_flight_is_never_reported_intact() {
        let home = scratch("inflight");
        let map = map_in(&home);
        map.insert("cc-1", record(1, 4)).unwrap();
        write_transcript(&home, "{\"a\"}\n");
        assert!(
            map.update("cc-1", |record| record.turn_in_flight = true)
                .unwrap()
        );
        assert_eq!(map.verify("cc-1"), Verdict::Interrupted);
        assert!(!map.update("cc-9", |_| {}).unwrap());
    }

    #[test]
    fn a_damaged_map_reads_as_empty() {
        let home = scratch("damaged");
        fs::create_dir_all(home.join("adapter")).unwrap();
        fs::write(home.join("adapter/sessions.json"), "not json").unwrap();
        assert_eq!(map_in(&home).verify("cc-1"), Verdict::Unknown);
    }
}
