use clap::{Parser, Subcommand, ValueEnum};
use rustconsole_host_windows::firewall::FirewallScope;
use rustconsole_host_windows::service::{ServiceCommand, execute_service_command};
use std::fs;
use std::path::Path;
use std::process::ExitCode;

const INSTALL_ERROR_REPORT: &str = r"C:\ProgramData\RustConsole\install-error.txt";
const UNINSTALL_ERROR_REPORT: &str = r"C:\ProgramData\RustConsole\uninstall-error.txt";

#[derive(Debug, Parser)]
#[command(
    name = "rustconsole-host",
    version,
    about = "Rust Console Windows host"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<HostCommand>,
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    AudioProof,
    AudioEncodeProof,
    CaptureProof,
    DesktopTransitionProof,
    LoginTransitionProof,
    DisplayModeTransitionProof,
    OneFrameProof,
    Install,
    #[command(hide = true)]
    InstallElevated,
    InstallCaptureProof,
    InstallDesktopTransitionProof,
    InstallLoginTransitionProof,
    InstallDisplayModeTransitionProof,
    InstallOneFrameProof,
    CaptureImageProof,
    SetPassword,
    SetPasswordStdin,
    #[command(hide = true)]
    MediaWorker {
        command_read: usize,
        event_write: usize,
        audio_write: usize,
    },
    #[command(hide = true)]
    MediaWorkerNamed {
        control_pipe: String,
        audio_pipe: String,
        connection_token: String,
    },
    #[command(hide = true)]
    SessionControls {
        pipe: String,
        connection_token: String,
    },
    Uninstall,
    #[command(hide = true)]
    UninstallElevated,
    Firewall {
        #[command(subcommand)]
        command: FirewallCommand,
    },
}

