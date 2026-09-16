use std::fmt::Display;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::container_image::Error;

/// `TagVariant` is build with the following components:
/// `(prefix)(major)(affix)(minor)(affix)(patch)(suffix)`
#[derive(Debug, Clone, Default, Eq, PartialEq, PartialOrd, Ord, Deserialize, Serialize)]
pub struct TagVariant {
    pub prefix:  Option<String>,
    pub major:   Option<u64>,
    pub minor:   Option<u64>,
    pub patch:   Option<u64>,
    pub affixes: Vec<String>,
    pub suffix:  Option<String>,
}

impl Display for TagVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.prefix {
            Some(prefix) => write!(f, "{prefix}")?,
            None => write!(f, "")?,
        }
        match self.major {
            Some(major) => write!(f, "{major}")?,
            None => write!(f, "")?,
        }
        match self.minor {
            Some(minor) => {
                if self.affixes.is_empty() {
                    write!(f, ".{minor}")?;
                } else {
                    write!(f, "{}{minor}", self.affixes.first().expect("Affixes exists"))?;
                }
            }
            None => write!(f, "")?,
        }
        match self.patch {
            Some(patch) => {
                if self.affixes.len() < 2 {
                    write!(f, ".{patch}")?;
                } else {
                    write!(f, "{}{patch}", self.affixes.get(1).expect("Affixes exists"))?;
                }
            }
            None => write!(f, "")?,
        }
        match &self.suffix {
            Some(suffix) => write!(f, "{suffix}"),
            None => write!(f, ""),
        }
    }
}

impl FromStr for TagVariant {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut prefix = None;
        let mut suffix = None;
        let mut affixes = Vec::new();

        let mut current = s;
        let mut version_parts = Vec::new();

        // Extract prefix (non-digit characters at the start)
        let mut prefix_end = 0;
        while prefix_end < current.len() && !current.as_bytes().get(prefix_end).expect("We ensure bounds.").is_ascii_digit() {
            prefix_end = prefix_end.saturating_add(1);
        }
        if prefix_end > 0 {
            prefix = Some(current.get(..prefix_end).expect("We did the match correctly").to_string());
            current = current.get(prefix_end..).expect("We did the match correctly");
        }

        // Parse version numbers and affixes
        while !current.is_empty() {
            // Extract leading non-digit characters (affixes)
            let mut affix_end = 0;
            while affix_end < current.len() && !current.as_bytes().get(affix_end).expect("We did the match correctly").is_ascii_digit() {
                affix_end = affix_end.saturating_add(1);
            }
            if affix_end > 0 {
                let part = &current.get(..affix_end).expect("We did the match correctly");
                // If this is the last part and starts with '-' or '_', treat as suffix
                if affix_end == current.len() && (part.starts_with('-') || part.starts_with('_')) {
                    suffix = Some(part.to_string());
                } else {
                    affixes.push(part.to_string());
                }
                current = current.get(affix_end..).expect("We did the match correctly");
            }

            // Extract leading digit characters (version numbers)
            let mut num_end = 0;
            while num_end < current.len() && current.as_bytes().get(num_end).expect("We did the match correctly").is_ascii_digit() {
                num_end = num_end.saturating_add(1);
            }
            if num_end > 0 {
                if let Ok(num) = current.get(..num_end).expect("We did the match correctly").parse::<u64>() {
                    version_parts.push(num);
                }
                current = current.get(num_end..).expect("We did the match correctly");
            }
        }

        // Assign version parts
        let major = version_parts.first().copied();
        let minor = version_parts.get(1).copied();
        let patch = version_parts.get(2).copied();

        // Clear affixes if they are only "."
        if affixes.iter().all(|affix| affix == ".") {
            affixes.clear();
        }

        Ok(Self {
            prefix,
            major,
            minor,
            patch,
            affixes,
            suffix,
        })
    }
}

impl TagVariant {
    /// Checks if the prefixes match.
    pub(crate) fn is_same_prefix(&self, rhs: &Self) -> bool {
        match (self.prefix.as_ref(), rhs.prefix.as_ref()) {
            (Some(_), None) | (None, Some(_)) => false,
            (None, None) => true,
            (Some(current), Some(next)) => current == next,
        }
    }

    /// Checks if the suffixes match.
    pub(crate) fn is_same_suffix(&self, rhs: &Self) -> bool {
        match (self.suffix.as_ref(), rhs.suffix.as_ref()) {
            (Some(_), None) | (None, Some(_)) => false,
            (None, None) => true,
            (Some(current), Some(next)) => current == next,
        }
    }

    /// Checks if the affixes match.
    pub(crate) fn is_same_affix(&self, rhs: &Self) -> bool {
        self.affixes == rhs.affixes
    }

    /// Checks if the next major version is greater than the current version.
    pub(crate) const fn is_next_major(&self, rhs: &Self) -> bool {
        match (self.major, rhs.major) {
            (None | Some(_), None) | (None, Some(_)) => false,
            (Some(current), Some(next)) => current < next,
        }
    }

    /// Checks if the next minor version is greater than the current version.
    pub(crate) const fn is_next_minor(&self, rhs: &Self) -> bool {
        match (self.minor, rhs.minor) {
            (None | Some(_), None) | (None, Some(_)) => false,
            (Some(current), Some(next)) => current < next,
        }
    }

    /// Checks if the next patch version is greater than the current version.
    pub(crate) const fn is_next_patch(&self, rhs: &Self) -> bool {
        match (self.patch, rhs.patch) {
            (None | Some(_), None) | (None, Some(_)) => false,
            (Some(current), Some(next)) => current < next,
        }
    }
}
