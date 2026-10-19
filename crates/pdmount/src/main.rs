//! `pdmount` command-line interface (DESIGN.md §14). M0 provides `login`,
//! `logout`, `status` and `doctor`; the mount commands follow in M1.

mod credentials;
mod doctor;
mod session;
mod status;

use clap::{Args, Parser, Subcommand};
use pdmount_core::config::BrowserMode;
use pdmount_core::{Config, Profile, ProfilePaths};
use std::process::ExitCode;

/// Mount Proton Drive "My Files" as a filesystem.
#[derive(Debug, Parser)]
#[command(name = "pdmount", version, about)]
struct Cli {
    /// Profile to use: one account and one mount.
    #[arg(long, global = true, default_value = Profile::DEFAULT)]
    profile: Profile,

    /// More log output (-v info, -vv debug); RUST_LOG overrides it.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Sign in with Proton in the browser and store the session.
    Login(BrowserArgs),
    /// End the session and remove stored secrets.
    Logout,
    /// Show session, account, storage and device.
    Status,
    /// Check FUSE, keyring, login, disk space and clock.
    Doctor,
}

#[derive(Debug, Args)]
struct BrowserArgs {
    /// Open the sign-in link in the default browser.
    #[arg(long, conflicts_with = "no_browser")]
    browser: bool,
    /// Only print the sign-in link.
    #[arg(long)]
    no_browser: bool,
}

impl BrowserArgs {
    /// Command-line choice, falling back to `auth.browser` in the config.
    fn mode(&self, config: &Config) -> BrowserMode {
        match (self.browser, self.no_browser) {
            (true, _) => BrowserMode::Always,
            (_, true) => BrowserMode::Never,
            _ => config.auth.browser,
        }
    }
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    // The SDK logs key material at debug level in this module; never show it.
    // pgp warns on every re-encoded key whose length differs from the
    // original; it is harmless noise, so only RUST_LOG shows it.
    let default = format!("{level},proton_sdk_rs2::secret=warn,pgp::packet::packet_sum=error");
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .map(|filter| filter.add_directive("proton_sdk_rs2::secret=warn".parse().expect("valid")))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn home_dir() -> anyhow::Result<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(Into::into)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set"))
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let paths = ProfilePaths::for_current_user(cli.profile)?;
    match cli.command {
        Command::Login(browser) => {
            let config = Config::load(&paths.config_file())?;
            let logged_in = session::login(&paths, browser.mode(&config)).await?;
            println!(
                "Logged in as {} (profile {}, session stored in {}).",
                logged_in.session.username, paths.profile, logged_in.storage
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Logout => {
            match session::logout(&paths).await? {
                session::LogoutOutcome::WasNotLoggedIn => {
                    println!("Profile {} was not logged in.", paths.profile);
                }
                session::LogoutOutcome::LoggedOut { revoked: true } => {
                    println!("Logged out; the session was ended on the server.");
                }
                session::LogoutOutcome::LoggedOut { revoked: false } => {
                    println!(
                        "Logged out locally; the session could not be ended on the server \
                         (it expires on its own)."
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => {
            let config = Config::load(&paths.config_file())?;
            let report = status::status(&paths, &config, &home_dir()?).await?;
            println!("{report}");
            Ok(if report.session.is_active() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Command::Doctor => {
            let checks = doctor::run(&paths).await;
            for check in &checks {
                println!("{check}");
            }
            let failed = checks.iter().any(|c| c.level == doctor::Level::Fail);
            Ok(if failed {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}

/// The SDK parses ML-DSA keys recursively; worker threads need a larger
/// stack than tokio's default (pdcli uses the same size).
const WORKER_STACK_SIZE: usize = 16 << 20;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Run on a worker thread so the large stack applies.
    let result = runtime.block_on(async { tokio::spawn(run(cli)).await });
    match result {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
        Err(join_error) => {
            eprintln!("error: {join_error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn profile_option_goes_before_or_after_the_subcommand() {
        let before =
            Cli::try_parse_from(["pdmount", "--profile", "work", "status"]).expect("parse");
        let after = Cli::try_parse_from(["pdmount", "status", "--profile", "work"]).expect("parse");
        assert_eq!(before.profile.as_str(), "work");
        assert_eq!(after.profile.as_str(), "work");
        assert!(Cli::try_parse_from(["pdmount", "--profile", "../x", "status"]).is_err());
        assert_eq!(
            Cli::try_parse_from(["pdmount", "doctor"])
                .expect("parse")
                .profile,
            Profile::default()
        );
    }

    #[test]
    fn login_browser_flags_override_the_config() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args).expect("parse").command {
            Command::Login(browser) => browser,
            other => panic!("unexpected {other:?}"),
        };
        let config = Config::default();
        assert_eq!(
            parse(&["pdmount", "login"]).mode(&config),
            BrowserMode::Auto
        );
        assert_eq!(
            parse(&["pdmount", "login", "--browser"]).mode(&config),
            BrowserMode::Always
        );
        assert_eq!(
            parse(&["pdmount", "login", "--no-browser"]).mode(&config),
            BrowserMode::Never
        );
        assert!(Cli::try_parse_from(["pdmount", "login", "--browser", "--no-browser"]).is_err());
        let mut never = Config::default();
        never.auth.browser = BrowserMode::Never;
        assert_eq!(
            parse(&["pdmount", "login"]).mode(&never),
            BrowserMode::Never
        );
    }
}
