// SPDX-License-Identifier: AGPL-3.0-or-later

/// Compile-time host installation identity. Guest paths and protocols are not
/// renamed: they belong to the independently packaged guest components.
#[derive(Debug, PartialEq, Eq)]
pub struct HostIdentity {
    pub package: &'static str,
    pub display_executable: &'static str,
    pub application_id: &'static str,
    pub name: &'static str,
    pub manager_title: &'static str,
}

const STANDARD: HostIdentity = HostIdentity {
    package: "buzzardos",
    display_executable: "buzzardos-display",
    application_id: "org.openresearchtools.buzzardos",
    name: "Buzzard OS",
    manager_title: "Buzzard OS Machines",
};

const POD: HostIdentity = HostIdentity {
    package: "buzzardos-pod",
    display_executable: "buzzardos-pod-display",
    application_id: "org.openresearchtools.buzzardos-pod",
    name: "Buzzard OS (Podman)",
    manager_title: "Buzzard OS (Podman) Machines",
};

pub fn host_identity() -> &'static HostIdentity {
    match option_env!("BUZZARDOS_HOST_IDENTITY") {
        None | Some("buzzardos") => &STANDARD,
        Some("buzzardos-pod") => &POD,
        Some(_) => panic!("unsupported BUZZARDOS_HOST_IDENTITY build setting"),
    }
}

impl HostIdentity {
    pub fn helper_name<'a>(&self, name: &'a str) -> &'a str {
        match name {
            "buzzardos" => self.package,
            "buzzardos-display" => self.display_executable,
            _ => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_by_side_identities_do_not_alias() {
        assert_ne!(STANDARD.package, POD.package);
        assert_ne!(STANDARD.display_executable, POD.display_executable);
        assert_ne!(STANDARD.application_id, POD.application_id);
        assert_eq!(POD.helper_name("buzzardos"), "buzzardos-pod");
        assert_eq!(
            POD.helper_name("buzzardos-display"),
            "buzzardos-pod-display"
        );
        assert_eq!(POD.helper_name("podman"), "podman");
    }

    #[test]
    fn compiled_identity_matches_build_selection() {
        assert_eq!(
            host_identity().package,
            option_env!("BUZZARDOS_HOST_IDENTITY").unwrap_or("buzzardos")
        );
    }
}
