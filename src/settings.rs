use anyhow::{Context, Result, bail, ensure};
use std::{
	collections::BTreeMap,
	env,
	path::{Component, Path, PathBuf},
};
use url::{Host, Url};

pub const SERVICE: &str = "litellm-trapi.service";
/// Credential selection the Azure SDK reads in managed-identity mode.
const MANAGED_IDENTITY_ENVIRONMENT: [(&str, &str); 2] = [
	("AZURE_TOKEN_CREDENTIALS", "ManagedIdentityCredential"),
	("AZURE_CREDENTIAL", "DefaultAzureCredential"),
];

/// How the gateway reaches TRAPI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
	/// Directly, with an Azure Managed Identity token.
	ManagedIdentity,
	/// Through another trapi2litellm gateway, authenticated by its master key.
	Gateway { upstream_url: String },
}

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
	pub mode: Mode,
}

#[derive(Default)]
pub struct Overrides {
	pub config_dir: Option<PathBuf>,
	pub state_dir: Option<PathBuf>,
	pub port: Option<u16>,
}

/// Validates an upstream gateway URL: plain HTTP to a loopback host, such as an SSH forward,
/// without credentials, query or fragment. The relay has no TLS client, so a remote upstream must
/// be forwarded. Returns the URL without a trailing slash.
fn upstream_url(value: &str) -> Result<String> {
	let parsed: Url = Url::parse(value).context("Invalid TRAPI2LITELLM_UPSTREAM_URL")?;
	let loopback: bool = match parsed.host() {
		Some(Host::Domain(name)) => name == "localhost",
		Some(Host::Ipv4(address)) => address.is_loopback(),
		Some(Host::Ipv6(address)) => address.is_loopback(),
		None => false,
	};
	ensure!(
		parsed.scheme() == "http"
			&& loopback
			&& parsed.username().is_empty()
			&& parsed.password().is_none()
			&& parsed.query().is_none()
			&& parsed.fragment().is_none(),
		"TRAPI2LITELLM_UPSTREAM_URL must be a loopback http URL (127.0.0.0/8, ::1 or localhost) without credentials, query or fragment, such as the SSH forward http://127.0.0.1:14000"
	);
	Ok(parsed.as_str().trim_end_matches('/').to_owned())
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
		let client_id: Option<String> = values
			.get("AZURE_CLIENT_ID")
			.filter(|value| !value.is_empty())
			.cloned();
		let upstream: Option<&String> = values
			.get("TRAPI2LITELLM_UPSTREAM_URL")
			.filter(|value| !value.is_empty());
		let mode: Mode = match get("TRAPI2LITELLM_MODE", "managed_identity").as_str() {
			"managed_identity" => {
				ensure!(
					upstream.is_none(),
					"TRAPI2LITELLM_UPSTREAM_URL requires TRAPI2LITELLM_MODE=gateway"
				);
				Mode::ManagedIdentity
			}
			"gateway" => {
				ensure!(
					client_id.is_none(),
					"AZURE_CLIENT_ID selects a managed identity and is not used with TRAPI2LITELLM_MODE=gateway; unset it"
				);
				Mode::Gateway {
					upstream_url: upstream_url(upstream.context(
						"TRAPI2LITELLM_MODE=gateway requires TRAPI2LITELLM_UPSTREAM_URL",
					)?)?,
				}
			}
			other => {
				bail!("Unknown TRAPI2LITELLM_MODE '{other}'; expected managed_identity or gateway")
			}
		};
		Ok(Self {
			config_dir,
			state_dir,
			user_config_home,
			base_url,
			port,
			api_version: get("TRAPI_API_VERSION", "2025-04-01-preview"),
			catalog_version: get("TRAPI_CATALOG_VERSION", "preview"),
			scope: get("TRAPI_SCOPE", "api://trapi/.default"),
			client_id,
			mode,
		})
	}
	pub fn config_path(&self) -> PathBuf {
		self.config_dir.join("config.yaml")
	}
	pub fn key_path(&self) -> PathBuf {
		self.config_dir.join("gateway.env")
	}
	/// Master key of the upstream gateway; only gateway mode reads it.
	pub fn upstream_key_path(&self) -> PathBuf {
		self.config_dir.join("upstream.env")
	}
	pub fn local_url(&self) -> String {
		format!("http://127.0.0.1:{}", self.port)
	}
	/// Where synchronization fetches the catalog: TRAPI itself, or the upstream gateway's copy.
	pub fn catalog_url(&self) -> String {
		match &self.mode {
			Mode::ManagedIdentity => {
				let mut url: Url =
					Url::parse(&format!("{}/openai/models", self.base_url)).expect("validated URL");
				url.query_pairs_mut()
					.append_pair("api-version", &self.catalog_version);
				url.into()
			}
			Mode::Gateway { upstream_url } => format!("{upstream_url}/catalog"),
		}
	}
	/// Azure SDK credential defaults for the gateway process; gateway mode has none.
	pub fn credential_environment(&self) -> &'static [(&'static str, &'static str)] {
		match self.mode {
			Mode::ManagedIdentity => &MANAGED_IDENTITY_ENVIRONMENT,
			Mode::Gateway { .. } => &[],
		}
	}
	/// Host of the upstream gateway as proxy exclusion lists name it: an IPv6 address without
	/// brackets.
	pub fn upstream_host(&self) -> Option<String> {
		let Mode::Gateway { upstream_url } = &self.mode else {
			return None;
		};
		Some(
			match Url::parse(upstream_url)
				.expect("validated URL")
				.host()
				.expect("validated URL")
			{
				Host::Ipv6(address) => address.to_string(),
				host => host.to_string(),
			},
		)
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
		]);
		match &self.mode {
			// The default mode stays implicit, so managed-identity units keep their text.
			Mode::ManagedIdentity => values.extend([
				("TRAPI_BASE_URL".into(), self.base_url.clone()),
				("TRAPI_API_VERSION".into(), self.api_version.clone()),
				("TRAPI_CATALOG_VERSION".into(), self.catalog_version.clone()),
				("TRAPI_SCOPE".into(), self.scope.clone()),
			]),
			// A relay never calls TRAPI itself, so the TRAPI settings do not apply.
			Mode::Gateway { upstream_url } => values.extend([
				("TRAPI2LITELLM_MODE".into(), "gateway".into()),
				("TRAPI2LITELLM_UPSTREAM_URL".into(), upstream_url.clone()),
			]),
		}
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

