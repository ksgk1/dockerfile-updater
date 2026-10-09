use std::fmt::{Display, Write as _};
use std::fs::File;
use std::io::{Read as _, copy};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use std::{env, fs};

use clap::builder::OsStr;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};
use ureq::Agent;
use walkdir::WalkDir;

use crate::container_image::{ContainerImage, Dockerfile, Error};
use crate::registries::{DURATION_HOUR_AS_SECS, TAGS_CACHE};
use crate::tag::Tag;
use crate::{cli, utils};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared HTTP agent, so connection pooling is used across all registry and
/// GitHub requests of a single run.
pub static HTTP_AGENT: LazyLock<Agent> = LazyLock::new(|| Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).build().into());

/// The base URL of the GitHub repository this binary was released from.
fn github_base_url() -> String {
    format!(
        "https://github.com/{}/{}",
        env!("CARGO_PKG_AUTHORS").to_ascii_lowercase(),
        env!("CARGO_PKG_NAME")
    )
}

/// The URL of the GitHub releases page of this binary.
fn github_release_url() -> String {
    format!("{}/releases", github_base_url())
}

/// The URL of the tag refs of this repository, as returned by the GitHub API.
fn github_release_refs_url() -> String {
    format!("{}/refs?type=tag", github_base_url())
}

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
///
/// # Errors
///
/// This function will return an error if the given image cannot be parsed or
/// its tags cannot be fetched.
pub fn handle_input(input_mode: &cli::InputArguments) -> Result<(), Error> {
    let docker_image: ContainerImage = input_mode.input.parse()?;
    let mut docker_image_tags = docker_image.get_remote_tags(input_mode.common.tag_search_limit, input_mode.common.arch.as_ref(), None)?;
    docker_image_tags.sort();
    if let Some(found_tag) = docker_image.get_tag().find_candidate_tag(&docker_image_tags, &input_mode.strat) {
        info!(
            "===> Candidate tag: {}:{found_tag} (from: {})",
            docker_image.registry_path(),
            docker_image.qualified_tagged_name(),
        );
        if input_mode.common.quiet {
            println!("{}:{}", docker_image.pullable_name(), found_tag.to_string().trim_end_matches('.'));
        }
    } else {
        info!("===> No candidate found.");
        if input_mode.common.quiet {
            println!();
        }
    }
    Ok(())
}

/// Handles data from standard input
///
/// # Errors
///
/// This function will return an error if the given image cannot be parsed or
/// its tags cannot be fetched.
pub fn handle_overview(overview_mode: &cli::OverviewArguments) -> Result<(), Error> {
    let docker_image: ContainerImage = overview_mode.input.parse()?;
    let mut docker_image_tags = docker_image.get_remote_tags(overview_mode.common.tag_search_limit, overview_mode.common.arch.as_ref(), None)?;
    docker_image_tags.sort();

    if overview_mode.common.quiet {
        println!("Results for:\t{}", docker_image.qualified_tagged_name());
    } else {
        info!("Results for:\t{}", docker_image.qualified_tagged_name());
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
                println!("{strategy}:\t{}:{}", docker_image.pullable_name(), found_tag.to_string().trim_end_matches('.'));
            } else {
                info!("===> {strategy}:\t{}:{found_tag}", docker_image.pullable_name());
            }
        } else if !overview_mode.common.quiet {
            info!("===> No candidate found for {strategy}.");
        }
    }
    Ok(())
}

/// Handles a single dockerfile.
///
/// # Errors
///
/// This function will return an error if the file does not exist, cannot be
/// read or the updated dockerfile cannot be written.
pub fn handle_file(file_mode: &cli::SingleFileArguments) -> Result<(), Error> {
    let file = file_mode.file.to_string_lossy().into_owned();
    let path = Path::new(&file);
    if path.exists() {
        info!(
            "Processing dockerfile: {}",
            path.canonicalize().map_or_else(|_| file.clone(), |p| p.display().to_string())
        );
        let mut dockerfile = Dockerfile::read(&file_mode.file)?;
        dockerfile.update_images(
            !file_mode.dry_run,
            &file_mode.strat,
            file_mode.common.tag_search_limit,
            file_mode.common.arch.as_ref(),
        )
    } else {
        error!("File `{file}` not found.");
        Err(Error::Io {
            path:   file,
            reason: String::from("the file does not exist"),
        })
    }
}

