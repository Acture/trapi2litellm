use anyhow::{Context, Result};
use rand::{RngCore, rngs::OsRng};
use std::{
	fs::{self, File, OpenOptions, Permissions},
	io::Write,
	os::unix::fs::PermissionsExt,
	path::Path,
};

pub fn private_directory(path: &Path) -> Result<()> {
	fs::create_dir_all(path)?;
	fs::set_permissions(path, Permissions::from_mode(0o700))?;
	Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
	let parent: &Path = path.parent().context("File has no parent directory")?;
	let mut temporary: tempfile::NamedTempFile = tempfile::Builder::new()
		.prefix(".trapi2litellm-")
		.tempfile_in(parent)?;
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

pub fn local_key(path: &Path) -> Result<String> {
	fs::read_to_string(path)?
		.lines()
		.find_map(|line| {
			line.strip_prefix("LITELLM_MASTER_KEY=")
				.filter(|value| !value.is_empty())
				.map(str::to_owned)
		})
		.context("gateway.env has no local master key")
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
