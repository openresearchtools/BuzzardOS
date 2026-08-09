//! Regression gate for Wild Buzzard's reviewed Linux-only TryCua source scope.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const WORKSPACE_MEMBERS: &[&str] = &[
    "cua-driver",
    "cua-driver-contract",
    "cua-driver-sdk",
    "cua-driver-testkit",
    "cua-driver-core",
    "platform-linux",
    "cursor-overlay",
    "pip-preview",
];

const SKILL_FILES: &[&str] = &[
    "BROWSER.md",
    "LINUX.md",
    "README.md",
    "RECORDING.md",
    "SKILL.md",
];

const EXCLUDED_PATHS: &[&str] = &[
    "cua-driver/rust/Skills/cua-driver/MACOS.md",
    "cua-driver/rust/Skills/cua-driver/WINDOWS.md",
    "cua-driver/rust/Skills/cua-driver/EMBEDDING.md",
    "cua-driver/rust/crates/cua-driver/build.rs",
    "cua-driver/rust/crates/cua-driver/cua-driver.manifest",
    "cua-driver/rust/crates/cua-driver/cua-driver.rc",
    "cua-driver/rust/crates/cua-driver-sdk/build.rs",
    "cua-driver/rust/crates/cua-driver-testkit/src/windows_setup.rs",
    "cua-driver/rust/crates/cua-driver/tests/agent_cursor_windows_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/desktop_scope_macos_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/desktop_scope_windows_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/harness_appkit_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/harness_libreoffice_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/harness_swiftui_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/harness_web_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/harness_winui3_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/harness_wpf_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/installed_app_launch_macos_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/installed_app_textedit_macos_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/launch_windows_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/protocol_handshake_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/protocol_media_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/protocol_session_test.rs",
    "cua-driver/rust/crates/cua-driver/tests/protocol_tools_call_test.rs",
];

fn fork_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("cua-driver crate must be nested under the vendored fork")
        .to_owned()
}

#[test]
fn workspace_and_skills_match_the_reviewed_linux_inventory() {
    let root = fork_root();
    let workspace = std::fs::read_to_string(root.join("cua-driver/rust/Cargo.toml"))
        .expect("read vendored Cargo workspace");
    let actual_members = workspace
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            line.strip_prefix('"')
                .and_then(|line| line.strip_suffix(","))
                .and_then(|line| line.strip_suffix('"'))
                .and_then(|line| line.strip_prefix("crates/"))
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let expected_members = WORKSPACE_MEMBERS
        .iter()
        .map(|member| (*member).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual_members, expected_members,
        "reviewed Linux workspace member inventory drifted"
    );
    for member in WORKSPACE_MEMBERS {
        let manifest = std::fs::read_to_string(
            root.join(format!("cua-driver/rust/crates/{member}/Cargo.toml")),
        )
        .unwrap_or_else(|error| panic!("read {member} manifest: {error}"));
        assert!(
            manifest.contains("license.workspace = true"),
            "{member} must inherit the preserved upstream MIT license metadata"
        );
    }
    for forbidden in ["platform-macos", "platform-windows", "cua-driver-uia"] {
        assert!(
            !workspace.contains(forbidden),
            "non-Linux workspace member returned: {forbidden}"
        );
    }

    let skill_dir = root.join("cua-driver/rust/Skills/cua-driver");
    let actual = std::fs::read_dir(&skill_dir)
        .expect("read Linux skill directory")
        .map(|entry| entry.expect("read skill entry").file_name())
        .filter_map(|name| name.to_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    let expected = SKILL_FILES
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected, "Linux skill file inventory drifted");
}

#[test]
fn platform_only_sources_and_direct_dependencies_remain_absent() {
    let root = fork_root();
    for relative in EXCLUDED_PATHS {
        assert!(
            !root.join(relative).exists(),
            "excluded non-Linux source returned: {relative}"
        );
    }

    for member in WORKSPACE_MEMBERS {
        let relative = format!("cua-driver/rust/crates/{member}/Cargo.toml");
        let manifest = std::fs::read_to_string(root.join(&relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        for forbidden in [
            "target_os = \"macos\"",
            "target_os = \"windows\"",
            "cfg(windows)",
            "objc2-app-kit",
            "core-foundation",
            "Win32_",
        ] {
            assert!(
                !manifest.contains(forbidden),
                "{relative} regained direct non-Linux dependency marker {forbidden}"
            );
        }
    }

    let scope = std::fs::read_to_string(root.join("LINUX_SCOPE.toml"))
        .expect("read machine-readable Linux scope record");
    assert!(scope.contains("status = \"reviewed-linux-only-source-subset\""));
    assert!(scope.contains("upstream_record = \"UPSTREAM.toml\""));
    for name in WORKSPACE_MEMBERS.iter().chain(SKILL_FILES) {
        assert!(
            scope.contains(&format!("\"{name}\"")),
            "machine-readable scope record is missing {name}"
        );
    }
}
