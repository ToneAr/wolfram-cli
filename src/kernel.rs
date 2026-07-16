use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};
use wolfram_app_discovery::WolframApp;

use crate::{
    native_wstp,
    profiler::profile_duration,
    theme::{Theme, ThemeHandle},
    wl::{
        EVALUATE_SCRIPT_SOURCE_WL, wolfram_function_call, wolfram_string_list,
        wolfram_string_literal,
    },
};

#[derive(Debug, Clone)]
pub(crate) enum KernelConnection {
    Launch {
        link_options: Option<u32>,
        link_mode: Option<String>,
    },
    Connect {
        link_name: String,
        link_protocol: native_wstp::LinkProtocol,
        link_options: Option<u32>,
        link_init_directory: Option<PathBuf>,
        link_mode: Option<String>,
    },
}

pub(crate) type SharedKernel = Arc<Mutex<KernelClient>>;

#[derive(Debug, Clone)]
pub(crate) struct WolframVersions {
    pub(crate) kernel: String,
    pub(crate) wolframscript: String,
}

#[derive(Debug)]
pub(crate) struct KernelExit {
    pub(crate) code: i32,
}

impl KernelExit {
    pub(crate) fn new(code: i32) -> Self {
        Self { code }
    }
}

impl fmt::Display for KernelExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "kernel requested process exit with code {}", self.code)
    }
}

impl Error for KernelExit {}

pub(crate) fn lock_kernel(
    kernel: &SharedKernel,
) -> Result<std::sync::MutexGuard<'_, KernelClient>> {
    kernel
        .lock()
        .map_err(|_| anyhow!("kernel session lock was poisoned"))
}

pub(crate) fn wolfram_versions() -> WolframVersions {
    WolframVersions {
        kernel: wolfram_kernel_version().unwrap_or_else(|_| "unavailable".to_string()),
        wolframscript: wolframscript_version().unwrap_or_else(|_| "unavailable".to_string()),
    }
}

fn wolfram_kernel_version() -> Result<String> {
    let mut command = Command::new(kernel_path()?);
    command.arg("-version");
    command_version(command, "WolframKernel")
}

fn wolframscript_version() -> Result<String> {
    let mut command = Command::new("wolframscript");
    command.arg("-version");
    command_version(command, "wolframscript")
}

fn command_version(mut command: Command, name: &str) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("failed to launch {name} for version detection"))?;

    if !output.status.success() {
        bail!("{name} version detection exited with {}", output.status);
    }

    first_output_line(&output.stdout)
        .or_else(|| first_output_line(&output.stderr))
        .with_context(|| format!("{name} did not print a version"))
}

fn first_output_line(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) struct KernelClient {
    wstp: native_wstp::WstpKernelClient,
    active: Arc<AtomicBool>,
    ready: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScriptInvocation {
    File,
    Direct,
}

#[derive(Clone, Copy)]
pub(crate) enum KernelStatus {
    Active,
    StartingWstp,
    ReadyWstp,
}

impl fmt::Display for KernelStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::StartingWstp => f.write_str("starting/WSTP"),
            Self::ReadyWstp => f.write_str("ready/WSTP"),
        }
    }
}

struct ActivityGuard {
    active: Arc<AtomicBool>,
}

