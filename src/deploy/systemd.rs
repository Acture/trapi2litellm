use super::{
	Enablement, Job, Linger, MARKER, ServiceManager, UnitState, gateway_environment,
	job_environment,
};
use crate::{
	process,
	settings::{Mode, SERVICE, Settings},
	sync,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
	collections::{BTreeMap, BTreeSet},
	fs,
	path::{Path, PathBuf},
	process::Command,
	time::Duration,
};

pub(super) const TIMER: &str = "litellm-trapi-sync.timer";
pub(super) const SYNC_SERVICE: &str = "litellm-trapi-sync.service";

pub trait Host {
	fn run(&self, program: &str, arguments: &[String]) -> Result<()>;
	fn preflight(&self, units: &BTreeMap<String, String>) -> Result<()>;
	fn unit_state(&self, name: &str) -> Result<UnitState>;
	fn linger_enabled(&self) -> Result<bool>;
}

impl UnitState {
	pub(super) fn parse(output: &str) -> Result<Self> {
		let properties: BTreeMap<&str, &str> = output
			.lines()
			.filter_map(|line| line.split_once('='))
			.collect();
		ensure!(
			matches!(properties.get("LoadState"), Some(&"loaded" | &"not-found")),
			"Cannot snapshot a missing, masked or invalid service-manager response"
		);
		let active: bool = match properties.get("ActiveState") {
			Some(&"active" | &"reloading") => true,
			Some(&"inactive" | &"failed") => false,
			_ => bail!("Cannot deploy while a service is changing activation state"),
		};
		let enablement: Enablement = match properties.get("UnitFileState") {
			Some(&"enabled") => Enablement::Persistent,
			Some(&"enabled-runtime") => Enablement::Runtime,
			Some(&"" | &"disabled" | &"static" | &"indirect") => Enablement::Disabled,
			_ => bail!("Unsupported unit enablement state; existing deployment kept"),
		};
		Ok(Self { active, enablement })
	}
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
	fn unit_state(&self, name: &str) -> Result<UnitState> {
		let output: std::process::Output = process::run(
			Command::new("systemctl").args([
				"--user",
				"show",
				"--property=LoadState,ActiveState,UnitFileState",
				name,
			]),
			&[],
			Duration::from_secs(15),
		)?;
		let properties: String = String::from_utf8(output.stdout)?;
		ensure!(
			output.status.success() || properties.lines().any(|line| line == "LoadState=not-found"),
			"Could not inspect service state ({})",
			output.status
		);
		UnitState::parse(&properties)
	}
	fn linger_enabled(&self) -> Result<bool> {
		// getuid has no pointer arguments or failure mode.
		let uid: u32 = unsafe { libc::getuid() };
		let output: std::process::Output = process::run(
			Command::new("loginctl").args([
				"show-user",
				&uid.to_string(),
				"--property=Linger",
				"--value",
			]),
			&[],
			Duration::from_secs(15),
		)?;
		ensure!(
			output.status.success(),
			"Could not inspect user linger state"
		);
		match String::from_utf8(output.stdout)?.trim() {
			"yes" => Ok(true),
			"no" => Ok(false),
			_ => bail!("Invalid user linger state"),
		}
	}
}

pub(super) fn control(host: &impl Host, arguments: &[&str]) -> Result<()> {
	host.run(
		"systemctl",
		&std::iter::once("--user")
			.chain(arguments.iter().copied())
			.map(str::to_owned)
			.collect::<Vec<String>>(),
	)
}

fn unit_quote(value: &str) -> Result<String> {
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
	// EnvironmentFile expands unit specifiers and then globs a raw filename.
	Ok(value
		.replace('\\', "\\\\")
		.replace('*', "\\*")
		.replace('?', "\\?")
		.replace('[', "\\[")
		.replace(']', "\\]")
		.replace('%', "%%"))
}

