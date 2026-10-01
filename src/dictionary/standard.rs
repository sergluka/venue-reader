//! How a tag stands against standard FIX, asked of two witnesses.
//!
//! FIX Latest (the Orchestra projection) says what is standard; the QuickFIX/J
//! files say what each bundled version contained. Neither is the truth: FIX
//! Latest keeps nothing FIX removed and misdates some tags, the QuickFIX/J files
//! stop at about EP94 and carry leftovers. So a claim is made only where both
//! agree or only one could know, and where they disagree, in either direction,
//! nothing is said. Both sources are pinned in `xml/README.md`.
//!
//! "Standard" means those pinned sources, FIX Latest as of its pinned pack
//! (EP312). A tag a later pack adds reads [`Standard::Nowhere`] until the
//! projection is refreshed, as would a definition that existed only in
//! FIX.2.7/FIX.3.0 (none is known). QuickFIX/J also declares retired tags no
//! message uses — 36 in `FIX50.xml`, 16 in `FIX43.xml`; FIX Latest has forgotten
//! them, so they stay [`Standard::InVersion`], unchallenged.

use super::bundle::{implied_first_release, release_of, FIRST_BUNDLED_RANK, USER_DEFINED};
use super::dict::{tag_record, Version};
use super::statics::{LatestTag, TagRecord, LATEST_NAMES, LATEST_TAGS, LATEST_VERSIONS};

/// How the two pinned sources see a tag, for one version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standard {
    /// The bundled dictionary of the version defines it, and FIX Latest does
    /// not contradict that.
    InVersion,
    /// The sources disagree on whether the version has it: nothing is claimed.
    Disputed,
    /// Another bundled dictionary defines it, and FIX Latest does not date it
    /// into this version.
    OtherVersion(StandardDef),
    /// FIX Latest defines it; no bundled dictionary does.
    Unbundled(StandardDef),
    /// In neither source, or only in QuickFIX/J inside the user-defined range.
    Nowhere,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardDef {
    /// FIX Latest's name; QuickFIX/J's for a retired tag.
    pub name: &'static str,
    /// As FIX Latest dates it; empty for a retired tag.
    pub added: &'static str,
    /// 0 when no extension pack is recorded.
    pub added_ep: u16,
    /// Whether the bundled files bear the date out.
    pub date_holds: bool,
    /// What the files say about a retired tag at the version asked; `None`
    /// also when that version predates the tag, which "not in" already says.
    pub retirement: Option<Retirement>,
}

/// A tag standard FIX no longer has, as of the version asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retirement {
    /// Gone by then, after this release — the run before it never breaks.
    After(&'static str),
    /// Gone by then; after which release is not claimed, because the run
    /// breaks: ExecTransType(20) leaves after FIX.4.3 and `FIX50.xml`
    /// declares it again.
    Gone,
}

impl StandardDef {
    /// Bundled once, gone from FIX Latest: it has no date.
    pub fn retired(&self) -> bool {
        self.added.is_empty()
    }

    /// The FIX version that introduced it: the release its date names, or
    /// "FIX Latest" past FIX.5.0SP2. `None` when the files do not bear the
    /// date out. An extension pack is how a tag got into a release, so it is
    /// never the answer: `FIX.5.0 EP55` was introduced in FIX.5.0SP1.
    ///
    /// The same `release_of` the lifecycle line uses, so one tag cannot be
    /// given two dates by two sentences.
    pub fn introduced(&self) -> Option<&'static str> {
        self.date_holds
            .then(|| release_of(self.added, self.added_ep))
            .flatten()
    }
}

