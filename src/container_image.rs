use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use tracing::{debug, error, info, warn};

use crate::registries::dockerhub::DockerHubResponse;
use crate::registries::mcr::McrResponseEntry;
use crate::registries::{RegistryResponse, TAG_RESULT_LIMIT, TAGS_CACHE};
use crate::tag::Tag;
use crate::utils::{DockerfileUpdate, HTTP_AGENT, Strategy, extract_cache_from_file};

const MCR_PREFIX: &str = "mcr.microsoft.com/";
/// Common registries that are not supported. Unknown registries are detected
/// heuristically (a `.` or `:` in the first path component), this list only
/// provides a better warning message for well-known ones.
const UNSUPPORTED_REGISTRIES: [&str; 5] = ["azurecr.io", "ghcr.io", "gcr.io", "quay.io", "registry.gitlab"];

/// The dockerfile related errors, that may occur during parsing or updating.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("No path was set for the given dockerfile.")]
    MissingPath,
    #[error("Could not find image: `{0}` in the registry.")]
    ImageNotFound(String),
    #[error("Rate limited by the registry while fetching tags for `{0}`. Try a lower --tag-search-limit.")]
    RateLimited(String),
    #[error("The request for `{target}` failed: {reason}")]
    Network { target: String, reason: String },
    #[error("Could not read or write `{path}`: {reason}")]
    Io { path: String, reason: String },
    #[error(transparent)]
    Parse(#[from] ParseError),
}

/// Parsing related errors
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("Image name is empty.")]
    EmptyImage,
    #[error("The given file is empty.")]
    EmptyFile,
    #[error("Could not parse dockerhub response.")]
    InvalidDockerhubResponse,
    #[error("The given FROM instruction could not be parsed.")]
    InvalidFromLine,
}

/// A dockerfile consists of a set of instructions and an optional path, in case
/// it was read from disk and not from standard input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dockerfile {
    instructions:     Vec<DockerInstruction>,
    /// Original path of the file, in case it shall be written again.
    path:             Option<PathBuf>,
    line_ending:      &'static str,
    trailing_newline: bool,
}

impl Dockerfile {
    /// Reads and parses a dockerfile from disk.
    ///
    /// # Errors
    ///
    /// This function will return an error if the file cannot be read or
    /// parsed.
    pub(crate) fn read<P>(path: &P) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        let content = fs::read_to_string(path).map_err(|e| Error::Io {
            path:   path.as_ref().display().to_string(),
            reason: e.to_string(),
        })?;
        let mut dockerfile = Self::parse(&content)?;
        dockerfile.set_path(path);
        Ok(dockerfile)
    }

    /// Returns the original path of the file, if it was read as a file. It
    /// will be `None` if the file was read from standard input.
    pub(crate) const fn get_path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    #[cfg(test)]
    fn get_path_str(&self) -> Option<String> {
        self.path.as_ref().and_then(|p| {
            let s = p.display().to_string();
            if s.is_empty() { None } else { Some(s) }
        })
    }

    fn set_path<P>(&mut self, path: P)
    where
        P: AsRef<Path>,
    {
        let pathbuf = PathBuf::from(path.as_ref());
        self.path = Option::from(pathbuf);
    }

    /// Returns a reference to the instructions in the given dockerfile.
    pub(crate) fn get_instructions(&self) -> &[DockerInstruction] {
        &self.instructions
    }

    /// Returns a mutable reference to the instructions in a given dockerfile.
    pub(crate) const fn get_instructions_mut(&mut self) -> &mut Vec<DockerInstruction> {
        &mut self.instructions
    }

    /// Returns mutable references to the images in a given dockerfile.
    pub(crate) fn get_base_images_mut(&mut self) -> Vec<&mut Box<ContainerImage>> {
        self.get_instructions_mut()
            .iter_mut()
            .filter_map(|instruction| instruction.get_image_mut())
            .collect::<Vec<&mut Box<ContainerImage>>>()
    }

    /// This function will parse a Dockerfile, an empty dockerfile will result
    /// in an error.
    ///
    /// # Errors
    ///
    /// This function will return an error if the file is empty or contains a
    /// line that cannot be parsed.
    pub(crate) fn parse(content: &str) -> Result<Self, Error> {
        let instructions = DockerInstruction::parse_file_content(content)?;
        let line_ending = if content.contains("\r\n") { "\r\n" } else { "\n" };
        let trailing_newline = content.ends_with('\n');
        Ok(Self {
            instructions,
            path: None,
            line_ending,
            trailing_newline,
        })
    }

    /// Writes the dockerfile to the disk, to the given path.
    ///
    /// # Errors
    ///
    /// This function will return an error if the file cannot be written.
    pub(crate) fn write_to_path<P>(&self, path: P) -> Result<(), Error>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let content = format!("{self}"); // since display is implemented.
        match fs::write(path, content) {
            Ok(()) => {
                info!("Successfully written new dockerfile to: {}", path.display());
                Ok(())
            }
            Err(e) => {
                error!("Could not write file: {}, reason: {e}", path.display());
                Err(Error::Io {
                    path:   path.display().to_string(),
                    reason: e.to_string(),
                })
            }
        }
    }

    /// Writes the dockerfile to the disk, using the path stored in the data.
    ///
    /// # Errors
    ///
    /// This function will return an error if the file cannot be written or if
    /// no path was set.
    pub(crate) fn write(&self) -> Result<(), Error> {
        let Some(path) = self.path.as_ref() else {
            error!("Could not write dockerfile, since no path is set.");
            return Err(Error::MissingPath);
        };
        self.write_to_path(path)
    }

    /// Updates the images in the dockerfile with the given strategy. If the
    /// changes shall not be applied, it will print out a preview. Images
    /// whose tags could not be fetched are logged and skipped, so a single
    /// failing image does not abort the whole update.
    ///
    /// # Errors
    ///
    /// This function only returns an error if writing the updated dockerfile
    /// fails. Failures while fetching tags are logged and the affected image
    /// is skipped.
    pub(crate) fn update_images(&mut self, apply_to_file: bool, strategy: &Strategy, limit: Option<u16>, arch: Option<&String>) -> Result<(), Error> {
        for image in self.get_base_images_mut() {
            match image.get_remote_tags(limit, arch, None) {
                Ok(mut docker_image_tags) => {
                    docker_image_tags.sort();
                    if let Some(found_tag) = image.get_tag().find_candidate_tag(&docker_image_tags, strategy) {
                        debug!("Found tag: {found_tag:?}");
                        image.set_tag(found_tag);
                    }
                }
                Err(e) => {
                    error!("Could not fetch tags for `{image}`: {e}. Skipping this image.");
                }
            }
        }

        if apply_to_file && self.get_path().is_some() {
            self.write()
        } else {
            info!("Resulting dockerfile:\n{}", self);
            Ok(())
        }
    }

    /// Generates a list of updates that should be applied to a file, since we
    /// want to preview the changes differently for multi file updates.
    /// Images whose tags could not be fetched are logged and skipped, so a
    /// single failing image does not abort the whole update.
    pub(crate) fn generate_image_updates(
        &self, strategy: &Strategy, limit: Option<u16>, arch: Option<&String>, ignore_versions: &[ContainerImage],
    ) -> DockerfileUpdate {
        let mut result = DockerfileUpdate {
            dockerfile: self.clone(),
            updates:    Vec::new(),
        };
        for (index, image) in result.dockerfile.get_base_images_mut().iter().enumerate() {
            match image.get_remote_tags(limit, arch, None) {
                Ok(mut docker_image_tags) => {
                    docker_image_tags.sort();
                    if let Some(found_tag) = image.get_tag().find_candidate_tag(&docker_image_tags, strategy) {
                        debug!("Found tag: {found_tag:?}");
                        if !ignore_versions.contains(image) {
                            result.updates.push((index, found_tag.clone()));
                        }
                    }
                }
                Err(e) => {
                    error!("Could not fetch tags for `{image}`: {e}. Skipping this image.");
                }
            }
        }
        result
    }
}

