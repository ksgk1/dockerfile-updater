use std::fmt::Display;
use std::fs::File;
use std::io::{Read as _, copy};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs};

use clap::builder::OsStr;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};
use ureq::Agent;
use walkdir::WalkDir;

use crate::container_image::{ContainerImage, Dockerfile};
use crate::registries::{DURATION_HOUR_AS_SECS, TAGS_CACHE};
use crate::tag::Tag;
use crate::{GITHUB_REPO_RELEASE_REFS_URL, GITHUB_REPO_RELEASE_URL, cli, utils};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Default, PartialEq, Eq, clap::ValueEnum, Deserialize, Serialize)]
#[clap(rename_all = "kebab-case")]
pub enum Strategy {
    #[default]
    Latest,
    NextPatch,
    LatestPatch,
    NextMinor,
    LatestMinor,
    NextMajor,
    LatestMajor,
}

// This needs to be OsStr since it is used by clap.
impl From<Strategy> for OsStr {
    fn from(value: Strategy) -> Self {
        match value {
            Strategy::Latest => Self::from("latest"),
            Strategy::NextPatch => Self::from("next-patch"),
            Strategy::LatestPatch => Self::from("latest-patch"),
            Strategy::NextMinor => Self::from("next-minor"),
            Strategy::LatestMinor => Self::from("latest-minor"),
            Strategy::NextMajor => Self::from("next-major"),
            Strategy::LatestMajor => Self::from("latest-major"),
        }
    }
}

impl Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NextPatch => write!(f, "next patch"),
            Self::LatestPatch => write!(f, "latest patch"),
            Self::NextMinor => write!(f, "next minor"),
            Self::LatestMinor => write!(f, "latest minor"),
            Self::NextMajor => write!(f, "next major"),
            Self::LatestMajor => write!(f, "latest major"),
            Self::Latest => write!(f, "latest"),
        }
    }
}

type StageIndex = usize;
type ImageUpdate = (StageIndex, Tag);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerfileUpdate {
    pub dockerfile: Dockerfile,
    pub updates:    Vec<ImageUpdate>,
}

impl DockerfileUpdate {
    pub(crate) fn apply(&self) -> Dockerfile {
        let mut result = self.dockerfile.clone();
        for (stage_index, image) in &mut result.get_base_images_mut().iter_mut().enumerate() {
            for (update_index, updated_tag) in &self.updates {
                if *update_index == stage_index {
                    image.update_image_tag(updated_tag);
                }
            }
        }
        result
    }
}

/// Handles data from standard input
pub fn handle_input(input_mode: &cli::InputArguments) {
    let docker_image: ContainerImage = input_mode.input.parse().expect("Image could be parsed.");
    let mut docker_image_tags = docker_image
        .get_remote_tags(input_mode.common.tag_search_limit, input_mode.common.arch.as_ref(), None)
        .expect("Getting tags finishes sucessful.");
    docker_image_tags.sort();
    if let Some(found_tag) = docker_image.get_tag().find_candidate_tag(&docker_image_tags, &input_mode.strat) {
        info!(
            "===> Candidate tag: {}:{found_tag} (from: {})",
            docker_image.get_full_name(),
            docker_image.get_full_tagged_name(),
        );
        if input_mode.common.quiet {
            println!("{}:{}", docker_image.get_dockerimage_name(), found_tag.to_string().trim_end_matches('.'));
        }
    } else {
        info!("===> No candidate found.");
        if input_mode.common.quiet {
            println!();
        }
    }
}

