//! Configuration and raw selectors resolve sources before entering the codec.

use crate::codec::{
    decode, decode_non_message, tokenize, LineAnalysis, LineKind, MessageAnalysis, Resolution,
    ResolutionIssue, Selector,
};
use crate::config::Config;
use crate::dictionary::{
    binding, ApplicationIdentity, ApplicationKey, Dict, DictionaryChoice, DictionarySources,
    Registry, SourceRole, Version,
};
use crate::tag;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::sync::{Arc, LazyLock};
use xxhash_rust::xxh3::Xxh3;

/// Immutable dictionary selection and defaults used to resolve each line.
pub struct Ctx {
    fingerprint: u64,
    registry: Arc<Registry>,
    choice: Option<DictionaryChoice>,
    default_application: Option<ApplicationIdentity>,
}

struct VersionInfo {
    name: Arc<str>,
    application: ApplicationIdentity,
}

/// Version labels and base identities. Every context shares them, so a line
/// with a known version allocates no label.
static VERSIONS: LazyLock<HashMap<&'static str, VersionInfo>> = LazyLock::new(|| {
    Version::ALL
        .into_iter()
        .map(Version::as_str)
        .chain(Version::KNOWN_UNBUNDLED)
        .chain(["FIX.Latest", ""])
        .map(|version| {
            (
                version,
                VersionInfo {
                    name: Arc::from(version),
                    application: ApplicationIdentity::release(version),
                },
            )
        })
        .collect()
});

impl Ctx {
    pub fn new(config: &Config, registry: Arc<Registry>, doc_dictionary: Option<&str>) -> Arc<Ctx> {
        let choice = doc_dictionary.and_then(|name| registry.choice(name));
        Self::with_choice(config, registry, choice)
    }

    pub fn fingerprint_for(
        config: &Config,
        registry: &Registry,
        choice: Option<&DictionaryChoice>,
    ) -> u64 {
        let mut hasher = Xxh3::new();
        if let Some(default) = &config.default_appl_ver_id {
            hasher.update(&[1]);
            hasher.update(default.as_bytes());
        } else {
            hasher.update(&[0]);
        }
        hasher.update(&registry.identity().to_le_bytes());
        if let Some(choice) = choice {
            hasher.update(&[1]);
            hasher.update(&choice.identity.to_le_bytes());
        } else {
            hasher.update(&[0]);
        }
        hasher.digest()
    }

    pub fn with_choice(
        config: &Config,
        registry: Arc<Registry>,
        choice: Option<DictionaryChoice>,
    ) -> Arc<Ctx> {
        Arc::new(Ctx {
            fingerprint: Self::fingerprint_for(config, &registry, choice.as_ref()),
            registry,
            choice,
            default_application: config
                .default_appl_ver_id
                .as_ref()
                .map(|version| ApplicationIdentity::release(version.clone())),
        })
    }
    fn version(&self, version: &str) -> Arc<str> {
        VERSIONS
            .get(version)
            .map(|version| version.name.clone())
            .unwrap_or_else(|| Arc::from(version))
    }
    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
    pub fn selected_version(&self) -> Option<&str> {
        self.choice.as_ref().map(DictionaryChoice::version)
    }
    pub fn choice(&self) -> Option<&DictionaryChoice> {
        self.choice.as_ref()
    }

    #[cfg(test)]
    pub fn for_test(default_version: &str) -> Arc<Ctx> {
        let config = Config {
            default_appl_ver_id: if binding::requires_fixt(default_version) {
                Some(default_version.to_owned())
            } else {
                None
            },
            ..Config::default()
        };
        Ctx::new(&config, Arc::new(Registry::bundled_only()), None)
    }
    #[cfg(test)]
    pub fn for_test_selected(version: &str) -> Arc<Ctx> {
        use crate::dictionary::parse::fixtures::{field, quickfix};
        Self::for_test_with_dict(version, quickfix(version, &field(9999, "Selected")))
    }
    #[cfg(test)]
    pub fn for_test_with_dict(version: &str, dict: crate::dictionary::ParsedDict) -> Arc<Ctx> {
        let dict = Dict::Owned(Arc::new(dict));
        let transport = if binding::requires_fixt(version) {
            Dict::Static(crate::dictionary::bundled(Version::Fixt11))
        } else {
            dict.clone()
        };
        let choice = DictionaryChoice {
            transport_source: transport.version().unwrap_or_default().into(),
            transport,
            application: Some(ApplicationIdentity::release(version)),
            dict,
            source: "test".into(),
            identity: 123,
            name: Some("selected".into()),
        };
        Self::with_choice(
            &Config::default(),
            Arc::new(Registry::bundled_only()),
            Some(choice),
        )
    }
}