/// Handling function that will handle multiple files at once, with a given
/// ignore for single files or specific images. Errors while processing one
/// dockerfile (reading, fetching tags, writing) are logged and the remaining
/// files are still processed.
pub fn handle_multi(multi_mode: &cli::MultiFileArguments) {
    let folder = multi_mode.folder.to_string_lossy().into_owned();
    let path = Path::new(&folder);
    info!(
        "Processing folder: {}",
        path.canonicalize().map_or_else(|_| folder.clone(), |p| p.display().to_string())
    );
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
    let ignored_images: Vec<ContainerImage> = multi_mode
        .ignore_versions
        .iter()
        .filter_map(|image| match image.parse::<ContainerImage>() {
            Ok(image) => Some(image),
            Err(e) => {
                error!("Could not parse ignored image `{image}`: {e}. It will not be ignored.");
                None
            }
        })
        .collect();
    if !ignored_images.is_empty() {
        debug!("Skipping image updates:");
        for image in &ignored_images {
            debug!("\t\t{}", image.get_name());
        }
    }
    for dockerfile_to_process in &dockerfiles_to_process {
        match Dockerfile::read(&PathBuf::from(dockerfile_to_process)) {
            Ok(dockerfile) => {
                let possible_updates = dockerfile.generate_image_updates(
                    &multi_mode.strat,
                    multi_mode.common.tag_search_limit,
                    multi_mode.common.arch.as_ref(),
                    &ignored_images,
                );
                let dockerfile_updated = possible_updates.apply();
                if multi_mode.dry_run {
                    info!("Updated dockerfile `{dockerfile_to_process}` would look like:\n{dockerfile_updated}");
                } else if let Err(e) = dockerfile_updated.write() {
                    error!("Could not write updated dockerfile `{dockerfile_to_process}`: {e}");
                }
            }
            Err(e) => {
                error!("Could not read dockerfile: `{dockerfile_to_process}` with error: {e}");
            }
        }
    }
}