/// Handles data from standard input
pub fn handle_overview(overview_mode: &cli::OverviewArguments) {
    let docker_image: ContainerImage = overview_mode.input.parse().expect("Image could be parsed.");
    let mut docker_image_tags = docker_image
        .get_remote_tags(overview_mode.common.tag_search_limit, overview_mode.common.arch.as_ref(), None)
        .expect("Getting tags finishes sucessful.");
    docker_image_tags.sort();

    if overview_mode.common.quiet {
        println!("Results for:\t{}", docker_image.get_full_tagged_name());
    } else {
        info!("Results for:\t{}", docker_image.get_full_tagged_name());
    }
    // create one found tag for every strategy
    for strategy in [
        Strategy::NextPatch,
        Strategy::LatestPatch,
        Strategy::NextMinor,
        Strategy::LatestMinor,
        Strategy::NextMajor,
        Strategy::LatestMajor,
    ] {
        if let Some(found_tag) = docker_image.get_tag().find_candidate_tag(&docker_image_tags, &strategy) {
            if overview_mode.common.quiet {
                println!(
                    "{strategy}:\t{}:{}",
                    docker_image.get_dockerimage_name(),
                    found_tag.to_string().trim_end_matches('.')
                );
            } else {
                info!("===> {strategy}:\t{}:{found_tag}", docker_image.get_dockerimage_name(),);
            }
        } else if !overview_mode.common.quiet {
            info!("===> No candidate found for {strategy}.");
        }
    }
}

pub fn handle_file(file_mode: &cli::SingleFileArguments) {
    let file = file_mode.file.to_string_lossy().into_owned();
    let path = Path::new(&file);
    if path.exists() {
        info!("Processing dockerfile: {}", path.canonicalize().expect("Path can be canonicalised.").display());
        let mut dockerfile = Dockerfile::read(&file_mode.file).expect("File is readable and a valid dockerfile");
        dockerfile.update_images(
            !file_mode.dry_run,
            &file_mode.strat,
            file_mode.common.tag_search_limit,
            file_mode.common.arch.as_ref(),
        );
    } else {
        error!("File `{file}` not found.");
    }
}

/// Handling function that will handle multiple files at once, with a given
/// ignore for single files or specific images.
pub fn handle_multi(multi_mode: &cli::MultiFileArguments) {
    let folder = multi_mode.folder.to_str().unwrap_or_default().to_owned();
    let path = Path::new(&folder);
    info!("Processing folder: {}", path.canonicalize().expect("Path can be canonicalised.").display());
    let mut dockerfiles_to_process = Vec::<String>::new();
    for entry in WalkDir::new(path).into_iter().filter_map(std::result::Result::ok) {
        if entry.file_name().to_string_lossy().to_ascii_lowercase().starts_with("dockerfile") {
            dockerfiles_to_process.push(entry.path().display().to_string());
        }
    }
    if !multi_mode.exclude_file.is_empty() {
        info!("Ignoring files: {:?}", &multi_mode.exclude_file);
        for excluded in &multi_mode.exclude_file {
            dockerfiles_to_process.retain(|f| !f.ends_with(excluded));
        }
    }
    info!("Found files: {dockerfiles_to_process:?}");
    for dockerfile_to_process in &dockerfiles_to_process {
        match Dockerfile::read(&PathBuf::from(dockerfile_to_process)) {
            Ok(dockerfile) => {
                let ignored_images: Vec<ContainerImage> = multi_mode
                    .ignore_versions
                    .iter()
                    .map(|image| image.parse().expect("Image could be parsed."))
                    .collect();
                if !ignored_images.is_empty() {
                    debug!("Skipping image updates:");
                    for image in &ignored_images {
                        debug!("\t\t{}", image.get_name());
                    }
                }
                let possible_updates = dockerfile.generate_image_updates(
                    &multi_mode.strat,
                    multi_mode.common.tag_search_limit,
                    multi_mode.common.arch.as_ref(),
                    &ignored_images,
                );
                let dockerfile_updated = possible_updates.apply();
                if multi_mode.dry_run {
                    info!(
                        "Updated dockerfile `{}` would look like:\n{dockerfile_updated}",
                        dockerfile.get_path().expect("Path is not empty.").display()
                    );
                } else {
                    let _ = dockerfile_updated.write();
                }
            }
            Err(e) => {
                error!("Could not read dockerfile: `{dockerfile_to_process}` with error: {e}");
            }
        }
    }
}

