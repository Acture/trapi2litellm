mod catalog;
mod deploy;
mod files;
mod process;
mod runtime;
mod settings;
mod sync;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use deploy::{
	Job,
	launchd::{self, LaunchdManager, LaunchdService, SystemLaunchHost},
	systemd::{SystemHost, SystemdManager},
};
use runtime::{PythonRuntime, Runtime, RuntimeError};
use serde_json::{Value, json};
use settings::{Overrides, Settings};
use std::{env, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
	version,
	about = "Managed-identity TRAPI discovery and a loopback LiteLLM gateway"
)]
struct Cli {
	#[command(subcommand)]
	command: Commands,
}
#[derive(Subcommand)]
enum Commands {
	/// Install user units; --start explicitly enables and starts services
	Deploy {
		#[arg(long)]
		config_dir: Option<PathBuf>,
		#[arg(long)]
		state_dir: Option<PathBuf>,
		#[arg(long)]
		port: Option<u16>,
		#[arg(long)]
		entry_point: Option<PathBuf>,
		#[arg(long)]
		start: bool,
		#[arg(long, requires = "start")]
		enable_linger: bool,
		#[arg(long)]
		dry_run: bool,
	},
	/// Run the gateway in the foreground using the installed Python environment
	Serve {
		#[arg(long)]
		port: Option<u16>,
	},
	/// Discover models and atomically update the configuration
	Sync {
		#[arg(long)]
		bootstrap: bool,
		#[arg(long)]
		no_reload: bool,
	},
	/// Run bounded live acceptance checks (billable inference)
	SmokeTest,
}

// Discovery is lazy: install-only and dry-run never inspect or invoke Python.
struct LazyRuntime;
impl Runtime for LazyRuntime {
	fn catalog(&self, settings: &Settings) -> Result<Value> {
		PythonRuntime::discover()?.catalog(settings)
	}
	fn validate(
		&self,
		config: &catalog::Config,
		previous_text: Option<&str>,
	) -> Result<Vec<String>> {
		PythonRuntime::discover()?.validate(config, previous_text)
	}
}

/// The per-user launchd domain: LaunchAgents and logs under the user's Library.
fn launchd_manager(settings: &Settings) -> Result<LaunchdManager<'_, SystemLaunchHost>> {
	let home: PathBuf = env::var_os("HOME").context("HOME is required")?.into();
	// A relative HOME would name a LaunchAgents directory below the working directory, which
	// launchd never scans at login.
	ensure!(home.is_absolute(), "HOME must be an absolute path");
	LaunchdManager::new(
		&SystemLaunchHost,
		settings,
		home.join("Library/LaunchAgents"),
		home.join("Library/Logs/trapi2litellm"),
		launchd::LABEL_PREFIX,
	)
}

fn deploy(settings: &Settings, options: &deploy::DeployOptions) -> Result<()> {
	if cfg!(target_os = "linux") {
		deploy::deploy(
			&LazyRuntime,
			&sync::SystemService { settings },
			&SystemdManager::new(&SystemHost, settings),
			settings,
			options,
		)
	} else if cfg!(target_os = "macos") {
		let manager: LaunchdManager<'_, SystemLaunchHost> = launchd_manager(settings)?;
		deploy::deploy(
			&LazyRuntime,
			&LaunchdService { manager: &manager },
			&manager,
			settings,
			options,
		)
	} else {
		bail!("Deployment supports Linux with systemd --user and macOS with launchd only")
	}
}

fn synchronize(settings: &Settings, bootstrap: bool, no_reload: bool) -> Result<sync::SyncResult> {
	if cfg!(target_os = "macos") {
		let manager: LaunchdManager<'_, SystemLaunchHost> = launchd_manager(settings)?;
		sync::synchronize(
			&LazyRuntime,
			&LaunchdService { manager: &manager },
			settings,
			bootstrap,
			no_reload,
		)
	} else {
		sync::synchronize(
			&LazyRuntime,
			&sync::SystemService { settings },
			settings,
			bootstrap,
			no_reload,
		)
	}
}

fn run(cli: Cli) -> Result<()> {
	match cli.command {
		Commands::Deploy {
			config_dir,
			state_dir,
			port,
			entry_point,
			start,
			enable_linger,
			dry_run,
		} => {
			let settings: Settings = Settings::from_env(Overrides {
				config_dir,
				state_dir,
				port,
			})?;
			let options: deploy::DeployOptions = deploy::DeployOptions {
				entry: deploy::entry_point(entry_point)?,
				start,
				enable_linger,
				dry_run,
			};
			deploy(&settings, &options)
		}
		Commands::Serve { port } => {
			let settings: Settings = Settings::from_env(Overrides {
				port,
				..Overrides::default()
			})?;
			PythonRuntime::discover()?.exec("serve", &settings, &[])
		}
		Commands::SmokeTest => {
			let settings: Settings = Settings::from_env(Overrides::default())?;
			let python: PythonRuntime = PythonRuntime::discover()?;
			if cfg!(target_os = "macos") {
				let manager: LaunchdManager<'_, SystemLaunchHost> = launchd_manager(&settings)?;
				python.exec(
					"smoke-test",
					&settings,
					&[
						("TRAPI2LITELLM_SERVICE_MANAGER", "launchd"),
						("TRAPI2LITELLM_SERVICE_LABEL", manager.label(Job::Gateway)),
					],
				)
			} else {
				python.exec("smoke-test", &settings, &[])
			}
		}
		Commands::Sync {
			bootstrap,
			no_reload,
		} => {
			let settings: Settings = Settings::from_env(Overrides::default())?;
			let result: Result<sync::SyncResult> = synchronize(&settings, bootstrap, no_reload);
			match result {
				Ok(result) => {
					println!("{}", serde_json::to_string(&result)?);
					Ok(())
				}
				Err(error) => {
					let mut result: Value = json!({"status": "error", "checked_at": sync::now(), "error_type": "SyncError"});
					if let Some(runtime) = error.downcast_ref::<RuntimeError>() {
						result["error_type"] = json!(runtime.error_type);
						if let Some(status) = runtime.http_status {
							result["http_status"] = json!(status);
						}
					} else {
						result["message"] = json!(error.to_string());
					}
					if settings.state_dir.exists() {
						files::atomic_write(
							&settings.state_dir.join("sync-error.json"),
							&serde_json::to_vec_pretty(&result)?,
						)?;
					}
					println!("{result}");
					Err(error)
				}
			}
		}
	}
}

fn main() -> ExitCode {
	let cli: Cli = Cli::parse();
	match run(cli) {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			eprintln!("{error}");
			ExitCode::FAILURE
		}
	}
}