impl Standard {
    /// `version` is the binding's, `None` when it has no bundled tables; then
    /// `InVersion` and `Disputed` are unreachable and `OtherVersion` means only
    /// that some bundled dictionary defines the tag.
    pub fn of_tag(version: Option<Version>, tag: u32) -> Standard {
        let Some(record) = tag_record(tag) else {
            return match latest_tag(tag) {
                Some(latest) => Standard::Unbundled(StandardDef {
                    name: latest.name,
                    added: LATEST_VERSIONS[usize::from(latest.added)],
                    added_ep: latest.added_ep,
                    // `build.rs` asserts these are dated after every bundled
                    // release, so no file can contradict them.
                    date_holds: true,
                    retirement: None,
                }),
                None => Standard::Nowhere,
            };
        };
        let latest_knows = !record.added.is_empty();
        if !latest_knows && USER_DEFINED.contains(&tag) {
            return Standard::Nowhere;
        }
        if let Some(version) = version {
            let listed = record.presence.contains(ordinal(version));
            // Where FIX Latest dates the tag, the date must agree with the file.
            let dated_in = latest_knows.then(|| dated_into(record, version));
            match (listed, dated_in) {
                (true, Some(false)) | (false, Some(true)) => return Standard::Disputed,
                (true, _) => return Standard::InVersion,
                (false, _) => {}
            }
        }
        Standard::OtherVersion(StandardDef {
            name: latest_name(tag).unwrap_or(record.name),
            added: record.added,
            added_ep: record.added_ep,
            date_holds: date_holds(tag),
            retirement: if latest_knows {
                None
            } else {
                retirement(record, version)
            },
        })
    }
}

/// Whether the bundled files bear out the date FIX Latest gives a bundled tag:
/// it first appears in the release that date implies. False for a tag FIX
/// Latest does not date and for one no bundled dictionary has.
pub fn date_holds(tag: u32) -> bool {
    tag_record(tag).is_some_and(|record| {
        let implied = implied_first_release(record.added, record.added_ep);
        implied.is_some() && implied == first_release(record)
    })
}

fn latest_tag(tag: u32) -> Option<&'static LatestTag> {
    LATEST_TAGS
        .binary_search_by_key(&tag, |latest| latest.tag)
        .ok()
        .map(|i| &LATEST_TAGS[i])
}

fn latest_name(tag: u32) -> Option<&'static str> {
    LATEST_NAMES
        .binary_search_by_key(&tag, |(tag, _)| *tag)
        .ok()
        .map(|i| LATEST_NAMES[i].1)
}

/// A bundled version's mask ordinal: its position in `Version::BUNDLED`, which
/// `build.rs` assigns bits in — asserted by the tests.
fn ordinal(version: Version) -> u8 {
    let version = version.resolve_alias();
    let at = Version::BUNDLED
        .iter()
        .position(|bundled| *bundled == version)
        .expect("every Version resolves to a bundled one");
    at as u8
}

/// The rank of a bundled version's tables. Bundled ranks are contiguous, so
/// the ordinal is the offset.
fn table_rank(version: Version) -> u8 {
    FIRST_BUNDLED_RANK + ordinal(version)
}

/// FIX Latest puts the tag in `version` or earlier.
fn dated_into(record: &TagRecord, version: Version) -> bool {
    implied_first_release(record.added, record.added_ep)
        .is_some_and(|release| release <= table_rank(version))
}

/// The rank of the first bundled release whose file declares the tag.
fn first_release(record: &TagRecord) -> Option<u8> {
    let first = record.presence.0.trailing_zeros();
    (first < u16::BITS).then(|| FIRST_BUNDLED_RANK + first as u8)
}