/// Reads already fetched data into the program's memory (global variable).
///
/// Cache invalidates after `DURATION_HOUR_AS_SECS` seconds, to ensure the data
/// is up to date.
pub fn extract_cache_from_file(full_name: &str, tags: &mut Vec<Tag>, cache_file_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    utils::create_cache_dir();

    // Only opening the file once, will prevent against TOCTOU (Time of check,
    // time of use) vulnerabilities. Might not be critical here, but its a
    // good habit.
    let mut file = match File::open(cache_file_name) {
        Ok(file) => file,
        Err(e) => {
            info!("No cache file exists under `{cache_file_name}`. Error: {e}. Fetching info from docker hub.");
            return Ok(());
        }
    };

    let metadata = file.metadata()?;
    if let Ok(time) = metadata.modified() {
        if time.elapsed()? < Duration::new(DURATION_HOUR_AS_SECS, 0) {
            let mut cache_file_content = String::new();
            file.read_to_string(&mut cache_file_content)?;
            if let Ok(read_tags) = serde_json::from_str(&cache_file_content) {
                tags.clone_from(&read_tags);
                let mut cache = TAGS_CACHE.write()?;
                if cache.insert(full_name.to_string(), tags.clone()).is_none() {
                    debug!("Populated cache successfully.");
                }
            } else {
                error!("Could not read tags from file");
            }
        } else {
            info!("Cache file is older than {DURATION_HOUR_AS_SECS} seconds. Fetching new data instead.");
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TagRefListResponse {
    refs:       Vec<String>,
    _cache_key: String,
}

/// Returns the latest available version if there is one published on Github.
fn fetch_latest_version(agent: &Agent) -> Option<Tag> {
    let mut response = match agent
        .get(GITHUB_REPO_RELEASE_REFS_URL.get().expect("We did not forget to initialise"))
        .header("Accept", "application/json")
        .call()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to check for updates: {e}");
            return None;
        }
    };

    let current_tag: Tag = VERSION.parse().expect("Valid semver version");
    let response_body = response.body_mut().read_to_string().expect("Well-formed response");
    let parsed_response: TagRefListResponse = match serde_json::from_str(&response_body) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to parse update response from GitHub. Error: {e}");
            return None;
        }
    };

    let latest = parsed_response
        .refs
        .iter()
        .filter_map(|tag| tag.strip_prefix("v").unwrap_or(tag).parse().ok())
        .max()?;

    if latest > current_tag {
        Some(latest)
    } else {
        println!("Already up to date.");
        None
    }
}

pub fn check_update() {
    let agent = Agent::new_with_defaults();
    if let Some(latest) = fetch_latest_version(&agent) {
        println!(
            "A newer version is available: v{latest}\nPlease check: {}",
            GITHUB_REPO_RELEASE_URL.get().expect("We did not forget to initialise.")
        );
    }
}

/// Handles file downloads
fn download_file(agent: &Agent, url: &str, output_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut response = agent.get(url).call()?;
    let mut file = File::create(output_path)?;
    copy(&mut response.body_mut().as_reader(), &mut file)?;
    Ok(())
}

/// Create dir that stores cache files
pub fn create_cache_dir() {
    let mut cache_dir_path = std::env::temp_dir();
    cache_dir_path.push("dfu");
    if let Err(e) = fs::create_dir_all(&cache_dir_path) {
        eprintln!("Could not create temp dir: {}. Error: {e}", cache_dir_path.display());
    }
}

