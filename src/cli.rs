use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::{fs, io};

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use crate::utils::Strategy;

#[derive(Parser, Debug, Deserialize, Serialize)]
#[command(version)]
#[command(long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) mode: Mode,
}

impl Cli {
    const fn new_from_mode(mode: Mode) -> Self {
        Self { mode }
    }

    /// Reads a saved set of command line arguments from the provided file.
    pub(crate) fn read_from_config(config_path: &str) -> Result<Self, io::Error> {
        let final_path = if Path::new(&config_path).is_relative() {
            let full_path = std::env::current_dir().expect("Current dir exists").join(config_path);
            &full_path.into_boxed_path()
        } else {
            Path::new(config_path)
        };
        let mut config_file = fs::File::open(final_path)?;
        let mut config_file_content = String::new();
        let _ = config_file.read_to_string(&mut config_file_content);

        let cli: Mode = serde_json::from_str(&config_file_content)?;
        Ok(Self::new_from_mode(cli))
    }

    /// Saves the current command line arguments to a configuration file so it
    /// can be reused.
    pub(crate) fn save_to_config(&self, config_path: &str) -> Result<(), io::Error> {
        let mut config_file = fs::File::create(config_path)?;
        let config = match &self.mode {
            Mode::Input(args) => json!({ "input": args }),
            Mode::Overview(args) => json!({ "overview": args }),
            Mode::File(args) => json!({ "file": args }),
            Mode::Multi(args) => json!({ "multi": args }),
            Mode::SelfUpdate | Mode::Load(_) => {
                info!("Nothing to save.");
                return Ok(());
            }
        };

        let config_json = serde_json::to_string_pretty(&config).expect("Failed to serialize config");
        config_file.write_all(config_json.as_bytes()).expect("Failed to write to config file");
        info!("Written config to: `{config_path}`");
        Ok(())
    }
}

#[derive(Subcommand, Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Input mode: Enter a docker image string via stdin and receive the
    /// updated version for a given strategy.
    #[command(alias = "i")]
    Input(InputArguments),

    /// Overview mode: Enter a docker image string via stdin and receive the
    /// all possible upgrades for each available strategy
    #[command(alias = "o")]
    Overview(OverviewArguments),

    /// File mode: Choose a dockerfile and update all images based on a given
    /// strategy.
    #[command(alias = "s")]
    File(SingleFileArguments),

    /// Multi file mode: Enter a folder path, the program will find all
    /// dockerfiles. Specific files can be excluded.
    #[command(alias = "m")]
    Multi(MultiFileArguments),

    /// Loading a saved config, so subsequent calls can reused the config
    /// without needing to keep all arguments in mind.
    Load(LoadArguments),

    /// Will download the latest binary and place it next to the current one.
    SelfUpdate,
}

#[derive(Args, Debug, Clone, Deserialize, Serialize)]
pub struct LoadArguments {
    #[arg(
        index = 1,
        help = "Loads config from file. Will overwrite all other arguments and settings",
        conflicts_with = "save_config"
    )]
    pub(crate) config: Option<String>,
}

#[derive(Args, Debug, Clone, Deserialize, Serialize)]
pub struct SingleFileArguments {
    // Using positional argument instead of named argument
    #[arg(value_name = "FILE", help = "Path to the file.")]
    pub(crate) file: PathBuf,

    #[arg(long, help = "Which strategy should be used.", default_value = Strategy::Latest)]
    pub(crate) strat: Strategy,

    #[arg(long, short = 'n', help = "If set will output the new file contents for inspection.")]
    pub(crate) dry_run: bool,

    #[command(flatten)]
    pub(crate) common: CommonOptions,
}

#[derive(Args, Debug, Clone, Deserialize, Serialize)]
pub struct InputArguments {
    // Using positional argument instead of named argument
    #[arg(value_name = "IMAGE", help = "The full docker image including the tag, that shall be updated.")]
    pub(crate) input: String,

    #[arg(long, help = "Which strategy should be used.", default_value = Strategy::Latest)]
    pub(crate) strat: Strategy,

    #[command(flatten)]
    pub(crate) common: CommonOptions,
}

#[derive(Args, Debug, Clone, Deserialize, Serialize)]
pub struct OverviewArguments {
    // Using positional argument instead of named argument
    #[arg(value_name = "IMAGE", help = "The full docker image including the tag, that shall be updated.")]
    pub(crate) input: String,

    #[command(flatten)]
    pub(crate) common: CommonOptions,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Args, Debug, Clone, Deserialize, Serialize)]
pub struct CommonOptions {
    #[arg(long, short, help = "Will filter out tags only for the given architecture.")]
    pub(crate) arch: Option<String>,

    #[arg(long, help = "Limit the amount of tags to be searched on Docker Hub.")]
    pub(crate) tag_search_limit: Option<u16>,

    #[arg(long, short, help = "Activates debug logging.")]
    pub(crate) debug: bool,

    #[arg(long, short, help = "Activates color output.", default_value_t = false)]
    pub(crate) color: bool,

    #[arg(long, short, help = "Saves config from arguments to disk.", conflicts_with = "load_config")]
    pub(crate) save_config: Option<String>,

    #[arg(
        long,
        short,
        help = "Will print out only the result or an empty string if no match was found when used in input mode."
    )]
    pub(crate) quiet: bool,
}

#[derive(Args, Debug, Clone, Deserialize, Serialize)]
pub struct MultiFileArguments {
    // Using positional argument instead of named argument
    #[arg(value_name = "FOLDER", help = "Path to the folder.")]
    pub(crate) folder: PathBuf,

    #[arg(long, help = "Which strategy should be used.", default_value = Strategy::Latest)]
    pub(crate) strat: Strategy,

    #[arg(long, short = 'n', help = "If set will output the new file contents for inspection.")]
    pub(crate) dry_run: bool,

    /// Allows the user to exclude certain files in the folder and its
    /// subfolders.
    #[arg(long, short, help = "The list of files to exclude", required = false, num_args = 0..)]
    pub(crate) exclude_file: Vec<String>,

    /// Allows to ignore certain versions to not be updated, in case of needed
    /// legacy compatibility. This ignore applies globally for all found
    /// files that will be processed.
    #[arg(long, short, help = "The list of versions to ignore (they will not be updated), e.g.: alpine:3.12", required = false, num_args = 0..)]
    pub(crate) ignore_versions: Vec<String>,

    #[command(flatten)]
    pub(crate) common: CommonOptions,
}

mod tests {

    #[test]
    fn config_parsing() {
        let config = crate::cli::Cli::read_from_config("./tests/fixtures/config_example.json");
        assert!(config.is_ok());
        assert!(config.unwrap().save_to_config("./tests/fixtures/config_example_new.json").is_ok());
        let content1 = std::fs::read_to_string("./tests/fixtures/config_example.json").unwrap();
        let content2 = std::fs::read_to_string("./tests/fixtures/config_example_new.json").unwrap();
        assert_eq!(content1, content2);
    }
}
