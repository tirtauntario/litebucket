//! Command-line interface.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::config::{Config, Overrides};
use crate::error::{Error, Result};
use crate::store::Store;

#[derive(Parser, Debug)]
#[command(
    name = "litebucket",
    version,
    about = "Compact single-host S3-compatible object storage"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigArg {
    /// Path to the TOML configuration file.
    #[arg(long, env = "LITEBUCKET_CONFIG", default_value = "./config.toml")]
    pub(crate) config: PathBuf,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a new store in an empty data directory, plus the master key
    /// (when missing) and a first admin access key.
    Init {
        #[command(flatten)]
        cfg: ConfigArg,
        /// Write the admin key to this new file (mode 0600) instead of
        /// printing the secret.
        #[arg(long)]
        admin_key_output: Option<PathBuf>,
    },
    /// Run the S3, management, and admin listeners.
    Serve {
        #[command(flatten)]
        cfg: ConfigArg,
        /// Override http.listen.
        #[arg(long)]
        listen: Option<String>,
        /// Override management.listen.
        #[arg(long)]
        management_listen: Option<String>,
        /// Override logging.level.
        #[arg(long)]
        log_level: Option<String>,
    },
    /// Configuration commands.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage access keys, grants, and buckets through the running server.
    Admin(crate::admin::commands::AdminArgs),
    /// Master key file commands.
    MasterKey {
        #[command(subcommand)]
        command: MasterKeyCommand,
    },
    /// Query the management readiness endpoint (exit 0 when ready).
    Healthcheck {
        #[arg(long, default_value = "http://127.0.0.1:9001/readyz")]
        url: String,
    },
    /// Offline diagnostics: schema, settings, references, counters, filesystem.
    Doctor(ConfigArg),
    /// Offline verification; `--full` hashes every referenced file and reports untracked files.
    Check {
        #[command(flatten)]
        cfg: ConfigArg,
        #[arg(long)]
        full: bool,
    },
    /// Offline garbage collection of tracked garbage (dry run by default).
    Gc {
        #[command(flatten)]
        cfg: ConfigArg,
        /// Only report (default).
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Delete eligible tracked garbage.
        #[arg(long)]
        apply: bool,
    },
    /// Offline consistent backup into a new directory.
    Backup {
        #[command(flatten)]
        cfg: ConfigArg,
        #[arg(long)]
        destination: PathBuf,
    },
    /// Restore a verified backup into a new, empty data directory.
    Restore {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        data_dir: PathBuf,
        /// Master key the backup's secrets are encrypted with; restore checks
        /// that it decrypts every access key.
        #[arg(long)]
        master_key_file: Option<PathBuf>,
        /// Restore even though encrypted access keys cannot be checked (no
        /// master key). Recover access afterwards with `admin recover --reset-keys`.
        #[arg(long)]
        skip_key_check: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Validate configuration structure without starting listeners.
    Check(ConfigArg),
    /// Print a commented configuration file with every setting and its default.
    Template {
        /// Paths and listeners for the Docker image (/data, /run/secrets, TLS).
        #[arg(long)]
        docker: bool,
    },
}

#[derive(Subcommand, Debug)]
enum MasterKeyCommand {
    /// Write a new random 256-bit master key to a new file (mode 0600).
    Generate {
        #[arg(long)]
        output: PathBuf,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("litebucket: {e}");
            ExitCode::from(1)
        }
    }
}

pub(crate) fn load_config(path: &std::path::Path, overrides: &Overrides) -> Result<Config> {
    Config::load(path, overrides)
}

