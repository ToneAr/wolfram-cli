use std::{ffi::OsString, path::PathBuf};

use anyhow::{Result, bail};
use clap::Parser;

use crate::{
    kernel::{KernelClient, KernelExit, run_wolframscript_file},
    repl::run_repl,
};

#[derive(Debug, Parser)]
#[command(name = "wolfish")]
#[command(about = "A friendlier CLI interface for the Wolfram Kernel")]
struct Args {
    /// Disable Wolfram FrontEnd-backed completions and rendering support.
    #[arg(long = "no-frontend")]
    no_frontend: bool,

    /// Disable ANSI coloring.
    #[arg(long = "no-color")]
    no_color: bool,

    /// Evaluate a Wolfram Language expression and exit.
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,

    /// Execute a Wolfram Language script or package file and exit.
    file: Option<PathBuf>,

    /// Arguments passed to the script file.
    #[arg(last = true)]
    script_args: Vec<OsString>,
}

pub(crate) fn run() -> Result<()> {
    let args = Args::parse();

    let use_color = !args.no_color;
    let result = match (args.eval, args.file) {
        (Some(expr), None) => KernelClient::new()?.evaluate_once(&expr, use_color),
        (None, Some(file)) => run_wolframscript_file(file, args.script_args),
        (None, None) => run_repl(!args.no_frontend, use_color),
        (Some(_), Some(_)) => bail!("use either --eval or a file, not both"),
    };

    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            if let Some(exit) = err.downcast_ref::<KernelExit>() {
                std::process::exit(exit.code);
            }
            Err(err)
        }
    }
}
