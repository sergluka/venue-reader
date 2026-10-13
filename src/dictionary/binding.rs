//! Transport and application identity remain independent of dictionary enum labels.

use super::dict::Dict;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceRole {
    Transport,
    Application,
}

/// Application release and profile identifiers used for exact dictionary matching.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApplicationIdentity {
    /// Canonical FIX release for ApplVerID(1128), such as `FIX.5.0SP2`.
    pub version: String,
    /// ApplExtID(1156); `None` is distinct from an explicit zero.
    pub extension_pack: Option<u32>,
    /// Agreed CstmApplVerID(1129) profile identifier, not a registration name.
    pub custom_version: Option<String>,
}

impl ApplicationIdentity {
    /// A release with no extension pack and no custom profile.
    pub fn release(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            extension_pack: None,
            custom_version: None,
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        Some(Self::release(from_wire(value)?))
    }

    pub fn key(&self) -> ApplicationKey<'_> {
        ApplicationKey {
            version: &self.version,
            extension_pack: self.extension_pack,
            custom_version: self.custom_version.as_deref(),
        }
    }
}

/// An [`ApplicationIdentity`] that borrows its strings, so that a lookup for
/// each line allocates nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ApplicationKey<'a> {
    pub version: &'a str,
    pub extension_pack: Option<u32>,
    pub custom_version: Option<&'a str>,
}

impl<'a> From<&'a ApplicationIdentity> for ApplicationKey<'a> {
    fn from(identity: &'a ApplicationIdentity) -> Self {
        identity.key()
    }
}

/// Dictionary owners retained by a line; both roles may share one physical source.
#[derive(Clone)]
pub struct DictionarySources {
    /// Header, trailer and administrative definitions; absent for unresolved transport.
    pub transport: Option<Dict>,
    /// Body definitions; absent for administrative or unresolved application messages.
    pub application: Option<Dict>,
}

impl DictionarySources {
    pub fn source(&self, role: SourceRole) -> Option<&Dict> {
        match role {
            SourceRole::Transport => self.transport.as_ref(),
            SourceRole::Application => self.application.as_ref(),
        }
    }
}

pub const FIXT_1_1: &str = "FIXT.1.1";

// Source: public FIX application-version mapping archived in docs/fix-version-model/.
/// ApplVerID(1128) wire codes and their canonical releases.
const APPLICATION_VERSIONS: [(&str, &str); 11] = [
    ("0", "FIX.2.7"),
    ("1", "FIX.3.0"),
    ("2", "FIX.4.0"),
    ("3", "FIX.4.1"),
    ("4", "FIX.4.2"),
    ("5", "FIX.4.3"),
    ("6", "FIX.4.4"),
    ("7", "FIX.5.0"),
    ("8", "FIX.5.0SP1"),
    ("9", "FIX.5.0SP2"),
    ("10", "FIX.Latest"),
];

pub fn from_wire(code: &str) -> Option<&'static str> {
    APPLICATION_VERSIONS
        .iter()
        .find(|(wire, _)| *wire == code)
        .map(|(_, version)| *version)
}

pub fn to_wire(version: &str) -> Option<&'static str> {
    APPLICATION_VERSIONS
        .iter()
        .find(|(_, release)| *release == version)
        .map(|(wire, _)| *wire)
}

/// True for a release that only FIXT 1.1 carries: FIX 5 and FIX Latest.
pub fn requires_fixt(version: &str) -> bool {
    version.starts_with("FIX.5.") || version == "FIX.Latest"
}

/// The BeginString(8) that carries an application release.
pub fn transport_begin_string(version: &str) -> &str {
    if requires_fixt(version) {
        FIXT_1_1
    } else {
        version
    }
}

/// True for a BeginString(8) that the server decodes: FIXT 1.1 and the
/// releases before FIX 5.
pub fn is_begin_string(value: &str) -> bool {
    value == FIXT_1_1 || (!requires_fixt(value) && to_wire(value).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_mapping_is_exact_and_round_trips() {
        for (wire, version) in APPLICATION_VERSIONS {
            assert_eq!(to_wire(version), Some(wire));
            assert_eq!(from_wire(wire), Some(version));
        }
        for invalid in ["", "09", "11", "-1", "9 ", "FIX.5.0SP2"] {
            assert_eq!(from_wire(invalid), None);
        }
        assert_eq!(to_wire("FIXT.1.1"), None);
        assert_eq!(to_wire("9"), None);
    }

    #[test]
    fn begin_strings_are_fixt_and_the_releases_before_fix5() {
        let accepted: Vec<_> = APPLICATION_VERSIONS
            .iter()
            .map(|(_, version)| *version)
            .chain([FIXT_1_1, "FIX.4.5", "FIX.5.0SP3", ""])
            .filter(|value| is_begin_string(value))
            .collect();
        assert_eq!(
            accepted,
            [
                "FIX.2.7", "FIX.3.0", "FIX.4.0", "FIX.4.1", "FIX.4.2", "FIX.4.3", "FIX.4.4",
                FIXT_1_1
            ]
        );
        assert_eq!(transport_begin_string("FIX.5.0SP1"), FIXT_1_1);
        assert_eq!(transport_begin_string("FIX.Latest"), FIXT_1_1);
        assert_eq!(transport_begin_string("FIX.4.2"), "FIX.4.2");
    }
}
