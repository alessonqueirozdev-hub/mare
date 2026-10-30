//! The names of a hub's objects.
//!
//! On Windows names live in the session's `Local\` namespace, so a hub is
//! reachable only from the same logon session, and objects are created with
//! an access list that allows their creator's user and nobody else.
//! Elsewhere they are POSIX shared memory names, created with mode `0600`
//! (the user alone). Names are kept under 31 bytes, macOS's limit.

use std::fmt;

/// The longest hub name, in bytes.
pub const MAX_HUB_NAME: usize = 10;

#[cfg(windows)]
const PREFIX: &str = "Local\\mare.";
#[cfg(not(windows))]
const PREFIX: &str = "/mare.";

/// A hub's name: 1 to [`MAX_HUB_NAME`] lower-case ASCII letters, digits and
/// hyphens.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HubName(String);

/// A hub name that breaks the rules of [`HubName`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadHubName;

impl fmt::Display for BadHubName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a hub name is 1 to {MAX_HUB_NAME} of a-z, 0-9 and '-'")
    }
}

impl std::error::Error for BadHubName {}

impl HubName {
    /// Checks `name` against the rules.
    pub fn new(name: &str) -> Result<Self, BadHubName> {
        let ok = !name.is_empty()
            && name.len() <= MAX_HUB_NAME
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if ok {
            Ok(Self(name.to_owned()))
        } else {
            Err(BadHubName)
        }
    }

    /// The name of the default hub of a user's session.
    pub fn default_hub() -> Self {
        Self("main".to_owned())
    }

    /// The name itself.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The registry segment's name.
    pub fn registry(&self) -> String {
        format!("{PREFIX}{}", self.0)
    }

    /// The name of the hub's wake object (Windows only).
    pub fn hub_wake(&self) -> String {
        format!("{PREFIX}{}.bell", self.0)
    }

    /// The name of the segment of the client in `slot`, at `generation`.
    pub fn segment(&self, slot: u32, generation: u32) -> String {
        format!("{PREFIX}{}.{slot:x}.{generation:x}", self.0)
    }

    /// The name of that client's wake object (Windows only).
    pub fn client_wake(&self, slot: u32, generation: u32) -> String {
        format!("{}.go", self.segment(slot, generation))
    }
}

impl fmt::Display for HubName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_rules() {
        assert!(HubName::new("main").is_ok());
        assert!(HubName::new("t-0123456").is_ok());
        for bad in ["", "Main", "a.b", "a/b", "abcdefghijk", "ação"] {
            assert_eq!(HubName::new(bad), Err(BadHubName), "{bad:?}");
        }
    }

    #[test]
    fn the_longest_names_fit_macos() {
        let hub = HubName::new("abcdefghij").unwrap();
        let longest = hub.segment(1023, u32::MAX);
        if cfg!(not(windows)) {
            assert!(longest.len() <= 31, "{longest} is {} bytes", longest.len());
        }
        assert!(longest.ends_with("abcdefghij.3ff.ffffffff"));
    }
}