impl ActivityGuard {
    fn new(active: Arc<AtomicBool>) -> Self {
        active.store(true, Ordering::Relaxed);
        Self { active }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

impl KernelClient {
    pub(crate) fn with_connection(connection: KernelConnection) -> Result<Self> {
        let wstp = match connection {
            KernelConnection::Launch {
                link_options,
                link_mode,
            } => native_wstp::WstpKernelClient::launch(link_options, link_mode.as_deref())?,
            KernelConnection::Connect {
                link_name,
                link_protocol,
                link_options,
                link_init_directory,
                link_mode,
            } => {
                let mut client = native_wstp::WstpKernelClient::connect(
                    &link_name,
                    link_protocol,
                    link_options,
                    link_mode.as_deref(),
                )?;
                if let Some(directory) = link_init_directory {
                    client.initialize_current_directory(&directory)?;
                }
                client
            }
        };

        Ok(Self {
            wstp,
            active: Arc::new(AtomicBool::new(false)),
            ready: false,
        })
    }

    pub(crate) fn evaluate_once(&mut self, input: &str, use_color: bool) -> Result<()> {
        let theme = (!use_color).then(|| ThemeHandle::builtin(Theme::plain()));
        self.evaluate_text(input, theme.as_ref())
    }

    pub(crate) fn evaluate_file(
        &mut self,
        file: &Path,
        script_args: &[OsString],
        invocation: ScriptInvocation,
        use_color: bool,
    ) -> Result<()> {
        let source = read_script_source(file)?;
        let script_command_line = script_command_line(file, script_args)?;
        let evaluation_environment = script_evaluation_environment(invocation);
        let input_file_name = script_input_file_name(file)?;
        let input = wolfram_function_call(
            EVALUATE_SCRIPT_SOURCE_WL,
            &[
                wolfram_string_literal(&source),
                wolfram_string_list(&script_command_line),
                evaluation_environment,
                wolfram_string_literal(&input_file_name),
            ],
        );
        let theme = (!use_color).then(|| ThemeHandle::builtin(Theme::plain()));

        self.evaluate_text(&input, theme.as_ref())
    }

    pub(crate) fn status(&self) -> KernelStatus {
        if self.active.load(Ordering::Relaxed) {
            return KernelStatus::Active;
        }

        if self.ready {
            KernelStatus::ReadyWstp
        } else {
            KernelStatus::StartingWstp
        }
    }

    pub(crate) fn input_prompt(&self) -> Option<String> {
        self.wstp.input_prompt().map(ToOwned::to_owned)
    }

    /// Polls packets generated independently of the active evaluation, such
    /// as `TaskObject` output. The REPL calls this while it is idle.
    pub(crate) fn drain_out_of_band_packets(&mut self, theme: &ThemeHandle) -> Result<String> {
        self.wstp.drain_out_of_band_packets(Some(theme))
    }

    /// Waits until the kernel has sent its first REPL prompt.
    pub(crate) fn initialize_repl(&mut self) -> Result<()> {
        self.wstp.ensure_initial_prompt_read()?;
        self.wstp.ensure_secondary_links()?;
        self.ready = true;
        Ok(())
    }

    pub(crate) fn evaluate_repl_input(
        &mut self,
        input: &str,
        theme: &ThemeHandle,
        input_handler: &mut dyn FnMut(&native_wstp::KernelInputRequest) -> Result<Option<String>>,
        show_output_prompt: bool,
    ) -> Result<()> {
        self.evaluate(
            input,
            Some(theme),
            Some(input_handler),
            true,
            show_output_prompt,
        )
    }

    fn evaluate(
        &mut self,
        input: &str,
        theme: Option<&ThemeHandle>,
        input_handler: Option<
            &mut dyn FnMut(&native_wstp::KernelInputRequest) -> Result<Option<String>>,
        >,
        separate_input_and_output: bool,
        show_output_prompt: bool,
    ) -> Result<()> {
        let _activity = ActivityGuard::new(self.active.clone());
        self.wstp.evaluate_once(
            input,
            theme,
            input_handler,
            separate_input_and_output,
            show_output_prompt,
        )?;
        self.ready = true;
        Ok(())
    }

    fn evaluate_text(&mut self, input: &str, theme: Option<&ThemeHandle>) -> Result<()> {
        let _activity = ActivityGuard::new(self.active.clone());
        self.wstp.evaluate_text_once(input, theme)?;
        self.ready = true;
        Ok(())
    }

    pub(crate) fn query_lines(&mut self, code: &str) -> Result<Vec<String>> {
        let start = Instant::now();
        let output = self.query_string(code)?;
        let lines: Vec<String> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        profile_duration(
            "kernel.query_lines",
            start.elapsed(),
            format!("lines={} code_len={}", lines.len(), code.len()),
        );
        Ok(lines)
    }

    pub(crate) fn query_string(&mut self, code: &str) -> Result<String> {
        let _activity = ActivityGuard::new(self.active.clone());
        let start = Instant::now();
        let output = self.wstp.evaluate_to_string(code)?;
        self.ready = true;
        profile_duration(
            "kernel.query_string.wstp",
            start.elapsed(),
            format!("bytes={} code_len={}", output.len(), code.len()),
        );
        Ok(output)
    }
}

fn read_script_source(file: &Path) -> Result<String> {
    let source = fs::read_to_string(file)
        .with_context(|| format!("failed to read script file {}", file.display()))?;
    Ok(strip_shebang_preserving_line_numbers(source))
}

fn strip_shebang_preserving_line_numbers(mut source: String) -> String {
    if !source.starts_with("#!") {
        return source;
    }

    if let Some(newline) = source.find('\n') {
        source.replace_range(..newline, "");
        source
    } else {
        String::new()
    }
}

fn script_command_line(file: &Path, script_args: &[OsString]) -> Result<Vec<String>> {
    let mut command_line = Vec::with_capacity(script_args.len() + 1);
    command_line.push(os_string_to_wolfram_string(
        file.as_os_str(),
        "script file path",
    )?);
    for arg in script_args {
        command_line.push(os_string_to_wolfram_string(arg, "script argument")?);
    }
    Ok(command_line)
}

fn script_evaluation_environment(invocation: ScriptInvocation) -> String {
    match invocation {
        ScriptInvocation::File => "None".to_string(),
        ScriptInvocation::Direct => wolfram_string_literal("Script"),
    }
}

fn script_input_file_name(file: &Path) -> Result<String> {
    let absolute_file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        env::current_dir()
            .context("failed to determine current directory for script input file name")?
            .join(file)
    };
    os_string_to_wolfram_string(absolute_file.as_os_str(), "script input file name")
}

fn os_string_to_wolfram_string(value: &std::ffi::OsStr, label: &str) -> Result<String> {
    value
        .to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("{label} is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::{
        ScriptInvocation, script_command_line, script_evaluation_environment,
        script_input_file_name, strip_shebang_preserving_line_numbers,
    };
    use std::{env, ffi::OsString, path::Path};

    #[test]
    fn strips_shebang_without_changing_following_line_numbers() {
        let source = "#!/usr/bin/wolfie\nx = 2 + 2\n\ny\n".to_string();

        assert_eq!(
            strip_shebang_preserving_line_numbers(source),
            "\nx = 2 + 2\n\ny\n"
        );
    }

    #[test]
    fn script_command_line_includes_file_and_script_args() {
        let command_line = script_command_line(
            Path::new("script.wl"),
            &[OsString::from("first"), OsString::from("--second")],
        )
        .expect("UTF-8 script command line should convert");

        assert_eq!(
            command_line,
            vec![
                "script.wl".to_string(),
                "first".to_string(),
                "--second".to_string(),
            ]
        );
    }

    #[test]
    fn direct_script_evaluation_environment_is_script() {
        assert_eq!(
            script_evaluation_environment(ScriptInvocation::Direct),
            "\"Script\""
        );
    }

    #[test]
    fn script_input_file_name_is_absolute_script_file_path() {
        let script_file = env::current_dir()
            .expect("test current directory should be available")
            .join("wolfie")
            .join("script.wl");
        let input_file_name =
            script_input_file_name(&script_file).expect("absolute script path should convert");

        assert_eq!(input_file_name, script_file.to_str().unwrap());
    }

    #[test]
    fn file_script_evaluation_environment_is_not_overridden() {
        assert_eq!(
            script_evaluation_environment(ScriptInvocation::File),
            "None"
        );
    }
}

static KERNEL_PATH: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn kernel_path() -> Result<PathBuf> {
    Ok(KERNEL_PATH.get_or_init(discover_kernel_path).clone())
}

fn discover_kernel_path() -> PathBuf {
    if let Some(path) = env::var_os("WOLFRAM_KERNEL") {
        return PathBuf::from(path);
    }

    if let Ok(install_dir) = wolfram_installation_directory() {
        if let Some(candidate) = native_kernel_path(&install_dir)
            && candidate.exists()
        {
            return candidate;
        }

        let candidate = install_dir.join("Executables").join(kernel_binary_name());
        if candidate.exists() {
            return candidate;
        }
    }

    PathBuf::from(kernel_binary_name())
}

fn native_kernel_path(install_dir: &Path) -> Option<PathBuf> {
    let platform = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "Linux-x86-64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "Linux-ARM64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "MacOSX-x86-64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "MacOSX-ARM64"
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        "Windows-x86-64"
    } else {
        return None;
    };

