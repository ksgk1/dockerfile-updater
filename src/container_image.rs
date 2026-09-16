use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use tracing::{debug, error, info};
use ureq::Agent;
use ureq::Error::StatusCode;

use crate::registries::dockerhub::DockerHubResponse;
use crate::registries::mcr::McrResponseEntry;
use crate::registries::{self, RegistryResponse, TAG_RESULT_LIMIT, TAGS_CACHE};
use crate::tag::Tag;
use crate::utils;
use crate::utils::{DockerfileUpdate, Strategy, extract_cache_from_file};

const MCR_PREFIX: &str = "mcr.microsoft.com/";

/// The dockerfile related errors, that may occur during parsing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("No path was set for the given dockerfile.")]
    MissingPath,
    #[error("Could not find image: `{0}` in the docker hub.")]
    ImageNotFound(String),
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
}

/// A dockerfile consists of a set of instructions and an optional path, in case
/// it was ready from disk and not from standard input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dockerfile {
    instructions: Vec<DockerInstruction>,
    /// Original path of the file, in case it shall be written again.
    path:         Option<PathBuf>,
}

impl Dockerfile {
    /// # Returns
    ///
    /// * `Ok(Self)` - The parsed result, with its path set to the input path.
    /// * `Err(Box<dyn std::error::Error>)` - An error if reading or parsing
    ///   fails.
    ///
    /// # Errors
    ///
    /// This function will return an error if the file cannot be read
    /// parsing may return an empty object if the file is empty or invalid.
    pub(crate) fn read<P>(path: &P) -> Result<Self, Box<dyn std::error::Error>>
    where
        P: AsRef<Path>,
    {
        let content = fs::read_to_string(path)?;
        let mut dockerfile = Self::parse(&content)?;
        dockerfile.set_path(path);
        Ok(dockerfile)
    }

    /// # Returns
    ///
    /// This function will return an `Option<Pathbuf>`. This will contain the
    /// original path of the file, if it was read as file It will be `None`
    /// if the file was ready from regular input.
    pub(crate) const fn get_path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    #[allow(unused)]
    /// For testing purposes only
    fn get_path_str(&self) -> Option<String> {
        self.path.as_ref().and_then(|p| {
            let s = p.display().to_string();
            if s.is_empty() { None } else { Some(s) }
        })
    }

    #[allow(unused)]
    /// For testing purposes only
    fn set_path<P>(&mut self, path: P)
    where
        P: AsRef<Path>,
    {
        let pathbuf = PathBuf::from(path.as_ref());
        self.path = Option::from(pathbuf);
    }

    #[allow(unused)]
    /// For testing purposes only
    fn clear_path(&mut self) {
        self.path = None;
    }

    /// # Returns
    ///
    /// This function will return a reference to the instructions in the given
    /// dockerfile.
    pub(crate) fn get_instructions(&self) -> &[DockerInstruction] {
        &self.instructions
    }

    /// # Returns
    ///
    /// This function will return a mutable reference to the instructions in a
    /// given dockerfile.
    pub(crate) const fn get_instructions_mut(&mut self) -> &mut Vec<DockerInstruction> {
        &mut self.instructions
    }

    /// # Returns
    ///
    /// This function will return a mutable references to the images in a given
    /// dockerfile.
    pub(crate) fn get_base_images_mut(&mut self) -> Vec<&mut Box<ContainerImage>> {
        self.get_instructions_mut()
            .iter_mut()
            .filter_map(|instruction| instruction.get_image_mut())
            .collect::<Vec<&mut Box<ContainerImage>>>()
    }

    /// This function will parse a Dockerfile, an empty dockerfile will result
    /// in an error.
    pub(crate) fn parse(content: &str) -> Result<Self, Error> {
        let instructions = DockerInstruction::parse_file_content(content)?;
        Ok(Self { instructions, path: None })
    }

