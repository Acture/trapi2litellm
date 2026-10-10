use anyhow::{Context, Result, bail, ensure};
use rand::{RngCore, rngs::OsRng};
use std::{
	fs::{self, File, OpenOptions, Permissions},
	io::{ErrorKind, Read, Write},
	os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
	path::Path,
};

pub const UPSTREAM_KEY: &str = "TRAPI2LITELLM_UPSTREAM_KEY";

/// Quotes `text` as one POSIX shell word.
pub fn shell_quote(text: &str) -> String {
	format!("'{}'", text.replace('\'', "'\"'\"'"))
}

pub fn private_directory(path: &Path) -> Result<()> {
	fs::create_dir_all(path)?;
	fs::set_permissions(path, Permissions::from_mode(0o700))?;
	Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
	atomic_write_in(
		path,
		bytes,
		path.parent().context("File has no parent directory")?,
	)
}

/// Writes through a temporary file in `temporary_dir`, which must share the volume of `path`.
pub fn atomic_write_in(path: &Path, bytes: &[u8], temporary_dir: &Path) -> Result<()> {
	let parent: &Path = path.parent().context("File has no parent directory")?;
	let mut temporary: tempfile::NamedTempFile = tempfile::Builder::new()
		.prefix(".trapi2litellm-")
		.tempfile_in(temporary_dir)?;
	temporary
		.as_file()
		.set_permissions(Permissions::from_mode(0o600))?;
	temporary.write_all(bytes)?;
	temporary.as_file().sync_all()?;
	temporary.persist(path).map_err(|error| error.error)?;
	File::open(parent)?.sync_all()?;
	Ok(())
}

pub fn bootstrap_key(path: &Path) -> Result<()> {
	if !path.exists() {
		let mut random: [u8; 36] = [0; 36];
		OsRng.fill_bytes(&mut random);
		let key: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
		let mut temporary: tempfile::NamedTempFile =
			tempfile::NamedTempFile::new_in(path.parent().context("Key has no parent")?)?;
		temporary
			.as_file()
			.set_permissions(Permissions::from_mode(0o600))?;
		writeln!(temporary, "LITELLM_MASTER_KEY=sk-trapi-{key}")?;
		temporary.as_file().sync_all()?;
		match temporary.persist_noclobber(path) {
			Ok(_) => {
				File::open(path.parent().context("Key has no parent")?)?.sync_all()?;
			}
			Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
			Err(error) => return Err(error.error.into()),
		}
	}
	fs::set_permissions(path, Permissions::from_mode(0o600))?;
	Ok(())
}

/// Value of the first nonempty `name=value` line of a key file.
fn assignment(text: &str, name: &str) -> Option<String> {
	text.lines().find_map(|line| {
		line.strip_prefix(name)?
			.strip_prefix('=')
			.filter(|value| !value.is_empty())
			.map(str::to_owned)
	})
}

pub fn local_key(path: &Path) -> Result<String> {
	assignment(&fs::read_to_string(path)?, "LITELLM_MASTER_KEY")
		.context("gateway.env has no local master key")
}

/// Reads the upstream gateway's master key, which the user provisions and only its owner may
/// access. Checks and reads one open file, so a swapped path cannot slip past the checks. Errors
/// name the file but never its content.
pub fn upstream_key(path: &Path) -> Result<String> {
	let file: std::path::Display<'_> = path.display();
	let quoted: String = shell_quote(&path.to_string_lossy());
	let remedy: String = format!(
		"create it with `install -d -m 700 {} && install -m 600 /dev/null {quoted}` and add the line {UPSTREAM_KEY}=<upstream gateway master key> in an editor",
		shell_quote(
			&path
				.parent()
				.context("Key file has no parent directory")?
				.to_string_lossy()
		)
	);
	// Nonblocking, so that a FIFO in its place fails the regular-file check instead of hanging.
	let mut handle: File = match OpenOptions::new()
		.read(true)
		.custom_flags(libc::O_NONBLOCK)
		.open(path)
	{
		Ok(handle) => handle,
		Err(error) if error.kind() == ErrorKind::NotFound => {
			bail!("Gateway mode requires the upstream key file {file}; {remedy}")
		}
		Err(error) => return Err(error).with_context(|| format!("Cannot open {file}")),
	};
	let metadata: fs::Metadata = handle
		.metadata()
		.with_context(|| format!("Cannot inspect {file}"))?;
	ensure!(
		metadata.is_file(),
		"{file} must be a regular file; {remedy}"
	);
	// getuid has no pointer arguments or failure mode.
	ensure!(
		metadata.uid() == unsafe { libc::getuid() },
		"{file} must belong to the current user; {remedy}"
	);
	ensure!(
		metadata.mode() & 0o077 == 0,
		"{file} must be accessible only by its owner; run chmod 600 {quoted}, and rotate the upstream key if other users could have read it"
	);
	let mut text: String = String::new();
	handle
		.read_to_string(&mut text)
		.with_context(|| format!("Cannot read {file}"))?;
	let key: String = assignment(&text, UPSTREAM_KEY)
		.with_context(|| format!("{file} has no nonempty {UPSTREAM_KEY}; {remedy}"))?;
	// The gateway's Python reader applies the same rule.
	ensure!(
		key.bytes()
			.all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\''),
		"{UPSTREAM_KEY} in {file} must be the bare key: printable ASCII without quotes, spaces or control characters"
	);
	Ok(key)
}

