use std::fmt::Display;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::container_image::Error;
use crate::tag::variant::TagVariant;
use crate::utils::Strategy;

pub mod variant;

/// `Tag` is build with the following components:
/// `(major).(minor).(patch)(variant)`
#[derive(Debug, Clone, Default, Eq, PartialEq, PartialOrd, Ord, Deserialize, Serialize)]
pub struct Tag {
    pub major:           Option<u64>,
    pub minor:           Option<u64>,
    pub patch:           Option<u64>,
    pub variant:         Option<TagVariant>,
    /// needed for images that reference other stages
    pub allowed_missing: bool,
    pub latest:          bool,
}

impl Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.latest {
            write!(f, "latest")
        } else {
            match self.major {
                Some(major) => write!(f, "{major}")?,
                None => write!(f, "")?,
            }
            match self.minor {
                Some(minor) => write!(f, ".{minor}")?,
                None => write!(f, "")?,
            }
            match self.patch {
                Some(patch) => write!(f, ".{patch}")?,
                None => write!(f, "")?,
            }
            match &self.variant {
                Some(variant) => {
                    write!(f, "{variant}")
                }
                None => write!(f, ""),
            }
        }
    }
}

fn split_version_and_rest(s: &str) -> (&str, &str) {
    let mut split_at = 0;
    for (i, c) in s.char_indices() {
        if !c.is_ascii_digit() && c != '.' {
            break;
        }
        split_at = i.saturating_add(1);
    }
    s.split_at(split_at)
}

impl FromStr for Tag {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim().eq_ignore_ascii_case("latest") {
            return Ok(Self {
                major:           None,
                minor:           None,
                patch:           None,
                variant:         None,
                allowed_missing: false,
                latest:          true,
            });
        }
        let (version, rest) = split_version_and_rest(s);

        let version_nums: Vec<&str> = version.split('.').collect();
        let major = version_nums.first().and_then(|v| v.parse().ok());
        let minor = version_nums.get(1).and_then(|v| v.parse().ok());
        let patch = version_nums.get(2).and_then(|v| v.parse().ok());

        let variant = if rest.is_empty() { None } else { Some(TagVariant::from_str(rest)?) };

        Ok(Self {
            major,
            minor,
            patch,
            variant,
            allowed_missing: false,
            latest: false,
        })
    }
}

impl AsRef<Self> for Tag {
    #[inline]
    fn as_ref(&self) -> &Self {
        self
    }
}

impl Tag {
    /// Checks if the major versions match.
    pub(crate) const fn is_same_major(&self, rhs: &Self) -> bool {
        match (self.major, rhs.major) {
            (None | Some(_), None) | (None, Some(_)) => false,
            (Some(current), Some(next)) => current == next,
        }
    }

    /// Checks if the minor versions match, requires the major versions to be
    /// the same.
    pub(crate) const fn is_same_minor(&self, rhs: &Self) -> bool {
        match (self.minor, rhs.minor) {
            (None | Some(_), None) | (None, Some(_)) => false,
            (Some(current), Some(next)) => current == next,
        }
    }

    pub(crate) const fn has_minor(&self) -> bool {
        self.minor.is_some()
    }

    /// Ensures that the variant prefix and suffix match properly
    pub(crate) fn is_same_variant(&self, rhs: &Self) -> bool {
        match (self.variant.as_ref(), rhs.variant.as_ref()) {
            (Some(_), None) | (None, Some(_)) => false,
            (None, None) => true,
            (Some(current), Some(next)) => current.is_same_prefix(next) && current.is_same_suffix(next) && current.is_same_affix(next),
        }
    }

    /// Checks if the next major version is greater than the current version.
    pub(crate) const fn is_next_major(&self, rhs: &Self) -> bool {
        rhs.has_minor()
            && match (self.major, rhs.major) {
                (None | Some(_), None) | (None, Some(_)) => false,
                (Some(current), Some(next)) => current < next,
            }
    }

    /// Checks if the next minor version is greater than the current version.
    pub(crate) const fn is_next_minor(&self, rhs: &Self) -> bool {
        self.is_same_major(rhs)
            && match (self.minor, rhs.minor) {
                (None | Some(_), None) | (None, Some(_)) => false,
                (Some(current), Some(next)) => current < next,
            }
    }

