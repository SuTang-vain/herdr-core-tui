use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus};

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct CoreTerminal {
    pub address: String,
    pub name: Option<String>,
    pub state: Option<String>,
    pub status: Option<serde_json::Value>,
    pub agent: Option<String>,
    pub cwd: Option<String>,
    pub turn: Option<u64>,
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
}

pub(crate) const SPACE_LABEL: &str = "herdr.space";
pub(crate) const TAB_LABEL: &str = "herdr.tab";
pub(crate) const WHERE_LABEL: &str = "where";

/// Split a `where` label ("space / tab") into its workspace and tab names.
pub(crate) fn parse_where(value: &str) -> Option<(String, String)> {
    let (workspace, tab) = value.split_once(" / ")?;
    let workspace = workspace.trim();
    let tab = tab.trim();
    if workspace.is_empty() || tab.is_empty() {
        return None;
    }
    Some((workspace.to_string(), tab.to_string()))
}

impl CoreTerminal {
    /// Human-readable workspace/tab organization shared with other core clients.
    pub(crate) fn where_parts(&self) -> Option<(String, String)> {
        self.labels.get(WHERE_LABEL).and_then(|v| parse_where(v))
    }

    pub(crate) fn space_label(&self) -> Option<&str> {
        self.labels.get(SPACE_LABEL).map(String::as_str)
    }

    pub(crate) fn tab_label(&self) -> Option<&str> {
        self.labels.get(TAB_LABEL).map(String::as_str)
    }
    pub(crate) fn status_state(&self) -> Option<&str> {
        match self.status.as_ref()? {
            serde_json::Value::String(state) => Some(state),
            serde_json::Value::Object(status) => status.get("state")?.as_str(),
            _ => None,
        }
    }

    pub(crate) fn status_app(&self) -> Option<&str> {
        match self.status.as_ref()? {
            serde_json::Value::Object(status) => status.get("app")?.as_str(),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CoreListResponse {
    Detailed { terminals: Vec<CoreTerminal> },
    Flat(Vec<CoreTerminal>),
}

impl CoreListResponse {
    fn terminals(self) -> Vec<CoreTerminal> {
        match self {
            Self::Detailed { terminals } => terminals,
            Self::Flat(terminals) => terminals,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CoreDiscovery {
    cli: OsString,
}

impl CoreDiscovery {
    pub(crate) fn cli(&self) -> &std::ffi::OsStr {
        &self.cli
    }

    pub(crate) fn from_env() -> Self {
        Self {
            cli: std::env::var_os("HERDR_CORE_CLI").unwrap_or_else(|| OsString::from("hc")),
        }
    }

    pub(crate) fn attach_argv(&self, address: &str) -> Vec<String> {
        vec![
            self.cli.to_string_lossy().into_owned(),
            "attach".into(),
            "--history".into(),
            "200".into(),
            address.into(),
        ]
    }

    pub(crate) fn get_terminal(&self, address: &str) -> io::Result<CoreTerminal> {
        let output = Command::new(&self.cli)
            .args(["get", "--json", address])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let kind = if stderr.contains("not_found") {
                io::ErrorKind::NotFound
            } else {
                io::ErrorKind::NotConnected
            };
            return Err(io::Error::new(
                kind,
                format!("could not read core terminal {address}: {}", stderr.trim()),
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid core terminal {address}: {err}"),
            )
        })
    }

    pub(crate) fn set_labels(&self, address: &str, labels: &[String]) -> io::Result<()> {
        if labels.is_empty() {
            return Ok(());
        }
        let output = Command::new(&self.cli)
            .arg("label")
            .arg(address)
            .args(labels)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "could not label core terminal {address}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    pub(crate) fn kill_terminal(&self, address: &str) -> io::Result<()> {
        let output = Command::new(&self.cli).args(["kill", address]).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "could not kill core terminal {address}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    pub(crate) fn attach(&self, address: &str) -> io::Result<ExitStatus> {
        Command::new(&self.cli)
            .args(["attach", "--history", "200", address])
            .status()
            .map_err(|err| {
                io::Error::other(format!(
                    "could not attach to core terminal {address}: {err}"
                ))
            })
    }

    pub(crate) fn create_terminal(&self, cwd: &Path) -> io::Result<CoreTerminal> {
        self.create_terminal_command(cwd, &[], None)
    }

    pub(crate) fn create_agent_terminal(
        &self,
        cwd: &Path,
        argv: &[String],
        resumed_from: &str,
    ) -> io::Result<CoreTerminal> {
        self.create_terminal_command(cwd, argv, Some(resumed_from))
    }

    fn create_terminal_command(
        &self,
        cwd: &Path,
        argv: &[String],
        resumed_from: Option<&str>,
    ) -> io::Result<CoreTerminal> {
        let mut command = Command::new(&self.cli);
        command
            .arg("run")
            .arg("--json")
            .arg("--bg")
            .arg("--no-parent")
            .arg("--no-notify")
            .arg("--label")
            .arg("herdr.client=core-native")
            .arg("--cwd")
            .arg(cwd);
        if let Some(previous) = resumed_from {
            command
                .arg("--label")
                .arg(format!("herdr.resumed-from={previous}"));
        }
        if !argv.is_empty() {
            command.arg("--").args(argv);
        }
        let output = command
            .output()
            .map_err(|err| io::Error::other(format!("could not create core terminal: {err}")))?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "could not create core terminal: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let terminal: CoreTerminal = serde_json::from_slice(&output.stdout).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("herdr-core returned an invalid new terminal: {err}"),
            )
        })?;
        if terminal.address.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "herdr-core returned an empty terminal address",
            ));
        }
        Ok(terminal)
    }

    /// `hc ls` omits labels, so organization-aware restore needs one `get`
    /// per terminal. Failures keep the list entry (adoption stays possible).
    pub(crate) fn enrich(&self, terminals: Vec<CoreTerminal>) -> Vec<CoreTerminal> {
        terminals
            .into_iter()
            .map(|core| {
                if core.state.as_deref() == Some("live") && core.labels.is_empty() {
                    self.get_terminal(&core.address).unwrap_or(core)
                } else {
                    core
                }
            })
            .collect()
    }

    pub(crate) fn discover(&self) -> io::Result<Vec<CoreTerminal>> {
        let output = Command::new(&self.cli)
            .args(["ls", "--json"])
            .output()
            .map_err(|err| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("herdr-core unavailable: could not run hc: {err}"),
                )
            })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!(
                    "herdr-core unavailable: hc ls failed ({}){}",
                    output.status,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                ),
            ));
        }
        let response: CoreListResponse = serde_json::from_slice(&output.stdout).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("herdr-core returned an invalid terminal list: {err}"),
            )
        })?;
        Ok(response.terminals())
    }
}

