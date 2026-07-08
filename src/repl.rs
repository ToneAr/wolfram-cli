use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Result;
use reedline::{FileBackedHistory, Reedline, ReedlineMenu, Signal};

use crate::{
    commands::{CommandAction, execute_repl_command},
    completion::{CompletionSource, WolframCompleter, builtin_symbol_names},
    editor::{
        WolframPrompt, WolframValidator, completion_edit_mode, completion_menu, history_path,
    },
    frontend::{FrontEndClient, frontend_status},
    highlighter::WolframHighlighter,
    kernel::{
        KernelClient, SharedKernel, kernel_may_be_slow_to_respond, kernel_status, lock_kernel,
        spawn_kernel_warmup,
    },
    theme::{Theme, ThemeHandle},
    wolfram_syntax::remember_user_symbols,
};

pub(crate) fn run_repl(enable_frontend: bool) -> Result<()> {
    let history = history_path()?;
    let completion_epoch = Arc::new(AtomicU64::new(0));
    let user_symbols = Arc::new(Mutex::new(HashSet::new()));
    let kernel = Arc::new(Mutex::new(KernelClient::new()?));
    let frontend = if enable_frontend {
        Some(Arc::new(Mutex::new(FrontEndClient::new())))
    } else {
        None
    };
    spawn_kernel_warmup(kernel.clone());
    print_welcome(&kernel, frontend.as_ref())?;
    let theme = ThemeHandle::new(Theme::Dark);
    let completion_source = CompletionSource::new(
        kernel.clone(),
        completion_epoch.clone(),
        user_symbols.clone(),
    );
    let symbol_set = builtin_symbol_names().collect();
    let mut line_editor = Reedline::create()
        .use_kitty_keyboard_enhancement(true)
        .with_history(Box::new(FileBackedHistory::with_file(2000, history)?))
        .with_highlighter(Box::new(WolframHighlighter::new(symbol_set, theme.clone())))
        .with_completer(Box::new(WolframCompleter::new(
            completion_source,
            theme.clone(),
        )))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(completion_menu())))
        .with_validator(Box::new(WolframValidator))
        .with_edit_mode(Box::new(completion_edit_mode()));
    let mut line_number = 1;
    loop {
        let prompt = WolframPrompt {
            line_number,
            kernel_status: kernel_status(&kernel)?,
            frontend_status: frontend_status(frontend.as_ref())?,
        };
        match line_editor.read_line(&prompt)? {
            Signal::Success(input) => {
                let input = input.trim();
                if input.is_empty() {
                    continue;
                }
                if matches!(input, "Exit" | "Quit") {
                    break;
                }
                if input.starts_with(':') {
                    if execute_repl_command(input, &theme) == CommandAction::Quit {
                        break;
                    }
                    continue;
                }
                if kernel_may_be_slow_to_respond(kernel_status(&kernel)?) {
                    println!(
                        "(kernel is still starting up; the first response can take a few seconds)"
                    );
                }
                lock_kernel(&kernel)?.evaluate_repl_input(input, line_number, &theme)?;
                remember_user_symbols(input, &user_symbols);
                completion_epoch.fetch_add(1, Ordering::Relaxed);
                line_number += 1;
            }
            Signal::CtrlC => continue,
            Signal::CtrlD => break,
        }
    }

    Ok(())
}

fn print_welcome(
    kernel: &SharedKernel,
    frontend: Option<&Arc<Mutex<FrontEndClient>>>,
) -> Result<()> {
    println!("Wolfram CLI");
    println!("{}", kernel_status(kernel)?);
    println!("{}", frontend_status(frontend)?);
    println!("Version: {}", env!("CARGO_PKG_VERSION"));
    println!("Type :help for commands, :quit or Ctrl-D to quit.\n");
    Ok(())
}
