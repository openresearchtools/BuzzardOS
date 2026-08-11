// SPDX-License-Identifier: AGPL-3.0-or-later

use gio::prelude::*;
use glib::variant::ToVariant;
use wildbuzzard_desktop_core::UpdateState;

const BUS_NAME: &str = "org.openresearchtools.WildBuzzard.Updater1";
const OBJECT_PATH: &str = "/org/openresearchtools/WildBuzzard/Updater1";
const INTERFACE: &str = BUS_NAME;
const CALL_TIMEOUT_MILLISECONDS: i32 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateRequest {
    Check,
    InstallPlan(String),
}

impl UpdateRequest {
    fn method_name(&self) -> &'static str {
        match self {
            Self::Check => "Check",
            Self::InstallPlan(_) => "InstallPlan",
        }
    }

    fn parameters(&self) -> Result<Option<glib::Variant>, String> {
        match self {
            Self::Check => Ok(None),
            Self::InstallPlan(generation) => {
                validate_generation(generation)?;
                Ok(Some((generation.as_str(),).to_variant()))
            }
        }
    }
}

fn validate_generation(generation: &str) -> Result<(), String> {
    if generation.len() == 64
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("The selected updater plan does not have a canonical opaque generation.".into())
    }
}

async fn proxy() -> Result<gio::DBusProxy, String> {
    gio::DBusProxy::for_bus_future(
        gio::BusType::System,
        gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
        None,
        BUS_NAME,
        OBJECT_PATH,
        INTERFACE,
    )
    .await
    .map_err(|error| format!("Cannot connect to the fixed guest updater service: {error}"))
}

pub(crate) fn submit(request: UpdateRequest, callback: impl FnOnce(Result<u64, String>) + 'static) {
    glib::MainContext::default().spawn_local(async move {
        let result = async {
            let parameters = request.parameters()?;
            let method = request.method_name();
            let proxy = proxy().await?;
            let reply = proxy
                .call_future(
                    method,
                    parameters.as_ref(),
                    gio::DBusCallFlags::NONE,
                    CALL_TIMEOUT_MILLISECONDS,
                )
                .await
                .map_err(|error| format!("Updater {method} request was rejected: {error}"))?;
            let (accepted, state_generation) = reply
                .get::<(bool, u64)>()
                .ok_or_else(|| format!("Updater {method} returned an invalid reply."))?;
            if !accepted {
                return Err(format!("Updater {method} did not accept the request."));
            }
            Ok(state_generation)
        }
        .await;
        callback(result);
    });
}

pub(crate) fn get_state(callback: impl FnOnce(Result<UpdateState, String>) + 'static) {
    glib::MainContext::default().spawn_local(async move {
        let result = async {
            let proxy = proxy().await?;
            let reply = proxy
                .call_future(
                    "GetState",
                    None,
                    gio::DBusCallFlags::NONE,
                    CALL_TIMEOUT_MILLISECONDS,
                )
                .await
                .map_err(|error| format!("Cannot read updater state: {error}"))?;
            let state = reply
                .get::<(String,)>()
                .map(|(state,)| state)
                .ok_or_else(|| "Updater GetState returned an invalid reply.".to_owned())?;
            UpdateState::from_json_bytes(state.as_bytes())
                .map_err(|error| format!("Updater GetState returned invalid state: {error}"))
        }
        .await;
        callback(result);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_surface_is_fixed_and_generations_are_opaque() {
        let generation = "a".repeat(64);
        let cases = [
            (UpdateRequest::Check, "Check", false),
            (UpdateRequest::InstallPlan(generation), "InstallPlan", true),
        ];
        for (request, method, has_parameters) in cases {
            assert_eq!(request.method_name(), method);
            assert_eq!(request.parameters().unwrap().is_some(), has_parameters);
        }
        assert!(
            UpdateRequest::InstallPlan("../apt".into())
                .parameters()
                .is_err()
        );
    }

    #[test]
    fn endpoint_is_the_system_updater_only() {
        assert_eq!(BUS_NAME, "org.openresearchtools.WildBuzzard.Updater1");
        assert_eq!(OBJECT_PATH, "/org/openresearchtools/WildBuzzard/Updater1");
        assert_eq!(INTERFACE, BUS_NAME);
    }
}
