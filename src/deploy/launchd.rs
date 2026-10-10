use super::{
	Enablement, Job, Linger, ServiceManager, UnitState, gateway_environment, job_environment,
};
use crate::{
	files, process,
	settings::Settings,
	sync::{self, Service},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
	collections::{BTreeMap, BTreeSet},
	fmt::Write,
	fs,
	io::ErrorKind,
	net::{SocketAddr, TcpListener, TcpStream},
	os::unix::fs::{MetadataExt, PermissionsExt},
	path::{Path, PathBuf},
	process::Command,
	time::Duration,
};

pub const LABEL_PREFIX: &str = "io.github.acture.trapi2litellm";
const MARKER: &str = "<!-- Managed by trapi2litellm -->";
const DECLARATION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;
const DOCTYPE: &str = r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#;
/// launchd starts agents with only the system directories on PATH.
const SEARCH_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// `launchctl print` exit status for a service its domain has not loaded.
const NOT_LOADED: i32 = 113;
/// Outlasts the gateway's ExitTimeOut, after which launchd kills it and `bootout --wait` returns.
const BOOTOUT_TIMEOUT: Duration = Duration::from_secs(960);
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(60);

/// The property-list subset the renderer writes and the parser reads back.
#[derive(Clone, Debug, PartialEq)]
enum Plist {
	String(String),
	Integer(i64),
	Bool(bool),
	Array(Vec<Plist>),
	Dict(BTreeMap<String, Plist>),
}

impl Plist {
	fn string(value: impl Into<String>) -> Self {
		Self::String(value.into())
	}
	fn strings<const N: usize>(values: [&str; N]) -> Self {
		Self::Array(values.map(Self::string).into())
	}
	fn dict<const N: usize>(entries: [(&str, Self); N]) -> Self {
		Self::Dict(
			entries
				.into_iter()
				.map(|(key, value)| (key.to_owned(), value))
				.collect(),
		)
	}
	fn get(&self, key: &str) -> Option<&Self> {
		match self {
			Self::Dict(entries) => entries.get(key),
			_ => None,
		}
	}
	fn as_str(&self) -> Option<&str> {
		match self {
			Self::String(text) => Some(text),
			_ => None,
		}
	}
	fn as_array(&self) -> Option<&[Self]> {
		match self {
			Self::Array(items) => Some(items),
			_ => None,
		}
	}
}

fn escape(text: &str) -> Result<String> {
	ensure!(
		!text
			.chars()
			.any(|character| character.is_control() || matches!(character, '\u{FFFE}' | '\u{FFFF}')),
		"Control characters and XML noncharacters are not supported in service settings"
	);
	Ok(text
		.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;"))
}

fn write_value(value: &Plist, depth: usize, output: &mut String) -> Result<()> {
	let indent: String = "\t".repeat(depth);
	match value {
		Plist::String(text) => writeln!(output, "{indent}<string>{}</string>", escape(text)?)?,
		Plist::Integer(number) => writeln!(output, "{indent}<integer>{number}</integer>")?,
		Plist::Bool(flag) => writeln!(output, "{indent}<{flag}/>")?,
		Plist::Array(items) => {
			writeln!(output, "{indent}<array>")?;
			for item in items {
				write_value(item, depth + 1, output)?;
			}
			writeln!(output, "{indent}</array>")?;
		}
		Plist::Dict(entries) => {
			writeln!(output, "{indent}<dict>")?;
			for (key, item) in entries {
				writeln!(output, "{indent}\t<key>{}</key>", escape(key)?)?;
				write_value(item, depth + 1, output)?;
			}
			writeln!(output, "{indent}</dict>")?;
		}
	}
	Ok(())
}

/// XML property list with the ownership marker on its second line.
fn serialize(value: &Plist) -> Result<String> {
	let mut output: String =
		format!("{DECLARATION}\n{MARKER}\n{DOCTYPE}\n<plist version=\"1.0\">\n");
	write_value(value, 0, &mut output)?;
	output.push_str("</plist>\n");
	Ok(output)
}

fn is_managed(old: &str) -> bool {
	old.split('\n').nth(1) == Some(MARKER)
}

fn unescape(raw: &str) -> Result<String> {
	let mut output: String = String::with_capacity(raw.len());
	let mut rest: &str = raw;
	while let Some(start) = rest.find('&') {
		output.push_str(&rest[..start]);
		let end: usize = start
			+ rest[start..]
				.find(';')
				.context("Unterminated property list entity")?;
		let entity: &str = &rest[start + 1..end];
		output.push(match entity {
			"amp" => '&',
			"lt" => '<',
			"gt" => '>',
			"quot" => '"',
			"apos" => '\'',
			_ => {
				let code: u32 = if let Some(hex) = entity.strip_prefix("#x") {
					u32::from_str_radix(hex, 16)
				} else if let Some(decimal) = entity.strip_prefix('#') {
					decimal.parse()
				} else {
					bail!("Unsupported property list entity &{entity};")
				}
				.context("Malformed property list character reference")?;
				char::from_u32(code).context("Invalid property list character reference")?
			}
		});
		rest = &rest[end + 1..];
	}
	output.push_str(rest);
	Ok(output)
}

struct Reader<'a> {
	rest: &'a str,
}

impl Reader<'_> {
	fn eat(&mut self, token: &str) -> bool {
		self.rest
			.trim_start()
			.strip_prefix(token)
			.map(|rest| self.rest = rest)
			.is_some()
	}
	fn expect(&mut self, token: &str) -> Result<()> {
		ensure!(self.eat(token), "Malformed property list: expected {token}");
		Ok(())
	}
	/// Skips whitespace, the XML declaration, comments and the document type.
	fn skip_markup(&mut self) -> Result<()> {
		loop {
			let rest: &str = self.rest.trim_start();
			let close: &str = if rest.starts_with("<?") {
				"?>"
			} else if rest.starts_with("<!--") {
				"-->"
			} else if rest.starts_with("<!DOCTYPE") {
				">"
			} else {
				self.rest = rest;
				return Ok(());
			};
			let end: usize = rest
				.find(close)
				.context("Malformed property list: unterminated markup")?;
			self.rest = &rest[end + close.len()..];
		}
	}
	fn text(&mut self, close: &str) -> Result<String> {
		let end: usize = self
			.rest
			.find('<')
			.context("Malformed property list: unterminated text")?;
		let raw: &str = &self.rest[..end];
		self.rest = self.rest[end..]
			.strip_prefix(close)
			.with_context(|| format!("Malformed property list: expected {close}"))?;
		unescape(raw)
	}
	fn value(&mut self) -> Result<Plist> {
		Ok(if self.eat("<string>") {
			Plist::String(self.text("</string>")?)
		} else if self.eat("<string/>") {
			Plist::String(String::new())
		} else if self.eat("<integer>") {
			Plist::Integer(
				self.text("</integer>")?
					.trim()
					.parse()
					.context("Malformed property list integer")?,
			)
		} else if self.eat("<true/>") {
			Plist::Bool(true)
		} else if self.eat("<false/>") {
			Plist::Bool(false)
		} else if self.eat("<array/>") {
			Plist::Array(Vec::new())
		} else if self.eat("<array>") {
			let mut items: Vec<Plist> = Vec::new();
			while !self.eat("</array>") {
				items.push(self.value()?);
			}
			Plist::Array(items)
		} else if self.eat("<dict/>") {
			Plist::Dict(BTreeMap::new())
		} else if self.eat("<dict>") {
			let mut entries: BTreeMap<String, Plist> = BTreeMap::new();
			while !self.eat("</dict>") {
				self.expect("<key>")?;
				let key: String = self.text("</key>")?;
				let value: Plist = self.value()?;
				ensure!(
					entries.insert(key.clone(), value).is_none(),
					"Duplicate property list key {key}"
				);
			}
			Plist::Dict(entries)
		} else {
			bail!("Unsupported property list element")
		})
	}
}

