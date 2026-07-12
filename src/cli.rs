use std::{env, ffi::OsString, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::{
    commands::ConfigMode,
    kernel::{KernelClient, KernelConnection, KernelExit, ScriptInvocation},
    native_wstp::LinkProtocol,
    repl::run_repl,
    theme::{UserConfig, load_user_config},
};

#[derive(Debug, Parser)]
#[command(name = "wolfie")]
#[command(about = "A friendlier CLI interface for the Wolfram Kernel")]
struct Args {
    /// Disable Wolfram FrontEnd-backed completions and rendering support.
    #[arg(long = "no-frontend")]
    no_frontend: bool,

    /// Disable ANSI coloring.
    #[arg(long = "no-color")]
    no_color: bool,

    /// Start the REPL without printing the welcome banner.
    #[arg(long = "no-welcome")]
    no_welcome: bool,

    /// Disable REPL input/output prompts and the welcome banner.
    #[arg(long = "no-prompt")]
    no_prompt: bool,

    /// Enable inline ghost text completion hints.
    #[arg(
        long = "completion-ghost-text",
        conflicts_with = "no_completion_ghost_text"
    )]
    completion_ghost_text: bool,

    /// Disable inline ghost text completion hints.
    #[arg(long = "no-completion-ghost-text")]
    no_completion_ghost_text: bool,

    /// Disable the popup completion menu.
    #[arg(long = "no-completion-menu")]
    no_completion_menu: bool,

    /// Ignore user config and use fresh in-memory defaults for this session.
    #[arg(long = "skip-config")]
    skip_config: bool,

    /// Evaluate a Wolfram Language expression and exit.
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,

    /// Connect to an existing WSTP link instead of launching a new kernel.
    #[arg(long = "linkconnect")]
    link_connect: bool,

    /// WSTP link name to use with --linkconnect.
    #[arg(long = "linkname", value_name = "name")]
    link_name: Option<String>,

    /// WSTP link protocol to use with --linkconnect.
    #[arg(
        long = "linkprotocol",
        value_name = "protocol",
        value_parser = parse_link_protocol,
    )]
    link_protocol: Option<LinkProtocol>,

    /// WSTP link options integer to use when launching or connecting to a Wolfram Kernel.
    #[arg(long = "linkoptions", value_name = "val")]
    link_options: Option<u32>,

    /// Initialize a connected kernel by setting its directory to wolfie's launch directory.
    #[arg(long = "linkinit")]
    link_init: bool,

    /// WSTP link mode to use when launching or connecting to a Wolfram Kernel.
    #[arg(long = "linkmode", value_name = "mode")]
    link_mode: Option<String>,

    /// Execute a Wolfram Language script or package file and exit.
    #[arg(short = 'f', long = "file", value_name = "file")]
    file: Option<PathBuf>,

    /// Arguments passed to the script file.
    #[arg(last = true)]
    script_args: Vec<OsString>,
}

#[derive(Debug)]
struct NormalizedArgs {
    args: Vec<OsString>,
    direct_script: bool,
}

struct ParsedArgs {
    args: Args,
    direct_script: bool,
}

#[derive(Debug)]
struct EffectiveArgs {
    no_frontend: bool,
    no_color: bool,
    no_welcome: bool,
    no_prompt: bool,
    no_completion_ghost_text: bool,
    no_completion_menu: bool,
    config_mode: ConfigMode,
    eval: Option<String>,
    link_connect: bool,
    link_name: Option<String>,
    link_protocol: LinkProtocol,
    link_options: Option<u32>,
    link_init: bool,
    link_mode: Option<String>,
    file: Option<PathBuf>,
    script_args: Vec<OsString>,
    script_invocation: ScriptInvocation,
}