    Some(
        install_dir
            .join("SystemFiles")
            .join("Kernel")
            .join("Binaries")
            .join(platform)
            .join(kernel_binary_name()),
    )
}

fn kernel_binary_name() -> &'static str {
    if cfg!(windows) {
        "WolframKernel.exe"
    } else {
        "WolframKernel"
    }
}

fn wolfram_installation_directory() -> Result<PathBuf> {
    if let Ok(path) = wolframscript_showkernels_installation_directory() {
        return Ok(path);
    }

    WolframApp::try_default()
        .map(|app| app.installation_directory())
        .context("failed to discover Wolfram installation")
}

fn wolframscript_showkernels_installation_directory() -> Result<PathBuf> {
    let output = Command::new("wolframscript")
        .arg("-showkernels")
        .output()
        .context("failed to launch wolframscript for installation discovery")?;

    if !output.status.success() {
        bail!(
            "wolframscript installation discovery exited with {}",
            output.status
        );
    }

    let stdout =
        String::from_utf8(output.stdout).context("wolframscript returned invalid UTF-8")?;
    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let path = PathBuf::from(line);
        if path
            .file_name()
            .is_some_and(|name| name == kernel_binary_name())
            && path.parent().and_then(Path::file_name) == Some("Executables".as_ref())
        {
            return path
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .context("wolframscript returned a kernel path without an installation directory");
        }
    }

    bail!("wolframscript -showkernels did not return a WolframKernel path")
}

