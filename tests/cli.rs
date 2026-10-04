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
	assert!(String::from_utf8_lossy(&result.stdout).contains("serve --port 4567"));
	assert!(!config.exists());
	assert!(!state.exists());
}

#[test]
fn linger_requires_start_before_environment_validation() {
	let result: Output = cli(&["deploy", "--enable-linger"]);
	assert!(!result.status.success());
	assert!(String::from_utf8_lossy(&result.stderr).contains("--start"));
}
