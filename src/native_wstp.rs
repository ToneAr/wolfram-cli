use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use nu_ansi_term::Color;
use wolfram_expr::{Expr, ExprKind, Symbol};
pub(crate) use wstp::Protocol as LinkProtocol;
use wstp::{Link, UrgentMessage, sys};

use crate::{
    interrupt,
    kernel::{KernelExit, kernel_path},
    profiler::{profile_duration, profile_duration_with, profile_event, profile_event_with},
    theme::ThemeHandle,
    wl::{WSTP_EVALUATE_USER_INPUT_WL, wolfram_function_call, wolfram_string_literal},
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum KernelInputKind {
    Expression,
    String,
}

#[derive(Debug, Clone)]
pub(crate) struct KernelInputRequest {
    pub(crate) kind: KernelInputKind,
    pub(crate) prompt: String,
}

#[derive(Debug)]
enum KernelPacket {
    BeginDialog(i32),
    Call { function: i32, args: Expr },
    DisplayEnd,
    Display,
    EndDialog(i32),
    EnterExpression(Expr),
    EnterText(String),
    Evaluate(Expr),
    InputName(String),
    Input,
    InputString,
    Menu { id: i32, title: String },
    Message { symbol: String, tag: String },
    OutputName(String),
    Resume,
    Return(Expr),
    ReturnExpression(Expr),
    ReturnText(String),
    Suspend,
    Syntax(i32),
    Text(String),
    Unknown(i32),
}

type KernelInputHandler<'a> = dyn FnMut(&KernelInputRequest) -> Result<Option<String>> + 'a;

const LOADING_TEXT_FRAMES: [&str; 10] = [
	"Evaluating",
	"Evaluating.",
	"Evaluating.",
	"Evaluating.",
	"Evaluating..",
	"Evaluating..",
	"Evaluating...",
	"Evaluating...",
	"Evaluating...",
	"Evaluating",
];
const LOADING_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOADING_FRAME_INTERVAL: Duration = Duration::from_millis(80);

/// Shows progress while a terminal evaluation is waiting for the kernel.
///
/// The worker is stopped before streamed kernel text is written, preventing
/// its terminal writes from interleaving with kernel output.
struct LoadingIndicator {
    running: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl LoadingIndicator {
    fn start(theme: Option<&ThemeHandle>) -> Option<Self> {
        if !io::stdout().is_terminal() {
            return None;
        }
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let worker_theme = theme.cloned();

        let worker = thread::spawn(move || {
            let mut frame = 0;
            let _ = write!(io::stdout(), "\x1b[?25l");
            while worker_running.load(Ordering::Relaxed) {
                let _ = match &worker_theme {
                    Some(theme) => write!(
                        io::stdout(),
                        "\r\x1b[2K{} {}",
                        theme.current().styles().prompt_left.paint(LOADING_FRAMES[frame]),
                        nu_ansi_term::Color::DarkGray.paint(LOADING_TEXT_FRAMES[frame]),
                    ),
                    None => write!(
                        io::stdout(),
                        "\r\x1b[2K   {} {}",
                        LOADING_FRAMES[frame],
                        LOADING_TEXT_FRAMES[frame],
                    ),
                };
                let _ = io::stdout().flush();
                frame = (frame + 1) % LOADING_FRAMES.len();
                thread::sleep(LOADING_FRAME_INTERVAL);
            }
            let _ = write!(io::stdout(), "\x1b[?25h\r\x1b[2K");
            let _ = io::stdout().flush();
        });

        Some(Self {
            running,
            worker: Some(worker),
        })
    }
}

impl Drop for LoadingIndicator {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = write!(io::stdout(), "\r\x1b[2K\r");
        let _ = io::stdout().flush();
    }
}

/// Owns the evaluation indicator so it can be hidden for output or input and
/// resumed below completed lines of streamed kernel output.
struct EvaluationLoadingIndicator {
    indicator: Option<LoadingIndicator>,
    theme: Option<ThemeHandle>,
}

impl EvaluationLoadingIndicator {
    fn start(theme: Option<&ThemeHandle>) -> Self {
        Self {
            indicator: LoadingIndicator::start(theme),
            theme: theme.cloned(),
        }
    }

    fn hide(&mut self) {
        drop(self.indicator.take());
    }

    fn show_below_text(&mut self, text: &str) {
        if text.ends_with('\n') {
            self.indicator = LoadingIndicator::start(self.theme.as_ref());
        }
    }
}

fn print_kernel_text(text: &str) -> Result<()> {
    print!("{text}");
    io::stdout().flush().context("failed to flush stdout")
}

fn print_kernel_message_text(
    text: &str,
    symbol: &str,
    tag: &str,
    theme: Option<&ThemeHandle>,
) -> Result<()> {
    print_kernel_text(&render_message_text_with_color(
        text,
        symbol,
        tag,
        message_identifier_color_enabled(theme),
    ))
}

fn print_startup_kernel_message_text(
    text: &str,
    symbol: &str,
    tag: &str,
    begin_on_new_line: bool,
) -> Result<()> {
    print_kernel_text(&render_startup_message_text(
        text,
        symbol,
        tag,
        begin_on_new_line,
    ))
}

fn message_identifier_color_enabled(theme: Option<&ThemeHandle>) -> bool {
    color_enabled(theme)
}

fn output_name_color_enabled(theme: Option<&ThemeHandle>) -> bool {
    color_enabled(theme)
}

fn color_enabled(theme: Option<&ThemeHandle>) -> bool {
    theme.is_none_or(|theme| !theme.current().is_plain())
}

fn render_message_text_with_color(text: &str, symbol: &str, tag: &str, use_color: bool) -> String {
    let Some((prefix, identifier, rest)) = message_text_identifier(text, symbol, tag) else {
        return text.to_owned();
    };

    if use_color {
        format!("{}{}{}\n", prefix, Color::Red.paint(identifier), rest)
    } else {
        text.to_owned()
    }
}

fn render_startup_message_text(
    text: &str,
    symbol: &str,
    tag: &str,
    begin_on_new_line: bool,
) -> String {
    let message = render_message_text_with_color(text, symbol, tag, false);
    let message = message.trim_end_matches(['\r', '\n']);
    let prefix = if begin_on_new_line { "\r\n" } else { "" };
    format!("{prefix}{message}\r\n")
}

fn message_text_identifier<'a>(
    text: &'a str,
    symbol: &str,
    tag: &str,
) -> Option<(&'a str, &'a str, &'a str)> {
    message_text_identifier_for_symbol(text, symbol, tag).or_else(|| {
        message_text_identifier_for_symbol(text, short_message_symbol_name(symbol), tag)
    })
}

fn message_text_identifier_for_symbol<'a>(
    text: &'a str,
    symbol: &str,
    tag: &str,
) -> Option<(&'a str, &'a str, &'a str)> {
    for line_start in message_line_starts(text) {
        let after_line_start = text.get(line_start..)?;
        let after_symbol = after_line_start.get(symbol.len()..)?;
        if !after_symbol.starts_with("::") {
            continue;
        }

        let identifier_end = line_start + symbol.len() + "::".len() + tag.len();
        let after_separator = text.get(line_start + symbol.len() + "::".len()..)?;
        if !after_separator.starts_with(tag) {
            continue;
        }

        let rest = text.get(identifier_end..)?;
        if rest.is_empty()
            || rest
                .chars()
                .next()
                .is_some_and(|ch| ch == ':' || ch.is_whitespace())
        {
            return Some((&text[..line_start], &text[line_start..identifier_end], rest));
        }
    }

    None
}

