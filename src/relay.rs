//! Gateway mode: the TRAPI catalog as an upstream trapi2litellm gateway relays it.

use crate::{files, settings::Settings};
use anyhow::{Context, Result, ensure};
use reqwest::{
	StatusCode,
	blocking::{Client, Response},
};
use serde_json::Value;
use std::time::Duration;

/// Fetches the raw TRAPI catalog from the upstream gateway configured in `settings`.
pub fn catalog(settings: &Settings) -> Result<Value> {
	fetch(
		&settings.catalog_url(),
		&files::upstream_key(&settings.upstream_key_path())?,
	)
}

/// GETs an upstream gateway's `/catalog` document, `{catalog, fetched_at, source}`, and returns
/// its raw catalog. Errors name the URL and status but never the key.
fn fetch(url: &str, key: &str) -> Result<Value> {
	// No proxy: the upstream is a loopback forward. No redirects: the bearer key must not follow
	// one.
	let client: Client = Client::builder()
		.no_proxy()
		.connect_timeout(Duration::from_secs(10))
		.timeout(Duration::from_secs(60))
		.redirect(reqwest::redirect::Policy::none())
		.build()?;
	let response: Response = client
		.get(url)
		.bearer_auth(key)
		.send()
		.map_err(reqwest::Error::without_url)
		.with_context(|| {
			format!(
				"Could not reach the upstream gateway at {url}; check that it runs and that any SSH forward to it is up"
			)
		})?;
	let status: StatusCode = response.status();
	ensure!(
		status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
		"The upstream gateway rejected the key from upstream.env ({status} from {url})"
	);
	ensure!(
		status == StatusCode::OK,
		"The upstream gateway answered {url} with {status}"
	);
	let mut document: Value = response
		.json()
		.with_context(|| format!("The upstream gateway answered {url} without JSON"))?;
	document
		.as_object_mut()
		.and_then(|document| document.remove("catalog"))
		.filter(Value::is_object)
		.with_context(|| format!("The upstream gateway answered {url} without a catalog object"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;
	use std::{
		fs,
		io::{Read, Write},
		net::TcpListener,
		os::unix::fs::PermissionsExt,
		thread::JoinHandle,
	};

	const KEY: &str = "sk-upstream-fixture";

	/// Answers one request per connection like the upstream `/catalog` route: 401 without the
	/// expected bearer key, otherwise the next scripted status and body, until the script runs
	/// out. Returns each request line.
	fn upstream(responses: Vec<(u16, String)>) -> (String, JoinHandle<Vec<String>>) {
		let listener: TcpListener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url: String = format!("http://{}", listener.local_addr().unwrap());
		let server: JoinHandle<Vec<String>> = std::thread::spawn(move || {
			let mut requests: Vec<String> = Vec::new();
			let mut responses = responses.into_iter().peekable();
			while responses.peek().is_some() {
				let (mut connection, _) = listener.accept().unwrap();
				connection
					.set_read_timeout(Some(Duration::from_secs(5)))
					.unwrap();
				let mut head: Vec<u8> = Vec::new();
				while !head.ends_with(b"\r\n\r\n") {
					let mut byte: [u8; 1] = [0];
					connection.read_exact(&mut byte).unwrap();
					head.push(byte[0]);
				}
				let head: String = String::from_utf8(head).unwrap();
				let authorized: bool = head.lines().any(|line| {
					line.split_once(':').is_some_and(|(name, value)| {
						name.eq_ignore_ascii_case("authorization")
							&& value.trim() == format!("Bearer {KEY}")
					})
				});
				let (status, body): (u16, String) = if authorized {
					responses.next().unwrap()
				} else {
					(
						401,
						"{\"error\":\"Invalid or missing gateway API key\"}".into(),
					)
				};
				write!(
					connection,
					"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nLocation: http://127.0.0.1:9/catalog\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
					body.len()
				)
				.unwrap();
				requests.push(head.lines().next().unwrap().to_owned());
			}
			requests
		});
		(url, server)
	}

	#[test]
	fn relay_fetch_requires_key_status_json_and_catalog() {
		let raw: Value = crate::catalog::catalog(&["gpt-5.2"]);
		let document: Value = json!({
			"catalog": raw,
			"fetched_at": "2026-10-10T00:00:00+00:00",
			"source": "https://trapi.invalid/openai/models?api-version=preview",
		});
		let (url, server) = upstream(vec![
			(500, "{}".into()),
			(302, String::new()),
			(200, "not json".into()),
			(200, "[]".into()),
			(200, "{\"fetched_at\":\"x\"}".into()),
			(200, "{\"catalog\":[]}".into()),
			(200, document.to_string()),
		]);
		let catalog_url: String = format!("{url}/catalog");
		let failure = |key: &str| -> String {
			let message: String = format!("{:#}", fetch(&catalog_url, key).unwrap_err());
			assert!(!message.contains(KEY) && !message.contains("wrong-key"));
			message
		};
		assert!(failure("wrong-key").contains("rejected the key"));
		for expected in [
			"500",
			"302",
			"without JSON",
			"without a catalog object",
			"without a catalog object",
			"without a catalog object",
		] {
			assert!(failure(KEY).contains(expected), "{expected}");
		}
		assert_eq!(fetch(&catalog_url, KEY).unwrap(), raw);
		let requests: Vec<String> = server.join().unwrap();
		assert_eq!(requests.len(), 8);
		assert!(
			requests
				.iter()
				.all(|request| request == "GET /catalog HTTP/1.1")
		);
	}

	#[test]
	fn relay_catalog_reads_the_upstream_key_file() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let mut settings: Settings = crate::settings::test_gateway_settings(root.path());
		let raw: Value = crate::catalog::catalog(&["a"]);
		let (url, server) = upstream(vec![(200, json!({"catalog": raw}).to_string())]);
		settings.mode = crate::settings::Mode::Gateway { upstream_url: url };
		fs::create_dir_all(&settings.config_dir).unwrap();
		let path: std::path::PathBuf = settings.upstream_key_path();
		fs::write(&path, format!("TRAPI2LITELLM_UPSTREAM_KEY={KEY}\n")).unwrap();
		fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
		assert_eq!(catalog(&settings).unwrap(), raw);
		assert_eq!(server.join().unwrap(), ["GET /catalog HTTP/1.1"]);
	}

	#[test]
	fn relay_catalog_fails_before_connecting_without_a_key_file() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let mut settings: Settings = crate::settings::test_gateway_settings(root.path());
		// Port 9 (discard) would refuse a connection; the key check must fail first.
		settings.mode = crate::settings::Mode::Gateway {
			upstream_url: "http://127.0.0.1:9".into(),
		};
		assert!(
			format!("{:#}", catalog(&settings).unwrap_err())
				.contains("requires the upstream key file")
		);
	}
}
