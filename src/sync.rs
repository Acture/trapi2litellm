use crate::{
	catalog::{self, Config},
	files,
	runtime::Runtime,
	settings::{SERVICE, Settings},
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
	collections::BTreeSet,
	fs,
	process::{Command, ExitStatus, Stdio},
	thread,
	time::{Duration, Instant},
};

pub trait Service {
	fn active(&self) -> Result<bool>;
	fn reload(&self) -> Result<()>;
	fn wait_for_models(&self, expected: &BTreeSet<String>, digest: &str) -> Result<()>;
}

pub fn checked_command(command: &mut Command, timeout: Duration) -> Result<()> {
	let mut child: std::process::Child = command
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.context("Could not start service command")?;
	let started: Instant = Instant::now();
	loop {
		if let Some(status) = child.try_wait()? {
			ensure!(status.success(), "Service command failed ({status})");
			return Ok(());
		}
		if started.elapsed() >= timeout {
			child.kill()?;
			child.wait()?;
			bail!("Service command timed out");
		}
		thread::sleep(Duration::from_millis(50));
	}
}

pub struct SystemService<'a> {
	pub settings: &'a Settings,
}
impl Service for SystemService<'_> {
	fn active(&self) -> Result<bool> {
		let status: ExitStatus = Command::new("systemctl")
			.args(["--user", "is-active", "--quiet", SERVICE])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()?;
		Ok(status.success())
	}
	fn reload(&self) -> Result<()> {
		checked_command(
			Command::new("systemctl").args(["--user", "reload", SERVICE]),
			Duration::from_secs(15),
		)
	}
	fn wait_for_models(&self, expected: &BTreeSet<String>, digest: &str) -> Result<()> {
		let key: String = files::local_key(&self.settings.key_path())?;
		let client: Client = Client::builder()
			.no_proxy()
			.timeout(Duration::from_secs(5))
			.redirect(reqwest::redirect::Policy::none())
			.build()?;
		let started: Instant = Instant::now();
		let mut consecutive: u8 = 0;
		let mut progress: Instant = started;
		eprintln!("INFO waiting for gateway readiness (3 consecutive model/hash matches)");
		while started.elapsed() < Duration::from_secs(90) {
			let matches: bool = (|| -> Result<bool> {
				let response: reqwest::blocking::Response = client
					.get(format!("{}/v1/models", self.settings.local_url()))
					.bearer_auth(&key)
					.header("Connection", "close")
					.send()?
					.error_for_status()?;
				if response
					.headers()
					.get("x-trapi-config-sha256")
					.and_then(|value| value.to_str().ok())
					!= Some(digest)
				{
					return Ok(false);
				}
				let response: ModelResponse = response.json()?;
				Ok(response
					.data
					.into_iter()
					.map(|model| model.id)
					.collect::<BTreeSet<String>>()
					== *expected)
			})()
			.unwrap_or(false);
			consecutive = if matches { consecutive + 1 } else { 0 };
			if consecutive >= 3 {
				eprintln!(
					"INFO gateway ready in {:.2}s",
					started.elapsed().as_secs_f64()
				);
				return Ok(());
			}
			if progress.elapsed() >= Duration::from_secs(10) {
				eprintln!(
					"INFO waiting for gateway readiness ({:.0}s elapsed; {consecutive}/3 consecutive matches)",
					started.elapsed().as_secs_f64()
				);
				progress = Instant::now();
			}
			thread::sleep(Duration::from_secs(1));
		}
		bail!("Gateway did not expose the expected model list within 90 seconds")
	}
}
#[derive(Deserialize)]
struct ModelResponse {
	data: Vec<ModelId>,
}
#[derive(Deserialize)]
struct ModelId {
	id: String,
}

#[derive(Debug, Serialize)]
pub struct SyncResult {
	pub checked_at: String,
	pub source_entries: usize,
	pub configured_models: usize,
	pub config_sha256: String,
	pub changed: bool,
	pub reloaded: bool,
	pub status: &'static str,
}

pub fn now() -> String {
	Utc::now().to_rfc3339()
}
pub fn render(config: &Config) -> Result<Vec<u8>> {
	let mut bytes: Vec<u8> = serde_json::to_vec_pretty(config)?;
	bytes.push(b'\n');
	Ok(bytes)
}