fn message_line_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        text.match_indices('\n')
            .map(|(idx, _)| idx + '\n'.len_utf8()),
    )
}

fn short_message_symbol_name(symbol: &str) -> &str {
    symbol.rsplit('`').next().unwrap_or(symbol)
}

enum KernelProcess {
    Launched(Child),
    External,
}

pub(crate) struct WstpKernelClient {
    process: KernelProcess,
    link: Option<Link>,
    input_prompt: Option<String>,
    initial_prompt_pending: bool,
    pending_current_directory: Option<PathBuf>,
}

impl WstpKernelClient {
    pub(crate) fn launch(link_options: Option<u32>, link_mode: Option<&str>) -> Result<Self> {
        let start = Instant::now();
        let path = kernel_path()?;
        let mut link = Link::listen(LinkProtocol::SharedMemory, "")
            .map_err(|err| anyhow!("failed to create WSTP listener: {err:?}"))?;
        let link_name = link.link_name();
        let spawn_start = Instant::now();
        let mut command = Command::new(path);
        configure_kernel_launch_command(&mut command, &link_name, link_options, link_mode);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        isolate_kernel_from_terminal_interrupts(&mut command);
        let process = command
            .spawn()
            .context("failed to launch WolframKernel in WSTP mode")?;
        profile_duration("wstp.launch.spawn", spawn_start.elapsed(), "");

        let activate_start = Instant::now();
        link.activate()
            .map_err(|err| anyhow!("failed to activate WSTP link: {err:?}"))?;
        profile_duration("wstp.launch.activate", activate_start.elapsed(), "");

        profile_duration("wstp.launch.total", start.elapsed(), "");

        Ok(Self {
            process: KernelProcess::Launched(process),
            link: Some(link),
            input_prompt: None,
            initial_prompt_pending: true,
            pending_current_directory: None,
        })
    }

    pub(crate) fn connect(
        link_name: &str,
        link_protocol: LinkProtocol,
        link_options: Option<u32>,
        link_mode: Option<&str>,
    ) -> Result<Self> {
        let start = Instant::now();
        let connect_start = Instant::now();
        let detail = format!("protocol={link_protocol:?} link_name={link_name}");
        let mut link = connect_link(link_protocol.clone(), link_name, link_options, link_mode)
            .map_err(|err| {
                anyhow!(
                    "failed to connect to WSTP link {link_name:?} using {link_protocol:?}: {err:?}"
                )
            })?;
        profile_duration("wstp.connect.open", connect_start.elapsed(), &detail);

        let activate_start = Instant::now();
        link.activate().map_err(|err| {
            anyhow!(
                "failed to activate connected WSTP link {link_name:?} using {link_protocol:?}: {err:?}"
            )
        })?;
        profile_duration("wstp.connect.activate", activate_start.elapsed(), &detail);
        profile_duration("wstp.connect.total", start.elapsed(), &detail);

        Ok(Self {
            process: KernelProcess::External,
            link: Some(link),
            input_prompt: None,
            initial_prompt_pending: true,
            pending_current_directory: None,
        })
    }

    pub(crate) fn evaluate_once(
        &mut self,
        input: &str,
        theme: Option<&ThemeHandle>,
        input_handler: Option<&mut KernelInputHandler<'_>>,
        separate_input_and_output: bool,
        show_output_prompt: bool,
    ) -> Result<()> {
        let previous_input_prompt = self.input_prompt.clone();
        let packets = self.evaluate_input_packets(input, input_handler, theme)?;
        let input_prompt =
            next_input_prompt_after_evaluation(previous_input_prompt.as_deref(), &packets);
        render_packets(
            &packets,
            theme,
            PacketRenderOptions {
                separate_input_and_output,
                show_output_prompt,
            },
        )?;
        if let Some(input_prompt) = input_prompt {
            self.input_prompt = Some(input_prompt);
        }
        Ok(())
    }

    pub(crate) fn evaluate_text_once(
        &mut self,
        input: &str,
        theme: Option<&ThemeHandle>,
    ) -> Result<()> {
        let packets = self.evaluate_text_packets(input, theme)?;
        render_packets(
            &packets,
            theme,
            PacketRenderOptions {
                separate_input_and_output: false,
                show_output_prompt: false,
            },
        )
    }

    pub(crate) fn input_prompt(&self) -> Option<&str> {
        self.input_prompt.as_deref()
    }

    fn ensure_initial_prompt_read(&mut self) -> Result<()> {
        if !self.initial_prompt_pending {
            return Ok(());
        }

        let start = Instant::now();
        let link = self.link.as_mut().context("WSTP link is closed")?;
        let input_prompt = read_initial_input_name_packet(link, &mut self.process)?;
        profile_duration(
            "wstp.initial_prompt",
            start.elapsed(),
            input_prompt.as_str(),
        );
        self.input_prompt = Some(input_prompt);
        self.initial_prompt_pending = false;
        Ok(())
    }

    /// Evaluates `input` and returns its textual result. Queries built from
    /// `StringRiffle`/`StringJoin` (as completion queries are) already
    /// evaluate to a `String`; re-wrapping that in `ToString[.., InputForm]`
    /// would double-encode it (quoting the string and escaping its tabs and
    /// newlines as literal `\t`/`\n` text), which callers that split on real
    /// tab/newline bytes then fail to parse. Only non-string results go
    /// through `ToString[.., InputForm]`.
    pub(crate) fn evaluate_to_string(&mut self, input: &str) -> Result<String> {
        let wrapped = wrap_to_string_query(input);
        let expr = call("System`ToExpression", vec![Expr::string(&wrapped)]);
        self.evaluate_packet_to_string(&expr)
    }

    pub(crate) fn initialize_current_directory(&mut self, directory: &Path) -> Result<()> {
        self.pending_current_directory = Some(directory.to_path_buf());
        profile_event(format!("wstp.linkinit.queued\tdirectory={directory:?}"));
        Ok(())
    }

    fn evaluate_input_packets(
        &mut self,
        input: &str,
        input_handler: Option<&mut KernelInputHandler<'_>>,
        theme: Option<&ThemeHandle>,
    ) -> Result<Vec<KernelPacket>> {
        self.ensure_initial_prompt_read()?;
        interrupt::clear_kernel_interrupt_request();
        let start = Instant::now();
        let input = self.user_input_text_with_pending_initialization(input)?;
        let link = self.link.as_mut().context("WSTP link is closed")?;
        put_enter_text_packet(link, &input)?;
        profile_duration("wstp.enter_text.sent", start.elapsed(), "");

        let mut loading = EvaluationLoadingIndicator::start(theme);
        let mut stream_text = |text: &str, message: Option<(&str, &str)>| {
            if let Some((symbol, tag)) = message {
                print_kernel_message_text(text, symbol, tag, theme)
            } else {
                print_kernel_text(text)
            }
        };
        let mut stream_dialog_marker = |marker: &str| {
            print_kernel_text(&format!("{}\n", render_dialog_marker(marker, theme)))
        };
        let packets = read_packets_until_return(
            link,
            &mut self.process,
            input_handler,
            true,
            "WSTP EnterTextPacket evaluation",
            Some(&mut loading),
            Some(&mut stream_text),
            Some(&mut stream_dialog_marker),
        )?;
        drop(loading);
        profile_duration_with("wstp.enter_text.total", start.elapsed(), || {
            format!("bytes={}", packet_output_bytes(&packets))
        });
        Ok(packets)
    }

