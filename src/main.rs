mod cli;
mod commands;
mod completion;
mod editor;
mod frontend;
mod highlighter;
mod interrupt;
mod kernel;
mod native_wstp;
mod profiler;
mod repl;
mod theme;
mod wl;
mod wolfram_syntax;

#[cfg(test)]
mod tests;

fn main() -> anyhow::Result<()> {
    interrupt::install_ctrlc_handler()?;
    cli::run()
}