impl Display for Dockerfile {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for instruction in self.get_instructions() {
            if !first {
                write!(f, "{}", self.line_ending)?;
            }
            first = false;
            write!(f, "{instruction}")?;
        }
        if self.trailing_newline && !self.instructions.is_empty() {
            write!(f, "{}", self.line_ending)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DockerInstruction {
    /// A `FROM` instruction: the base image, optional flags (e.g.
    /// `--platform=...`) and an optional stage name.
    From(Box<ContainerImage>, Option<String>, Option<String>),
    Raw(String),
}

impl DockerInstruction {
    /// On successful parsing will return a vector of docker instructions.
    fn parse_file_content(content: &str) -> Result<Vec<Self>, Error> {
        if content.is_empty() {
            return Err(Error::Parse(ParseError::EmptyFile));
        }

        let mut instructions = Vec::new();
        for line in content.lines() {
            instructions.push(Self::from_str(line)?);
        }
        Ok(instructions)
    }

    const fn has_valid_image(&self) -> bool {
        match self {
            Self::From(container_image, _, _) => container_image.is_supported() && !container_image.get_tag().allowed_missing,
            Self::Raw(_) => false,
        }
    }

    const fn get_image_mut(&mut self) -> Option<&mut Box<ContainerImage>> {
        if !self.has_valid_image() {
            None
        } else if let Self::From(image, _, _) = self {
            Some(image)
        } else {
            None
        }
    }

    // Used for testing
    #[cfg(test)]
    pub(crate) fn get_full_image_name(&self) -> Option<String> {
        match self {
            Self::From(container_image, _, _) => Some(container_image.to_string()),
            Self::Raw(_) => None,
        }
    }

    // Used for testing
    #[cfg(test)]
    pub(crate) fn get_only_image_name(&self) -> Option<String> {
        match self {
            Self::From(container_image, _, _) => Some(container_image.tagged_name()),
            Self::Raw(_) => None,
        }
    }

    // Used for testing
    #[cfg(test)]
    pub(crate) const fn get_image_tag(&self) -> Option<&Tag> {
        match self {
            Self::From(container_image, _, _) => Some(container_image.get_tag()),
            Self::Raw(_) => None,
        }
    }

    // Used for testing
    #[cfg(test)]
    pub(crate) fn get_stage_name(&self) -> Option<String> {
        match self {
            Self::From(_, _, stage_name) => stage_name.clone(),
            Self::Raw(_) => None,
        }
    }
}

impl Display for DockerInstruction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::From(image, flags, stage_name) => {
                write!(f, "FROM")?;
                if let Some(flags) = flags {
                    write!(f, " {flags}")?;
                }
                write!(f, " {image}")?;
                if let Some(stage_name) = stage_name {
                    write!(f, " AS {stage_name}")?;
                }
                Ok(())
            }
            Self::Raw(s) => write!(f, "{s}"),
        }
    }
}