/// The names of the dictionaries that decoded one line, one for each source role.
/// `None` for bundled tables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Labels<'a> {
    pub transport: Option<&'a str>,
    pub application: Option<&'a str>,
}

impl<'a> Labels<'a> {
    /// The name of the dictionary that `owner` names, if any.
    pub fn of(self, owner: Option<SourceRole>) -> Option<&'a str> {
        match owner? {
            SourceRole::Transport => self.transport,
            SourceRole::Application => self.application,
        }
    }
}

/// Resolves custom dictionary labels from the bindings used to decode a line.
pub struct DictionaryNames {
    /// Built once for each configuration, so that a line does not scan the definitions.
    by_identity: HashMap<u64, Arc<str>>,
}
impl DictionaryNames {
    pub fn new(registry: &Registry, choice: Option<&DictionaryChoice>) -> Self {
        let reachable: HashSet<_> = registry.default_binding_identities().collect();
        let mut by_identity: HashMap<u64, Arc<str>> = registry
            .default_names()
            .iter()
            .map(|(identity, name)| (*identity, Arc::from(name.as_str())))
            .collect();
        for definition in registry.definitions() {
            if reachable.contains(&definition.choice.identity) {
                by_identity
                    .entry(definition.choice.identity)
                    .or_insert_with(|| Arc::from(definition.spec.name.as_str()));
            }
        }
        if let Some(choice) = choice {
            if let Some(label) = choice_label(choice) {
                by_identity.insert(choice.identity, label);
            }
        }
        Self { by_identity }
    }
    pub fn of_analysis(&self, message: &MessageAnalysis) -> Labels<'_> {
        let name =
            |binding: Option<NonZeroU64>| self.by_identity.get(&binding?.get()).map(|name| &**name);
        Labels {
            transport: name(message.resolution.transport_binding),
            application: name(message.resolution.application_binding),
        }
    }
    pub fn same_as(&self, other: &Self) -> bool {
        self.by_identity == other.by_identity
    }
}
fn choice_label(choice: &DictionaryChoice) -> Option<Arc<str>> {
    if choice.dict.is_static() && choice.transport.is_static() {
        return None;
    }
    if let Some(name) = &choice.name {
        return Some(Arc::from(name.as_str()));
    }
    std::path::Path::new(&choice.source)
        .file_name()?
        .to_str()
        .map(Arc::from)
}

struct Selectors<'a> {
    values: [Result<Option<&'a str>, ResolutionIssue>; tag::IDENTITY_SELECTORS.len()],
}

impl<'a> Selectors<'a> {
    fn new(bytes: &'a [u8], tokens: &crate::codec::LineTokens) -> Self {
        let mut values = [Ok(None); tag::IDENTITY_SELECTORS.len()];
        for field in &tokens.fields {
            let Some(selector) = Selector::of(field.tag) else {
                continue;
            };
            let index = selector.index();
            if values[index] != Ok(None) {
                values[index] = Err(ResolutionIssue::DuplicateSelector(selector));
                continue;
            }
            values[index] =
                std::str::from_utf8(&bytes[field.value_start.index()..field.value_end.index()])
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(Some)
                    .ok_or(ResolutionIssue::InvalidSelector(selector));
        }
        Self { values }
    }

    fn get(&self, selector: Selector) -> Result<Option<&'a str>, ResolutionIssue> {
        self.values[selector.index()]
    }
}

