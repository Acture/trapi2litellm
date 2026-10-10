use crate::{
	files::UPSTREAM_KEY,
	settings::{Mode, Settings},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
	pub model_list: Vec<Model>,
	pub litellm_settings: Value,
	pub router_settings: Value,
	pub general_settings: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
	pub model_name: String,
	pub litellm_params: Parameters,
	pub model_info: BTreeMap<String, Value>,
}

/// How LiteLLM calls a deployment: TRAPI's Azure endpoint with a Managed Identity token, or the
/// upstream gateway's LiteLLM proxy endpoint with its master key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
	untagged,
	expecting = "litellm_params with model, api_base and either api_version and azure_scope (managed identity) or api_key (gateway relay)"
)]
pub enum Parameters {
	Azure {
		model: String,
		api_base: String,
		api_version: String,
		azure_scope: String,
	},
	Relay {
		model: String,
		api_base: String,
		api_key: String,
	},
}

impl Parameters {
	fn new(settings: &Settings, name: &str) -> Self {
		match &settings.mode {
			Mode::ManagedIdentity => Self::Azure {
				model: format!("azure/{name}"),
				api_base: settings.base_url.clone(),
				api_version: settings.api_version.clone(),
				azure_scope: settings.scope.clone(),
			},
			// The upstream gateway serves the same deployment as `trapi/<name>`. LiteLLM's proxy
			// provider forwards every OpenAI parameter and route as is, so the upstream alone
			// validates requests; the `openai` provider would map them for an unknown model and
			// silently drop some, such as `tool_choice` on gpt-5.
			Mode::Gateway { upstream_url } => Self::Relay {
				model: format!("litellm_proxy/trapi/{name}"),
				api_base: format!("{upstream_url}/v1"),
				api_key: format!("os.environ/{UPSTREAM_KEY}"),
			},
		}
	}
}

pub fn digest(bytes: &[u8]) -> String {
	format!("{:x}", Sha256::digest(bytes))
}

pub fn model_names(config: &Config) -> BTreeSet<String> {
	config
		.model_list
		.iter()
		.map(|model| model.model_name.clone())
		.collect()
}

pub fn enabled(value: Option<&Value>) -> bool {
	match value {
		Some(Value::Bool(value)) => *value,
		Some(Value::String(value)) => value.eq_ignore_ascii_case("true"),
		_ => false,
	}
}

pub fn build_config(catalog: &Value, settings: &Settings) -> Result<Config> {
	let source: &str = match &settings.mode {
		Mode::ManagedIdentity => &settings.base_url,
		Mode::Gateway { upstream_url } => upstream_url,
	};
	let object = catalog.as_object().context("Catalog must be an object")?;
	for key in ["nextLink", "next_link", "@odata.nextLink", "next"] {
		ensure!(
			object
				.get(key)
				.is_none_or(|value| value.is_null() || value == "" || value == false),
			"Paginated catalog requires support before publishing a partial list"
		);
	}
	let entries: &Vec<Value> = object
		.get("data")
		.and_then(Value::as_array)
		.context("Empty or malformed catalog; keeping previous configuration")?;
	ensure!(
		!entries.is_empty(),
		"Empty or malformed catalog; keeping previous configuration"
	);
	let mut models: Vec<Model> = Vec::new();
	let mut seen: BTreeSet<String> = BTreeSet::new();
	for entry in entries {
		let entry = entry
			.as_object()
			.context("Catalog contains a non-object entry")?;
		let name: &str = entry
			.get("id")
			.and_then(Value::as_str)
			.context("Catalog contains an invalid deployment ID")?;
		ensure!(
			name.as_bytes()
				.first()
				.is_some_and(u8::is_ascii_alphanumeric)
				&& name
					.bytes()
					.all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte)),
			"Catalog contains an invalid deployment ID"
		);
		ensure!(
			!name.split('/').any(|part| part == "..") && seen.insert(name.to_owned()),
			"Catalog contains a duplicate or unsafe deployment ID"
		);
		if entry.get("provisioningState").and_then(Value::as_str) != Some("Succeeded") {
			continue;
		}
		let capabilities: Value = entry
			.get("capabilities")
			.filter(|value| !value.is_null())
			.cloned()
			.unwrap_or_else(|| json!({}));
		let capability_map = capabilities
			.as_object()
			.context("Malformed model capabilities")?;
		let upstream: Value = entry.get("model").cloned().unwrap_or(Value::Null);
		let upstream_model: Value = if upstream.is_null() {
			json!({})
		} else {
			upstream.clone()
		};
		let upstream_map = upstream_model
			.as_object()
			.context("Malformed model metadata")?;
		let mut info: BTreeMap<String, Value> = BTreeMap::from([
			(
				"id".into(),
				json!(digest(format!("{source}/{name}").as_bytes())),
			),
			("source".into(), json!("trapi")),
			("upstream_deployment".into(), json!(name)),
			("provisioning_state".into(), json!("Succeeded")),
			("capabilities".into(), capabilities.clone()),
			("upstream_model".into(), upstream),
			(
				"rate_limits".into(),
				entry.get("RateLimits").cloned().unwrap_or(Value::Null),
			),
			(
				"availability_evidence".into(),
				json!("catalog_only_not_an_inference_health_check"),
			),
		]);
		if upstream_map.get("Format").and_then(Value::as_str) == Some("OpenAI")
			&& let Some(base) = upstream_map
				.get("Name")
				.and_then(Value::as_str)
				.filter(|name| !name.is_empty())
		{
			info.insert("base_model".into(), json!(format!("azure/{base}")));
		}
		for (capability, mode) in [
			("chatCompletion", "chat"),
			("responses", "responses"),
			("embeddings", "embedding"),
		] {
			if enabled(capability_map.get(capability)) {
				info.insert("mode".into(), json!(mode));
				break;
			}
		}
		models.push(Model {
			model_name: format!("trapi/{name}"),
			litellm_params: Parameters::new(settings, name),
			model_info: info,
		});
	}
	ensure!(
		!models.is_empty(),
		"No provisioned deployments; keeping previous configuration"
	);
	models.sort_by(|left, right| left.model_name.cmp(&right.model_name));
	let mut litellm_settings: Value = json!({"drop_params": false, "telemetry": false, "set_verbose": false, "turn_off_message_logging": true});
	if settings.mode == Mode::ManagedIdentity {
		litellm_settings["enable_azure_ad_token_refresh"] = json!(true);
	}
	Ok(Config {
		model_list: models,
		litellm_settings,
		router_settings: json!({"num_retries": 0, "timeout": 600, "fallbacks": []}),
		general_settings: json!({"master_key": "os.environ/LITELLM_MASTER_KEY", "disable_spend_logs": true}),
	})
}

