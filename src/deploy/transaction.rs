use super::{
	DeployOptions, Enablement, Host, SYNC_SERVICE, TIMER, UnitState, managed_write, unit_quote,
};
use crate::{
	catalog, files,
	runtime::Runtime,
	settings::{SERVICE, Settings},
	sync::{self, Service},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
	collections::{BTreeMap, BTreeSet},
	fs,
	os::unix::fs::PermissionsExt,
	path::{Path, PathBuf},
};

struct SavedFile {
	bytes: Vec<u8>,
	mode: u32,
}

impl SavedFile {
	fn read(path: &Path) -> Result<Option<Self>> {
		match fs::read(path) {
			Ok(bytes) => Ok(Some(Self {
				bytes,
				mode: fs::metadata(path)?.permissions().mode() & 0o777,
			})),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
			Err(error) => Err(error.into()),
		}
	}
	fn restore(path: &Path, saved: Option<&Self>) -> Result<()> {
		if let Some(saved) = saved {
			files::atomic_write(path, &saved.bytes)?;
			fs::set_permissions(path, fs::Permissions::from_mode(saved.mode))?;
		} else {
			match fs::remove_file(path) {
				Ok(()) => {}
				Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
				Err(error) => return Err(error.into()),
			}
		}
		Ok(())
	}
}

struct DeploymentSnapshot {
	files: BTreeMap<PathBuf, Option<SavedFile>>,
	gateway: UnitState,
	timer: UnitState,
	linger: Option<bool>,
}

fn control(host: &impl Host, arguments: &[&str]) -> Result<()> {
	host.run(
		"systemctl",
		&std::iter::once("--user")
			.chain(arguments.iter().copied())
			.map(str::to_owned)
			.collect::<Vec<String>>(),
	)
}