impl FromStr for DockerInstruction {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some(rest) = strip_from_keyword(s.trim_start()) else {
            return Ok(Self::Raw(s.to_string()));
        };
        match ContainerImage::parse_from_line(rest) {
            Ok((image, flags, stage_name)) => Ok(Self::From(Box::new(image), flags, stage_name)),
            // Not a FROM instruction we understand (e.g. a `FROM` without an
            // image); keep the line untouched instead of failing the file.
            Err(Error::Parse(ParseError::EmptyImage | ParseError::InvalidFromLine)) => Ok(Self::Raw(s.to_string())),
            Err(e) => Err(e),
        }
    }
}

/// Strips the (case-insensitive) `FROM` keyword from a line, if the line is a
/// `FROM` instruction. Returns the remainder, which is either empty or starts
/// with whitespace. Returns `None` for all other lines.
fn strip_from_keyword(line: &str) -> Option<&str> {
    let head = line.get(..4)?;
    if !head.eq_ignore_ascii_case("FROM") {
        return None;
    }
    let rest = line.get(4..).unwrap_or("");
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest)
    } else {
        None
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageMetadata {
    group: Option<String>,
    name:  String,
    tag:   Tag,
}

impl Display for ImageMetadata {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(group) = &self.group {
            write!(f, "{group}/")?;
        }
        if self.tag.allowed_missing {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{}:{}", self.name, self.tag)
        }
    }
}

impl FromStr for ImageMetadata {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cleaned_slice = s.strip_suffix(':').unwrap_or(s);
        if cleaned_slice.trim().is_empty() {
            return Err(Error::Parse(ParseError::EmptyImage));
        }
        if let Some((group, name)) = cleaned_slice.split_once('/') {
            if let Some((name, tag)) = name.split_once(':') {
                return Ok(Self {
                    group: Some(group.to_owned()),
                    name:  name.to_owned(),
                    tag:   tag.parse()?,
                });
            }
        } else if let Some((name, tag)) = cleaned_slice.split_once(':') {
            return Ok(Self {
                group: None,
                name:  name.to_owned(),
                tag:   tag.parse()?,
            });
        }
        // This happens if we reference another image that did not have a :<tag>
        Ok(Self {
            group: None,
            name:  cleaned_slice.to_owned(),
            tag:   Tag::missing(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerImage {
    Dockerhub(ImageMetadata),
    Mcr(ImageMetadata),
    /// An image from a registry that is not supported (or pinned by digest).
    /// The original string is preserved verbatim; such images are never
    /// updated.
    Unsupported(String, Tag),
}

impl ContainerImage {
    /// Returns the group of an image, e.g. `dotnet` for
    /// `mcr.microsoft.com/dotnet/aspnet:9.0.0`, or `None` for library images.
    pub(crate) const fn get_group(&self) -> Option<&String> {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => metadata.group.as_ref(),
            Self::Unsupported(_, _) => None,
        }
    }

    /// Returns the plain image name without group and tag, e.g. `node`,
    /// `python`, `aspnet`.
    pub fn get_name(&self) -> &str {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => &metadata.name,
            Self::Unsupported(s, _) => s,
        }
    }

    /// Returns the path used to query the registry, e.g. `library/node`,
    /// `guacamole/guacamole`, `dotnet/aspnet`. For unsupported images the
    /// full original string is returned.
    pub(crate) fn registry_path(&self) -> String {
        match self {
            Self::Unsupported(s, _) => s.clone(),
            Self::Dockerhub(_) | Self::Mcr(_) => match self.get_group() {
                Some(group) => format!("{group}/{}", self.get_name()),
                None if self.is_mcr() => self.get_name().to_owned(),
                None => format!("library/{}", self.get_name()),
            },
        }
    }

    /// Returns the image name as it can be given to `docker pull`, e.g.
    /// `node`, `guacamole/guacamole` or `mcr.microsoft.com/dotnet/aspnet`.
    pub(crate) fn pullable_name(&self) -> String {
        match self {
            Self::Unsupported(s, _) => s.clone(),
            Self::Dockerhub(_) => self
                .get_group()
                .map_or_else(|| self.get_name().to_owned(), |group| format!("{group}/{}", self.get_name())),
            Self::Mcr(_) => format!("{MCR_PREFIX}{}", self.registry_path()),
        }
    }

    /// Returns the image name including group and tag, e.g. `node:8.0.0`,
    /// `guacamole/guacamole:latest`, `dotnet/aspnet:9.0.0`.
    pub(crate) fn qualified_tagged_name(&self) -> String {
        match self {
            Self::Unsupported(s, _) => s.clone(),
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => format!("{metadata}"),
        }
    }

    /// Returns the image name including the tag, but without the group, e.g.
    /// `node:8.0.0`, `aspnet:9.0.0`.
    #[cfg(test)]
    pub(crate) fn tagged_name(&self) -> String {
        match self {
            Self::Unsupported(s, _) => s.clone(),
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => format!("{}:{}", metadata.name, metadata.tag),
        }
    }

    pub const fn get_tag(&self) -> &Tag {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => &metadata.tag,
            Self::Unsupported(_, tag) => tag,
        }
    }

    fn set_tag(&mut self, tag: &Tag) {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => metadata.tag = tag.clone(),
            Self::Unsupported(_, _) => {}
        }
    }

    /// Whether the image's registry is supported and the image can be
    /// updated.
    pub const fn is_supported(&self) -> bool {
        match self {
            Self::Dockerhub(_) | Self::Mcr(_) => true,
            Self::Unsupported(_, _) => false,
        }
    }

