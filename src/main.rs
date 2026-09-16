use std::env;
use std::sync::OnceLock;

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use crate::cli::Cli;
use crate::utils::{check_update, handle_file, handle_input, handle_multi, handle_overview, handle_self_update};

mod cli;
mod container_image;
mod registries;
mod tag;
pub(crate) mod utils;

pub(crate) static GITHUB_REPO_BASE_URL: OnceLock<String> = OnceLock::new();
pub(crate) static GITHUB_REPO_RELEASE_REFS_URL: OnceLock<String> = OnceLock::new();
pub(crate) static GITHUB_REPO_RELEASE_URL: OnceLock<String> = OnceLock::new();

fn main() {
    init();
    let mut cli = cli::Cli::parse();

    if matches!(&cli.mode, cli::Mode::SelfUpdate) {
        check_update();
    }

    let load_config = match &cli.mode {
        cli::Mode::Input(_) | cli::Mode::Overview(_) | cli::Mode::File(_) | cli::Mode::Multi(_) | cli::Mode::SelfUpdate => None,
        cli::Mode::Load(load_arguments) => load_arguments.config.as_ref(),
    };

    if let Some(config_path) = load_config {
        let config = Cli::read_from_config(config_path).expect("Could read config");
        info!("Loading configuration from file: {config_path}");
        cli = config;
    }

    let save_config = match &cli.mode {
        cli::Mode::Input(input_arguments) => &input_arguments.common.save_config,
        cli::Mode::Overview(overview_arguments) => &overview_arguments.common.save_config,
        cli::Mode::File(single_file_arguments) => &single_file_arguments.common.save_config,
        cli::Mode::Multi(multi_file_arguments) => &multi_file_arguments.common.save_config,
        cli::Mode::SelfUpdate | cli::Mode::Load(_) => &None,
    };

    if let Some(config_path) = save_config {
        info!("Saving configuration from command line options to file: {config_path}. This will override any existing configuration.");
        let _ = cli.save_to_config(config_path);
    }

    init_logging(&cli);

    match cli.mode {
        cli::Mode::Input(input_mode) => {
            handle_input(&input_mode);
        }
        cli::Mode::Overview(overview_mode) => {
            handle_overview(&overview_mode);
        }
        cli::Mode::File(file_mode) => {
            handle_file(&file_mode);
        }
        cli::Mode::Multi(multi_mode) => {
            handle_multi(&multi_mode);
        }
        cli::Mode::SelfUpdate => {
            handle_self_update();
        }
        cli::Mode::Load(_) => {
            error!("Should already handled by other functions. Loaded config was processed already.");
        }
    }
}

fn init_logging(cli: &cli::Cli) {
    let debug = match &cli.mode {
        cli::Mode::File(file_mode) => file_mode.common.debug,
        cli::Mode::Input(input_mode) => input_mode.common.debug,
        cli::Mode::Multi(multi_file_mode) => multi_file_mode.common.debug,
        cli::Mode::Overview(overview_mode) => overview_mode.common.debug,
        cli::Mode::SelfUpdate | cli::Mode::Load(_) => false,
    };

    let color = match &cli.mode {
        cli::Mode::File(file_mode) => file_mode.common.color,
        cli::Mode::Input(input_mode) => input_mode.common.color,
        cli::Mode::Multi(multi_file_mode) => multi_file_mode.common.color,
        cli::Mode::Overview(overview_mode) => overview_mode.common.color,
        cli::Mode::SelfUpdate | cli::Mode::Load(_) => false,
    };

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(if debug { "debug" } else { "info" }));
    let custom_format = fmt::format()
        .with_target(false)
        .with_file(true)
        .with_level(true)
        .with_line_number(true)
        .with_ansi(color)
        .compact();
    let fmt_layer = fmt::layer().event_format(custom_format);

    // If quiet flag is set, we do not initialise and use the
    // tracing_subscriber. Only (e)print(ln) will be printed.
    if let cli::Mode::Input(input_mode) = &cli.mode {
        if !input_mode.common.quiet {
            tracing_subscriber::registry().with(env_filter).with(fmt_layer).init();
        }
    } else {
        tracing_subscriber::registry().with(env_filter).with(fmt_layer).init();
    }
}

fn init() {
    // Needs to be initialised so that ureq can use rustls and not be
    // dependendant on openssl. This makes building for musl a lot easier.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // URLs that are being used for the update mechanism
    let github_author = env!("CARGO_PKG_AUTHORS").to_ascii_lowercase();
    let github_repo = env!("CARGO_PKG_NAME");
    GITHUB_REPO_BASE_URL.get_or_init(|| format!("https://github.com/{github_author}/{github_repo}"));
    GITHUB_REPO_RELEASE_REFS_URL.get_or_init(|| format!("{}/refs?type=tag", GITHUB_REPO_BASE_URL.get().expect("We did not forget to intialise.")));
    GITHUB_REPO_RELEASE_URL.get_or_init(|| format!("{}/releases", GITHUB_REPO_BASE_URL.get().expect("We did not forget to intialise.")));
}