/// Reads already fetched tags into the program's memory (global variable).
///
/// Cache invalidates after `DURATION_HOUR_AS_SECS` seconds, to ensure the data
/// is up to date.
///
/// # Errors
///
/// This function will return an error if the cache file cannot be read.
pub fn extract_cache_from_file(cache_key: &str, tags: &mut Vec<Tag>, cache_file_name: &str) -> Result<(), Error> {
    utils::create_cache_dir();

    // Only opening the file once, will prevent against TOCTOU (Time of check,
    // time of use) vulnerabilities. Might not be critical here, but its a
    // good habit.
    let mut file = match File::open(cache_file_name) {
        Ok(file) => file,
        Err(e) => {
            info!("No cache file exists under `{cache_file_name}`. Error: {e}. Fetching info from the registry.");
            return Ok(());
        }
    };

    let metadata = file.metadata().map_err(|e| Error::Io {
        path:   cache_file_name.to_owned(),
        reason: e.to_string(),
    })?;
    let Ok(time) = metadata.modified() else {
        return Ok(());
    };
    match time.elapsed() {
        Ok(elapsed) if elapsed < Duration::from_secs(DURATION_HOUR_AS_SECS) => {
            let mut cache_file_content = String::new();
            file.read_to_string(&mut cache_file_content).map_err(|e| Error::Io {
                path:   cache_file_name.to_owned(),
                reason: e.to_string(),
            })?;
            if let Ok(read_tags) = serde_json::from_str::<Vec<Tag>>(&cache_file_content) {
                *tags = read_tags;
                {
                    let mut cache = TAGS_CACHE.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                    cache.insert(cache_key.to_owned(), tags.clone());
                }
                debug!("Populated cache successfully from `{cache_file_name}`.");
            } else {
                error!("Could not read tags from file `{cache_file_name}`. Fetching new data instead.");
            }
        }
        Ok(_) => {
            info!("Cache file is older than {} seconds. Fetching new data instead.", DURATION_HOUR_AS_SECS);
        }
        Err(_) => {
            info!("Could not determine the age of the cache file. Fetching new data instead.");
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

/// Returns the latest available version if there is one published on GitHub.
fn fetch_latest_version(agent: &Agent) -> Option<Tag> {
    let mut response = match agent.get(github_release_refs_url()).header("Accept", "application/json").call() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to check for updates: {e}");
            return None;
        }
    };

    let Ok(current_tag) = VERSION.parse::<Tag>() else {
        eprintln!("Could not parse own version `{VERSION}`; skipping the update check.");
        return None;
    };
    let response_body = match response.body_mut().read_to_string() {
        Ok(body) => body,
        Err(e) => {
            eprintln!("Failed to read the update response from GitHub: {e}");
            return None;
        }
    };
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
    let agent = &*HTTP_AGENT;
    if let Some(latest) = fetch_latest_version(agent) {
        println!("A newer version is available: v{latest}\nPlease check: {}", github_release_url());
    }
}

/// Handles file downloads
///
/// # Errors
///
/// This function will return an error if the download or writing the output
/// file fails.
fn download_file(agent: &Agent, url: &str, output_path: &Path) -> Result<(), Error> {
    let mut response = agent.get(url).call().map_err(|e| Error::Network {
        target: url.to_owned(),
        reason: e.to_string(),
    })?;
    let mut file = File::create(output_path).map_err(|e| Error::Io {
        path:   output_path.display().to_string(),
        reason: e.to_string(),
    })?;
    copy(&mut response.body_mut().as_reader(), &mut file).map_err(|e| Error::Io {
        path:   output_path.display().to_string(),
        reason: e.to_string(),
    })?;
    Ok(())
}

/// Verifies the SHA-256 checksum of a downloaded release against the
/// `.sha256` asset published alongside it, if such an asset exists.
///
/// # Errors
///
/// This function will return an error if the checksum asset exists but the
/// downloaded file does not match it. If no checksum asset exists, the
/// verification is skipped with a warning.
fn verify_checksum(agent: &Agent, checksum_url: &str, downloaded: &Path) -> Result<(), Error> {
    let Ok(mut response) = agent.get(checksum_url).call() else {
        warn!("No checksum asset found for the release; skipping the verification.");
        return Ok(());
    };
    let body = response.body_mut().read_to_string().map_err(|e| Error::Network {
        target: checksum_url.to_owned(),
        reason: e.to_string(),
    })?;
    let Some(published) = body.split_whitespace().next() else {
        return Err(Error::Network {
            target: checksum_url.to_owned(),
            reason: String::from("the published checksum file is empty"),
        });
    };
    let bytes = fs::read(downloaded).map_err(|e| Error::Io {
        path:   downloaded.display().to_string(),
        reason: e.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut actual = String::with_capacity(digest.len().saturating_mul(2));
    for byte in digest {
        let _ = write!(actual, "{byte:02x}");
    }
    if published.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(Error::Network {
            target: checksum_url.to_owned(),
            reason: format!("checksum mismatch: expected `{published}`, got `{actual}`"),
        })
    }
}

/// Create dir that stores cache files
pub fn create_cache_dir() {
    let mut cache_dir_path = std::env::temp_dir();
    cache_dir_path.push("dfu");
    if let Err(e) = fs::create_dir_all(&cache_dir_path) {
        eprintln!("Could not create temp dir: {}. Error: {e}", cache_dir_path.display());
    }
}

/// Handling the self update, to download a new version from GitHub, if one is
/// available. The download is verified against the published checksum if one
/// exists.
pub fn handle_self_update() {
    let agent = &*HTTP_AGENT;
    let Some(latest) = fetch_latest_version(agent) else { return };

    let suffix = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", _) => ".exe",
        ("linux", "x86_64") => "-x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "-aarch64-unknown-linux-musl",
        (os, arch) => {
            error!(
                "Self-update is not available for {os} ({arch}). Please download a release for your platform manually from: {}",
                github_release_url()
            );
            return;
        }
    };

    let file_name = format!("dfu-v{latest}{suffix}");
    let download_url = format!("{}/download/v{latest}/{file_name}", github_release_url());
    debug!("Download URL: {download_url}");
    let Ok(mut full_path) = env::current_dir() else {
        error!("Could not determine the current directory; cannot place the downloaded release.");
        return;
    };
    full_path.push(&file_name);

    if let Err(e) = download_file(agent, &download_url, &full_path) {
        error!("Error while downloading new release: {e}");
        return;
    }
    if let Err(e) = verify_checksum(agent, &format!("{download_url}.sha256"), &full_path) {
        error!("Downloaded release could not be verified and was removed: {e}");
        let _ = fs::remove_file(&full_path);
        return;
    }
    mark_executable(&full_path);
    info!("Successfully downloaded new version to: {}", full_path.display());
}

/// Marks the given file as executable on unix systems, since downloaded
/// release binaries would otherwise not be runnable. On other systems this is
/// a no-op. Failures are logged, since the download itself already succeeded.
#[cfg(unix)]
fn mark_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    match fs::metadata(path) {
        Ok(metadata) => {
            let mut permissions = metadata.permissions();
            // 0o755: read/write/execute for the owner, read/execute for group
            // and others.
            permissions.set_mode(0o755);
            if let Err(e) = fs::set_permissions(path, permissions) {
                warn!("Could not mark the downloaded release as executable: {e}");
            }
        }
        Err(e) => warn!("Could not read the permissions of the downloaded release: {e}"),
    }
}

