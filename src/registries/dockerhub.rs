use serde::Deserialize;

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
/// The inner response from Dockerhub when requesting a list of tags for a given
/// image.
pub struct DockerHubResult {
    pub images: Vec<HubImage>,
    pub name:   String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
/// The image metadata for a dockerhub image.
pub struct HubImage {
    pub architecture: String,
}

#[allow(dead_code)]
#[derive(Debug, Default, Clone, Deserialize)]
/// The outer response from Dockerhub when requesting a list of tags for a given
/// image.
pub struct DockerHubResponse {
    count:       Option<u32>,
    pub next:    Option<String>,
    previous:    Option<String>,
    pub results: Vec<DockerHubResult>,
}
