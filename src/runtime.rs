use crate::{catalog::Config, files, process, settings::Settings};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
	env, fmt,
	os::unix::process::CommandExt,
	path::{Path, PathBuf},
	process::Command,
	time::Duration,
};

pub trait Runtime {
	fn catalog(&self, settings: &Settings) -> Result<Value>;
	fn validate(&self, config: &Config, previous_text: Option<&str>) -> Result<Vec<String>>;
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RuntimeError {
	pub error_type: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub http_status: Option<u16>,
}
impl fmt::Display for RuntimeError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "Python runtime failed ({})", self.error_type)
	}
}
impl std::error::Error for RuntimeError {}

pub struct PythonRuntime {
	pub python: PathBuf,
	isolated: bool,
}

impl PythonRuntime {
	pub fn discover() -> Result<Self> {
		let explicit: Option<std::ffi::OsString> =
			env::var_os("TRAPI2LITELLM_PYTHON").filter(|value| !value.is_empty());
		let isolated: bool = explicit.is_none();
		let python: PathBuf = if let Some(explicit) = explicit {
			let path: PathBuf = explicit.into();
			ensure!(
				path.is_absolute(),
				"TRAPI2LITELLM_PYTHON must be an absolute interpreter path"
			);
			path
		} else {
			Self::sibling_python(&env::current_exe()?)?
		};
		ensure!(
			python.is_file(),
			"No Python interpreter beside the installed executable; source runs require TRAPI2LITELLM_PYTHON"
		);
		Ok(Self { python, isolated })
	}

	fn sibling_python(executable: &Path) -> Result<PathBuf> {
		let binary: PathBuf = executable.canonicalize()?;
		let sibling: &Path = binary.parent().context("Native executable has no parent")?;
		let python: PathBuf = sibling.join("python");
		Ok(if python.is_file() {
			python
		} else {
			sibling.join("python3")
		})
	}

	fn command(&self, operation: &str) -> Command {
		let mut command: Command = Command::new(&self.python);
		if self.isolated {
			command.arg("-I");
		}
		command.args(["-m", "trapi2litellm.runtime", operation]);
		command
	}

	fn request<T: Serialize>(&self, operation: &str, request: &T) -> Result<Value> {
		let bytes: Vec<u8> = serde_json::to_vec(request)?;
		let timeout: Duration = match operation {
			"catalog" => Duration::from_secs(60),
			"validate" => Duration::from_secs(30),
			_ => bail!("Unsupported Python runtime request"),
		};
		let output: std::process::Output =
			process::run(&mut self.command(operation), &bytes, timeout)?;
		if !output.status.success() {
			let error: RuntimeError =
				serde_json::from_slice(&output.stdout).unwrap_or(RuntimeError {
					error_type: "RuntimeProtocolError".into(),
					http_status: None,
				});
			return Err(error.into());
		}
		serde_json::from_slice(&output.stdout).context("Invalid JSON from Python runtime")
	}

	pub fn exec(&self, operation: &str, settings: &Settings) -> Result<()> {
		let mut command: Command = self.command(operation);
		let config_path: PathBuf = match env::var_os("CONFIG_FILE_PATH") {
			Some(value) => {
				let path: PathBuf = value.into();
				// Python changes cwd before Gunicorn imports the gateway.
				// Keep caller-relative paths and absolute overrides' spelling intact.
				if path.is_absolute() {
					path
				} else {
					env::current_dir()?.join(path)
				}
			}
			None => settings.config_path(),
		};
		command
			.envs(settings.environment())
			.env("CONFIG_FILE_PATH", config_path);
		for (key, default) in [
			("LITELLM_LOCAL_MODEL_COST_MAP", "True"),
			("LITELLM_MODE", "PRODUCTION"),
			("LITELLM_LOG", "WARNING"),
			("AZURE_TOKEN_CREDENTIALS", "ManagedIdentityCredential"),
			("AZURE_CREDENTIAL", "DefaultAzureCredential"),
		] {
			command.env(key, env::var(key).unwrap_or_else(|_| default.into()));
		}
		let key: String = env::var("LITELLM_MASTER_KEY")
			.ok()
			.filter(|value| !value.is_empty())
			.map_or_else(|| files::local_key(&settings.key_path()), Ok)?;
		command.env("LITELLM_MASTER_KEY", key);
		let error: std::io::Error = command.exec();
		bail!("Could not exec Python runtime: {error}")
	}
}