    #[cfg(test)]
    const fn is_latest(&self) -> bool {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => metadata.tag.latest,
            Self::Unsupported(_, _) => false,
        }
    }

    const fn is_mcr(&self) -> bool {
        match self {
            Self::Mcr(_) => true,
            Self::Dockerhub(_) | Self::Unsupported(_, _) => false,
        }
    }

    #[cfg(test)]
    const fn is_dockerhub(&self) -> bool {
        match self {
            Self::Dockerhub(_) => true,
            Self::Mcr(_) | Self::Unsupported(_, _) => false,
        }
    }

    fn get_query_url(&self, page_size: Option<u16>) -> String {
        match self {
            Self::Unsupported(_, _) => String::new(),
            Self::Dockerhub(_) => {
                let full_name = self.registry_path();
                if let Some(page_size) = page_size {
                    return format!("https://hub.docker.com/v2/repositories/{full_name}/tags?page_size={page_size}");
                }
                format!("https://hub.docker.com/v2/repositories/{full_name}/tags?page_size=100")
            }
            Self::Mcr(_) => {
                let full_name = self.registry_path();
                format!("https://mcr.microsoft.com/api/v1/catalog/{full_name}/tags?reg=mar")
            }
        }
    }

    /// Handles the data fetching for dockerhub, since dockerhub only returns a
    /// limited amount of versions, but will return the next query link.
    fn request_dockerhub(&self, limit: Option<u16>, page_size: Option<u16>) -> Result<DockerHubResponse, Error> {
        let agent = &*HTTP_AGENT;
        let image = self.registry_path();

        let mut request_url = Some(self.get_query_url(page_size));
        let mut parsed_response = DockerHubResponse::default();

        while let Some(ref inner_url) = request_url {
            let mut response = match agent.get(inner_url).call() {
                Ok(resp) => {
                    debug!("Received response: {resp:?}");
                    resp
                }
                Err(ureq::Error::StatusCode(404)) => {
                    error!("Image `{image}` was not found on Docker Hub.");
                    return Err(Error::ImageNotFound(image));
                }
                Err(ureq::Error::StatusCode(403 | 429)) => {
                    error!("Docker Hub refused the request for `{image}` (rate limit). URL: {inner_url}");
                    return Err(Error::RateLimited(image));
                }
                Err(e) => {
                    error!("Failed to send request to Docker Hub: {e}");
                    return Err(Error::Network {
                        target: image,
                        reason: e.to_string(),
                    });
                }
            };

            let json: DockerHubResponse = match response.body_mut().read_json() {
                Ok(json) => {
                    debug!("Parsed JSON response successfully.");
                    json
                }
                Err(e) => {
                    error!("Failed to parse JSON response: {e}. Exiting tag retrieval.");
                    if parsed_response.results.is_empty() {
                        // If the error happens on the first iteration
                        return Err(Error::Parse(ParseError::InvalidDockerhubResponse));
                    }
                    break;
                }
            };

            request_url.clone_from(&json.next);
            let mut results = json.results.clone();
            if results.is_empty() {
                info!("Fetching tags done!");
                break;
            }

            parsed_response.results.append(&mut results);
            debug!("Parsed results length: {}", parsed_response.results.len());

            let max_results = limit.map_or(TAG_RESULT_LIMIT, usize::from);
            debug!("Fetched {}/{max_results}.", parsed_response.results.len());

            if parsed_response.results.len() >= max_results {
                info!("Fetching tags done!");
                break;
            }
        }
        {
            let names: Vec<&String> = parsed_response.results.iter().map(|r| &r.name).collect();
            debug!("Found raw tags: {names:?}");
        }

        Ok(parsed_response)
    }

    fn request_mcr(&self) -> Result<Vec<McrResponseEntry>, Error> {
        let agent = &*HTTP_AGENT;
        let image = self.registry_path();

        let url = self.get_query_url(None);
        let mut response = match agent.get(&url).call() {
            Ok(resp) => {
                debug!("Received response: {resp:?}");
                resp
            }
            Err(ureq::Error::StatusCode(404)) => {
                error!("Image `{image}` was not found on the Microsoft Container Registry.");
                return Err(Error::ImageNotFound(image));
            }
            Err(e) => {
                error!("Failed to send request to the Microsoft Container Registry: {e}");
                return Err(Error::Network {
                    target: image,
                    reason: e.to_string(),
                });
            }
        };

        match response.body_mut().read_json::<Vec<McrResponseEntry>>() {
            Ok(json) => Ok(json),
            Err(e) => {
                error!("Failed to parse JSON response: {e}");
                Err(Error::Network {
                    target: image,
                    reason: e.to_string(),
                })
            }
        }
    }

    /// Returns the path of the cache file for this image. The architecture
    /// filter is part of the file name, since cached tags are filtered by
    /// architecture before being cached.
    pub(crate) fn cache_file_name(&self, arch: Option<&str>) -> String {
        let mut file_name = self.registry_path().replace('/', "-");
        if let Some(arch) = arch {
            file_name.push('-');
            file_name.push_str(arch);
        }
        let mut cache_file_name = std::env::temp_dir();
        cache_file_name.push("dfu");
        cache_file_name.push(file_name);
        cache_file_name.set_extension("json");
        cache_file_name.display().to_string()
    }

    /// Fetches all remote tags for this image, filtered by the given
    /// architecture. Results are cached in memory and on disk, keyed by
    /// image and architecture.
    ///
    /// # Errors
    ///
    /// This function will return an error if the tags could not be fetched or
    /// the cache could not be read. Unsupported images and images without a
    /// tag (stage references) return an empty list instead of an error.
    pub(crate) fn get_remote_tags(&self, limit: Option<u16>, arch: Option<&String>, page_size: Option<u16>) -> Result<Vec<Tag>, Error> {
        if let Self::Unsupported(name, _) = self {
            info!("Skipping `{name}`: its registry is not supported for updates.");
            return Ok(Vec::new());
        }
        if self.get_tag().allowed_missing {
            // This happens if we reference a previous stage, so we just return
            return Ok(Vec::new());
        }
        let full_name = self.registry_path();
        if full_name.is_empty() || full_name == "/" || (self.get_group().is_none() && self.get_name().is_empty()) {
            return Ok(Vec::new());
        }

        let arch_str = arch.map(String::as_str);
        let cache_key = tags_cache_key(&full_name, arch_str);
        let cache_file_name = self.cache_file_name(arch_str);
        let mut cached_tags = Vec::new();
        extract_cache_from_file(&cache_key, &mut cached_tags, &cache_file_name)?;

        debug!("Searching for all tags for image: {full_name}");
        let cache = TAGS_CACHE.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(tags) = cache.get(&cache_key) {
            debug!("Found tags in application cache.");
            return Ok(tags.clone());
        }
        drop(cache); // explicit drop, since the cache would still be locked for reading otherwise.

        let registry_response = match self {
            Self::Dockerhub(_) => RegistryResponse::DockerHub(self.request_dockerhub(limit, page_size)?),
            Self::Mcr(_) => RegistryResponse::MicrosoftContainerRegistry(self.request_mcr()?),
            // Unsupported images returned early above.
            Self::Unsupported(_, _) => return Ok(Vec::new()),
        };

        let mut tags = registry_response.get_tags(arch.map(String::as_str));
        tags.sort();
        tags.dedup();

        // Inserting found tags into cache
        let mut cache = TAGS_CACHE.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(cache_key, tags.clone());
        debug!("Inserted tags into cache successfully. Cache contains {} tags for {full_name}", tags.len());
        drop(cache); // drop since we no longer need to keep the lock after the insertion
        match serde_json::to_string_pretty(&tags) {
            Ok(tags_content) => {
                if let Err(e) = fs::write(&cache_file_name, tags_content) {
                    error!("Could not write cache file `{cache_file_name}`: {e}");
                }
            }
            Err(e) => error!("Could not serialise tags for the cache file: {e}"),
        }
        Ok(tags)
    }

    /// Parses the part of a `FROM` instruction that follows the `FROM`
    /// keyword, e.g. ` --platform=$BUILDPLATFORM node:22 AS build`.
    pub(crate) fn parse_from_line(rest: &str) -> Result<(Self, Option<String>, Option<String>), Error> {
        let mut flags: Vec<&str> = Vec::new();
        let mut image: Option<&str> = None;
        let mut stage: Option<String> = None;
        let mut saw_as = false;

        for token in rest.split_whitespace() {
            if saw_as {
                stage = Some(token.to_owned());
                saw_as = false;
                continue;
            }
            if token.eq_ignore_ascii_case("AS") {
                saw_as = true;
                continue;
            }
            if image.is_none() && token.starts_with("--") {
                flags.push(token);
                continue;
            }
            if image.is_none() {
                image = Some(token);
                continue;
            }
            // more tokens than a valid FROM instruction can have.
            return Err(Error::Parse(ParseError::InvalidFromLine));
        }
        if saw_as {
            // a dangling `AS` without a stage name.
            return Err(Error::Parse(ParseError::InvalidFromLine));
        }
        let Some(image) = image else {
            return Err(Error::Parse(ParseError::EmptyImage));
        };

        let flags = if flags.is_empty() { None } else { Some(flags.join(" ")) };
        Ok((image.parse()?, flags, stage))
    }

    /// Updates the tag of a stage's image.
    pub(crate) fn update_image_tag(&mut self, new_tag: &Tag) {
        self.set_tag(new_tag);
    }
}

