use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, fs,
    io::{self, BufRead, Write},
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
    commands::top_level_run_exit_code,
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
        if !self.evaluate_top_level_run(input, theme.as_ref())? {
            let mut input_handler =
                |request: &native_wstp::KernelInputRequest| read_terminal_input(request);
            self.evaluate_text(input, theme.as_ref(), Some(&mut input_handler))?;
        }
        Ok(())
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

        let mut input_handler =
            |request: &native_wstp::KernelInputRequest| read_terminal_input(request);
        self.evaluate_script(
            &input,
            theme.as_ref(),
            Some(&mut input_handler),
            false,
            false,
        )
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

    fn evaluate_script(
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
        self.wstp.evaluate_script_once(
            input,
            theme,
            input_handler,
            separate_input_and_output,
            show_output_prompt,
        )?;
        self.ready = true;
        Ok(())
    }

    fn evaluate_top_level_run(&mut self, input: &str, theme: Option<&ThemeHandle>) -> Result<bool> {
        let Some(exit_code) = top_level_run_exit_code(input)? else {
            return Ok(false);
        };
        self.evaluate_text(&exit_code.to_string(), theme, None)?;
        Ok(true)
    }

    fn evaluate_text(
        &mut self,
        input: &str,
        theme: Option<&ThemeHandle>,
        input_handler: Option<
            &mut dyn FnMut(&native_wstp::KernelInputRequest) -> Result<Option<String>>,
        >,
    ) -> Result<()> {
        let _activity = ActivityGuard::new(self.active.clone());
        self.wstp.evaluate_text_once(input, theme, input_handler)?;
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

fn read_terminal_input(request: &native_wstp::KernelInputRequest) -> Result<Option<String>> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    read_input_from_streams(request, &mut stdin.lock(), &mut stdout.lock())
}