pub(super) fn render_units(entry: &Path, settings: &Settings) -> Result<BTreeMap<String, String>> {
	let command: String = unit_quote(&entry.to_string_lossy())?.replace('$', "$$");
	let shared: String = job_environment(settings)
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
	let gateway_only: String = gateway_environment(settings)
		.into_iter()
		.map(|(key, value)| format!("Environment={key}={value}\n"))
		.collect();
	let port: u16 = settings.port;
	let authentication: &str = match settings.mode {
		Mode::ManagedIdentity => "Managed Identity",
		Mode::Gateway { .. } => "gateway relay",
	};
	let gateway: String = format!(
		"{MARKER}[Unit]\nDescription=Local TRAPI LiteLLM gateway ({authentication})\nStartLimitIntervalSec=300\nStartLimitBurst=5\n\n[Service]\nType=simple\nEnvironmentFile={key_file}\n{shared}Environment={config}\n{gateway_only}ExecStart={command} serve --port {port}\nExecReload=/bin/kill -HUP $MAINPID\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=930\nKillMode=mixed\nUMask=0077\nNoNewPrivileges=true\nPrivateTmp=true\n\n[Install]\nWantedBy=default.target\n"
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

fn unit(job: Job) -> &'static str {
	match job {
		Job::Gateway => SERVICE,
		Job::Sync => TIMER,
	}
}

/// User systemd manager: the gateway service plus the hourly sync timer.
pub struct SystemdManager<'a, H: Host> {
	host: &'a H,
	settings: &'a Settings,
	unit_dir: PathBuf,
}

impl<'a, H: Host> SystemdManager<'a, H> {
	pub fn new(host: &'a H, settings: &'a Settings) -> Self {
		Self {
			host,
			settings,
			unit_dir: settings.user_config_home.join("systemd/user"),
		}
	}
}

impl<H: Host> ServiceManager for SystemdManager<'_, H> {
	fn definitions_dir(&self) -> &Path {
		&self.unit_dir
	}
	fn definitions_kind(&self) -> &str {
		"units"
	}
	fn definition(&self, job: Job) -> &str {
		unit(job)
	}
	fn loaded_definition(&self, job: Job) -> PathBuf {
		self.unit_dir.join(unit(job))
	}
	fn render(&self, entry: &Path) -> Result<BTreeMap<String, String>> {
		render_units(entry, self.settings)
	}
	fn is_managed(&self, old: &str, new: &str) -> bool {
		old.starts_with(MARKER)
			|| legacy_description(new)
				.is_some_and(|description| old.lines().any(|line| line == description))
	}
	fn check_available(&self, _start: bool) -> Result<()> {
		// Install-only also needs the user manager for daemon-reload.
		control(self.host, &["show-environment"])
	}
	fn preflight(&self, definitions: &BTreeMap<String, String>) -> Result<()> {
		self.host.preflight(definitions)
	}
	fn definitions_changed(&self) -> Result<()> {
		control(self.host, &["daemon-reload"])
	}
	fn outdated(&self, definitions: &BTreeMap<String, String>) -> Result<BTreeSet<Job>> {
		// daemon-reload re-arms the timer, and every oneshot sync run reads the reloaded unit.
		let path: PathBuf = self.loaded_definition(Job::Gateway);
		let current: bool = path.exists() && fs::read(&path)? == definitions[SERVICE].as_bytes();
		Ok(if current {
			BTreeSet::new()
		} else {
			BTreeSet::from([Job::Gateway])
		})
	}
	fn state(&self, job: Job) -> Result<UnitState> {
		self.host.unit_state(unit(job))
	}
	fn activate_all(&self) -> Result<()> {
		control(self.host, &["enable", "--now", SERVICE, TIMER])
	}
	fn restart(&self, job: Job) -> Result<()> {
		control(self.host, &["restart", unit(job)])
	}
	fn start(&self, job: Job) -> Result<()> {
		control(self.host, &["start", unit(job)])
	}
	fn stop_all(&self) -> Result<()> {
		control(self.host, &["stop", TIMER, SYNC_SERVICE, SERVICE])
	}
	fn disable_all(&self) -> Result<()> {
		control(self.host, &["disable", SERVICE, TIMER])
	}
	fn restore_enablement(&self, job: Job, enablement: Enablement) -> Result<()> {
		match enablement {
			Enablement::Disabled => Ok(()),
			Enablement::Persistent => control(self.host, &["enable", unit(job)]),
			Enablement::Runtime => control(self.host, &["enable", "--runtime", unit(job)]),
		}
	}
	fn same_target(&self, old_gateway: &str) -> Result<bool> {
		let settings: &Settings = self.settings;
		let config_line: String = format!(
			"Environment={}",
			unit_quote(&format!(
				"CONFIG_FILE_PATH={}",
				settings.config_path().display()
			))?
		);
		let state_line: String = format!(
			"Environment={}",
			unit_quote(&format!(
				"TRAPI2LITELLM_STATE_DIR={}",
				settings.state_dir.display()
			))?
		);
		Ok(old_gateway.lines().any(|line| line == config_line)
			&& old_gateway.lines().any(|line| line == state_line)
			&& old_gateway.lines().any(|line| {
				line.starts_with("ExecStart=")
					&& line.ends_with(&format!(" serve --port {}", settings.port))
			}))
	}
	fn linger(&self) -> Result<&dyn Linger> {
		Ok(self)
	}
}