/// Parses the XML property-list subset `serialize` writes; anything else fails.
fn parse(text: &str) -> Result<Plist> {
	let mut reader: Reader<'_> = Reader { rest: text };
	reader.skip_markup()?;
	reader.expect("<plist")?;
	let end: usize = reader
		.rest
		.find('>')
		.context("Malformed property list: unterminated <plist>")?;
	reader.rest = &reader.rest[end + 1..];
	let value: Plist = reader.value()?;
	reader.expect("</plist>")?;
	reader.skip_markup()?;
	ensure!(
		reader.rest.is_empty(),
		"Malformed property list: trailing content"
	);
	Ok(value)
}

fn path_string(path: &Path) -> Result<String> {
	path.to_str()
		.map(str::to_owned)
		.with_context(|| format!("{} must be UTF-8", path.display()))
}

fn environment(mut values: BTreeMap<String, String>) -> Plist {
	values.insert("PATH".into(), SEARCH_PATH.into());
	for key in ["TRAPI2LITELLM_PYTHON", "PYTHONPATH", "PYTHONHOME"] {
		values.insert(key.into(), String::new());
	}
	Plist::Dict(
		values
			.into_iter()
			.map(|(key, value)| (key, Plist::String(value)))
			.collect(),
	)
}

/// LaunchAgent property lists keyed by file name: the KeepAlive gateway and the hourly sync.
fn render_plists(
	entry: &Path,
	settings: &Settings,
	labels: &[String; 2],
	logs_dir: &Path,
) -> Result<BTreeMap<String, String>> {
	let program: String = path_string(entry)?;
	// The environment renders these lossily, as systemd does; a plist must not hide that.
	path_string(&settings.config_dir)?;
	path_string(&settings.state_dir)?;
	let gateway_log: String = path_string(&logs_dir.join("gateway.log"))?;
	let sync_log: String = path_string(&logs_dir.join("sync.log"))?;
	let shared: BTreeMap<String, String> = job_environment(settings);
	let mut gateway_variables: BTreeMap<String, String> = shared.clone();
	gateway_variables.insert(
		"CONFIG_FILE_PATH".into(),
		settings.config_path().display().to_string(),
	);
	gateway_variables.extend(
		gateway_environment(settings)
			.into_iter()
			.map(|(key, value)| (key.into(), value.into())),
	);
	let gateway: Plist = Plist::dict([
		("Label", Plist::string(&labels[0])),
		(
			"ProgramArguments",
			Plist::strings([&program, "serve", "--port", &settings.port.to_string()]),
		),
		("RunAtLoad", Plist::Bool(true)),
		(
			"KeepAlive",
			Plist::dict([("SuccessfulExit", Plist::Bool(false))]),
		),
		("ThrottleInterval", Plist::Integer(10)),
		("ExitTimeOut", Plist::Integer(930)),
		("Umask", Plist::Integer(0o077)),
		("StandardOutPath", Plist::string(&gateway_log)),
		("StandardErrorPath", Plist::string(&gateway_log)),
		("EnvironmentVariables", environment(gateway_variables)),
	]);
	let sync: Plist = Plist::dict([
		("Label", Plist::string(&labels[1])),
		("ProgramArguments", Plist::strings([&program, "sync"])),
		(
			"StartCalendarInterval",
			Plist::dict([("Minute", Plist::Integer(7))]),
		),
		("RunAtLoad", Plist::Bool(false)),
		("Umask", Plist::Integer(0o077)),
		("StandardOutPath", Plist::string(&sync_log)),
		("StandardErrorPath", Plist::string(&sync_log)),
		("EnvironmentVariables", environment(shared)),
	]);
	Ok(BTreeMap::from([
		(format!("{}.plist", labels[0]), serialize(&gateway)?),
		(format!("{}.plist", labels[1]), serialize(&sync)?),
	]))
}

/// Value of a top-level `key = value` line of `launchctl print`; nested blocks are skipped.
fn property<'o>(output: &'o str, key: &str) -> Result<Option<&'o str>> {
	let mut depth: usize = 0;
	let mut found: Option<&str> = None;
	for line in output.lines().map(str::trim) {
		if line == "}" {
			depth = depth
				.checked_sub(1)
				.context("Unbalanced launchctl print output")?;
			continue;
		}
		if depth == 1
			&& let Some(value) = line
				.strip_prefix(key)
				.and_then(|rest| rest.strip_prefix(" = "))
		{
			ensure!(
				found.replace(value).is_none(),
				"launchctl print reported {key} twice"
			);
		}
		if line.ends_with('{') {
			depth += 1;
		}
	}
	ensure!(depth == 0, "Unbalanced launchctl print output");
	Ok(found)
}

/// Plist path and state of a loaded service, such as `running`, `not running` or a transitional
/// `spawn scheduled`, or `None` when its domain has not loaded it.
fn parse_print(code: i32, output: &str) -> Result<Option<(PathBuf, &str)>> {
	match code {
		NOT_LOADED => return Ok(None),
		0 => {}
		_ => bail!("launchctl print exited with status {code}"),
	}
	let path: &str = property(output, "path")?.context("launchctl print reported no plist path")?;
	let state: &str = property(output, "state")?.context("launchctl print reported no state")?;
	Ok(Some((path.into(), state)))
}

/// Whether `launchctl print-disabled` overrides `label` to disabled. Understands both the
/// `=> disabled`/`=> enabled` form and the older `=> true`/`=> false` form, and the line launchd
/// prints for a domain without overrides.
fn parse_disabled(output: &str, label: &str) -> Result<bool> {
	let lines: Vec<&str> = output.lines().map(str::trim).collect();
	if lines.contains(&"disabled services = (no disabled services)") {
		return Ok(false);
	}
	let mut lines = lines
		.into_iter()
		.skip_while(|line| *line != "disabled services = {");
	ensure!(
		lines.next().is_some(),
		"Unrecognized launchctl print-disabled output"
	);
	let quoted: String = format!("\"{label}\" => ");
	let mut found: Option<bool> = None;
	for line in lines {
		if line == "}" {
			return Ok(found.unwrap_or(false));
		}
		if let Some(value) = line.strip_prefix(&quoted) {
			let disabled: bool = match value {
				"disabled" | "true" => true,
				"enabled" | "false" => false,
				_ => bail!("Unrecognized launchctl override for {label}: {value}"),
			};
			ensure!(
				found
					.replace(disabled)
					.is_none_or(|previous| previous == disabled),
				"Conflicting launchctl overrides for {label}"
			);
		}
	}
	bail!("Unterminated launchctl print-disabled output")
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
	match fs::read(path) {
		Ok(bytes) => Ok(Some(bytes)),
		Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
		Err(error) => Err(error.into()),
	}
}

fn remove_optional(path: &Path) -> Result<()> {
	match fs::remove_file(path) {
		Err(error) if error.kind() != ErrorKind::NotFound => {
			Err(error).with_context(|| format!("Could not remove {}", path.display()))
		}
		_ => Ok(()),
	}
}

