// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native Podman mounts for external filesystems behind host-only parent ACLs.
//! Only image construction needs the short-lived image mount owner. Machine
//! startup uses its permanent, empty runtime anchor and no additional process.

use super::{Podman, append_bind_mount};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use uuid::Uuid;

pub(super) fn append_rootfs(
    arguments: &mut Vec<OsString>,
    source: &Path,
    anchor: &Path,
    read_only: bool,
) {
    append_bind_mount(arguments, source, Path::new("/"), read_only);
    arguments.push("--rootfs".into());
    arguments.push(anchor.as_os_str().to_owned());
}

impl Podman {
    pub(super) fn with_rootfs_anchor<T>(
        &self,
        operation: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is required for a rootfs operation")?;
        if !runtime.is_absolute() {
            bail!("XDG_RUNTIME_DIR must be absolute");
        }
        let parent = runtime
            .join(crate::host_identity().package)
            .join("rootfs-operations");
        fs::create_dir_all(&parent).context("creating private rootfs-operation directory")?;
        let path = parent.join(Uuid::new_v4().simple().to_string());
        fs::create_dir(&path).context("creating empty rootfs anchor")?;
        let result = operation(&path);
        // Native crun may have created empty mount targets owned by mapped
        // guest IDs. Podman owns their removal too; no custom UID translation.
        let cleanup = self.remove_external_tree(&path);
        combine(result, cleanup)
    }

    pub(super) fn with_image_root<T>(
        &self,
        image: &str,
        operation: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        // Rootless overlay mounts live in Podman's own mount namespace.
        // Keep its native unshare command alive while crun acquires the image
        // through that process's /proc root. No source-image binary runs and
        // no host mount, copied rootfs or manually constructed namespace exists.
        // Arguments are positional, never interpolated into shell source.
        const MOUNT: &str = r#"
set -eu
source=$("$1" --runtime "$2" image mount "$3")
trap '"$1" --runtime "$2" image unmount "$3" >/dev/null' EXIT
printf '/proc/%s/root%s\n' "$$" "$source"
read -r finished || :
"#;
        let child = self
            .command()
            .args(["unshare", "/bin/sh", "-c", MOUNT, "buzzardos-image-mount"])
            .arg(&self.executable)
            .arg(&self.oci_runtime)
            .arg(image)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("mounting image with native rootless Podman")?;
        let mut owner = ImageMountOwner(Some(child));
        let stdout = owner.0.as_mut().unwrap().stdout.take().unwrap();
        let mut source = String::new();
        BufReader::new(stdout.take(65_536))
            .read_line(&mut source)
            .context("reading Podman's image mount location")?;
        let path = PathBuf::from(source.trim_end_matches('\n'));
        let result = if path.starts_with("/proc") && path.is_dir() {
            operation(&path)
        } else {
            Err(anyhow::anyhow!(
                "Podman did not provide a reachable image rootfs"
            ))
        };
        combine(result, owner.finish())
    }
}

struct ImageMountOwner(Option<Child>);

impl ImageMountOwner {
    fn finish(&mut self) -> Result<()> {
        let Some(mut child) = self.0.take() else {
            return Ok(());
        };
        drop(child.stdin.take());
        let output = child
            .wait_with_output()
            .context("releasing native Podman image mount")?;
        if !output.status.success() {
            bail!(
                "Podman image mount owner exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

impl Drop for ImageMountOwner {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn combine<T>(result: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("mount cleanup also failed: {cleanup:#}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_is_a_direct_bind_and_anchor_is_only_the_native_root() {
        let mut args = vec!["--userns=keep-id:uid=1000,gid=1000".into()];
        append_rootfs(
            &mut args,
            Path::new("/media/data/rootfs"),
            Path::new("/run/anchor"),
            false,
        );
        assert_eq!(
            args,
            Vec::<OsString>::from([
                "--userns=keep-id:uid=1000,gid=1000".into(),
                "--mount".into(),
                "type=bind,src=/media/data/rootfs,dst=/".into(),
                "--rootfs".into(),
                "/run/anchor".into(),
            ])
        );
    }

    #[test]
    fn readonly_operations_quote_mount_paths_without_shell_interpolation() {
        let mut args = Vec::new();
        append_rootfs(
            &mut args,
            Path::new("/media/data,a/rootfs"),
            Path::new("/run/anchor"),
            true,
        );
        assert_eq!(
            args[1],
            "type=bind,src=\"/media/data,a/rootfs\",dst=/,ro=true"
        );
    }

    #[test]
    fn cleanup_errors_are_not_hidden_by_success_or_failure() {
        assert!(combine(Ok(()), Err(anyhow::anyhow!("cleanup"))).is_err());
        let error = combine::<()>(
            Err(anyhow::anyhow!("operation")),
            Err(anyhow::anyhow!("cleanup")),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("operation"));
        assert!(message.contains("cleanup"));
    }
}
