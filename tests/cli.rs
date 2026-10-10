use std::{
	path::PathBuf,
	process::{Command, Output},
};

fn cli(arguments: &[&str]) -> Output {
	Command::new(env!("CARGO_BIN_EXE_trapi2litellm"))
		.args(arguments)
		.env("TRAPI_BASE_URL", "not-a-url")
		.env("TRAPI2LITELLM_PORT", "invalid")
		.env("TRAPI2LITELLM_PYTHON", "/missing/python")
		.output()
		.unwrap()
}

#[test]
fn help_and_version_ignore_invalid_settings_and_python() {
	for arguments in [
		vec!["--help"],
		vec!["--version"],
		vec!["deploy", "--help"],
		vec!["serve", "--help"],
		vec!["sync", "--help"],
		vec!["smoke-test", "--help"],
	] {
		let result: Output = cli(&arguments);
		assert!(
			result.status.success(),
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
		assert!(String::from_utf8_lossy(&result.stdout).contains("trapi2litellm"));
	}
}

#[test]
fn dry_run_has_no_writes_or_python_invocation() {
	let root: tempfile::TempDir = tempfile::tempdir().unwrap();
	let config: PathBuf = root.path().join("config");
	let state: PathBuf = root.path().join("state");
	let result: Output = Command::new(env!("CARGO_BIN_EXE_trapi2litellm"))
		.args([
			"deploy",
			"--dry-run",
			"--start",
			"--port",
			"4567",
			"--config-dir",
		])
		.arg(&config)
		.arg("--state-dir")
		.arg(&state)
		.env("TRAPI_BASE_URL", "https://example.invalid/pool")
		.env("TRAPI2LITELLM_PYTHON", "/missing/python")
		.output()
		.unwrap();
	assert!(
		result.status.success(),
		"{}",
		String::from_utf8_lossy(&result.stderr)
	);
	let preview: String = String::from_utf8_lossy(&result.stdout).into_owned();
	assert!(if cfg!(target_os = "macos") {
		preview.contains(
			"<string>serve</string>\n\t\t<string>--port</string>\n\t\t<string>4567</string>",
		)
	} else {
		preview.contains("serve --port 4567")
	});
	assert!(!config.exists());
	assert!(!state.exists());
}

#[test]
fn linger_requires_start_before_environment_validation() {
	let result: Output = cli(&["deploy", "--enable-linger"]);
	assert!(!result.status.success());
	assert!(String::from_utf8_lossy(&result.stderr).contains("--start"));
}

#[test]
fn gateway_dry_run_notes_a_missing_upstream_key_without_writing() {
	let root: tempfile::TempDir = tempfile::tempdir().unwrap();
	let config: PathBuf = root.path().join("config");
	let state: PathBuf = root.path().join("state");
	let result: Output = Command::new(env!("CARGO_BIN_EXE_trapi2litellm"))
		.args(["deploy", "--dry-run", "--start", "--config-dir"])
		.arg(&config)
		.arg("--state-dir")
		.arg(&state)
		.env("HOME", root.path())
		.env("TRAPI2LITELLM_MODE", "gateway")
		.env("TRAPI2LITELLM_UPSTREAM_URL", "http://127.0.0.1:14000")
		.env_remove("AZURE_CLIENT_ID")
		.env("TRAPI2LITELLM_PYTHON", "/missing/python")
		.output()
		.unwrap();
	let stderr: String = String::from_utf8_lossy(&result.stderr).into_owned();
	assert!(result.status.success(), "{stderr}");
	assert!(stderr.contains(
		"Note: deployment would refuse to start: Gateway mode requires the upstream key file"
	));
	assert!(!stderr.contains("would refuse this mode"));
	assert!(!config.exists());
	assert!(!state.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_refuses_managed_identity_before_writing() {
	let root: tempfile::TempDir = tempfile::tempdir().unwrap();
	let config: PathBuf = root.path().join("config");
	let state: PathBuf = root.path().join("state");
	let deploy = |start: bool| -> Vec<std::ffi::OsString> {
		let mut arguments: Vec<std::ffi::OsString> = vec![
			"deploy".into(),
			"--config-dir".into(),
			config.clone().into(),
			"--state-dir".into(),
			state.clone().into(),
		];
		if start {
			arguments.push("--start".into());
		}
		arguments
	};
	// Synchronization and serving take their directories from the environment.
	for arguments in [
		deploy(true),
		deploy(false),
		vec!["sync".into()],
		vec!["serve".into()],
	] {
		let result: Output = Command::new(env!("CARGO_BIN_EXE_trapi2litellm"))
			.args(&arguments)
			.env("HOME", root.path())
			.env("TRAPI2LITELLM_CONFIG_DIR", &config)
			.env("TRAPI2LITELLM_STATE_DIR", &state)
			.env_remove("TRAPI2LITELLM_MODE")
			.env_remove("TRAPI2LITELLM_UPSTREAM_URL")
			.env_remove("AZURE_CLIENT_ID")
			.env("TRAPI2LITELLM_PYTHON", "/missing/python")
			.output()
			.unwrap();
		assert!(!result.status.success(), "{arguments:?}");
		assert!(
			String::from_utf8_lossy(&result.stderr).contains(
				"macOS has no Azure Managed Identity endpoint; set TRAPI2LITELLM_MODE=gateway and TRAPI2LITELLM_UPSTREAM_URL"
			),
			"{arguments:?}"
		);
		assert!(!config.exists(), "{arguments:?}");
		assert!(!state.exists(), "{arguments:?}");
	}
}