pub(crate) fn run() -> Result<()> {
    let parsed = parse_args();
    let config = if parsed.args.skip_config {
        UserConfig::default()
    } else {
        load_user_config()
    };
    let args = effective_args(parsed, config.clone())?;

    let use_color = !args.no_color;
    let link_init_directory = if args.link_init {
        Some(env::current_dir().context("failed to determine wolfie launch directory")?)
    } else {
        None
    };
    let connection = kernel_connection(&args, link_init_directory)?;
    let result = match (args.eval, args.file) {
        (Some(expr), None) => {
            KernelClient::with_connection(connection)?.evaluate_once(&expr, use_color)
        }
        (None, Some(file)) => KernelClient::with_connection(connection)?.evaluate_file(
            &file,
            &args.script_args,
            args.script_invocation,
            use_color,
        ),
        (None, None) => run_repl(
            !args.no_frontend,
            use_color,
            !args.no_welcome,
            !args.no_prompt,
            connection,
            config,
            args.config_mode,
            !args.no_completion_ghost_text,
            !args.no_completion_menu,
        ),
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

fn parse_args() -> ParsedArgs {
    let normalized = normalized_args(env::args_os());
    ParsedArgs {
        args: Args::parse_from(normalized.args),
        direct_script: normalized.direct_script,
    }
}

fn normalized_args<I>(args: I) -> NormalizedArgs
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    let Some(script_index) = direct_script_arg_index(&args) else {
        return NormalizedArgs {
            args,
            direct_script: false,
        };
    };

    let mut normalized = Vec::with_capacity(args.len() + 2);
    normalized.extend(args[..script_index].iter().cloned());
    normalized.push(OsString::from("--file"));
    normalized.push(args[script_index].clone());
    if args.len() > script_index + 1 {
        if args[script_index + 1] != "--" {
            normalized.push(OsString::from("--"));
        }
        normalized.extend(args[script_index + 1..].iter().cloned());
    }
    NormalizedArgs {
        args: normalized,
        direct_script: true,
    }
}

fn direct_script_arg_index(args: &[OsString]) -> Option<usize> {
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].to_string_lossy();
        if arg == "--" || arg == "-f" || arg == "--file" || arg.starts_with("--file=") {
            return None;
        }
        if value_option_consumes_next_arg(&arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

fn value_option_consumes_next_arg(arg: &str) -> bool {
    matches!(
        arg,
        "-e" | "--eval" | "--linkname" | "--linkprotocol" | "--linkoptions" | "--linkmode"
    )
}

fn effective_completion_ghost_text_disabled(
    cli_enable: bool,
    cli_disable: bool,
    command: &crate::theme::CommandConfig,
) -> bool {
    if cli_enable {
        return false;
    }
    if cli_disable {
        return true;
    }
    if let Some(enabled) = command.completion_ghost_text {
        return !enabled;
    }
    if let Some(disabled) = command.no_completion_ghost_text {
        return disabled;
    }

    true
}

fn effective_args(parsed: ParsedArgs, config: UserConfig) -> Result<EffectiveArgs> {
    let ParsedArgs {
        args,
        direct_script,
    } = parsed;
    let config_mode = if args.skip_config {
        ConfigMode::Ephemeral
    } else {
        ConfigMode::User
    };
    let config = if args.skip_config {
        UserConfig::default()
    } else {
        config
    };
    let command = config.command;
    let link_connect = args.link_connect || command.linkconnect.unwrap_or(false);
    let link_options = args.link_options.or(command.linkoptions);
    let link_init = args.link_init || command.linkinit.unwrap_or(false);

    if !link_connect {
        if args.link_name.is_some() {
            bail!("--linkname requires --linkconnect");
        }
        if args.link_protocol.is_some() {
            bail!("--linkprotocol requires --linkconnect");
        }
        if link_init {
            bail!("--linkinit requires --linkconnect");
        }
    }

    if link_init && link_options != Some(4) {
        bail!("--linkinit requires --linkoptions 4");
    }

    let link_protocol = if link_connect {
        match (args.link_protocol, command.linkprotocol.as_deref()) {
            (Some(protocol), _) => protocol,
            (None, Some(protocol)) => parse_link_protocol(protocol).map_err(anyhow::Error::msg)?,
            (None, None) => LinkProtocol::SharedMemory,
        }
    } else {
        LinkProtocol::SharedMemory
    };

    let no_prompt = args.no_prompt || command.no_prompt.unwrap_or(false);

    Ok(EffectiveArgs {
        no_frontend: args.no_frontend || command.no_frontend.unwrap_or(false),
        no_color: args.no_color || command.no_color.unwrap_or(false),
        no_welcome: no_prompt || args.no_welcome || command.no_welcome.unwrap_or(false),
        no_prompt,
        no_completion_ghost_text: effective_completion_ghost_text_disabled(
            args.completion_ghost_text,
            args.no_completion_ghost_text,
            &command,
        ),
        no_completion_menu: args.no_completion_menu || command.no_completion_menu.unwrap_or(false),
        eval: args.eval,
        link_connect,
        link_name: if link_connect {
            args.link_name.or(command.linkname)
        } else {
            None
        },
        link_protocol,
        link_options,
        link_init,
        link_mode: args.link_mode.or(command.linkmode),
        file: args.file,
        script_args: args.script_args,
        script_invocation: if direct_script {
            ScriptInvocation::Direct
        } else {
            ScriptInvocation::File
        },
        config_mode,
    })
}

fn kernel_connection(
    args: &EffectiveArgs,
    link_init_directory: Option<PathBuf>,
) -> Result<KernelConnection> {
    match (args.link_connect, args.link_name.as_ref()) {
        (true, Some(link_name)) => Ok(KernelConnection::Connect {
            link_name: link_name.clone(),
            link_protocol: args.link_protocol.clone(),
            link_options: args.link_options,
            link_init_directory,
            link_mode: args.link_mode.clone(),
        }),
        (true, None) => bail!("--linkconnect requires --linkname <name>"),
        (false, Some(_)) => bail!("--linkname requires --linkconnect"),
        (false, None) => Ok(KernelConnection::Launch {
            link_options: args.link_options,
            link_mode: args.link_mode.clone(),
        }),
    }
}

fn parse_link_protocol(value: &str) -> std::result::Result<LinkProtocol, String> {
    match value.to_ascii_lowercase().as_str() {
        "intraprocess" => Ok(LinkProtocol::IntraProcess),
        "sharedmemory" => Ok(LinkProtocol::SharedMemory),
        "tcpip" => Ok(LinkProtocol::TCPIP),
        _ => Err(format!(
            "unsupported WSTP link protocol {value:?}; expected IntraProcess, SharedMemory, or TCPIP"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::CommandConfig;
    use std::path::Path;

    fn parsed(args: Args) -> ParsedArgs {
        ParsedArgs {
            args,
            direct_script: false,
        }
    }

    fn effective(args: Args) -> EffectiveArgs {
        effective_args(parsed(args), UserConfig::default())
            .expect("args should merge with empty config")
    }

    fn effective_with_config(args: Args, config: UserConfig) -> EffectiveArgs {
        effective_args(parsed(args), config).expect("args should merge with config")
    }

    fn connection(args: &EffectiveArgs) -> Result<KernelConnection> {
        kernel_connection(args, None)
    }

    fn os_strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_file_flag_and_script_args() {
        let args =
            Args::try_parse_from(["wolfie", "--file", "script.wl", "--", "first", "--second"])
                .expect("file args should parse");
        let args = effective(args);

        assert_eq!(args.file.as_deref(), Some(Path::new("script.wl")));
        assert_eq!(args.script_args, os_strings(&["first", "--second"]));
        assert_eq!(args.script_invocation, ScriptInvocation::File);
    }

    #[test]
    fn parses_short_file_flag() {
        let args = Args::try_parse_from(["wolfie", "-f", "script.wl"])
            .expect("short file args should parse");
        let args = effective(args);

        assert_eq!(args.file.as_deref(), Some(Path::new("script.wl")));
    }

    #[test]
    fn normalizes_direct_script_invocation_for_shebang() {
        let normalized = normalized_args(os_strings(&["wolfie", "script.wl", "a", "--b"]));

        assert_eq!(
            normalized.args,
            os_strings(&["wolfie", "--file", "script.wl", "--", "a", "--b"])
        );
        assert!(normalized.direct_script);
    }

    #[test]
    fn direct_script_invocation_reaches_effective_args() {
        let normalized = normalized_args(os_strings(&["wolfie", "script.wl", "a", "--b"]));
        let args = Args::try_parse_from(normalized.args).expect("normalized args should parse");
        let args = effective_args(
            ParsedArgs {
                args,
                direct_script: normalized.direct_script,
            },
            UserConfig::default(),
        )
        .expect("direct script args should be effective");

        assert_eq!(args.file.as_deref(), Some(Path::new("script.wl")));
        assert_eq!(args.script_args, os_strings(&["a", "--b"]));
        assert_eq!(args.script_invocation, ScriptInvocation::Direct);
    }

    #[test]
    fn normalizes_direct_script_invocation_after_options() {
        let normalized = normalized_args(os_strings(&["wolfie", "--no-color", "script.wl"]));

        assert_eq!(
            normalized.args,
            os_strings(&["wolfie", "--no-color", "--file", "script.wl"])
        );
        assert!(normalized.direct_script);
    }

    #[test]
    fn normalizes_direct_script_invocation_after_skip_config() {
        let normalized = normalized_args(os_strings(&["wolfie", "--skip-config", "script.wl"]));

        assert_eq!(
            normalized.args,
            os_strings(&["wolfie", "--skip-config", "--file", "script.wl"])
        );
        assert!(normalized.direct_script);
    }

    #[test]
    fn parses_repl_display_flags() {
        let args = Args::try_parse_from(["wolfie", "--no-welcome", "--no-prompt"])
            .expect("display flags should parse");
        let args = effective(args);

        assert!(args.no_welcome);
        assert!(args.no_prompt);
    }

    #[test]
    fn completion_ghost_text_is_disabled_by_default() {
        let args = effective(Args::try_parse_from(["wolfie"]).expect("default args should parse"));

        assert!(args.no_completion_ghost_text);
    }

    #[test]
    fn parses_completion_ui_flags() {
        let args =
            Args::try_parse_from(["wolfie", "--completion-ghost-text", "--no-completion-menu"])
                .expect("completion UI flags should parse");
        let args = effective(args);

        assert!(!args.no_completion_ghost_text);
        assert!(args.no_completion_menu);
    }

    #[test]
    fn no_completion_ghost_text_flag_disables_ghost_text() {
        let args = Args::try_parse_from(["wolfie", "--no-completion-ghost-text"])
            .expect("completion UI flags should parse");
        let args = effective(args);

        assert!(args.no_completion_ghost_text);
    }

    #[test]
    fn no_prompt_also_disables_welcome() {
        let args = Args::try_parse_from(["wolfie", "--no-prompt"]).expect("no-prompt should parse");
        let args = effective(args);

        assert!(args.no_prompt);
        assert!(args.no_welcome);
    }

    #[test]
    fn normalizes_direct_script_invocation_after_repl_display_flags() {
        let normalized = normalized_args(os_strings(&[
            "wolfie",
            "--no-welcome",
            "--no-prompt",
            "script.wl",
        ]));

        assert_eq!(
            normalized.args,
            os_strings(&[
                "wolfie",
                "--no-welcome",
                "--no-prompt",
                "--file",
                "script.wl",
            ])
        );
        assert!(normalized.direct_script);
    }

    #[test]
    fn normalizes_direct_script_invocation_with_explicit_separator() {
        let normalized = normalized_args(os_strings(&["wolfie", "script.wl", "--", "a"]));

        assert_eq!(
            normalized.args,
            os_strings(&["wolfie", "--file", "script.wl", "--", "a"])
        );
        assert!(normalized.direct_script);
    }

    #[test]
    fn does_not_normalize_option_invocation_as_script() {
        let normalized = normalized_args(os_strings(&["wolfie", "--eval", "2 + 2"]));

        assert_eq!(normalized.args, os_strings(&["wolfie", "--eval", "2 + 2"]));
        assert!(!normalized.direct_script);
    }

    #[test]
    fn parses_linkconnect_linkname_as_connected_kernel() {
        let args = Args::try_parse_from(["wolfie", "--linkconnect", "--linkname", "test-link"])
            .expect("linkconnect args should parse");

        let args = effective(args);
        let connection = connection(&args).expect("linkconnect args should be valid");

        match connection {
            KernelConnection::Connect {
                link_name,
                link_protocol,
                ..
            } => {
                assert_eq!(link_name, "test-link");
                assert_eq!(link_protocol, LinkProtocol::SharedMemory);
            }
            KernelConnection::Launch { .. } => panic!("expected connected kernel mode"),
        }
    }

    #[test]
    fn parses_custom_linkprotocol_for_connected_kernel() {
        let args = Args::try_parse_from([
            "wolfie",
            "--linkconnect",
            "--linkname",
            "test-link",
            "--linkprotocol",
            "TCPIP",
        ])
        .expect("linkconnect args should parse");

        let args = effective(args);
        let connection = connection(&args).expect("linkconnect args should be valid");

        match connection {
            KernelConnection::Connect { link_protocol, .. } => {
                assert_eq!(link_protocol, LinkProtocol::TCPIP);
            }
            KernelConnection::Launch { .. } => panic!("expected connected kernel mode"),
        }
    }

    #[test]
    fn rejects_unknown_linkprotocol() {
        let err = Args::try_parse_from([
            "wolfie",
            "--linkconnect",
            "--linkname",
            "test-link",
            "--linkprotocol",
            "Bogus",
        ])
        .expect_err("unknown linkprotocol should be rejected");

        assert!(err.to_string().contains("unsupported WSTP link protocol"));
    }

    #[test]
    fn rejects_linkconnect_without_linkname() {
        let args = Args::try_parse_from(["wolfie", "--linkconnect"])
            .expect("linkconnect without linkname should parse before validation");

        let args = effective(args);
        let err = connection(&args).expect_err("linkconnect should require linkname");

        assert!(
            err.to_string()
                .contains("--linkconnect requires --linkname")
        );
    }

    #[test]
    fn parses_linkoptions_as_launched_kernel_options() {
        let args = Args::try_parse_from(["wolfie", "--linkoptions", "4"])
            .expect("linkoptions should parse");

        let args = effective(args);
        let connection = connection(&args).expect("linkoptions should be valid");

        match connection {
            KernelConnection::Launch { link_options, .. } => {
                assert_eq!(link_options, Some(4));
            }
            KernelConnection::Connect { .. } => panic!("expected launched kernel mode"),
        }
    }

    #[test]
    fn parses_linkmode_as_launched_kernel_option() {
        let args = Args::try_parse_from(["wolfie", "--linkmode", "Listen"])
            .expect("linkmode should parse");

        let args = effective(args);
        let connection = connection(&args).expect("linkmode should be valid");

        match connection {
            KernelConnection::Launch { link_mode, .. } => {
                assert_eq!(link_mode.as_deref(), Some("Listen"));
            }
            KernelConnection::Connect { .. } => panic!("expected launched kernel mode"),
        }
    }

    #[test]
    fn parses_linkoptions_and_linkmode_with_linkconnect() {
        let args = Args::try_parse_from([
            "wolfie",
            "--linkconnect",
            "--linkname",
            "test-link",
            "--linkoptions",
            "4",
            "--linkmode",
            "Connect",
        ])
        .expect("linkoptions should parse before validation");

        let args = effective(args);
        let connection = connection(&args).expect("linkoptions should apply to connected links");

        match connection {
            KernelConnection::Connect {
                link_options,
                link_mode,
                ..
            } => {
                assert_eq!(link_options, Some(4));
                assert_eq!(link_mode.as_deref(), Some("Connect"));
            }
            KernelConnection::Launch { .. } => panic!("expected connected kernel mode"),
        }
    }

    #[test]
    fn parses_linkinit_for_linkoption_four_connected_kernel() {
        let args = Args::try_parse_from([
            "wolfie",
            "--linkconnect",
            "--linkname",
            "test-link",
            "--linkoptions",
            "4",
            "--linkinit",
        ])
        .expect("linkinit args should parse");

        let args = effective(args);
        let launch_directory = PathBuf::from("/tmp/wolfie-launch-dir");
        let connection = kernel_connection(&args, Some(launch_directory.clone()))
            .expect("linkinit args should be valid");

        match connection {
            KernelConnection::Connect {
                link_init_directory,
                ..
            } => {
                assert_eq!(link_init_directory, Some(launch_directory));
            }
            KernelConnection::Launch { .. } => panic!("expected connected kernel mode"),
        }
    }

    #[test]
    fn rejects_linkinit_without_linkoption_four() {
        let args = Args::try_parse_from([
            "wolfie",
            "--linkconnect",
            "--linkname",
            "test-link",
            "--linkinit",
        ])
        .expect("linkinit should parse before validation");

        let err = effective_args(parsed(args), UserConfig::default())
            .expect_err("linkinit should require linkoptions 4");

        assert!(
            err.to_string()
                .contains("--linkinit requires --linkoptions 4")
        );
    }

    #[test]
    fn rejects_linkinit_without_linkconnect() {
        let args = Args::try_parse_from(["wolfie", "--linkoptions", "4", "--linkinit"])
            .expect("linkinit should parse before validation");

        let err = effective_args(parsed(args), UserConfig::default())
            .expect_err("linkinit should require linkconnect");

        assert!(
            err.to_string()
                .contains("--linkinit requires --linkconnect")
        );
    }

    #[test]
    fn applies_config_defaults_when_cli_options_are_absent() {
        let args = Args::try_parse_from(["wolfie"]).expect("empty args should parse");
        let args = effective_with_config(
            args,
            UserConfig {
                command: CommandConfig {
                    no_frontend: Some(true),
                    no_color: Some(true),
                    no_welcome: Some(true),
                    no_prompt: Some(true),
                    no_completion_ghost_text: Some(true),
                    no_completion_menu: Some(true),
                    linkconnect: Some(true),
                    linkname: Some("config-link".to_string()),
                    linkprotocol: Some("TCPIP".to_string()),
                    ..CommandConfig::default()
                },
                ..UserConfig::default()
            },
        );

        assert!(args.no_frontend);
        assert!(args.no_color);
        assert!(args.no_welcome);
        assert!(args.no_prompt);
        assert!(args.no_completion_ghost_text);
        assert!(args.no_completion_menu);
        let connection = connection(&args).expect("config defaults should be valid");

        match connection {
            KernelConnection::Connect {
                link_name,
                link_protocol,
                ..
            } => {
                assert_eq!(link_name, "config-link");
                assert_eq!(link_protocol, LinkProtocol::TCPIP);
            }
            KernelConnection::Launch { .. } => panic!("expected connected kernel mode"),
        }
    }

    #[test]
    fn config_no_prompt_also_disables_welcome() {
        let args = Args::try_parse_from(["wolfie"]).expect("empty args should parse");
        let args = effective_with_config(
            args,
            UserConfig {
                command: CommandConfig {
                    no_prompt: Some(true),
                    ..CommandConfig::default()
                },
                ..UserConfig::default()
            },
        );

        assert!(args.no_prompt);
        assert!(args.no_welcome);
    }

    #[test]
    fn skip_config_ignores_config_defaults() {
        let args = Args::try_parse_from(["wolfie", "--skip-config"])
            .expect("skip-config args should parse");
        let args = effective_with_config(
            args,
            UserConfig {
                command: CommandConfig {
                    no_frontend: Some(true),
                    no_color: Some(true),
                    no_welcome: Some(true),
                    no_prompt: Some(true),
                    no_completion_ghost_text: Some(true),
                    no_completion_menu: Some(true),
                    linkconnect: Some(true),
                    linkname: Some("config-link".to_string()),
                    linkprotocol: Some("TCPIP".to_string()),
                    ..CommandConfig::default()
                },
                ..UserConfig::default()
            },
        );

        assert_eq!(args.config_mode, ConfigMode::Ephemeral);
        assert!(!args.no_frontend);
        assert!(!args.no_color);
        assert!(!args.no_welcome);
        assert!(!args.no_prompt);
        assert!(args.no_completion_ghost_text);
        assert!(!args.no_completion_menu);
        assert!(!args.link_connect);
        let connection = connection(&args).expect("default args should launch a kernel");
        assert!(matches!(connection, KernelConnection::Launch { .. }));
    }

    #[test]
    fn parses_command_config_from_argument_name_keys() {
        let config: UserConfig = serde_json::from_str(
            r#"{
              "command": {
                "no-frontend": true,
                "no-color": true,
                "no-welcome": true,
                "no-prompt": true,
                "completion-ghost-text": true,
                "no-completion-ghost-text": true,
                "no-completion-menu": true,
                "linkconnect": true,
                "linkname": "config-link",
                "linkprotocol": "SharedMemory",
                "linkmode": "Listen",
                "linkoptions": 4,
                "linkinit": true
              }
            }"#,
        )
        .expect("command config should deserialize");

        assert_eq!(config.command.no_frontend, Some(true));
        assert_eq!(config.command.no_color, Some(true));
        assert_eq!(config.command.no_welcome, Some(true));
        assert_eq!(config.command.no_prompt, Some(true));
        assert_eq!(config.command.completion_ghost_text, Some(true));
        assert_eq!(config.command.no_completion_ghost_text, Some(true));
        assert_eq!(config.command.no_completion_menu, Some(true));
        assert_eq!(config.command.linkconnect, Some(true));
        assert_eq!(config.command.linkname.as_deref(), Some("config-link"));
        assert_eq!(config.command.linkprotocol.as_deref(), Some("SharedMemory"));
        assert_eq!(config.command.linkmode.as_deref(), Some("Listen"));
        assert_eq!(config.command.linkoptions, Some(4));
        assert_eq!(config.command.linkinit, Some(true));
    }

    #[test]
    fn applies_config_linkoptions_to_launched_kernel() {
        let args = Args::try_parse_from(["wolfie"]).expect("empty args should parse");
        let args = effective_with_config(
            args,
            UserConfig {
                command: CommandConfig {
                    linkoptions: Some(4),
                    linkmode: Some("Listen".to_string()),
                    ..CommandConfig::default()
                },
                ..UserConfig::default()
            },
        );

        let connection = connection(&args).expect("config linkoptions should be valid");

        match connection {
            KernelConnection::Launch {
                link_options,
                link_mode,
            } => {
                assert_eq!(link_options, Some(4));
                assert_eq!(link_mode.as_deref(), Some("Listen"));
            }
            KernelConnection::Connect { .. } => panic!("expected launched kernel mode"),
        }
    }

    #[test]
    fn cli_linkprotocol_overrides_config_default() {
        let args = Args::try_parse_from([
            "wolfie",
            "--linkconnect",
            "--linkname",
            "cli-link",
            "--linkprotocol",
            "TCPIP",
        ])
        .expect("linkconnect args should parse");
        let args = effective_with_config(
            args,
            UserConfig {
                command: CommandConfig {
                    linkprotocol: Some("SharedMemory".to_string()),
                    ..CommandConfig::default()
                },
                ..UserConfig::default()
            },
        );

        let connection = connection(&args).expect("linkconnect args should be valid");

        match connection {
            KernelConnection::Connect { link_protocol, .. } => {
                assert_eq!(link_protocol, LinkProtocol::TCPIP);
            }
            KernelConnection::Launch { .. } => panic!("expected connected kernel mode"),
        }
    }

    #[test]
    fn rejects_invalid_config_linkprotocol() {
        let args = Args::try_parse_from(["wolfie"]).expect("empty args should parse");
        let err = effective_args(
            parsed(args),
            UserConfig {
                command: CommandConfig {
                    linkconnect: Some(true),
                    linkname: Some("config-link".to_string()),
                    linkprotocol: Some("Bogus".to_string()),
                    ..CommandConfig::default()
                },
                ..UserConfig::default()
            },
        )
        .expect_err("invalid config linkprotocol should be rejected");

        assert!(err.to_string().contains("unsupported WSTP link protocol"));
    }
}