#[cfg(test)]
mod tests {
    use super::CoreListResponse;

    #[test]
    fn parses_current_core_list_and_status_shapes() {
        let response: CoreListResponse = serde_json::from_str(
            r#"{"terminals":[{"address":"t1","name":"herdr","status":"idle"},{"address":"t2","status":{"app":"kimi-code","state":"working"},"turn":3}]}"#,
        )
        .expect("current core response");
        let terminals = response.terminals();
        assert_eq!(terminals.len(), 2);
        assert_eq!(terminals[0].status_state(), Some("idle"));
        assert_eq!(terminals[1].status_app(), Some("kimi-code"));
        assert_eq!(terminals[1].status_state(), Some("working"));
        assert_eq!(terminals[1].turn, Some(3));
    }

    #[test]
    fn parses_where_labels_into_workspace_and_tab_names() {
        let terminal: super::CoreTerminal = serde_json::from_str(
            r#"{"address":"t8","labels":{"herdr.space":"sp1","herdr.tab":"tb1","where":"main / 工作台"}}"#,
        )
        .expect("labeled terminal");
        assert_eq!(
            terminal.where_parts(),
            Some(("main".to_string(), "工作台".to_string()))
        );
        assert_eq!(terminal.space_label(), Some("sp1"));
        assert_eq!(terminal.tab_label(), Some("tb1"));

        assert!(super::parse_where("no separator").is_none());
        assert!(super::parse_where(" / tab").is_none());
        assert!(super::parse_where("space / ").is_none());
        assert_eq!(
            super::parse_where("a / b / c"),
            Some(("a".to_string(), "b / c".to_string()))
        );
    }

    #[test]
    fn accepts_flat_terminal_array_for_compatible_core_clients() {
        let response: CoreListResponse =
            serde_json::from_str(r#"[{"address":"t7","state":"live"}]"#).expect("flat response");
        assert_eq!(response.terminals()[0].address, "t7");
    }
}