fn effective_identity<'a>(
    defaults: Option<&'a ApplicationIdentity>,
    version: Option<&'a str>,
    extension: Option<u32>,
    custom: Option<&'a str>,
) -> Option<ApplicationKey<'a>> {
    let version = version.or_else(|| defaults.map(|identity| identity.version.as_str()))?;
    let defaults = defaults.filter(|identity| identity.version == version);
    Some(ApplicationKey {
        version,
        extension_pack: extension.or_else(|| defaults.and_then(|identity| identity.extension_pack)),
        custom_version: custom
            .or_else(|| defaults.and_then(|identity| identity.custom_version.as_deref())),
    })
}

pub fn analyze(bytes: &[u8], ctx: &Ctx) -> LineAnalysis {
    let tokens = tokenize(bytes);
    if tokens.kind != LineKind::Message {
        return decode_non_message(tokens);
    }
    let selectors = Selectors::new(bytes, &tokens);
    let (Ok(resolved) | Err(resolved)) = resolve(ctx, &selectors);
    let msg_type = selectors
        .get(Selector::MSG_TYPE)
        .ok()
        .flatten()
        .and_then(|value| decode::message_ref(&resolved.sources, value));
    decode::decode_resolved(
        bytes,
        tokens,
        resolved.sources,
        resolved.version,
        resolved.resolution,
        msg_type,
    )
}

/// The dictionary sources of one line, its version label and the facts of the selection.
struct Resolved {
    sources: DictionarySources,
    version: Arc<str>,
    resolution: Resolution,
}

impl Resolved {
    fn unresolved(ctx: &Ctx, issue: ResolutionIssue, version: &str) -> Self {
        Self {
            sources: DictionarySources {
                transport: None,
                application: None,
            },
            version: ctx.version(version),
            resolution: Resolution {
                issue: Some(issue),
                ..Resolution::default()
            },
        }
    }
}

