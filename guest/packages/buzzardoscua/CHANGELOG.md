# Buzzard CUA changelog

## 0.17.0+buzzard1

- Initial independently versioned Buzzard CUA Debian package.
- Renamed the installed product and executable from the upstream driver name.
- Reduced the driver to one daemonless Linux/Sway crate with numbered
  output/seat commands and detached application launches.
- Added output-local screenshots, coordinates, cursor state, bounded
  runtime-directory state, and daemonless Wayland clipboard ownership.
- Added global window output/workspace metadata, target-to-caller focus moves,
  exact post-launch bounds, and observable line/page scroll behavior.