#[derive(Serialize)]
struct CatalogRequest<'a> {
	base_url: &'a str,
	catalog_version: &'a str,
	scope: &'a str,
	client_id: &'a Option<String>,
}
#[derive(Serialize)]
struct ValidationRequest<'a> {
	config: &'a Config,
	previous_text: Option<&'a str>,
}
#[derive(Deserialize)]
struct ValidationResponse {
	previous_models: Vec<String>,
}

impl Runtime for PythonRuntime {
	fn catalog(&self, settings: &Settings) -> Result<Value> {
		self.request(
			"catalog",
			&CatalogRequest {
				base_url: &settings.base_url,
				catalog_version: &settings.catalog_version,
				scope: &settings.scope,
				client_id: &settings.client_id,
			},
		)
	}
	fn validate(&self, config: &Config, previous_text: Option<&str>) -> Result<Vec<String>> {
		let value: Value = self.request(
			"validate",
			&ValidationRequest {
				config,
				previous_text,
			},
		)?;
		Ok(serde_json::from_value::<ValidationResponse>(value)
			.context("Invalid validation response")?
			.previous_models)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{
		fs,
		os::unix::fs::{PermissionsExt, symlink},
	};

	fn mock_python(root: &Path, response: &str, status: u8) -> PythonRuntime {
		let path: PathBuf = root.join("python");
		let script: String = format!(
			"#!/bin/sh\ncat > \"${{0%/*}}/input.json\"\nprintf '%s' '{response}'\nprintf '%s' 'token-secret-sentinel' >&2\nexit {status}\n"
		);
		fs::write(&path, script).unwrap();
		fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
		PythonRuntime {
			python: path,
			isolated: false,
		}
	}

	#[test]
	fn json_protocol_and_sanitized_errors() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let runtime: PythonRuntime = mock_python(root.path(), "{\"data\":[]}", 0);
		assert_eq!(
			runtime.catalog(&settings).unwrap(),
			serde_json::json!({"data": []})
		);
		let request: Value =
			serde_json::from_slice(&fs::read(root.path().join("input.json")).unwrap()).unwrap();
		assert_eq!(request["base_url"], settings.base_url);
		assert_eq!(request["catalog_version"], settings.catalog_version);
		let runtime: PythonRuntime =
			mock_python(root.path(), "{\"previous_models\":[\"trapi/old\"]}", 0);
		let config: Config =
			crate::catalog::build_config(&crate::catalog::catalog(&["a"]), &settings).unwrap();
		assert_eq!(
			runtime.validate(&config, Some("old YAML")).unwrap(),
			vec!["trapi/old"]
		);
		let request: Value =
			serde_json::from_slice(&fs::read(root.path().join("input.json")).unwrap()).unwrap();
		assert_eq!(request["previous_text"], "old YAML");
		let runtime: PythonRuntime = mock_python(
			root.path(),
			"{\"error_type\":\"HttpResponseError\",\"http_status\":403}",
			1,
		);
		let error: anyhow::Error = runtime.catalog(&settings).unwrap_err();
		assert_eq!(
			error.downcast_ref::<RuntimeError>().unwrap().http_status,
			Some(403)
		);
		assert!(!error.to_string().contains("token-secret-sentinel"));
	}

	#[test]
	fn resolves_entry_symlink_but_keeps_venv_python_symlink() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let bin: PathBuf = root.path().join("venv/bin");
		fs::create_dir_all(&bin).unwrap();
		fs::write(bin.join("trapi2litellm"), "binary").unwrap();
		fs::write(root.path().join("base-python"), "python").unwrap();
		symlink(root.path().join("base-python"), bin.join("python")).unwrap();
		symlink(bin.join("trapi2litellm"), root.path().join("stable-entry")).unwrap();
		assert_eq!(
			PythonRuntime::sibling_python(&root.path().join("stable-entry")).unwrap(),
			bin.canonicalize().unwrap().join("python")
		);
	}
}