/// `Err` is a line without dictionary sources.
fn resolve(ctx: &Ctx, selectors: &Selectors) -> Result<Resolved, Resolved> {
    let begin = match selectors.get(Selector::BEGIN_STRING) {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Err(Resolved::unresolved(
                ctx,
                ResolutionIssue::MissingBeginString,
                "",
            ))
        }
        Err(issue) => return Err(Resolved::unresolved(ctx, issue, "")),
    };
    let unresolved = |issue| Resolved::unresolved(ctx, issue, begin);
    if !binding::is_begin_string(begin) {
        return Err(unresolved(ResolutionIssue::UnsupportedBeginString));
    }
    let message = selectors.get(Selector::MSG_TYPE).map_err(unresolved)?;
    let selected = ctx
        .choice
        .as_ref()
        .filter(|choice| choice.transport.version() == Some(begin));
    let default_transport = ctx.registry.default_transport(begin);
    let transport = selected
        .or(default_transport)
        .map(|choice| choice.transport.clone())
        .ok_or_else(|| unresolved(ResolutionIssue::UnsupportedBeginString))?;
    let admin = message.is_some_and(|value| decode::session_message(&transport, value).is_some());
    if admin {
        let issue = [
            Selector::APPL_VER_ID,
            Selector::APPL_EXT_ID,
            Selector::CSTM_APPL_VER_ID,
        ]
        .into_iter()
        .find(|selector| selectors.get(*selector) != Ok(None))
        .map(ResolutionIssue::ProhibitedSelector);
        return Ok(Resolved {
            sources: DictionarySources {
                transport: Some(transport),
                application: None,
            },
            version: ctx.version(begin),
            resolution: Resolution {
                issue,
                selected_mismatch: ctx.choice.is_some() && selected.is_none(),
                transport_binding: selected
                    .or(default_transport)
                    .and_then(|choice| NonZeroU64::new(choice.identity)),
                application_binding: None,
            },
        });
    }
    let application_version = selectors.get(Selector::APPL_VER_ID).map_err(unresolved)?;
    let extension = selectors.get(Selector::APPL_EXT_ID).map_err(unresolved)?;
    let custom = selectors
        .get(Selector::CSTM_APPL_VER_ID)
        .map_err(unresolved)?;
    let invalid = |selector| unresolved(ResolutionIssue::InvalidSelector(selector));
    let explicit_version = application_version
        .map(|value| binding::from_wire(value).ok_or_else(|| invalid(Selector::APPL_VER_ID)))
        .transpose()?;
    let explicit_extension = extension
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| invalid(Selector::APPL_EXT_ID))
        })
        .transpose()?;
    let base = if begin == binding::FIXT_1_1 {
        ctx.default_application.as_ref()
    } else {
        VERSIONS.get(begin).map(|version| &version.application)
    };
    let mut identity = effective_identity(
        selected
            .and_then(|choice| choice.application.as_ref())
            .or(base),
        explicit_version,
        explicit_extension,
        custom,
    );
    let mut mismatch = ctx.choice.is_some() && selected.is_none();
    let chosen = if let Some(choice) = selected.filter(|choice| choice.application.is_some()) {
        if choice.application.as_ref().map(ApplicationIdentity::key) == identity {
            Some(choice)
        } else {
            mismatch = true;
            identity = effective_identity(base, explicit_version, explicit_extension, custom);
            None
        }
    } else {
        None
    };
    // A conflicting selection gives up its transport with its application.
    let without_application = |transport: Dict, issue, version: &str| Resolved {
        sources: DictionarySources {
            transport: if mismatch {
                default_transport.map(|choice| choice.transport.clone())
            } else {
                Some(transport)
            },
            application: None,
        },
        version: ctx.version(version),
        resolution: Resolution {
            issue: Some(issue),
            selected_mismatch: mismatch,
            transport_binding: None,
            application_binding: None,
        },
    };
    let Some(identity) = identity else {
        return Ok(without_application(
            transport,
            ResolutionIssue::MissingApplication,
            begin,
        ));
    };
    let choice = chosen.or_else(|| {
        (begin == binding::FIXT_1_1 || identity.version == begin)
            .then(|| ctx.registry.default_pair(begin, identity))
            .flatten()
    });
    let Some(choice) = choice else {
        return Ok(without_application(
            transport,
            ResolutionIssue::UnregisteredApplication,
            identity.version,
        ));
    };
    let mut sources = choice.sources();
    let family = selected.filter(|selected| selected.application.is_none());
    if let Some(family) = family {
        sources.transport = Some(family.transport.clone());
    }
    Ok(Resolved {
        sources,
        version: ctx.version(identity.version),
        resolution: Resolution {
            issue: None,
            selected_mismatch: mismatch,
            transport_binding: NonZeroU64::new(family.unwrap_or(choice).identity),
            application_binding: NonZeroU64::new(choice.identity),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(line: &str, context: &Ctx) -> MessageAnalysis {
        match analyze(line.as_bytes(), context) {
            LineAnalysis::Message(message) => message,
            _ => panic!("message"),
        }
    }

    #[test]
    fn every_bundled_application_resolves_over_fixt() {
        let context = Ctx::for_test("FIX.4.4");
        for (code, version) in [
            ("2", "FIX.4.0"),
            ("3", "FIX.4.1"),
            ("4", "FIX.4.2"),
            ("5", "FIX.4.3"),
            ("6", "FIX.4.4"),
            ("7", "FIX.5.0"),
            ("8", "FIX.5.0SP1"),
            ("9", "FIX.5.0SP2"),
        ] {
            let decoded = message(
                &format!("8=FIXT.1.1|35=D|1128={code}|55=TEST|54=1"),
                &context,
            );
            assert_eq!(decoded.version.as_ref(), version);
            assert_eq!(decoded.resolution.issue, None);
            assert_eq!(
                decoded.sources.transport.as_ref().unwrap().version(),
                Some("FIXT.1.1")
            );
            assert_eq!(
                decoded.sources.application.as_ref().unwrap().version(),
                Some(version)
            );
            assert_eq!(
                decoded.first(8).unwrap().owner(),
                Some(SourceRole::Transport)
            );
            assert_eq!(
                decoded.first(55).unwrap().owner(),
                Some(SourceRole::Application)
            );
        }
    }

    #[test]
    fn missing_or_unsupported_begin_string_never_uses_selection() {
        let context = Ctx::for_test_selected("FIX.4.4");
        for line in [
            "35=D|9999=X",
            "8=FIX.5.0SP2|35=D|9999=X",
            "8=FIX.9.1|35=D|9999=X",
        ] {
            let decoded = message(line, &context);
            assert!(!decoded.is_bound());
            assert!(decoded
                .fields
                .iter()
                .all(|field| field.def().is_none() && field.unresolved()));
            assert!(decoded.groups.is_empty());
        }
    }

    #[test]
    fn explicit_default_and_selected_pair_supply_missing_application() {
        let line = "8=FIXT.1.1|35=D|55=TEST";
        let missing = message(line, &Ctx::for_test("FIX.4.4"));
        assert_eq!(
            missing.resolution.issue,
            Some(ResolutionIssue::MissingApplication)
        );
        assert!(missing.first(55).unwrap().unresolved());
        let explicit = message(line, &Ctx::for_test("FIX.5.0SP1"));
        assert_eq!(explicit.version.as_ref(), "FIX.5.0SP1");
        let selected = Ctx::for_test_selected("FIX.5.0SP2");
        let decoded = message("8=FIXT.1.1|35=D|9999=X", &selected);
        assert_eq!(decoded.version.as_ref(), "FIX.5.0SP2");
        assert_eq!(
            decoded.field_name(decoded.first(9999).unwrap()),
            Some("Selected")
        );
    }

    #[test]
    fn unknown_explicit_identity_never_uses_selected_default() {
        let context = Ctx::for_test_selected("FIX.5.0SP2");
        for (line, issue) in [
            (
                "8=FIXT.1.1|35=D|1128=11|9999=X",
                ResolutionIssue::InvalidSelector(Selector::APPL_VER_ID),
            ),
            (
                "8=FIXT.1.1|35=D|1128=9|1156=12345|9999=X",
                ResolutionIssue::UnregisteredApplication,
            ),
            (
                "8=FIXT.1.1|35=D|1128=9|1129=unregistered|9999=X",
                ResolutionIssue::UnregisteredApplication,
            ),
            (
                "8=FIXT.1.1|35=D|1128=9|1156=0|9999=X",
                ResolutionIssue::UnregisteredApplication,
            ),
        ] {
            let decoded = message(line, &context);
            assert_eq!(decoded.resolution.issue, Some(issue));
            assert!(decoded.first(9999).unwrap().unresolved());
        }
    }

    #[test]
    fn mismatching_complete_pair_discards_both_sources() {
        let context = Ctx::for_test_selected("FIX.5.0SP2");
        let decoded = message("8=FIXT.1.1|35=D|1128=8|55=TEST|9999=X", &context);
        assert!(decoded.resolution.selected_mismatch);
        assert_eq!(decoded.version.as_ref(), "FIX.5.0SP1");
        assert!(decoded.sources.transport.as_ref().unwrap().is_static());
        assert!(decoded.sources.application.as_ref().unwrap().is_static());
        assert_eq!(
            decoded.field_name(decoded.first(55).unwrap()),
            Some("Symbol")
        );
        assert_eq!(decoded.first(9999).unwrap().def(), None);
    }

    #[test]
    fn repeated_selectors_reject_even_identical_values() {
        let context = Ctx::for_test("FIX.4.4");
        for (line, tag) in [
            ("8=FIX.4.4|8=FIX.4.4|35=D", 8),
            ("8=FIX.4.4|35=D|35=D", 35),
            ("8=FIXT.1.1|35=D|1128=9|1128=9", 1128),
            ("8=FIXT.1.1|35=D|1128=9|1156=0|1156=0", 1156),
            ("8=FIXT.1.1|35=D|1128=9|1129=A|1129=A", 1129),
        ] {
            let decoded = message(line, &context);
            assert_eq!(
                decoded.resolution.issue,
                Some(ResolutionIssue::DuplicateSelector(
                    Selector::of(tag).unwrap()
                ))
            );
            assert!(!decoded.is_bound());
        }
    }

    #[test]
    fn admin_resolution_does_not_require_or_change_application_defaults() {
        let context = Ctx::for_test("FIX.4.4");
        let logon = message("8=FIXT.1.1|35=A|1137=9|108=30", &context);
        assert!(logon.message_definition().unwrap().is_session());
        assert!(logon.sources.application.is_none());
        let next = message("8=FIXT.1.1|35=D|55=TEST", &context);
        assert_eq!(
            next.resolution.issue,
            Some(ResolutionIssue::MissingApplication)
        );
        let prohibited = message("8=FIXT.1.1|35=0|1128=9", &context);
        assert!(prohibited.message_definition().unwrap().is_session());
        assert_eq!(
            prohibited.resolution.issue,
            Some(ResolutionIssue::ProhibitedSelector(Selector::APPL_VER_ID))
        );
    }

    #[test]
    fn missing_envelope_fields_do_not_block_available_decoding() {
        let context = Ctx::for_test("FIX.4.4");
        let decoded = message("8=FIX.4.4|55=TEST", &context);
        assert!(decoded.is_bound());
        assert_eq!(
            decoded.field_name(decoded.first(55).unwrap()),
            Some("Symbol")
        );
        assert!(decoded.groups.is_empty());
    }

    #[test]
    fn rendering_and_names_do_not_change_interpretation_fingerprint() {
        let original = Ctx::for_test_selected("FIX.4.4");
        let mut choice = original.choice().unwrap().clone();
        choice.source = "renamed.xml".into();
        choice.name = Some("renamed".into());
        let equivalent = Ctx::with_choice(
            &Config::default(),
            original.registry().clone(),
            Some(choice.clone()),
        );
        assert_eq!(original.fingerprint(), equivalent.fingerprint());
        let old_names = DictionaryNames::new(original.registry(), original.choice());
        let new_names = DictionaryNames::new(equivalent.registry(), equivalent.choice());
        assert!(!old_names.same_as(&new_names));
        choice.identity += 1;
        let changed = Ctx::with_choice(
            &Config::default(),
            original.registry().clone(),
            Some(choice),
        );
        assert_ne!(original.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn application_default_presence_and_release_change_the_fingerprint() {
        let registry = Arc::new(Registry::bundled_only());
        let selected = registry.choice("FIX.5.0SP2").unwrap();
        for choice in [None, Some(selected.clone())] {
            let fingerprints: Vec<_> = [None, Some("FIX.5.0"), Some("FIX.5.0SP1")]
                .into_iter()
                .map(|version| {
                    let config = Config {
                        default_appl_ver_id: version.map(str::to_owned),
                        ..Config::default()
                    };
                    Ctx::with_choice(&config, registry.clone(), choice.clone()).fingerprint()
                })
                .collect();
            assert_ne!(fingerprints[0], fingerprints[1]);
            assert_ne!(fingerprints[0], fingerprints[2]);
            assert_ne!(fingerprints[1], fingerprints[2]);
        }
    }

    #[test]
    fn selected_choice_presence_and_identity_change_the_fingerprint() {
        let config = Config::default();
        let registry = Arc::new(Registry::bundled_only());
        let choice = registry.choice("FIX.5.0SP2").unwrap();
        let unselected = Ctx::with_choice(&config, registry.clone(), None);
        let selected = Ctx::with_choice(&config, registry.clone(), Some(choice.clone()));
        assert_ne!(unselected.fingerprint(), selected.fingerprint());
        let changed = DictionaryChoice {
            identity: choice.identity.wrapping_add(1),
            ..choice
        };
        let changed = Ctx::with_choice(&config, registry, Some(changed));
        assert_ne!(selected.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn registry_identity_changes_the_fingerprint_without_changing_selection() {
        use crate::config::DictionarySpec;
        use crate::dictionary::{BuildContext, DictionaryLayer, DictionaryStore};

        let config = Config::default();
        let original = Arc::new(Registry::bundled_only());
        let specs = [DictionarySpec {
            name: "synthetic-default".into(),
            transport: Some("FIXT.1.1".into()),
            application: Some("FIX.5.0SP2".into()),
            default: true,
            ..DictionarySpec::default()
        }];
        let (changed, events) = DictionaryStore::default()
            .build_registry(
                &[DictionaryLayer {
                    specs: &specs,
                    source: "test",
                    root_dir: None,
                }],
                &BuildContext {
                    root_dir: None,
                    home_dir: None,
                    limits: &config.limits,
                },
            )
            .unwrap();
        assert!(events.is_empty(), "{events:?}");
        assert_ne!(original.identity(), changed.identity());
        let changed = Arc::new(changed);
        for choice in [None, original.choice("FIX.4.4")] {
            let before = Ctx::with_choice(&config, original.clone(), choice.clone());
            let after = Ctx::with_choice(&config, changed.clone(), choice);
            assert_ne!(before.fingerprint(), after.fingerprint());
        }
    }

    #[test]
    fn rendering_settings_are_not_fingerprint_inputs() {
        let registry = Arc::new(Registry::bundled_only());
        let plain = Config::default();
        let mut decorated = plain.clone();
        decorated.message_summary.template = "{35:name}".into();
        decorated.message_summary.channels.code_lens = false;
        decorated.hover.enabled = false;
        decorated.semantic_tokens.enabled = false;
        decorated.document_link.url = "https://example.com/{version}/{tag}".into();
        decorated.document_symbol.template = "{35:name}".into();
        decorated.inlay_hints = serde_json::from_value(serde_json::json!({
            "tagNames": {"enabled": true, "format": "[{}]"},
            "enumValues": {"enabled": true},
            "groupPaths": {"enabled": false}
        }))
        .unwrap();
        for choice in [None, registry.choice("FIX.5.0SP2")] {
            let before = Ctx::with_choice(&plain, registry.clone(), choice.clone());
            let after = Ctx::with_choice(&decorated, registry.clone(), choice);
            assert_eq!(before.fingerprint(), after.fingerprint());
        }
    }

    #[test]
    fn diagnostic_switches_severity_and_options_are_not_fingerprint_inputs() {
        let registry = Arc::new(Registry::bundled_only());
        let plain = Config::default();
        let mut decorated = plain.clone();
        decorated.diagnostics.enabled = false;
        decorated.diagnostics.rules.insert(
            "integrity".into(),
            crate::config::RuleSetting::Enabled(false),
        );
        decorated.diagnostics.rules.insert(
            "unknownVersion".into(),
            serde_json::from_value(serde_json::json!({
                "severity": "hint", "syntheticOption": true
            }))
            .unwrap(),
        );
        for choice in [None, registry.choice("FIX.5.0SP2")] {
            let before = Ctx::with_choice(&plain, registry.clone(), choice.clone());
            let after = Ctx::with_choice(&decorated, registry.clone(), choice);
            assert_eq!(before.fingerprint(), after.fingerprint());
        }
    }

    #[test]
    fn pair_default_precedes_separate_application_default() {
        let selected = Ctx::for_test_selected("FIX.5.0SP2");
        let config = Config {
            default_appl_ver_id: Some("FIX.5.0SP1".into()),
            ..Config::default()
        };
        let context = Ctx::with_choice(
            &config,
            selected.registry().clone(),
            selected.choice().cloned(),
        );
        let decoded = message("8=FIXT.1.1|35=D|9999=X", &context);
        assert_eq!(decoded.version.as_ref(), "FIX.5.0SP2");
        let decoded = message("8=FIXT.1.1|35=D|1128=8|55=TEST", &context);
        assert_eq!(decoded.version.as_ref(), "FIX.5.0SP1");
        assert!(decoded.resolution.selected_mismatch);
    }
}
