pub mod launchd;
pub mod systemd;
mod transaction;

use crate::{
	files,
	runtime::Runtime,
	settings::{Mode, Settings},
	sync::Service,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
	collections::{BTreeMap, BTreeSet},
	env, fs,
	os::unix::fs::PermissionsExt,
	path::{Path, PathBuf},
};

pub const MARKER: &str = "# Managed by trapi2litellm\n";
const REMEDY: &str = "Install persistently with uv tool install, Homebrew or the Debian package; use --entry-point to select that command";
/// Gateway-only environment that every manager sets after `CONFIG_FILE_PATH`, in this order.
fn gateway_environment(settings: &Settings) -> Vec<(&'static str, &'static str)> {
	[("LITELLM_MODE", "PRODUCTION"), ("LITELLM_LOG", "WARNING")]
		.iter()
		.chain(settings.credential_environment())
		.copied()
		.collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Job {
	Gateway,
	Sync,
}

impl Job {
	pub const ALL: [Self; 2] = [Self::Gateway, Self::Sync];
}

/// Whether a job starts with the user's session.
///
/// `Runtime` is systemd's `enable --runtime`. A manager that cannot express an enablement reports
/// only `Disabled` or `Persistent` and fails closed on anything else, as systemd does for an
/// unsupported `UnitFileState`: launchd reports `Persistent` when the LaunchAgents plist is
/// present, and fails closed on a `launchctl disable` override, which deploy never changes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Enablement {
	#[default]
	Disabled,
	Persistent,
	Runtime,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UnitState {
	/// The job is armed: the gateway is running, or the sync schedule is set.
	active: bool,
	enablement: Enablement,
}

/// Keeps user services running without a login session.
pub trait Linger {
	fn enabled(&self) -> Result<bool>;
	fn set(&self, enabled: bool) -> Result<()>;
}

/// Semantic operations the deployment transaction needs from a user service manager.
///
/// A manager may load a job from a copy of its installed definition (launchd runs plists from
/// LaunchAgents). `activate_all` publishes that copy, and the deployment snapshot captures,
/// ownership-checks and restores it with the installed definitions.
pub trait ServiceManager {
	/// Directory deploy installs rendered definitions into, together with their `.previous`
	/// backups and the atomic-write temporary files of every definition, including the loaded
	/// copies published elsewhere.
	fn definitions_dir(&self) -> &Path;
	/// What `definitions_dir` holds, as user-facing messages name it.
	fn definitions_kind(&self) -> &str;
	/// File name of the installed definition that controls a job.
	fn definition(&self, job: Job) -> &str;
	/// File the manager loads a job from: the installed definition itself, or the copy that
	/// `activate_all` publishes outside `definitions_dir`.
	fn loaded_definition(&self, job: Job) -> PathBuf;
	/// Definition files keyed by file name.
	fn render(&self, entry: &Path) -> Result<BTreeMap<String, String>>;
	/// Whether an installed or loaded definition may be replaced with new content.
	fn is_managed(&self, old: &str, new: &str) -> bool;
	/// Fails when the manager cannot install definitions or, with `start`, activate jobs.
	fn check_available(&self, start: bool) -> Result<()>;
	fn preflight(&self, definitions: &BTreeMap<String, String>) -> Result<()>;
	/// Makes the manager pick up definitions installed into `definitions_dir`.
	fn definitions_changed(&self) -> Result<()>;
	/// Jobs whose loaded definition, not the installed copy, differs from the rendered
	/// `definitions` in a way that only a restart applies. Called before anything is installed.
	fn outdated(&self, definitions: &BTreeMap<String, String>) -> Result<BTreeSet<Job>>;
	/// Activation and enablement of a job. `active` means armed: the gateway is running, or the
	/// sync schedule is set (an active systemd timer; a loaded launchd calendar job, whose idle
	/// state is loaded but not running). A launchd gateway that is loaded but not running is not
	/// armed: KeepAlive restarts it only after a failure. States the transaction cannot restore
	/// fail closed, such as a systemd unit that is `activating` or a launchd `spawn scheduled`.
	fn state(&self, job: Job) -> Result<UnitState>;
	/// Publishes every installed definition where the manager loads it, enables every job and
	/// arms those that are not armed from their new definitions. Never restarts an armed job: the
	/// transaction restarts the `outdated` ones and reloads an up-to-date gateway.
	fn activate_all(&self) -> Result<()>;
	/// Arms a job again from its loaded definition, stopping it first if it is armed and loading
	/// the definition if the manager has unloaded it.
	fn restart(&self, job: Job) -> Result<()>;
	/// Arms a job from its loaded definition, loading the definition if needed.
	fn start(&self, job: Job) -> Result<()>;
	/// Disarms every job so that nothing respawns it; launchd must unload, because KeepAlive
	/// restarts a killed gateway.
	fn stop_all(&self) -> Result<()>;
	/// Removes the enablement of every job.
	fn disable_all(&self) -> Result<()>;
	/// Re-applies a snapshotted enablement after the definition files are restored.
	fn restore_enablement(&self, job: Job, enablement: Enablement) -> Result<()>;
	/// Whether the old gateway definition uses the same configuration, state and port.
	fn same_target(&self, old_gateway: &str) -> Result<bool>;
	/// Keeps jobs running without a login session, or explains why the manager cannot.
	fn linger(&self) -> Result<&dyn Linger>;
}

/// Environment both jobs get on top of the settings, whatever the manager.
fn job_environment(settings: &Settings) -> BTreeMap<String, String> {
	let mut no_proxy: String = "127.0.0.1,localhost,169.254.169.254".into();
	// The upstream key travels in plain HTTP to a loopback forward; no proxy may carry it.
	if let Some(host) = settings.upstream_host()
		&& !no_proxy.split(',').any(|entry| entry == host)
	{
		no_proxy = format!("{no_proxy},{host}");
	}
	let mut environment: BTreeMap<String, String> = settings.environment();
	environment.extend(BTreeMap::from([
		("LITELLM_LOCAL_MODEL_COST_MAP".into(), "True".into()),
		("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
		("NO_PROXY".into(), no_proxy.clone()),
		("no_proxy".into(), no_proxy),
	]));
	environment
}

fn marked(old: &str) -> bool {
	old.starts_with(MARKER)
}

fn check_managed(path: &Path, managed: impl Fn(&str) -> bool) -> Result<()> {
	if path.exists() {
		let old: String = fs::read_to_string(path)?;
		ensure!(
			managed(&old),
			"Refusing to overwrite unmanaged file: {}",
			path.display()
		);
	}
	Ok(())
}

fn managed_write(path: &Path, content: &str, managed: impl Fn(&str) -> bool) -> Result<bool> {
	check_managed(path, managed)?;
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
	let shell_path: String = files::shell_quote(&path);
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

/// Refuses to replace an installed or loaded definition, or a client file, that deploy does not
/// manage.
fn check_ownership(
	manager: &impl ServiceManager,
	settings: &Settings,
	definitions: &BTreeMap<String, String>,
	clients: &BTreeMap<String, String>,
) -> Result<()> {
	for (name, content) in definitions {
		check_managed(&manager.definitions_dir().join(name), |old| {
			manager.is_managed(old, content)
		})?;
	}
	for job in Job::ALL {
		let content: &str = &definitions[manager.definition(job)];
		check_managed(&manager.loaded_definition(job), |old| {
			manager.is_managed(old, content)
		})?;
	}
	for name in clients.keys() {
		check_managed(&settings.config_dir.join(name), marked)?;
	}
	Ok(())
}

/// Fails before anything is written when a started gateway could not authenticate upstream.
fn check_upstream_key(settings: &Settings, start: bool) -> Result<()> {
	if start && let Mode::Gateway { .. } = settings.mode {
		files::upstream_key(&settings.upstream_key_path())?;
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
	manager: &impl ServiceManager,
	settings: &Settings,
	options: &DeployOptions,
) -> Result<()> {
	ensure!(
		!options.enable_linger || options.start,
		"--enable-linger requires --start"
	);
	if options.enable_linger {
		manager.linger()?;
	}
	runtime_paths(settings, &options.entry)?;
	let definitions: BTreeMap<String, String> = manager.render(&options.entry)?;
	let problem: Option<String> = persistence_problem(&options.entry)?;
	if options.dry_run {
		for (name, content) in definitions {
			println!("# {name}\n{content}");
		}
		if let Some(problem) = problem {
			eprintln!("Note: deployment would refuse this entry point: {problem}. {REMEDY}");
		}
		if let Err(error) = check_upstream_key(settings, options.start) {
			eprintln!("Note: deployment would refuse to start: {error:#}");
		}
		return Ok(());
	}
	if let Some(problem) = problem {
		bail!("Refusing a non-persistent entry point: {problem}. {REMEDY}");
	}
	ensure!(
		options.entry.is_file() && fs::metadata(&options.entry)?.permissions().mode() & 0o111 != 0,
		"Entry point is not executable. {REMEDY}"
	);
	check_upstream_key(settings, options.start)?;
	manager.check_available(options.start)?;
	let clients: BTreeMap<String, String> = client_files(settings);
	check_ownership(manager, settings, &definitions, &clients)?;
	manager.preflight(&definitions)?;
	transaction::apply_deployment(
		runtime,
		service,
		manager,
		settings,
		options,
		&definitions,
		&clients,
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn ownership_noop_and_backup() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let path: PathBuf = root.path().join("example.service");
		fs::write(&path, "foreign\n").unwrap();
		assert!(managed_write(&path, MARKER, marked).is_err());
		assert_eq!(fs::read_to_string(&path).unwrap(), "foreign\n");
		fs::write(&path, format!("{MARKER}first\n")).unwrap();
		assert!(!managed_write(&path, &format!("{MARKER}first\n"), marked).unwrap());
		assert!(managed_write(&path, &format!("{MARKER}second\n"), marked).unwrap());
		assert_eq!(
			fs::read_to_string(root.path().join("example.service.previous")).unwrap(),
			format!("{MARKER}first\n")
		);
	}

	/// Catalog the managed-identity snapshots were rendered from.
	const SNAPSHOT_CATALOG: &str = r#"{"data": [
	{"id": "gpt-5.2_2025-12-11", "provisioningState": "Succeeded", "capabilities": {"chatCompletion": "true", "maxContextToken": "1234"}, "model": {"Format": "OpenAI", "Name": "gpt-5.2"}, "RateLimits": {"RequestsPerMinute": {"count": 60}, "TokensPerMinute": 100000}},
	{"id": "Qwen/Qwen3.5-9B", "provisioningState": "Succeeded", "capabilities": {"embeddings": true}},
	{"id": "failed", "provisioningState": "Failed"}
]}"#;
	const SNAPSHOT_HOME: &str = "/trapi2litellm-fixture/home";
	const SNAPSHOT_ENTRY: &str = "/opt/trapi2litellm/bin/trapi2litellm";

	/// Definitions and configuration for `values` on top of the snapshot home, keyed by file name.
	fn rendered(values: &[(&str, &str)]) -> BTreeMap<String, String> {
		let mut map: BTreeMap<String, String> =
			BTreeMap::from([("HOME".into(), SNAPSHOT_HOME.into())]);
		map.extend(
			values
				.iter()
				.map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
		);
		let settings: Settings =
			Settings::from_map(&map, crate::settings::Overrides::default()).unwrap();
		let entry: &Path = Path::new(SNAPSHOT_ENTRY);
		let host: launchd::fixture::LaunchdFixture = launchd::fixture::LaunchdFixture::default();
		let home: &Path = Path::new(SNAPSHOT_HOME);
		let mut files: BTreeMap<String, String> = systemd::render_units(entry, &settings).unwrap();
		files.extend(
			launchd::LaunchdManager::new(
				&host,
				&settings,
				home.join("Library/LaunchAgents"),
				home.join("Library/Logs/trapi2litellm"),
				launchd::LABEL_PREFIX,
			)
			.unwrap()
			.render(entry)
			.unwrap(),
		);
		let catalog: Value = serde_json::from_str(SNAPSHOT_CATALOG).unwrap();
		files.insert(
			"config.json".into(),
			String::from_utf8(
				crate::sync::render(&crate::catalog::build_config(&catalog, &settings).unwrap())
					.unwrap(),
			)
			.unwrap(),
		);
		files
	}

	#[test]
	fn managed_identity_output_matches_the_pre_relay_snapshots() {
		let snapshots: BTreeMap<&str, &str> = BTreeMap::from([
			(
				"config.json",
				include_str!("../tests/snapshots/managed-identity/config.json"),
			),
			(
				"io.github.acture.trapi2litellm.gateway.plist",
				include_str!(
					"../tests/snapshots/managed-identity/io.github.acture.trapi2litellm.gateway.plist"
				),
			),
			(
				"io.github.acture.trapi2litellm.sync.plist",
				include_str!(
					"../tests/snapshots/managed-identity/io.github.acture.trapi2litellm.sync.plist"
				),
			),
			(
				"litellm-trapi-sync.service",
				include_str!("../tests/snapshots/managed-identity/litellm-trapi-sync.service"),
			),
			(
				"litellm-trapi-sync.timer",
				include_str!("../tests/snapshots/managed-identity/litellm-trapi-sync.timer"),
			),
			(
				"litellm-trapi.service",
				include_str!("../tests/snapshots/managed-identity/litellm-trapi.service"),
			),
		]);
		for values in [
			&[("AZURE_CLIENT_ID", "identity-selector")][..],
			&[
				("AZURE_CLIENT_ID", "identity-selector"),
				("TRAPI2LITELLM_MODE", "managed_identity"),
			],
		] {
			let files: BTreeMap<String, String> = rendered(values);
			assert_eq!(
				files.keys().map(String::as_str).collect::<Vec<&str>>(),
				snapshots.keys().copied().collect::<Vec<&str>>()
			);
			for (name, snapshot) in &snapshots {
				assert_eq!(files[*name], *snapshot, "{name} changed");
			}
		}
	}

	#[test]
	fn gateway_definitions_relay_without_azure_credentials() {
		let files: BTreeMap<String, String> = rendered(&[
			("TRAPI2LITELLM_MODE", "gateway"),
			("TRAPI2LITELLM_UPSTREAM_URL", "http://127.0.0.1:14000/"),
		]);
		for (name, content) in &files {
			assert!(!content.contains("AZURE_"), "{name}");
			assert!(!content.contains("upstream.env"), "{name}");
			if name != "config.json" {
				assert!(!content.contains("TRAPI2LITELLM_UPSTREAM_KEY"), "{name}");
				assert!(!content.contains("TRAPI_"), "{name}");
			}
		}
		assert_eq!(
			files["litellm-trapi.service"],
			r#"# Managed by trapi2litellm
[Unit]
Description=Local TRAPI LiteLLM gateway (gateway relay)
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
EnvironmentFile=/trapi2litellm-fixture/home/.config/litellm-trapi/gateway.env
Environment="LITELLM_LOCAL_MODEL_COST_MAP=True"
Environment="NO_PROXY=127.0.0.1,localhost,169.254.169.254"
Environment="PYTHONDONTWRITEBYTECODE=1"
Environment="TRAPI2LITELLM_CONFIG_DIR=/trapi2litellm-fixture/home/.config/litellm-trapi"
Environment="TRAPI2LITELLM_MODE=gateway"
Environment="TRAPI2LITELLM_PORT=4000"
Environment="TRAPI2LITELLM_STATE_DIR=/trapi2litellm-fixture/home/.local/state/trapi2litellm"
Environment="TRAPI2LITELLM_UPSTREAM_URL=http://127.0.0.1:14000"
Environment="no_proxy=127.0.0.1,localhost,169.254.169.254"
UnsetEnvironment=TRAPI2LITELLM_PYTHON PYTHONPATH PYTHONHOME
Environment="CONFIG_FILE_PATH=/trapi2litellm-fixture/home/.config/litellm-trapi/config.yaml"
Environment=LITELLM_MODE=PRODUCTION
Environment=LITELLM_LOG=WARNING
ExecStart="/opt/trapi2litellm/bin/trapi2litellm" serve --port 4000
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5
TimeoutStopSec=930
KillMode=mixed
UMask=0077
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=default.target
"#
		);
		for name in [
			"litellm-trapi-sync.service",
			"io.github.acture.trapi2litellm.gateway.plist",
			"io.github.acture.trapi2litellm.sync.plist",
		] {
			assert!(
				files[name].contains("TRAPI2LITELLM_MODE")
					&& files[name].contains("http://127.0.0.1:14000"),
				"{name}"
			);
		}
		let plist: &str = &files["io.github.acture.trapi2litellm.gateway.plist"];
		assert!(plist.contains("<key>TRAPI2LITELLM_MODE</key>\n\t\t<string>gateway</string>\n"));
		assert!(plist.contains(
			"<key>TRAPI2LITELLM_UPSTREAM_URL</key>\n\t\t<string>http://127.0.0.1:14000</string>\n"
		));
		assert!(
			files["config.json"].contains("\"api_key\": \"os.environ/TRAPI2LITELLM_UPSTREAM_KEY\"")
		);
		// Another loopback forward joins the proxy exclusions, so the upstream key bypasses any
		// HTTP proxy the job environment names.
		for (url, no_proxy) in [
			(
				"http://[::1]:14000",
				"127.0.0.1,localhost,169.254.169.254,::1",
			),
			(
				"http://127.0.0.2:14000",
				"127.0.0.1,localhost,169.254.169.254,127.0.0.2",
			),
			(
				"http://localhost:14000",
				"127.0.0.1,localhost,169.254.169.254",
			),
		] {
			let files: BTreeMap<String, String> = rendered(&[
				("TRAPI2LITELLM_MODE", "gateway"),
				("TRAPI2LITELLM_UPSTREAM_URL", url),
			]);
			for name in ["litellm-trapi.service", "litellm-trapi-sync.service"] {
				for key in ["NO_PROXY", "no_proxy"] {
					assert!(
						files[name].contains(&format!("Environment=\"{key}={no_proxy}\"\n")),
						"{url} {name} {key}"
					);
				}
			}
			for name in [
				"io.github.acture.trapi2litellm.gateway.plist",
				"io.github.acture.trapi2litellm.sync.plist",
			] {
				for key in ["NO_PROXY", "no_proxy"] {
					assert!(
						files[name].contains(&format!(
							"<key>{key}</key>\n\t\t<string>{no_proxy}</string>\n"
						)),
						"{url} {name} {key}"
					);
				}
			}
		}
	}

	#[test]
	fn started_gateway_mode_requires_the_upstream_key_before_writing() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		check_upstream_key(&crate::settings::test_settings(root.path()), true).unwrap();
		let settings: Settings = crate::settings::test_gateway_settings(root.path());
		check_upstream_key(&settings, false).unwrap();
		assert!(
			format!("{:#}", check_upstream_key(&settings, true).unwrap_err())
				.contains("requires the upstream key file")
		);
		assert!(!settings.config_dir.exists());
		fs::create_dir_all(&settings.config_dir).unwrap();
		let path: PathBuf = settings.upstream_key_path();
		fs::write(&path, "TRAPI2LITELLM_UPSTREAM_KEY=sk-upstream\n").unwrap();
		fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
		assert!(check_upstream_key(&settings, true).is_err());
		fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
		check_upstream_key(&settings, true).unwrap();
	}

	#[test]
	fn ownership_covers_loaded_launch_agents() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: launchd::fixture::LaunchdFixture = launchd::fixture::LaunchdFixture::default();
		let manager: launchd::LaunchdManager<'_, launchd::fixture::LaunchdFixture> =
			launchd::LaunchdManager::new(
				&host,
				&settings,
				root.path().join("LaunchAgents"),
				root.path().join("Logs"),
				"io.github.acture.trapi2litellm.test",
			)
			.unwrap();
		let definitions: BTreeMap<String, String> =
			manager.render(Path::new("/bin/trapi2litellm")).unwrap();
		let clients: BTreeMap<String, String> = client_files(&settings);
		check_ownership(&manager, &settings, &definitions, &clients).unwrap();
		let agent: PathBuf = manager.loaded_definition(Job::Gateway);
		fs::create_dir_all(agent.parent().unwrap()).unwrap();
		fs::write(&agent, &definitions[manager.definition(Job::Gateway)]).unwrap();
		check_ownership(&manager, &settings, &definitions, &clients).unwrap();
		fs::write(&agent, "<?xml version=\"1.0\"?>\n<plist/>\n").unwrap();
		assert!(
			check_ownership(&manager, &settings, &definitions, &clients)
				.unwrap_err()
				.to_string()
				.contains("Refusing to overwrite unmanaged file")
		);
	}
}
