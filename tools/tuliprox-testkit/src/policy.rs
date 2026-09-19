use crate::{
    config::{GraceContract, PolicyContract},
    oracle::{AdmissionStrategy, GraceMode},
    TestkitError,
};
use serde_json::Value;
use std::collections::HashMap;

/// Validate the persisted fixture configuration before any playback is started.
///
/// The v1 configuration endpoint intentionally returns configuration files, rather
/// than the atomically loaded runtime configuration.  The controller therefore
/// uses this only for isolated fixtures it started itself; a remote instance needs
/// runtime evidence and is reported as inconclusive by the caller.
pub fn validate_fixture_policy(contract: &PolicyContract, config: &Value) -> Result<(), TestkitError> {
    let configured_access_control = required_bool(config, "/config/user_access_control")?;
    if configured_access_control != contract.user_access_control {
        return mismatch("user_access_control", contract.user_access_control, configured_access_control);
    }

    let stream = required_object(config, "/config/reverse_proxy/stream")?;
    let configured_strategies = stream
        .get("admission_strategies")
        .map(|strategies| {
            serde_json::from_value::<Vec<AdmissionStrategy>>(strategies.clone())
                .map_err(|error| TestkitError::Protocol(format!("invalid fixture admission_strategies: {error}")))
        })
        .transpose()?;
    if configured_strategies != contract.admission_strategies {
        return Err(TestkitError::Configuration(format!(
            "fixture admission_strategies mismatch: scenario {:?}, fixture {:?}",
            contract.admission_strategies, configured_strategies
        )));
    }

    if let Some(grace) = &contract.grace {
        validate_grace(grace, stream, &contract.effective_admission_strategies())?;
    }

    let configured_users = index_users(config)?;
    for (username, expected) in &contract.users {
        let configured = configured_users
            .get(username.as_str())
            .ok_or_else(|| TestkitError::Configuration(format!("fixture policy does not contain user {username}")))?;
        let max_connections = usize::try_from(required_u64(configured, "max_connections")?).map_err(|_| {
            TestkitError::Protocol("fixture max_connections cannot be represented on this platform".to_owned())
        })?;
        let soft_connections = usize::try_from(required_u64(configured, "soft_connections")?).map_err(|_| {
            TestkitError::Protocol("fixture soft_connections cannot be represented on this platform".to_owned())
        })?;
        if max_connections != expected.max_connections || soft_connections != expected.soft_connections {
            return Err(TestkitError::Configuration(format!(
                "fixture policy mismatch for user {username}: scenario max/soft {}/{}, fixture {}/{}",
                expected.max_connections, expected.soft_connections, max_connections, soft_connections
            )));
        }
    }

    if let Some(expected_limit) = contract.provider_max_connections {
        validate_provider_limit(expected_limit.into(), config)?;
    }

    Ok(())
}

fn validate_provider_limit(expected_limit: usize, config: &Value) -> Result<(), TestkitError> {
    let inputs = config
        .pointer("/sources/inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| TestkitError::Protocol("fixture configuration omits sources.inputs".to_owned()))?;

    let matching_input = inputs
        .iter()
        .find(|input| {
            input.get("name").and_then(Value::as_str) == Some("testkit-origin")
                || input.get("max_connections").is_some()
        })
        .ok_or_else(|| {
            TestkitError::Configuration(
                "fixture sources do not contain a provider input with max_connections".to_owned(),
            )
        })?;

    let configured_limit = matching_input
        .get("max_connections")
        .and_then(Value::as_u64)
        .ok_or_else(|| TestkitError::Configuration("fixture provider input omits max_connections".to_owned()))?;

    let configured_limit = usize::try_from(configured_limit).map_err(|_| {
        TestkitError::Protocol("fixture provider max_connections cannot be represented on this platform".to_owned())
    })?;

    if configured_limit != expected_limit {
        return Err(TestkitError::Configuration(format!(
            "fixture provider max_connections mismatch: scenario {expected_limit}, fixture {configured_limit}"
        )));
    }
    Ok(())
}

fn validate_grace(
    expected: &GraceContract,
    stream: &serde_json::Map<String, Value>,
    strategies: &[AdmissionStrategy],
) -> Result<(), TestkitError> {
    const DEFAULT_GRACE_PERIOD_MILLIS: u64 = 2000;
    const DEFAULT_GRACE_HOLD_STREAM: bool = true;

    let timeout = match stream.get("grace_period_millis") {
        Some(value) => value.as_u64().ok_or_else(|| {
            TestkitError::Protocol(
                "fixture configuration omits integer config.reverse_proxy.stream.grace_period_millis".to_owned(),
            )
        })?,
        None => DEFAULT_GRACE_PERIOD_MILLIS,
    };
    if timeout != expected.timeout_millis {
        return Err(TestkitError::Configuration(format!(
            "fixture grace_period_millis mismatch: scenario {}, fixture {timeout}",
            expected.timeout_millis
        )));
    }
    let hold = match stream.get("grace_period_hold_stream") {
        Some(value) => value.as_bool().ok_or_else(|| {
            TestkitError::Protocol(
                "fixture configuration omits boolean config.reverse_proxy.stream.grace_period_hold_stream".to_owned(),
            )
        })?,
        None => DEFAULT_GRACE_HOLD_STREAM,
    };
    let expected_hold = expected.mode == GraceMode::HoldStream;
    if hold != expected_hold {
        return mismatch("grace_period_hold_stream", expected_hold, hold);
    }
    // A grace mode without its corresponding strategy would be silently ignored
    // by the SUT after capacity is exhausted.
    let grace_strategy = match expected.mode {
        GraceMode::Instant => "grace_instant_stream",
        GraceMode::HoldStream => "grace_hold_stream",
    };
    let encoded = serde_json::to_value(strategies)
        .map_err(|error| TestkitError::Protocol(format!("cannot encode admission strategies: {error}")))?;
    if !encoded.as_array().is_some_and(|values| values.iter().any(|value| value == grace_strategy)) {
        return Err(TestkitError::Configuration(format!(
            "fixture policy grace mode requires {grace_strategy} admission strategy"
        )));
    }
    Ok(())
}

fn index_users(config: &Value) -> Result<HashMap<&str, &Value>, TestkitError> {
    let users = config
        .pointer("/api_proxy/user")
        .and_then(Value::as_array)
        .ok_or_else(|| TestkitError::Protocol("fixture configuration omits api_proxy.user".to_owned()))?;
    let mut indexed = HashMap::new();
    for target in users {
        let credentials = target
            .get("credentials")
            .and_then(Value::as_array)
            .ok_or_else(|| TestkitError::Protocol("fixture api_proxy user entry omits credentials".to_owned()))?;
        for user in credentials {
            let username = user
                .get("username")
                .and_then(Value::as_str)
                .ok_or_else(|| TestkitError::Protocol("fixture user omits username".to_owned()))?;
            if indexed.insert(username, user).is_some() {
                return Err(TestkitError::Configuration(format!("fixture has duplicate username {username}")));
            }
        }
    }
    Ok(indexed)
}

fn required_object<'a>(value: &'a Value, pointer: &str) -> Result<&'a serde_json::Map<String, Value>, TestkitError> {
    value
        .pointer(pointer)
        .and_then(Value::as_object)
        .ok_or_else(|| TestkitError::Protocol(format!("fixture configuration omits object {pointer}")))
}

