//! Input delivery policy for numbered Buzzard CUA seats.

use serde_json::Value;

/// Visual input defaults to the caller's numbered seat. The legacy explicit
/// `background` value remains accepted for semantic or Xwayland operations
/// that can prove target-addressed delivery without focus.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    Background,
    #[default]
    Foreground,
}

impl DeliveryMode {
    pub fn parse(arg: Option<&str>) -> Self {
        match arg {
            Some(value) if value.eq_ignore_ascii_case("background") => Self::Background,
            _ => Self::Foreground,
        }
    }

    pub fn from_args(args: &Value) -> Self {
        Self::parse(args.get("delivery_mode").and_then(Value::as_str))
    }

    pub fn is_foreground(self) -> bool {
        matches!(self, Self::Foreground)
    }
}

pub fn delivery_mode_schema() -> Value {
    let mut schema = crate::core::tool_schema::delivery_mode_schema_with(
        "Input delivery mode. The default 'foreground' moves the exact target \
         into this cuaN workspace and focuses it only for seatN before injecting \
         input. Desktop/seat0 and other numbered seats are unaffected. Explicit \
         'background' is a compatibility request and succeeds only when a \
         target-addressed semantic or Xwayland route can prove delivery.",
    );
    schema["default"] = serde_json::json!("foreground");
    schema
}

#[derive(Copy, Clone, Debug)]
pub enum BackgroundUnavailable {
    ChromiumInput,
    FocusedInputOnly,
    WebKitSyntheticInput,
}

impl BackgroundUnavailable {
    fn detail(self) -> &'static str {
        match self {
            Self::ChromiumInput => {
                "the application does not accept target-addressed background input"
            }
            Self::FocusedInputOnly => {
                "the available input route delivers only to this seat's focused surface"
            }
            Self::WebKitSyntheticInput => {
                "the application rejects target-addressed synthetic background input"
            }
        }
    }
}

pub fn background_unavailable_error(
    reason: BackgroundUnavailable,
) -> crate::core::protocol::ToolResult {
    let detail = reason.detail();
    crate::core::protocol::ToolResult::error(format!(
        "Background delivery is unavailable because {detail}. Retry without \
         delivery_mode, or pass delivery_mode:\"foreground\", to use this \
         numbered CUA seat."
    ))
    .with_structured(serde_json::json!({
        "code": "background_unavailable",
        "detail": detail,
        "suggestion": "Retry without delivery_mode or use delivery_mode:\"foreground\".",
        "escalation": {
            "recommended": "foreground",
            "reason": "seat-local focus is required for observable input delivery"
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_mode_defaults_to_numbered_seat_foreground() {
        assert_eq!(DeliveryMode::parse(None), DeliveryMode::Foreground);
        assert_eq!(
            DeliveryMode::parse(Some("unknown")),
            DeliveryMode::Foreground
        );
    }

    #[test]
    fn delivery_mode_accepts_both_compatible_values() {
        assert_eq!(
            DeliveryMode::parse(Some("background")),
            DeliveryMode::Background
        );
        assert_eq!(
            DeliveryMode::parse(Some("FOREGROUND")),
            DeliveryMode::Foreground
        );
    }

    #[test]
    fn delivery_mode_schema_advertises_numbered_seat_default() {
        let schema = delivery_mode_schema();
        assert_eq!(schema["type"], "string");
        assert_eq!(
            schema["enum"],
            serde_json::json!(["background", "foreground"])
        );
        assert_eq!(schema["default"], "foreground");
    }

    #[test]
    fn background_unavailable_error_carries_code() {
        let result = background_unavailable_error(BackgroundUnavailable::FocusedInputOnly);
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["code"],
            "background_unavailable"
        );
    }
}
