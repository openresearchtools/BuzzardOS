// SPDX-License-Identifier: AGPL-3.0-or-later

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "Buzzard OS",
    version,
    about = "Persistent rootless Podman desktop machines"
)]
pub(crate) struct Cli {
    /// Exact directory containing this machine's metadata and external rootfs.
    #[arg(long, global = true)]
    pub(crate) machine_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Pull an image and create a persistent machine.
    Create(CreateArguments),
    /// Pull an image and create a persistent machine.
    Pull(PullArguments),
    /// Build a Containerfile and create a persistent machine.
    Build {
        name: String,
        #[arg(long)]
        context: PathBuf,
        #[arg(long)]
        file: Option<PathBuf>,
        #[command(flatten)]
        machine: NewMachineArguments,
    },
    /// Start the existing persistent Podman container.
    Start {
        name: String,
        /// Return after the native window and Podman container have started.
        #[arg(long)]
        detach: bool,
    },
    /// Stop the existing persistent Podman container.
    Stop { name: String },
    /// Restart the existing persistent Podman container.
    Restart { name: String },
    /// Import a Podman-supported archive or image reference.
    Import {
        source: String,
        #[arg(long)]
        name: String,
        #[arg(long, value_enum, default_value_t = ImportMode::Restore)]
        mode: ImportMode,
        #[command(flatten)]
        machine: NewMachineArguments,
    },
    /// Export a stopped machine as an OCI archive.
    Export {
        name: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Clone a stopped machine into a newly selected directory.
    Clone {
        source: String,
        name: String,
        #[command(flatten)]
        machine: NewMachineArguments,
    },
    /// Permanently delete a stopped machine and its external rootfs.
    Delete {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Control the native machine window.
    Window {
        name: String,
        #[arg(value_enum)]
        action: WindowAction,
    },
    /// Print current Podman-derived machine status.
    Status { name: String },
    /// List registered machines.
    List,
    /// Register one existing self-describing machine directory.
    Register,
    /// Remove a machine from the manager without deleting it.
    Unregister { name: String },
    /// Check Podman, Wayland, media, and device prerequisites.
    Doctor,
}

#[derive(Debug, clap::Args)]
pub(crate) struct CreateArguments {
    pub(crate) name: String,
    /// Image reference passed directly to `podman pull`.
    #[arg(long)]
    pub(crate) image: String,
    #[command(flatten)]
    pub(crate) machine: NewMachineArguments,
}

#[derive(Debug, clap::Args)]
pub(crate) struct PullArguments {
    pub(crate) name: String,
    /// Image reference passed directly to `podman pull`.
    pub(crate) image: String,
    #[command(flatten)]
    pub(crate) machine: NewMachineArguments,
}

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct NewMachineArguments {
    /// Host file or folder to mount under `/shared`; repeat as needed.
    #[arg(long = "share")]
    pub(crate) shares: Vec<PathBuf>,
    #[arg(long, value_enum, default_value_t = NetworkArgument::User)]
    pub(crate) network: NetworkArgument,
    /// Native Podman GPU selection; repeat or use a comma-separated value.
    #[arg(long = "gpu", value_delimiter = ',')]
    pub(crate) gpus: Vec<String>,
    /// Unrestricted native `podman create` arguments, parsed directly to argv.
    #[arg(long = "podman-arguments", allow_hyphen_values = true)]
    pub(crate) podman_arguments: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum NetworkArgument {
    User,
    Host,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ImportMode {
    Restore,
    Clone,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum WindowAction {
    Minimize,
    Maximize,
    Restore,
    FocusMonitor,
    ToggleMaximize,
    Close,
}

impl WindowAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Restore => "restore",
            Self::FocusMonitor => "focus-monitor",
            Self::ToggleMaximize => "toggle-maximize",
            Self::Close => "close",
        }
    }
}

impl From<NetworkArgument> for wb_core::NetworkMode {
    fn from(value: NetworkArgument) -> Self {
        match value {
            NetworkArgument::User => Self::User,
            NetworkArgument::Host => Self::Host,
            NetworkArgument::None => Self::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn podman_arguments_accept_native_hyphen_leading_values_verbatim() {
        let cli = Cli::try_parse_from([
            "buzzardos",
            "build",
            "fixture",
            "--context",
            "/tmp/context",
            "--podman-arguments",
            "--userns=keep-id:uid=1000,gid=1000 --env WLR_RENDERER=gles2",
        ])
        .unwrap();
        let Some(Command::Build { machine, .. }) = cli.command else {
            panic!("build subcommand was not parsed");
        };
        assert_eq!(
            machine.podman_arguments.as_deref(),
            Some("--userns=keep-id:uid=1000,gid=1000 --env WLR_RENDERER=gles2")
        );
    }
}
