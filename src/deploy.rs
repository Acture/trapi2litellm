use crate::{
	catalog, files, process,
	runtime::Runtime,
	settings::{SERVICE, Settings},
	sync::{self, Service},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
	collections::BTreeMap,
	env, fs,
	os::unix::fs::PermissionsExt,
	path::{Path, PathBuf},
	process::Command,
	time::Duration,
};

pub const MARKER: &str = "# Managed by trapi2litellm\n";
const REMEDY: &str = "Install persistently with uv tool install, Homebrew or the Debian package; use --entry-point to select that command";

pub trait Host {
	fn run(&self, program: &str, arguments: &[String]) -> Result<()>;
	fn preflight(&self, units: &BTreeMap<String, String>) -> Result<()>;
}
pub struct SystemHost;
impl Host for SystemHost {
	fn run(&self, program: &str, arguments: &[String]) -> Result<()> {
		sync::checked_command(
			Command::new(program).args(arguments),
			Duration::from_secs(240),
		)
	}
	fn preflight(&self, units: &BTreeMap<String, String>) -> Result<()> {
		let folder: tempfile::TempDir = tempfile::Builder::new()
			.prefix("trapi2litellm-units-")
			.tempdir()?;
		let mut paths: Vec<PathBuf> = Vec::new();
		for (name, content) in units {
			let path: PathBuf = folder.path().join(name);
			fs::write(&path, content)?;
			paths.push(path);
		}
		let output: std::process::Output = process::run(
			Command::new("systemd-analyze")
				.args(["--user", "verify"])
				.args(paths),
			&[],
			Duration::from_secs(30),
		)?;
		let diagnostics: String = [
			String::from_utf8_lossy(&output.stderr).trim(),
			String::from_utf8_lossy(&output.stdout).trim(),
		]
		.into_iter()
		.filter(|part| !part.is_empty())
		.collect::<Vec<&str>>()
		.join("\n");
		ensure!(
			output.status.success()
				&& !String::from_utf8_lossy(&output.stderr).contains("path is not absolute"),
			"systemd unit validation failed ({}): {diagnostics}",
			output.status
		);
		if !diagnostics.is_empty() {
			eprintln!("INFO systemd unit validation diagnostics: {diagnostics}");
		}
		Ok(())
	}
}

pub fn unit_quote(value: &str) -> Result<String> {
	ensure!(
		!value.chars().any(char::is_control),
		"Control characters are not supported in service settings"
	);
	Ok(format!(
		"\"{}\"",
		value
			.replace('\\', "\\\\")
			.replace('"', "\\\"")
			.replace('%', "%%")
	))
}

fn unit_env_file(path: &Path) -> Result<String> {
	let value: &str = path
		.to_str()
		.context("EnvironmentFile path must be UTF-8")?;
	ensure!(path.is_absolute(), "EnvironmentFile path must be absolute");
	ensure!(
		!value.chars().any(char::is_control),
		"Control characters are not supported in service settings"
	);
	// EnvironmentFile consumes one raw filename, expanding only unit specifiers.
	Ok(value.replace('%', "%%"))
}