#[derive(Debug, Subcommand)]
enum FirewallCommand {
    Status,
    Enable { scope: FirewallScopeArgument },
    Disable,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FirewallScopeArgument {
    PrivateLocalSubnet,
    AllProfilesLocalSubnet,
    AllAddresses,
}

fn service_command(command: Option<HostCommand>) -> ServiceCommand {
    match command {
        None => ServiceCommand::Run,
        Some(HostCommand::AudioProof) => ServiceCommand::RunAudioProof,
        Some(HostCommand::AudioEncodeProof) => ServiceCommand::RunAudioEncodeProof,
        Some(HostCommand::CaptureProof) => ServiceCommand::RunCaptureProof,
        Some(HostCommand::DesktopTransitionProof) => ServiceCommand::RunDesktopTransitionProof,
        Some(HostCommand::LoginTransitionProof) => ServiceCommand::RunLoginTransitionProof,
        Some(HostCommand::DisplayModeTransitionProof) => {
            ServiceCommand::RunDisplayModeTransitionProof
        }
        Some(HostCommand::OneFrameProof) => ServiceCommand::RunOneFrameProof,
        Some(HostCommand::Install) => ServiceCommand::Install,
        Some(HostCommand::InstallElevated) => ServiceCommand::InstallElevated,
        Some(HostCommand::InstallCaptureProof) => ServiceCommand::InstallCaptureProof,
        Some(HostCommand::InstallDesktopTransitionProof) => {
            ServiceCommand::InstallDesktopTransitionProof
        }
        Some(HostCommand::InstallLoginTransitionProof) => {
            ServiceCommand::InstallLoginTransitionProof
        }
        Some(HostCommand::InstallDisplayModeTransitionProof) => {
            ServiceCommand::InstallDisplayModeTransitionProof
        }
        Some(HostCommand::InstallOneFrameProof) => ServiceCommand::InstallOneFrameProof,
        Some(HostCommand::CaptureImageProof) => ServiceCommand::CaptureImageProof,
        Some(HostCommand::SetPassword) => ServiceCommand::SetPassword,
        Some(HostCommand::SetPasswordStdin) => ServiceCommand::SetPasswordStdin,
        Some(HostCommand::MediaWorker {
            command_read,
            event_write,
            audio_write,
        }) => ServiceCommand::MediaWorker {
            command_read,
            event_write,
            audio_write,
        },
        Some(HostCommand::MediaWorkerNamed {
            control_pipe,
            audio_pipe,
            connection_token,
        }) => ServiceCommand::MediaWorkerNamed {
            control_pipe,
            audio_pipe,
            connection_token,
        },
        Some(HostCommand::SessionControls {
            pipe,
            connection_token,
        }) => ServiceCommand::SessionControls {
            pipe,
            connection_token,
        },
        Some(HostCommand::Uninstall) => ServiceCommand::Uninstall,
        Some(HostCommand::UninstallElevated) => ServiceCommand::UninstallElevated,
        Some(HostCommand::Firewall {
            command: FirewallCommand::Status,
        }) => ServiceCommand::FirewallStatus,
        Some(HostCommand::Firewall {
            command: FirewallCommand::Disable,
        }) => ServiceCommand::FirewallDisable,
        Some(HostCommand::Firewall {
            command: FirewallCommand::Enable { scope },
        }) => ServiceCommand::FirewallEnable(match scope {
            FirewallScopeArgument::PrivateLocalSubnet => FirewallScope::PrivateLocalSubnet,
            FirewallScopeArgument::AllProfilesLocalSubnet => FirewallScope::AllProfilesLocalSubnet,
            FirewallScopeArgument::AllAddresses => FirewallScope::AllAddresses,
        }),
    }
}

fn main() -> ExitCode {
    let command = service_command(Cli::parse().command);
    let error_report = match command {
        ServiceCommand::InstallElevated => Some(INSTALL_ERROR_REPORT),
        ServiceCommand::UninstallElevated => Some(UNINSTALL_ERROR_REPORT),
        _ => None,
    };
    if let Some(error_report) = error_report {
        if let Some(parent) = Path::new(error_report).parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::remove_file(error_report);
    }
    match execute_service_command(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustconsole-host: {error}");
            if let Some(error_report) = error_report {
                let _ = fs::write(error_report, format!("{error}\n"));
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<ServiceCommand, clap::Error> {
        Cli::try_parse_from(arguments).map(|cli| service_command(cli.command))
    }

    #[test]
    fn existing_service_and_worker_commands_keep_their_shape() {
        assert_eq!(parse(&["host"]).unwrap(), ServiceCommand::Run);
        assert_eq!(
            parse(&["host", "audio-encode-proof"]).unwrap(),
            ServiceCommand::RunAudioEncodeProof
        );
        assert_eq!(
            parse(&["host", "media-worker", "12", "34", "56"]).unwrap(),
            ServiceCommand::MediaWorker {
                command_read: 12,
                event_write: 34,
                audio_write: 56,
            }
        );
        assert!(parse(&["host", "media-worker", "bad", "34", "56"]).is_err());
        assert_eq!(
            parse(&[
                "host",
                "media-worker-named",
                r"\\.\pipe\control",
                r"\\.\pipe\audio",
                "00112233445566778899aabbccddeeff",
            ])
            .unwrap(),
            ServiceCommand::MediaWorkerNamed {
                control_pipe: r"\\.\pipe\control".to_owned(),
                audio_pipe: r"\\.\pipe\audio".to_owned(),
                connection_token: "00112233445566778899aabbccddeeff".to_owned(),
            }
        );
        assert_eq!(
            parse(&[
                "host",
                "session-controls",
                r"\\.\pipe\session",
                "00112233445566778899aabbccddeeff",
            ])
            .unwrap(),
            ServiceCommand::SessionControls {
                pipe: r"\\.\pipe\session".to_owned(),
                connection_token: "00112233445566778899aabbccddeeff".to_owned(),
            }
        );
    }

    #[test]
    fn firewall_commands_accept_only_the_three_scopes() {
        assert_eq!(
            parse(&["host", "firewall", "enable", "all-profiles-local-subnet"]).unwrap(),
            ServiceCommand::FirewallEnable(FirewallScope::AllProfilesLocalSubnet)
        );
        assert_eq!(
            parse(&["host", "firewall", "status"]).unwrap(),
            ServiceCommand::FirewallStatus
        );
        assert_eq!(
            parse(&["host", "firewall", "disable"]).unwrap(),
            ServiceCommand::FirewallDisable
        );
        assert!(parse(&["host", "firewall", "enable", "internet"]).is_err());
    }
}