/// Device of the nearest existing ancestor, which directories created below it share.
fn device(path: &Path) -> Result<u64> {
	let existing: &Path = path
		.ancestors()
		.find(|ancestor| ancestor.exists())
		.with_context(|| format!("{} has no existing ancestor", path.display()))?;
	Ok(fs::metadata(existing)?.dev())
}

/// Runs `operation` for every job, even after one fails, and reports every failure.
fn each_job(jobs: [Job; 2], operation: impl Fn(Job) -> Result<()>) -> Result<()> {
	let errors: Vec<String> = jobs
		.into_iter()
		.filter_map(|job| operation(job).err())
		.map(|error| format!("{error:#}"))
		.collect();
	ensure!(errors.is_empty(), "{}", errors.join("; "));
	Ok(())
}

fn same_file(left: &Path, right: &Path) -> Result<bool> {
	Ok(left == right
		|| (left.exists() && right.exists() && left.canonicalize()? == right.canonicalize()?))
}

/// launchctl access to the user's GUI domain.
pub trait LaunchHost {
	/// Runs a launchctl subcommand that must succeed within `timeout`.
	fn run(&self, arguments: &[&str], timeout: Duration) -> Result<()>;
	/// Exit status and standard output of `launchctl print <target>`.
	fn print(&self, target: &str) -> Result<(i32, String)>;
	/// Standard output of `launchctl print-disabled <domain>`.
	fn print_disabled(&self, domain: &str) -> Result<String>;
	fn uid(&self) -> u32;
	/// Whether nothing listens on the loopback port.
	fn port_free(&self, port: u16) -> Result<bool>;
}

pub struct SystemLaunchHost;

impl SystemLaunchHost {
	fn launchctl(arguments: &[&str], timeout: Duration) -> Result<std::process::Output> {
		process::run(Command::new("/bin/launchctl").args(arguments), &[], timeout)
	}
}