pub fn render_units(entry: &Path, settings: &Settings) -> Result<BTreeMap<String, String>> {
	let command: String = unit_quote(&entry.to_string_lossy())?.replace('$', "$$");
	let mut environment: BTreeMap<String, String> = settings.environment();
	environment.extend(BTreeMap::from([
		("LITELLM_LOCAL_MODEL_COST_MAP".into(), "True".into()),
		("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
		(
			"NO_PROXY".into(),
			"127.0.0.1,localhost,169.254.169.254".into(),
		),
		(
			"no_proxy".into(),
			"127.0.0.1,localhost,169.254.169.254".into(),
		),
	]));
	let shared: String = environment
		.iter()
		.map(|(key, value)| {
			unit_quote(&format!("{key}={value}")).map(|quoted| format!("Environment={quoted}\n"))
		})
		.collect::<Result<Vec<String>>>()?
		.concat()
		+ "UnsetEnvironment=TRAPI2LITELLM_PYTHON PYTHONPATH PYTHONHOME\n";
	let key_file: String = unit_env_file(&settings.key_path())?;
	let config: String = unit_quote(&format!(
		"CONFIG_FILE_PATH={}",
		settings.config_path().display()
	))?;
	let port: u16 = settings.port;
	let gateway: String = format!(
		"{MARKER}[Unit]\nDescription=Local TRAPI LiteLLM gateway (Managed Identity)\nStartLimitIntervalSec=300\nStartLimitBurst=5\n\n[Service]\nType=simple\nEnvironmentFile={key_file}\n{shared}Environment={config}\nEnvironment=LITELLM_MODE=PRODUCTION\nEnvironment=LITELLM_LOG=WARNING\nEnvironment=AZURE_TOKEN_CREDENTIALS=ManagedIdentityCredential\nEnvironment=AZURE_CREDENTIAL=DefaultAzureCredential\nExecStart={command} serve --port {port}\nExecReload=/bin/kill -HUP $MAINPID\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=930\nKillMode=mixed\nUMask=0077\nNoNewPrivileges=true\nPrivateTmp=true\n\n[Install]\nWantedBy=default.target\n"
	);
	let sync: String = format!(
		"{MARKER}[Unit]\nDescription=Discover TRAPI models and update the local LiteLLM gateway\n\n[Service]\nType=oneshot\n{shared}ExecStart={command} sync\nTimeoutStartSec=240\nUMask=0077\nNoNewPrivileges=true\nPrivateTmp=true\n"
	);
	let timer: String = format!(
		"{MARKER}[Unit]\nDescription=Refresh TRAPI model catalog every hour\n\n[Timer]\nOnCalendar=hourly\nRandomizedDelaySec=120\nPersistent=true\nUnit=litellm-trapi-sync.service\n\n[Install]\nWantedBy=timers.target\n"
	);
	Ok(BTreeMap::from([
		(SERVICE.into(), gateway),
		("litellm-trapi-sync.service".into(), sync),
		("litellm-trapi-sync.timer".into(), timer),
	]))
}

fn legacy_description(content: &str) -> Option<&str> {
	content
		.lines()
		.find(|line| line.starts_with("Description="))
}

pub fn check_managed(path: &Path, content: &str, legacy: bool) -> Result<()> {
	if path.exists() {
		let old: String = fs::read_to_string(path)?;
		ensure!(
			old.starts_with(MARKER)
				|| (legacy
					&& legacy_description(content)
						.is_some_and(|description| old.lines().any(|line| line == description))),
			"Refusing to overwrite unmanaged file: {}",
			path.display()
		);
	}
	Ok(())
}

pub fn managed_write(path: &Path, content: &str, legacy: bool) -> Result<bool> {
	check_managed(path, content, legacy)?;
	if path.exists() {
		let old: String = fs::read_to_string(path)?;
		if old == content {
			return Ok(false);
		}
		let mut backup: std::ffi::OsString = path.as_os_str().to_owned();
		backup.push(".previous");
		files::atomic_write(Path::new(&backup), old.as_bytes())?;
	}
	files::atomic_write(path, content.as_bytes())?;
	Ok(true)
}

pub fn client_files(settings: &Settings) -> BTreeMap<String, String> {
	let path: String = settings.key_path().to_string_lossy().into_owned();
	let shell_path: String = format!("'{}'", path.replace('\'', "'\"'\"'"));
	let fish_path: String = path.replace('\\', "\\\\").replace('\'', "\\'");
	BTreeMap::from([
		(
			"client.sh".into(),
			format!(
				"{MARKER}. {shell_path}\nexport OPENAI_BASE_URL={}/v1\nexport OPENAI_API_KEY=\"$LITELLM_MASTER_KEY\"\n",
				settings.local_url()
			),
		),
		(
			"client.fish".into(),
			format!(
				"{MARKER}set -gx OPENAI_BASE_URL {}/v1\nset -gx OPENAI_API_KEY (string replace 'LITELLM_MASTER_KEY=' '' < '{fish_path}')\n",
				settings.local_url()
			),
		),
	])
}

fn editable(venv: &Path) -> Result<bool> {
	for lib in fs::read_dir(venv)? {
		let lib: PathBuf = lib?.path();
		if !lib.is_dir()
			|| !lib
				.file_name()
				.is_some_and(|name| name.to_string_lossy().starts_with("lib"))
		{
			continue;
		}
		for python in fs::read_dir(lib)? {
			let python: PathBuf = python?.path();
			if !python.is_dir()
				|| !python
					.file_name()
					.is_some_and(|name| name.to_string_lossy().starts_with("python"))
			{
				continue;
			}
			let packages: PathBuf = python.join("site-packages");
			if !packages.is_dir() {
				continue;
			}
			for package in fs::read_dir(packages)? {
				let package: PathBuf = package?.path();
				let name: String = package
					.file_name()
					.context("Invalid package path")?
					.to_string_lossy()
					.into_owned();
				let record: PathBuf = package.join("direct_url.json");
				if name.starts_with("trapi2litellm-")
					&& name.ends_with(".dist-info")
					&& record.is_file()
				{
					let value: Value = serde_json::from_slice(&fs::read(record)?)?;
					if value["dir_info"]["editable"] == true {
						return Ok(true);
					}
				}
			}
		}
	}
	Ok(false)
}

pub fn persistence_problem(entry: &Path) -> Result<Option<String>> {
	let home: PathBuf = PathBuf::from(env::var_os("HOME").context("HOME is required")?);
	let mut roots: Vec<PathBuf> = vec![
		env::temp_dir(),
		PathBuf::from("/tmp"),
		PathBuf::from("/var/tmp"),
		env::var_os("XDG_CACHE_HOME").map_or_else(|| home.join(".cache"), PathBuf::from),
	];
	if let Some(cache) = env::var_os("UV_CACHE_DIR") {
		roots.push(cache.into());
	}
	let resolved: PathBuf = if entry.exists() {
		entry.canonicalize()?
	} else {
		entry.to_owned()
	};
	for path in [entry, resolved.as_path()] {
		for root in &roots {
			if path.starts_with(crate::settings::absolute(root, &home)?) {
				return Ok(Some(format!(
					"{} is in a temporary or cache directory",
					path.display()
				)));
			}
		}
		for parent in path.ancestors().skip(1) {
			if parent.join("CACHEDIR.TAG").is_file() && !parent.join("pyvenv.cfg").is_file() {
				return Ok(Some(format!("{} is inside a cache", path.display())));
			}
			if parent != home && parent.join("pyproject.toml").is_file() {
				return Ok(Some(format!(
					"{} is inside a source checkout",
					path.display()
				)));
			}
			if parent.join("pyvenv.cfg").is_file() && editable(parent)? {
				return Ok(Some(format!(
					"{} is an editable installation",
					path.display()
				)));
			}
		}
	}
	Ok(None)
}

pub fn entry_point(explicit: Option<PathBuf>) -> Result<PathBuf> {
	let found: PathBuf = if let Some(path) = explicit {
		path
	} else {
		env::var_os("PATH")
			.and_then(|paths| {
				env::split_paths(&paths)
					.map(|root| root.join("trapi2litellm"))
					.find(|path| {
						path.is_file()
							&& fs::metadata(path)
								.is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
					})
			})
			.unwrap_or(env::current_exe()?)
	};
	let home: PathBuf = PathBuf::from(env::var_os("HOME").context("HOME is required")?);
	let expanded: PathBuf = if let Ok(rest) = found.strip_prefix("~/") {
		home.join(rest)
	} else {
		found
	};
	Ok(if expanded.is_absolute() {
		expanded
	} else {
		env::current_dir()?.join(expanded)
	})
}

fn runtime_paths(settings: &Settings, entry: &Path) -> Result<()> {
	let executable: PathBuf = if entry.exists() {
		entry.canonicalize()?
	} else {
		entry.to_owned()
	};
	for directory in [&settings.config_dir, &settings.state_dir] {
		for source in executable.ancestors().skip(1) {
			if source == executable.parent().context("Executable has no parent")?
				|| source.join("pyproject.toml").is_file()
			{
				ensure!(
					!directory.starts_with(source),
					"Configuration and state must live outside the application/source tree"
				);
			}
		}
	}
	Ok(())
}

pub struct DeployOptions {
	pub entry: PathBuf,
	pub start: bool,
	pub enable_linger: bool,
	pub dry_run: bool,
}

pub fn deploy(
	runtime: &impl Runtime,
	service: &impl Service,
	host: &impl Host,
	settings: &Settings,
	options: &DeployOptions,
) -> Result<()> {
	ensure!(
		!options.enable_linger || options.start,
		"--enable-linger requires --start"
	);
	runtime_paths(settings, &options.entry)?;
	let units: BTreeMap<String, String> = render_units(&options.entry, settings)?;
	let problem: Option<String> = persistence_problem(&options.entry)?;
	if options.dry_run {
		for (name, content) in units {
			println!("# {name}\n{content}");
		}
		if let Some(problem) = problem {
			eprintln!("Note: deployment would refuse this entry point: {problem}. {REMEDY}");
		}
		return Ok(());
	}
	if let Some(problem) = problem {
		bail!("Refusing a non-persistent entry point: {problem}. {REMEDY}");
	}
	ensure!(
		cfg!(target_os = "linux"),
		"Deployment currently supports Linux with systemd --user only"
	);
	ensure!(
		options.entry.is_file() && fs::metadata(&options.entry)?.permissions().mode() & 0o111 != 0,
		"Entry point is not executable. {REMEDY}"
	);
	host.run("systemctl", &["--user".into(), "show-environment".into()])?;
	let unit_dir: PathBuf = settings.user_config_home.join("systemd/user");
	let clients: BTreeMap<String, String> = client_files(settings);
	for (name, content) in &units {
		check_managed(&unit_dir.join(name), content, true)?;
	}
	for (name, content) in &clients {
		check_managed(&settings.config_dir.join(name), content, false)?;
	}
	host.preflight(&units)?;
	fs::create_dir_all(&unit_dir)?;
	files::private_directory(&settings.config_dir)?;
	let mut gateway_changed: bool = false;
	for (name, content) in &units {
		let changed: bool = managed_write(&unit_dir.join(name), content, true)?;
		gateway_changed |= changed && name == SERVICE;
	}
	for (name, content) in &clients {
		managed_write(&settings.config_dir.join(name), content, false)?;
	}
	host.run("systemctl", &["--user".into(), "daemon-reload".into()])?;
	if !options.start {
		println!(
			"Installed units only. To bootstrap and start: trapi2litellm deploy --start (with the same settings)"
		);
		return Ok(());
	}
	let result: sync::SyncResult = sync::synchronize(runtime, service, settings, true, true)?;
	if options.enable_linger {
		// getuid has no pointer arguments or failure mode.
		let uid: u32 = unsafe { libc::getuid() };
		host.run("loginctl", &["enable-linger".into(), uid.to_string()])?;
	}
	host.run(
		"systemctl",
		&[
			"--user".into(),
			"enable".into(),
			"--now".into(),
			SERVICE.into(),
			"litellm-trapi-sync.timer".into(),
		],
	)?;
	if gateway_changed {
		host.run(
			"systemctl",
			&["--user".into(), "restart".into(), SERVICE.into()],
		)?;
	} else {
		service.reload()?;
	}
	let config: catalog::Config = serde_json::from_slice(&fs::read(settings.config_path())?)?;
	service.wait_for_models(&catalog::model_names(&config), &result.config_sha256)?;
	println!(
		"{}",
		json!({"status": "ready", "base_url": format!("{}/v1", settings.local_url()), "models": config.model_list.len(), "key_file": settings.key_path()})
	);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn escaping_and_rendering() {
		assert_eq!(unit_quote("/a b/%u").unwrap(), "\"/a b/%%u\"");
		assert!(unit_quote("/tmp/a\nExecStart=oops").is_err());
		assert!(unit_env_file(Path::new("relative/gateway.env")).is_err());
		assert!(unit_env_file(Path::new("/tmp/a\nEnvironmentFile=oops")).is_err());
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let mut settings: Settings = crate::settings::test_settings(root.path());
		settings.config_dir = root.path().join("config space%u\"\\$HOME");
		let units: BTreeMap<String, String> =
			render_units(Path::new("/opt/$gateway/bin/trapi2litellm"), &settings).unwrap();
		assert_eq!(units.len(), 3);
		let gateway: &String = &units[SERVICE];
		assert!(gateway.contains("/opt/$$gateway/bin/trapi2litellm"));
		assert!(gateway.contains("serve --port 4000"));
		assert!(gateway.contains("ExecReload=/bin/kill -HUP $MAINPID"));
		assert!(gateway.contains(&format!(
			"EnvironmentFile={}/config space%%u\"\\$HOME/gateway.env\n",
			root.path().display()
		)));
		assert!(!gateway.contains("LITELLM_MASTER_KEY="));
		assert!(!gateway.contains("WorkingDirectory"));
		for content in client_files(&settings).values() {
			assert!(content.contains("gateway.env"));
			assert!(content.contains("127.0.0.1:4000/v1"));
			assert!(!content.contains("sk-trapi-"));
		}
	}
	#[test]
	fn ownership_noop_and_backup() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let path: PathBuf = root.path().join("example.service");
		fs::write(&path, "foreign\n").unwrap();
		assert!(managed_write(&path, MARKER, false).is_err());
		assert_eq!(fs::read_to_string(&path).unwrap(), "foreign\n");
		fs::write(&path, format!("{MARKER}first\n")).unwrap();
		assert!(!managed_write(&path, &format!("{MARKER}first\n"), false).unwrap());
		assert!(managed_write(&path, &format!("{MARKER}second\n"), false).unwrap());
		assert_eq!(
			fs::read_to_string(root.path().join("example.service.previous")).unwrap(),
			format!("{MARKER}first\n")
		);
	}
}
