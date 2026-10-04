use anyhow::{Context, Result, ensure};
use std::{
	collections::BTreeMap,
	env,
	path::{Component, Path, PathBuf},
};
use url::Url;

pub const SERVICE: &str = "litellm-trapi.service";

#[derive(Clone, Debug)]
pub struct Settings {
	pub config_dir: PathBuf,
	pub state_dir: PathBuf,
	pub user_config_home: PathBuf,
	pub base_url: String,
	pub api_version: String,
	pub catalog_version: String,
	pub scope: String,
	pub client_id: Option<String>,
	pub port: u16,
}

#[derive(Default)]
pub struct Overrides {
	pub config_dir: Option<PathBuf>,
	pub state_dir: Option<PathBuf>,
	pub port: Option<u16>,
}

pub fn absolute(path: &Path, home: &Path) -> Result<PathBuf> {
	let expanded: PathBuf = if path == Path::new("~") {
		home.to_owned()
	} else if let Ok(rest) = path.strip_prefix("~/") {
		home.join(rest)
	} else {
		path.to_owned()
	};
	let full: PathBuf = if expanded.is_absolute() {
		expanded
	} else {
		env::current_dir()?.join(expanded)
	};
	let mut normalized: PathBuf = PathBuf::new();
	for component in full.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				normalized.pop();
			}
			value => normalized.push(value.as_os_str()),
		}
		if normalized.exists() {
			normalized = normalized.canonicalize()?;
		}
	}
	Ok(normalized)
}

impl Settings {
	pub fn from_env(overrides: Overrides) -> Result<Self> {
		Self::from_map(&env::vars().collect(), overrides)
	}

	pub fn from_map(values: &BTreeMap<String, String>, overrides: Overrides) -> Result<Self> {
		let get = |key: &str, fallback: &str| -> String {
			values
				.get(key)
				.cloned()
				.unwrap_or_else(|| fallback.to_owned())
		};
		let home: PathBuf = PathBuf::from(values.get("HOME").context("HOME is required")?);
		let user_config_home: PathBuf = absolute(
			&PathBuf::from(get(
				"XDG_CONFIG_HOME",
				&home.join(".config").to_string_lossy(),
			)),
			&home,
		)?;
		let state_home: PathBuf = absolute(
			&PathBuf::from(get(
				"XDG_STATE_HOME",
				&home.join(".local/state").to_string_lossy(),
			)),
			&home,
		)?;
		let config_dir: PathBuf = absolute(
			&overrides.config_dir.unwrap_or_else(|| {
				PathBuf::from(get(
					"TRAPI2LITELLM_CONFIG_DIR",
					&user_config_home.join("litellm-trapi").to_string_lossy(),
				))
			}),
			&home,
		)?;
		let state_dir: PathBuf = absolute(
			&overrides.state_dir.unwrap_or_else(|| {
				PathBuf::from(get(
					"TRAPI2LITELLM_STATE_DIR",
					&state_home.join("trapi2litellm").to_string_lossy(),
				))
			}),
			&home,
		)?;
		let base_url: String = get(
			"TRAPI_BASE_URL",
			"https://trapi.research.microsoft.com/redmond/interactive",
		)
		.trim_end_matches('/')
		.to_owned();
		let parsed: Url = Url::parse(&base_url).context("Invalid TRAPI_BASE_URL")?;
		ensure!(
			parsed.scheme() == "https"
				&& parsed.host_str().is_some()
				&& parsed.username().is_empty()
				&& parsed.password().is_none()
				&& parsed.query().is_none()
				&& parsed.fragment().is_none(),
			"TRAPI_BASE_URL must be an HTTPS base URL without credentials, query or fragment"
		);
		let port: u16 = overrides.port.map_or_else(
			|| {
				get("TRAPI2LITELLM_PORT", "4000")
					.parse()
					.context("Invalid TRAPI2LITELLM_PORT")
			},
			Ok,
		)?;
		ensure!(
			port >= 1024,
			"TRAPI2LITELLM_PORT must be between 1024 and 65535"
		);
		Ok(Self {
			config_dir,
			state_dir,
			user_config_home,
			base_url,
			port,
			api_version: get("TRAPI_API_VERSION", "2025-04-01-preview"),
			catalog_version: get("TRAPI_CATALOG_VERSION", "preview"),
			scope: get("TRAPI_SCOPE", "api://trapi/.default"),
			client_id: values
				.get("AZURE_CLIENT_ID")
				.filter(|value| !value.is_empty())
				.cloned(),
		})
	}
	pub fn config_path(&self) -> PathBuf {
		self.config_dir.join("config.yaml")
	}
	pub fn key_path(&self) -> PathBuf {
		self.config_dir.join("gateway.env")
	}
	pub fn local_url(&self) -> String {
		format!("http://127.0.0.1:{}", self.port)
	}
	pub fn catalog_url(&self) -> String {
		let mut url: Url =
			Url::parse(&format!("{}/openai/models", self.base_url)).expect("validated URL");
		url.query_pairs_mut()
			.append_pair("api-version", &self.catalog_version);
		url.into()
	}
	pub fn environment(&self) -> BTreeMap<String, String> {
		let mut values: BTreeMap<String, String> = BTreeMap::from([
			(
				"TRAPI2LITELLM_CONFIG_DIR".into(),
				self.config_dir.to_string_lossy().into(),
			),
			(
				"TRAPI2LITELLM_STATE_DIR".into(),
				self.state_dir.to_string_lossy().into(),
			),
			("TRAPI2LITELLM_PORT".into(), self.port.to_string()),
			("TRAPI_BASE_URL".into(), self.base_url.clone()),
			("TRAPI_API_VERSION".into(), self.api_version.clone()),
			("TRAPI_CATALOG_VERSION".into(), self.catalog_version.clone()),
			("TRAPI_SCOPE".into(), self.scope.clone()),
		]);
		if let Some(id) = &self.client_id {
			values.insert("AZURE_CLIENT_ID".into(), id.clone());
		}
		values
	}
}

#[cfg(test)]
pub fn test_settings(root: &Path) -> Settings {
	Settings::from_map(
		&BTreeMap::from([("HOME".into(), root.to_string_lossy().into())]),
		Overrides {
			config_dir: Some(root.join("config")),
			state_dir: Some(root.join("state")),
			port: None,
		},
	)
	.unwrap()
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn defaults_and_invalid_environment() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = test_settings(root.path());
		assert_eq!(settings.port, 4000);
		assert_eq!(settings.scope, "api://trapi/.default");
		for url in [
			"http://example.com",
			"https://user@example.com",
			"https://example.com?x=1",
			"https://example.com#x",
		] {
			let values: BTreeMap<String, String> = BTreeMap::from([
				("HOME".into(), root.path().to_string_lossy().into()),
				("TRAPI_BASE_URL".into(), url.into()),
			]);
			assert!(Settings::from_map(&values, Overrides::default()).is_err());
		}
	}
}
