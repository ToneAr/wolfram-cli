use std::{
    io,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use wolfram_expr::{Expr, Symbol};
use wstp::{Link, Protocol, sys};

use crate::{
    highlighter::print_highlighted,
    kernel::{KernelExit, kernel_path},
    profiler::profile_duration,
    theme::ThemeHandle,
    wl::{
        TO_EXPRESSION_WITHOUT_SHADOWING_WL, WSTP_EVALUATE_INPUT_TO_STRING_WL,
        wolfram_string_literal, wolfram_user_input_evaluation_expr,
    },
};

fn print_kernel_text(text: &str) -> Result<()> {
    use std::io::Write;
    print!("{text}");
    io::stdout().flush().context("failed to flush stdout")
}

pub(crate) struct WstpKernelClient {
    process: Child,
    link: Option<Link>,
}

impl WstpKernelClient {
    pub(crate) fn launch() -> Result<Self> {
        let start = Instant::now();
        let path = kernel_path()?;
        let mut link = Link::listen(Protocol::SharedMemory, "")
            .map_err(|err| anyhow!("failed to create WSTP listener: {err:?}"))?;
        let link_name = link.link_name();
        let spawn_start = Instant::now();
        let process = Command::new(path)
            .arg("-wstp")
            .arg("-linkprotocol")
            .arg("SharedMemory")
            .arg("-linkconnect")
            .arg("-linkname")
            .arg(&link_name)
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
        })
    }

    pub(crate) fn evaluate_once(
        &mut self,
        input: &str,
        line_number: Option<(usize, &ThemeHandle)>,
    ) -> Result<()> {
        let text = self.evaluate_input_to_string(input)?;
        if !text.is_empty() {
            if let Some((line_number, theme)) = line_number {
                print!("Out[{line_number}]= ");
                print_highlighted(&text, theme.current());
            } else {
                println!("{text}");
            }
        }
        Ok(())
    }

    fn evaluate_input_to_string(&mut self, input: &str) -> Result<String> {
        let input_expr = wolfram_user_input_evaluation_expr(input);
        let wrapped_input = WSTP_EVALUATE_INPUT_TO_STRING_WL.replace("__INPUT_EXPR__", &input_expr);
        let expr = wstp_safe_to_expression(&wrapped_input);
        self.evaluate_packet_to_string(&expr)
    }

    pub(crate) fn evaluate_to_string(&mut self, input: &str) -> Result<String> {
        let expr = call(
            "System`ToString",
            vec![
                call("System`ToExpression", vec![Expr::string(input)]),
                symbol("System`InputForm"),
            ],
        );
        self.evaluate_packet_to_string(&expr)
    }

    fn evaluate_packet_to_string(&mut self, expr: &Expr) -> Result<String> {
        let start = Instant::now();
        let link = self.link.as_mut().context("WSTP link is closed")?;
        link.put_eval_packet(expr)
            .map_err(|err| anyhow!("failed to send WSTP evaluate packet: {err:?}"))?;
        link.flush()
            .map_err(|err| anyhow!("failed to flush WSTP link: {err:?}"))?;
        profile_duration("wstp.eval.sent", start.elapsed(), "");

        loop {
            let packet = match link.raw_next_packet() {
                Ok(packet) => packet,
                Err(err) => {
                    if let Some(code) = Self::child_exit_code_after_link_error(&mut self.process) {
                        return Err(KernelExit::new(code).into());
                    }
                    return Err(anyhow!("failed to read WSTP packet: {err:?}"));
                }
            };
            match packet {
                sys::RETURNPKT => {
                    let text = link
                        .get_string()
                        .map_err(|err| anyhow!("failed to read WSTP return value: {err:?}"))?;
                    profile_duration(
                        "wstp.eval.total",
                        start.elapsed(),
                        format!("bytes={}", text.len()),
                    );
                    return Ok(text);
                }
                sys::TEXTPKT | sys::RETURNTEXTPKT => {
                    let text = link
                        .get_string()
                        .map_err(|err| anyhow!("failed to read WSTP text packet: {err:?}"))?;
                    print_kernel_text(&text)?;
                    link.new_packet()
                        .map_err(|err| anyhow!("failed to finish WSTP text packet: {err:?}"))?;
                }
                sys::INPUTNAMEPKT => {
                    // INPUTNAMEPKT only announces the next prompt label
                    // (for example In[1]:=). It is not an input request.
                    // Reading stdin here races reedline and makes typing
                    // appear to hang while background WSTP queries run.
                    link.new_packet().map_err(|err| {
                        anyhow!("failed to finish WSTP input name packet: {err:?}")
                    })?;
                }
                sys::INPUTPKT | sys::INPUTSTRPKT => {
                    let mut input = String::new();
                    io::stdin()
                        .read_line(&mut input)
                        .context("failed to read kernel input from stdin")?;
                    link.new_packet()
                        .map_err(|err| anyhow!("failed to finish WSTP input packet: {err:?}"))?;
                    link.put_str(input.trim_end_matches(['\r', '\n']))
                        .map_err(|err| anyhow!("failed to send WSTP input response: {err:?}"))?;
                    link.end_packet().map_err(|err| {
                        anyhow!("failed to finish WSTP input response packet: {err:?}")
                    })?;
                    link.flush()
                        .map_err(|err| anyhow!("failed to flush WSTP input response: {err:?}"))?;
                }
                _ => {
                    link.new_packet()
                        .map_err(|err| anyhow!("failed to skip WSTP packet: {err:?}"))?;
                }
            }
        }
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

fn symbol(name: &str) -> Expr {
    Expr::symbol(Symbol::try_new(name).expect("internal symbol names are qualified"))
}

fn call(head: &str, args: Vec<Expr>) -> Expr {
    Expr::normal(symbol(head), args)
}

fn wstp_safe_to_expression(input: &str) -> Expr {
    call(
        "System`ToExpression",
        vec![Expr::string(
            &TO_EXPRESSION_WITHOUT_SHADOWING_WL
                .replace("__INPUT__", &wolfram_string_literal(input)),
        )],
    )
}
