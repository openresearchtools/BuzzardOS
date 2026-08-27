// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use nix::unistd::{Gid, Uid, User};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

const GUEST_USER_ID: u32 = 1000;
const GUEST_ID_COUNT: u32 = 65_536;

#[derive(Debug, Clone, Copy)]
pub struct IdMap {
    subuid_start: u32,
    subgid_start: u32,
    mapping_helper_path: &'static str,
}

impl IdMap {
    pub fn discover() -> Result<Self> {
        let host_uid = Uid::current().as_raw();
        let host_gid = Gid::current().as_raw();
        let user = User::from_uid(Uid::current())
            .context("looking up current user")?
            .context("current uid has no passwd entry")?;
        let uid_owner = user.name;
        let numeric_owner = host_uid.to_string();

        let (subuid_start, subuid_count) =
            find_range(Path::new("/etc/subuid"), &uid_owner, &numeric_owner)?;
        let (subgid_start, subgid_count) =
            find_range(Path::new("/etc/subgid"), &uid_owner, &numeric_owner)?;
        let needed = GUEST_ID_COUNT;
        if subuid_count < needed || subgid_count < needed {
            bail!("account '{uid_owner}' needs at least {needed} subordinate UIDs and GIDs");
        }
        if range_contains(subuid_start, needed, host_uid)
            || range_contains(subgid_start, needed, host_gid)
        {
            bail!(
                "account '{uid_owner}' has a subordinate ID range overlapping its host UID/GID; a non-overlapping range is required for guest UID/GID {GUEST_USER_ID} keep-id mapping"
            );
        }
        let mapping_helper_path = mapping_helper_path()?;
        Ok(Self {
            subuid_start,
            subgid_start,
            mapping_helper_path,
        })
    }

    /// util-linux unshare delegates subordinate-ID authorization to the
    /// host's trusted newuidmap/newgidmap gates. Buzzard OS never manufactures
    /// setuid privilege, so use a validated system directory rather than
    /// inheriting a user-controlled PATH.
    pub fn configure_command<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command.env("PATH", self.mapping_helper_path)
    }

    /// Use the validated distribution-provided namespace executable.
    /// Subordinate-ID authorization still goes through the host's trusted
    /// newuidmap/newgidmap gates selected above.
    pub fn namespace_program<'a>(&self, unshare: &'a Path) -> Result<&'a Path> {
        Ok(unshare)
    }

    /// Guest UID/GID 1000 maps to the host
    /// desktop user, while every other guest identity maps into subordinate
    /// ranges. The interactive guest can therefore access explicitly shared
    /// host paths without giving guest root the host user's identity. Host
    /// Wayland access is separately scoped to the display gateway's private
    /// socket; the real host compositor socket never enters this namespace.
    pub fn namespace_args(&self) -> Vec<OsString> {
        vec![
            "--user".into(),
            "--map-users".into(),
            format!("0:{}:{GUEST_ID_COUNT}", self.subuid_start).into(),
            "--map-user".into(),
            GUEST_USER_ID.to_string().into(),
            "--map-groups".into(),
            format!("0:{}:{GUEST_ID_COUNT}", self.subgid_start).into(),
            "--map-group".into(),
            GUEST_USER_ID.to_string().into(),
            "--mount".into(),
            "--setuid".into(),
            "0".into(),
            "--setgid".into(),
            "0".into(),
            "--".into(),
        ]
    }

    /// Build the first of Bubblewrap's two user namespaces.
    ///
    /// Namespace root maps to the invoking desktop user so Bubblewrap can
    /// resolve and mount a machine selected below a private host directory.
    /// The complete subordinate ranges are also made visible so the final
    /// guest namespace can be created as this namespace's child.
    pub fn mount_setup_namespace_args(&self) -> Vec<OsString> {
        vec![
            "--user".into(),
            "--map-users".into(),
            format!("1:{}:{GUEST_ID_COUNT}", self.subuid_start).into(),
            "--map-user".into(),
            "0".into(),
            "--map-groups".into(),
            format!("1:{}:{GUEST_ID_COUNT}", self.subgid_start).into(),
            "--map-group".into(),
            "0".into(),
            "--mount".into(),
            "--setuid".into(),
            "0".into(),
            "--setgid".into(),
            "0".into(),
            "--".into(),
        ]
    }

    /// Build the durable guest identity namespace below the mount-setup
    /// namespace. Setup ID 0 is the host desktop user and setup IDs 1 onward
    /// are the authorized subordinate range, so the usual guest keep-id map
    /// is expressed relative to those parent IDs.
    pub fn guest_namespace_args_from_mount_setup(&self) -> Vec<OsString> {
        vec![
            "--user".into(),
            "--map-users".into(),
            format!("0:1:{GUEST_ID_COUNT}").into(),
            "--map-user".into(),
            GUEST_USER_ID.to_string().into(),
            "--map-groups".into(),
            format!("0:1:{GUEST_ID_COUNT}").into(),
            "--map-group".into(),
            GUEST_USER_ID.to_string().into(),
            "--mount".into(),
            "--setuid".into(),
            "0".into(),
            "--setgid".into(),
            "0".into(),
            "--".into(),
        ]
    }
}

