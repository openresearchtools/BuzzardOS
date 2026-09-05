# Buzzard CUA changelog

## 0.1.0

- Fix numbered-seat focus switching away from obstructing fullscreen windows
  without changing other workspaces or using the human seat.

- Initial independently versioned Buzzard CUA Debian package.
- Uses Buzzard CUA's own product version; the historical TryCua source version
  is retained only in the packaged provenance and license records.
- Renamed the installed product and executable from the upstream driver name.
- Reduced the driver to one daemonless Linux/Sway crate with numbered
  output/seat commands and detached application launches.
- Added output-local screenshots, coordinates, cursor state, bounded
  runtime-directory state, and daemonless Wayland clipboard ownership.
- Added global window output/workspace metadata, target-to-caller focus moves,
  exact post-launch bounds, and observable line/page scroll behavior.
