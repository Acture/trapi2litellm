pub mod systemd;
mod transaction;

use crate::{files, runtime::Runtime, settings::Settings, sync::Service};
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
/// unsupported `UnitFileState`: launchd reports `Persistent` when the loaded plist is present and
/// no `launchctl disable` override applies, and bails when file presence and override disagree.
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
	/// backups and atomic-write temporary files.
	fn definitions_dir(&self) -> &Path;
	/// File name of the installed definition that controls a job.
	fn definition(&self, job: Job) -> &str;
	/// File the manager loads a job from: the installed definition itself, or the copy that
	/// `activate_all` publishes outside `definitions_dir`.
	fn loaded_definition(&self, job: Job) -> PathBuf;
	/// Definition files keyed by file name.
	fn render(&self, entry: &Path) -> Result<BTreeMap<String, String>>;
	/// Whether an installed or loaded definition may be replaced with new content.
	fn is_managed(&self, old: &str, new: &str) -> bool;
	fn check_available(&self) -> Result<()>;
	fn preflight(&self, definitions: &BTreeMap<String, String>) -> Result<()>;
	/// Makes the manager pick up definitions installed into `definitions_dir`.
	fn definitions_changed(&self) -> Result<()>;
	/// Jobs whose loaded definition, not the installed copy, differs from the rendered
	/// `definitions` in a way that only a restart applies. Called before anything is installed.
	fn outdated(&self, definitions: &BTreeMap<String, String>) -> Result<BTreeSet<Job>>;
	/// Activation and enablement of a job. `active` means armed: the gateway is running, or the
	/// sync schedule is set (an active systemd timer; a loaded launchd calendar job, whose idle
	/// state is loaded but not running). States the transaction cannot restore fail closed, such
	/// as a systemd unit that is `activating` or a launchd gateway that is loaded but not running.
	fn state(&self, job: Job) -> Result<UnitState>;
	/// Publishes every installed definition where the manager loads it, enables every job and
	/// arms those that are not armed. Never restarts an armed job: the transaction restarts the
	/// `outdated` ones and reloads an up-to-date gateway.
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
	manager.check_available()?;
	let clients: BTreeMap<String, String> = client_files(settings);
	for (name, content) in &definitions {
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
}
