use serde::Deserialize;

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
/// The inner response from Microsoft Container Registry when requesting a list
/// of tags for a given image.
pub struct McrResponseEntry {
    pub name:         String,
    pub architecture: Option<String>,
}

pub type McrResponse = Vec<McrResponseEntry>;