/// Settings relaying through an upstream gateway on the conventional SSH forward.
#[cfg(test)]
pub fn test_gateway_settings(root: &Path) -> Settings {
	Settings {
		mode: Mode::Gateway {
			upstream_url: "http://127.0.0.1:14000".into(),
		},
		..test_settings(root)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(root: &Path, values: &[(&str, &str)]) -> Result<Settings> {
		let mut map: BTreeMap<String, String> =
			BTreeMap::from([("HOME".into(), root.to_string_lossy().into())]);
		map.extend(
			values
				.iter()
				.map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
		);
		Settings::from_map(&map, Overrides::default())
	}

	fn error(root: &Path, values: &[(&str, &str)]) -> String {
		format!("{:#}", parse(root, values).unwrap_err())
	}

	#[test]
	fn mode_defaults_to_managed_identity_and_stays_implicit() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		for values in [
			&[][..],
			&[("TRAPI2LITELLM_MODE", "managed_identity")],
			&[("TRAPI2LITELLM_UPSTREAM_URL", "")],
		] {
			let settings: Settings = parse(root.path(), values).unwrap();
			assert_eq!(settings.mode, Mode::ManagedIdentity);
			let environment: BTreeMap<String, String> = settings.environment();
			assert!(!environment.contains_key("TRAPI2LITELLM_MODE"));
			assert!(!environment.contains_key("TRAPI2LITELLM_UPSTREAM_URL"));
			assert_eq!(settings.credential_environment().len(), 2);
		}
	}

	#[test]
	fn gateway_mode_normalizes_and_persists_the_upstream_url() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		for (url, expected) in [
			("http://127.0.0.1:14000", "http://127.0.0.1:14000"),
			("http://127.0.0.1:14000/", "http://127.0.0.1:14000"),
			(
				"http://127.8.9.10:14000/relay//",
				"http://127.8.9.10:14000/relay",
			),
			("http://[::1]:14000/", "http://[::1]:14000"),
			("http://LOCALHOST:14000", "http://localhost:14000"),
		] {
			let settings: Settings = parse(
				root.path(),
				&[
					("TRAPI2LITELLM_MODE", "gateway"),
					("TRAPI2LITELLM_UPSTREAM_URL", url),
					("AZURE_CLIENT_ID", ""),
				],
			)
			.unwrap();
			assert_eq!(
				settings.mode,
				Mode::Gateway {
					upstream_url: expected.into()
				},
				"{url}"
			);
			assert_eq!(settings.catalog_url(), format!("{expected}/catalog"));
			assert!(settings.credential_environment().is_empty());
			let environment: BTreeMap<String, String> = settings.environment();
			assert_eq!(environment["TRAPI2LITELLM_MODE"], "gateway");
			assert_eq!(environment["TRAPI2LITELLM_UPSTREAM_URL"], expected);
			assert!(!environment.contains_key("AZURE_CLIENT_ID"));
			assert!(!environment.contains_key("TRAPI2LITELLM_UPSTREAM_KEY"));
			assert!(!environment.keys().any(|key| key.starts_with("TRAPI_")));
		}
		for (url, host) in [
			("http://127.0.0.1:14000", "127.0.0.1"),
			("http://127.8.9.10:14000", "127.8.9.10"),
			("http://[::1]:14000", "::1"),
			("http://LOCALHOST:14000", "localhost"),
		] {
			let settings: Settings = parse(
				root.path(),
				&[
					("TRAPI2LITELLM_MODE", "gateway"),
					("TRAPI2LITELLM_UPSTREAM_URL", url),
				],
			)
			.unwrap();
			assert_eq!(settings.upstream_host().as_deref(), Some(host), "{url}");
		}
		assert_eq!(test_settings(root.path()).upstream_host(), None);
		let settings: Settings = test_gateway_settings(root.path());
		assert_eq!(
			settings.upstream_key_path(),
			settings.config_dir.join("upstream.env")
		);
	}

	#[test]
	fn invalid_modes_and_upstream_urls_fail() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		for mode in ["relay", "", "Gateway", "managed-identity"] {
			assert!(
				error(root.path(), &[("TRAPI2LITELLM_MODE", mode)])
					.contains("expected managed_identity or gateway"),
				"{mode}"
			);
		}
		assert!(
			error(root.path(), &[("TRAPI2LITELLM_MODE", "gateway")])
				.contains("requires TRAPI2LITELLM_UPSTREAM_URL")
		);
		assert!(
			error(
				root.path(),
				&[
					("TRAPI2LITELLM_MODE", "gateway"),
					("TRAPI2LITELLM_UPSTREAM_URL", "")
				]
			)
			.contains("requires TRAPI2LITELLM_UPSTREAM_URL")
		);
		assert!(
			error(
				root.path(),
				&[("TRAPI2LITELLM_UPSTREAM_URL", "http://127.0.0.1:14000")]
			)
			.contains("requires TRAPI2LITELLM_MODE=gateway")
		);
		assert!(
			error(
				root.path(),
				&[
					("TRAPI2LITELLM_MODE", "gateway"),
					("TRAPI2LITELLM_UPSTREAM_URL", "http://127.0.0.1:14000"),
					("AZURE_CLIENT_ID", "identity-selector"),
				]
			)
			.contains("AZURE_CLIENT_ID")
		);
		for url in [
			"127.0.0.1:14000",
			"/relative",
			"not a url",
			"ftp://127.0.0.1/",
			"file:///tmp/catalog",
			"http://example.com",
			"http://10.0.0.1:14000",
			"http://localhost.example.com",
			"http://[::2]:14000",
			"http://user:secret@127.0.0.1:14000",
			"http://user@127.0.0.1:14000",
			"http://127.0.0.1:14000/?key=x",
			"http://127.0.0.1:14000/#x",
			// The relay has no TLS client; a remote upstream is reached through an SSH forward.
			"https://relay.example.com",
			"https://127.0.0.1:14000",
		] {
			let message: String = error(
				root.path(),
				&[
					("TRAPI2LITELLM_MODE", "gateway"),
					("TRAPI2LITELLM_UPSTREAM_URL", url),
				],
			);
			assert!(
				message.contains("TRAPI2LITELLM_UPSTREAM_URL"),
				"{url}: {message}"
			);
			assert!(!message.contains("secret"), "{url}: {message}");
		}
	}
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