/// Cache key for the tag caches: the registry path plus the architecture
/// filter, since tags are filtered by architecture before being cached.
fn tags_cache_key(full_name: &str, arch: Option<&str>) -> String {
    arch.map_or_else(|| format!("{full_name}|"), |arch| format!("{full_name}|{arch}"))
}

/// Whether the first path component of `image` looks like a registry host,
/// following the same rule docker itself uses: the component contains a `.`
/// (domain) or `:` (port), or is `localhost`.
fn is_unsupported_registry(image: &str) -> bool {
    match image.split_once('/') {
        Some((first, _)) => first.contains('.') || first.contains(':') || first == "localhost",
        None => false,
    }
}

/// Strips `prefix` from `s`, ignoring ASCII case. `prefix` must be ASCII.
fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix)) {
        s.get(prefix.len()..)
    } else {
        None
    }
}

impl FromStr for ContainerImage {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains('@') {
            warn!("Images pinned by digest (`{s}`) are not updated.");
            return Ok(Self::Unsupported(s.to_string(), Tag::missing()));
        }
        if let Some(rest) = strip_prefix_ascii_ci(s, MCR_PREFIX) {
            return Ok(Self::Mcr(rest.parse()?));
        }
        // `docker.io` is the explicit form of the implicit default registry,
        // `index.docker.io` its legacy alias.
        if let Some(rest) = strip_prefix_ascii_ci(s, "docker.io/") {
            return Ok(Self::Dockerhub(rest.parse()?));
        }
        if let Some(rest) = strip_prefix_ascii_ci(s, "index.docker.io/") {
            return Ok(Self::Dockerhub(rest.parse()?));
        }
        if is_unsupported_registry(s) {
            let registry = s.split_once('/').map_or(s, |(registry, _)| registry);
            if UNSUPPORTED_REGISTRIES.iter().any(|known| s.starts_with(*known)) {
                warn!("Registry `{registry}` is not supported, only Docker Hub and `{MCR_PREFIX}` images can be updated.");
            } else {
                warn!("Unknown registry `{registry}` is not supported, only Docker Hub and `{MCR_PREFIX}` images can be updated.");
            }
            return Ok(Self::Unsupported(s.to_string(), Tag::missing()));
        }
        Ok(Self::Dockerhub(s.parse()?))
    }
}

