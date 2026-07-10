use std::{
    io::{self, Write},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use nu_ansi_term::Color;
use wolfram_expr::{Expr, ExprKind, Symbol};
use wstp::{Link, Protocol, UrgentMessage, sys};

use crate::{
    interrupt,
    kernel::{KernelExit, kernel_path},
    profiler::{profile_duration, profile_event},
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
        format!("{}{}{}", prefix, Color::Red.paint(identifier), rest)
    } else {
        text.to_owned()
    }
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

pub(crate) struct WstpKernelClient {
    process: Child,
    link: Option<Link>,
    input_prompt: Option<String>,
    initial_prompt_pending: bool,
}

impl WstpKernelClient {
    pub(crate) fn launch() -> Result<Self> {
        let start = Instant::now();
        let path = kernel_path()?;
        let mut link = Link::listen(Protocol::SharedMemory, "")
            .map_err(|err| anyhow!("failed to create WSTP listener: {err:?}"))?;
        let link_name = link.link_name();
        let spawn_start = Instant::now();
        let mut command = Command::new(path);
        command
            .arg("-wstp")
            .arg("-linkprotocol")
            .arg("SharedMemory")
            .arg("-linkconnect")
            .arg("-linkname")
            .arg(&link_name)
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
            process,
            link: Some(link),
            input_prompt: None,
            initial_prompt_pending: true,
        })
    }

    pub(crate) fn evaluate_once(
        &mut self,
        input: &str,
        theme: Option<&ThemeHandle>,
        input_handler: Option<&mut KernelInputHandler<'_>>,
        separate_input_and_output: bool,
    ) -> Result<()> {
        let previous_input_prompt = self.input_prompt.clone();
        let packets = self.evaluate_input_packets(input, input_handler)?;
        let input_prompt =
            next_input_prompt_after_evaluation(previous_input_prompt.as_deref(), &packets);
        render_packets(&packets, theme, separate_input_and_output)?;
        if let Some(input_prompt) = input_prompt {
            self.input_prompt = Some(input_prompt);
        }
        Ok(())
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

    fn evaluate_input_packets(
        &mut self,
        input: &str,
        input_handler: Option<&mut KernelInputHandler<'_>>,
    ) -> Result<Vec<KernelPacket>> {
        self.ensure_initial_prompt_read()?;
        interrupt::clear_kernel_interrupt_request();
        let start = Instant::now();
        let link = self.link.as_mut().context("WSTP link is closed")?;
        let input = wstp_user_input_text(input);
        put_enter_text_packet(link, &input)?;
        profile_duration("wstp.enter_text.sent", start.elapsed(), "");

        let packets = read_packets_until_return(
            link,
            &mut self.process,
            input_handler,
            true,
            "WSTP EnterTextPacket evaluation",
        )?;
        let output_bytes = packet_output_bytes(&packets);
        profile_duration(
            "wstp.enter_text.total",
            start.elapsed(),
            format!("bytes={output_bytes}"),
        );
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

    fn child_exit_code_after_link_error(process: &mut Child) -> Option<i32> {
        for _ in 0..20 {
            match process.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
        None
    }

    fn stop_child(&mut self) {
        if let Some(link) = self.link.take() {
            std::mem::forget(link);
        }

        for _ in 0..20 {
            if self.process.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Drop for WstpKernelClient {
    fn drop(&mut self) {
        self.stop_child();
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

fn read_initial_input_name_packet(link: &mut Link, process: &mut Child) -> Result<String> {
    loop {
        let packet_id = next_packet_id(link, process, "initial prompt")?;
        let packet = read_packet_payload(link, packet_id)?;
        if let KernelPacket::InputName(prompt) = packet {
            finish_packet(link, "initial InputNamePacket")?;
            return Ok(prompt);
        }
        if matches!(packet, KernelPacket::Input | KernelPacket::InputString) {
            bail!(
                "kernel sent {} before the initial InputNamePacket",
                packet_name(packet_id)
            );
        }
        finish_packet(link, "initial packet")?;
    }
}

fn read_packets_until_return(
    link: &mut Link,
    process: &mut Child,
    mut input_handler: Option<&mut KernelInputHandler<'_>>,
    read_next_input_name: bool,
    operation: &str,
) -> Result<Vec<KernelPacket>> {
    let mut packets = Vec::new();

    loop {
        let packet_id = next_packet_id(link, process, operation)?;
        let packet = read_packet_payload(link, packet_id)?;
        trace_packet(operation, &packet);
        let terminal = packet_is_terminal(&packet);
        let next_prompt_after_result =
            read_next_input_name && matches!(packet, KernelPacket::InputName(_));
        let input_request = match packet {
            KernelPacket::Input => Some(KernelInputRequest {
                kind: KernelInputKind::Expression,
                prompt: input_request_prompt(&packets),
            }),
            KernelPacket::InputString => Some(KernelInputRequest {
                kind: KernelInputKind::String,
                prompt: input_request_prompt(&packets),
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

fn next_packet_id(link: &mut Link, process: &mut Child, operation: &str) -> Result<i32> {
    wait_for_packet_activity(link, process, operation)?;
    match link.raw_next_packet() {
        Ok(packet_id) => Ok(packet_id),
        Err(err) => {
            if let Some(code) = WstpKernelClient::child_exit_code_after_link_error(process) {
                return Err(KernelExit::new(code).into());
            }
            Err(anyhow!("failed to read packet during {operation}: {err:?}"))
        }
    }
}

fn wait_for_packet_activity(link: &mut Link, process: &mut Child, operation: &str) -> Result<()> {
    while !link.is_ready() {
        if let Some(status) = process
            .try_wait()
            .context("failed to check WolframKernel process status")?
        {
            return kernel_exit_result(status, operation);
        }

        if take_kernel_interrupt_request() {
            send_interrupt_message(link)?;
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

fn send_interrupt_message(link: &mut Link) -> Result<()> {
    link.put_message(UrgentMessage::INTERRUPT)
        .map_err(|err| anyhow!("failed to send WSTP interrupt message: {err:?}"))?;
    profile_event("wstp.interrupt.sent");
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

fn input_request_prompt(packets: &[KernelPacket]) -> String {
    packets
        .iter()
        .rev()
        .find_map(|packet| match packet {
            KernelPacket::Text(text) if !text.ends_with('\n') => Some(text.clone()),
            KernelPacket::InputName(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn next_input_prompt_after_evaluation(
    previous_prompt: Option<&str>,
    packets: &[KernelPacket],
) -> Option<String> {
    last_input_name(packets)
        .or_else(|| last_output_name(packets).and_then(next_input_prompt_from_output_name))
        .or_else(|| previous_prompt.and_then(increment_input_prompt))
}

fn last_input_name(packets: &[KernelPacket]) -> Option<String> {
    packets.iter().rev().find_map(|packet| match packet {
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
    profile_event(format!(
        "wstp.packet\t{operation}\t{}",
        packet_summary(packet)
    ));
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

fn render_packets(
    packets: &[KernelPacket],
    theme: Option<&ThemeHandle>,
    separate_input_and_output: bool,
) -> Result<()> {
    let mut output_name: Option<&str> = None;
    let mut pending_message_identifier: Option<(&str, &str)> = None;
    let mut text_without_trailing_newline = false;
    let mut output_separator_pending = separate_input_and_output;

    for (index, packet) in packets.iter().enumerate() {
        match packet {
            KernelPacket::Text(text) => {
                if text_is_input_prompt(packets, index) {
                    text_without_trailing_newline = false;
                    continue;
                }
                if output_separator_pending {
                    print_kernel_text("\n")?;
                    output_separator_pending = false;
                }
                if let Some((symbol, tag)) = pending_message_identifier.take() {
                    print_kernel_message_text(text, symbol, tag, theme)?;
                } else {
                    print_kernel_text(text)?;
                }
                text_without_trailing_newline = !text.ends_with('\n');
            }
            KernelPacket::Message { symbol, tag } => {
                pending_message_identifier = Some((symbol, tag));
            }
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
                render_return_text(&text, output_name.take(), theme)?;
                if separate_input_and_output && !text.is_empty() {
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
                render_return_text(text, output_name.take(), theme)?;
                if separate_input_and_output && !text.is_empty() {
                    print_kernel_text("\n")?;
                }
            }
            KernelPacket::Syntax(position) => {
                print_kernel_text(&format!("Syntax error at position {position}\n"))?;
            }
            KernelPacket::BeginDialog(id) => {
                print_kernel_text(&format!("BeginDialogPacket[{id}]\n"))?;
            }
            KernelPacket::EndDialog(id) => {
                print_kernel_text(&format!("EndDialogPacket[{id}]\n"))?;
            }
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
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }

    if let Some(output_name) = output_name {
        print_kernel_text(&render_output_name_with_color(
            output_name,
            output_name_color_enabled(theme),
        ))?;
    }

    println!("{text}");
    Ok(())
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
        KernelExit, KernelPacket, kernel_exit_result, next_input_prompt_after_evaluation,
        render_message_text_with_color, render_output_name_with_color, wrap_to_string_query,
    };
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
    fn message_text_renders_short_identifier_red() {
        let rendered = render_message_text_with_color(
            "Power::infy: Infinite expression 1/0 encountered.\n",
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
            "                                 1\nPower::infy: Infinite expression - encountered.\n                                 0",
            "Power",
            "infy",
            true,
        );

        assert_eq!(
            rendered,
            format!(
                "                                 1\n{}: Infinite expression - encountered.\n                                 0",
                nu_ansi_term::Color::Red.paint("Power::infy")
            )
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
    fn message_text_only_colors_matching_identifier() {
        let text = "not a message: Power::infy appears later.\n";

        assert_eq!(
            render_message_text_with_color(text, "System`Power", "infy", true),
            text
        );
    }

    #[test]
    fn next_input_prompt_uses_non_empty_input_name_packet() {
        let packets = vec![
            KernelPacket::OutputName("Out[7]=".to_string()),
            KernelPacket::InputName("In[8]:=".to_string()),
        ];

        assert_eq!(
            next_input_prompt_after_evaluation(Some("In[7]:="), &packets),
            Some("In[8]:=".to_string())
        );
    }

    #[test]
    fn next_input_prompt_falls_back_to_output_name_when_input_name_is_empty() {
        let packets = vec![
            KernelPacket::OutputName("Out[7]=".to_string()),
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
