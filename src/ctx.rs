//! What `analyze` needs that is not the line: which dictionary answers which
//! BeginString, and the fingerprint that says when a cached result died.
//!
//! Sits above `codec`, which stays config-free, and below `document`, which
//! caches per line.

use std::sync::Arc;

use xxhash_rust::xxh3::Xxh3;

use crate::codec::{decode_non_message, decode_tokenized, tokenize, LineAnalysis, LineKind};
use crate::config::Config;
use crate::dictionary::{Dict, DictionaryChoice, Registry};

/// Version of the decoding behaviour, mixed into the fingerprint: the other
/// inputs cover the data, none covers the algorithm. Bump it when a codec
/// change invalidates cached analyses. Never zero, so a default is visible.
const CODEC_POLICY: u8 = 1;

/// A dictionary bound to the version string that selected it.
#[derive(Clone)]
pub struct Binding {
    /// What the line said. Not `Dict::version`, which names the *tables*:
    /// `8=FIXT.1.1` resolves to the FIX.5.0SP2 ones.
    pub version: Arc<str>,
    pub dict: Dict,
}

pub struct Ctx {
    /// Identity of the *interpretation*: while it holds, every cached
    /// `LineAnalysis` is still correct. See [`Ctx::new`] for what goes into it.
    fingerprint: u64,
    bindings: Vec<Binding>,
    default_binding: Binding,
    registry: Arc<Registry>,
    choice: Option<DictionaryChoice>,
}

impl Ctx {
    /// Resolve a named choice without filesystem work.
    pub fn new(config: &Config, registry: Arc<Registry>, doc_dictionary: Option<&str>) -> Arc<Ctx> {
        let choice = doc_dictionary.and_then(|name| registry.choice(name));
        Self::with_choice(config, registry, choice)
    }