fn read_input_from_streams<R: BufRead, W: Write>(
    request: &native_wstp::KernelInputRequest,
    input_reader: &mut R,
    output: &mut W,
) -> Result<Option<String>> {
    if !request.prompt.is_empty() {
        write!(output, "{}", request.prompt).context("failed to write input prompt")?;
        output.flush().context("failed to flush input prompt")?;
    }

    let mut input = String::new();
    if input_reader
        .read_line(&mut input)
        .context("failed to read input")?
        == 0
    {
        return Ok(None);
    }

    Ok(Some(input.trim_end_matches(['\r', '\n']).to_string()))
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
        ScriptInvocation, kernel_binary_name, kernel_path_in_installation,
        read_input_from_streams, reported_kernel_installation_directory, script_command_line,
        script_evaluation_environment, script_input_file_name, showkernels_kernel_path_from_output,
        strip_shebang_preserving_line_numbers,
    };
    use crate::native_wstp::{KernelInputKind, KernelInputRequest};
    use std::{
        env,
        ffi::OsString,
        fs,
        io::Cursor,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn terminal_input_writes_prompt_and_strips_line_ending() {
        let request = KernelInputRequest {
            kind: KernelInputKind::Expression,
            prompt: "> ".to_string(),
        };
        let mut input = Cursor::new(b"1 + 1\r\n");
        let mut output = Vec::new();

        let response = read_input_from_streams(&request, &mut input, &mut output)
            .expect("terminal input should be read");

        assert_eq!(response, Some("1 + 1".to_string()));
        assert_eq!(output, b"> ");
    }

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

    #[test]
    fn showkernels_accepts_wolfram_executable_from_windows_product_installation() {
        let output = r#"
Among all detected Wolfram product installations, the best wolfram.exe location is the following:
        C:/Program Files/Wolfram Research/Wolfram/15.0 New/wolfram.exe

Among all detected Wolfram Engine installations, the best wolfram.exe location is the following:
"#;

        assert_eq!(
            showkernels_kernel_path_from_output(output),
            Some(PathBuf::from(
                "C:/Program Files/Wolfram Research/Wolfram/15.0 New/wolfram.exe"
            ))
        );
    }

    #[test]
    fn showkernels_prefers_explicit_wolframkernel_over_product_launcher() {
        let output = r#"
C:/Program Files/Wolfram Research/Wolfram/15.0/wolfram.exe
C:/Program Files/Wolfram Research/Wolfram Engine/15.0/WolframKernel.exe
"#;

        assert_eq!(
            showkernels_kernel_path_from_output(output),
            Some(PathBuf::from(
                "C:/Program Files/Wolfram Research/Wolfram Engine/15.0/WolframKernel.exe"
            ))
        );
    }

    #[test]
    fn reported_windows_kernel_uses_its_parent_as_installation_directory() {
        let kernel = Path::new("C:/Program Files/Wolfram Research/Wolfram/15.0 New/wolfram.exe");

        assert_eq!(
            reported_kernel_installation_directory(kernel),
            Some(PathBuf::from(
                "C:/Program Files/Wolfram Research/Wolfram/15.0 New"
            ))
        );
    }

    #[test]
    fn reported_executables_kernel_uses_parent_installation_directory() {
        let kernel = Path::new("/usr/local/Wolfram/WolframEngine/15.0/Executables/WolframKernel");

        assert_eq!(
            reported_kernel_installation_directory(kernel),
            Some(PathBuf::from("/usr/local/Wolfram/WolframEngine/15.0"))
        );
    }

    #[test]
    fn kernel_discovery_checks_the_installation_root() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let install_dir = env::temp_dir().join(format!(
            "wolfie-kernel-discovery-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&install_dir).expect("test installation directory should be created");
        let kernel = install_dir.join(kernel_binary_name());
        fs::write(&kernel, []).expect("test kernel should be created");

        assert_eq!(kernel_path_in_installation(&install_dir), Some(kernel));

        fs::remove_dir_all(install_dir).expect("test installation directory should be removed");
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

    if let Ok(reported_kernel) = wolframscript_showkernels_kernel_path() {
        if let Some(install_dir) = reported_kernel_installation_directory(&reported_kernel)
            && let Some(candidate) = kernel_path_in_installation(&install_dir)
        {
            return candidate;
        }

        if reported_kernel.exists() {
            return reported_kernel;
        }
    }

    if let Ok(app) = WolframApp::try_default()
        && let Some(candidate) = kernel_path_in_installation(&app.installation_directory())
    {
        return candidate;
    }

    PathBuf::from(kernel_binary_name())
}

fn kernel_path_in_installation(install_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        native_kernel_path(install_dir),
        Some(install_dir.join(kernel_binary_name())),
        Some(install_dir.join("Executables").join(kernel_binary_name())),
    ];

    candidates.into_iter().flatten().find(|path| path.exists())
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

fn wolframscript_showkernels_kernel_path() -> Result<PathBuf> {
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

    let stdout = String::from_utf8(output.stdout)
        .context("wolframscript returned invalid UTF-8 on stdout")?;
    let stderr = String::from_utf8(output.stderr)
        .context("wolframscript returned invalid UTF-8 on stderr")?;

    showkernels_kernel_path_from_output(&stdout)
        .or_else(|| showkernels_kernel_path_from_output(&stderr))
        .context("wolframscript -showkernels did not return a Wolfram kernel path")
}

fn showkernels_kernel_path_from_output(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .map(str::trim)
        .map(|line| line.trim_matches('"'))
        .filter_map(|line| {
            let path = PathBuf::from(line);
            let name = path.file_name()?.to_str()?;
            let priority = if name.eq_ignore_ascii_case("WolframKernel")
                || name.eq_ignore_ascii_case("WolframKernel.exe")
            {
                0
            } else if name.eq_ignore_ascii_case("wolfram")
                || name.eq_ignore_ascii_case("wolfram.exe")
            {
                1
            } else {
                return None;
            };
            Some((priority, path))
        })
        .min_by_key(|(priority, _)| *priority)
        .map(|(_, path)| path)
}

fn reported_kernel_installation_directory(kernel_path: &Path) -> Option<PathBuf> {
    let parent = kernel_path.parent()?;
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("Executables"))
    {
        parent.parent().map(Path::to_path_buf)
    } else {
        Some(parent.to_path_buf())
    }
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