impl LaunchHost for SystemLaunchHost {
	fn run(&self, arguments: &[&str], timeout: Duration) -> Result<()> {
		let output: std::process::Output = Self::launchctl(arguments, timeout)?;
		ensure!(
			output.status.success(),
			"launchctl {} failed ({}): {}",
			arguments.join(" "),
			output.status,
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}
	fn print(&self, target: &str) -> Result<(i32, String)> {
		let output: std::process::Output = Self::launchctl(&["print", target], LAUNCHCTL_TIMEOUT)?;
		Ok((
			output
				.status
				.code()
				.context("launchctl print was terminated by a signal")?,
			// The domain listing includes third-party services; the parser needs only ASCII keys.
			String::from_utf8_lossy(&output.stdout).into_owned(),
		))
	}
	fn print_disabled(&self, domain: &str) -> Result<String> {
		let output: std::process::Output =
			Self::launchctl(&["print-disabled", domain], LAUNCHCTL_TIMEOUT)?;
		ensure!(
			output.status.success(),
			"launchctl print-disabled {domain} failed ({})",
			output.status
		);
		Ok(String::from_utf8(output.stdout)?)
	}
	fn uid(&self) -> u32 {
		// getuid has no pointer arguments or failure mode.
		unsafe { libc::getuid() }
	}
	fn port_free(&self, port: u16) -> Result<bool> {
		let address: SocketAddr = SocketAddr::from(([127, 0, 0, 1], port));
		match TcpListener::bind(address) {
			Ok(listener) => drop(listener),
			Err(error) if error.kind() == ErrorKind::AddrInUse => return Ok(false),
			Err(error) => {
				return Err(error)
					.with_context(|| format!("Cannot check whether port {port} is free"));
			}
		}
		// A wildcard listener may still let a specific bind succeed, so also try connecting.
		match TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
			Ok(_) => Ok(false),
			Err(error) if error.kind() == ErrorKind::ConnectionRefused => Ok(true),
			Err(error) => {
				Err(error).with_context(|| format!("Cannot check whether port {port} is free"))
			}
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
	Unloaded,
	Idle,
	Running,
}

fn index(job: Job) -> usize {
	match job {
		Job::Gateway => 0,
		Job::Sync => 1,
	}
}

/// Per-user launchd: deploy stages plists in the configuration directory and `activate_all`
/// publishes them to the LaunchAgents directory, where file presence is the enablement.
pub struct LaunchdManager<'a, H: LaunchHost> {
	host: &'a H,
	settings: &'a Settings,
	staging_dir: PathBuf,
	agents_dir: PathBuf,
	logs_dir: PathBuf,
	labels: [String; 2],
	files: [String; 2],
}

impl<'a, H: LaunchHost> LaunchdManager<'a, H> {
	pub fn new(
		host: &'a H,
		settings: &'a Settings,
		agents_dir: PathBuf,
		logs_dir: PathBuf,
		label_prefix: &str,
	) -> Result<Self> {
		ensure!(
			!label_prefix.is_empty()
				&& label_prefix
					.chars()
					.all(|character| character.is_ascii_alphanumeric() || "._-".contains(character)),
			"Invalid launchd label prefix {label_prefix}"
		);
		let labels: [String; 2] = [
			format!("{label_prefix}.gateway"),
			format!("{label_prefix}.sync"),
		];
		Ok(Self {
			host,
			settings,
			staging_dir: settings.config_dir.join("launchd"),
			agents_dir,
			logs_dir,
			files: labels.clone().map(|label| format!("{label}.plist")),
			labels,
		})
	}
	pub fn label(&self, job: Job) -> &str {
		&self.labels[index(job)]
	}
	fn domain(&self) -> String {
		format!("gui/{}", self.host.uid())
	}
	fn target(&self, job: Job) -> String {
		format!("{}/{}", self.domain(), self.label(job))
	}
	fn launchctl(&self, arguments: &[&str], timeout: Duration) -> Result<()> {
		self.host.run(arguments, timeout)
	}
	fn loaded(&self, job: Job) -> Result<bool> {
		let target: String = self.target(job);
		match self.host.print(&target)?.0 {
			0 => Ok(true),
			NOT_LOADED => Ok(false),
			code => bail!("launchctl print {target} exited with status {code}"),
		}
	}
	/// launchd's state of a loaded job, or `None` when the job is not loaded. Fails closed on a job
	/// loaded from a plist other than its LaunchAgents copy.
	fn loaded_state(&self, job: Job) -> Result<Option<String>> {
		let (code, output) = self.host.print(&self.target(job))?;
		let Some((path, state)) = parse_print(code, &output)
			.with_context(|| format!("Cannot inspect launchd job {}", self.label(job)))?
		else {
			return Ok(None);
		};
		let expected: PathBuf = self.loaded_definition(job);
		ensure!(
			same_file(&path, &expected)?,
			"{} is loaded from {}, not {}; boot it out first",
			self.label(job),
			path.display(),
			expected.display()
		);
		Ok(Some(state.to_owned()))
	}
	/// Whether launchd has loaded the job and runs it. Fails closed on transitional states, such as
	/// a throttled `spawn scheduled`, that deploy cannot snapshot or restore.
	pub fn status(&self, job: Job) -> Result<Status> {
		Ok(match self.loaded_state(job)?.as_deref() {
			None => Status::Unloaded,
			Some("running") => Status::Running,
			Some("not running") => Status::Idle,
			Some(state) => bail!(
				"launchd reports {} as '{state}'; wait until it is running or not running, or boot it out with launchctl bootout {}",
				self.label(job),
				self.target(job)
			),
		})
	}
	fn disabled(&self, job: Job) -> Result<bool> {
		parse_disabled(&self.host.print_disabled(&self.domain())?, self.label(job))
	}
	fn check_port(&self) -> Result<()> {
		let port: u16 = self.settings.port;
		ensure!(
			self.host.port_free(port)?,
			"Port {port} is already in use; find the holder with lsof -nP -iTCP:{port} -sTCP:LISTEN"
		);
		Ok(())
	}
	fn bootstrap(&self, job: Job) -> Result<()> {
		// launchd creates log files but not their directory.
		files::private_directory(&self.logs_dir)?;
		self.launchctl(
			&[
				"bootstrap",
				&self.domain(),
				&path_string(&self.loaded_definition(job))?,
			],
			LAUNCHCTL_TIMEOUT,
		)
	}
	fn bootout(&self, job: Job) -> Result<()> {
		self.launchctl(&["bootout", "--wait", &self.target(job)], BOOTOUT_TIMEOUT)
	}
	fn kickstart(&self, job: Job) -> Result<()> {
		self.launchctl(&["kickstart", &self.target(job)], LAUNCHCTL_TIMEOUT)
	}
}

impl<H: LaunchHost> ServiceManager for LaunchdManager<'_, H> {
	fn definitions_dir(&self) -> &Path {
		&self.staging_dir
	}
	fn definitions_kind(&self) -> &str {
		"LaunchAgent plists"
	}
	fn definition(&self, job: Job) -> &str {
		&self.files[index(job)]
	}
	fn loaded_definition(&self, job: Job) -> PathBuf {
		self.agents_dir.join(self.definition(job))
	}
	fn render(&self, entry: &Path) -> Result<BTreeMap<String, String>> {
		render_plists(entry, self.settings, &self.labels, &self.logs_dir)
	}
	fn is_managed(&self, old: &str, _new: &str) -> bool {
		is_managed(old)
	}
	fn check_available(&self, start: bool) -> Result<()> {
		// Install-only stages plists without touching launchd.
		if !start {
			return Ok(());
		}
		ensure!(
			self.host.uid() != 0,
			"Run deploy --start as the desktop user, not root: LaunchAgents load into that user's GUI login session"
		);
		let domain: String = self.domain();
		let code: i32 = self.host.print(&domain)?.0;
		ensure!(
			code == 0,
			"No GUI login session for this user (launchctl print {domain} exited with status {code}); LaunchAgents run only while the user is logged in to the desktop"
		);
		// Activation checks again after synchronization; checking first saves the catalog fetch.
		if self.status(Job::Gateway)? != Status::Running {
			self.check_port()?;
		}
		Ok(())
	}
	fn preflight(&self, definitions: &BTreeMap<String, String>) -> Result<()> {
		ensure!(
			device(&self.staging_dir)? == device(&self.agents_dir)?,
			"{} and {} must be on the same volume: plists are written in the first and renamed into the second",
			self.staging_dir.display(),
			self.agents_dir.display()
		);
		for (name, content) in definitions {
			let plist: Plist = parse(content).with_context(|| format!("Invalid {name}"))?;
			ensure!(
				plist
					.get("Label")
					.and_then(Plist::as_str)
					.is_some_and(|label| name.strip_suffix(".plist") == Some(label)),
				"{name}: Label must match the file name"
			);
			let program: &Path = plist
				.get("ProgramArguments")
				.and_then(Plist::as_array)
				.and_then(<[Plist]>::first)
				.and_then(Plist::as_str)
				.map(Path::new)
				.with_context(|| format!("{name}: ProgramArguments has no program"))?;
			ensure!(
				program.is_absolute()
					&& fs::metadata(program).is_ok_and(|metadata| {
						metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
					}),
				"{name}: {} is not an absolute executable path",
				program.display()
			);
		}
		let plutil: &Path = Path::new("/usr/bin/plutil");
		if !plutil.is_file() {
			return Ok(());
		}
		let folder: tempfile::TempDir = tempfile::Builder::new()
			.prefix("trapi2litellm-plists-")
			.tempdir()?;
		let mut paths: Vec<PathBuf> = Vec::new();
		for (name, content) in definitions {
			let path: PathBuf = folder.path().join(name);
			fs::write(&path, content)?;
			paths.push(path);
		}
		let output: std::process::Output = process::run(
			Command::new(plutil).arg("-lint").args(paths),
			&[],
			LAUNCHCTL_TIMEOUT,
		)?;
		ensure!(
			output.status.success(),
			"launchd plist validation failed ({}): {}{}",
			output.status,
			String::from_utf8_lossy(&output.stdout).trim(),
			String::from_utf8_lossy(&output.stderr).trim()
		);
		Ok(())
	}
	fn definitions_changed(&self) -> Result<()> {
		// launchd reads a plist only when it bootstraps the job.
		Ok(())
	}
	fn outdated(&self, definitions: &BTreeMap<String, String>) -> Result<BTreeSet<Job>> {
		let mut jobs: BTreeSet<Job> = BTreeSet::new();
		for job in Job::ALL {
			if read_optional(&self.loaded_definition(job))?.as_deref()
				!= Some(definitions[self.definition(job)].as_bytes())
			{
				jobs.insert(job);
			}
		}
		Ok(jobs)
	}
	fn state(&self, job: Job) -> Result<UnitState> {
		let label: &str = self.label(job);
		let status: Status = self.status(job)?;
		let path: PathBuf = self.loaded_definition(job);
		let installed: bool = path.exists();
		ensure!(
			status == Status::Unloaded || installed,
			"{label} is loaded without {}; boot it out first",
			path.display()
		);
		// Deploy never changes a launchctl override, so a user's disable keeps deploy out.
		ensure!(
			!self.disabled(job)?,
			"launchctl disables {label}; run launchctl enable {} before deploying",
			self.target(job)
		);
		Ok(UnitState {
			// An idle gateway exited successfully, which KeepAlive does not restart; like an
			// unloaded one with its plist installed, it starts at the next login.
			active: status == Status::Running || (job == Job::Sync && status == Status::Idle),
			enablement: if installed {
				Enablement::Persistent
			} else {
				Enablement::Disabled
			},
		})
	}
	fn activate_all(&self) -> Result<()> {
		if self.status(Job::Gateway)? != Status::Running {
			self.check_port()?;
		}
		fs::create_dir_all(&self.agents_dir)?;
		let mut published: BTreeSet<Job> = BTreeSet::new();
		for job in Job::ALL {
			let staged: Vec<u8> = fs::read(self.staging_dir.join(self.definition(job)))?;
			let path: PathBuf = self.loaded_definition(job);
			if read_optional(&path)?.as_deref() != Some(staged.as_slice()) {
				// launchd may load any file in LaunchAgents, so the temporary file stays out of it.
				files::atomic_write_in(&path, &staged, &self.staging_dir)?;
				published.insert(job);
			}
		}
		for job in Job::ALL {
			// The transaction restarts armed jobs whose plist changed. An idle gateway is not
			// armed, and kickstart would run the plist launchd loaded before.
			if job == Job::Gateway && published.contains(&job) && self.status(job)? == Status::Idle
			{
				self.restart(job)?;
			} else {
				self.start(job)?;
			}
		}
		Ok(())
	}
	fn restart(&self, job: Job) -> Result<()> {
		// kickstart -k would keep the plist launchd loaded; only a new bootstrap reads the file.
		if self.loaded(job)? {
			self.bootout(job)?;
		}
		self.bootstrap(job)
	}
	fn start(&self, job: Job) -> Result<()> {
		match self.status(job)? {
			Status::Unloaded => self.bootstrap(job),
			Status::Idle if job == Job::Gateway => self.kickstart(job),
			Status::Idle | Status::Running => Ok(()),
		}
	}
	fn stop_all(&self) -> Result<()> {
		each_job([Job::Sync, Job::Gateway], |job| {
			if self.loaded(job)? {
				self.bootout(job)?;
			}
			Ok(())
		})
	}
	fn disable_all(&self) -> Result<()> {
		each_job(Job::ALL, |job| {
			remove_optional(&self.loaded_definition(job))
		})
	}
	fn restore_enablement(&self, job: Job, enablement: Enablement) -> Result<()> {
		// Restoring the snapshot already restored or removed the LaunchAgents copy.
		let path: PathBuf = self.loaded_definition(job);
		match enablement {
			Enablement::Persistent => ensure!(path.exists(), "{} was not restored", path.display()),
			Enablement::Disabled => ensure!(!path.exists(), "{} was not removed", path.display()),
			Enablement::Runtime => bail!("launchd has no runtime-only enablement"),
		}
		Ok(())
	}
	fn same_target(&self, old_gateway: &str) -> Result<bool> {
		let plist: Plist = parse(old_gateway)?;
		let environment = |key: &str| -> Option<&str> {
			plist
				.get("EnvironmentVariables")
				.and_then(|values| values.get(key))
				.and_then(Plist::as_str)
		};
		let config: String = self.settings.config_path().display().to_string();
		let state: String = self.settings.state_dir.to_string_lossy().into_owned();
		let tail: Plist = Plist::strings(["serve", "--port", &self.settings.port.to_string()]);
		Ok(environment("CONFIG_FILE_PATH") == Some(config.as_str())
			&& environment("TRAPI2LITELLM_STATE_DIR") == Some(state.as_str())
			&& plist
				.get("ProgramArguments")
				.and_then(Plist::as_array)
				.zip(tail.as_array())
				.is_some_and(|(arguments, tail)| arguments.ends_with(tail)))
	}
	fn linger(&self) -> Result<&dyn Linger> {
		bail!(
			"--enable-linger is not available with launchd: LaunchAgents run only while the user is logged in to the desktop, and there is no linger equivalent"
		)
	}
}