pub(crate) fn kernel_status(kernel: &SharedKernel) -> Result<KernelStatus> {
    match kernel.try_lock() {
        Ok(kernel) => Ok(kernel.status()),
        Err(std::sync::TryLockError::WouldBlock) => Ok(KernelStatus::Active),
        Err(std::sync::TryLockError::Poisoned(_)) => {
            Err(anyhow!("kernel session lock was poisoned"))
        }
    }
}

pub(crate) fn kernel_input_prompt(kernel: &SharedKernel) -> Result<Option<String>> {
    match kernel.try_lock() {
        Ok(kernel) => Ok(kernel.input_prompt()),
        Err(std::sync::TryLockError::WouldBlock) => Ok(None),
        Err(std::sync::TryLockError::Poisoned(_)) => {
            Err(anyhow!("kernel session lock was poisoned"))
        }
    }
}

/// Whether evaluating right now might sit silently for a while: the kernel is
/// still finishing its own startup before the first successful WSTP round trip,
/// or another thread (e.g. the background warm-up or a completion query) is
/// currently occupying the only kernel session.
pub(crate) fn kernel_may_be_slow_to_respond(status: KernelStatus) -> bool {
    match status {
        KernelStatus::Active | KernelStatus::StartingWstp => true,
        KernelStatus::ReadyWstp => false,
    }
}

/// Launches the WSTP kernel's first real evaluation in the background as
/// soon as the REPL starts, instead of leaving it for the user's first
/// submitted input. The kernel process and link are already created by this
/// point (`KernelClient::with_connection` did that cheaply); what remains is the slow
/// first prompt/readiness handshake plus first-time evaluation setup. Doing
/// that here overlaps it with the time the user spends reading the banner and
/// typing their first command, instead of after they press Enter.
pub(crate) fn spawn_kernel_warmup(kernel: SharedKernel) {
    std::thread::spawn(move || {
        if let Ok(mut client) = lock_kernel(&kernel) {
            let _ = client.query_string("Null");
        }
    });
}