    fn evaluate_packet_to_string(&mut self, expr: &Expr) -> Result<String> {
        self.ensure_initial_prompt_read()?;
        interrupt::clear_kernel_interrupt_request();
        let start = Instant::now();
        let link = self.link.as_mut().context("WSTP link is closed")?;
        link.put_eval_packet(expr)
            .map_err(|err| anyhow!("failed to send WSTP EvaluatePacket: {err:?}"))?;
        link.flush()
            .map_err(|err| anyhow!("failed to flush WSTP link: {err:?}"))?;
        profile_duration("wstp.eval.sent", start.elapsed(), "");

        let packets = read_packets_until_return(
            link,
            &mut self.process,
            None,
            false,
            "WSTP EvaluatePacket query",
            None,
            None,
            None,
        )?;
        let text = packets
            .iter()
            .rev()
            .find_map(packet_text_result)
            .unwrap_or_default();
        profile_duration(
            "wstp.eval.total",
            start.elapsed(),
            format!("bytes={}", text.len()),
        );
        Ok(text)
    }

    fn evaluate_text_packets(
        &mut self,
        input: &str,
        theme: Option<&ThemeHandle>,
    ) -> Result<Vec<KernelPacket>> {
        self.ensure_initial_prompt_read()?;
        interrupt::clear_kernel_interrupt_request();
        let start = Instant::now();
        let input = self.user_input_text_with_pending_initialization(input)?;
        let wrapped = plain_text_result_input(&input);
        let expr = call("System`ToExpression", vec![Expr::string(&wrapped)]);
        let link = self.link.as_mut().context("WSTP link is closed")?;
        link.put_eval_packet(&expr)
            .map_err(|err| anyhow!("failed to send WSTP EvaluatePacket: {err:?}"))?;
        link.flush()
            .map_err(|err| anyhow!("failed to flush WSTP link: {err:?}"))?;
        profile_duration("wstp.eval_text.sent", start.elapsed(), "");

        let mut stream_text = |text: &str, message: Option<(&str, &str)>| {
            if let Some((symbol, tag)) = message {
                print_kernel_message_text(text, symbol, tag, theme)
            } else {
                print_kernel_text(text)
            }
        };
        let mut stream_dialog_marker = |marker: &str| {
            print_kernel_text(&format!("{}\n", render_dialog_marker(marker, theme)))
        };
        let packets = read_packets_until_return(
            link,
            &mut self.process,
            None,
            false,
            "WSTP EvaluatePacket text evaluation",
            None,
            Some(&mut stream_text),
            Some(&mut stream_dialog_marker),
        )?;
        profile_duration_with("wstp.eval_text.total", start.elapsed(), || {
            format!("bytes={}", packet_output_bytes(&packets))
        });
        Ok(packets)
    }

    fn user_input_text_with_pending_initialization(&mut self, input: &str) -> Result<String> {
        let input = wstp_user_input_text(input);
        let Some(directory) = self.pending_current_directory.take() else {
            return Ok(input);
        };
        let directory = directory.to_str().with_context(|| {
            format!("cannot initialize Wolfram kernel directory from non-UTF-8 path {directory:?}")
        })?;
        Ok(format!("{}; {input}", set_directory_expression(directory)))
    }