/// The launchd gateway as synchronization sees it.
pub struct LaunchdService<'a, H: LaunchHost> {
	pub manager: &'a LaunchdManager<'a, H>,
}

impl<H: LaunchHost> Service for LaunchdService<'_, H> {
	fn active(&self) -> Result<bool> {
		// Unlike deploy, synchronization tolerates transitional states such as a throttled
		// `spawn scheduled`: a gateway that is not running reads the new configuration when it
		// next starts.
		Ok(self.manager.loaded_state(Job::Gateway)?.as_deref() == Some("running"))
	}
	fn reload(&self) -> Result<()> {
		self.manager.launchctl(
			&["kill", "SIGHUP", &self.manager.target(Job::Gateway)],
			LAUNCHCTL_TIMEOUT,
		)
	}
	fn wait_for_models(&self, expected: &BTreeSet<String>, digest: &str) -> Result<()> {
		sync::wait_for_models(self.manager.settings, expected, digest)
	}
}

/// Representative `launchctl print` output for a LaunchAgent in each state.
#[cfg(test)]
fn sample_print(label: &str, path: &Path, state: &str) -> String {
	let process: String = if state == "running" {
		"\tpid = 4242\n\timmediate reason = speculative\n".into()
	} else {
		String::new()
	};
	format!(
		"gui/501/{label} = {{
	active count = 1
	path = {path}
	type = LaunchAgent
	state = {state}

	program = /Users/me/.local/bin/trapi2litellm
	arguments = {{
		/Users/me/.local/bin/trapi2litellm
		sync
	}}

	stdout path = /Users/me/Library/Logs/trapi2litellm/sync.log
	stderr path = /Users/me/Library/Logs/trapi2litellm/sync.log
	inherited environment = {{
		SSH_AUTH_SOCK => /private/tmp/com.apple.launchd.abc/Listeners
	}}

	default environment = {{
		PATH => /usr/bin:/bin:/usr/sbin:/sbin
	}}

	environment = {{
		PATH => /opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
		XPC_SERVICE_NAME => {label}
	}}

	domain = gui/501 [100005]
	asid = 100005
	minimum runtime = 10
	exit timeout = 5
	runs = 3
{process}	last exit code = 0

	event triggers = {{
		{label}.268435474 => {{
			keepalive = 0
			service = {label}
			stream = com.apple.launchd.calendarinterval.system
			monitor = 0
			descriptor = {{
				\"Minute\" => 7
			}}
		}}
	}}

	spawn type = daemon (3)
	jetsam priority = 40
	jetsam memory limit (active) = (unlimited)
	jetsam memory limit (inactive) = (unlimited)
	jetsamproperties category = daemon
	jetsam thread limit = 32
	cpumon = default
	probabilistic guard malloc policy = {{
		activation rate = 1/1000
		sample rate = 1/0
	}}

	properties = inferred program
}}
",
		path = path.display()
	)
}

/// A GUI domain that applies launchctl subcommands to in-memory jobs, read back from the plists
/// they were bootstrapped from.
#[cfg(test)]
pub(super) mod fixture {
	use super::*;
	use std::cell::{Cell, RefCell};

	struct Loaded {
		path: PathBuf,
		plist: String,
		/// launchd's state, such as `running`, `not running` or `spawn scheduled`.
		state: &'static str,
	}

	/// A launchctl subcommand that fails once, on its `occurrence`th call after the fault is set.
	#[derive(Clone, Copy, Debug)]
	pub struct Fault {
		subcommand: &'static str,
		occurrence: usize,
		before_effect: bool,
	}

	impl Fault {
		/// Fails after taking effect, as an interrupted or timed-out launchctl can.
		pub const fn after(subcommand: &'static str, occurrence: usize) -> Self {
			Self {
				subcommand,
				occurrence,
				before_effect: false,
			}
		}
		/// Fails without taking effect, as a `bootout --wait` that times out does.
		pub const fn before(subcommand: &'static str, occurrence: usize) -> Self {
			Self {
				subcommand,
				occurrence,
				before_effect: true,
			}
		}
	}

	#[derive(Default)]
	pub struct LaunchdFixture {
		jobs: RefCell<BTreeMap<String, Loaded>>,
		overrides: RefCell<BTreeMap<String, bool>>,
		pub port_busy: Cell<bool>,
		/// `launchctl print gui/501` fails, as it does without a GUI login session.
		pub no_session: Cell<bool>,
		pub fault: Cell<Option<Fault>>,
		log: RefCell<Vec<String>>,
	}

