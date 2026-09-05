# buzzardos-guest changelog

## Unreleased

- Remove the desktop session's inherited systemd task ceiling with
  `TasksMax=infinity`; applications remain subject to the selected outer
  container limit and host resources. Other service policies are unchanged.

## 0.1.1

- Provision the official reference-image account as `user` at UID/GID 1000,
  while keeping package upgrades and machine lifecycle out of account setup.
- Add an exact guest-local, `visudo`-validated passwordless-sudo policy helper;
  it has no Polkit rule, host route, or generic privileged interface.
- Replace the broad guest Polkit authorization with private, socket-activated
  sudo and AppImage mount handoffs whose root services expose no generic RPC.
- Execute the distro's real sudo with ordinary machine-password
  authentication while preserving arguments, environment, working directory,
  TTY/non-TTY streams, signals, terminal size, and exact exit status.
- Restrict Type-2 AppImage FUSE requests to UID/GID 1000 peers, the pinned
  runtime argument shapes, validated communication descriptors, and
  read-only `nosuid,nodev` mounts below approved guest paths.

## 0.1.0

- Initial independently versioned guest-mechanics package using distro Sway
  and wlroots.
- Kept every active guest-only workspace output synchronized with native host
  window resizes and repacked their non-overlapping positions atomically.