#[cfg(not(unix))]
fn mark_executable(_path: &Path) {}

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

    #[cfg(unix)]
    #[test]
    fn mark_executable_sets_the_exec_bits() {
        use std::os::unix::fs::PermissionsExt;

        use rand::RngExt;

        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut rng = rand::rng();
        let name: String = (0..15)
            .map(|_| {
                let idx = rng.random_range(0..CHARSET.len());
                char::from(*CHARSET.get(idx).unwrap())
            })
            .collect();
        let mut path = std::env::temp_dir();
        path.push(format!("dfu-exec-test-{name}"));
        fs::write(&path, b"#!/bin/sh\n").expect("Test file can be created");

        let mode = fs::metadata(&path).expect("Metadata can be read").permissions().mode();
        assert_eq!(mode & 0o111, 0);

        crate::utils::mark_executable(&path);

        let mode = fs::metadata(&path).expect("Metadata can be read").permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        let _ = fs::remove_file(&path);
    }

    #[test]
    #[ignore = "requires live network access to Docker Hub and MCR"]
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
        handle_overview(&o).expect("Overview succeeds");
        o.common.quiet = true;
        handle_overview(&o).expect("Overview succeeds");

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
        handle_input(&i).expect("Input succeeds");
        i.common.quiet = true;
        handle_input(&i).expect("Input succeeds");
        i.input = "clamav/clamav:1.5.1-99_base".into();
        handle_input(&i).expect("Input succeeds");

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
        handle_file(&f).expect("File handling succeeds");

        // copy fixtures folder
        assert!(copy_dir_all("./tests/fixtures", "./tests/fixtures.backup").is_ok());
        m.dry_run = false;
        f.dry_run = false;
        handle_multi(&m);
        handle_file(&f).expect("File handling succeeds");

        m.common.arch = Some("amd64".to_owned());
        handle_multi(&m);
        handle_file(&f).expect("File handling succeeds");
        f.common.arch = Some("amd64".to_owned());
        // restore fixtures folder
        let _ = fs::remove_dir_all("./tests/fixtures");
        let _ = fs::rename("./tests/fixtures.backup", "./tests/fixtures").is_ok();
    }
}