    /// Writes the dockerfile to the disk, with the given path. It ignores the
    /// path set in the data. # Returns
    ///
    /// * `Ok()` - If the file can be successfully written.
    /// * `Err(Box<dyn std::error::Error>)` - An error if writing the file
    ///   fails.
    ///
    /// # Errors
    ///
    /// This function will return an error if the file cannot be written.
    #[allow(unused)]
    /// For testing purposes only
    pub(crate) fn write_to_path(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let content = format!("{self}"); // since display is implemented.
        match fs::write(path, content) {
            Ok(()) => {
                info!("Successfully written new dockerfile to: {path}");
                Ok(())
            }
            Err(e) => {
                error!("Could not write file: {path}, reason: {e}");
                Err(e.into())
            }
        }
    }

    /// Writes the dockerfile to the disk, with the given path. Will use the
    /// path given in the data. # Returns
    ///
    /// * `Ok()` - If the file can be successfully written.
    /// * `Err(Box<dyn std::error::Error>)` - An error if writing the file
    ///   fails.
    ///
    /// # Errors
    ///
    /// This function will return an error if the file cannot be written or if
    /// no path was set.
    pub(crate) fn write(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.path.is_some() {
            let content = format!("{self}"); // since display is implemented.
            match fs::write(self.path.clone().expect("Path is set."), content) {
                Ok(()) => {
                    info!("Successfully written new dockerfile to: {}", self.path.clone().expect("Path is set").display());
                    return Ok(());
                }
                Err(e) => {
                    error!("Could not write file: {}, reason: {e}", self.path.clone().expect("Path is set").display());
                    return Err(e.into());
                }
            }
        }
        error!("Could not write dockerfile, since no path is set.");
        Err(Box::new(Error::MissingPath))
    }

    /// Updates the images in a the dockerfile with the given strategy. If the
    /// changes shall not be applied, it will print out a preview.
    pub(crate) fn update_images(&mut self, apply_to_file: bool, strategy: &Strategy, limit: Option<u16>, arch: Option<&String>) {
        for image in self.get_base_images_mut() {
            if image.is_empty() {
                // If this happens, we can not fetch any data. This can be cause by comments
                // above the first FROM instruction, since it is considered an empty stage with
                // an empty image. This can be caused by referencing previous stages.
                continue;
            }
            let mut docker_image_tags = image.get_remote_tags(limit, arch, None).expect("Tags could be found.");
            docker_image_tags.sort();

            if let Some(found_tag) = image.get_tag().find_candidate_tag(&docker_image_tags, strategy) {
                debug!("Found tag: {found_tag:?}");
                image.set_tag(&found_tag.clone());
            }
        }

        if apply_to_file && self.get_path().is_some() {
            let _ = self.write();
        } else {
            info!("Resulting dockerfile:\n{}", self);
        }
    }

    /// Generates a list of updates that should be applied to a file, since we
    /// want to preview the changes differently for multi file updates.
    pub(crate) fn generate_image_updates(
        &self, strategy: &Strategy, limit: Option<u16>, arch: Option<&String>, ignore_versions: &[ContainerImage],
    ) -> DockerfileUpdate {
        let mut result = DockerfileUpdate {
            dockerfile: self.clone(),
            updates:    Vec::new(),
        };
        for (index, image) in result.dockerfile.get_base_images_mut().iter().enumerate() {
            if image.get_tag().allowed_missing {
                continue;
            }
            let mut docker_image_tags = image.get_remote_tags(limit, arch, None).expect("Tags could be found.");
            docker_image_tags.sort();
            if let Some(found_tag) = image.get_tag().find_candidate_tag(&docker_image_tags, strategy) {
                debug!("Found tag: {found_tag:?}");
                if !ignore_versions.contains(image) {
                    result.updates.push((index, found_tag.clone()));
                }
            }
        }
        result
    }
}