	impl LaunchdFixture {
		fn label(target: &str) -> Result<String> {
			target
				.strip_prefix("gui/501/")
				.map(str::to_owned)
				.context("Fixture target outside gui/501")
		}
		/// launchctl subcommands run since the last call.
		pub fn take_log(&self) -> Vec<String> {
			self.log.take()
		}
		pub fn loaded_plist(&self, label: &str) -> Option<String> {
			self.jobs.borrow().get(label).map(|job| job.plist.clone())
		}
		/// Sets the override that `launchctl disable` (true) or `launchctl enable` (false) leaves.
		pub fn set_override(&self, label: &str, disabled: bool) {
			self.overrides.borrow_mut().insert(label.into(), disabled);
		}
		/// Moves a loaded job to another launchd state, as a process exit or a throttled respawn
		/// does.
		pub fn set_state(&self, label: &str, state: &'static str) {
			self.jobs
				.borrow_mut()
				.get_mut(label)
				.expect("Fixture job is loaded")
				.state = state;
		}
		/// Unloads a job outside the deployment.
		pub fn unload(&self, label: &str) {
			self.jobs.borrow_mut().remove(label);
		}
		/// Whether the fault fires on this call, and whether it fires before taking effect.
		fn fire(&self, subcommand: &str) -> Option<bool> {
			let fault: Fault = self
				.fault
				.get()
				.filter(|fault| fault.subcommand == subcommand)?;
			if fault.occurrence > 1 {
				self.fault.set(Some(Fault {
					occurrence: fault.occurrence - 1,
					..fault
				}));
				return None;
			}
			self.fault.set(None);
			Some(fault.before_effect)
		}
	}

	impl LaunchHost for LaunchdFixture {
		fn run(&self, arguments: &[&str], timeout: Duration) -> Result<()> {
			self.log.borrow_mut().push(arguments.join(" "));
			let fired: Option<bool> = self.fire(arguments[0]);
			ensure!(
				fired != Some(true),
				"Fixture {} failure before taking effect",
				arguments[0]
			);
			match arguments {
				["bootstrap", "gui/501", path] => {
					let plist: String = fs::read_to_string(path)?;
					let parsed: Plist = parse(&plist)?;
					let label: String = parsed
						.get("Label")
						.and_then(Plist::as_str)
						.context("Fixture plist has no Label")?
						.into();
					ensure!(
						!self.jobs.borrow().contains_key(&label),
						"Bootstrap failed: 5: Input/output error"
					);
					ensure!(
						self.overrides.borrow().get(&label) != Some(&true),
						"Bootstrap failed: 119: Service is disabled"
					);
					let state: &str = if parsed.get("RunAtLoad") == Some(&Plist::Bool(true)) {
						"running"
					} else {
						"not running"
					};
					self.jobs.borrow_mut().insert(
						label,
						Loaded {
							path: PathBuf::from(path),
							plist,
							state,
						},
					);
				}
				["bootout", "--wait", target] => {
					ensure!(
						timeout >= Duration::from_secs(930),
						"bootout --wait must outlast ExitTimeOut"
					);
					ensure!(
						self.jobs
							.borrow_mut()
							.remove(&Self::label(target)?)
							.is_some(),
						"Boot-out failed: 3: No such process"
					);
				}
				["kickstart", target] => {
					self.jobs
						.borrow_mut()
						.get_mut(&Self::label(target)?)
						.context("Could not find service")?
						.state = "running";
				}
				["kill", "SIGHUP", target] => ensure!(
					self.jobs
						.borrow()
						.get(&Self::label(target)?)
						.is_some_and(|job| job.state == "running"),
					"No process to signal"
				),
				_ => bail!("Unexpected launchctl {}", arguments.join(" ")),
			}
			ensure!(
				fired.is_none(),
				"Fixture {} failure after taking effect",
				arguments[0]
			);
			Ok(())
		}
		fn print(&self, target: &str) -> Result<(i32, String)> {
			if target == "gui/501" {
				return Ok(if self.no_session.get() {
					(112, String::new())
				} else {
					(0, "gui/501 = {\n\ttype = gui\n}\n".into())
				});
			}
			let label: String = Self::label(target)?;
			Ok(match self.jobs.borrow().get(&label) {
				Some(job) => (0, sample_print(&label, &job.path, job.state)),
				None => (NOT_LOADED, String::new()),
			})
		}
		fn print_disabled(&self, domain: &str) -> Result<String> {
			ensure!(domain == "gui/501", "Unexpected domain {domain}");
			let overrides = self.overrides.borrow();
			let disabled: String = if overrides.is_empty() {
				"disabled services = (no disabled services)\n".into()
			} else {
				format!(
					"disabled services = {{\n{}}}\n",
					overrides
						.iter()
						.map(|(label, disabled)| format!(
							"\t\"{label}\" => {}\n",
							if *disabled { "disabled" } else { "enabled" }
						))
						.collect::<String>()
				)
			};
			Ok(format!("{disabled}\nlogin item associations = {{\n}}\n"))
		}
		fn uid(&self) -> u32 {
			501
		}
		fn port_free(&self, _: u16) -> Result<bool> {
			Ok(!self.port_busy.get())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{fixture::LaunchdFixture, *};

	const PREFIX: &str = "io.github.acture.trapi2litellm.test";

	fn manager<'a>(
		host: &'a LaunchdFixture,
		settings: &'a Settings,
		root: &Path,
	) -> LaunchdManager<'a, LaunchdFixture> {
		LaunchdManager::new(
			host,
			settings,
			root.join("LaunchAgents"),
			root.join("Logs"),
			PREFIX,
		)
		.unwrap()
	}

