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
}

impl CoreTerminal {
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
    fn accepts_flat_terminal_array_for_compatible_core_clients() {
        let response: CoreListResponse =
            serde_json::from_str(r#"[{"address":"t7","state":"live"}]"#).expect("flat response");
        assert_eq!(response.terminals()[0].address, "t7");
    }
}