impl Display for ContainerImage {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(s, _) => write!(f, "{s}"),
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => {
                if self.is_mcr() {
                    write!(f, "{MCR_PREFIX}")?;
                }
                if let Some(group) = &metadata.group {
                    write!(f, "{group}/{}", metadata.name)?;
                } else {
                    write!(f, "{}", metadata.name)?;
                }
                if !metadata.tag.allowed_missing {
                    write!(f, ":{}", metadata.tag)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::as_conversions)]
    use std::fs::{File, remove_file};
    use std::io::Write;
    use std::path::PathBuf;
    use std::str::FromStr;

    use pretty_assertions::assert_eq;
    use rand::RngExt;

    use crate::container_image::{ContainerImage, DockerInstruction, Dockerfile, ImageMetadata};
    use crate::tag::Tag;
    use crate::utils::Strategy;

    const CONTENT: &str = r#"# Comment 1
# Comment 2
# Comment 3
# comment 3.1
FROM alpine:3.0 AS base
FROM base AS something
COPY /app /app
ADD src dest
CMD ["/command"]
ENTRYPOINT ["/entrypoint.sh"]
HEALTHCHECK /bin/true
LABEL multi.label1="value1" \
      multi.label2="value2" \
      other="value3"

MAINTAINER info@example.com
WORKDIR /tmp

FROM node:8.0-alpine AS build
RUN apk install \
        python \
        make \
        g++

# comment in the middle
COPY --from=base /app /app
RUN npm install

FROM node:12.0-alpine AS release
COPY /app /app

FROM python:3.12.3-alpine

FROM nginx:1.26.1-alpine3.19

FROM guacamole/guacamole:1.3.0

# comment 4
FROM mcr.microsoft.com/dotnet/aspnet:9.0.0 AS Final
# comment 5
ARG ARG1=ARG1
ENV ENV1=ENV1 \
    ENV2=ENV2

USER ${USERNAME}:${GROUPNAME}
EXPOSE 1337
SHELL /bin/bash
VOLUME /data
ONBUILD echo "hello world"
STOPSIGNAL SIGTERM

RUN echo && \
    # comment
    echo "hi" && \
    # comment
    ( echo "meow" ) | piped -a "hello"
"#;

