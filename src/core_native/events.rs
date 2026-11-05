use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;

use serde::Deserialize;

use super::discovery::{CoreDiscovery, CoreTerminal};

#[derive(Debug)]
pub(crate) enum CoreEvent {
    Snapshot(Vec<CoreTerminal>),
    Changed(CoreChange),
    Disconnected(String),
}

#[derive(Debug)]
pub(crate) struct CoreChange {
    pub kind: String,
    pub address: String,
    pub data: serde_json::Value,
}

#[derive(Deserialize)]
struct EventEnvelope {
    #[serde(default)]
    terminals: Option<Vec<CoreTerminal>>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<EventParams>,
}

#[derive(Deserialize)]
struct EventParams {
    #[serde(rename = "type")]
    kind: String,
    terminal: String,
    #[serde(default)]
    data: serde_json::Value,
}

fn parse_event(line: &str) -> Option<CoreEvent> {
    let envelope: EventEnvelope = serde_json::from_str(line).ok()?;
    if let Some(terminals) = envelope.terminals {
        return Some(CoreEvent::Snapshot(terminals));
    }
    if envelope.method.as_deref() != Some("event") {
        return None;
    }
    envelope.params.map(|params| {
        CoreEvent::Changed(CoreChange {
            kind: params.kind,
            address: params.terminal,
            data: params.data,
        })
    })
}

pub(crate) struct CoreEventListener {
    child: Child,
    reader: Option<JoinHandle<()>>,
}

impl CoreEventListener {
    pub(crate) fn spawn(
        discovery: &CoreDiscovery,
    ) -> std::io::Result<(Self, mpsc::Receiver<CoreEvent>)> {
        let mut child = Command::new(discovery.cli())
            .args(["events", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("core events stdout is unavailable"))?;
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ =
                            sender.send(CoreEvent::Disconnected("core event stream ended".into()));
                        return;
                    }
                    Ok(_) => match parse_event(&line) {
                        Some(event) => {
                            if sender.send(event).is_err() {
                                return;
                            }
                        }
                        None => {
                            let _ = sender.send(CoreEvent::Disconnected(format!(
                                "unrecognized core event: {}",
                                line.trim()
                            )));
                        }
                    },
                    Err(err) => {
                        let _ = sender.send(CoreEvent::Disconnected(format!(
                            "core event stream failed: {err}"
                        )));
                        return;
                    }
                }
            }
        });
        Ok((
            Self {
                child,
                reader: Some(reader),
            },
            receiver,
        ))
    }
}

impl Drop for CoreEventListener {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_event, CoreEvent};

    #[test]
    fn parses_snapshot_and_incremental_status() {
        let snapshot =
            parse_event(r#"{"seq":1,"terminals":[{"address":"t1"}]}"#).expect("snapshot event");
        assert!(matches!(snapshot, CoreEvent::Snapshot(_)));

        let changed = parse_event(
            r#"{"jsonrpc":"2.0","method":"event","params":{"type":"status","terminal":"t1","data":{"app":"kimi-code","state":"working"}}}"#,
        )
        .expect("status event");
        let CoreEvent::Changed(change) = changed else {
            panic!("expected change");
        };
        assert_eq!(change.kind, "status");
        assert_eq!(change.address, "t1");
        assert_eq!(change.data["state"], "working");
    }
}