pub fn synchronize(
	runtime: &impl Runtime,
	service: &impl Service,
	settings: &Settings,
	bootstrap: bool,
	no_reload: bool,
) -> Result<SyncResult> {
	let started: Instant = Instant::now();
	files::private_directory(&settings.config_dir)?;
	files::private_directory(&settings.state_dir)?;
	eprintln!("INFO acquiring synchronization lock");
	let _lock: fs::File = files::sync_lock(&settings.state_dir.join("sync.lock"))?;
	if bootstrap {
		files::bootstrap_key(&settings.key_path())?;
	}
	eprintln!("INFO fetching TRAPI catalog");
	let fetch_started: Instant = Instant::now();
	let raw: Value = runtime.catalog(settings)?;
	eprintln!(
		"INFO catalog fetched in {:.2}s",
		fetch_started.elapsed().as_secs_f64()
	);
	let config: Config = catalog::build_config(&raw, settings)?;
	let old_text: Option<String> = match fs::read_to_string(settings.config_path()) {
		Ok(text) => Some(text),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
		Err(error) => return Err(error.into()),
	};
	eprintln!("INFO validating generated configuration");
	let validation_started: Instant = Instant::now();
	let previous_models: Vec<String> = runtime.validate(&config, old_text.as_deref())?;
	eprintln!(
		"INFO configuration validated in {:.2}s",
		validation_started.elapsed().as_secs_f64()
	);
	ensure!(
		config.model_list.len() * 4 >= previous_models.len() * 3,
		"Catalog shrank by over 25%; manual review required, old config kept"
	);
	let rendered: Vec<u8> = render(&config)?;
	let changed: bool = old_text
		.as_ref()
		.is_none_or(|old| old.as_bytes() != rendered);
	let digest: String = catalog::digest(&rendered);
	files::atomic_write(
		&settings.state_dir.join("catalog.json"),
		&serde_json::to_vec_pretty(
			&json!({"fetched_at": now(), "source": settings.catalog_url(), "catalog": raw}),
		)?,
	)?;
	let mut result: SyncResult = SyncResult {
		checked_at: now(),
		source_entries: raw["data"]
			.as_array()
			.context("Missing catalog entries")?
			.len(),
		configured_models: config.model_list.len(),
		config_sha256: digest.clone(),
		changed,
		reloaded: false,
		status: "ok",
	};
	if changed {
		if let Some(old) = &old_text {
			files::atomic_write(
				&settings.state_dir.join("config.previous.yaml"),
				old.as_bytes(),
			)?;
		}
		// Probe service state before publication so an unavailable service manager
		// cannot turn a successful atomic write into an unhandled partial transaction.
		let reload: bool = !no_reload && service.active()?;
		files::atomic_write(&settings.config_path(), &rendered)?;
		if reload {
			eprintln!("INFO reloading gateway");
			let activation: Result<()> = service
				.reload()
				.and_then(|()| service.wait_for_models(&catalog::model_names(&config), &digest));
			if let Err(error) = activation {
				eprintln!("INFO gateway reload failed; restoring previous configuration");
				// Restore the previous bytes even if saving the rejected artifact fails.
				let rejected: Result<()> = files::atomic_write(
					&settings.state_dir.join("config.rejected.yaml"),
					&rendered,
				);
				if let Some(old) = &old_text {
					files::atomic_write(&settings.config_path(), old.as_bytes())
						.context("Failed to restore previous configuration")?;
					service
						.reload()
						.context("Previous configuration restored but rollback reload failed")?;
				} else {
					fs::remove_file(settings.config_path())?;
				}
				rejected?;
				return Err(error);
			}
			result.reloaded = true;
		}
	}
	files::atomic_write(
		&settings.state_dir.join("sync-status.json"),
		&serde_json::to_vec_pretty(&result)?,
	)?;
	eprintln!(
		"INFO synchronization completed in {:.2}s",
		started.elapsed().as_secs_f64()
	);
	Ok(result)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{
		cell::Cell,
		io::{Read, Write},
		net::TcpListener,
	};
	struct MockRuntime {
		catalog: Value,
		previous: usize,
		fail: bool,
	}
	impl Runtime for MockRuntime {
		fn catalog(&self, _: &Settings) -> Result<Value> {
			ensure!(!self.fail, "offline");
			Ok(self.catalog.clone())
		}
		fn validate(&self, _: &Config, _: Option<&str>) -> Result<Vec<String>> {
			Ok(vec!["old".into(); self.previous])
		}
	}
	struct MockService {
		active: bool,
		fail: bool,
		reloads: Cell<usize>,
	}
	impl Service for MockService {
		fn active(&self) -> Result<bool> {
			Ok(self.active)
		}
		fn reload(&self) -> Result<()> {
			self.reloads.set(self.reloads.get() + 1);
			Ok(())
		}
		fn wait_for_models(&self, _: &BTreeSet<String>, _: &str) -> Result<()> {
			ensure!(!self.fail, "not ready");
			Ok(())
		}
	}
	#[test]
	fn publish_noop_fetch_failure_and_rollback() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let mut runtime: MockRuntime = MockRuntime {
			catalog: catalog::catalog(&["a", "b"]),
			previous: 0,
			fail: false,
		};
		let mut service: MockService = MockService {
			active: false,
			fail: false,
			reloads: Cell::new(0),
		};
		assert!(
			synchronize(&runtime, &service, &settings, true, false)
				.unwrap()
				.changed
		);
		assert!(
			!synchronize(&runtime, &service, &settings, false, false)
				.unwrap()
				.changed
		);
		assert_eq!(service.reloads.get(), 0);
		let before: Vec<u8> = fs::read(settings.config_path()).unwrap();
		runtime.fail = true;
		assert!(synchronize(&runtime, &service, &settings, false, false).is_err());
		assert_eq!(fs::read(settings.config_path()).unwrap(), before);
		runtime.fail = false;
		runtime.catalog = catalog::catalog(&["a", "b", "c"]);
		service.active = true;
		service.fail = true;
		assert!(synchronize(&runtime, &service, &settings, false, false).is_err());
		assert_eq!(service.reloads.get(), 2);
		assert_eq!(fs::read(settings.config_path()).unwrap(), before);
		assert!(settings.state_dir.join("config.rejected.yaml").exists());
	}
	#[test]
	fn shrink_boundary_and_first_publish_rollback() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let mut runtime: MockRuntime = MockRuntime {
			catalog: catalog::catalog(&["a", "b"]),
			previous: 4,
			fail: false,
		};
		let service: MockService = MockService {
			active: false,
			fail: false,
			reloads: Cell::new(0),
		};
		assert!(synchronize(&runtime, &service, &settings, false, false).is_err());
		assert!(!settings.config_path().exists());
		runtime.catalog = catalog::catalog(&["a", "b", "c"]);
		assert!(synchronize(&runtime, &service, &settings, false, false).is_ok());
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let service: MockService = MockService {
			active: true,
			fail: true,
			reloads: Cell::new(0),
		};
		assert!(synchronize(&runtime, &service, &settings, false, false).is_err());
		assert!(!settings.config_path().exists());
		assert!(settings.state_dir.join("config.rejected.yaml").exists());
	}

	#[test]
	fn readiness_checks_connection_header_hash_and_consecutive_matches() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let mut settings: Settings = crate::settings::test_settings(root.path());
		files::private_directory(&settings.config_dir).unwrap();
		files::bootstrap_key(&settings.key_path()).unwrap();
		let listener: TcpListener = TcpListener::bind("127.0.0.1:0").unwrap();
		settings.port = listener.local_addr().unwrap().port();
		let server: std::thread::JoinHandle<usize> = std::thread::spawn(move || {
			for hash in ["expected", "old", "expected", "expected", "expected"] {
				let (mut connection, _) = listener.accept().unwrap();
				connection
					.set_read_timeout(Some(Duration::from_secs(5)))
					.unwrap();
				let mut request: Vec<u8> = Vec::new();
				while !request.ends_with(b"\r\n\r\n") {
					let mut byte: [u8; 1] = [0];
					connection.read_exact(&mut byte).unwrap();
					request.push(byte[0]);
				}
				let headers: String = String::from_utf8(request).unwrap().to_ascii_lowercase();
				assert!(headers.contains("connection: close\r\n"));
				assert!(headers.contains("authorization: bearer sk-trapi-"));
				let body: &str = "{\"data\":[{\"id\":\"trapi/a\"}]}";
				write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-trapi-config-sha256: {hash}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
			}
			5
		});
		SystemService {
			settings: &settings,
		}
		.wait_for_models(&BTreeSet::from(["trapi/a".into()]), "expected")
		.unwrap();
		assert_eq!(server.join().unwrap(), 5);
	}
}
