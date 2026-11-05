use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::store::CoreAgentSessionSnapshot;

#[derive(Deserialize)]
struct KimiSessionRecord {
    terminal: String,
    session_id: String,
    #[serde(default)]
    resume_argv: Vec<String>,
}

fn default_sessions_path() -> Option<PathBuf> {
    std::env::var_os("HERDR_CORE_NATIVE_KIMI_SESSIONS")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                PathBuf::from(home).join(".config/herdr-app/data/agents/kimi-sessions.jsonl")
            })
        })
}

pub(crate) fn lookup_from(
    path: &Path,
    address: &str,
) -> io::Result<Option<CoreAgentSessionSnapshot>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut latest = None;
    for line in content.lines() {
        let Ok(record) = serde_json::from_str::<KimiSessionRecord>(line) else {
            continue;
        };
        if record.terminal != address
            || record.session_id.is_empty()
            || record.resume_argv.is_empty()
            || !record
                .resume_argv
                .iter()
                .any(|argument| argument == &record.session_id)
        {
            continue;
        }
        latest = Some(CoreAgentSessionSnapshot {
            agent: "kimi-code".into(),
            session_id: record.session_id,
            resume_argv: record.resume_argv,
        });
    }
    Ok(latest)
}

pub(crate) fn lookup(address: &str) -> Option<CoreAgentSessionSnapshot> {
    default_sessions_path()
        .and_then(|path| lookup_from(&path, address).ok())
        .flatten()
}

pub(crate) fn executable_argv(argv: &[String]) -> io::Result<Vec<String>> {
    let Some(command) = argv.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent resume argv is empty",
        ));
    };
    if argv
        .iter()
        .any(|argument| argument.chars().any(char::is_control))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent resume argv contains control characters",
        ));
    }
    let mut resolved = argv.to_vec();
    if command == "kimi" {
        let executable = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".kimi-code/bin/kimi"))
            .filter(|path| path.is_file());
        let executable = executable.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Kimi executable was not found for core resume",
            )
        })?;
        resolved[0] = executable.to_string_lossy().into_owned();
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{executable_argv, lookup_from};

    fn test_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "herdr-native-kimi-resume-{}-{nonce}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn selects_latest_valid_record_for_the_core_address() {
        let path = test_path();
        std::fs::write(
            &path,
            concat!(
                "not json\n",
                "{\"terminal\":\"t1\",\"session_id\":\"session_old\",\"resume_argv\":[\"kimi\",\"--session\",\"session_old\"]}\n",
                "{\"terminal\":\"other\",\"session_id\":\"session_other\",\"resume_argv\":[\"kimi\",\"--session\",\"session_other\"]}\n",
                "{\"terminal\":\"t1\",\"session_id\":\"session_new\",\"resume_argv\":[\"kimi\",\"--session\",\"session_new\"]}\n"
            ),
        )
        .expect("write records");
        let record = lookup_from(&path, "t1")
            .expect("lookup")
            .expect("matching record");
        assert_eq!(record.session_id, "session_new");
        assert!(lookup_from(&path, "missing")
            .expect("missing lookup")
            .is_none());
        std::fs::remove_file(path).expect("remove test records");
    }

    #[test]
    fn resume_rejects_empty_or_control_argv() {
        assert!(executable_argv(&[]).is_err());
        assert!(executable_argv(&["kimi\n".into()]).is_err());
    }
}
