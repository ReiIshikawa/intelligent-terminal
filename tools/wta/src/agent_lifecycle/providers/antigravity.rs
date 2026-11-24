use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use futures::future::BoxFuture;

use super::{
    DependencyKind, InstallerKind, PreparationOutcome, PreparedArtifact, PreparedHostCommand,
    ProviderLifecycleAdapter,
};
use crate::agent_source::AgentSource;

pub(super) struct AntigravityProvider;
pub(super) static ADAPTER: AntigravityProvider = AntigravityProvider;

impl ProviderLifecycleAdapter for AntigravityProvider {
    fn family_id(&self) -> &'static str {
        crate::agent_registry::ANTIGRAVITY_AGENT_ID
    }

    fn dependency_kind(&self) -> DependencyKind {
        DependencyKind::StandaloneAcp
    }

    fn installer_kind(&self) -> InstallerKind {
        InstallerKind::StandaloneAcp
    }

    fn find_host_acp(&self) -> Option<String> {
        let profile = crate::agent_registry::lookup_profile_by_id(self.family_id());
        let path = crate::agent_check::spawn_path()
            .map(std::ffi::OsString::from)
            .or_else(|| std::env::var_os("PATH"))?;
        let resolved = crate::agent_check::find_standalone_acp_executable_in_path(
            profile,
            &path,
            Path::is_file,
        );
        if resolved.is_some() || has_server_on_path(&path) {
            return resolved.map(|candidate| candidate.to_string_lossy().into_owned());
        }
        crate::antigravity_acp::managed_host_executable()
            .map(|candidate| candidate.to_string_lossy().into_owned())
    }

    fn host_bootstrap_available(&self, launch_ready: bool) -> bool {
        crate::antigravity_acp::host_platform().is_ok()
            && !launch_ready
            && crate::agent_check::find_exe(self.family_id()).is_some()
            && (process_per_session_requested()
                || !crate::agent_check::spawn_path()
                    .is_some_and(|path| has_server_on_path(OsStr::new(&path))))
    }

    fn prepare<'a>(
        &'a self,
        command: &'a str,
        source: &'a AgentSource,
    ) -> BoxFuture<'a, Result<PreparedArtifact>> {
        Box::pin(async move {
            let windows_builtin = is_builtin_windows_command(command, source);
            if windows_builtin && process_per_session_requested() {
                if let Some(host_command) = native_host_command().await {
                    return Ok(PreparedArtifact {
                        host_command: Some(host_command),
                        ..PreparationOutcome::Ready.into()
                    });
                }
                tracing::debug!(
                    target: "antigravity_acp",
                    "native ACP process isolation unavailable; using standalone ACP"
                );
            }
            match crate::antigravity_acp::prepare(command, Some(self.family_id()), source).await? {
                Some(program) if windows_builtin => Ok(PreparedArtifact {
                    host_command: Some(PreparedHostCommand { program, args: &[] }),
                    ..PreparationOutcome::Ready.into()
                }),
                Some(executable) => Ok(PreparedArtifact {
                    executable: Some(executable),
                    ..PreparationOutcome::Ready.into()
                }),
                None => Ok(PreparationOutcome::NotRequired.into()),
            }
        })
    }
}

fn process_per_session_requested() -> bool {
    std::env::var_os("WTA_ANTIGRAVITY_PROCESS_PER_SESSION").as_deref() == Some(OsStr::new("1"))
}

fn is_builtin_windows_command(command: &str, source: &AgentSource) -> bool {
    matches!(source, AgentSource::Host) && command.trim().eq_ignore_ascii_case("agy_acp_server.exe")
}

async fn native_host_command() -> Option<PreparedHostCommand> {
    let program = crate::agent_check::find_exe(crate::agent_registry::ANTIGRAVITY_AGENT_ID)?;
    if !Path::new(&program).is_absolute() {
        return None;
    }
    // Older native CLIs may lack this subcommand or flag. Ask the exact binary
    // that would run and keep any probe descendants in a private process job.
    let mut help = tokio::process::Command::new(&program);
    help.args(["acp", "--help"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (child, _job) = crate::protocol::acp::process::ProcessJob::spawn(&mut help).ok()?;
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    // The Windows CLI writes its help to stderr even when it exits successfully.
    if !output.status.success()
        || ![output.stdout, output.stderr]
            .iter()
            .any(|help| supports_process_per_session(help))
    {
        return None;
    }
    Some(PreparedHostCommand {
        program,
        args: &["acp", "--process-per-session"],
    })
}

fn supports_process_per_session(help: &[u8]) -> bool {
    String::from_utf8_lossy(help)
        .lines()
        .any(|line| line.split_ascii_whitespace().next() == Some("--process-per-session"))
}

fn has_server_on_path(path: &OsStr) -> bool {
    std::env::split_paths(path).any(|directory| directory.join("agy_acp_server.exe").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_builtin_windows_command_can_switch_backends() {
        let host = AgentSource::Host;
        assert!(is_builtin_windows_command("AGY_ACP_SERVER.EXE", &host));
        assert!(!is_builtin_windows_command(
            r"C:\tools\agy_acp_server.exe",
            &host
        ));
        assert!(!is_builtin_windows_command(
            "agy_acp_server.exe --custom",
            &host
        ));
        assert!(!is_builtin_windows_command(
            "agy_acp_server.exe",
            &AgentSource::Wsl {
                distro: "Ubuntu".into(),
            }
        ));
    }

    #[test]
    fn worker_mode_requires_the_exact_help_option() {
        assert!(supports_process_per_session(
            b"Flags:\r\n  --process-per-session  Run each session in a worker\r\n"
        ));
        assert!(!supports_process_per_session(
            b"  --process-per-session-timeout  Expire idle workers"
        ));
        assert!(!supports_process_per_session(
            b"unknown flag --process-per-session"
        ));
    }
}
