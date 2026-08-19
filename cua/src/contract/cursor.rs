// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Visual state used by the optional in-guest agent cursor.

use serde::{Deserialize, Serialize};

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum CursorAction {
    #[default]
    Idle,
    Observe,
    Click,
    Drag,
    Scroll,
    Text,
    Key,
    Navigate,
    App,
    Transfer,
    System,
}

impl CursorAction {
    pub const ALL: [Self; 11] = [
        Self::Idle,
        Self::Observe,
        Self::Click,
        Self::Drag,
        Self::Scroll,
        Self::Text,
        Self::Key,
        Self::Navigate,
        Self::App,
        Self::Transfer,
        Self::System,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Observe => "observe",
            Self::Click => "click",
            Self::Drag => "drag",
            Self::Scroll => "scroll",
            Self::Text => "text",
            Self::Key => "key",
            Self::Navigate => "navigate",
            Self::App => "app",
            Self::Transfer => "transfer",
            Self::System => "system",
        }
    }

    pub const fn playback(self) -> CursorPlayback {
        match self {
            Self::Idle => CursorPlayback::Resting,
            Self::Observe | Self::Scroll | Self::Transfer => CursorPlayback::Loop,
            Self::Drag | Self::Text => CursorPlayback::Held,
            Self::Click | Self::Key | Self::Navigate | Self::App | Self::System => {
                CursorPlayback::OneShot
            }
        }
    }

    pub const fn duration_secs(self) -> f64 {
        match self {
            Self::Idle => 4.0,
            Self::Click => 0.67,
            Self::Observe
            | Self::Drag
            | Self::Scroll
            | Self::Text
            | Self::Key
            | Self::Navigate
            | Self::App
            | Self::Transfer
            | Self::System => 1.6,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CursorPlayback {
    Resting,
    OneShot,
    Held,
    Loop,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CursorDelivery {
    Background,
    Foreground,
}

impl CursorDelivery {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Foreground => "foreground",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CursorTarget {
    Ax,
    Pixel,
    Desktop,
}

#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum CursorReducedMotion {
    #[default]
    Auto,
    On,
    Off,
}

impl CursorTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ax => "ax",
            Self::Pixel => "pixel",
            Self::Desktop => "desktop",
        }
    }
}