	#[test]
	fn rendering_escapes_and_round_trips() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let mut settings: Settings = crate::settings::test_settings(root.path());
		settings.config_dir = root.path().join("config <&> space%u\"\\$HOME*?[x]");
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> = manager(&host, &settings, root.path());
		let plists: BTreeMap<String, String> = manager
			.render(Path::new("/opt/a&b/bin/trapi2litellm"))
			.unwrap();
		let names: Vec<&str> = plists.keys().map(String::as_str).collect();
		assert_eq!(
			names,
			[
				format!("{PREFIX}.gateway.plist"),
				format!("{PREFIX}.sync.plist")
			]
		);
		let gateway_text: &str = &plists[manager.definition(Job::Gateway)];
		assert!(gateway_text.starts_with(&format!("{DECLARATION}\n{MARKER}\n{DOCTYPE}\n")));
		assert!(is_managed(gateway_text));
		assert!(!is_managed(&gateway_text.replacen(MARKER, "<!-- x -->", 1)));
		assert!(gateway_text.contains("<string>/opt/a&amp;b/bin/trapi2litellm</string>"));
		assert!(gateway_text.contains("config &lt;&amp;&gt; space%u\"\\$HOME*?[x]/config.yaml"));
		assert!(!gateway_text.contains("LITELLM_MASTER_KEY"));
		assert!(!gateway_text.contains("gateway.env"));
		let gateway: Plist = parse(gateway_text).unwrap();
		let config: String = settings.config_path().display().to_string();
		let environment: &Plist = gateway.get("EnvironmentVariables").unwrap();
		let mut expected: BTreeMap<String, String> = job_environment(&settings);
		expected.extend([
			("CONFIG_FILE_PATH".into(), config),
			("LITELLM_MODE".into(), "PRODUCTION".into()),
			("LITELLM_LOG".into(), "WARNING".into()),
			(
				"AZURE_TOKEN_CREDENTIALS".into(),
				"ManagedIdentityCredential".into(),
			),
			("AZURE_CREDENTIAL".into(), "DefaultAzureCredential".into()),
			("PATH".into(), SEARCH_PATH.into()),
			("TRAPI2LITELLM_PYTHON".into(), String::new()),
			("PYTHONPATH".into(), String::new()),
			("PYTHONHOME".into(), String::new()),
		]);
		assert_eq!(
			*environment,
			Plist::Dict(
				expected
					.into_iter()
					.map(|(key, value)| (key, Plist::String(value)))
					.collect()
			)
		);
		let log: String = root.path().join("Logs/gateway.log").display().to_string();
		assert_eq!(
			gateway,
			Plist::dict([
				("Label", Plist::string(format!("{PREFIX}.gateway"))),
				(
					"ProgramArguments",
					Plist::strings(["/opt/a&b/bin/trapi2litellm", "serve", "--port", "4000"]),
				),
				("RunAtLoad", Plist::Bool(true)),
				(
					"KeepAlive",
					Plist::dict([("SuccessfulExit", Plist::Bool(false))])
				),
				("ThrottleInterval", Plist::Integer(10)),
				("ExitTimeOut", Plist::Integer(930)),
				("Umask", Plist::Integer(63)),
				("StandardOutPath", Plist::string(&log)),
				("StandardErrorPath", Plist::string(&log)),
				("EnvironmentVariables", environment.clone()),
			])
		);
		let sync: Plist = parse(&plists[manager.definition(Job::Sync)]).unwrap();
		assert_eq!(
			sync.get("ProgramArguments"),
			Some(&Plist::strings(["/opt/a&b/bin/trapi2litellm", "sync"]))
		);
		assert_eq!(
			sync.get("StartCalendarInterval"),
			Some(&Plist::dict([("Minute", Plist::Integer(7))]))
		);
		assert_eq!(sync.get("RunAtLoad"), Some(&Plist::Bool(false)));
		assert_eq!(sync.get("ProcessType"), None);
		let sync_environment: &Plist = sync.get("EnvironmentVariables").unwrap();
		for key in ["CONFIG_FILE_PATH", "LITELLM_MODE", "AZURE_CREDENTIAL"] {
			assert_eq!(sync_environment.get(key), None);
		}
		for key in ["PATH", "PYTHONHOME", "NO_PROXY", "TRAPI2LITELLM_STATE_DIR"] {
			assert_eq!(sync_environment.get(key), environment.get(key));
		}
		assert!(manager.same_target(gateway_text).unwrap());
		let mut moved: Settings = settings.clone();
		moved.port += 1;
		assert!(
			!LaunchdManager::new(
				&host,
				&moved,
				root.path().into(),
				root.path().into(),
				PREFIX
			)
			.unwrap()
			.same_target(gateway_text)
			.unwrap()
		);
	}

	#[test]
	fn control_characters_and_foreign_markup_fail_closed() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let mut settings: Settings = crate::settings::test_settings(root.path());
		settings.state_dir = root.path().join("state\n<key>Program</key>");
		let host: LaunchdFixture = LaunchdFixture::default();
		assert!(
			manager(&host, &settings, root.path())
				.render(Path::new("/bin/trapi2litellm"))
				.is_err()
		);
		assert!(serialize(&Plist::string("tab\tseparated")).is_err());
		assert!(serialize(&Plist::string("\u{FFFF}")).is_err());
		let mut lossy: Settings = crate::settings::test_settings(root.path());
		lossy.config_dir = root
			.path()
			.join(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"\xff"));
		assert!(
			manager(&host, &lossy, root.path())
				.render(Path::new("/bin/trapi2litellm"))
				.is_err()
		);
		assert!(
			LaunchdManager::new(
				&host,
				&settings,
				root.path().into(),
				root.path().into(),
				"a/b"
			)
			.is_err()
		);
		for text in [
			"<plist><dict><key>a</key><data>AA==</data></dict></plist>",
			"<plist><dict><key>a</key><string>x</string><key>a</key><string>y</string></dict></plist>",
			"<plist><string>&unknown;</string></plist>",
			"<plist><string>unterminated</plist>",
			"<plist><string>x</string></plist><string>y</string>",
		] {
			assert!(parse(text).is_err(), "{text}");
		}
		assert_eq!(
			parse(
				"<plist version=\"1.0\"><string>&lt;&#65;&#x42;&quot;&apos;&gt;</string></plist>"
			)
			.unwrap(),
			Plist::string("<AB\"'>")
		);
	}

	#[test]
	fn print_parser_fails_closed() {
		let path: &Path = Path::new("/Users/me/Library/LaunchAgents/x.plist");
		for state in ["running", "not running", "spawn scheduled"] {
			assert_eq!(
				parse_print(0, &sample_print("x", path, state)).unwrap(),
				Some((path.into(), state))
			);
		}
		assert_eq!(parse_print(NOT_LOADED, "").unwrap(), None);
		assert!(
			parse_print(
				0,
				&sample_print("x", path, "running").replace("\tstate = running\n", "")
			)
			.is_err()
		);
		assert!(
			parse_print(
				0,
				&sample_print("x", path, "running")
					.replace("\ttype =", "\tstate = waiting\n\ttype =")
			)
			.is_err()
		);
		assert!(parse_print(5, "").is_err());
		assert!(parse_print(0, "gui/501/x = {\n\tstate = running\n").is_err());
		assert_eq!(
			property(&sample_print("x", path, "running"), "pid").unwrap(),
			Some("4242")
		);
	}

	#[test]
	fn print_disabled_parser_reads_both_dialects() {
		let modern: &str = "disabled services = {\n\t\"com.apple.ScriptMenuApp\" => disabled\n\t\"io.x.gateway\" => disabled\n\t\"io.x.sync\" => enabled\n}\n\nlogin item associations = {\n\t\"io.x.other\" => \"io.x\"\n}\n";
		assert!(parse_disabled(modern, "io.x.gateway").unwrap());
		assert!(!parse_disabled(modern, "io.x.sync").unwrap());
		assert!(!parse_disabled(modern, "io.x.other").unwrap());
		let legacy: &str =
			"disabled services = {\n\t\"io.x.gateway\" => true\n\t\"io.x.sync\" => false\n}\n";
		assert!(parse_disabled(legacy, "io.x.gateway").unwrap());
		assert!(!parse_disabled(legacy, "io.x.sync").unwrap());
		assert!(parse_disabled("", "io.x.gateway").is_err());
		assert!(
			!parse_disabled(
				"disabled services = (no disabled services)\n\nlogin item associations = {\n}\n",
				"io.x.gateway"
			)
			.unwrap()
		);
		assert!(
			parse_disabled(
				"disabled services = {\n\t\"io.x.gateway\" => maybe\n}\n",
				"io.x.gateway"
			)
			.is_err()
		);
		assert!(
			parse_disabled(
				"disabled services = {\n\t\"io.x.gateway\" => true\n",
				"io.x.gateway"
			)
			.is_err()
		);
		assert!(
			parse_disabled(
				"disabled services = {\n\t\"io.x.gateway\" => true\n\t\"io.x.gateway\" => false\n}\n",
				"io.x.gateway"
			)
			.is_err()
		);
	}

	/// Renders, stages and preflights the plists, as install-only deployment does.
	fn stage(manager: &LaunchdManager<'_, LaunchdFixture>, root: &Path) {
		let entry: PathBuf = root.join("trapi2litellm");
		fs::write(&entry, "").unwrap();
		fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
		let plists: BTreeMap<String, String> = manager.render(&entry).unwrap();
		manager.preflight(&plists).unwrap();
		fs::create_dir_all(manager.definitions_dir()).unwrap();
		for (name, content) in &plists {
			fs::write(manager.definitions_dir().join(name), content).unwrap();
		}
	}

	#[test]
	fn enablement_follows_file_presence_and_refuses_overrides() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> = manager(&host, &settings, root.path());
		let gateway: &str = manager.label(Job::Gateway);
		assert_eq!(manager.state(Job::Gateway).unwrap(), UnitState::default());
		host.set_override(gateway, true);
		assert!(manager.state(Job::Gateway).is_err());
		host.set_override(gateway, false);
		assert_eq!(manager.state(Job::Gateway).unwrap(), UnitState::default());
		stage(&manager, root.path());
		manager.activate_all().unwrap();
		assert_eq!(
			host.take_log(),
			[
				format!(
					"bootstrap gui/501 {}",
					manager.loaded_definition(Job::Gateway).display()
				),
				format!(
					"bootstrap gui/501 {}",
					manager.loaded_definition(Job::Sync).display()
				),
			]
		);
		assert!(
			fs::read_dir(root.path().join("LaunchAgents"))
				.unwrap()
				.all(|entry| entry
					.unwrap()
					.file_name()
					.to_string_lossy()
					.ends_with(".plist")),
			"Publishing left a temporary file in LaunchAgents"
		);
		let armed: UnitState = UnitState {
			active: true,
			enablement: Enablement::Persistent,
		};
		assert_eq!(manager.state(Job::Gateway).unwrap(), armed);
		assert_eq!(manager.state(Job::Sync).unwrap(), armed);
		assert!(LaunchdService { manager: &manager }.active().unwrap());
		host.set_override(manager.label(Job::Sync), true);
		assert!(manager.state(Job::Sync).is_err());
		host.set_override(manager.label(Job::Sync), false);
		host.set_state(gateway, "not running");
		assert_eq!(
			manager.state(Job::Gateway).unwrap(),
			UnitState {
				active: false,
				enablement: Enablement::Persistent,
			}
		);
		assert!(!LaunchdService { manager: &manager }.active().unwrap());
		manager.start(Job::Gateway).unwrap();
		assert_eq!(host.take_log(), [format!("kickstart gui/501/{gateway}")]);
		assert_eq!(manager.state(Job::Gateway).unwrap(), armed);
		manager.stop_all().unwrap();
		assert_eq!(
			host.take_log(),
			[
				format!("bootout --wait gui/501/{PREFIX}.sync"),
				format!("bootout --wait gui/501/{gateway}"),
			]
		);
		fs::remove_file(manager.loaded_definition(Job::Gateway)).unwrap();
		manager.start(Job::Gateway).unwrap_err();
		assert!(manager.linger().is_err());
		assert!(
			manager
				.restore_enablement(Job::Gateway, Enablement::Runtime)
				.is_err()
		);
	}

	#[test]
	fn transitional_states_fail_deploy_but_not_synchronization() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> = manager(&host, &settings, root.path());
		let gateway: &str = manager.label(Job::Gateway);
		stage(&manager, root.path());
		manager.activate_all().unwrap();
		host.set_state(gateway, "spawn scheduled");
		assert!(!LaunchdService { manager: &manager }.active().unwrap());
		for error in [
			manager.state(Job::Gateway).unwrap_err(),
			manager.check_available(true).unwrap_err(),
		] {
			assert!(
				format!("{error:#}").contains(&format!("launchctl bootout gui/501/{gateway}")),
				"{error:#}"
			);
		}
		host.unload(gateway);
		assert!(!LaunchdService { manager: &manager }.active().unwrap());
		let foreign: PathBuf = root.path().join(manager.definition(Job::Gateway));
		fs::copy(manager.loaded_definition(Job::Gateway), &foreign).unwrap();
		host.run(
			&["bootstrap", "gui/501", foreign.to_str().unwrap()],
			LAUNCHCTL_TIMEOUT,
		)
		.unwrap();
		assert!(LaunchdService { manager: &manager }.active().is_err());
	}

	#[test]
	fn activation_requires_a_desktop_session_and_a_free_port() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> = manager(&host, &settings, root.path());
		host.no_session.set(true);
		host.port_busy.set(true);
		// Install-only stages plists, which needs neither.
		manager.check_available(false).unwrap();
		assert!(
			manager
				.check_available(true)
				.unwrap_err()
				.to_string()
				.contains("No GUI login session")
		);
		host.no_session.set(false);
		assert!(
			manager
				.check_available(true)
				.unwrap_err()
				.to_string()
				.contains("lsof -nP -iTCP:4000 -sTCP:LISTEN")
		);
		host.port_busy.set(false);
		manager.check_available(true).unwrap();
		stage(&manager, root.path());
		manager.activate_all().unwrap();
		// The running gateway holds its own port.
		host.port_busy.set(true);
		manager.check_available(true).unwrap();
		assert!(
			host.take_log()
				.iter()
				.all(|command| command.starts_with("bootstrap"))
		);
		assert_eq!(
			device(&root.path().join("a/b")).unwrap(),
			device(root.path()).unwrap()
		);
	}

	#[test]
	fn stop_and_disable_attempt_every_job() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> = manager(&host, &settings, root.path());
		stage(&manager, root.path());
		manager.activate_all().unwrap();
		host.take_log();
		host.fault.set(Some(fixture::Fault::before("bootout", 1)));
		assert!(manager.stop_all().is_err());
		assert_eq!(
			host.take_log(),
			[
				format!("bootout --wait gui/501/{PREFIX}.sync"),
				format!("bootout --wait gui/501/{PREFIX}.gateway"),
			]
		);
		assert_eq!(manager.status(Job::Gateway).unwrap(), Status::Unloaded);
		assert_eq!(manager.status(Job::Sync).unwrap(), Status::Idle);
		let blocked: PathBuf = manager.loaded_definition(Job::Gateway);
		fs::remove_file(&blocked).unwrap();
		fs::create_dir(&blocked).unwrap();
		assert!(manager.disable_all().is_err());
		assert!(!manager.loaded_definition(Job::Sync).exists());
	}

	#[test]
	fn foreign_or_orphaned_loads_fail_closed() {
		let root: tempfile::TempDir = tempfile::tempdir().unwrap();
		let settings: Settings = crate::settings::test_settings(root.path());
		let host: LaunchdFixture = LaunchdFixture::default();
		let manager: LaunchdManager<'_, LaunchdFixture> = manager(&host, &settings, root.path());
		let foreign: PathBuf = root.path().join(manager.definition(Job::Gateway));
		let plists: BTreeMap<String, String> =
			manager.render(Path::new("/bin/trapi2litellm")).unwrap();
		fs::write(&foreign, &plists[manager.definition(Job::Gateway)]).unwrap();
		host.run(
			&["bootstrap", "gui/501", foreign.to_str().unwrap()],
			LAUNCHCTL_TIMEOUT,
		)
		.unwrap();
		assert!(manager.status(Job::Gateway).is_err());
		host.run(
			&["bootout", "--wait", &manager.target(Job::Gateway)],
			BOOTOUT_TIMEOUT,
		)
		.unwrap();
		let agent: PathBuf = manager.loaded_definition(Job::Gateway);
		fs::create_dir_all(agent.parent().unwrap()).unwrap();
		fs::write(&agent, &plists[manager.definition(Job::Gateway)]).unwrap();
		manager.restart(Job::Gateway).unwrap();
		host.run(
			&["kill", "SIGHUP", &manager.target(Job::Gateway)],
			LAUNCHCTL_TIMEOUT,
		)
		.unwrap();
		fs::remove_file(&agent).unwrap();
		assert!(manager.state(Job::Gateway).is_err());
	}
}
