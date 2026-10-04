use anyhow::{Context, Result};
use std::{
	fmt, fs,
	io::Write,
	os::unix::fs::PermissionsExt,
	process::{Child, Command, Output},
	thread,
	time::{Duration, Instant},
};

#[derive(Debug)]
pub struct ProcessTimeout {
	pub child_id: u32,
	pub timeout: Duration,
}

impl fmt::Display for ProcessTimeout {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			formatter,
			"Subprocess {} exceeded its {:.2}s deadline",
			self.child_id,
			self.timeout.as_secs_f64()
		)
	}
}
impl std::error::Error for ProcessTimeout {}

struct ChildGuard {
	child: Child,
}
impl ChildGuard {
	fn terminate(&mut self) -> Result<()> {
		// An exited child may race the deadline; wait still reaps it in that case.
		let kill: std::io::Result<()> = self.child.kill();
		let wait: std::io::Result<std::process::ExitStatus> = self.child.wait();
		if let Err(error) = kill
			&& error.kind() != std::io::ErrorKind::InvalidInput
		{
			return Err(error.into());
		}
		wait.context("Could not reap subprocess")?;
		Ok(())
	}
}
impl Drop for ChildGuard {
	fn drop(&mut self) {
		// Cover early I/O/wait errors as well as normal/deadline exits.
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

fn private_file() -> Result<tempfile::NamedTempFile> {
	let file: tempfile::NamedTempFile = tempfile::Builder::new()
		.prefix("trapi2litellm-process-")
		.tempfile()?;
	file.as_file()
		.set_permissions(fs::Permissions::from_mode(0o600))?;
	Ok(file)
}

pub fn run(command: &mut Command, input: &[u8], timeout: Duration) -> Result<Output> {
	let started: Instant = Instant::now();
	let mut stdin: tempfile::NamedTempFile = private_file()?;
	let stdout: tempfile::NamedTempFile = private_file()?;
	let stderr: tempfile::NamedTempFile = private_file()?;
	stdin.write_all(input)?;
	// Regular files cannot fill a pipe while the parent waits or the child stops
	// reading. Reopen gives the child's stdin an independent offset at byte zero.
	let mut child: ChildGuard = ChildGuard {
		child: command
			.stdin(stdin.reopen()?)
			.stdout(stdout.reopen()?)
			.stderr(stderr.reopen()?)
			.spawn()
			.context("Could not start subprocess")?,
	};
	loop {
		if started.elapsed() >= timeout {
			let child_id: u32 = child.child.id();
			child.terminate()?;
			return Err(ProcessTimeout { child_id, timeout }.into());
		}
		if let Some(status) = child.child.try_wait()? {
			return Ok(Output {
				status,
				stdout: fs::read(stdout.path())?,
				stderr: fs::read(stderr.path())?,
			});
		}
		thread::sleep(Duration::from_millis(20).min(timeout.saturating_sub(started.elapsed())));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hung_child_is_killed_and_reaped() {
		let started: Instant = Instant::now();
		let error: anyhow::Error = run(
			Command::new("/bin/sleep").arg("30"),
			&[],
			Duration::from_millis(100),
		)
		.unwrap_err();
		assert!(started.elapsed() < Duration::from_secs(2));
		let timeout: &ProcessTimeout = error.downcast_ref::<ProcessTimeout>().unwrap();
		let mut status: libc::c_int = 0;
		let pid: libc::pid_t = timeout.child_id.try_into().unwrap();
		// status points to a valid integer; WNOHANG prevents a blocking wait.
		assert_eq!(
			unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
			-1
		);
		assert_eq!(
			std::io::Error::last_os_error().raw_os_error(),
			Some(libc::ECHILD)
		);
	}

	#[test]
	fn stalled_stdin_consumption_does_not_block_deadline() {
		let input: Vec<u8> = vec![b'x'; 4 * 1024 * 1024];
		let started: Instant = Instant::now();
		let error: anyhow::Error = run(
			Command::new("/bin/sleep").arg("30"),
			&input,
			Duration::from_millis(100),
		)
		.unwrap_err();
		assert!(error.downcast_ref::<ProcessTimeout>().is_some());
		assert!(started.elapsed() < Duration::from_secs(2));
	}

	#[test]
	fn output_larger_than_pipe_buffer_is_fully_captured() {
		let input: Vec<u8> = vec![b'x'; 4 * 1024 * 1024];
		let output: Output = run(
			&mut Command::new("/bin/cat"),
			&input,
			Duration::from_secs(5),
		)
		.unwrap();
		assert!(output.status.success());
		assert_eq!(output.stdout, input);
		assert!(output.stderr.is_empty());
		let output: Output = run(
			Command::new("/bin/sh").args(["-c", "cat >&2"]),
			&input,
			Duration::from_secs(5),
		)
		.unwrap();
		assert!(output.status.success());
		assert!(output.stdout.is_empty());
		assert_eq!(output.stderr, input);
	}
}