fn range_contains(start: u32, count: u32, id: u32) -> bool {
    id >= start && id.checked_sub(start).is_some_and(|offset| offset < count)
}

fn mapping_helper_path() -> Result<&'static str> {
    for directory in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        if ["newuidmap", "newgidmap"]
            .iter()
            .all(|name| trusted_mapping_helper(&Path::new(directory).join(name)))
        {
            return Ok(directory);
        }
    }
    bail!(
        "host lacks trusted newuidmap/newgidmap gates for subordinate IDs; this kernel/userspace configuration is unsupported"
    )
}

fn trusted_mapping_helper(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.uid() == 0
            && metadata.permissions().mode() & 0o111 != 0
            && (metadata.permissions().mode() & libc::S_ISUID != 0 || Uid::effective().is_root())
    })
}

fn find_range(path: &Path, username: &str, numeric_owner: &str) -> Result<(u32, u32)> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for line in contents.lines() {
        let mut fields = line.split(':');
        let Some(owner) = fields.next() else {
            continue;
        };
        if owner != username && owner != numeric_owner {
            continue;
        }
        let start: u32 = fields
            .next()
            .context("subordinate ID entry has no start")?
            .parse()
            .context("invalid subordinate ID start")?;
        let count: u32 = fields
            .next()
            .context("subordinate ID entry has no count")?
            .parse()
            .context("invalid subordinate ID count")?;
        return Ok((start, count));
    }
    bail!(
        "{} has no subordinate ID range for account '{username}'",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_guest_identity_is_kept_as_the_host_user() {
        let map = IdMap {
            subuid_start: 100_000,
            subgid_start: 200_000,
            mapping_helper_path: "/usr/bin",
        };
        let arguments = map
            .namespace_args()
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-users", "0:100000:65536"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-user", "1000"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-groups", "0:200000:65536"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-group", "1000"] })
        );
        assert!(arguments.iter().any(|argument| argument == "--mount"));
    }

    #[test]
    fn two_level_map_mounts_as_host_user_then_enters_guest_ids() {
        let map = IdMap {
            subuid_start: 100_000,
            subgid_start: 200_000,
            mapping_helper_path: "/usr/bin",
        };
        let setup = map
            .mount_setup_namespace_args()
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let guest = map
            .guest_namespace_args_from_mount_setup()
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            setup
                .windows(2)
                .any(|pair| pair == ["--map-users", "1:100000:65536"])
        );
        assert!(setup.windows(2).any(|pair| pair == ["--map-user", "0"]));
        assert!(
            setup
                .windows(2)
                .any(|pair| pair == ["--map-groups", "1:200000:65536"])
        );
        assert!(
            guest
                .windows(2)
                .any(|pair| pair == ["--map-users", "0:1:65536"])
        );
        assert!(guest.windows(2).any(|pair| pair == ["--map-user", "1000"]));
    }

    #[test]
    fn subordinate_range_overlap_is_detected_without_overflow() {
        assert!(range_contains(100_000, 65_536, 100_000));
        assert!(range_contains(100_000, 65_536, 165_535));
        assert!(!range_contains(100_000, 65_536, 165_536));
        assert!(!range_contains(u32::MAX - 1, 10, 0));
    }
}