    // rand will be a dev dependency
    fn random_string(length: usize) -> String {
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut rng = rand::rng();

        (0..length)
            .map(|_| {
                let idx = rng.random_range(0..CHARSET.len());
                *CHARSET.get(idx).unwrap() as char
            })
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn parse_tests_valid_checks() {
        let dockerfile = Dockerfile::parse(CONTENT).unwrap();
        assert_eq!(dockerfile.get_path(), None);
        assert_eq!(
            dockerfile.get_instructions().first().unwrap(),
            &(DockerInstruction::Raw(String::from("# Comment 1")))
        );
        assert_eq!(
            dockerfile.get_instructions().get(3).unwrap(),
            &(DockerInstruction::Raw(String::from("# comment 3.1")))
        );
        assert_eq!(dockerfile.get_instructions().get(4).unwrap().get_full_image_name().unwrap(), "alpine:3.0");
        assert_eq!(dockerfile.get_instructions().get(4).unwrap().get_stage_name().unwrap(), "base");
        assert_eq!(dockerfile.get_instructions().get(18).unwrap().get_full_image_name().unwrap(), "node:8.0-alpine");
        assert_eq!(
            dockerfile.get_instructions().get(38).unwrap().get_full_image_name().unwrap(),
            "mcr.microsoft.com/dotnet/aspnet:9.0.0"
        );
        assert_eq!(dockerfile.get_instructions().get(38).unwrap().get_only_image_name().unwrap(), "aspnet:9.0.0");
        assert_eq!(
            *dockerfile.get_instructions().get(38).unwrap().get_image_tag().unwrap(),
            "9.0.0".parse::<Tag>().unwrap()
        );
        assert_eq!(CONTENT, dockerfile.to_string());
    }

    #[test]
    fn round_trip() {
        let crlf = "FROM node:22 AS build\r\nRUN echo hi\r\n";
        assert_eq!(crlf, Dockerfile::parse(crlf).unwrap().to_string());

        let no_newline = "FROM node:22 AS build\nRUN echo hi";
        assert_eq!(no_newline, Dockerfile::parse(no_newline).unwrap().to_string());

        let single_line = "FROM alpine:3.20";
        assert_eq!(single_line, Dockerfile::parse(single_line).unwrap().to_string());

        let empty_lines = "FROM alpine:3.20\n\n\nRUN echo hi\n";
        assert_eq!(empty_lines, Dockerfile::parse(empty_lines).unwrap().to_string());
    }

    #[test]
    fn parse_from_edge_cases() {
        let content = "FROM --platform=$BUILDPLATFORM --pull=never node:22 AS build";
        let dockerfile = Dockerfile::parse(content).unwrap();
        assert_eq!(content, dockerfile.to_string());

        let dockerfile = Dockerfile::parse("FROM    node:22   AS   build").unwrap();
        assert_eq!(dockerfile.to_string(), "FROM node:22 AS build");

        let dockerfile = Dockerfile::parse("from node:22 as build").unwrap();
        assert_eq!(dockerfile.to_string(), "FROM node:22 AS build");
        assert_eq!(dockerfile.get_instructions().first().unwrap().get_full_image_name().unwrap(), "node:22");

        let dockerfile = Dockerfile::parse("From node:22").unwrap();
        assert_eq!(dockerfile.to_string(), "FROM node:22");

        let bare = "FROM";
        assert_eq!(bare, Dockerfile::parse(bare).unwrap().to_string());

        let flags_only = "FROM --platform=$BUILDPLATFORM AS build";
        assert_eq!(flags_only, Dockerfile::parse(flags_only).unwrap().to_string());

        let dangling_as = "FROM node:22 AS";
        assert_eq!(dangling_as, Dockerfile::parse(dangling_as).unwrap().to_string());

        let not_from = "FROMscratch node:22";
        assert_eq!(not_from, Dockerfile::parse(not_from).unwrap().to_string());
    }

    #[test]
    fn file_handling() {
        #[cfg(target_os = "linux")]
        let filename = format!("/tmp/{}", random_string(15));
        #[cfg(target_os = "windows")]
        let filename = format!("C:\\Windows\\Temp\\{}", random_string(15));

        let mut file = File::create(&filename).expect("File can be created.");
        assert!(file.write_all(CONTENT.as_bytes()).is_ok());
        let d = Dockerfile::read(&filename).expect("Reading succeeds.");
        let p = d.get_path_str();
        assert_eq!(Some(filename.clone()), p);
        assert!(remove_file(&filename).is_ok());

        assert!(d.write_to_path(&filename).is_ok());
        assert!(remove_file(&filename).is_ok());
    }

    #[test]
    fn write_failures_are_reported() {
        let mut dockerfile = Dockerfile::parse("FROM node:22").unwrap();
        dockerfile.set_path(PathBuf::from("/nonexistent-dfu-test-dir/dockerfile"));
        assert!(matches!(dockerfile.write(), Err(crate::container_image::Error::Io { .. })));

        let dockerfile = Dockerfile::parse("FROM node:22").unwrap();
        assert_eq!(dockerfile.write().err(), Some(crate::container_image::Error::MissingPath));
    }

    #[test]
    fn cache_files() {
        let expected = |file: &str| {
            let mut path = std::env::temp_dir();
            path.push("dfu");
            path.push(file);
            path.display().to_string()
        };

        let image = ContainerImage::Dockerhub(ImageMetadata::from_str("python:3.14").unwrap());
        assert_eq!(image.cache_file_name(None), expected("library-python.json"));
        assert_eq!(image.cache_file_name(Some("amd64")), expected("library-python-amd64.json"));

        let image = ContainerImage::Dockerhub(ImageMetadata::from_str("guacamole/guacamole:latest").unwrap());
        assert_eq!(image.cache_file_name(None), expected("guacamole-guacamole.json"));

        let image = ContainerImage::Mcr(ImageMetadata::from_str("dotnet/aspnet:9.0.0").unwrap());
        assert_eq!(image.cache_file_name(None), expected("dotnet-aspnet.json"));
    }

    #[test]
    fn parse_registry_image_dockerhub() {
        // parsing library dockerhub image
        let image = "node:8.0.0-alpine3.10";
        let registry_image: ContainerImage = image.parse().unwrap();
        assert!(!registry_image.is_latest());
        assert!(registry_image.is_dockerhub());
        assert!(registry_image.is_supported());
        assert!(registry_image.get_group().is_none());
        assert_eq!(registry_image.get_tag(), "8.0.0-alpine3.10".parse::<Tag>().unwrap().as_ref());
        assert_eq!(registry_image.get_name(), "node");
        assert_eq!(registry_image.registry_path(), "library/node");
        assert_eq!(registry_image.pullable_name(), "node");
        assert_eq!(registry_image.qualified_tagged_name(), "node:8.0.0-alpine3.10");
        assert_eq!(registry_image.tagged_name(), "node:8.0.0-alpine3.10");

        let image = "node:8.0-alpine";
        let registry_image: ContainerImage = image.parse().unwrap();
        assert!(!registry_image.is_latest());
        assert!(registry_image.is_dockerhub());
        assert!(registry_image.get_group().is_none());
        assert_eq!(registry_image.get_tag(), "8.0-alpine".parse::<Tag>().unwrap().as_ref());
        assert_eq!(registry_image.get_name(), "node");

        // parsing non-library dockerhub image
        let image = "guacamole/guacamole:latest";
        let registry_image: ContainerImage = image.parse().unwrap();
        assert!(registry_image.is_latest());
        assert!(registry_image.is_dockerhub());
        assert_eq!(registry_image.get_group(), Some(&String::from("guacamole")));
        assert_eq!(registry_image.get_name(), "guacamole");
        assert_eq!(registry_image.registry_path(), "guacamole/guacamole");
        assert_eq!(registry_image.pullable_name(), "guacamole/guacamole");
        assert_eq!(registry_image.qualified_tagged_name(), "guacamole/guacamole:latest");
        assert_eq!(image, &registry_image.to_string());
    }

    #[test]
    fn parse_registry_image_mcr() {
        let image = "mcr.microsoft.com/dotnet/aspnet:9.0.0";
        let registry_image: ContainerImage = image.parse().unwrap();
        assert!(!registry_image.is_latest());
        assert!(registry_image.is_mcr());
        assert!(registry_image.is_supported());
        assert!(registry_image.get_group().is_some());
        assert_eq!(registry_image.get_group(), Some(&String::from("dotnet")));
        assert_eq!(registry_image.get_tag(), "9.0.0".parse::<Tag>().unwrap().as_ref());
        assert_eq!(registry_image.get_name(), "aspnet");
        assert_eq!(registry_image.registry_path(), "dotnet/aspnet");
        assert_eq!(registry_image.pullable_name(), "mcr.microsoft.com/dotnet/aspnet");
        assert_eq!(registry_image.qualified_tagged_name(), "dotnet/aspnet:9.0.0");
        assert_eq!(image, &registry_image.to_string());

        let registry_image: ContainerImage = "MCR.MICROSOFT.COM/dotnet/aspnet:9.0.0".parse().unwrap();
        assert!(registry_image.is_mcr());
        assert_eq!(registry_image.qualified_tagged_name(), "dotnet/aspnet:9.0.0");
    }

    #[test]
    fn parse_unsupported_registry() {
        let image = "ghcr.io/user/image:v1";
        let container_image: ContainerImage = image.parse().unwrap();
        assert!(!container_image.is_supported());
        assert_eq!(container_image.to_string(), image);
        assert!(container_image.get_tag().allowed_missing);

        let image2 = "azurecr.io/myimage:latest";
        let container_image2: ContainerImage = image2.parse().unwrap();
        assert!(!container_image2.is_supported());
        assert_eq!(container_image2.to_string(), image2);

        let image3 = "quay.io/repo/app:v2.0";
        let container_image3: ContainerImage = image3.parse().unwrap();
        assert!(!container_image3.is_supported());
        assert_eq!(container_image3.to_string(), image3);
    }

    #[test]
    fn unknown_registries_are_detected() {
        for image in ["registry.example.com/foo/bar:1.0", "myregistry:5000/img:1.0", "localhost/foo:1.0"] {
            let container_image: ContainerImage = image.parse().unwrap();
            assert!(!container_image.is_supported(), "`{image}` should be unsupported");
            assert_eq!(container_image.to_string(), image);
        }
    }

    #[test]
    fn docker_io_is_the_default_registry() {
        let image = "docker.io/node:8.0.0-alpine3.10";
        let registry_image: ContainerImage = image.parse().unwrap();
        assert!(registry_image.is_dockerhub());
        assert!(registry_image.is_supported());
        assert_eq!(registry_image.registry_path(), "library/node");
        assert_eq!(registry_image.pullable_name(), "node");
        assert_eq!(registry_image.to_string(), "node:8.0.0-alpine3.10");

        let registry_image: ContainerImage = "docker.io/library/node:22".parse().unwrap();
        assert!(registry_image.is_dockerhub());
        assert_eq!(registry_image.registry_path(), "library/node");
        assert_eq!(registry_image.pullable_name(), "library/node");

        let registry_image: ContainerImage = "index.docker.io/node:22".parse().unwrap();
        assert!(registry_image.is_dockerhub());
        assert_eq!(registry_image.registry_path(), "library/node");
        assert_eq!(registry_image.to_string(), "node:22");

        let registry_image: ContainerImage = "DOCKER.IO/node:22".parse().unwrap();
        assert!(registry_image.is_dockerhub());
        assert_eq!(registry_image.registry_path(), "library/node");

        let content = "FROM docker.io/node:22 AS build\n";
        let mut dockerfile = Dockerfile::parse(content).unwrap();
        assert_eq!(dockerfile.get_base_images_mut().len(), 1);
    }

    #[test]
    fn digest_pinned_images_are_unsupported() {
        let image = "node@sha256:0123456789abcdef";
        let container_image: ContainerImage = image.parse().unwrap();
        assert!(!container_image.is_supported());
        assert_eq!(container_image.to_string(), image);
    }

    #[test]
    fn dockerfile_with_unsupported_registry() {
        let content = "FROM ghcr.io/user/image:v1\nFROM node:14";
        let dockerfile = Dockerfile::parse(content).unwrap();
        assert_eq!(dockerfile.get_instructions().len(), 2);

        let first_instruction = dockerfile.get_instructions().first().unwrap();
        assert_eq!(first_instruction.to_string(), "FROM ghcr.io/user/image:v1");

        if let DockerInstruction::From(image, flags, stage) = first_instruction {
            assert!(matches!(&**image, ContainerImage::Unsupported(..)));
            assert!(flags.is_none());
            assert!(stage.is_none());
        } else {
            panic!("Expected From instruction");
        }
    }

    #[test]
    fn unsupported_only_dockerfile_is_not_changed() {
        let content = "FROM ghcr.io/user/image:v1\nFROM node@sha256:abc123\n";
        let mut dockerfile = Dockerfile::parse(content).unwrap();

        dockerfile.update_images(false, &Strategy::Latest, None, None).expect("update_images succeeds");
        assert_eq!(content, dockerfile.to_string());
    }

    #[test]
    #[ignore = "requires live network access to Docker Hub and MCR"]
    fn remote_tags() {
        let registry_image: ContainerImage = "node:8.0.0-alpine3.10".parse().unwrap();
        let tags = registry_image.get_remote_tags(None, None, None).expect("Getting tags finishes successfully");
        assert_ne!(tags, [] as [Tag; 0]);

        let registry_image: ContainerImage = "guacamole/guacamole:latest".parse().unwrap();
        let tags = registry_image
            .get_remote_tags(None, Some(&String::from("amd64")), None)
            .expect("Getting tags finishes successfully");
        assert_ne!(tags, [] as [Tag; 0]);

        let registry_image: ContainerImage = "mcr.microsoft.com/dotnet/aspnet:9.0.0".parse().unwrap();
        let tags = registry_image.get_remote_tags(None, None, None).expect("Getting tags finishes successfully");
        assert_ne!(tags, [] as [Tag; 0]);
    }
}