    /// The fingerprint [`Ctx::with_choice`] would produce, without building the
    /// bindings: a caller holding a context can tell whether it still applies
    /// for the price of a hash instead of a rebuild.
    pub fn fingerprint_for(
        config: &Config,
        registry: &Registry,
        choice: Option<&DictionaryChoice>,
    ) -> u64 {
        let mut hasher = Xxh3::new();
        hasher.update(&[CODEC_POLICY]);
        hasher.update(config.default_version.as_bytes());
        hasher.update(&registry.identity().to_le_bytes());
        if let Some(choice) = choice {
            hasher.update(&[1]);
            hasher.update(&choice.identity.to_le_bytes());
            hasher.update(choice.version.as_bytes());
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
        let fingerprint = Self::fingerprint_for(config, &registry, choice.as_ref());

        let mut bindings: Vec<Binding> = registry
            .bindings()
            .into_iter()
            .map(|(version, dict)| Binding {
                version: Arc::from(version),
                dict,
            })
            .collect();

        if let Some(choice) = &choice {
            for binding in &mut bindings {
                if choice.compatible_with(&binding.version) {
                    binding.dict = choice.dict.clone();
                }
            }
            if !bindings
                .iter()
                .any(|binding| binding.version.as_ref() == choice.version)
            {
                bindings.push(Binding {
                    version: Arc::from(choice.version.as_str()),
                    dict: choice.dict.clone(),
                });
            }
        }

        let default_version = registry.default_version();
        let default_binding = bindings
            .iter()
            .find(|b| b.version.as_ref() == default_version)
            .cloned()
            .unwrap_or_else(|| Binding {
                version: Arc::from(default_version),
                dict: registry.default_dict(),
            });

        Arc::new(Ctx {
            fingerprint,
            bindings,
            default_binding,
            registry,
            choice,
        })
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub fn selected_version(&self) -> Option<&str> {
        self.choice.as_ref().map(|choice| choice.version.as_str())
    }

    pub fn choice(&self) -> Option<&DictionaryChoice> {
        self.choice.as_ref()
    }

    /// A linear scan: no hashing, no interior mutability, and no per-line
    /// allocation of a version string. `fix.dictionaries` is unbounded, so a
    /// configuration with hundreds of entries would want a map; the realistic
    /// count is the nine bundled versions plus a handful, and the switch is a
    /// measurement away rather than a guess.
    pub fn resolve(&self, begin_string: Option<&[u8]>) -> &Binding {
        // A BeginString that is absent, not UTF-8, or unknown all mean the same
        // thing here: fall back to the default.
        begin_string
            .and_then(|raw| std::str::from_utf8(raw).ok())
            .and_then(|version| {
                self.bindings
                    .iter()
                    .find(|binding| binding.version.as_ref() == version)
            })
            .unwrap_or(&self.default_binding)
    }

    #[cfg(test)]
    pub fn for_test(default_version: &str) -> Arc<Ctx> {
        let config = Config {
            default_version: default_version.to_owned(),
            ..Config::default()
        };
        Ctx::new(&config, Arc::new(Registry::bundled_only()), None)
    }

    /// Bundled tables everywhere except `version`, which `dict` answers for,
    /// as a selected custom dictionary would.
    #[cfg(test)]
    pub fn for_test_with_dict(version: &str, dict: crate::dictionary::ParsedDict) -> Arc<Ctx> {
        let choice = DictionaryChoice {
            dict: Dict::Owned(Arc::new(dict)),
            version: version.to_owned(),
            source: "test".into(),
            identity: 123,
            name: Some("selected".into()),
        };
        Ctx::with_choice(
            &Config::default(),
            Arc::new(Registry::bundled_only()),
            Some(choice),
        )
    }
}

/// What hover calls the dictionary bound to each version.
///
/// Beside the context rather than inside it, for the reason `render::Options`
/// is: a name invalidates no analysis, and a context is reused whenever its
/// fingerprint holds — which a rename must not change. Built from the registry
/// and choice in force, so a rename is visible at once.
pub struct DictionaryNames {
    /// The selected dictionary's version and name, when one is selected.
    choice: Option<(String, Arc<str>)>,
    registry: Arc<Registry>,
}

impl DictionaryNames {
    pub fn new(registry: Arc<Registry>, choice: Option<&DictionaryChoice>) -> DictionaryNames {
        DictionaryNames {
            choice: choice.and_then(|choice| Some((choice.version.clone(), choice_label(choice)?))),
            registry,
        }
    }

    /// The name for the dictionary that answers `version`, `None` for bundled
    /// tables. Follows `Ctx::with_choice`: a selection wins for the versions it
    /// is compatible with, otherwise the registry's default for that version.
    pub fn of(&self, version: &str) -> Option<&str> {
        if let Some((selected, label)) = &self.choice {
            if crate::dictionary::versions_compatible(selected, version) {
                return Some(label);
            }
        }
        self.registry.default_name(version)
    }
}

/// A choice's configured name, else the last component of its source path —
/// never the full path. A Repository resolves to its `Base/` directory, so the
/// directory holding that is the one named. `None` for a bundled version.
fn choice_label(choice: &DictionaryChoice) -> Option<Arc<str>> {
    if choice.dict.is_static() {
        return None;
    }
    if let Some(name) = &choice.name {
        return Some(Arc::from(name.as_str()));
    }
    let path = std::path::Path::new(&choice.source);
    let path = if path.file_name().is_some_and(|name| name == "Base") {
        path.parent()?
    } else {
        path
    };
    path.file_name()?.to_str().map(Arc::from)
}

/// Decode one line: classify, pick the dictionary from BeginString(8), decode.
///
/// Tokenizes exactly once — `codec::decode` re-tokenizes and exists for unit
/// tests. Callers that walk a whole document use this and drop the result;
/// only viewport-bounded requests may put it in a line's cache.
pub fn analyze(bytes: &[u8], ctx: &Ctx) -> LineAnalysis {
    let tokenized = tokenize(bytes);
    if tokenized.kind != LineKind::Message {
        return decode_non_message(tokenized);
    }
    let begin_string = tokenized
        .fields
        .iter()
        .find(|field| field.tag == 8)
        .map(|field| &bytes[field.value_start.index()..field.value_end.index()]);
    let binding = ctx.resolve(begin_string);
    decode_tokenized(bytes, tokenized, &binding.dict, &binding.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected_context(version: &str) -> Arc<Ctx> {
        use crate::dictionary::parse::fixtures::{field, quickfix};
        Ctx::for_test_with_dict(version, quickfix(version, &field(9999, "Selected")))
    }

    #[test]
    fn a_selected_nondefault_version_is_known_but_does_not_change_missing_tag8_fallback() {
        let context = selected_context("FIX.9.1");
        let selected = context.resolve(Some(b"FIX.9.1"));
        assert_eq!(selected.version.as_ref(), "FIX.9.1");
        assert!(selected.dict.field_idx(9999).is_some());
        assert_eq!(context.resolve(None).version.as_ref(), "FIX.4.4");
        assert!(context.resolve(None).dict.is_static());
        assert!(context.resolve(Some(b"FIX.4.2")).dict.is_static());
        assert_eq!(context.selected_version(), Some("FIX.9.1"));
    }

    #[test]
    fn selected_alias_compatibility_is_asymmetric() {
        let application = selected_context("FIX.5.0SP2");
        assert!(!application.resolve(Some(b"FIXT.1.1")).dict.is_static());
        let transport = selected_context("FIXT.1.1");
        assert!(!transport.resolve(Some(b"FIXT.1.1")).dict.is_static());
        assert!(transport.resolve(Some(b"FIX.5.0SP2")).dict.is_static());
    }

    #[test]
    fn choice_provenance_is_not_a_context_fingerprint_input() {
        let original = selected_context("FIX.4.4");
        let mut choice = original.choice().unwrap().clone();
        choice.source = "another.toml:6".into();
        choice.name = Some("different-name".into());
        let equivalent = Ctx::with_choice(
            &Config::default(),
            original.registry().clone(),
            Some(choice.clone()),
        );
        assert_eq!(original.fingerprint(), equivalent.fingerprint());
        choice.identity += 1;
        let changed = Ctx::with_choice(
            &Config::default(),
            original.registry().clone(),
            Some(choice),
        );
        assert_ne!(original.fingerprint(), changed.fingerprint());
    }

    /// A custom dictionary bound to FIX.5.0SP2 has to answer `8=FIXT.1.1` as
    /// well — that is the BeginString every FIX 5.x session actually sends, and
    /// `Registry::resolve` applies the alias. A `Ctx` that decided versions on
    /// its own filled the gap with the bundled tables and silently ignored the
    /// user's dictionary for every 5.x line.
    #[test]
    fn the_fixt11_alias_reaches_a_custom_five_x_dictionary() {
        let spec = crate::config::DictionarySpec {
            path: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/xml/custom/synthetic/repository/FIX.5.0SP2/Base"
            )
            .to_owned(),
            mode: crate::config::DictionaryMode::Auto,
            name: "md-fix50".to_owned(),
            default: true,
        };
        let limits = crate::config::Limits::default();
        let (registry, events) = Registry::build(
            std::slice::from_ref(&spec),
            "FIX.4.4",
            &crate::dictionary::BuildContext {
                root_dir: None,
                home_dir: None,
                limits: &limits,
            },
            &crate::dictionary::DictionaryStore::default(),
        );
        assert!(events.is_empty(), "{events:?}");

        let ctx = Ctx::new(&Config::default(), Arc::new(registry), None);
        let via_registry = ctx
            .registry()
            .resolve("FIXT.1.1")
            .expect("the registry resolves the alias");
        assert!(
            !via_registry.is_static(),
            "the custom dictionary is what answers the alias"
        );
        assert!(
            ctx.resolve(Some(b"FIXT.1.1")).dict.ptr_eq(&via_registry),
            "and the context agrees with the registry"
        );
    }

    #[test]
    fn resolve_prefers_an_exact_version_then_falls_back() {
        let ctx = Ctx::for_test("FIX.4.4");
        assert_eq!(ctx.resolve(Some(b"FIX.4.2")).version.as_ref(), "FIX.4.2");
        assert_eq!(
            ctx.resolve(Some(b"FIXT.1.1")).version.as_ref(),
            "FIXT.1.1",
            "the alias keeps its own name"
        );
        assert_eq!(
            ctx.resolve(Some(b"FIX.9.9")).version.as_ref(),
            "FIX.4.4",
            "unknown falls back to the default"
        );
        assert_eq!(ctx.resolve(None).version.as_ref(), "FIX.4.4");
        assert_eq!(
            ctx.resolve(Some(b"\xff\xfe")).version.as_ref(),
            "FIX.4.4",
            "so does a non-utf8 BeginString"
        );
    }

    /// The resolved version of a line that is a message.
    fn version_of(bytes: &[u8], ctx: &Ctx) -> Option<Arc<str>> {
        analyze(bytes, ctx)
            .message()
            .map(|message| message.version.clone())
    }

    #[test]
    fn analyze_records_the_resolved_version_not_the_dictionarys_own() {
        let ctx = Ctx::for_test("FIX.4.4");
        let analysis = analyze(b"8=FIXT.1.1|35=0", &ctx);
        let message = analysis.message().expect("a message");
        assert_eq!(message.version.as_ref(), "FIXT.1.1");
        assert_eq!(
            message.dict.version(),
            Some("FIX.5.0SP2"),
            "the tables are SP2's"
        );
    }

    #[test]
    fn analyze_falls_back_for_unknown_and_missing_begin_strings() {
        let ctx = Ctx::for_test("FIX.4.4");
        assert_eq!(
            version_of(b"8=FIX.9.9|35=0", &ctx).as_deref(),
            Some("FIX.4.4")
        );
        assert_eq!(version_of(b"35=0|49=A", &ctx).as_deref(), Some("FIX.4.4"));
        assert_eq!(version_of(b"", &ctx), None);
    }

    #[test]
    fn analyze_tokenizes_once() {
        // Same answer either way; the point is that `analyze` does not pay for
        // the second pass `decode` would.
        let ctx = Ctx::for_test("FIX.4.4");
        let line = b"8=FIX.4.4|35=W|268=1|269=0";
        let once = analyze(line, &ctx);
        let binding = ctx.resolve(Some(b"FIX.4.4"));
        let twice = crate::codec::decode(line, &binding.dict, &binding.version);
        let (once, twice) = (once.message().unwrap(), twice.message().unwrap());
        assert_eq!(once.fields, twice.fields);
        assert_eq!(once.groups, twice.groups);
    }

    /// `Options::from_config` owns the template now, so this states the
    /// remaining fact: a config differing only there is the same interpretation.
    #[test]
    fn the_template_is_not_a_fingerprint_input() {
        let registry = Arc::new(Registry::bundled_only());
        let mut other = Config::default();
        other.message_summary.template = "{35:name}".into();
        assert_eq!(
            Ctx::new(&other, registry.clone(), None).fingerprint(),
            Ctx::new(&Config::default(), registry, None).fingerprint()
        );
    }

    #[test]
    fn the_fingerprint_ignores_diagnostics() {
        let registry = Arc::new(Registry::bundled_only());
        let plain = Config::default();
        let mut decorated = plain.clone();
        decorated.diagnostics.enabled = false;
        decorated.diagnostics.rules.insert(
            "integrity".into(),
            crate::config::RuleSetting::Enabled(false),
        );
        assert_eq!(
            Ctx::new(&plain, registry.clone(), None).fingerprint(),
            Ctx::new(&decorated, registry, None).fingerprint()
        );
    }

    #[test]
    fn the_fingerprint_follows_every_input_it_hashes() {
        let registry = Arc::new(Registry::bundled_only());
        let base = Ctx::new(&Config::default(), registry.clone(), None).fingerprint();
        let other = Config {
            default_version: "FIX.4.2".into(),
            ..Config::default()
        };
        assert_ne!(Ctx::new(&other, registry.clone(), None).fingerprint(), base);
        assert_ne!(
            Ctx::new(&Config::default(), registry, Some("FIX.4.2")).fingerprint(),
            base
        );
    }
}