    /// Checks if the next patch version is greater than the current version or
    /// if any of the version within the variant are greater than in the current
    /// version. See check functions for `TagVariant`.
    pub(crate) fn is_next_patch(&self, rhs: &Self) -> bool {
        self.is_same_major(rhs)
            && self.is_same_minor(rhs)
            && match (self.patch, rhs.patch) {
                (None | Some(_), None) | (None, Some(_)) => false,
                (Some(current), Some(next)) => {
                    current < next
                        || current == next
                            && match (self.variant.as_ref(), rhs.variant.as_ref()) {
                                (None | Some(_), None) | (None, Some(_)) => false,
                                (Some(current_variant), Some(next_variant)) => {
                                    current_variant.is_same_prefix(next_variant)
                                        && current_variant.is_same_affix(next_variant)
                                        && current_variant.is_next_major(next_variant)
                                        || current_variant.is_next_minor(next_variant)
                                        || current_variant.is_next_patch(next_variant)
                                }
                            }
                }
            }
    }

    /// Will return an Option, to an item in the list, with a tag that matches
    /// the strategy.
    pub(crate) fn find_candidate_tag<'a>(&self, tag_list: &'a [Self], strategy: &Strategy) -> Option<&'a Self> {
        let mut filtered_tags: Vec<&Self> = tag_list
            .iter()
            .filter(|tag| {
                self.is_same_variant(tag)
                    && match strategy {
                        Strategy::NextPatch | Strategy::LatestPatch => self.is_next_patch(tag),
                        Strategy::NextMinor | Strategy::LatestMinor => self.is_next_minor(tag),
                        Strategy::NextMajor | Strategy::LatestMajor => self.is_next_major(tag),
                        Strategy::Latest => self.is_next_major(tag) || self.is_next_minor(tag) || self.is_next_patch(tag),
                    }
            })
            .collect();

        if filtered_tags.is_empty() {
            debug!("No matching tags found");
            return None;
        }

        // Ensuring that the results are sorted, in ascending order,
        // so that the first entry is closes to the starting tag.
        // The last entry in the list is the latest one depending on the chosen
        // strategy.
        filtered_tags.sort();

        for result_tag in &filtered_tags {
            debug!("{result_tag}");
        }

        match strategy {
            Strategy::NextMajor | Strategy::NextMinor | Strategy::NextPatch => filtered_tags.first().copied(),
            Strategy::LatestMajor | Strategy::LatestMinor | Strategy::LatestPatch | Strategy::Latest => filtered_tags.last().copied(),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use crate::tag::Tag;
    use crate::tag::variant::TagVariant;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn parsing() {
        let expected = "3.15.0a6-slim-trixie";
        let tag: Tag = expected.parse().unwrap();

        assert_eq!(tag.major, Some(3));
        assert_eq!(tag.minor, Some(15));
        assert_eq!(tag.patch, Some(0));
        assert_eq!(tag.variant.clone().unwrap().prefix, Some("a".to_owned()));
        assert_eq!(tag.variant.clone().unwrap().major, Some(6));
        assert_eq!(tag.variant.clone().unwrap().suffix, Some("-slim-trixie".to_owned()));
        assert_eq!(tag.to_string(), expected);

        let expected = "3.15.0a6-alpine3.23";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.major, Some(3));
        assert_eq!(tag.minor, Some(15));
        assert_eq!(tag.patch, Some(0));
        assert_eq!(tag.variant.clone().unwrap().prefix, Some("a".to_owned()));
        assert_eq!(tag.variant.clone().unwrap().major, Some(6));
        assert_eq!(tag.variant.clone().unwrap().minor, Some(3));
        assert_eq!(tag.variant.clone().unwrap().patch, Some(23));
        assert_eq!(tag.variant.clone().unwrap().affixes, ["-alpine", ".",]);
        assert_eq!(tag.to_string(), expected);

        let expected = "1.29.3-alpine3.22-slim";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.major, Some(1));
        assert_eq!(tag.minor, Some(29));
        assert_eq!(tag.patch, Some(3));
        assert_eq!(tag.variant.clone().unwrap().prefix, Some("-alpine".to_owned()));
        assert_eq!(tag.variant.clone().unwrap().major, Some(3));
        assert_eq!(tag.variant.clone().unwrap().minor, Some(22));
        assert_eq!(tag.variant.clone().unwrap().suffix, Some("-slim".to_owned()));
        assert_eq!(tag.to_string(), expected);

        let expected = "24.6.0-trixie-slim";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.major, Some(24));
        assert_eq!(tag.minor, Some(6));
        assert_eq!(tag.patch, Some(0));
        assert_eq!(tag.variant.clone().unwrap().prefix, Some("-trixie-slim".to_owned()));
        assert_eq!(tag.variant.clone().unwrap().major, None);
        assert_eq!(tag.to_string(), expected);

        let expected = "13.1-slim";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.major, Some(13));
        assert_eq!(tag.minor, Some(1));
        assert_eq!(tag.patch, None);
        assert_eq!(tag.variant.clone().unwrap().prefix, Some("-slim".to_owned()));
        assert_eq!(tag.to_string(), expected);

        let expected = "1.5.1-11_base";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.major, Some(1));
        assert_eq!(tag.minor, Some(5));
        assert_eq!(tag.patch, Some(1));
        assert_eq!(tag.variant.clone().unwrap().prefix, Some("-".to_owned()));
        assert_eq!(tag.variant.clone().unwrap().major, Some(11));
        assert_eq!(tag.variant.clone().unwrap().minor, None);
        assert_eq!(tag.variant.clone().unwrap().suffix, Some("_base".to_owned()));
        assert_eq!(tag.to_string(), expected);

        let tag: Tag = "24".parse().unwrap();
        assert_eq!(tag.major, Some(24));
        assert_eq!(tag.minor, None);
        assert_eq!(tag.variant, None);

        let expected = "24.0.0-alpine3.22";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.major, Some(24));
        assert_eq!(tag.minor, Some(0));
        assert_eq!(tag.patch, Some(0));
        assert_eq!(
            tag.variant,
            Some(TagVariant {
                prefix:  Some("-alpine".to_owned()),
                major:   Some(3),
                minor:   Some(22),
                patch:   None,
                affixes: vec![],
                suffix:  None,
            })
        );
        assert_eq!(tag.to_string(), expected);

        let expected = "24.0-alpine3.21.1";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.major, Some(24));
        assert_eq!(tag.minor, Some(0));
        assert_eq!(tag.patch, None);
        assert_eq!(
            tag.variant,
            Some(TagVariant {
                prefix:  Some("-alpine".to_owned()),
                major:   Some(3),
                minor:   Some(21),
                patch:   Some(1),
                affixes: vec![],
                suffix:  None,
            })
        );
        assert_eq!(tag.to_string(), expected);

        let expected = "";
        let tag: Tag = expected.parse().unwrap();
        let empty_tag = Tag::default();
        assert_eq!(empty_tag, tag);
        assert_eq!(empty_tag.to_string(), expected);

        let expected = "9.1.1-debian-13-r8";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.variant.clone().unwrap().prefix, Some("-debian-".to_owned()));
        assert_eq!(tag.variant.unwrap().major, Some(13));

        let expected = "10.0.1-azurelinux3.0-amd64";
        let tag: Tag = expected.parse().unwrap();
        assert_eq!(tag.major, Some(10));
        assert_eq!(tag.minor, Some(0));
        assert_eq!(tag.patch, Some(1));
        assert_eq!(tag.variant.clone().unwrap().prefix, Some("-azurelinux".to_owned()));
        assert_eq!(tag.variant.clone().unwrap().major, Some(3));
        assert_eq!(tag.variant.clone().unwrap().minor, Some(0));
        assert_eq!(tag.variant.clone().unwrap().affixes.get(1), Some("-amd".to_owned()).as_ref());
        assert_eq!(tag.variant.clone().unwrap().patch, Some(64));
        assert_eq!(tag.to_string(), expected);
    }

    #[test]
    fn comparing() {
        let current: Tag = "1.29.3-alpine3.22-slim".parse().unwrap();
        let next: Tag = "1.29.3-alpine3.22".parse().unwrap();
        assert!(current.is_same_major(&next));
        assert!(current.is_same_minor(&next));
        assert!(!current.is_same_variant(&next));

        let current: Tag = "1.29.3-alpine3.22-slim".parse().unwrap();
        let next: Tag = "1.29.3-alpine3.23".parse().unwrap();
        assert!(!current.is_next_major(&next));
        assert!(!current.is_next_minor(&next));
        assert!(current.is_next_patch(&next));

        let current: Tag = "1.29.3-alpine3.22-slim".parse().unwrap();
        let next: Tag = "1.29.3-alpine4.1".parse().unwrap();
        assert!(!current.is_next_major(&next));
        assert!(!current.is_next_minor(&next));
        assert!(current.is_next_patch(&next));

        let current: Tag = "0.28.2-alpine3.22-slim".parse().unwrap();
        let next: Tag = "1.29.3-alpine3.22".parse().unwrap();
        assert!(current.is_next_major(&next));
        assert!(!current.is_next_minor(&next));

        let current: Tag = "1.5.1-11_base".parse().unwrap();
        let next: Tag = "1.5.1-14_base".parse().unwrap();
        assert!(current.is_same_variant(&next));

        let current: Tag = "1.5.1-bookworm-11_base".parse().unwrap();
        let next: Tag = "1.5.1-bookworm-14_base".parse().unwrap();
        assert!(current.is_same_variant(&next));

        let current: Tag = "24.12.0-bookworm-slim".parse().unwrap();
        let next: Tag = "24.12.0-trixie-slim".parse().unwrap();
        assert!(!current.is_same_variant(&next));

        let current: Tag = "1.29.3-alpine3.22.1".parse().unwrap();
        let next: Tag = "1.29.3-alpine4.0.0".parse().unwrap();
        assert!(current.is_next_patch(&next));

        let current: Tag = "1.29.3-alpine3.22.1".parse().unwrap();
        let next: Tag = "1.29.3-alpine3.23.0".parse().unwrap();
        assert!(current.is_next_patch(&next));

        let current: Tag = "1.29.3-alpine3.22.1".parse().unwrap();
        let next: Tag = "1.29.3-alpine3.22.2".parse().unwrap();
        assert!(current.is_next_patch(&next));
    }

    #[test]
    fn next_patch() {
        let cases = [
            ("2.5.0", "2.5.01", true),
            ("2.5.0", "2.5.0", false),
            ("2.6.9-bookworm-slim", "2.6.10-bookworm-slim", true),
            ("9.0.1-debian-12-r8", "9.0.1-debian-12-r9", true),
            ("9.0.1-debian-12-r8", "9.0.1-debian-13-r8", true),
            ("1.5.1-11_base", "1.5", false),
            ("1.5.1-11_base", "1.5.1-10_base", false),
            ("9.0.11-alpine3.22", "9.0.12-alpine3.23", true),
            ("2.6.8-debian-12-r1", "2.6.2-debian-11-r2", false),
        ];

        for (current, next, expect) in &cases {
            let c = current.parse::<Tag>().expect("left tag valid");
            let n = next.parse::<Tag>().expect("right tag valid");
            let got = c.is_next_patch(&n);
            assert_eq!(got, *expect, "is_next_minor({}, {}) → expected {}, got {}", current, next, expect, got);
        }
    }

    #[test]
    fn next_minor() {
        let cases = [
            ("2.5.0", "2.6.0", true),
            ("2.5.7", "2.6.9", true),
            ("2.6.9-bookworm-slim", "2.7.0-bookworm-slim", true),
            ("9.0.1-debian-12-r8", "9.1.0-debian-12-r9", true),
            ("9.0.1-debian-12-r8", "9.1.0-debian-13-r8", true),
            ("9.0-debian-12-r8", "9.1-debian-13-r8", true),
            ("1.4.9-11_base", "1.5.1-14_base", true),
            ("2.6.9", "2.6.10", false),
            ("2.6.9", "3.6.10", false),
            ("2.6.9-bookworm-slim", "3.6.10-bookworm-slim", false),
            ("2.6.9-bookworm-slim", "2.6.8-bookworm-slim", false),
            ("2.6.9-bookworm-slim", "2.6.10-bookwork-slim", false),
            ("1.5.1-11_base", "1.5", false),
        ];

        for (current, next, expect) in &cases {
            let c = current.parse::<Tag>().expect("left tag valid");
            let n = next.parse::<Tag>().expect("right tag valid");
            let got = c.is_next_minor(&n);
            assert_eq!(got, *expect, "is_next_minor({}, {}) → expected {}, got {}", current, next, expect, got);
        }
    }

    #[test]
    fn next_major() {
        let cases = [
            ("2.5.7", "3.0.0", true),
            ("2.6.9-bookworm-slim", "3.6.10-bookworm-slim", true),
            ("8.0.1-debian-12-r8", "9.0.1-debian-12-r8", true),
            ("2.6.9", "2.7.9", false),
        ];

        for (current, next, expect) in &cases {
            let c = current.parse::<Tag>().expect("left tag valid");
            let n = next.parse::<Tag>().expect("right tag valid");
            let got = c.is_next_major(&n);
            assert_eq!(got, *expect, "is_next_major({}, {}) → expected {}, got {}", current, next, expect, got);
        }
    }
}
