use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
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
};

pub(crate) type SharedKernel = Arc<Mutex<KernelClient>>;

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

pub(crate) fn run_wolframscript_file(file: PathBuf, script_args: Vec<OsString>) -> Result<()> {
    let status = Command::new("wolframscript")
        .arg("-file")
        .arg(file)
        .args(script_args)
        .status()
        .context("failed to launch wolframscript")?;

    if !status.success() {
        if let Some(code) = status.code() {
            return Err(KernelExit::new(code).into());
        }
        bail!("wolframscript exited with {status}");
    }
    Ok(())
}

pub(crate) struct KernelClient {
    wstp: native_wstp::WstpKernelClient,
    active: Arc<AtomicBool>,
    ready: bool,
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
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            wstp: native_wstp::WstpKernelClient::launch()?,
            active: Arc::new(AtomicBool::new(false)),
            ready: false,
        })
    }

    pub(crate) fn evaluate_once(&mut self, input: &str, use_color: bool) -> Result<()> {
        let theme = (!use_color).then(|| ThemeHandle::new(Theme::Plain));
        self.evaluate(input, theme.as_ref(), None)
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

    pub(crate) fn evaluate_repl_input(
        &mut self,
        input: &str,
        theme: &ThemeHandle,
        input_handler: &mut dyn FnMut(&native_wstp::KernelInputRequest) -> Result<Option<String>>,
    ) -> Result<()> {
        self.evaluate(input, Some(theme), Some(input_handler))
    }

    fn evaluate(
        &mut self,
        input: &str,
        theme: Option<&ThemeHandle>,
        input_handler: Option<
            &mut dyn FnMut(&native_wstp::KernelInputRequest) -> Result<Option<String>>,
        >,
    ) -> Result<()> {
        let _activity = ActivityGuard::new(self.active.clone());
        self.wstp.evaluate_once(input, theme, input_handler)?;
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

pub(crate) fn kernel_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("WOLFRAM_KERNEL") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(install_dir) = wolfram_installation_directory() {
        if let Some(candidate) = native_kernel_path(&install_dir)
            && candidate.exists() {
                return Ok(candidate);
            }

        let candidate = install_dir.join("Executables").join(kernel_binary_name());
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Ok(PathBuf::from(kernel_binary_name()))
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
/// point (`KernelClient::new` did that cheaply); what remains is the slow
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