/// Handling the self update, to download a new version from Github, if one is
/// available.
pub fn handle_self_update() {
    let agent = Agent::new_with_defaults();
    let Some(latest) = fetch_latest_version(&agent) else { return };

    let extension = match std::env::consts::OS {
        "windows" => ".exe",
        _ => "-x86_64-unknown-linux-musl",
    };

    let file_name = format!("dfu-v{latest}{extension}");
    let download_url = format!(
        "{}/download/v{latest}/{file_name}",
        GITHUB_REPO_RELEASE_URL.get().expect("We did not forget to initialise.")
    );
    debug!("Download URL: {download_url}");
    let mut full_path = env::current_dir().expect("Valid current dir");
    full_path.push(&file_name);

    match download_file(&agent, &download_url, full_path.to_str().expect("Valid path")) {
        Ok(()) => info!("Successfully downloaded new version to: {}", full_path.display()),
        Err(e) => error!("Error while downloading new release: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::{fs, io};

    use clap::ValueEnum;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    use crate::cli::{CommonOptions, InputArguments, MultiFileArguments, OverviewArguments, SingleFileArguments};
    use crate::utils::{Strategy, handle_file, handle_input, handle_multi, handle_overview};

    fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
        fs::create_dir_all(&dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
            } else {
                fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    #[test]
    fn strat_parsing() {
        let fixtures = vec![
            ("latest", Strategy::Latest, "latest"),
            ("next-patch", Strategy::NextPatch, "next patch"),
            ("latest-patch", Strategy::LatestPatch, "latest patch"),
            ("next-minor", Strategy::NextMinor, "next minor"),
            ("latest-minor", Strategy::LatestMinor, "latest minor"),
            ("next-major", Strategy::NextMajor, "next major"),
            ("latest-major", Strategy::LatestMajor, "latest major"),
        ];
        for fixture in fixtures {
            assert_eq!(Strategy::from_str(fixture.0, true).unwrap(), fixture.1);
            assert_eq!(fixture.1.to_string(), fixture.2);
        }
    }

    #[test]
    fn input_single_multi() {
        let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let custom_format = fmt::format()
            .with_target(false)
            .with_file(true)
            .with_level(true)
            .with_line_number(true)
            .compact();
        let fmt_layer = fmt::layer().event_format(custom_format);
        tracing_subscriber::registry().with(env_filter).with(fmt_layer).init();

        let mut o = OverviewArguments {
            input:  "node:8.0".to_owned(),
            common: CommonOptions {
                arch:             None,
                tag_search_limit: Some(1000),
                debug:            false,
                quiet:            false,
                color:            false,
                save_config:      None,
            },
        };
        handle_overview(&o);
        o.common.quiet = true;
        handle_overview(&o);

        let mut i = InputArguments {
            input:  "clamav/clamav:1.5.1-11_base".into(),
            strat:  Strategy::Latest,
            common: CommonOptions {
                arch:             None,
                tag_search_limit: Some(1000),
                debug:            false,
                quiet:            false,
                color:            false,
                save_config:      None,
            },
        };
        handle_input(&i);
        i.common.quiet = true;
        handle_input(&i);
        i.input = "clamav/clamav:1.5.1-99_base".into();
        handle_input(&i);

        let mut f = SingleFileArguments {
            file:    "./tests/fixtures/DockerfileExample1".to_owned().into(),
            strat:   Strategy::Latest,
            dry_run: true,
            common:  CommonOptions {
                arch:             None,
                tag_search_limit: Some(1000),
                debug:            false,
                quiet:            false,
                color:            false,
                save_config:      None,
            },
        };

        let mut m = MultiFileArguments {
            folder:          "./tests/fixtures".into(),
            strat:           Strategy::Latest,
            dry_run:         true,
            exclude_file:    vec!["./tests/fixtures/DockerfileExample1".to_owned()],
            ignore_versions: vec!["node:8.0-alpine".to_owned()],
            common:          CommonOptions {
                arch:             None,
                tag_search_limit: Some(1000),
                debug:            false,
                quiet:            false,
                color:            false,
                save_config:      None,
            },
        };

        handle_multi(&m);
        handle_file(&f);

        // copy fixtures folder
        assert!(copy_dir_all("./tests/fixtures", "./tests/fixtures.backup").is_ok());
        m.dry_run = false;
        f.dry_run = false;
        handle_multi(&m);
        handle_file(&f);

        m.common.arch = Some("amd64".to_owned());
        handle_multi(&m);
        handle_file(&f);
        f.common.arch = Some("amd64".to_owned());
        // restore fixtures folder
        let _ = fs::remove_dir_all("./tests/fixtures");
        let _ = fs::rename("./tests/fixtures.backup", "./tests/fixtures").is_ok();
    }
}
