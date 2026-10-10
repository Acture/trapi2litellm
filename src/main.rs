mod catalog;
mod deploy;
mod files;
mod process;
mod runtime;
mod settings;
mod sync;

use anyhow::Result;
use clap::{Parser, Subcommand};
use runtime::{PythonRuntime, Runtime, RuntimeError};
use serde_json::{Value, json};
use settings::{Overrides, Settings};
use std::{path::PathBuf, process::ExitCode};

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
			deploy::deploy(
				&LazyRuntime,
				&sync::SystemService {
					settings: &settings,
				},
				&deploy::systemd::SystemdManager::new(&deploy::systemd::SystemHost, &settings),
				&settings,
				&options,
			)
		}
		Commands::Serve { port } => {
			let settings: Settings = Settings::from_env(Overrides {
				port,
				..Overrides::default()
			})?;
			PythonRuntime::discover()?.exec("serve", &settings)
		}
		Commands::SmokeTest => {
			let settings: Settings = Settings::from_env(Overrides::default())?;
			PythonRuntime::discover()?.exec("smoke-test", &settings)
		}
		Commands::Sync {
			bootstrap,
			no_reload,
		} => {
			let settings: Settings = Settings::from_env(Overrides::default())?;
			let result: Result<sync::SyncResult> = sync::synchronize(
				&LazyRuntime,
				&sync::SystemService {
					settings: &settings,
				},
				&settings,
				bootstrap,
				no_reload,
			);
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