    fn child_exit_code_after_link_error(process: &mut KernelProcess) -> Option<i32> {
        let KernelProcess::Launched(process) = process else {
            return None;
        };

        for _ in 0..20 {
            match process.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
        None
    }

    fn close(&mut self) {
        match &mut self.process {
            KernelProcess::Launched(process) => {
                if let Some(link) = self.link.take() {
                    std::mem::forget(link);
                }

                for _ in 0..20 {
                    if process.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                let _ = process.kill();
                let _ = process.wait();
            }
            KernelProcess::External => {
                drop(self.link.take());
            }
        }
    }
}

fn set_directory_expression(directory: &str) -> String {
    format!("SetDirectory[{}]; Null", wolfram_string_literal(directory))
}

fn connect_link(
    link_protocol: LinkProtocol,
    link_name: &str,
    link_options: Option<u32>,
    link_mode: Option<&str>,
) -> std::result::Result<Link, wstp::Error> {
    if link_options.is_none() && link_mode.is_none() {
        return Link::connect(link_protocol, link_name);
    }

    let args = connect_link_args(link_protocol, link_name, link_options, link_mode);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Link::open_with_args(&args)
}

fn connect_link_args(
    link_protocol: LinkProtocol,
    link_name: &str,
    link_options: Option<u32>,
    link_mode: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "-wstp".to_string(),
        "-linkmode".to_string(),
        link_mode.unwrap_or("connect").to_string(),
        "-linkprotocol".to_string(),
        link_protocol.to_string(),
        "-linkname".to_string(),
        link_name.to_string(),
    ];
    if let Some(link_options) = link_options {
        args.push("-linkoptions".to_string());
        args.push(link_options.to_string());
    }
    args
}

fn configure_kernel_launch_command(
    command: &mut Command,
    link_name: &str,
    link_options: Option<u32>,
    link_mode: Option<&str>,
) {
    command
        .arg("-wstp")
        .arg("-linkprotocol")
        .arg("SharedMemory")
        .arg("-linkconnect")
        .arg("-linkname")
        .arg(link_name);
    if let Some(link_mode) = link_mode {
        command.arg("-linkmode").arg(link_mode);
    }
    if let Some(link_options) = link_options {
        command.arg("-linkoptions").arg(link_options.to_string());
    }
}

impl Drop for WstpKernelClient {
    fn drop(&mut self) {
        self.close();
    }
}

fn wstp_user_input_text(input: &str) -> String {
    if input.contains("Input[") || input.contains("InputString[") {
        wolfram_function_call(
            WSTP_EVALUATE_USER_INPUT_WL,
            &[wolfram_string_literal(input)],
        )
    } else {
        input.to_owned()
    }
}

fn plain_text_result_input(input: &str) -> String {
    wolfram_function_call(
        r#"
Function[
    {input},
    Module[{result},
        Internal`WithLocalSettings[
            Off[General::shdw],
            result = ReleaseHold[ToExpression[input, InputForm, HoldComplete]],
            On[General::shdw]
        ];
        If[StringQ[result], result, ToString[result, OutputForm, PageWidth -> Infinity]]
    ]
]
"#,
        &[wolfram_string_literal(input)],
    )
}

fn put_enter_text_packet(link: &mut Link, input: &str) -> Result<()> {
    link.put_function("System`EnterTextPacket", 1)
        .map_err(|err| anyhow!("failed to begin WSTP EnterTextPacket: {err:?}"))?;
    link.put_str(input)
        .map_err(|err| anyhow!("failed to write WSTP EnterTextPacket text: {err:?}"))?;
    link.end_packet()
        .map_err(|err| anyhow!("failed to finish WSTP EnterTextPacket: {err:?}"))?;
    link.flush()
        .map_err(|err| anyhow!("failed to flush WSTP EnterTextPacket: {err:?}"))
}

fn read_initial_input_name_packet(link: &mut Link, process: &mut KernelProcess) -> Result<String> {
    let mut pending_message_identifier: Option<(String, String)> = None;
    let mut startup_message_printed = false;

    loop {
        let packet_id = next_packet_id(link, process, "initial prompt")?;
        let packet = read_packet_payload(link, packet_id)?;
        match &packet {
            KernelPacket::InputName(prompt) => {
                finish_packet(link, "initial InputNamePacket")?;
                return Ok(prompt.clone());
            }
            // A MessagePacket identifies the TextPacket that follows it. Startup
            // messages must not be swallowed while waiting for the first prompt.
            KernelPacket::Message { symbol, tag } => {
                pending_message_identifier = Some((symbol.clone(), tag.clone()));
            }
            KernelPacket::Text(text) => {
                if let Some((symbol, tag)) = pending_message_identifier.take() {
                    print_startup_kernel_message_text(
                        text,
                        &symbol,
                        &tag,
                        !startup_message_printed,
                    )?;
                    startup_message_printed = true;
                }
            }
            KernelPacket::Input | KernelPacket::InputString => {
                bail!(
                    "kernel sent {} before the initial InputNamePacket",
                    packet_name(packet_id)
                );
            }
            _ => {}
        }
        finish_packet(link, "initial packet")?;
    }
}

fn read_packets_until_return(
    link: &mut Link,
    process: &mut KernelProcess,
    mut input_handler: Option<&mut KernelInputHandler<'_>>,
    read_next_input_name: bool,
    operation: &str,
    mut loading: Option<&mut EvaluationLoadingIndicator>,
    mut stream_text: Option<&mut dyn FnMut(&str, Option<(&str, &str)>) -> Result<()>>,
    mut stream_dialog_marker: Option<&mut dyn FnMut(&str) -> Result<()>>,
) -> Result<Vec<KernelPacket>> {
    let mut packets = Vec::new();
    let mut pending_message_identifier: Option<(String, String)> = None;

    loop {
        let packet_id = next_packet_id(link, process, operation)?;
        let packet = read_packet_payload(link, packet_id)?;
        trace_packet(operation, &packet);
        if matches!(
            packet,
            KernelPacket::Text(_)
                | KernelPacket::BeginDialog(_)
                | KernelPacket::EndDialog(_)
                | KernelPacket::Input
                | KernelPacket::InputString
        ) && let Some(loading) = loading.as_deref_mut()
        {
            loading.hide();
        }
        match &packet {
            KernelPacket::Message { symbol, tag } => {
                pending_message_identifier = Some((symbol.clone(), tag.clone()));
            }
            KernelPacket::Text(text) => {
                if let Some(render) = stream_text.as_deref_mut() {
                    let pending_message = pending_message_identifier.take();
                    let message = pending_message
                        .as_ref()
                        .map(|(symbol, tag)| (symbol.as_str(), tag.as_str()));
                    render(text, message)?;
                    if let Some(loading) = loading.as_deref_mut() {
                        loading.show_below_text(text);
                    }
                }
            }
            KernelPacket::BeginDialog(_) => {
                if let Some(render) = stream_dialog_marker.as_deref_mut() {
                    render("enter dialog")?;
                }
            }
            KernelPacket::EndDialog(_) => {
                if let Some(render) = stream_dialog_marker.as_deref_mut() {
                    render("exit dialog")?;
                }
            }
            _ => {}
        }
        let terminal = packet_is_terminal(&packet);
        let next_prompt_after_result =
            read_next_input_name && matches!(packet, KernelPacket::InputName(_));
        let input_request = match packet {
            KernelPacket::Input => Some(KernelInputRequest {
                kind: KernelInputKind::Expression,
                prompt: input_request_prompt(&packets, stream_text.is_some()),
            }),
            KernelPacket::InputString => Some(KernelInputRequest {
                kind: KernelInputKind::String,
                prompt: input_request_prompt(&packets, stream_text.is_some()),
            }),
            _ => None,
        };

        packets.push(packet);

        if let Some(request) = input_request {
            let response = match input_handler.as_deref_mut() {
                Some(handler) => handler(&request)?,
                None => bail!(
                    "kernel requested input during {operation}, but no input handler is available"
                ),
            };
            let response = response.context("kernel input was cancelled")?;
            send_input_response(link, &request, &response)?;
            continue;
        }

        if next_prompt_after_result {
            finish_packet(link, "WSTP InputNamePacket")?;
            return Ok(packets);
        }

        if terminal {
            if read_next_input_name {
                finish_packet(link, "WSTP terminal packet")?;
                continue;
            }
            return Ok(packets);
        }

        finish_packet(link, "WSTP packet")?;
    }
}

fn next_packet_id(link: &mut Link, process: &mut KernelProcess, operation: &str) -> Result<i32> {
    let wait_start = Instant::now();
    wait_for_packet_activity(link, process, operation)?;
    profile_duration("wstp.packet.wait", wait_start.elapsed(), operation);
    let next_start = Instant::now();
    match link.raw_next_packet() {
        Ok(packet_id) => {
            profile_duration("wstp.packet.next", next_start.elapsed(), operation);
            Ok(packet_id)
        }
        Err(err) => {
            if let Some(code) = WstpKernelClient::child_exit_code_after_link_error(process) {
                return Err(KernelExit::new(code).into());
            }
            Err(anyhow!("failed to read packet during {operation}: {err:?}"))
        }
    }
}

fn wait_for_packet_activity(
    link: &mut Link,
    process: &mut KernelProcess,
    operation: &str,
) -> Result<()> {
    while !link.is_ready() {
        if let KernelProcess::Launched(process) = process
            && let Some(status) = process
                .try_wait()
                .context("failed to check WolframKernel process status")?
        {
            return kernel_exit_result(status, operation);
        }

        // Connected kernels have no local child process to inspect, but they
        // must still poll terminal input so Ctrl-C can send WSAbortMessage.
        if take_kernel_interrupt_request() {
            send_abort_message(link)?;
        }
        thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

fn kernel_exit_result(status: ExitStatus, operation: &str) -> Result<()> {
    if let Some(code) = status.code() {
        return Err(KernelExit::new(code).into());
    }

    bail!("WolframKernel exited with {status} during {operation}")
}

fn take_kernel_interrupt_request() -> bool {
    interrupt::take_kernel_interrupt_request() || take_ctrl_c_key_event()
}

fn take_ctrl_c_key_event() -> bool {
    match event::poll(Duration::from_millis(0)) {
        Ok(true) => match event::read() {
            Ok(event) => is_ctrl_c_key_event(&event),
            Err(err) => {
                profile_event(format!("terminal.event.read.failed\t{err:?}"));
                false
            }
        },
        Ok(false) => false,
        Err(err) => {
            profile_event(format!("terminal.event.poll.failed\t{err:?}"));
            false
        }
    }
}

fn is_ctrl_c_key_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c') | KeyCode::Char('C'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

fn send_abort_message(link: &mut Link) -> Result<()> {
    // Match the Front End's Alt+. behavior: WSAbortMessage is urgent-message
    // code 3, while WSInterruptMessage (code 2) only requests an interrupt.
    link.put_message(UrgentMessage::ABORT)
        .map_err(|err| anyhow!("failed to send WSTP abort message: {err:?}"))?;
    profile_event("wstp.abort.sent");
    Ok(())
}

fn isolate_kernel_from_terminal_interrupts(command: &mut Command) {
    configure_kernel_process_group(command);
}

#[cfg(unix)]
fn configure_kernel_process_group(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_kernel_process_group(_command: &mut Command) {}

fn send_input_response(
    link: &mut Link,
    request: &KernelInputRequest,
    response: &str,
) -> Result<()> {
    let response = response.trim_end_matches(['\r', '\n']);
    finish_packet(link, "WSTP input request packet")?;
    match request.kind {
        KernelInputKind::Expression => {
            put_enter_text_packet(link, response)
                .map_err(|err| anyhow!("failed to send WSTP InputPacket response: {err:?}"))?;
        }
        KernelInputKind::String => {
            link.put_str(response).map_err(|err| {
                anyhow!("failed to send WSTP InputStringPacket response: {err:?}")
            })?;
            link.end_packet().map_err(|err| {
                anyhow!("failed to finish WSTP InputStringPacket response packet: {err:?}")
            })?;
            link.flush().map_err(|err| {
                anyhow!("failed to flush WSTP InputStringPacket response: {err:?}")
            })?;
        }
    }
    Ok(())
}

fn finish_packet(link: &mut Link, context: &str) -> Result<()> {
    link.new_packet()
        .map_err(|err| anyhow!("failed to finish {context}: {err:?}"))
}

fn read_packet_payload(link: &mut Link, packet_id: i32) -> Result<KernelPacket> {
    let packet = match packet_id {
        sys::BEGINDLGPKT => KernelPacket::BeginDialog(read_i32(link, "BeginDialogPacket")?),
        sys::CALLPKT => KernelPacket::Call {
            function: read_i32(link, "CallPacket function")?,
            args: read_expr(link, "CallPacket arguments")?,
        },
        sys::DISPLAYENDPKT => KernelPacket::DisplayEnd,
        sys::DISPLAYPKT => KernelPacket::Display,
        sys::ENDDLGPKT => KernelPacket::EndDialog(read_i32(link, "EndDialogPacket")?),
        sys::ENTEREXPRPKT => {
            KernelPacket::EnterExpression(read_expr(link, "EnterExpressionPacket")?)
        }
        sys::ENTERTEXTPKT => KernelPacket::EnterText(read_string(link, "EnterTextPacket")?),
        sys::EVALUATEPKT => KernelPacket::Evaluate(read_expr(link, "EvaluatePacket")?),
        sys::INPUTNAMEPKT => KernelPacket::InputName(read_string(link, "InputNamePacket")?),
        sys::INPUTPKT => KernelPacket::Input,
        sys::INPUTSTRPKT => KernelPacket::InputString,
        sys::MENUPKT => KernelPacket::Menu {
            id: read_i32(link, "MenuPacket id")?,
            title: read_string(link, "MenuPacket title")?,
        },
        sys::MESSAGEPKT => KernelPacket::Message {
            symbol: read_symbol(link, "MessagePacket symbol")?,
            tag: read_string(link, "MessagePacket tag")?,
        },
        sys::OUTPUTNAMEPKT => KernelPacket::OutputName(read_string(link, "OutputNamePacket")?),
        sys::RESUMEPKT => KernelPacket::Resume,
        sys::RETURNEXPRPKT => {
            KernelPacket::ReturnExpression(read_expr(link, "ReturnExpressionPacket")?)
        }
        sys::RETURNPKT => KernelPacket::Return(read_expr(link, "ReturnPacket")?),
        sys::RETURNTEXTPKT => KernelPacket::ReturnText(read_string(link, "ReturnTextPacket")?),
        sys::SUSPENDPKT => KernelPacket::Suspend,
        sys::SYNTAXPKT => KernelPacket::Syntax(read_i32(link, "SyntaxPacket")?),
        sys::TEXTPKT => KernelPacket::Text(read_string(link, "TextPacket")?),
        unknown => KernelPacket::Unknown(unknown),
    };
    Ok(packet)
}

fn read_string(link: &mut Link, context: &str) -> Result<String> {
    link.get_string()
        .map_err(|err| anyhow!("failed to read {context} string: {err:?}"))
}

fn read_symbol(link: &mut Link, context: &str) -> Result<String> {
    link.get_symbol_ref()
        .map(|symbol| symbol.as_str().to_owned())
        .map_err(|err| anyhow!("failed to read {context}: {err:?}"))
}

fn read_i32(link: &mut Link, context: &str) -> Result<i32> {
    link.get_i32()
        .map_err(|err| anyhow!("failed to read {context} integer: {err:?}"))
}

fn read_expr(link: &mut Link, context: &str) -> Result<Expr> {
    link.get_expr()
        .map_err(|err| anyhow!("failed to read {context} expression: {err:?}"))
}

fn packet_is_terminal(packet: &KernelPacket) -> bool {
    matches!(
        packet,
        KernelPacket::Return(_)
            | KernelPacket::ReturnExpression(_)
            | KernelPacket::ReturnText(_)
            | KernelPacket::Syntax(_)
    )
}





fn input_request_prompt(packets: &[KernelPacket], text_packets_already_rendered: bool) -> String {
    packets
        .iter()
        .rev()
        .find_map(|packet| match packet {
            KernelPacket::Text(text) if !text.ends_with('\n') => Some(if text_packets_already_rendered {
                String::new()
            } else {
                text.clone()
            }),
            KernelPacket::InputName(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn next_input_prompt_after_evaluation(
    previous_prompt: Option<&str>,
    packets: &[KernelPacket],
) -> Option<String> {
    open_dialog_input_name(packets)
        .or_else(|| next_input_name_after_result(packets))
        .or_else(|| last_output_name(packets).and_then(next_input_prompt_from_output_name))
        .or_else(|| previous_prompt.and_then(increment_input_prompt))
}

fn open_dialog_input_name(packets: &[KernelPacket]) -> Option<String> {
    let begin_index = packets
        .iter()
        .rposition(|packet| matches!(packet, KernelPacket::BeginDialog(_)))?;
    let end_index = packets
        .iter()
        .rposition(|packet| matches!(packet, KernelPacket::EndDialog(_)));
    if end_index.is_some_and(|end_index| end_index > begin_index) {
        return None;
    }

    packets[begin_index + 1..]
        .iter()
        .rev()
        .find_map(|packet| match packet {
            KernelPacket::InputName(prompt) if !prompt.trim().is_empty() => Some(prompt.clone()),
            _ => None,
        })
}

fn next_input_name_after_result(packets: &[KernelPacket]) -> Option<String> {
    let result_index = packets.iter().rposition(packet_is_terminal)?;
    packets[result_index + 1..]
        .iter()
        .rev()
        .find_map(|packet| match packet {
            KernelPacket::InputName(text) if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
}

fn last_output_name(packets: &[KernelPacket]) -> Option<&str> {
    packets.iter().rev().find_map(|packet| match packet {
        KernelPacket::OutputName(text) if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })
}

fn next_input_prompt_from_output_name(output_name: &str) -> Option<String> {
    let number = prompt_number(output_name, "Out[", "]")?;
    Some(format!("In[{}]:=", number + 1))
}

fn increment_input_prompt(input_prompt: &str) -> Option<String> {
    let start = input_prompt.find("In[")? + "In[".len();
    let end = input_prompt[start..].find("]:=")? + start;
    let number = input_prompt[start..end].parse::<u64>().ok()?;

    let mut next = String::new();
    next.push_str(&input_prompt[..start]);
    next.push_str(&(number + 1).to_string());
    next.push_str(&input_prompt[end..]);
    Some(next)
}

fn prompt_number(prompt: &str, number_prefix: &str, number_suffix: &str) -> Option<u64> {
    let start = prompt.find(number_prefix)? + number_prefix.len();
    let end = prompt[start..].find(number_suffix)? + start;
    prompt[start..end].parse::<u64>().ok()
}

fn packet_name(packet_id: i32) -> &'static str {
    match packet_id {
        sys::BEGINDLGPKT => "BeginDialogPacket",
        sys::CALLPKT => "CallPacket",
        sys::DISPLAYENDPKT => "DisplayEndPacket",
        sys::DISPLAYPKT => "DisplayPacket",
        sys::ENDDLGPKT => "EndDialogPacket",
        sys::ENTEREXPRPKT => "EnterExpressionPacket",
        sys::ENTERTEXTPKT => "EnterTextPacket",
        sys::EVALUATEPKT => "EvaluatePacket",
        sys::INPUTNAMEPKT => "InputNamePacket",
        sys::INPUTPKT => "InputPacket",
        sys::INPUTSTRPKT => "InputStringPacket",
        sys::MENUPKT => "MenuPacket",
        sys::MESSAGEPKT => "MessagePacket",
        sys::OUTPUTNAMEPKT => "OutputNamePacket",
        sys::RESUMEPKT => "ResumePacket",
        sys::RETURNEXPRPKT => "ReturnExpressionPacket",
        sys::RETURNPKT => "ReturnPacket",
        sys::RETURNTEXTPKT => "ReturnTextPacket",
        sys::SUSPENDPKT => "SuspendPacket",
        sys::SYNTAXPKT => "SyntaxPacket",
        sys::TEXTPKT => "TextPacket",
        _ => "unknown packet",
    }
}

fn packet_text_result(packet: &KernelPacket) -> Option<String> {
    match packet {
        KernelPacket::Return(expr) | KernelPacket::ReturnExpression(expr) => {
            Some(expr_string_value(expr).unwrap_or_else(|| expr.to_string()))
        }
        KernelPacket::ReturnText(text) => Some(text.clone()),
        _ => None,
    }
}

fn expr_string_value(expr: &Expr) -> Option<String> {
    match expr.kind() {
        ExprKind::String(value) => Some(value.clone()),
        _ => None,
    }
}

fn packet_output_bytes(packets: &[KernelPacket]) -> usize {
    packets
        .iter()
        .map(|packet| match packet {
            KernelPacket::Text(text)
            | KernelPacket::ReturnText(text)
            | KernelPacket::InputName(text)
            | KernelPacket::OutputName(text) => text.len(),
            KernelPacket::Return(expr) | KernelPacket::ReturnExpression(expr) => {
                expr.to_string().len()
            }
            _ => 0,
        })
        .sum()
}

fn trace_packet(operation: &str, packet: &KernelPacket) {
    profile_event_with(|| format!("wstp.packet\t{operation}\t{}", packet_summary(packet)));
}

fn packet_summary(packet: &KernelPacket) -> String {
    match packet {
        KernelPacket::BeginDialog(id) => format!("BeginDialogPacket[{id}]"),
        KernelPacket::Call { function, args } => format!("CallPacket[{function}, {args}]"),
        KernelPacket::DisplayEnd => "DisplayEndPacket[]".to_owned(),
        KernelPacket::Display => "DisplayPacket[]".to_owned(),
        KernelPacket::EndDialog(id) => format!("EndDialogPacket[{id}]"),
        KernelPacket::EnterExpression(expr) => format!("EnterExpressionPacket[{expr}]"),
        KernelPacket::EnterText(text) => format!("EnterTextPacket[{}]", debug_text(text)),
        KernelPacket::Evaluate(expr) => format!("EvaluatePacket[{expr}]"),
        KernelPacket::InputName(text) => format!("InputNamePacket[{}]", debug_text(text)),
        KernelPacket::Input => "InputPacket[]".to_owned(),
        KernelPacket::InputString => "InputStringPacket[]".to_owned(),
        KernelPacket::Menu { id, title } => format!("MenuPacket[{id}, {}]", debug_text(title)),
        KernelPacket::Message { symbol, tag } => {
            format!("MessagePacket[{symbol}, {}]", debug_text(tag))
        }
        KernelPacket::OutputName(text) => format!("OutputNamePacket[{}]", debug_text(text)),
        KernelPacket::Resume => "ResumePacket[]".to_owned(),
        KernelPacket::Return(expr) => format!("ReturnPacket[{expr}]"),
        KernelPacket::ReturnExpression(expr) => format!("ReturnExpressionPacket[{expr}]"),
        KernelPacket::ReturnText(text) => format!("ReturnTextPacket[{}]", debug_text(text)),
        KernelPacket::Suspend => "SuspendPacket[]".to_owned(),
        KernelPacket::Syntax(position) => format!("SyntaxPacket[{position}]"),
        KernelPacket::Text(text) => format!("TextPacket[{}]", debug_text(text)),
        KernelPacket::Unknown(id) => format!("UnknownPacket[{id}]"),
    }
}

fn debug_text(text: &str) -> String {
    format!("{text:?}")
}

struct PacketRenderOptions {
    separate_input_and_output: bool,
    show_output_prompt: bool,
}

fn render_dialog_marker(marker: &str, theme: Option<&ThemeHandle>) -> String {
    let marker = format!("({marker})");
    match theme {
        Some(theme) => theme.current().styles().comment.paint(marker).to_string(),
        None => marker,
    }
}

fn render_packets(
    packets: &[KernelPacket],
    theme: Option<&ThemeHandle>,
    options: PacketRenderOptions,
) -> Result<()> {
    let mut output_name: Option<&str> = None;
    let mut text_without_trailing_newline = false;
    let mut output_separator_pending = options.separate_input_and_output;

    for (index, packet) in packets.iter().enumerate() {
        match packet {
            // Text packets are rendered as soon as they arrive from the kernel.
            // Keep their layout state here so deferred result rendering still
            // inserts a separating newline when the text has no trailing newline.
            KernelPacket::Text(text) => {
                if text_is_input_prompt(packets, index) {
                    text_without_trailing_newline = false;
                    continue;
                }
                output_separator_pending = false;
                text_without_trailing_newline = !text.ends_with('\n');
            }
            KernelPacket::Message { .. } => {}
            KernelPacket::OutputName(name) => output_name = Some(name),
            KernelPacket::Return(expr) | KernelPacket::ReturnExpression(expr) => {
                if output_separator_pending {
                    print_kernel_text("\n")?;
                    output_separator_pending = false;
                }
                if text_without_trailing_newline {
                    print_kernel_text("\n")?;
                    text_without_trailing_newline = false;
                }
                let text = expr_string_value(expr).unwrap_or_else(|| expr.to_string());
                render_return_text(&text, output_name.take(), theme, options.show_output_prompt)?;
                if options.separate_input_and_output && !text.is_empty() {
                    print_kernel_text("\n")?;
                }
            }
            KernelPacket::ReturnText(text) => {
                if output_separator_pending {
                    print_kernel_text("\n")?;
                    output_separator_pending = false;
                }
                if text_without_trailing_newline {
                    print_kernel_text("\n")?;
                    text_without_trailing_newline = false;
                }
                render_return_text(text, output_name.take(), theme, options.show_output_prompt)?;
                if options.separate_input_and_output && !text.is_empty() {
                    print_kernel_text("\n")?;
                }
            }
            KernelPacket::Syntax(position) => {
                print_kernel_text(&format!("Syntax error at position {position}\n"))?;
            }
            KernelPacket::BeginDialog(_) | KernelPacket::EndDialog(_) => {}
            KernelPacket::Menu { id, title } => {
                print_kernel_text(&format!("MenuPacket[{id}, {title}]\n"))?;
            }
            KernelPacket::Call { function, args } => {
                print_kernel_text(&format!("CallPacket[{function}, {args}]\n"))?;
            }
            KernelPacket::Unknown(id) => {
                print_kernel_text(&format!("Unknown WSTP packet {id}\n"))?;
            }
            KernelPacket::EnterExpression(expr) => {
                print_kernel_text(&format!("EnterExpressionPacket[{expr}]\n"))?;
            }
            KernelPacket::EnterText(text) => {
                print_kernel_text(&format!("EnterTextPacket[{text}]\n"))?;
            }
            KernelPacket::Evaluate(expr) => {
                print_kernel_text(&format!("EvaluatePacket[{expr}]\n"))?;
            }
            KernelPacket::Display
            | KernelPacket::DisplayEnd
            | KernelPacket::Input
            | KernelPacket::InputName(_)
            | KernelPacket::InputString
            | KernelPacket::Resume
            | KernelPacket::Suspend => {}
        }
    }

    Ok(())
}

fn text_is_input_prompt(packets: &[KernelPacket], index: usize) -> bool {
    matches!(
        packets.get(index + 1),
        Some(KernelPacket::Input | KernelPacket::InputString)
    ) && matches!(packets.get(index), Some(KernelPacket::Text(text)) if !text.ends_with('\n'))
}

fn render_return_text(
    text: &str,
    output_name: Option<&str>,
    theme: Option<&ThemeHandle>,
    show_output_prompt: bool,
) -> Result<()> {
    if let Some(text) = rendered_return_text(text, output_name, theme, show_output_prompt) {
        println!("{text}");
    }
    Ok(())
}

fn rendered_return_text(
    text: &str,
    output_name: Option<&str>,
    theme: Option<&ThemeHandle>,
    show_output_prompt: bool,
) -> Option<String> {
    if text.is_empty() {
        return None;
    }

    let mut rendered = String::new();
    if show_output_prompt && let Some(output_name) = output_name {
        rendered.push_str(&render_output_name_with_color(
            output_name,
            output_name_color_enabled(theme),
        ));
    }
    rendered.push_str(text);
    Some(rendered)
}

fn render_output_name_with_color(output_name: &str, use_color: bool) -> String {
    if use_color {
        Color::DarkGray.paint(output_name).to_string()
    } else {
        output_name.to_owned()
    }
}

fn symbol(name: &str) -> Expr {
    Expr::symbol(Symbol::try_new(name).expect("internal symbol names are qualified"))
}

fn call(head: &str, args: Vec<Expr>) -> Expr {
    Expr::normal(symbol(head), args)
}

/// Wraps `input` so a `String` result is returned as-is, and anything else
/// is rendered via `ToString[.., InputForm]`. See `evaluate_to_string`.
fn wrap_to_string_query(input: &str) -> String {
    format!(
        "Module[{{wolframCliQueryResult$ = ({input})}}, \
         If[StringQ[wolframCliQueryResult$], wolframCliQueryResult$, \
         ToString[wolframCliQueryResult$, InputForm]]]"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        KernelExit, KernelPacket, configure_kernel_launch_command, connect_link_args,
        kernel_exit_result, next_input_prompt_after_evaluation, plain_text_result_input,
        render_dialog_marker, render_message_text_with_color,
        render_output_name_with_color, render_startup_message_text,
        rendered_return_text, set_directory_expression, wrap_to_string_query,
    };
    use crate::theme::{Theme, ThemeHandle};
    use std::process::{Command, ExitStatus};

    #[test]
    fn kernel_exit_result_preserves_child_exit_code() {
        let err = kernel_exit_result(child_status_for_exit_code(23), "test operation")
            .expect_err("kernel exit status should be returned as an error");
        let exit = err
            .downcast_ref::<KernelExit>()
            .expect("child exit status should map to KernelExit");

        assert_eq!(exit.code, 23);
    }

    #[cfg(unix)]
    fn child_status_for_exit_code(code: i32) -> ExitStatus {
        Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .status()
            .expect("failed to run test shell process")
    }

    #[cfg(windows)]
    fn child_status_for_exit_code(code: i32) -> ExitStatus {
        Command::new("cmd")
            .arg("/C")
            .arg(format!("exit /B {code}"))
            .status()
            .expect("failed to run test shell process")
    }

    #[test]
    fn wrap_to_string_query_returns_string_results_unconverted() {
        let wrapped = wrap_to_string_query("StringJoin[\"a\", \"b\"]");
        assert!(wrapped.contains("StringQ[wolframCliQueryResult$]"));
        assert!(wrapped.contains("ToString[wolframCliQueryResult$, InputForm]"));
        assert!(wrapped.contains("StringJoin[\"a\", \"b\"]"));
    }

    #[test]
    fn plain_text_result_input_returns_strings_unconverted_and_disables_page_wrapping() {
        let wrapped = plain_text_result_input("$InputFileName");

        assert!(wrapped.contains("StringQ[result]"));
        assert!(wrapped.contains("ToString[result, OutputForm, PageWidth -> Infinity]"));
        assert!(wrapped.contains("$InputFileName"));
    }

    #[test]
    fn set_directory_expression_escapes_directory_path() {
        assert_eq!(
            set_directory_expression("/tmp/wolfie \"project\""),
            "SetDirectory[\"/tmp/wolfie \\\"project\\\"\"]; Null"
        );
    }

    #[test]
    fn kernel_launch_command_adds_linkoptions_only_when_present() {
        let mut command = Command::new("WolframKernel");
        configure_kernel_launch_command(&mut command, "test-link", None, None);

        let args = command_args(&command);
        assert!(!args.contains(&"-linkoptions".to_string()));
        assert!(!args.contains(&"-linkmode".to_string()));

        let mut command = Command::new("WolframKernel");
        configure_kernel_launch_command(&mut command, "test-link", Some(4), Some("Listen"));

        assert_eq!(
            command_args(&command),
            [
                "-wstp",
                "-linkprotocol",
                "SharedMemory",
                "-linkconnect",
                "-linkname",
                "test-link",
                "-linkmode",
                "Listen",
                "-linkoptions",
                "4",
            ]
        );
    }

    #[test]
    fn connected_link_args_add_linkmode_and_linkoptions_when_present() {
        assert_eq!(
            connect_link_args(super::LinkProtocol::SharedMemory, "test-link", None, None),
            [
                "-wstp",
                "-linkmode",
                "connect",
                "-linkprotocol",
                "SharedMemory",
                "-linkname",
                "test-link",
            ]
        );

        assert_eq!(
            connect_link_args(
                super::LinkProtocol::TCPIP,
                "1234@localhost",
                Some(4),
                Some("Connect"),
            ),
            [
                "-wstp",
                "-linkmode",
                "Connect",
                "-linkprotocol",
                "TCPIP",
                "-linkname",
                "1234@localhost",
                "-linkoptions",
                "4",
            ]
        );
    }

    fn command_args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn message_text_renders_short_identifier_red() {
        let rendered = render_message_text_with_color(
            "Power::infy: Infinite expression 1/0 encountered.",
            "System`Power",
            "infy",
            true,
        );

        assert_eq!(
            rendered,
            format!(
                "{}: Infinite expression 1/0 encountered.\n",
                nu_ansi_term::Color::Red.paint("Power::infy")
            )
        );
    }

    #[test]
    fn message_text_renders_identifier_red_after_layout_prefix() {
        let rendered = render_message_text_with_color(
            "                                 1\nPower::infy: Infinite expression - encountered.                                 0",
            "Power",
            "infy",
            true,
        );

        assert_eq!(
            rendered,
            format!(
                "                                 1\n{}: Infinite expression - encountered.                                 0\n",
                nu_ansi_term::Color::Red.paint("Power::infy")
            )
        );
    }

    #[test]
    fn startup_message_starts_on_a_fresh_line_and_has_one_terminator() {
        assert_eq!(
            render_startup_message_text(
                "Get::noopen: Cannot open WAssistant`Server`.\n",
                "Get",
                "noopen",
                true,
            ),
            "\r\nGet::noopen: Cannot open WAssistant`Server`.\r\n"
        );
    }

    #[test]
    fn startup_message_preserves_internal_line_breaks() {
        assert_eq!(
            render_startup_message_text(
                "Needs::nocont: Context WAssistant`Server`\nwas not created.\n",
                "Needs",
                "nocont",
                false,
            ),
            "Needs::nocont: Context WAssistant`Server`\nwas not created.\r\n"
        );
    }

    #[test]
    fn message_text_leaves_identifier_plain_when_color_is_disabled() {
        let text = "General::stop: Further output will be suppressed.\n";

        assert_eq!(
            render_message_text_with_color(text, "System`General", "stop", false),
            text
        );
    }

    #[test]
    fn dialog_markers_use_the_comment_style() {
        let theme = ThemeHandle::builtin(Theme::dark());

        assert_eq!(
            render_dialog_marker("enter dialog", Some(&theme)),
            theme
                .current()
                .styles()
                .comment
                .paint("(enter dialog)")
                .to_string()
        );
        assert_eq!(
            render_dialog_marker("exit dialog", Some(&theme)),
            theme
                .current()
                .styles()
                .comment
                .paint("(exit dialog)")
                .to_string()
        );
    }

    #[test]
    fn dialog_markers_are_unstyled_for_the_plain_theme() {
        let theme = ThemeHandle::builtin(Theme::plain());

        assert_eq!(
            render_dialog_marker("enter dialog", Some(&theme)),
            "(enter dialog)"
        );
    }

    #[test]
    fn output_name_renders_dark_gray() {
        assert_eq!(
            render_output_name_with_color("Out[7]= ", true),
            nu_ansi_term::Color::DarkGray.paint("Out[7]= ").to_string()
        );
    }

    #[test]
    fn output_name_stays_plain_when_color_is_disabled() {
        assert_eq!(render_output_name_with_color("Out[7]= ", false), "Out[7]= ");
    }

    #[test]
    fn return_text_includes_output_name_for_repl() {
        assert_eq!(
            rendered_return_text("2", Some("Out[1]= "), None, true),
            Some(format!(
                "{}2",
                render_output_name_with_color("Out[1]= ", true)
            ))
        );
    }

    #[test]
    fn return_text_suppresses_output_name_for_eval() {
        assert_eq!(
            rendered_return_text("2", Some("Out[1]= "), None, false),
            Some("2".to_string())
        );
    }

    #[test]
    fn message_text_only_colors_matching_identifier() {
        let text = "not a message: Power::infy appears later.\n";

        assert_eq!(
            render_message_text_with_color(text, "System`Power", "infy", true),
            text
        );
    }







    #[test]
    fn next_input_prompt_uses_open_dialog_input_name() {
        let packets = vec![
            KernelPacket::BeginDialog(1),
            KernelPacket::InputName(" In[2]:= ".to_string()),
        ];

        assert_eq!(
            next_input_prompt_after_evaluation(Some("In[1]:= "), &packets),
            Some(" In[2]:= ".to_string())
        );
    }

    #[test]
    fn next_input_prompt_uses_non_empty_input_name_packet_after_result() {
        let packets = vec![
            KernelPacket::OutputName("Out[7]=".to_string()),
            KernelPacket::ReturnText("2".to_string()),
            KernelPacket::InputName("In[8]:=".to_string()),
        ];

        assert_eq!(
            next_input_prompt_after_evaluation(Some("In[7]:="), &packets),
            Some("In[8]:=".to_string())
        );
    }

    #[test]
    fn next_input_prompt_ignores_an_internal_input_name_before_result() {
        let packets = vec![KernelPacket::InputName("In[1]:=".to_string())];

        assert_eq!(
            next_input_prompt_after_evaluation(Some("In[7]:="), &packets),
            Some("In[8]:=".to_string())
        );
    }

    #[test]
    fn next_input_prompt_ignores_input_name_packets_before_result() {
        let packets = vec![
            KernelPacket::InputName("In[1]:=".to_string()),
            KernelPacket::Text("loaded file side effect\n".to_string()),
            KernelPacket::ReturnText(String::new()),
        ];

        assert_eq!(
            next_input_prompt_after_evaluation(Some("In[7]:="), &packets),
            Some("In[8]:=".to_string())
        );
    }


    #[test]
    fn next_input_prompt_falls_back_to_output_name_when_post_result_input_name_is_empty() {
        let packets = vec![
            KernelPacket::OutputName("Out[7]=".to_string()),
            KernelPacket::ReturnText("2".to_string()),
            KernelPacket::InputName(String::new()),
        ];

        assert_eq!(
            next_input_prompt_after_evaluation(Some("In[7]:="), &packets),
            Some("In[8]:=".to_string())
        );
    }

    #[test]
    fn next_input_prompt_falls_back_to_previous_input_prompt() {
        let packets = vec![KernelPacket::Text("side effect only\n".to_string())];

        assert_eq!(
            next_input_prompt_after_evaluation(Some("In[7]:="), &packets),
            Some("In[8]:=".to_string())
        );
    }
}