impl<H: Host> Linger for SystemdManager<'_, H> {
	fn enabled(&self) -> Result<bool> {
		self.host.linger_enabled()
	}
	fn set(&self, enabled: bool) -> Result<()> {
		// getuid has no pointer arguments or failure mode.
		let uid: u32 = unsafe { libc::getuid() };
		let operation: &str = if enabled {
			"enable-linger"
		} else {
			"disable-linger"
		};
		self.host
			.run("loginctl", &[operation.into(), uid.to_string()])
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::deploy::client_files;
	#[test]
	fn escaping_and_rendering() {
		assert_eq!(unit_quote("/a b/%u").unwrap(), "\"/a b/%%u\"");
		assert!(unit_quote("/tmp/a\nExecStart=oops").is_err());
		assert!(unit_env_file(Path::new("relative/gateway.env")).is_err());
		assert!(unit_env_file(Path::new("/tmp/a\nEnvironmentFile=oops")).is_err());
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let mut settings: Settings = crate::settings::test_settings(root.path());
		settings.config_dir = root.path().join("config space%u\"\\$HOME*?[x]");
		let units: BTreeMap<String, String> =
			render_units(Path::new("/opt/$gateway/bin/trapi2litellm"), &settings).unwrap();
		assert_eq!(units.len(), 3);
		let gateway: &String = &units[SERVICE];
		assert!(gateway.contains("/opt/$$gateway/bin/trapi2litellm"));
		assert!(gateway.contains("serve --port 4000"));
		assert!(gateway.contains("ExecReload=/bin/kill -HUP $MAINPID"));
		assert!(gateway.contains(&format!(
			"EnvironmentFile={}/config space%%u\"\\\\$HOME\\*\\?\\[x\\]/gateway.env\n",
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
	fn environment_file_glob_selects_only_literal_key() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let key: PathBuf = root.path().join("config [xy]*?\\%u/gateway.env");
		let decoy: PathBuf = root.path().join("config xa%u/gateway.env");
		for path in [&key, &decoy] {
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, "LITELLM_MASTER_KEY=fixture\n").unwrap();
		}
		// systemd expands percent specifiers before calling POSIX glob().
		let pattern: std::ffi::CString =
			std::ffi::CString::new(unit_env_file(&key).unwrap().replace("%%", "%")).unwrap();
		// glob() initializes this zeroed output; pattern remains alive throughout.
		let mut paths: libc::glob_t = unsafe { std::mem::zeroed() };
		let result: i32 = unsafe { libc::glob(pattern.as_ptr(), 0, None, &mut paths) };
		let selected: Vec<PathBuf> = if result == 0 {
			(0..paths.gl_pathc)
				.map(|index| {
					// A successful glob() provides gl_pathc valid, NUL-terminated strings.
					let path: &std::ffi::CStr =
						unsafe { std::ffi::CStr::from_ptr(*paths.gl_pathv.add(index)) };
					PathBuf::from(path.to_str().unwrap())
				})
				.collect()
		} else {
			Vec::new()
		};
		// globfree() accepts the initialized output even when glob() fails.
		unsafe { libc::globfree(&mut paths) };
		assert_eq!(result, 0);
		assert_eq!(selected, vec![key]);
	}
	#[test]
	fn service_state_parser_rejects_unstable_or_masked_units() {
		assert_eq!(
			UnitState::parse("LoadState=not-found\nActiveState=inactive\nUnitFileState=\n")
				.unwrap(),
			UnitState::default()
		);
		assert!(
			UnitState::parse("LoadState=loaded\nActiveState=activating\nUnitFileState=enabled\n")
				.is_err()
		);
		assert!(
			UnitState::parse("LoadState=masked\nActiveState=inactive\nUnitFileState=masked\n")
				.is_err()
		);
	}
}