pub struct SyncLock {
	_file: File,
}

pub fn sync_lock(path: &Path) -> Result<SyncLock> {
	let file: File = OpenOptions::new().create(true).append(true).open(path)?;
	file.set_permissions(Permissions::from_mode(0o600))?;
	file.lock()?;
	Ok(SyncLock { _file: file })
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn key_preserved_and_private() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let path: std::path::PathBuf = root.path().join("gateway.env");
		bootstrap_key(&path).unwrap();
		let first: String = local_key(&path).unwrap();
		bootstrap_key(&path).unwrap();
		assert_eq!(first, local_key(&path).unwrap());
		assert_eq!(
			fs::metadata(path).unwrap().permissions().mode() & 0o777,
			0o600
		);
	}

	#[test]
	fn upstream_key_requires_a_private_nonempty_file() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let path: std::path::PathBuf = root.path().join("upstream.env");
		let error = |path: &Path| -> String { format!("{:#}", upstream_key(path).unwrap_err()) };
		let missing: String = error(&path);
		assert!(missing.contains(&path.display().to_string()));
		assert!(missing.contains(&format!(
			"`install -d -m 700 '{}' && install -m 600 /dev/null '{}'`",
			root.path().display(),
			path.display()
		)));
		assert!(!path.exists(), "the key file must never be created");
		for content in ["", "\n", "OTHER=x\n", "TRAPI2LITELLM_UPSTREAM_KEY=\n"] {
			fs::write(&path, content).unwrap();
			fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
			assert!(error(&path).contains("has no nonempty TRAPI2LITELLM_UPSTREAM_KEY"));
		}
		fs::write(
			&path,
			"# relay\nTRAPI2LITELLM_UPSTREAM_KEY=sk-upstream-secret\n",
		)
		.unwrap();
		for mode in [0o640, 0o604, 0o620, 0o644, 0o660] {
			fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
			let message: String = error(&path);
			assert!(message.contains("chmod 600"), "{mode:o}");
			assert!(message.contains("rotate the upstream key"), "{mode:o}");
			assert!(!message.contains("sk-upstream-secret"));
		}
		for mode in [0o600, 0o400] {
			fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
			assert_eq!(upstream_key(&path).unwrap(), "sk-upstream-secret");
		}
		fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
		fs::write(&path, "TRAPI2LITELLM_UPSTREAM_KEY=sk-crlf\r\n").unwrap();
		assert_eq!(upstream_key(&path).unwrap(), "sk-crlf");
		for value in [
			"\"sk-upstream-secret\"",
			"'sk-upstream-secret'",
			"sk-upstream-secret ",
			" sk-upstream-secret",
			"sk-upstream secret",
			"sk-upstream-secret\r",
			"sk-upstream-secret\t",
			"sk-upstream\u{7f}secret",
			"sk-upstream-\u{e9}",
		] {
			fs::write(&path, format!("TRAPI2LITELLM_UPSTREAM_KEY={value}")).unwrap();
			let message: String = error(&path);
			assert!(message.contains("must be the bare key"), "{value:?}");
			assert!(!message.contains("sk-upstream"), "{value:?}: {message}");
		}
		assert!(error(root.path()).contains("must be a regular file"));
	}

	#[test]
	fn upstream_key_checks_the_target_of_a_symbolic_link() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let target: std::path::PathBuf = root.path().join("target.env");
		let link: std::path::PathBuf = root.path().join("upstream.env");
		fs::write(&target, "TRAPI2LITELLM_UPSTREAM_KEY=sk-linked\n").unwrap();
		std::os::unix::fs::symlink(&target, &link).unwrap();
		fs::set_permissions(&target, Permissions::from_mode(0o644)).unwrap();
		assert!(
			format!("{:#}", upstream_key(&link).unwrap_err())
				.contains("must be accessible only by its owner")
		);
		fs::set_permissions(&target, Permissions::from_mode(0o600)).unwrap();
		assert_eq!(upstream_key(&link).unwrap(), "sk-linked");
	}

	#[test]
	fn upstream_key_refuses_a_fifo_without_blocking() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let path: std::path::PathBuf = root.path().join("upstream.env");
		let name: std::ffi::CString =
			std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
		// The path is a valid C string and the mode has no special bits.
		assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
		assert!(
			format!("{:#}", upstream_key(&path).unwrap_err()).contains("must be a regular file")
		);
	}

	#[test]
	fn shell_quote_keeps_one_word() {
		assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
		assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
	}

	#[test]
	fn concurrent_bootstrap_publishes_one_complete_key() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let path: std::path::PathBuf = root.path().join("gateway.env");
		let keys: Vec<String> = std::thread::scope(|scope| {
			let handles: Vec<std::thread::ScopedJoinHandle<'_, String>> = (0..8)
				.map(|_| {
					scope.spawn(|| {
						bootstrap_key(&path).unwrap();
						local_key(&path).unwrap()
					})
				})
				.collect();
			handles
				.into_iter()
				.map(|handle| handle.join().unwrap())
				.collect()
		});
		assert!(keys.iter().all(|key| key == &keys[0]));
		assert!(keys[0].starts_with("sk-trapi-"));
	}
}