fn execute(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Init {
            cfg,
            admin_key_output,
        } => {
            let cfg = load_config(&cfg.config, &Overrides::default())?;
            if let Some(out) = &admin_key_output
                && out.exists()
            {
                return Err(Error::config(format!("{} already exists", out.display())));
            }
            let report = crate::store::initialize(&cfg)?;
            println!(
                "initialized store {} (region {}) at {}",
                report.meta.store_id,
                report.meta.region,
                cfg.data_dir.display()
            );
            if report.master_key_created
                && let Some(p) = &cfg.secrets.master_key_file
            {
                println!(
                    "created master key {} (back it up separately; without it the stored access keys cannot be used)",
                    p.display()
                );
            }
            crate::admin::commands::emit_issued(
                &report.admin,
                admin_key_output.as_deref(),
                "admin access key created",
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve {
            cfg,
            listen,
            management_listen,
            log_level,
        } => {
            let overrides = Overrides {
                listen,
                management_listen,
                log_level,
            };
            let cfg = load_config(&cfg.config, &overrides)?;
            serve(cfg)
        }
        Command::Config {
            command: ConfigCommand::Check(a),
        } => {
            let cfg = load_config(&a.config, &Overrides::default())?;
            crate::store::secret_codec(&cfg)?;
            println!("configuration OK");
            println!(
                "{}",
                serde_json::to_string_pretty(&cfg.summary()).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Config {
            command: ConfigCommand::Template { docker },
        } => {
            print!(
                "{}",
                if docker {
                    crate::config::TEMPLATE_DOCKER
                } else {
                    crate::config::TEMPLATE_STANDALONE
                }
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Admin(args) => crate::admin::commands::run(args),
        Command::MasterKey {
            command: MasterKeyCommand::Generate { output },
        } => {
            crate::secrets::MasterKey::generate(&output)?;
            println!("wrote a new master key to {} (mode 0600)", output.display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Healthcheck { url } => healthcheck(&url),
        Command::Doctor(a) => {
            let cfg = load_config(&a.config, &Overrides::default())?;
            let ok = crate::doctor::doctor(&cfg, false)?;
            Ok(if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Command::Check { cfg, full } => {
            let cfg = load_config(&cfg.config, &Overrides::default())?;
            let ok = crate::doctor::doctor(&cfg, full)?;
            Ok(if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Command::Gc { cfg, apply, .. } => {
            let cfg = load_config(&cfg.config, &Overrides::default())?;
            crate::doctor::gc(&cfg, apply)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Backup { cfg, destination } => {
            let cfg = load_config(&cfg.config, &Overrides::default())?;
            crate::backup::backup(&cfg, &destination)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Restore {
            source,
            data_dir,
            master_key_file,
            skip_key_check,
        } => {
            crate::backup::restore(
                &source,
                &data_dir,
                master_key_file.as_deref(),
                skip_key_check,
            )?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Parse `123`, `10G`, `512M` (binary multiples); prints are exact bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| Error::config(format!("invalid size: {s}")))
}

/// Parse `90`, `90s`, `15m`, `24h`, `7d` into seconds.
pub fn parse_duration_secs(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1u64),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| {
            Error::config(format!(
                "invalid duration: {s} (examples: 90s, 15m, 24h, 7d)"
            ))
        })
}

fn healthcheck(url: &str) -> Result<ExitCode> {
    use std::io::{Read, Write};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Error::config("healthcheck supports http:// management URLs only"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/readyz"),
    };
    let mut stream = std::net::TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    let ok = buf.starts_with("HTTP/1.1 200");
    println!("{}", buf.split("\r\n\r\n").nth(1).unwrap_or("").trim());
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn serve(cfg: Config) -> Result<ExitCode> {
    crate::telemetry::init_logging(&cfg.logging);
    crate::metadata::check_sqlite_runtime()?;
    tracing::info!(
        event = "startup",
        version = env!("CARGO_PKG_VERSION"),
        sqlite_version = %crate::metadata::sqlite_version(),
        sqlite_source_id = %crate::metadata::sqlite_source_id(),
        storage_format = crate::metadata::migrations::FORMAT_VERSION,
        config = %cfg.summary(),
        "starting litebucket"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("litebucket")
        .build()?;
    runtime.block_on(async move {
        let store = Store::open(cfg)?;
        let running = crate::server::start(store.clone()).await?;
        wait_for_signals().await?;
        let drained = running.shutdown().await;
        Ok(if drained {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(3)
        })
    })
}

async fn wait_for_signals() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    loop {
        tokio::select! {
            // Access keys are managed through the admin API and apply
            // immediately; SIGHUP is accepted and ignored.
            _ = hup.recv() => tracing::info!(event = "sighup_ignored", "SIGHUP ignored: access keys are managed with `litebucket admin`"),
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    tracing::info!(event = "shutdown_requested", "stopping");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10G").unwrap(), 10_737_418_240);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert!(parse_size("x").is_err());
        assert!(parse_size("99999999999T").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("24h").unwrap(), 86_400);
        assert_eq!(parse_duration_secs("90").unwrap(), 90);
        assert_eq!(parse_duration_secs("15m").unwrap(), 900);
        assert!(parse_duration_secs("soon").is_err());
    }
}