impl DeploymentSnapshot {
	fn capture(
		host: &impl Host,
		settings: &Settings,
		options: &DeployOptions,
		units: &BTreeMap<String, String>,
		clients: &BTreeMap<String, String>,
	) -> Result<Self> {
		let unit_dir: PathBuf = settings.user_config_home.join("systemd/user");
		let paths = units
			.keys()
			.map(|name| unit_dir.join(name))
			.chain(clients.keys().map(|name| settings.config_dir.join(name)))
			.chain([
				settings.config_path(),
				settings.state_dir.join("catalog.json"),
				settings.state_dir.join("sync-status.json"),
			]);
		let files: BTreeMap<PathBuf, Option<SavedFile>> = paths
			.map(|path| Ok((path.clone(), SavedFile::read(&path)?)))
			.collect::<Result<_>>()?;
		let snapshot: Self = Self {
			files,
			gateway: host.unit_state(SERVICE)?,
			timer: host.unit_state(TIMER)?,
			linger: options
				.enable_linger
				.then(|| host.linger_enabled())
				.transpose()?,
		};
		if snapshot.gateway.active {
			ensure!(
				snapshot.config(settings).is_some(),
				"An active gateway requires its existing configuration directory"
			);
			let old_unit: &SavedFile = snapshot.files[&unit_dir.join(SERVICE)]
				.as_ref()
				.context("An active gateway requires its existing managed unit")?;
			let old_unit: &str = std::str::from_utf8(&old_unit.bytes)?;
			let config_line: String = format!(
				"Environment={}",
				unit_quote(&format!(
					"CONFIG_FILE_PATH={}",
					settings.config_path().display()
				))?
			);
			ensure!(
				old_unit.lines().any(|line| line == config_line)
					&& old_unit.lines().any(|line| {
						line.starts_with("ExecStart=")
							&& line.ends_with(&format!(" serve --port {}", settings.port))
					}),
				"Stop the gateway before changing its configuration directory or port"
			);
		}
		Ok(snapshot)
	}
	fn config(&self, settings: &Settings) -> Option<&[u8]> {
		self.files[&settings.config_path()]
			.as_ref()
			.map(|saved| saved.bytes.as_slice())
	}
	fn rollback(
		&self,
		host: &impl Host,
		service: &impl Service,
		settings: &Settings,
		activated: bool,
		previous_models: &[String],
	) -> Result<()> {
		let mut errors: Vec<anyhow::Error> = Vec::new();
		if activated {
			if let Err(error) = control(host, &["stop", TIMER, SYNC_SERVICE, SERVICE]) {
				errors.push(error.context("Rollback stop failed"));
			}
			if let Err(error) = control(host, &["disable", SERVICE, TIMER]) {
				errors.push(error.context("Rollback disable failed"));
			}
		}
		let mut restored: bool = true;
		for (path, saved) in &self.files {
			if let Err(error) = SavedFile::restore(path, saved.as_ref()) {
				restored = false;
				errors.push(error.context(format!("Could not restore {}", path.display())));
			}
		}
		if let Err(error) = control(host, &["daemon-reload"]) {
			restored = false;
			errors.push(error.context("Rollback daemon-reload failed"));
		}
		if activated {
			for (name, state) in [(SERVICE, self.gateway), (TIMER, self.timer)] {
				let arguments: &[&str] = match state.enablement {
					Enablement::Disabled => continue,
					Enablement::Persistent => &["enable", name],
					Enablement::Runtime => &["enable", "--runtime", name],
				};
				if let Err(error) = control(host, arguments) {
					errors.push(error.context(format!("Could not restore {name} enablement")));
				}
			}
			if restored {
				if self.gateway.active {
					let recovery: Result<()> =
						control(host, &["restart", SERVICE]).and_then(|()| {
							service.wait_for_models(
								&previous_models
									.iter()
									.cloned()
									.collect::<BTreeSet<String>>(),
								&catalog::digest(
									self.config(settings)
										.expect("Captured active configuration"),
								),
							)
						});
					if let Err(error) = recovery {
						errors.push(error.context("Previous gateway failed rollback readiness"));
					}
				}
				if self.timer.active
					&& let Err(error) = control(host, &["start", TIMER])
				{
					errors.push(error.context("Could not restore timer activation"));
				}
			}
			if self.linger == Some(false) {
				// getuid has no pointer arguments or failure mode.
				let uid: u32 = unsafe { libc::getuid() };
				if let Err(error) =
					host.run("loginctl", &["disable-linger".into(), uid.to_string()])
				{
					errors.push(error.context("Could not restore linger state"));
				}
			}
			for (name, expected) in [(SERVICE, self.gateway), (TIMER, self.timer)] {
				let verification: Result<()> = host.unit_state(name).and_then(|actual| {
					ensure!(
						actual == expected,
						"{name} activation/enablement was not restored"
					);
					Ok(())
				});
				if let Err(error) = verification {
					errors.push(error);
				}
			}
			if let Some(expected) = self.linger {
				let verification: Result<()> = host.linger_enabled().and_then(|actual| {
					ensure!(actual == expected, "User linger state was not restored");
					Ok(())
				});
				if let Err(error) = verification {
					errors.push(error);
				}
			}
		}
		if errors.is_empty() {
			Ok(())
		} else {
			bail!(
				"Deployment rollback incomplete: {}",
				errors
					.iter()
					.map(|error| format!("{error:#}"))
					.collect::<Vec<String>>()
					.join("; ")
			)
		}
	}
}

fn deployment_status(
	settings: &Settings,
	status: &str,
	phase: &str,
	attempted: Option<&sync::SyncResult>,
	config: Option<&[u8]>,
) -> Result<()> {
	files::atomic_write(
		&settings.state_dir.join("deployment-status.json"),
		&serde_json::to_vec_pretty(&json!({
			"checked_at": sync::now(), "status": status, "phase": phase,
			"attempted_sha256": attempted.map(|result| &result.config_sha256),
			"config_sha256": config.map(catalog::digest),
		}))?,
	)
}

fn install_units(
	host: &impl Host,
	settings: &Settings,
	units: &BTreeMap<String, String>,
	clients: &BTreeMap<String, String>,
) -> Result<bool> {
	let unit_dir: PathBuf = settings.user_config_home.join("systemd/user");
	fs::create_dir_all(&unit_dir)?;
	files::private_directory(&settings.config_dir)?;
	let mut gateway_changed: bool = false;
	for (name, content) in units {
		let changed: bool = managed_write(&unit_dir.join(name), content, true)?;
		gateway_changed |= changed && name == SERVICE;
	}
	for (name, content) in clients {
		managed_write(&settings.config_dir.join(name), content, false)?;
	}
	control(host, &["daemon-reload"])?;
	Ok(gateway_changed)
}

