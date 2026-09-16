use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use dockerhub::DockerHubResponse;
use mcr::McrResponse;

use crate::tag::Tag;

pub mod dockerhub;
pub mod mcr;

/// The default limit of how many tags should be fetched. Can be overwritten
/// with --tag-search-limit
pub const TAG_RESULT_LIMIT: usize = 1000;
/// Conversion constant
pub const DURATION_HOUR_AS_SECS: u64 = 60 * 60;
/// A cache for quicker lookups for repeated usage of already cached tags. Will
/// be valid for max. 1 hour.
pub static TAGS_CACHE: LazyLock<RwLock<HashMap<String, Vec<Tag>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

#[derive(Debug)]
pub enum RegistryResponse {
    DockerHub(DockerHubResponse),
    MicrosoftContainerRegistry(McrResponse),
}

trait ResponseTagList {
    /// Returns all entries that match the given architecture (if any).
    fn filter_by_arch<'a>(&'a self, arch: Option<&str>) -> Box<dyn Iterator<Item = &'a str> + 'a>;

    /// Parses tags from the filtered entries.
    fn get_tags(&self, arch: Option<&str>) -> Vec<Tag> {
        self.filter_by_arch(arch)
            .filter_map(|name| {
                // Parse the tag and return `Some(tag)` if successful, or `None` if parsing
                // fails.
                name.parse::<Tag>().ok()
            })
            .filter(|tag| tag.major.is_some() || tag.variant.is_some())
            .collect()
    }
}

impl ResponseTagList for DockerHubResponse {
    fn filter_by_arch<'a>(&'a self, arch: Option<&str>) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        let arch_owned = arch.map(std::string::ToString::to_string); // Clone `arch` to avoid lifetime issues
        let iter = self
            .results
            .iter()
            .filter(move |entry| arch_owned.as_ref().is_none_or(|a| entry.images.iter().any(|image| image.architecture == *a)))
            .map(|entry| entry.name.as_str());
        Box::new(iter)
    }
}

impl ResponseTagList for McrResponse {
    fn filter_by_arch<'a>(&'a self, arch: Option<&str>) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        let arch_owned = arch.map(std::string::ToString::to_string); // Clone `arch` to avoid lifetime issues
        let iter = self
            .iter()
            .filter(move |entry| {
                arch_owned
                    .as_ref()
                    .is_none_or(|a| entry.architecture.as_ref().is_some_and(|arch_in_entry| arch_in_entry == a))
            })
            .map(|entry| entry.name.as_str());
        Box::new(iter)
    }
}

impl RegistryResponse {
    /// Returns the list of tags for a given image, optionally filtered by
    /// architecture.
    pub(crate) fn get_tags(&self, arch: Option<&str>) -> Vec<Tag> {
        match self {
            Self::DockerHub(response) => response.get_tags(arch),
            Self::MicrosoftContainerRegistry(response) => response.get_tags(arch),
        }
    }
}