impl Display for Dockerfile {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for instructions in self.get_instructions() {
            write!(f, "{instructions}")?;
        }
        write!(f, "")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DockerInstruction {
    From(Box<ContainerImage>, Option<String>),
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
            Self::From(container_image, _) => !container_image.get_tag().allowed_missing,
            Self::Raw(_) => false,
        }
    }

    const fn get_image_mut(&mut self) -> Option<&mut Box<ContainerImage>> {
        if !self.has_valid_image() {
            None
        } else if let Self::From(image, _) = self {
            Some(image)
        } else {
            None
        }
    }

    // Used for testing
    #[cfg(test)]
    pub(crate) fn get_full_image_name(&self) -> Option<String> {
        match self {
            Self::From(container_image, _) => Some(container_image.to_string()),
            Self::Raw(_) => None,
        }
    }

    // Used for testing
    #[cfg(test)]
    pub(crate) fn get_only_image_name(&self) -> Option<String> {
        match self {
            Self::From(container_image, _) => Some(container_image.get_tagged_name()),
            Self::Raw(_) => None,
        }
    }

    // Used for testing
    #[cfg(test)]
    pub(crate) const fn get_image_tag(&self) -> Option<&Tag> {
        match self {
            Self::From(container_image, _) => Some(container_image.get_tag()),
            Self::Raw(_) => None,
        }
    }

    // Used for testing
    #[cfg(test)]
    pub(crate) fn get_stage_name(&self) -> Option<String> {
        match self {
            Self::From(_, stage_name) => stage_name.clone(),
            Self::Raw(_) => None,
        }
    }
}

impl Display for DockerInstruction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::From(image, stage_name) => match stage_name {
                Some(stage_name) => {
                    writeln!(f, "FROM {image} AS {stage_name}")
                }
                None => {
                    writeln!(f, "FROM {image}")
                }
            },
            Self::Raw(s) => writeln!(f, "{s}"),
        }
    }
}

impl FromStr for DockerInstruction {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim_start().to_uppercase().starts_with("FROM ") {
            let (image, stage_name) = ContainerImage::parse_from_line(s)?;
            return Ok(Self::From(Box::new(image), stage_name));
        }
        Ok(Self::Raw(s.to_string()))
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
        if self.group.is_some() {
            write!(f, "{}/", self.group.clone().expect("Group exists"))?;
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
        let cleaned_slice = if s.ends_with(':') {
            s.strip_suffix(':').expect("We just checked if the slice ends with a colon")
        } else {
            s
        };
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
        //This happens if we reference another image that did not have a :<tag>
        Ok(Self {
            group: None,
            name:  cleaned_slice.to_owned(),
            tag:   Tag {
                major:           None,
                minor:           None,
                patch:           None,
                variant:         None,
                allowed_missing: true,
                latest:          false,
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerImage {
    Dockerhub(ImageMetadata),
    Mcr(ImageMetadata),
}

impl Default for ContainerImage {
    fn default() -> Self {
        Self::Dockerhub(ImageMetadata::default())
    }
}

#[allow(unused)]
impl ContainerImage {
    /// Returns the full name for a  given image, e.g. Some(library),
    /// Some(dotnet) or None
    const fn get_group(&self) -> Option<&String> {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => metadata.group.as_ref(),
        }
    }

    /// Returns the full name for a  given image, e.g. library, dotnet or "" if
    /// no group was set
    fn get_group_string(&self) -> String {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => metadata.group.clone().unwrap_or_default(),
        }
    }

    /// Returns the full name for a  given image, e.g. node, python, aspnet
    pub const fn get_name(&self) -> &String {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => &metadata.name,
        }
    }

    /// Returns the full name for a  given image, e.g. node, library/python,
    /// dotnet/aspnet
    pub(crate) fn get_full_name(&self) -> String {
        match self {
            Self::Dockerhub(metadata) => {
                if metadata.tag.allowed_missing {
                    self.get_name().clone()
                } else if self.get_group().is_some() {
                    format!("{}/{}", self.get_group().expect("Group was set."), self.get_name())
                } else {
                    format!("library/{}", self.get_name())
                }
            }
            Self::Mcr(metadata) => {
                if self.get_group().is_some() {
                    format!("{}/{}", self.get_group().expect("Group was set"), self.get_name())
                } else {
                    String::from(self.get_name())
                }
            }
        }
    }

    /// Returns the full name for a  given image, e.g. node, library/python,
    /// dotnet/aspnet
    pub(crate) fn get_dockerimage_name(&self) -> String {
        match self {
            Self::Dockerhub(metadata) => {
                if metadata.tag.allowed_missing {
                    self.get_name().clone()
                } else if self.get_group().is_some() {
                    format!("{}/{}", self.get_group().expect("Group was set."), self.get_name())
                } else {
                    self.get_name().to_owned()
                }
            }
            Self::Mcr(metadata) => {
                if self.get_group().is_some() {
                    format!("{MCR_PREFIX}{}/{}", self.get_group().expect("Group was set"), self.get_name())
                } else {
                    format!("{MCR_PREFIX}{}", self.get_name())
                }
            }
        }
    }

    /// Returns the full name for a  given image, e.g. node:<tag>,
    /// library/python:<tag>, dotnet/aspnet:<tag>
    pub(crate) fn get_full_tagged_name(&self) -> String {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => {
                if self.get_group_string().is_empty() {
                    format!("{}:{}", self.get_name(), self.get_tag())
                } else {
                    format!("{}/{}:{}", self.get_group_string(), self.get_name(), self.get_tag())
                }
            }
        }
    }

    /// Returns the full name for a  given image, e.g. node:<tag>, python:<tag>,
    /// aspnet:<tag>
    pub(crate) fn get_tagged_name(&self) -> String {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => {
                format!("{}:{}", self.get_name(), self.get_tag())
            }
        }
    }

