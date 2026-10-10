use super::{DeployOptions, Job, ServiceManager, UnitState, managed_write, marked};
use crate::{
	catalog, files,
	runtime::Runtime,
	settings::Settings,
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
	/// Writes `saved` back through a temporary file in `temporary_dir`, unless the file already
	/// matches, or removes the file that did not exist.
	fn restore(path: &Path, saved: Option<&Self>, temporary_dir: &Path) -> Result<()> {
		if let Some(saved) = saved {
			if Self::read(path)?
				.is_some_and(|current| current.bytes == saved.bytes && current.mode == saved.mode)
			{
				return Ok(());
			}
			files::atomic_write_in(path, &saved.bytes, temporary_dir)?;
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
	sync: UnitState,
	linger: Option<bool>,
}

impl DeploymentSnapshot {
	fn capture(
		manager: &impl ServiceManager,
		settings: &Settings,
		options: &DeployOptions,
		definitions: &BTreeMap<String, String>,
		clients: &BTreeMap<String, String>,
	) -> Result<Self> {
		let definitions_dir: &Path = manager.definitions_dir();
		let paths = definitions
			.keys()
			.map(|name| definitions_dir.join(name))
			.chain(Job::ALL.map(|job| manager.loaded_definition(job)))
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
			gateway: manager.state(Job::Gateway)?,
			sync: manager.state(Job::Sync)?,
			linger: options
				.enable_linger
				.then(|| manager.linger()?.enabled())
				.transpose()?,
		};
		if snapshot.gateway.active {
			ensure!(
				snapshot.config(settings).is_some(),
				"An active gateway requires its existing configuration directory"
			);
			let old_gateway: &SavedFile = snapshot.files[&manager.loaded_definition(Job::Gateway)]
				.as_ref()
				.context("An active gateway requires its existing managed unit")?;
			ensure!(
				manager.same_target(std::str::from_utf8(&old_gateway.bytes)?)?,
				"Stop the gateway before changing its configuration/state directories or port"
			);
		}
		Ok(snapshot)
	}
	fn state(&self, job: Job) -> UnitState {
		match job {
			Job::Gateway => self.gateway,
			Job::Sync => self.sync,
		}
	}
	fn config(&self, settings: &Settings) -> Option<&[u8]> {
		self.files[&settings.config_path()]
			.as_ref()
			.map(|saved| saved.bytes.as_slice())
	}
	fn rollback(
		&self,
		manager: &impl ServiceManager,
		service: &impl Service,
		settings: &Settings,
		activated: bool,
		previous_models: &[String],
	) -> Result<()> {
		let mut errors: Vec<anyhow::Error> = Vec::new();
		if activated {
			if let Err(error) = manager.stop_all() {
				errors.push(error.context("Rollback stop failed"));
			}
			if let Err(error) = manager.disable_all() {
				errors.push(error.context("Rollback disable failed"));
			}
		}
		let mut restored: bool = true;
		let loaded: BTreeSet<PathBuf> = Job::ALL
			.into_iter()
			.map(|job| manager.loaded_definition(job))
			.collect();
		for (path, saved) in &self.files {
			// A loaded definition may live where the manager picks up any file.
			let temporary_dir: &Path = if loaded.contains(path) {
				manager.definitions_dir()
			} else {
				path.parent().expect("Snapshot paths name files")
			};
			if let Err(error) = SavedFile::restore(path, saved.as_ref(), temporary_dir) {
				restored = false;
				errors.push(error.context(format!("Could not restore {}", path.display())));
			}
		}
		if let Err(error) = manager.definitions_changed() {
			restored = false;
			errors.push(error.context("Rollback daemon-reload failed"));
		}
		if activated {
			for job in Job::ALL {
				if let Err(error) = manager.restore_enablement(job, self.state(job).enablement) {
					errors.push(error.context(format!(
						"Could not restore {} enablement",
						manager.definition(job)
					)));
				}
			}
			if restored {
				if self.gateway.active {
					let recovery: Result<()> = manager.restart(Job::Gateway).and_then(|()| {
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
				if self.sync.active
					&& let Err(error) = manager.start(Job::Sync)
				{
					errors.push(error.context("Could not restore timer activation"));
				}
			}
			if self.linger == Some(false)
				&& let Err(error) = manager.linger().and_then(|linger| linger.set(false))
			{
				errors.push(error.context("Could not restore linger state"));
			}
			for job in Job::ALL {
				let verification: Result<()> = manager.state(job).and_then(|actual| {
					ensure!(
						actual == self.state(job),
						"{} activation/enablement was not restored",
						manager.definition(job)
					);
					Ok(())
				});
				if let Err(error) = verification {
					errors.push(error);
				}
			}
			if let Some(expected) = self.linger {
				let verification: Result<()> = manager
					.linger()
					.and_then(|linger| linger.enabled())
					.and_then(|actual| {
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

fn install_definitions(
	manager: &impl ServiceManager,
	settings: &Settings,
	definitions: &BTreeMap<String, String>,
	clients: &BTreeMap<String, String>,
) -> Result<()> {
	let definitions_dir: &Path = manager.definitions_dir();
	fs::create_dir_all(definitions_dir)?;
	files::private_directory(&settings.config_dir)?;
	for (name, content) in definitions {
		managed_write(&definitions_dir.join(name), content, |old| {
			manager.is_managed(old, content)
		})?;
	}
	for (name, content) in clients {
		managed_write(&settings.config_dir.join(name), content, marked)?;
	}
	manager.definitions_changed()
}

pub(super) fn apply_deployment(
	runtime: &impl Runtime,
	service: &impl Service,
	manager: &impl ServiceManager,
	settings: &Settings,
	options: &DeployOptions,
	definitions: &BTreeMap<String, String>,
	clients: &BTreeMap<String, String>,
) -> Result<()> {
	let lock: files::SyncLock = sync::lock(settings)?;
	if !options.start {
		install_definitions(manager, settings, definitions, clients)?;
		println!(
			"Installed {} only. To bootstrap and start: trapi2litellm deploy --start (with the same settings)",
			manager.definitions_kind()
		);
		return Ok(());
	}
	let snapshot: DeploymentSnapshot =
		DeploymentSnapshot::capture(manager, settings, options, definitions, clients)?;
	let outdated: BTreeSet<Job> = manager.outdated(definitions)?;
	let mut phase: &str = "installation";
	let mut activated: bool = false;
	let mut attempted: Option<sync::SyncResult> = None;
	let activation: Result<()> = (|| {
		install_definitions(manager, settings, definitions, clients)?;
		phase = "synchronization";
		attempted = Some(sync::synchronize_locked(
			runtime, service, settings, true, true, &lock,
		)?);
		let result: &sync::SyncResult = attempted.as_ref().expect("Synchronization completed");
		phase = "activation";
		activated = true;
		if options.enable_linger {
			manager.linger()?.set(true)?;
		}
		manager.activate_all()?;
		for job in Job::ALL
			.into_iter()
			.filter(|job| snapshot.state(*job).active)
		{
			if outdated.contains(&job) {
				manager.restart(job)?;
			} else if job == Job::Gateway {
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
			snapshot.rollback(manager, service, settings, activated, previous_models);
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
	#[cfg(target_os = "macos")]
	use crate::deploy::launchd::{LaunchHost, SystemLaunchHost};
	use crate::deploy::{
		Enablement, MARKER, client_files,
		launchd::{
			LaunchdManager, LaunchdService, Status,
			fixture::{Fault, LaunchdFixture},
		},
		systemd::{Host, SYNC_SERVICE, SystemHost, SystemdManager, TIMER, control, render_units},
	};
	use crate::settings::SERVICE;
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
		/// Commands run so far.
		log: RefCell<Vec<String>>,
	}
	impl Host for FixtureHost {
		fn run(&self, program: &str, arguments: &[String]) -> Result<()> {
			self.log
				.borrow_mut()
				.push(format!("{program} {}", arguments.join(" ")));
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

	fn no_signal() -> Result<()> {
		Ok(())
	}

	struct FixtureService<'a, M: ServiceManager> {
		manager: &'a M,
		settings: &'a Settings,
		/// Delivers a successful reload to the gateway.
		signal: &'a dyn Fn() -> Result<()>,
		reload_failure: Cell<bool>,
		readiness_failures: Cell<u8>,
	}
	impl<'a, M: ServiceManager> FixtureService<'a, M> {
		fn new(manager: &'a M, settings: &'a Settings, signal: &'a dyn Fn() -> Result<()>) -> Self {
			Self {
				manager,
				settings,
				signal,
				reload_failure: Cell::new(false),
				readiness_failures: Cell::new(0),
			}
		}
	}
	impl<M: ServiceManager> Service for FixtureService<'_, M> {
		fn active(&self) -> Result<bool> {
			Ok(self.manager.state(Job::Gateway)?.active)
		}
		fn reload(&self) -> Result<()> {
			ensure!(
				!self.reload_failure.replace(false),
				"Fixture reload failure"
			);
			(self.signal)()
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

	fn launchd_manager<'a>(
		host: &'a LaunchdFixture,
		settings: &'a Settings,
		root: &Path,
	) -> Result<LaunchdManager<'a, LaunchdFixture>> {
		LaunchdManager::new(
			host,
			settings,
			root.join("LaunchAgents"),
			root.join("Logs"),
			"io.github.acture.trapi2litellm.test",
		)
	}

	fn deploy_options(root: &Path, enable_linger: bool) -> DeployOptions {
		DeployOptions {
			entry: root.join("bin/trapi2litellm"),
			start: true,
			enable_linger,
			dry_run: false,
		}
	}

	/// What rollback achieves after a deployment fails.
	#[derive(Clone, Copy, Debug, PartialEq)]
	enum Rollback {
		/// Files, job states and readiness are restored.
		Complete,
		/// Files and job states are restored, but a rollback step failed.
		Reported,
		/// Files are restored, but a failed rollback step left jobs that differ from the snapshot.
		Incomplete,
	}

	/// A deployment, over an optional existing one, that fails at `failure`.
	struct FaultCase<'a, M: ServiceManager> {
		root: &'a Path,
		settings: &'a Settings,
		manager: &'a M,
		service: &'a FixtureService<'a, M>,
		enable_linger: bool,
		existing: bool,
		failure: &'static str,
		/// Text appended to job definitions, so that those jobs, if armed, must restart.
		changes: &'a [(Job, &'static str)],
		/// Arms the existing deployment in the host.
		arm: &'a dyn Fn() -> Result<()>,
		/// Makes a host operation fail.
		inject: &'a dyn Fn(&'static str),
		rollback: Rollback,
		/// Checks host state that files and job states do not show, once rollback restored jobs.
		verify: &'a dyn Fn(&DeploymentSnapshot) -> Result<()>,
	}

	/// Checks that the failed deployment restored files, job states and deployment status.
	fn assert_fault_restored<M: ServiceManager>(case: FaultCase<'_, M>) -> Result<()> {
		let FaultCase {
			root,
			settings,
			manager,
			service,
			enable_linger,
			existing,
			failure,
			changes,
			arm,
			inject,
			rollback,
			verify,
		} = case;
		let options: DeployOptions = deploy_options(root, enable_linger);
		let mut runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["old".into()],
			failure: None,
		};
		let mut definitions: BTreeMap<String, String> = manager.render(&options.entry)?;
		let clients: BTreeMap<String, String> = client_files(settings);
		if existing {
			install_definitions(manager, settings, &definitions, &clients)?;
			sync::synchronize(&runtime, service, settings, true, true)?;
			arm()?;
		}
		let _lock: files::SyncLock = sync::lock(settings)?;
		let before: DeploymentSnapshot =
			DeploymentSnapshot::capture(manager, settings, &options, &definitions, &clients)?;
		drop(_lock);
		runtime.models.push("new".into());
		match failure {
			"fetch" | "validate" => runtime.failure = Some(failure),
			"reload" => service.reload_failure.set(true),
			"readiness" | "rejected" => service.readiness_failures.set(1),
			"rollback-readiness" => service.readiness_failures.set(2),
			operation => inject(operation),
		}
		for (job, change) in changes {
			definitions
				.get_mut(manager.definition(*job))
				.context("Missing definition")?
				.push_str(change);
		}
		if failure == "rejected" {
			fs::create_dir(settings.state_dir.join("config.rejected.yaml"))?;
		}
		let outcome: Result<()> = apply_deployment(
			&runtime,
			service,
			manager,
			settings,
			&options,
			&definitions,
			&clients,
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
		if rollback != Rollback::Incomplete {
			ensure!(
				manager.state(Job::Gateway)? == before.gateway
					&& manager.state(Job::Sync)? == before.sync,
				"{existing}/{failure}: activation state was not restored"
			);
			verify(&before).with_context(|| format!("{existing}/{failure}"))?;
		}
		if before.config(settings).is_none() && !matches!(failure, "daemon-reload") {
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
				== if rollback == Rollback::Complete {
					"rolled_back"
				} else {
					"rollback_failed"
				},
			"Incorrect rollback status for {failure}"
		);
		ensure!(
			status["config_sha256"] == json!(before.config(settings).map(catalog::digest)),
			"Incorrect restored hash"
		);
		Ok(())
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
				let host: FixtureHost = FixtureHost::default();
				let manager: SystemdManager<'_, FixtureHost> =
					SystemdManager::new(&host, &settings);
				let existing_linger: bool = existing && matches!(failure, "enable" | "readiness");
				host.linger.set(existing_linger);
				assert_fault_restored(FaultCase {
					root: root.path(),
					settings: &settings,
					manager: &manager,
					service: &FixtureService::new(&manager, &settings, &no_signal),
					enable_linger: true,
					existing,
					failure,
					changes: if failure == "restart" {
						&[(Job::Gateway, "# changed unit\n")]
					} else {
						&[]
					},
					arm: &|| {
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
						Ok(())
					},
					inject: &|operation| host.failure.set(Some(operation)),
					rollback: if failure == "rollback-readiness" {
						Rollback::Reported
					} else {
						Rollback::Complete
					},
					verify: &|_| Ok(()),
				})?;
				ensure!(
					host.linger.get() == existing_linger,
					"Linger was not restored"
				);
			}
		}
		Ok(())
	}

	#[test]
	fn systemd_redeploy_reloads_unchanged_and_restarts_changed_gateway() -> Result<()> {
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: FixtureHost = FixtureHost::default();
		let manager: SystemdManager<'_, FixtureHost> = SystemdManager::new(&host, &settings);
		let reloads: Cell<u8> = Cell::new(0);
		let signal = || -> Result<()> {
			reloads.set(reloads.get() + 1);
			Ok(())
		};
		let service: FixtureService<'_, SystemdManager<'_, FixtureHost>> =
			FixtureService::new(&manager, &settings, &signal);
		let options: DeployOptions = deploy_options(root.path(), false);
		let runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["a".into()],
			failure: None,
		};
		let mut units: BTreeMap<String, String> = manager.render(&options.entry)?;
		let clients: BTreeMap<String, String> = client_files(&settings);
		let deploy = |units: &BTreeMap<String, String>| -> Result<()> {
			apply_deployment(
				&runtime, &service, &manager, &settings, &options, units, &clients,
			)
		};
		let activation: [String; 2] = [
			"systemctl --user daemon-reload".into(),
			format!("systemctl --user enable --now {SERVICE} {TIMER}"),
		];
		deploy(&units)?;
		ensure!(
			host.log.take() == activation && reloads.get() == 0,
			"First deployment did not only enable and start both units"
		);
		deploy(&units)?;
		ensure!(
			host.log.take() == activation && reloads.get() == 1,
			"An unchanged gateway was not reloaded exactly once"
		);
		units
			.get_mut(SERVICE)
			.context("Missing gateway unit")?
			.push_str("# changed unit\n");
		deploy(&units)?;
		ensure!(
			host.log.take()
				== [
					activation[0].clone(),
					activation[1].clone(),
					format!("systemctl --user restart {SERVICE}")
				] && reloads.get() == 1,
			"A changed gateway was not restarted without a reload"
		);
		Ok(())
	}

	/// How an existing launchd deployment differs, when it is snapshotted, from what
	/// `activate_all` left.
	#[derive(Clone, Copy, Debug)]
	enum Existing {
		Armed,
		/// The gateway exited successfully, which KeepAlive does not restart.
		GatewayIdle,
		GatewayBootedOut,
		SyncRemoved,
		SyncRunning,
	}

	/// An existing deployment, if any, the failure, the jobs whose plists change, a launchctl
	/// fault and what rollback achieves.
	type LaunchdCase = (
		Option<Existing>,
		&'static str,
		&'static [Job],
		Option<Fault>,
		Rollback,
	);

	fn arm_launchd(
		host: &LaunchdFixture,
		manager: &LaunchdManager<'_, LaunchdFixture>,
		existing: Existing,
	) -> Result<()> {
		manager.activate_all()?;
		let gateway: &str = manager.label(Job::Gateway);
		let sync: &str = manager.label(Job::Sync);
		match existing {
			Existing::Armed => {}
			Existing::GatewayIdle => host.set_state(gateway, "not running"),
			Existing::GatewayBootedOut => host.unload(gateway),
			Existing::SyncRemoved => {
				host.unload(sync);
				fs::remove_file(manager.loaded_definition(Job::Sync))?;
			}
			Existing::SyncRunning => host.set_state(sync, "running"),
		}
		Ok(())
	}

	#[test]
	fn launchd_deployment_faults_restore_files_activation_and_metadata() -> Result<()> {
		use Existing::{Armed, GatewayBootedOut, GatewayIdle, SyncRemoved, SyncRunning};
		use Rollback::{Complete, Incomplete, Reported};
		const NONE: &[Job] = &[];
		const GATEWAY: &[Job] = &[Job::Gateway];
		const SYNC: &[Job] = &[Job::Sync];
		const BOTH: &[Job] = &[Job::Gateway, Job::Sync];
		let (after, before) = (Fault::after, Fault::before);
		// A rollback-* failure fails readiness so that its launchctl fault hits the rollback.
		let mut cases: Vec<LaunchdCase> = vec![
			(None, "fetch", NONE, None, Complete),
			(None, "validate", NONE, None, Complete),
			(None, "port", NONE, None, Complete),
			(
				None,
				"bootstrap",
				NONE,
				Some(after("bootstrap", 1)),
				Complete,
			),
			(
				None,
				"bootstrap",
				NONE,
				Some(before("bootstrap", 1)),
				Complete,
			),
			(
				None,
				"bootstrap",
				NONE,
				Some(after("bootstrap", 2)),
				Complete,
			),
			(
				None,
				"bootstrap",
				NONE,
				Some(before("bootstrap", 2)),
				Complete,
			),
			(None, "readiness", NONE, None, Complete),
			(None, "rejected", NONE, None, Complete),
			(
				None,
				"rollback-bootout",
				NONE,
				Some(after("bootout", 1)),
				Reported,
			),
			// A sync or gateway job that rollback could not boot out stays loaded without its plist.
			(
				None,
				"rollback-bootout",
				NONE,
				Some(before("bootout", 1)),
				Incomplete,
			),
			(
				None,
				"rollback-bootout",
				NONE,
				Some(before("bootout", 2)),
				Incomplete,
			),
			(Some(Armed), "fetch", NONE, None, Complete),
			(Some(Armed), "validate", NONE, None, Complete),
			(
				Some(Armed),
				"bootstrap",
				GATEWAY,
				Some(after("bootstrap", 1)),
				Complete,
			),
			(
				Some(Armed),
				"bootstrap",
				GATEWAY,
				Some(before("bootstrap", 1)),
				Complete,
			),
			(
				Some(Armed),
				"bootout",
				GATEWAY,
				Some(after("bootout", 1)),
				Complete,
			),
			(
				Some(Armed),
				"bootout",
				GATEWAY,
				Some(before("bootout", 1)),
				Complete,
			),
			(
				Some(Armed),
				"bootstrap",
				SYNC,
				Some(after("bootstrap", 1)),
				Complete,
			),
			(
				Some(Armed),
				"bootout",
				SYNC,
				Some(before("bootout", 1)),
				Complete,
			),
			(Some(Armed), "kill", NONE, Some(after("kill", 1)), Complete),
			(Some(Armed), "kill", NONE, Some(before("kill", 1)), Complete),
			(Some(Armed), "readiness", NONE, None, Complete),
			(Some(Armed), "readiness", BOTH, None, Complete),
			(Some(Armed), "rollback-readiness", NONE, None, Reported),
			(Some(Armed), "rejected", NONE, None, Complete),
			(
				Some(Armed),
				"rollback-bootout",
				NONE,
				Some(after("bootout", 1)),
				Reported,
			),
			// The sync job keeps the plist it was loaded from, which rollback restores.
			(
				Some(Armed),
				"rollback-bootout",
				NONE,
				Some(before("bootout", 1)),
				Reported,
			),
			(
				Some(Armed),
				"rollback-bootout",
				GATEWAY,
				Some(before("bootout", 2)),
				Reported,
			),
			// The sync job keeps running the new plist.
			(
				Some(Armed),
				"rollback-bootout",
				SYNC,
				Some(before("bootout", 2)),
				Incomplete,
			),
			(
				Some(Armed),
				"rollback-bootstrap",
				NONE,
				Some(after("bootstrap", 1)),
				Reported,
			),
			(
				Some(Armed),
				"rollback-bootstrap",
				NONE,
				Some(before("bootstrap", 1)),
				Incomplete,
			),
			(
				Some(Armed),
				"rollback-bootstrap",
				GATEWAY,
				Some(after("bootstrap", 2)),
				Reported,
			),
			(Some(GatewayIdle), "port", NONE, None, Complete),
		];
		for existing in [GatewayIdle, GatewayBootedOut, SyncRemoved, SyncRunning] {
			cases.extend([
				(Some(existing), "fetch", NONE, None, Complete),
				(Some(existing), "readiness", NONE, None, Complete),
				(Some(existing), "readiness", BOTH, None, Complete),
			]);
		}
		for (existing, failure, changed, fault, rollback) in cases {
			let root: tempfile::TempDir = tempfile::tempdir()?;
			let settings: Settings = crate::settings::test_settings(root.path());
			let host: LaunchdFixture = LaunchdFixture::default();
			let manager: LaunchdManager<'_, LaunchdFixture> =
				launchd_manager(&host, &settings, root.path())?;
			let signal = || LaunchdService { manager: &manager }.reload();
			let service: FixtureService<'_, LaunchdManager<'_, LaunchdFixture>> =
				FixtureService::new(&manager, &settings, &signal);
			let changes: Vec<(Job, &str)> = changed
				.iter()
				.map(|job| (*job, "<!-- changed -->\n"))
				.collect();
			assert_fault_restored(FaultCase {
				root: root.path(),
				settings: &settings,
				manager: &manager,
				service: &service,
				enable_linger: false,
				existing: existing.is_some(),
				failure,
				changes: &changes,
				arm: &|| {
					existing
						.context("No existing deployment")
						.and_then(|existing| arm_launchd(&host, &manager, existing))
				},
				inject: &|failure| {
					host.port_busy.set(failure == "port");
					if failure.starts_with("rollback-") {
						service.readiness_failures.set(1);
					}
					host.fault.set(fault);
				},
				rollback,
				verify: &|before: &DeploymentSnapshot| {
					for job in Job::ALL {
						let saved: Option<&[u8]> = before.files[&manager.loaded_definition(job)]
							.as_ref()
							.map(|file| file.bytes.as_slice());
						ensure!(
							match host.loaded_plist(manager.label(job)) {
								Some(plist) => Some(plist.as_bytes()) == saved,
								None => !before.state(job).active,
							},
							"launchd does not run the restored {}",
							manager.definition(job)
						);
					}
					Ok(())
				},
			})
			.with_context(|| format!("{existing:?} {changed:?} {failure} {fault:?}"))?;
		}
		Ok(())
	}

	#[test]
	fn launchd_redeploy_reloads_unchanged_and_restarts_changed_or_idle_jobs() -> Result<()> {
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> =
			launchd_manager(&host, &settings, root.path())?;
		let signal = || LaunchdService { manager: &manager }.reload();
		let service: FixtureService<'_, LaunchdManager<'_, LaunchdFixture>> =
			FixtureService::new(&manager, &settings, &signal);
		let options: DeployOptions = deploy_options(root.path(), false);
		let runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["a".into()],
			failure: None,
		};
		let mut plists: BTreeMap<String, String> = manager.render(&options.entry)?;
		let clients: BTreeMap<String, String> = client_files(&settings);
		let deploy = |plists: &BTreeMap<String, String>| -> Result<()> {
			apply_deployment(
				&runtime, &service, &manager, &settings, &options, plists, &clients,
			)
		};
		let change = |plists: &mut BTreeMap<String, String>, job: Job| -> Result<()> {
			plists
				.get_mut(manager.definition(job))
				.context("Missing plist")?
				.push_str("<!-- changed -->\n");
			Ok(())
		};
		let gateway: &str = manager.label(Job::Gateway);
		let sync: &str = manager.label(Job::Sync);
		let bootstrap = |job: Job| {
			format!(
				"bootstrap gui/501 {}",
				manager.loaded_definition(job).display()
			)
		};
		let bootout = |label: &str| format!("bootout --wait gui/501/{label}");
		let reload: String = format!("kill SIGHUP gui/501/{gateway}");
		deploy(&plists)?;
		ensure!(
			host.take_log() == [bootstrap(Job::Gateway), bootstrap(Job::Sync)],
			"First deployment did not bootstrap both jobs"
		);
		deploy(&plists)?;
		ensure!(
			host.take_log() == [reload.clone()],
			"An unchanged gateway was not reloaded in place"
		);
		change(&mut plists, Job::Gateway)?;
		deploy(&plists)?;
		ensure!(
			host.take_log() == [bootout(gateway), bootstrap(Job::Gateway)],
			"A changed gateway plist was not bootstrapped again"
		);
		change(&mut plists, Job::Sync)?;
		deploy(&plists)?;
		ensure!(
			host.take_log() == [reload.clone(), bootout(sync), bootstrap(Job::Sync)],
			"A changed sync plist was not bootstrapped again"
		);
		// gunicorn exits 0 on SIGTERM, which KeepAlive does not restart.
		host.set_state(gateway, "not running");
		deploy(&plists)?;
		ensure!(
			host.take_log() == [format!("kickstart gui/501/{gateway}")],
			"An idle gateway was not kickstarted"
		);
		host.set_state(gateway, "not running");
		change(&mut plists, Job::Gateway)?;
		deploy(&plists)?;
		ensure!(
			host.take_log() == [bootout(gateway), bootstrap(Job::Gateway)],
			"An idle gateway did not load its changed plist"
		);
		for job in Job::ALL {
			ensure!(
				host.loaded_plist(manager.label(job)).as_ref()
					== Some(&plists[manager.definition(job)]),
				"launchd did not load the changed {}",
				manager.definition(job)
			);
		}
		service.readiness_failures.set(1);
		ensure!(
			deploy(&plists).is_err(),
			"Readiness failure was not injected"
		);
		ensure!(
			host.take_log()
				== [
					reload,
					bootout(sync),
					bootout(gateway),
					bootstrap(Job::Gateway),
					bootstrap(Job::Sync),
				],
			"Rollback did not unload and bootstrap the previous jobs"
		);
		let armed: UnitState = UnitState {
			active: true,
			enablement: Enablement::Persistent,
		};
		ensure!(
			manager.status(Job::Sync)? == Status::Idle
				&& manager.state(Job::Sync)? == armed
				&& manager.state(Job::Gateway)? == armed,
			"Rollback did not keep the idle sync schedule and the gateway armed"
		);
		Ok(())
	}

	#[test]
	fn launchd_install_only_stages_plists_without_publishing() -> Result<()> {
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> =
			launchd_manager(&host, &settings, root.path())?;
		let options: DeployOptions = DeployOptions {
			start: false,
			..deploy_options(root.path(), false)
		};
		let plists: BTreeMap<String, String> = manager.render(&options.entry)?;
		apply_deployment(
			&FixtureRuntime {
				models: vec!["a".into()],
				failure: None,
			},
			&FixtureService::new(&manager, &settings, &no_signal),
			&manager,
			&settings,
			&options,
			&plists,
			&client_files(&settings),
		)?;
		for (name, content) in &plists {
			ensure!(
				fs::read_to_string(manager.definitions_dir().join(name))? == *content,
				"{name} was not staged"
			);
		}
		ensure!(
			host.take_log().is_empty()
				&& !root.path().join("LaunchAgents").exists()
				&& !root.path().join("Logs").exists(),
			"Install-only deployment touched launchd"
		);
		Ok(())
	}

	#[test]
	fn launchd_busy_port_fails_before_bootstrap() -> Result<()> {
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> =
			launchd_manager(&host, &settings, root.path())?;
		let options: DeployOptions = deploy_options(root.path(), false);
		host.port_busy.set(true);
		let remedy: String = format!("lsof -nP -iTCP:{} -sTCP:LISTEN", settings.port);
		ensure!(
			manager
				.check_available(true)
				.is_err_and(|error| error.to_string().contains(&remedy)),
			"A busy port did not fail before synchronization"
		);
		// The guard in activation covers a port taken after that check.
		let outcome: Result<()> = apply_deployment(
			&FixtureRuntime {
				models: vec!["a".into()],
				failure: None,
			},
			&FixtureService::new(&manager, &settings, &no_signal),
			&manager,
			&settings,
			&options,
			&manager.render(&options.entry)?,
			&client_files(&settings),
		);
		ensure!(
			outcome.is_err_and(|error| error.to_string().contains(&remedy)),
			"A busy port did not name how to find its holder"
		);
		ensure!(
			host.take_log().is_empty() && !root.path().join("LaunchAgents").exists(),
			"Deployment changed launchd despite a busy port"
		);
		Ok(())
	}

	/// Deploys twice and checks that the key survives and the deployment is ready.
	fn assert_redeploy_keeps_key<M: ServiceManager>(
		root: &Path,
		settings: &Settings,
		manager: &M,
		service: &FixtureService<'_, M>,
	) -> Result<()> {
		let options: DeployOptions = deploy_options(root, false);
		let runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["ready".into()],
			failure: None,
		};
		let definitions: BTreeMap<String, String> = manager.render(&options.entry)?;
		let clients: BTreeMap<String, String> = client_files(settings);
		apply_deployment(
			&runtime,
			service,
			manager,
			settings,
			&options,
			&definitions,
			&clients,
		)?;
		let key: String = files::local_key(&settings.key_path())?;
		apply_deployment(
			&runtime,
			service,
			manager,
			settings,
			&options,
			&definitions,
			&clients,
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
	fn deployment_success_preserves_key_and_marks_ready() -> Result<()> {
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: FixtureHost = FixtureHost::default();
		let manager: SystemdManager<'_, FixtureHost> = SystemdManager::new(&host, &settings);
		assert_redeploy_keeps_key(
			root.path(),
			&settings,
			&manager,
			&FixtureService::new(&manager, &settings, &no_signal),
		)?;
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> =
			launchd_manager(&host, &settings, root.path())?;
		let signal = || LaunchdService { manager: &manager }.reload();
		assert_redeploy_keeps_key(
			root.path(),
			&settings,
			&manager,
			&FixtureService::new(&manager, &settings, &signal),
		)
	}

	/// Deploys, then checks that moving the running gateway's configuration, state or port is
	/// rejected before anything is published.
	fn assert_reconfiguration_rejected<M: ServiceManager>(
		root: &Path,
		settings: &Settings,
		manager: &M,
		service: &FixtureService<'_, M>,
		redeploy: &dyn Fn(&Settings) -> Result<()>,
	) -> Result<()> {
		let options: DeployOptions = deploy_options(root, false);
		let definitions: BTreeMap<String, String> = manager.render(&options.entry)?;
		apply_deployment(
			&FixtureRuntime {
				models: vec!["old".into()],
				failure: None,
			},
			service,
			manager,
			settings,
			&options,
			&definitions,
			&client_files(settings),
		)?;
		let previous: Vec<u8> = fs::read(settings.config_path())?;
		for change in ["config", "state", "port"] {
			let mut changed: Settings = settings.clone();
			match change {
				"port" => changed.port += 1,
				"state" => changed.state_dir = root.join("different-state"),
				_ => changed.config_dir = root.join("different-config"),
			}
			ensure!(
				redeploy(&changed).is_err(),
				"An active gateway was reconfigured without a valid rollback target"
			);
			ensure!(
				fs::read(settings.config_path())? == previous
					&& manager.state(Job::Gateway)?.active,
				"Reconfiguration changed the existing gateway"
			);
			for (name, content) in &definitions {
				ensure!(
					fs::read_to_string(manager.definitions_dir().join(name))? == *content,
					"Reconfiguration replaced an existing unit"
				);
			}
		}
		Ok(())
	}

	#[test]
	fn active_reconfiguration_is_rejected_before_publication() -> Result<()> {
		let runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["old".into()],
			failure: None,
		};
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: FixtureHost = FixtureHost::default();
		let manager: SystemdManager<'_, FixtureHost> = SystemdManager::new(&host, &settings);
		let service: FixtureService<'_, SystemdManager<'_, FixtureHost>> =
			FixtureService::new(&manager, &settings, &no_signal);
		assert_reconfiguration_rejected(
			root.path(),
			&settings,
			&manager,
			&service,
			&|changed: &Settings| -> Result<()> {
				let options: DeployOptions = deploy_options(root.path(), false);
				apply_deployment(
					&runtime,
					&service,
					&SystemdManager::new(&host, changed),
					changed,
					&options,
					&render_units(&options.entry, changed)?,
					&client_files(changed),
				)
			},
		)?;
		let root: tempfile::TempDir = tempfile::tempdir()?;
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> =
			launchd_manager(&host, &settings, root.path())?;
		let signal = || LaunchdService { manager: &manager }.reload();
		let service: FixtureService<'_, LaunchdManager<'_, LaunchdFixture>> =
			FixtureService::new(&manager, &settings, &signal);
		assert_reconfiguration_rejected(
			root.path(),
			&settings,
			&manager,
			&service,
			&|changed: &Settings| -> Result<()> {
				let options: DeployOptions = deploy_options(root.path(), false);
				let moved: LaunchdManager<'_, LaunchdFixture> =
					launchd_manager(&host, changed, root.path())?;
				apply_deployment(
					&runtime,
					&service,
					&moved,
					changed,
					&options,
					&moved.render(&options.entry)?,
					&client_files(changed),
				)
			},
		)
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

	struct InterruptedReadiness<S: Service> {
		inner: S,
		interrupt: Cell<bool>,
	}
	impl<S: Service> Service for InterruptedReadiness<S> {
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

	/// Checks the running gateway's view of the rolled-back deployment.
	fn assert_gateway_reports_rollback(settings: &Settings) -> Result<()> {
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
		let service: InterruptedReadiness<sync::SystemService<'_>> = InterruptedReadiness {
			inner: sync::SystemService {
				settings: &settings,
			},
			interrupt: Cell::new(false),
		};
		let mut runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["offline".into()],
			failure: None,
		};
		let manager: SystemdManager<'_, SystemHost> = SystemdManager::new(&SystemHost, &settings);
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
				&SystemdManager::new(&InterruptedEnable, &settings),
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
			install_definitions(&manager, &settings, &units, &clients)?;
			control(&SystemHost, &["enable", "--runtime", SERVICE])?;
			control(&SystemHost, &["start", SERVICE, TIMER])?;
			let before: DeploymentSnapshot =
				DeploymentSnapshot::capture(&manager, &settings, &options, &units, &clients)?;
			let previous: Vec<u8> = fs::read(settings.config_path())?;
			let models: BTreeSet<String> = BTreeSet::from(["trapi/offline".into()]);
			service.wait_for_models(&models, &catalog::digest(&previous))?;
			runtime.models.push("new".into());
			service.interrupt.set(true);
			let failure: Result<()> = apply_deployment(
				&runtime, &service, &manager, &settings, &options, &units, &clients,
			);
			ensure!(failure.is_err(), "Redeployment failure was not injected");
			ensure!(
				fs::read(settings.config_path())? == previous,
				"Previous configuration was not restored"
			);
			ensure!(
				SystemHost.unit_state(SERVICE)? == before.gateway
					&& SystemHost.unit_state(TIMER)? == before.sync,
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
			assert_gateway_reports_rollback(&settings)
		})();
		let cleanup: Result<()> = (|| {
			let unit_dir: &Path = manager.definitions_dir();
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

	/// The real launchd domain, failing once right after it bootstraps the gateway.
	#[cfg(target_os = "macos")]
	struct InterruptedBootstrap {
		fired: Cell<bool>,
	}
	#[cfg(target_os = "macos")]
	impl LaunchHost for InterruptedBootstrap {
		fn run(&self, arguments: &[&str], timeout: Duration) -> Result<()> {
			SystemLaunchHost.run(arguments, timeout)?;
			ensure!(
				arguments[0] != "bootstrap" || self.fired.replace(true),
				"Injected failure after bootstrapping the gateway"
			);
			Ok(())
		}
		fn print(&self, target: &str) -> Result<(i32, String)> {
			SystemLaunchHost.print(target)
		}
		fn print_disabled(&self, domain: &str) -> Result<String> {
			SystemLaunchHost.print_disabled(domain)
		}
		fn uid(&self) -> u32 {
			SystemLaunchHost.uid()
		}
		fn port_free(&self, port: u16) -> Result<bool> {
			SystemLaunchHost.port_free(port)
		}
	}

	/// Boots out and removes the acceptance jobs even when the test panics.
	#[cfg(target_os = "macos")]
	struct Bootout<'a>(&'a LaunchdManager<'a, SystemLaunchHost>);
	#[cfg(target_os = "macos")]
	impl Drop for Bootout<'_> {
		fn drop(&mut self) {
			// Best effort while unwinding; the test checks its own teardown.
			let _ = self.0.stop_all();
			let _ = self.0.disable_all();
		}
	}

	/// Loads PID-suffixed `io.github.acture.trapi2litellm.test-*` labels from a temporary
	/// LaunchAgents directory. A run killed before its cleanup leaves its jobs loaded until logout:
	/// find them with `launchctl print gui/$(id -u) | grep trapi2litellm.test-` and remove each with
	/// `launchctl bootout gui/$(id -u)/<label>`.
	#[cfg(target_os = "macos")]
	#[test]
	#[ignore = "requires a persistent installation and a logged-in macOS GUI session"]
	fn launchd_activation_rollback() -> Result<()> {
		let entry: PathBuf = env::var_os("TRAPI2LITELLM_ACCEPTANCE_ENTRY")
			.context("Set TRAPI2LITELLM_ACCEPTANCE_ENTRY to a persistent trapi2litellm command")?
			.into();
		let folder: tempfile::TempDir = tempfile::tempdir()?;
		let root: PathBuf = folder.path().canonicalize()?;
		let home: PathBuf = env::var_os("HOME").context("HOME is required")?.into();
		let agents: PathBuf = root.join("LaunchAgents");
		ensure!(
			!agents.starts_with(home.join("Library")),
			"Acceptance must never use the real LaunchAgents directory"
		);
		let port: u16 = std::net::TcpListener::bind("127.0.0.1:0")?
			.local_addr()?
			.port();
		let settings: Settings = Settings::from_env(crate::settings::Overrides {
			config_dir: Some(root.join("config")),
			state_dir: Some(root.join("state")),
			port: Some(port),
		})?;
		let prefix: String = format!("io.github.acture.trapi2litellm.test-{}", std::process::id());
		let manager: LaunchdManager<'_, SystemLaunchHost> = LaunchdManager::new(
			&SystemLaunchHost,
			&settings,
			agents.clone(),
			root.join("Logs"),
			&prefix,
		)?;
		ensure!(
			manager.state(Job::Gateway)? == UnitState::default()
				&& manager.state(Job::Sync)? == UnitState::default(),
			"Acceptance labels are already in use"
		);
		// Armed only now, so that a failed precondition never boots out jobs this run did not load.
		let _bootout: Bootout<'_> = Bootout(&manager);
		let options: DeployOptions = DeployOptions {
			entry,
			start: true,
			enable_linger: false,
			dry_run: false,
		};
		let service: InterruptedReadiness<LaunchdService<'_, SystemLaunchHost>> =
			InterruptedReadiness {
				inner: LaunchdService { manager: &manager },
				interrupt: Cell::new(false),
			};
		let mut runtime: FixtureRuntime = FixtureRuntime {
			models: vec!["offline".into()],
			failure: None,
		};
		let mut plists: BTreeMap<String, String> = manager.render(&options.entry)?;
		// Keep the real calendar job, but never let it authenticate to Azure.
		let sync_plist: String = manager.definition(Job::Sync).into();
		let quiet: String = manager
			.render(Path::new("/usr/bin/true"))?
			.remove(&sync_plist)
			.context("Missing sync plist")?;
		plists.insert(sync_plist, quiet);
		let clients: BTreeMap<String, String> = client_files(&settings);
		manager.preflight(&plists)?;
		let outcome: Result<()> = (|| {
			let interrupted: InterruptedBootstrap = InterruptedBootstrap {
				fired: Cell::new(false),
			};
			let failure: Result<()> = apply_deployment(
				&runtime,
				&service,
				&LaunchdManager::new(
					&interrupted,
					&settings,
					agents.clone(),
					root.join("Logs"),
					&prefix,
				)?,
				&settings,
				&options,
				&plists,
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
				manager.state(Job::Gateway)? == UnitState::default()
					&& manager.state(Job::Sync)? == UnitState::default(),
				"First deployment left a loaded or installed job"
			);

			sync::synchronize(&runtime, &service, &settings, true, true)?;
			install_definitions(&manager, &settings, &plists, &clients)?;
			manager.activate_all()?;
			let previous: Vec<u8> = fs::read(settings.config_path())?;
			let models: BTreeSet<String> = BTreeSet::from(["trapi/offline".into()]);
			// A snapshot fails closed while launchd is still spawning the gateway.
			service.wait_for_models(&models, &catalog::digest(&previous))?;
			// Ties the print parser to this release's output for both settled states.
			ensure!(
				manager.status(Job::Gateway)? == Status::Running
					&& manager.status(Job::Sync)? == Status::Idle,
				"launchd did not report a running gateway and an idle schedule"
			);
			let before: DeploymentSnapshot =
				DeploymentSnapshot::capture(&manager, &settings, &options, &plists, &clients)?;
			let armed: UnitState = UnitState {
				active: true,
				enablement: Enablement::Persistent,
			};
			ensure!(
				before.gateway == armed && before.sync == armed,
				"Activation did not arm both jobs"
			);
			runtime.models.push("new".into());
			service.interrupt.set(true);
			let failure: Result<()> = apply_deployment(
				&runtime, &service, &manager, &settings, &options, &plists, &clients,
			);
			ensure!(failure.is_err(), "Redeployment failure was not injected");
			ensure!(
				fs::read(settings.config_path())? == previous,
				"Previous configuration was not restored"
			);
			ensure!(
				manager.state(Job::Gateway)? == before.gateway
					&& manager.state(Job::Sync)? == before.sync,
				"Previous job activation/enablement was not restored"
			);
			service.wait_for_models(&models, &catalog::digest(&previous))?;
			let status: Value = serde_json::from_slice(&fs::read(
				settings.state_dir.join("deployment-status.json"),
			)?)?;
			ensure!(
				status["status"] == "rolled_back",
				"Rollback was not recorded"
			);
			assert_gateway_reports_rollback(&settings)
		})();
		let cleanup: Result<()> = manager.stop_all().and_then(|()| manager.disable_all());
		outcome?;
		cleanup?;
		ensure!(
			manager.state(Job::Gateway)? == UnitState::default()
				&& manager.state(Job::Sync)? == UnitState::default(),
			"Acceptance jobs were not removed"
		);
		Ok(())
	}
}
