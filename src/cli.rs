use std::{ffi::OsString, path::PathBuf};

use anyhow::{Result, bail};
use clap::Parser;

use crate::{
    kernel::{KernelClient, KernelConnection, KernelExit, run_wolframscript_file},
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

    /// WSTP link options integer to pass to a launched Wolfram Kernel.
    #[arg(long = "linkoptions", value_name = "val")]
    link_options: Option<u32>,

    /// WSTP link mode to pass to a launched Wolfram Kernel.
    #[arg(long = "linkmode", value_name = "mode")]
    link_mode: Option<String>,

    /// Execute a Wolfram Language script or package file and exit.
    file: Option<PathBuf>,

    /// Arguments passed to the script file.
    #[arg(last = true)]
    script_args: Vec<OsString>,
}

#[derive(Debug)]
struct EffectiveArgs {
    no_frontend: bool,
    no_color: bool,
    eval: Option<String>,
    link_connect: bool,
    link_name: Option<String>,
    link_protocol: LinkProtocol,
    link_options: Option<u32>,
    link_mode: Option<String>,
    file: Option<PathBuf>,
    script_args: Vec<OsString>,
}

pub(crate) fn run() -> Result<()> {
    let args = effective_args(Args::parse(), load_user_config())?;

    let use_color = !args.no_color;
    let connection = kernel_connection(&args)?;
    let result = match (args.eval, args.file) {
        (Some(expr), None) => {
            KernelClient::with_connection(connection)?.evaluate_once(&expr, use_color)
        }
        (None, Some(file)) => match &connection {
            KernelConnection::Launch {
                link_options: None,
                link_mode: None,
            } => run_wolframscript_file(file, args.script_args),
            KernelConnection::Launch { .. } => {
                bail!(
                    "--linkoptions and --linkmode are only supported for the REPL and --eval modes"
                )
            }
            KernelConnection::Connect { .. } => {
                bail!("--linkconnect is only supported for the REPL and --eval modes")
            }
        },
        (None, None) => run_repl(!args.no_frontend, use_color, connection),
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

fn effective_args(args: Args, config: UserConfig) -> Result<EffectiveArgs> {
    let command = config.command;
    let link_connect = args.link_connect || command.linkconnect.unwrap_or(false);

    if !link_connect {
        if args.link_name.is_some() {
            bail!("--linkname requires --linkconnect");
        }
        if args.link_protocol.is_some() {
            bail!("--linkprotocol requires --linkconnect");
        }
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

    Ok(EffectiveArgs {
        no_frontend: args.no_frontend || command.no_frontend.unwrap_or(false),
        no_color: args.no_color || command.no_color.unwrap_or(false),
        eval: args.eval,
        link_connect,
        link_name: if link_connect {
            args.link_name.or(command.linkname)
        } else {
            None
        },
        link_protocol,
        link_options: args.link_options.or(command.linkoptions),
        link_mode: args.link_mode.or(command.linkmode),
        file: args.file,
        script_args: args.script_args,
    })
}

fn kernel_connection(args: &EffectiveArgs) -> Result<KernelConnection> {
    match (args.link_connect, args.link_name.as_ref()) {
        (true, Some(link_name)) => Ok(KernelConnection::Connect {
            link_name: link_name.clone(),
            link_protocol: args.link_protocol.clone(),
            link_options: args.link_options.clone(),
            link_mode: args.link_mode.clone(),
        }),
        (true, None) => bail!("--linkconnect requires --linkname <name>"),
        (false, Some(_)) => bail!("--linkname requires --linkconnect"),
        (false, None) => Ok(KernelConnection::Launch {
            link_options: args.link_options.clone(),
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

    fn effective(args: Args) -> EffectiveArgs {
        effective_args(args, UserConfig::default()).expect("args should merge with empty config")
    }

    fn effective_with_config(args: Args, config: UserConfig) -> EffectiveArgs {
        effective_args(args, config).expect("args should merge with config")
    }

    #[test]
    fn parses_linkconnect_linkname_as_connected_kernel() {
        let args = Args::try_parse_from(["wolfie", "--linkconnect", "--linkname", "test-link"])
            .expect("linkconnect args should parse");

        let args = effective(args);
        let connection = kernel_connection(&args).expect("linkconnect args should be valid");

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
        let connection = kernel_connection(&args).expect("linkconnect args should be valid");

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
        let err = kernel_connection(&args).expect_err("linkconnect should require linkname");

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
        let connection = kernel_connection(&args).expect("linkoptions should be valid");

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
        let connection = kernel_connection(&args).expect("linkmode should be valid");

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
        let connection =
            kernel_connection(&args).expect("linkoptions should apply to connected links");

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
    fn applies_config_defaults_when_cli_options_are_absent() {
        let args = Args::try_parse_from(["wolfie"]).expect("empty args should parse");
        let args = effective_with_config(
            args,
            UserConfig {
                command: CommandConfig {
                    no_frontend: Some(true),
                    no_color: Some(true),
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
        let connection = kernel_connection(&args).expect("config defaults should be valid");

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
    fn parses_command_config_from_argument_name_keys() {
        let config: UserConfig = serde_json::from_str(
            r#"{
              "command": {
                "no-frontend": true,
                "no-color": true,
                "linkconnect": true,
                "linkname": "config-link",
                "linkprotocol": "SharedMemory",
                "linkmode": "Listen",
                "linkoptions": 4
              }
            }"#,
        )
        .expect("command config should deserialize");

        assert_eq!(config.command.no_frontend, Some(true));
        assert_eq!(config.command.no_color, Some(true));
        assert_eq!(config.command.linkconnect, Some(true));
        assert_eq!(config.command.linkname.as_deref(), Some("config-link"));
        assert_eq!(config.command.linkprotocol.as_deref(), Some("SharedMemory"));
        assert_eq!(config.command.linkmode.as_deref(), Some("Listen"));
        assert_eq!(config.command.linkoptions, Some(4));
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

        let connection = kernel_connection(&args).expect("config linkoptions should be valid");

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

        let connection = kernel_connection(&args).expect("linkconnect args should be valid");

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
            args,
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