fn required_bool(value: &Value, pointer: &str) -> Result<bool, TestkitError> {
    value
        .pointer(pointer)
        .and_then(Value::as_bool)
        .ok_or_else(|| TestkitError::Protocol(format!("fixture configuration omits boolean {pointer}")))
}

fn required_u64(value: &Value, key: &str) -> Result<u64, TestkitError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| TestkitError::Protocol(format!("fixture user omits integer {key}")))
}

fn mismatch<T: std::fmt::Display>(field: &str, expected: T, actual: T) -> Result<(), TestkitError> {
    Err(TestkitError::Configuration(format!("fixture {field} mismatch: scenario {expected}, fixture {actual}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserPolicy;
    use serde_json::json;

    fn contract() -> PolicyContract {
        PolicyContract {
            user_access_control: true,
            users: HashMap::from([("alice".to_owned(), UserPolicy { max_connections: 2, soft_connections: 1 })]),
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpLatest]),
            recent_eviction_reentry_ttl_ms: None,
            grace: None,
            provider_max_connections: None,
            expected_provider_slots: None,
        }
    }

    fn fixture() -> Value {
        json!({
            "config": {"user_access_control": true, "reverse_proxy": {"stream": {"admission_strategies": ["evict_user_same_ip_latest"]}}},
            "api_proxy": {"user": [{"credentials": [{"username": "alice", "max_connections": 2, "soft_connections": 1}]}]}
        })
    }

    #[test]
    fn accepts_matching_fixture_policy() {
        assert!(validate_fixture_policy(&contract(), &fixture()).is_ok());
    }

    #[test]
    fn rejects_mismatched_user_limit() {
        let mut config = fixture();
        config["api_proxy"]["user"][0]["credentials"][0]["max_connections"] = json!(3);
        assert!(validate_fixture_policy(&contract(), &config).is_err());
    }

    #[test]
    fn distinguishes_omitted_and_explicit_admission_strategies() {
        let mut config = fixture();
        config["config"]["reverse_proxy"]["stream"].as_object_mut().unwrap().remove("admission_strategies");
        assert!(matches!(validate_fixture_policy(&contract(), &config), Err(TestkitError::Configuration(_))));

        let mut omitted_contract = contract();
        omitted_contract.admission_strategies = None;
        assert!(validate_fixture_policy(&omitted_contract, &config).is_ok());
    }

    #[test]
    fn validates_provider_max_connections() {
        let mut contract_with_provider = contract();
        contract_with_provider.provider_max_connections = Some(2);

        let mut config_matching = fixture();
        config_matching["sources"] = json!({
            "inputs": [{"name": "testkit-origin", "max_connections": 2}]
        });
        assert!(validate_fixture_policy(&contract_with_provider, &config_matching).is_ok());

        let mut config_mismatched = fixture();
        config_mismatched["sources"] = json!({
            "inputs": [{"name": "testkit-origin", "max_connections": 1}]
        });
        assert!(validate_fixture_policy(&contract_with_provider, &config_mismatched).is_err());

        let mut config_missing = fixture();
        config_missing["sources"] = json!({
            "inputs": [{"name": "other-origin"}]
        });
        assert!(validate_fixture_policy(&contract_with_provider, &config_missing).is_err());
    }
}