/// Where a retired tag stands at `version`: whether it is gone by then, and
/// after which release the files can say so.
fn retirement(record: &TagRecord, version: Option<Version>) -> Option<Retirement> {
    let asked = table_rank(version?);
    let mask = record.presence.0;
    let first = first_release(record)?;
    let last = FIRST_BUNDLED_RANK + (u16::BITS - 1 - mask.leading_zeros()) as u8;
    if asked <= first || last >= asked {
        return None;
    }
    // Contiguous ranks: an unbroken run has one bit per release it spans.
    let unbroken = u32::from(last - first + 1) == mask.count_ones();
    Some(if unbroken {
        Retirement::After(Version::BUNDLED[usize::from(last - FIRST_BUNDLED_RANK)].as_str())
    } else {
        Retirement::Gone
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::dictionary::bundle::{parse_metadata, rank, LAST_BUNDLED_RANK};
    use crate::dictionary::dict::bundled;
    use crate::dictionary::parse::Source;
    use crate::dictionary::statics::TAGS;
    use crate::dictionary::Dict;

    /// The mask rewrite rests on this: bit `i` is `Version::BUNDLED[i]`, and
    /// bundled ranks are contiguous from `FIRST_BUNDLED_RANK`.
    #[test]
    fn mask_ordinals_follow_bundled_order_and_ranks_are_contiguous() {
        for (at, version) in Version::BUNDLED.into_iter().enumerate() {
            assert_eq!(usize::from(bundled(version).mask_bit), at, "{version:?}");
            assert_eq!(usize::from(ordinal(version)), at);
            assert_eq!(
                rank(version.as_str()),
                Some(table_rank(version)),
                "{version:?}"
            );
        }
        assert_eq!(ordinal(Version::Fixt11), ordinal(Version::Fix50Sp2));
    }

    const FIX42: Option<Version> = Some(Version::Fix42);
    const FIX44: Option<Version> = Some(Version::Fix44);
    const FIX50: Option<Version> = Some(Version::Fix50);
    const FIX50SP1: Option<Version> = Some(Version::Fix50Sp1);
    const FIX50SP2: Option<Version> = Some(Version::Fix50Sp2);

    /// The six tags QuickFIX/J's `FIX50.xml` declares in the user-defined range.
    const QUICKFIX_ONLY: [u32; 6] = [5450, 8000, 9000, 9001, 9002, 9995];

    fn def(standard: Standard) -> StandardDef {
        match standard {
            Standard::OtherVersion(def) | Standard::Unbundled(def) => def,
            other => panic!("no definition: {other:?}"),
        }
    }

    fn all_tags() -> Vec<u32> {
        TAGS.iter()
            .map(|record| record.tag)
            .chain(LATEST_TAGS.iter().map(|latest| latest.tag))
            .collect()
    }

    #[test]
    fn a_tag_the_version_lists_is_in_version() {
        assert_eq!(Standard::of_tag(FIX44, 55), Standard::InVersion);
        // EncryptedPasswordMethod(1400) is FIX.5.0 EP56, and the SP1 file has it.
        assert_eq!(Standard::of_tag(FIX50SP1, 1400), Standard::InVersion);
        assert_eq!(Standard::of_tag(FIX50SP2, 1426), Standard::InVersion);
        assert_eq!(
            Standard::of_tag(Some(Version::Fixt11), 1328),
            Standard::InVersion,
            "a FIXT line reads the FIX.5.0SP2 tables"
        );
    }

    /// RejectText(1328) is FIX.5.0 EP55: added after 5.0, first shipped in SP1.
    #[test]
    fn an_extension_pack_tag_is_elsewhere_before_its_release() {
        let expected = Standard::OtherVersion(StandardDef {
            name: "RejectText",
            added: "FIX.5.0",
            added_ep: 55,
            date_holds: true,
            retirement: None,
        });
        assert_eq!(Standard::of_tag(FIX44, 1328), expected);
        assert_eq!(Standard::of_tag(FIX50, 1328), expected);
        assert_eq!(Standard::of_tag(FIX50SP1, 1328), Standard::InVersion);
    }

    #[test]
    fn the_sources_disagreeing_either_way_is_disputed() {
        // FIX Latest dates it FIX.4.2; the FIX.4.2 file lacks it.
        assert_eq!(Standard::of_tag(FIX42, 220), Standard::Disputed);
        // Session tags FIX Latest dates earlier, missing from the SP1 file.
        for tag in [108, 1137, 1407] {
            assert_eq!(Standard::of_tag(FIX50SP1, tag), Standard::Disputed, "{tag}");
        }
        assert_eq!(Standard::of_tag(FIX44, 809), Standard::Disputed);
        // ApplReportType(1426): in the SP1 file, dated SP2.
        assert_eq!(Standard::of_tag(FIX50SP1, 1426), Standard::Disputed);
    }

    #[test]
    fn a_date_the_files_do_not_bear_out_is_not_claimed() {
        // DefaultApplExtID(1407) is FIX.5.0 EP56, so FIX.5.0 lacks it either way.
        assert!(matches!(
            Standard::of_tag(FIX50, 1407),
            Standard::OtherVersion(StandardDef {
                date_holds: false,
                ..
            })
        ));
        let before = def(Standard::of_tag(Some(Version::Fix41), 220));
        assert_eq!(before.added, "FIX.4.2");
        assert!(!before.date_holds, "first in the FIX.4.3 file");
        let holds = def(Standard::of_tag(FIX42, 559));
        assert_eq!(
            (holds.name, holds.added),
            ("SecurityListRequestType", "FIX.4.3")
        );
        assert!(holds.date_holds);
    }

    /// Only FIX versions: `FIX.5.0 EP55` was introduced in FIX.5.0SP1.
    #[test]
    fn introduced_names_the_release_never_the_pack() {
        let introduced = |version, tag| def(Standard::of_tag(version, tag)).introduced();
        assert_eq!(introduced(FIX44, 1328), Some("FIX.5.0SP1"));
        assert_eq!(introduced(FIX42, 559), Some("FIX.4.3"));
        assert_eq!(introduced(FIX44, 50001), Some("FIX Latest"));
        assert_eq!(
            introduced(Some(Version::Fix41), 220),
            None,
            "the files say 4.3"
        );
        assert_eq!(introduced(FIX44, 47), None, "retired: no date");
    }

    /// A date before the first bundled file is its own answer. Unreachable
    /// through `of_tag` — such a tag is in every bundled file, so it is never
    /// `OtherVersion` — and pinned because `release_of` stopped clamping it to
    /// FIX.4.0, which the numeric form still does for the table checks.
    #[test]
    fn a_date_older_than_the_bundled_files_is_named_as_written() {
        let def = StandardDef {
            name: "T",
            added: "FIX.2.7",
            added_ep: 0,
            date_holds: true,
            retirement: None,
        };
        assert_eq!(def.introduced(), Some("FIX.2.7"));
        assert_eq!(
            implied_first_release("FIX.2.7", 0),
            Some(FIRST_BUNDLED_RANK)
        );
    }

    /// Rule80A(47) runs unbroken through FIX.4.3. ExecTransType(20) leaves
    /// after FIX.4.3 and `FIX50.xml` declares it again: under FIX.4.4 it is in
    /// a gap, not retired, and once past FIX.5.0 no release can be named.
    /// SendingDate(51) is only in `FIX43.xml`, so under FIX.4.2 it is not yet
    /// a tag at all, let alone a retired one.
    #[test]
    fn a_retirement_is_dated_only_after_an_unbroken_run() {
        let retirement = |version, tag| def(Standard::of_tag(version, tag)).retirement;
        assert_eq!(retirement(FIX44, 47), Some(Retirement::After("FIX.4.3")));
        assert_eq!(retirement(FIX44, 20), None, "absent, but it comes back");
        assert_eq!(
            retirement(Some(Version::Fix50Sp1), 20),
            Some(Retirement::Gone)
        );
        assert_eq!(retirement(FIX42, 51), None, "not there yet");
        assert_eq!(retirement(None, 47), None, "no version to compare with");
    }

    /// Every release a retirement names is one the tag has left: before the
    /// version asked, and the last release the files declare it in.
    #[test]
    fn every_retirement_boundary_is_before_the_version_asked() {
        for version in Version::ALL {
            let asked = rank(version.resolve_alias().as_str()).unwrap();
            for record in TAGS {
                let Standard::OtherVersion(def) = Standard::of_tag(Some(version), record.tag)
                else {
                    continue;
                };
                let Some(Retirement::After(release)) = def.retirement else {
                    continue;
                };
                assert!(
                    rank(release).unwrap() < asked,
                    "{} {}",
                    version.as_str(),
                    record.tag
                );
                assert_eq!(
                    Some(release),
                    Version::BUNDLED
                        .into_iter()
                        .rfind(|release| record.presence.contains(bundled(*release).mask_bit))
                        .map(Version::as_str),
                    "the last release that declares it"
                );
            }
        }
    }

    #[test]
    fn a_retired_tag_keeps_its_quickfix_name_and_no_date() {
        let rule80a = def(Standard::of_tag(FIX44, 47));
        assert_eq!(rule80a.name, "Rule80A");
        assert!(rule80a.retired());
        assert!(!rule80a.date_holds);
        // ExecTransType(20) is retired, but `FIX50.xml` still declares it.
        assert_eq!(Standard::of_tag(FIX50, 20), Standard::InVersion);
    }

    #[test]
    fn the_name_is_fix_latests() {
        assert_eq!(def(Standard::of_tag(FIX44, 1008)).name, "SideTrdSubType");
    }

    #[test]
    fn a_tag_only_fix_latest_knows_is_unbundled() {
        assert_eq!(
            Standard::of_tag(FIX44, 50001),
            Standard::Unbundled(StandardDef {
                name: "BatchTotalMessages",
                added: "FIX.5.0SP2",
                added_ep: 178,
                date_holds: true,
                retirement: None,
            })
        );
        let upi = def(Standard::of_tag(FIX44, 2891));
        assert_eq!(upi.introduced(), Some("FIX Latest"), "{upi:?}");
    }

    #[test]
    fn a_tag_in_neither_source_or_in_the_user_range_is_nowhere() {
        assert_eq!(Standard::of_tag(FIX44, 20001), Standard::Nowhere);
        for tag in QUICKFIX_ONLY {
            assert_eq!(Standard::of_tag(FIX50, tag), Standard::Nowhere, "{tag}");
            assert_eq!(Standard::of_tag(FIX44, tag), Standard::Nowhere, "{tag}");
        }
    }

    /// Symbol(55) is dated FIX.2.7; the first bundled file is FIX.4.0.
    #[test]
    fn without_a_version_only_bundled_presence_is_said() {
        let symbol = def(Standard::of_tag(None, 55));
        assert!(symbol.date_holds);
        assert_eq!(Standard::of_tag(None, 20001), Standard::Nowhere);
    }

    /// What guarantees a bundled dictionary's hover stays as it was: a field
    /// that passed its presence test is in version, bar the six and
    /// ApplReportType(1426).
    #[test]
    fn every_bundled_field_is_in_version_but_the_known_exceptions() {
        let mut exceptions = Vec::new();
        for version in Version::ALL {
            let dict = Dict::Static(bundled(version));
            for record in TAGS {
                if dict.field_idx(record.tag).is_some()
                    && Standard::of_tag(Some(version), record.tag) != Standard::InVersion
                {
                    exceptions.push((version, record.tag));
                }
            }
        }
        let mut expected: Vec<(Version, u32)> =
            QUICKFIX_ONLY.map(|tag| (Version::Fix50, tag)).to_vec();
        expected.push((Version::Fix50Sp1, 1426));
        exceptions.sort_by_key(|(v, t)| (v.as_str(), *t));
        expected.sort_by_key(|(v, t)| (v.as_str(), *t));
        assert_eq!(exceptions, expected);
    }

    #[test]
    fn the_disputed_pairs_are_exactly_the_known_disagreements() {
        let mut disputed: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
        for version in Version::ALL {
            for tag in all_tags() {
                if Standard::of_tag(Some(version), tag) == Standard::Disputed {
                    disputed.entry(version.as_str()).or_default().push(tag);
                }
            }
        }
        let benchmark_block = vec![
            220, 221, 222, 224, 225, 226, 227, 228, 229, 230, 232, 233, 234, 235, 236, 237, 238,
            240, 241, 242, 244, 245, 246, 247, 248, 249, 251, 252, 253, 254, 255, 256, 257, 258,
            259, 260,
        ];
        let session_and_1426 = vec![
            7, 16, 36, 98, 108, 112, 123, 141, 371, 373, 383, 384, 385, 464, 789, 1137, 1407, 1408,
            1409, 1410, 1426,
        ];
        let expected = BTreeMap::from([
            ("FIX.4.2", benchmark_block),
            ("FIX.4.4", vec![809]),
            ("FIX.5.0", vec![809]),
            ("FIX.5.0SP1", session_and_1426),
        ]);
        assert_eq!(disputed, expected);
        assert_eq!(disputed.values().map(Vec::len).sum::<usize>(), 59);
    }

    #[test]
    fn exactly_the_known_dates_do_not_hold() {
        let failing: Vec<u32> = TAGS
            .iter()
            .filter(|record| !record.added.is_empty() && !date_holds(record.tag))
            .map(|record| record.tag)
            .collect();
        assert_eq!(failing.len(), 42, "{failing:?}");
        assert!(failing.contains(&226) && failing.contains(&1426));
    }

    /// A printed date says "not in yet", so it must be later than the tables.
    #[test]
    fn every_date_hover_would_print_is_later_than_the_tables() {
        for version in Version::BUNDLED {
            let tables = rank(version.as_str()).unwrap();
            for tag in all_tags() {
                let def = match Standard::of_tag(Some(version), tag) {
                    Standard::OtherVersion(def) | Standard::Unbundled(def) => def,
                    _ => continue,
                };
                if def.date_holds && !def.retired() {
                    let release = implied_first_release(def.added, def.added_ep).unwrap();
                    assert!(release > tables, "{} {tag}: {def:?}", version.as_str());
                }
            }
        }
    }

    /// The lifecycle line and the standing note are one tag's two sentences, so
    /// the date either reaches both or neither: whatever hover prints for a
    /// bundled tag whose date holds must imply the first file that declares it.
    ///
    /// Read back through the numeric form rather than compared by rank, because
    /// the 102 tags dated FIX.2.7 or FIX.3.0 are older than every bundled file
    /// and print it — MsgType(35) says FIX.2.7, which no file contradicts. The
    /// clamp is what makes those agree, and it lives in one place.
    #[test]
    fn the_lifecycle_line_names_the_file_that_first_has_the_tag() {
        for version in Version::BUNDLED {
            let dict = Dict::Static(bundled(version));
            for record in TAGS {
                let Some(idx) = dict.field_idx(record.tag) else {
                    continue;
                };
                let Some(printed) = dict.field(idx).lifecycle().added else {
                    continue;
                };
                if !date_holds(record.tag) {
                    // Hover drops these; what the view carries is not printed.
                    continue;
                }
                assert_eq!(
                    implied_first_release(printed, 0),
                    first_release(record),
                    "{} {}: {printed}",
                    version.as_str(),
                    record.tag
                );
            }
        }
    }

    fn metadata() -> crate::dictionary::bundle::Metadata {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("xml/standard/FIXLatest-metadata.xml");
        parse_metadata(&Source::read(path).unwrap()).unwrap()
    }

    #[test]
    fn the_fix_latest_tables_cover_the_metadata_exactly() {
        let meta = metadata();
        assert!(LATEST_TAGS.windows(2).all(|w| w[0].tag < w[1].tag));
        assert!(LATEST_TAGS
            .iter()
            .all(|latest| tag_record(latest.tag).is_none()));
        for tag in meta.fields.keys() {
            let known = tag_record(*tag).is_some()
                || LATEST_TAGS
                    .binary_search_by_key(tag, |latest| latest.tag)
                    .is_ok();
            assert!(known, "{tag}");
        }
        assert_eq!(LATEST_TAGS.len(), 4751);
        assert!(LATEST_VERSIONS
            .iter()
            .all(|version| rank(version).is_some()));
        assert!(LATEST_TAGS.iter().all(|latest| {
            let added = LATEST_VERSIONS[usize::from(latest.added)];
            implied_first_release(added, latest.added_ep).unwrap() > LAST_BUNDLED_RANK
        }));

        let renamed: Vec<(u32, &str)> = TAGS
            .iter()
            .filter_map(|record| {
                let latest = meta.fields.get(&record.tag)?;
                (latest.name != record.name).then_some((record.tag, record.name))
            })
            .collect();
        assert_eq!(
            renamed.iter().map(|(tag, _)| *tag).collect::<Vec<_>>(),
            LATEST_NAMES.iter().map(|(tag, _)| *tag).collect::<Vec<_>>()
        );
        assert_eq!(
            LATEST_NAMES,
            &[
                (1008, "SideTrdSubType"),
                (1225, "DerivativeOptPayoutAmount")
            ]
        );
    }
}
