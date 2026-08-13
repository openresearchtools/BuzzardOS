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
    host_uid: u32,
    host_gid: u32,
    subuid_start: u32,
    subgid_start: u32,
    mapping_helper_path: &'static str,
    namespace_backend: NamespaceBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamespaceBackend {
    Unshare,
    LxcUsernsExec,
}

const LXC_USERNS_EXEC: &str = "/usr/bin/lxc-usernsexec";
const APPARMOR_USERNS_RESTRICTION: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";

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
        let needed = GUEST_ID_COUNT - 1;
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
        let namespace_backend = namespace_backend()?;

        Ok(Self {
            host_uid,
            host_gid,
            subuid_start,
            subgid_start,
            mapping_helper_path,
            namespace_backend,
        })
    }

    /// util-linux unshare delegates subordinate-ID authorization to the
    /// host's trusted newuidmap/newgidmap gates. AppImages cannot safely
    /// manufacture setuid privilege, so use a validated system directory
    /// rather than inheriting a user-controlled PATH.
    pub fn configure_command<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command.env("PATH", self.mapping_helper_path)
    }

    /// Select the root-owned distro entry point required by Ubuntu's AppArmor
    /// user-namespace policy, or the bundled util-linux unshare elsewhere.
    pub fn namespace_program<'a>(&self, bundled_unshare: &'a Path) -> Result<&'a Path> {
        match self.namespace_backend {
            NamespaceBackend::Unshare => Ok(bundled_unshare),
            NamespaceBackend::LxcUsernsExec => {
                let program = Path::new(LXC_USERNS_EXEC);
                if !trusted_lxc_userns_exec(program) {
                    bail!(
                        "Ubuntu's AppArmor user-namespace restriction is active, but trusted {LXC_USERNS_EXEC} is unavailable; run ./Install-Dependencies"
                    );
                }
                // The path is process-static even though the caller's bundled
                // path has a shorter lifetime.
                Ok(Path::new(LXC_USERNS_EXEC))
            }
        }
    }

    /// Guest UID/GID 1000 maps to the host
    /// desktop user, while every other guest identity maps into subordinate
    /// ranges. The interactive guest can therefore access the portable data
    /// directory without giving guest root the host user's identity. Host
    /// Wayland access is separately scoped to the display gateway's private
    /// socket; the real host compositor socket never enters this namespace.
    pub fn namespace_args(&self) -> Vec<OsString> {
        let tail_count = GUEST_ID_COUNT - GUEST_USER_ID - 1;
        let user_maps = [
            format!("0:{}:{GUEST_USER_ID}", self.subuid_start),
            format!("{GUEST_USER_ID}:{}:1", self.host_uid),
            format!(
                "{}:{}:{tail_count}",
                GUEST_USER_ID + 1,
                self.subuid_start + GUEST_USER_ID
            ),
        ];
        let group_maps = [
            format!("0:{}:{GUEST_USER_ID}", self.subgid_start),
            format!("{GUEST_USER_ID}:{}:1", self.host_gid),
            format!(
                "{}:{}:{tail_count}",
                GUEST_USER_ID + 1,
                self.subgid_start + GUEST_USER_ID
            ),
        ];
        match self.namespace_backend {
            NamespaceBackend::Unshare => {
                let mut arguments = vec!["--user".into()];
                for mapping in user_maps {
                    arguments.extend([OsString::from("--map-users"), mapping.into()]);
                }
                for mapping in group_maps {
                    arguments.extend([OsString::from("--map-groups"), mapping.into()]);
                }
                arguments.extend([
                    "--setuid".into(),
                    "0".into(),
                    "--setgid".into(),
                    "0".into(),
                    "--".into(),
                ]);
                arguments
            }
            NamespaceBackend::LxcUsernsExec => {
                let mut arguments = Vec::new();
                for mapping in user_maps {
                    arguments.extend([OsString::from("-m"), format!("u:{mapping}").into()]);
                }
                for mapping in group_maps {
                    arguments.extend([OsString::from("-m"), format!("g:{mapping}").into()]);
                }
                arguments.push("--".into());
                arguments
            }
        }
    }
}

fn namespace_backend() -> Result<NamespaceBackend> {
    let restricted = match fs::read_to_string(APPARMOR_USERNS_RESTRICTION) {
        Ok(value) => value.trim() == "1",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).context("reading Ubuntu AppArmor user-namespace policy");
        }
    };
    if !restricted {
        return Ok(NamespaceBackend::Unshare);
    }
    if trusted_lxc_userns_exec(Path::new(LXC_USERNS_EXEC)) {
        Ok(NamespaceBackend::LxcUsernsExec)
    } else {
        bail!(
            "Ubuntu's AppArmor user-namespace restriction is active, but trusted {LXC_USERNS_EXEC} is unavailable; run ./Install-Dependencies"
        )
    }
}

fn trusted_lxc_userns_exec(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        !metadata.file_type().is_symlink()
            && metadata.is_file()
            && metadata.uid() == 0
            && metadata.permissions().mode() & 0o022 == 0
            && metadata.permissions().mode() & 0o111 != 0
    })
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
            host_uid: 1000,
            host_gid: 1000,
            subuid_start: 100_000,
            subgid_start: 200_000,
            mapping_helper_path: "/usr/bin",
            namespace_backend: NamespaceBackend::Unshare,
        };
        let arguments = map
            .namespace_args()
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-users", "0:100000:1000"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-users", "1000:1000:1"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-users", "1001:101000:64535"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-groups", "0:200000:1000"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-groups", "1000:1000:1"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--map-groups", "1001:201000:64535"] })
        );
    }

    #[test]
    fn restricted_ubuntu_backend_uses_lxc_mapping_syntax() {
        let map = IdMap {
            host_uid: 1000,
            host_gid: 1000,
            subuid_start: 100_000,
            subgid_start: 200_000,
            mapping_helper_path: "/usr/bin",
            namespace_backend: NamespaceBackend::LxcUsernsExec,
        };
        let arguments = map
            .namespace_args()
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "-m",
                "u:0:100000:1000",
                "-m",
                "u:1000:1000:1",
                "-m",
                "u:1001:101000:64535",
                "-m",
                "g:0:200000:1000",
                "-m",
                "g:1000:1000:1",
                "-m",
                "g:1001:201000:64535",
                "--"
            ]
        );
    }

    #[test]
    fn subordinate_range_overlap_is_detected_without_overflow() {
        assert!(range_contains(100_000, 65_535, 100_000));
        assert!(range_contains(100_000, 65_535, 165_534));
        assert!(!range_contains(100_000, 65_535, 165_535));
        assert!(!range_contains(u32::MAX - 1, 10, 0));
    }
}