    pub const fn get_tag(&self) -> &Tag {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => &metadata.tag,
        }
    }

    fn set_tag(&mut self, tag: &Tag) {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => metadata.tag = tag.clone(),
        }
    }

    const fn is_latest(&self) -> bool {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => metadata.tag.latest,
        }
    }

    const fn is_mcr(&self) -> bool {
        match self {
            Self::Dockerhub(_) => false,
            Self::Mcr(_) => true,
        }
    }

    const fn is_dockerhub(&self) -> bool {
        match self {
            Self::Dockerhub(_) => true,
            Self::Mcr(_) => false,
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Dockerhub(image_metadata) | Self::Mcr(image_metadata) => *image_metadata == ImageMetadata::default(),
        }
    }

    fn get_query_url(&self, page_size: Option<u16>) -> String {
        match self {
            Self::Dockerhub(_) => {
                let full_name = self.get_full_name();
                if let Some(page_size) = page_size {
                    return format!("https://hub.docker.com/v2/repositories/{full_name}/tags?page_size={page_size}");
                }
                format!("https://hub.docker.com/v2/repositories/{full_name}/tags?page_size=100")
            }
            Self::Mcr(_) => {
                let full_name = self.get_full_name();
                format!("https://mcr.microsoft.com/api/v1/catalog/{full_name}/tags?reg=mar")
            }
        }
    }

    /// Handles the data fetching for dockerhub, since dockerhub only returns a
    /// limited amount of versions, but will return the next query link.
    fn request_dockerhub(&self, limit: Option<u16>, page_size: Option<u16>) -> Result<DockerHubResponse, Box<dyn std::error::Error>> {
        // build agent with global timeout
        let config = Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build();
        let agent: Agent = config.into();

        let mut request_url = Some(self.get_query_url(page_size));
        let mut parsed_response = DockerHubResponse::default();

        while let Some(ref inner_url) = request_url {
            let mut response = match agent.get(inner_url).call() {
                Ok(resp) => {
                    debug!("Received response: {:?}", resp);
                    resp
                }
                Err(e) => {
                    if e.to_string() == StatusCode(403).to_string() {
                        error!("Pagination offset too large for anonymous requests; sign in to page further");
                        error!("URL: {inner_url}");
                    }
                    error!("Failed to send request to DockerHub: {e}");
                    return Err(Box::new(Error::ImageNotFound(self.get_full_name())));
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
                        return Err(Box::new(Error::Parse(ParseError::InvalidDockerhubResponse)));
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

            let limit = limit.unwrap_or_else(|| u16::try_from(TAG_RESULT_LIMIT).expect("Tag result limit is <= 65535"));
            info!("Fetched {}/{}.", parsed_response.results.len(), limit);

            if parsed_response.results.len() >= usize::from(limit) {
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

    fn request_mcr(&self) -> Result<Vec<McrResponseEntry>, Box<dyn std::error::Error>> {
        // build agent with global timeout
        let config = Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build();
        let agent: Agent = config.into();

        let url = self.get_query_url(None);
        let mut response = match agent.get(&url).call() {
            Ok(resp) => {
                debug!("Received response: {:?}", resp);
                resp
            }
            Err(e) => {
                error!("Failed to send request to DockerHub: {e}");
                return Err(Box::new(Error::ImageNotFound(self.get_full_name())));
            }
        };

        match response.body_mut().read_json::<Vec<McrResponseEntry>>() {
            Ok(json) => Ok(json),
            Err(e) => {
                error!("Failed to parse JSON response: {e}");
                Err(Box::new(Error::ImageNotFound(self.get_full_name())))
            }
        }
    }

    /// Creates
    pub(crate) fn cache_file_name(&self) -> String {
        let mut cache_file_name = std::env::temp_dir();
        cache_file_name.push("dfu");
        cache_file_name.push(self.get_full_name().replace('/', "-"));
        cache_file_name.add_extension("json");
        cache_file_name.display().to_string()
    }

    pub(crate) fn get_remote_tags(&self, limit: Option<u16>, arch: Option<&String>, page_size: Option<u16>) -> Result<Vec<Tag>, Box<dyn std::error::Error>> {
        utils::create_cache_dir();
        if self.get_tag().clone().allowed_missing {
            // This happens if we reference a previous stage, so we just return
            return Ok(Vec::new());
        }
        let full_name = &self.get_full_name();
        let mut tags = Vec::<Tag>::new();
        if full_name.is_empty() || full_name == "/" || (self.get_group().is_none() && self.get_name().is_empty()) {
            return Ok(tags);
        }
        let cache_file_name = self.cache_file_name();
        extract_cache_from_file(full_name, &mut tags, &cache_file_name)?;

        debug!("Searching for all tags for image: {full_name}");
        let cache = TAGS_CACHE.read().expect("Tags cache can be read.");
        if cache.contains_key(full_name) {
            debug!("Found tags in application cache.");
            tags.clone_from(cache.get(full_name).expect("Version exists in cache."));
            Ok(tags)
        } else {
            drop(cache); // explicit drop, since the cache would still be locked for reading otherwise.

            let registry_response: RegistryResponse = match &self {
                Self::Dockerhub(image_metadata) => registries::RegistryResponse::DockerHub(self.request_dockerhub(limit, page_size)?),
                Self::Mcr(image_metadata) => registries::RegistryResponse::MicrosoftContainerRegistry(self.request_mcr()?),
            };

            let mut tags = registry_response.get_tags(arch.map(std::string::String::as_str));
            tags.sort();
            tags.dedup();
            let tags = tags;

            // Inserting found tags into cache
            let mut cache = TAGS_CACHE.write().expect("Cache can be written.");
            if cache.insert(full_name.clone(), tags.clone()).is_none() {
                debug!(
                    "Inserted tags into cache successfully. Cache contains {} tags for {full_name}",
                    cache.get(full_name).expect("Version exists in cache.").len()
                );
            }
            drop(cache); // drop since we no longer need to keep the lock after the insertion
            {
                let tags_content = serde_json::to_string_pretty(&tags);
                let _ = fs::write(cache_file_name, tags_content.expect("Tags can be turned into json string."));
            }
            Ok(tags)
        }
    }

    pub(crate) fn parse_from_line(line: &str) -> Result<(Self, Option<String>), Error> {
        let trimmed = line.trim_start().replace("  ", " "); // replace multispaces
        let without_from = trimmed.strip_prefix("FROM").or_else(|| trimmed.strip_prefix("from")).unwrap_or(&trimmed).trim();

        without_from.to_ascii_lowercase().find(" as").map_or_else(
            || without_from.trim().parse().map(|parsed| (parsed, None)),
            |i| {
                let (image, alias) = without_from.split_at(i);
                let alias = alias.get(3..).expect("We ensured to check for the string").trim(); // skip " as"
                image.trim().parse().map(|parsed| (parsed, Some(alias.to_owned())))
            },
        )
    }

    /// Updates the tag of a stage's image.
    pub(crate) fn update_image_tag(&mut self, new_tag: &Tag) {
        self.set_tag(new_tag);
    }
}

impl FromStr for ContainerImage {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(if s.to_ascii_lowercase().starts_with(MCR_PREFIX) {
            Self::Mcr(s.strip_prefix(MCR_PREFIX).expect("Prefix exists.").parse()?)
        } else {
            Self::Dockerhub(s.parse()?)
        })
    }
}

impl Display for ContainerImage {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dockerhub(metadata) | Self::Mcr(metadata) => {
                if self.is_mcr() {
                    write!(f, "mcr.microsoft.com/")?;
                }
                if metadata.group.is_some() {
                    write!(f, "{}/{}", metadata.group.clone().expect("Group was set"), metadata.name)?;
                } else {
                    write!(f, "{}", metadata.name)?;
                }
                if metadata.tag.allowed_missing {
                    write!(f, "{}", metadata.tag)?;
                } else {
                    write!(f, ":{}", metadata.tag)?;
                }
                write!(f, "")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::as_conversions)]
    use std::fs::{File, remove_file};
    use std::io::Write;
    use std::str::FromStr;

    use pretty_assertions::assert_eq;
    use rand::RngExt;

    use crate::container_image::{ContainerImage, DockerInstruction, Dockerfile, ImageMetadata};
    use crate::tag;
    use crate::tag::Tag;

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
                *CHARSET.get(idx).expect("We did the math correctly.") as char
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
        println!("{d}");
        assert!(remove_file(&filename).is_ok());

        assert!(d.write_to_path(&filename).is_ok());
        assert!(remove_file(&filename).is_ok());
    }

    #[test]
    fn cache_files() {
        let image = ContainerImage::Dockerhub(ImageMetadata::from_str("python:3.14").unwrap());
        let path = image.cache_file_name();
        assert_eq!(path, "/tmp/dfu/library-python.json".to_owned());

        let image = ContainerImage::Dockerhub(ImageMetadata::from_str("guacamole/guacamole:latest").unwrap());
        let path = image.cache_file_name();
        assert_eq!(path, "/tmp/dfu/guacamole-guacamole.json".to_owned());

        let image = ContainerImage::Mcr(ImageMetadata::from_str("dotnet/aspnet:9.0.0").unwrap());
        let path = image.cache_file_name();
        assert_eq!(path, "/tmp/dfu/dotnet-aspnet.json".to_owned());
    }

    #[test]
    fn parse_registry_image_dockerhub() {
        // parsing library dockerhub image
        let image = "node:8.0.0-alpine3.10";
        let registry_image: ContainerImage = image.parse().unwrap();
        assert!(!registry_image.is_latest());
        assert!(registry_image.is_dockerhub());
        assert!(registry_image.get_group().is_none());
        assert_eq!(registry_image.get_tag(), "8.0.0-alpine3.10".parse::<Tag>().unwrap().as_ref());
        assert_eq!(registry_image.get_name(), "node");
        let tags = registry_image.get_remote_tags(None, None, None);
        assert!(tags.is_ok());
        assert_ne!(tags.unwrap(), [] as [tag::Tag; 0]);

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
        assert_eq!(image, &registry_image.to_string());
        let tags = registry_image.get_remote_tags(None, Some(&String::from("amd64")), None);
        assert!(tags.is_ok());
        assert_ne!(tags.unwrap(), [] as [tag::Tag; 0]);
    }

    #[test]
    fn parse_registry_image_mcr() {
        let image = "mcr.microsoft.com/dotnet/aspnet:9.0.0";
        let registry_image: ContainerImage = image.parse().unwrap();
        assert!(!registry_image.is_latest());
        assert!(registry_image.is_mcr());
        assert!(registry_image.get_group().is_some());
        assert_eq!(registry_image.get_group(), Some(&String::from("dotnet")));
        assert_eq!(registry_image.get_tag(), "9.0.0".parse::<Tag>().unwrap().as_ref());
        assert_eq!(registry_image.get_name(), "aspnet");
        assert_eq!(image, &registry_image.to_string());
        let tags = registry_image.get_remote_tags(None, None, None);
        assert!(tags.is_ok());
        assert_ne!(tags.unwrap(), [] as [tag::Tag; 0]);
    }
}