pub(super) fn apply_deployment(
	runtime: &impl Runtime,
	service: &impl Service,
	host: &impl Host,
	settings: &Settings,
	options: &DeployOptions,
	units: &BTreeMap<String, String>,
	clients: &BTreeMap<String, String>,
) -> Result<()> {
	let lock: files::SyncLock = sync::lock(settings)?;
	if !options.start {
		install_units(host, settings, units, clients)?;
		println!(
			"Installed units only. To bootstrap and start: trapi2litellm deploy --start (with the same settings)"
		);
		return Ok(());
	}
	let snapshot: DeploymentSnapshot =
		DeploymentSnapshot::capture(host, settings, options, units, clients)?;
	let mut phase: &str = "installation";
	let mut activated: bool = false;
	let mut attempted: Option<sync::SyncResult> = None;
	let activation: Result<()> = (|| {
		let gateway_changed: bool = install_units(host, settings, units, clients)?;
		phase = "synchronization";
		attempted = Some(sync::synchronize_locked(
			runtime, service, settings, true, true, &lock,
		)?);
		let result: &sync::SyncResult = attempted.as_ref().expect("Synchronization completed");
		phase = "activation";
		activated = true;
		if options.enable_linger {
			// getuid has no pointer arguments or failure mode.
			let uid: u32 = unsafe { libc::getuid() };
			host.run("loginctl", &["enable-linger".into(), uid.to_string()])?;
		}
		control(host, &["enable", "--now", SERVICE, TIMER])?;
		if snapshot.gateway.active {
			if gateway_changed {
				control(host, &["restart", SERVICE])?;
			} else {
				service.reload()?;
			}
		}
		let config: catalog::Config = serde_json::from_slice(&fs::read(settings.config_path())?)?;
		service.wait_for_models(&catalog::model_names(&config), &result.config_sha256)?;
		deployment_status(
			settings,
			"ready",
			phase,
			attempted.as_ref(),
			Some(&fs::read(settings.config_path())?),
		)?;
		println!(
			"{}",
			json!({"status": "ready", "base_url": format!("{}/v1", settings.local_url()), "models": config.model_list.len(), "key_file": settings.key_path()})
		);
		Ok(())
	})();
	if let Err(error) = activation {
		eprintln!("INFO deployment failed during {phase}; restoring previous deployment");
		let rejected: Result<()> = (|| {
			if let Some(current) = SavedFile::read(&settings.config_path())?
				&& (phase == "activation"
					|| snapshot
						.config(settings)
						.is_none_or(|old| old != current.bytes))
			{
				files::atomic_write(
					&settings.state_dir.join("config.rejected.yaml"),
					&current.bytes,
				)
			} else {
				Ok(())
			}
		})();
		let previous_models: &[String] = attempted
			.as_ref()
			.map_or(&[], |result| result.previous_models.as_slice());
		let rollback: Result<()> =
			snapshot.rollback(host, service, settings, activated, previous_models);
		let status: &str = if rollback.is_ok() {
			"rolled_back"
		} else {
			"rollback_failed"
		};
		let current: Option<SavedFile> = SavedFile::read(&settings.config_path())?;
		deployment_status(
			settings,
			status,
			phase,
			attempted.as_ref(),
			current.as_ref().map(|saved| saved.bytes.as_slice()),
		)
		.with_context(|| format!("Deployment failed: {error}; could not record {status} status"))?;
		if let Err(recovery) = rollback {
			bail!("Deployment failed: {error}; {recovery:#}");
		}
		rejected.context("Deployment restored but rejected configuration could not be saved")?;
		return Err(error);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::deploy::{MARKER, SystemHost, client_files, render_units};
	use serde_json::Value;
	use std::cell::{Cell, RefCell};
	use std::{env, time::Duration};

	struct FixtureRuntime {
		models: Vec<String>,
		failure: Option<&'static str>,
	}
	impl Runtime for FixtureRuntime {
		fn catalog(&self, _: &Settings) -> Result<Value> {
			ensure!(self.failure != Some("fetch"), "Fixture fetch failure");
			Ok(catalog::catalog(
				&self
					.models
					.iter()
					.map(String::as_str)
					.collect::<Vec<&str>>(),
			))
		}
		fn validate(&self, _: &catalog::Config, previous: Option<&str>) -> Result<Vec<String>> {
			ensure!(
				self.failure != Some("validate"),
				"Fixture validation failure"
			);
			previous.map_or_else(
				|| Ok(Vec::new()),
				|text| {
					Ok(serde_json::from_str::<catalog::Config>(text)?
						.model_list
						.into_iter()
						.map(|model| model.model_name)
						.collect())
				},
			)
		}
	}

	#[derive(Default)]
	struct FixtureHost {
		units: RefCell<BTreeMap<String, UnitState>>,
		linger: Cell<bool>,
		failure: Cell<Option<&'static str>>,
	}
	impl Host for FixtureHost {
		fn run(&self, program: &str, arguments: &[String]) -> Result<()> {
			let operation: &str = if program == "loginctl" {
				&arguments[0]
			} else {
				&arguments[1]
			};
			if program == "loginctl" {
				self.linger.set(operation == "enable-linger");
			} else {
				for name in arguments
					.iter()
					.skip(2)
					.filter(|argument| !argument.starts_with('-'))
				{
					let mut units = self.units.borrow_mut();
					let state: &mut UnitState = units.entry(name.clone()).or_default();
					match operation {
						"enable" => {
							state.enablement =
								if arguments.iter().any(|argument| argument == "--runtime") {
									Enablement::Runtime
								} else {
									Enablement::Persistent
								};
							state.active |= arguments.iter().any(|argument| argument == "--now");
						}
						"disable" => state.enablement = Enablement::Disabled,
						"stop" => state.active = false,
						"start" | "restart" => state.active = true,
						_ => bail!("Unexpected fixture operation {operation}"),
					}
				}
			}
			if self.failure.get() == Some(operation) {
				self.failure.set(None);
				bail!("Fixture {operation} failure after partial application");
			}
			Ok(())
		}
		fn preflight(&self, _: &BTreeMap<String, String>) -> Result<()> {
			Ok(())
		}
		fn unit_state(&self, name: &str) -> Result<UnitState> {
			Ok(self.units.borrow().get(name).copied().unwrap_or_default())
		}
		fn linger_enabled(&self) -> Result<bool> {
			Ok(self.linger.get())
		}
	}

	struct FixtureService<'a> {
		host: &'a FixtureHost,
		settings: &'a Settings,
		reload_failure: Cell<bool>,
		readiness_failures: Cell<u8>,
	}
	impl Service for FixtureService<'_> {
		fn active(&self) -> Result<bool> {
			Ok(self.host.unit_state(SERVICE)?.active)
		}
		fn reload(&self) -> Result<()> {
			ensure!(
				!self.reload_failure.replace(false),
				"Fixture reload failure"
			);
			Ok(())
		}
		fn wait_for_models(&self, expected: &BTreeSet<String>, digest: &str) -> Result<()> {
			let lock: fs::File = fs::File::open(self.settings.state_dir.join("sync.lock"))?;
			ensure!(
				matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
				"Deployment released its synchronization lock too soon"
			);
			let failures: u8 = self.readiness_failures.get();
			if failures > 0 {
				self.readiness_failures.set(failures - 1);
				bail!("Fixture readiness failure");
			}
			let bytes: Vec<u8> = fs::read(self.settings.config_path())?;
			let config: catalog::Config = serde_json::from_slice(&bytes)?;
			ensure!(self.active()?, "Gateway was not started");
			ensure!(
				&catalog::model_names(&config) == expected && catalog::digest(&bytes) == digest,
				"Restored configuration did not pass readiness"
			);
			Ok(())
		}
	}

	#[test]
	fn deployment_faults_restore_files_activation_and_metadata() -> Result<()> {
		for existing in [false, true] {
			for failure in [
				"daemon-reload",
				"fetch",
				"validate",
				"enable-linger",
				"enable",
				"restart",
				"reload",
				"readiness",
				"rollback-readiness",
				"rejected",
			] {
				if !existing && matches!(failure, "restart" | "reload" | "rollback-readiness") {
					continue;
				}
				let root: tempfile::TempDir = tempfile::tempdir()?;
				let settings: Settings = crate::settings::test_settings(root.path());
				let options: DeployOptions = DeployOptions {
					entry: root.path().join("bin/trapi2litellm"),
					start: true,
					enable_linger: true,
					dry_run: false,
				};
				let mut runtime: FixtureRuntime = FixtureRuntime {
					models: vec!["old".into()],
					failure: None,
				};
				let host: FixtureHost = FixtureHost::default();
				let existing_linger: bool = existing && matches!(failure, "enable" | "readiness");
				host.linger.set(existing_linger);
				let service: FixtureService<'_> = FixtureService {
					host: &host,
					settings: &settings,
					reload_failure: Cell::new(false),
					readiness_failures: Cell::new(0),
				};
				let mut units: BTreeMap<String, String> = render_units(&options.entry, &settings)?;
				let clients: BTreeMap<String, String> = client_files(&settings);
				if existing {
					install_units(&host, &settings, &units, &clients)?;
					sync::synchronize(&runtime, &service, &settings, true, true)?;
					host.units.borrow_mut().insert(
						SERVICE.into(),
						UnitState {
							active: true,
							enablement: Enablement::Runtime,
						},
					);
					host.units.borrow_mut().insert(
						TIMER.into(),
						UnitState {
							active: true,
							enablement: Enablement::Disabled,
						},
					);
				}
				let _lock: files::SyncLock = sync::lock(&settings)?;
				let before: DeploymentSnapshot =
					DeploymentSnapshot::capture(&host, &settings, &options, &units, &clients)?;
				drop(_lock);
				runtime.models.push("new".into());
				match failure {
					"fetch" | "validate" => runtime.failure = Some(failure),
					"reload" => service.reload_failure.set(true),
					"readiness" | "rejected" => service.readiness_failures.set(1),
					"rollback-readiness" => service.readiness_failures.set(2),
					operation => host.failure.set(Some(operation)),
				}
				if failure == "restart" {
					units.get_mut(SERVICE).unwrap().push_str("# changed unit\n");
				}
				if failure == "rejected" {
					fs::create_dir(settings.state_dir.join("config.rejected.yaml"))?;
				}
				let outcome: Result<()> = apply_deployment(
					&runtime, &service, &host, &settings, &options, &units, &clients,
				);
				ensure!(
					outcome.is_err(),
					"{existing}/{failure}: failure was not injected"
				);
				for (path, saved) in &before.files {
					let current: Option<SavedFile> = SavedFile::read(path)?;
					ensure!(
						current.as_ref().map(|file| (&file.bytes, file.mode))
							== saved.as_ref().map(|file| (&file.bytes, file.mode)),
						"{existing}/{failure}: {} was not restored",
						path.display()
					);
				}
				ensure!(
					host.unit_state(SERVICE)? == before.gateway
						&& host.unit_state(TIMER)? == before.timer,
					"{existing}/{failure}: activation state was not restored"
				);
				ensure!(
					host.linger.get() == existing_linger,
					"Linger was not restored"
				);
				if before.config(&settings).is_none() && !matches!(failure, "daemon-reload") {
					ensure!(
						settings.key_path().is_file(),
						"Generated retry key was not retained"
					);
				}
				let status: Value = serde_json::from_slice(&fs::read(
					settings.state_dir.join("deployment-status.json"),
				)?)?;
				ensure!(
					status["status"]
						== if failure == "rollback-readiness" {
							"rollback_failed"
						} else {
							"rolled_back"
						},
					"Incorrect rollback status for {failure}"
				);
				ensure!(
					status["config_sha256"] == json!(before.config(&settings).map(catalog::digest)),
					"Incorrect restored hash"
				);
			}
		}
		Ok(())
	}

	#[test]
	fn deployment_success_preserves_key_and_marks_ready() -> Result<()> {
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let options: DeployOptions = DeployOptions {
			entry: root.path().join("bin/trapi2litellm"),
			start: true,
			enable_linger: false,
			dry_run: false,
		};
		let runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["ready".into()],
			failure: None,
		};
		let host: FixtureHost = FixtureHost::default();
		let service: FixtureService<'_> = FixtureService {
			host: &host,
			settings: &settings,
			reload_failure: Cell::new(false),
			readiness_failures: Cell::new(0),
		};
		let units: BTreeMap<String, String> = render_units(&options.entry, &settings)?;
		let clients: BTreeMap<String, String> = client_files(&settings);
		apply_deployment(
			&runtime, &service, &host, &settings, &options, &units, &clients,
		)?;
		let key: String = files::local_key(&settings.key_path())?;
		apply_deployment(
			&runtime, &service, &host, &settings, &options, &units, &clients,
		)?;
		ensure!(
			files::local_key(&settings.key_path())? == key,
			"Redeploy rotated the key"
		);
		let status: Value = serde_json::from_slice(&fs::read(
			settings.state_dir.join("deployment-status.json"),
		)?)?;
		ensure!(
			status["status"] == "ready",
			"Deployment status was not ready"
		);
		Ok(())
	}

	#[test]
	fn active_reconfiguration_is_rejected_before_publication() -> Result<()> {
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let options: DeployOptions = DeployOptions {
			entry: root.path().join("bin/trapi2litellm"),
			start: true,
			enable_linger: false,
			dry_run: false,
		};
		let runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["old".into()],
			failure: None,
		};
		let host: FixtureHost = FixtureHost::default();
		let service: FixtureService<'_> = FixtureService {
			host: &host,
			settings: &settings,
			reload_failure: Cell::new(false),
			readiness_failures: Cell::new(0),
		};
		let units: BTreeMap<String, String> = render_units(&options.entry, &settings)?;
		let clients: BTreeMap<String, String> = client_files(&settings);
		apply_deployment(
			&runtime, &service, &host, &settings, &options, &units, &clients,
		)?;
		let previous: Vec<u8> = fs::read(settings.config_path())?;
		for change_port in [false, true] {
			let mut changed: Settings = settings.clone();
			if change_port {
				changed.port += 1;
			} else {
				changed.config_dir = root.path().join("different-config");
			}
			let outcome: Result<()> = apply_deployment(
				&runtime,
				&service,
				&host,
				&changed,
				&options,
				&render_units(&options.entry, &changed)?,
				&client_files(&changed),
			);
			ensure!(
				outcome.is_err(),
				"An active gateway was reconfigured without a valid rollback target"
			);
			ensure!(
				fs::read(settings.config_path())? == previous && host.unit_state(SERVICE)?.active,
				"Reconfiguration changed the existing gateway"
			);
			let unit_dir: PathBuf = settings.user_config_home.join("systemd/user");
			for (name, content) in &units {
				ensure!(
					fs::read_to_string(unit_dir.join(name))? == *content,
					"Reconfiguration replaced an existing unit"
				);
			}
		}
		Ok(())
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

	struct InterruptedEnable;
	impl Host for InterruptedEnable {
		fn run(&self, program: &str, arguments: &[String]) -> Result<()> {
			if program == "systemctl" && arguments.iter().any(|argument| argument == "--now") {
				control(&SystemHost, &["enable", "--now", SERVICE])?;
				bail!("Injected failure after starting the gateway");
			}
			SystemHost.run(program, arguments)
		}
		fn preflight(&self, units: &BTreeMap<String, String>) -> Result<()> {
			SystemHost.preflight(units)
		}
		fn unit_state(&self, name: &str) -> Result<UnitState> {
			SystemHost.unit_state(name)
		}
		fn linger_enabled(&self) -> Result<bool> {
			SystemHost.linger_enabled()
		}
	}

	struct InterruptedReadiness<'a> {
		inner: sync::SystemService<'a>,
		interrupt: Cell<bool>,
	}
	impl Service for InterruptedReadiness<'_> {
		fn active(&self) -> Result<bool> {
			self.inner.active()
		}
		fn reload(&self) -> Result<()> {
			self.inner.reload()
		}
		fn wait_for_models(&self, expected: &BTreeSet<String>, digest: &str) -> Result<()> {
			ensure!(!self.interrupt.replace(false), "Injected readiness failure");
			self.inner.wait_for_models(expected, digest)
		}
	}

	#[test]
	#[ignore = "requires isolated persistent installation and a real user systemd manager"]
	fn systemd_activation_rollback() -> Result<()> {
		ensure!(
			cfg!(target_os = "linux"),
			"Systemd acceptance requires Linux"
		);
		let entry: PathBuf = env::var_os("TRAPI2LITELLM_ACCEPTANCE_ENTRY")
			.context("Use packaging/check_dist.py --tool-root on an isolated Linux host")?
			.into();
		ensure!(
			SystemHost.unit_state(SERVICE)? == UnitState::default()
				&& SystemHost.unit_state(TIMER)? == UnitState::default(),
			"Acceptance requires no existing gateway or timer"
		);
		let mut settings: Settings = Settings::from_env(crate::settings::Overrides::default())?;
		let name: String = format!("activation-rollback-{}", std::process::id());
		settings.config_dir = settings.config_dir.join(&name);
		settings.state_dir = settings.state_dir.join(name);
		let options: DeployOptions = DeployOptions {
			entry,
			start: true,
			enable_linger: false,
			dry_run: false,
		};
		let service: InterruptedReadiness<'_> = InterruptedReadiness {
			inner: sync::SystemService {
				settings: &settings,
			},
			interrupt: Cell::new(false),
		};
		let mut runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["offline".into()],
			failure: None,
		};
		let mut units: BTreeMap<String, String> = render_units(&options.entry, &settings)?;
		// Keep the real timer lifecycle, but never let it authenticate to Azure.
		units.insert(
			SYNC_SERVICE.into(),
			format!("{MARKER}[Service]\nType=oneshot\nExecStart=/bin/true\n"),
		);
		let clients: BTreeMap<String, String> = client_files(&settings);
		SystemHost.preflight(&units)?;
		let outcome: Result<()> = (|| {
			let failure: Result<()> = apply_deployment(
				&runtime,
				&service,
				&InterruptedEnable,
				&settings,
				&options,
				&units,
				&clients,
			);
			ensure!(
				failure.is_err(),
				"First-deployment failure was not injected"
			);
			ensure!(
				!settings.config_path().exists() && settings.key_path().exists(),
				"First deployment did not restore configuration and retain its retry key"
			);
			ensure!(
				SystemHost.unit_state(SERVICE)? == UnitState::default()
					&& SystemHost.unit_state(TIMER)? == UnitState::default(),
				"First deployment left an active or enabled unit"
			);

			sync::synchronize(&runtime, &service, &settings, true, true)?;
			install_units(&SystemHost, &settings, &units, &clients)?;
			control(&SystemHost, &["enable", "--runtime", SERVICE])?;
			control(&SystemHost, &["start", SERVICE, TIMER])?;
			let before: DeploymentSnapshot =
				DeploymentSnapshot::capture(&SystemHost, &settings, &options, &units, &clients)?;
			let previous: Vec<u8> = fs::read(settings.config_path())?;
			let models: BTreeSet<String> = BTreeSet::from(["trapi/offline".into()]);
			service.wait_for_models(&models, &catalog::digest(&previous))?;
			runtime.models.push("new".into());
			service.interrupt.set(true);
			let failure: Result<()> = apply_deployment(
				&runtime,
				&service,
				&SystemHost,
				&settings,
				&options,
				&units,
				&clients,
			);
			ensure!(failure.is_err(), "Redeployment failure was not injected");
			ensure!(
				fs::read(settings.config_path())? == previous,
				"Previous configuration was not restored"
			);
			ensure!(
				SystemHost.unit_state(SERVICE)? == before.gateway
					&& SystemHost.unit_state(TIMER)? == before.timer,
				"Previous unit activation/enablement was not restored"
			);
			service.wait_for_models(&models, &catalog::digest(&previous))?;
			let status: Value = serde_json::from_slice(&fs::read(
				settings.state_dir.join("deployment-status.json"),
			)?)?;
			ensure!(
				status["status"] == "rolled_back",
				"Rollback was not recorded"
			);
			let client: reqwest::blocking::Client = reqwest::blocking::Client::builder()
				.no_proxy()
				.timeout(Duration::from_secs(5))
				.redirect(reqwest::redirect::Policy::none())
				.build()?;
			let response: Value = client
				.get(format!("{}/status", settings.local_url()))
				.bearer_auth(files::local_key(&settings.key_path())?)
				.send()?
				.error_for_status()?
				.json()?;
			ensure!(
				response["deployment-status"]["status"] == "rolled_back",
				"Running gateway did not expose deployment rollback status"
			);
			Ok(())
		})();
		let cleanup: Result<()> = (|| {
			let unit_dir: PathBuf = settings.user_config_home.join("systemd/user");
			let installed: Vec<&str> = units
				.keys()
				.filter(|name| unit_dir.join(name).exists())
				.map(String::as_str)
				.collect();
			if !installed.is_empty() {
				control(&SystemHost, &[&["stop"][..], &installed].concat())?;
				control(&SystemHost, &["disable", SERVICE, TIMER])?;
				control(&SystemHost, &["disable", "--runtime", SERVICE, TIMER])?;
			}
			for name in units.keys() {
				let path: PathBuf = unit_dir.join(name);
				if path.exists() && fs::read_to_string(&path)?.starts_with(MARKER) {
					fs::remove_file(path)?;
				}
			}
			control(&SystemHost, &["daemon-reload"])
		})();
		outcome?;
		cleanup
	}
}