#[cfg(test)]
pub fn catalog(names: &[&str]) -> Value {
	json!({"data": names.iter().map(|name| json!({"id": name, "provisioningState": "Succeeded", "capabilities": {"chatCompletion": "true"}})).collect::<Vec<Value>>()})
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn catalog_policy_and_stability() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		assert_eq!(
			build_config(&catalog(&["b", "a"]), &settings).unwrap(),
			build_config(&catalog(&["a", "b"]), &settings).unwrap()
		);
		for data in [
			json!({"data": []}),
			json!({"data": [null]}),
			catalog(&["a", "a"]),
			json!({"data": [{"id": "a", "provisioningState": "Succeeded", "model": "bad"}]}),
			json!({"data": [{"id": "a", "provisioningState": "Failed"}]}),
		] {
			assert!(build_config(&data, &settings).is_err());
		}
		for name in ["../x", "a/../b", "https://bad.invalid", "a\nfoo", "a?key=x"] {
			assert!(build_config(&catalog(&[name]), &settings).is_err());
		}
		let mut data: Value = catalog(&["a"]);
		data["nextLink"] = json!("https://example.invalid/page2");
		assert!(build_config(&data, &settings).is_err());
	}
	#[test]
	fn metadata_and_dated_slash_ids() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let mut data: Value = catalog(&["Qwen/Qwen3.5-9B", "gpt-5.2_2025-12-11"]);
		data["data"][1]["model"] = json!({"Format": "OpenAI", "Name": "gpt-5.2"});
		data["data"][1]["capabilities"] = json!({"responses": true, "maxContextToken": "1234"});
		let config: Config = build_config(&data, &settings).unwrap();
		for (model, name) in config
			.model_list
			.iter()
			.zip(["Qwen/Qwen3.5-9B", "gpt-5.2_2025-12-11"])
		{
			assert_eq!(
				model.litellm_params,
				Parameters::Azure {
					model: format!("azure/{name}"),
					api_base: settings.base_url.clone(),
					api_version: settings.api_version.clone(),
					azure_scope: settings.scope.clone(),
				}
			);
		}
		assert_eq!(
			config.model_list[1].model_info["base_model"],
			"azure/gpt-5.2"
		);
		assert_eq!(config.model_list[1].model_info["mode"], "responses");
		assert_eq!(
			config.model_list[1].model_info["capabilities"],
			data["data"][1]["capabilities"]
		);
		assert!(
			!serde_json::to_string(&config)
				.unwrap()
				.contains("fetched_at")
		);
		assert_eq!(
			config.general_settings["master_key"],
			"os.environ/LITELLM_MASTER_KEY"
		);
		data["data"][0]["provisioningState"] = json!("Failed");
		assert_eq!(build_config(&data, &settings).unwrap().model_list.len(), 1);
		data["data"][1]["capabilities"] = json!({});
		assert!(
			!build_config(&data, &settings).unwrap().model_list[0]
				.model_info
				.contains_key("mode")
		);
	}
	#[test]
	fn gateway_mode_relays_through_the_upstream_proxy() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_gateway_settings(root.path());
		let direct: Settings = crate::settings::test_settings(root.path());
		let mut data: Value = catalog(&["Qwen/Qwen3.5-9B", "gpt-5.2_2025-12-11"]);
		data["data"][1]["model"] = json!({"Format": "OpenAI", "Name": "gpt-5.2"});
		data["data"][1]["capabilities"] = json!({"responses": true});
		data["data"][1]["RateLimits"] = json!({"TokensPerMinute": 1000});
		let config: Config = build_config(&data, &settings).unwrap();
		let managed: Config = build_config(&data, &direct).unwrap();
		for ((model, managed), name) in config
			.model_list
			.iter()
			.zip(&managed.model_list)
			.zip(["Qwen/Qwen3.5-9B", "gpt-5.2_2025-12-11"])
		{
			assert_eq!(model.model_name, format!("trapi/{name}"));
			assert_eq!(
				serde_json::to_value(&model.litellm_params).unwrap(),
				json!({
					"model": format!("litellm_proxy/trapi/{name}"),
					"api_base": "http://127.0.0.1:14000/v1",
					"api_key": "os.environ/TRAPI2LITELLM_UPSTREAM_KEY",
				})
			);
			assert_eq!(
				model.model_info["id"],
				digest(format!("http://127.0.0.1:14000/{name}").as_bytes())
			);
			let mut info: BTreeMap<String, Value> = model.model_info.clone();
			info.insert("id".into(), managed.model_info["id"].clone());
			assert_eq!(info, managed.model_info);
		}
		assert_eq!(config.model_list[1].model_info["mode"], "responses");
		assert_eq!(
			config.model_list[1].model_info["base_model"],
			"azure/gpt-5.2"
		);
		assert_eq!(
			config.model_list[1].model_info["rate_limits"],
			json!({"TokensPerMinute": 1000})
		);
		assert_eq!(
			config.litellm_settings,
			json!({"drop_params": false, "telemetry": false, "set_verbose": false, "turn_off_message_logging": true})
		);
		assert_eq!(config.router_settings, managed.router_settings);
		assert_eq!(config.general_settings, managed.general_settings);
		let text: String = serde_json::to_string(&config).unwrap();
		for absent in [
			"azure_scope",
			"api_version",
			"enable_azure_ad_token_refresh",
		] {
			assert!(!text.contains(absent), "{absent}");
		}
		// Synchronization reads published configurations back.
		assert_eq!(serde_json::from_str::<Config>(&text).unwrap(), config);
		assert_eq!(
			serde_json::from_str::<Config>(&serde_json::to_string(&managed).unwrap()).unwrap(),
			managed
		);
		assert!(
			serde_json::from_value::<Parameters>(json!({"model": "azure/a", "api_base": "x"}))
				.unwrap_err()
				.to_string()
				.contains("either api_version and azure_scope (managed identity) or api_key")
		);
	}

	/// The relay configuration tests/test_relay.py loads into the real LiteLLM proxy, so a change
	/// here reaches the request-forwarding checks there.
	#[test]
	fn gateway_config_matches_the_relay_snapshot() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_gateway_settings(root.path());
		let openai = |name: &str| -> Value { json!({"Format": "OpenAI", "Name": name}) };
		let catalog: Value = json!({"data": [
			{"id": "gpt-5.2_2025-12-11", "provisioningState": "Succeeded", "capabilities": {"chatCompletion": "true"}, "model": openai("gpt-5.2")},
			{"id": "gpt-4o-mini_2024-07-18", "provisioningState": "Succeeded", "capabilities": {"chatCompletion": "true"}, "model": openai("gpt-4o-mini")},
			{"id": "o3-mini_2025-01-31", "provisioningState": "Succeeded", "capabilities": {"chatCompletion": "true"}, "model": openai("o3-mini")},
			{"id": "gpt-5-codex_2025-09-15", "provisioningState": "Succeeded", "capabilities": {"responses": "true"}, "model": openai("gpt-5-codex")},
			{"id": "text-embedding-3-large_1", "provisioningState": "Succeeded", "capabilities": {"embeddings": "true"}, "model": openai("text-embedding-3-large")},
			{"id": "Qwen/Qwen3.5-9B", "provisioningState": "Succeeded", "capabilities": {"chatCompletion": "true"}},
		]});
		assert_eq!(
			String::from_utf8(
				crate::sync::render(&build_config(&catalog, &settings).unwrap()).unwrap()
			)
			.unwrap(),
			include_str!("../tests/snapshots/gateway/config.json")
		);
	}
}
