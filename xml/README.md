# Bundled dictionary data

Everything under `standard/` is compiled into the binary by `build.rs`. The
manifest `standard/SHA256SUMS` lists every file; check it from that directory
with `sha256sum -c SHA256SUMS` (the test suite checks it too).

## Why these two sources

Both carry permissive licences that allow bundling: FIX Orchestra is
Apache-2.0, QuickFIX/J the BSD-style QuickFIX Software License 1.0.

- **FIX Orchestra** is published by the FIX Trading Community itself, so it is
  as close to the standard as a machine-readable source gets. But it describes
  FIX Latest only, with no per-version structure.
- **QuickFIX/J** is a widely used, long-running FIX engine, so its per-version
  dictionaries are battle-tested against real sessions.

The per-version FIX Repository (2010 edition) was not used: its package is
under the proprietary FPL licence, with no Apache statement.

### What is taken from each

| | QuickFIX/J, per version | FIX Orchestra, FIX Latest |
|---|---|---|
| Which fields, messages and groups a version has | the whole list; the join adds none | — |
| Field name | what hover shows for the version | the standard name, where it differs |
| Datatype | the class used to parse and format | the spelling shown |
| Description | — (these files carry none) | field, value and message synopses |
| Enum values | every code the version's file lists | own-set codes dated to that version or earlier (below) |
| Enum value label | fallback when Orchestra has no such code | when the id and the code match |
| `added`, `addedEP`, `deprecated` | — | the whole pedigree |
| Message category | — | taken |
| What is standard FIX today | — | the witness |
| What a version had and the standard has since dropped | the only witness | forgets it |

## QuickFIX/J dictionaries — structure and enum membership

Unmodified copies, so the source and output hashes are equal.

- Repository: <https://github.com/quickfix-j/quickfixj>
- Commit: `560200e6237bb6172acb18cbf3e5513fdf8cacb5`
- Retrieved: 2026-09-17
- Licence: QuickFIX Software License, Version 1.0 (see `../THIRD_PARTY_LICENSES.txt`)

| File | Original path | Source SHA-256 | Output SHA-256 |
|---|---|---|---|
| `FIX40.xml` | `quickfixj-messages/quickfixj-messages-fix40/src/main/resources/FIX40.xml` | `99105cc3deb65d1f2d6d7028c6b52d5a24825cc463d9aeb0d26295a3e1f93704` | `99105cc3deb65d1f2d6d7028c6b52d5a24825cc463d9aeb0d26295a3e1f93704` |
| `FIX41.xml` | `quickfixj-messages/quickfixj-messages-fix41/src/main/resources/FIX41.xml` | `953c948b8de436b642f0e02cfe738a2ed276ff31335672b25de6ae4adcd24f04` | `953c948b8de436b642f0e02cfe738a2ed276ff31335672b25de6ae4adcd24f04` |
| `FIX42.xml` | `quickfixj-messages/quickfixj-messages-fix42/src/main/resources/FIX42.xml` | `5c40750d2514ac1fe6d103223ebd365fa825e799dd5a4f274d698183a160cae4` | `5c40750d2514ac1fe6d103223ebd365fa825e799dd5a4f274d698183a160cae4` |
| `FIX43.xml` | `quickfixj-messages/quickfixj-messages-fix43/src/main/resources/FIX43.xml` | `2fc6ecdb903a5e34cc4c690ba4698e812f5b2bd834e39d9ab5167e7a5e213fbe` | `2fc6ecdb903a5e34cc4c690ba4698e812f5b2bd834e39d9ab5167e7a5e213fbe` |
| `FIX44.xml` | `quickfixj-messages/quickfixj-messages-fix44/src/main/resources/FIX44.xml` | `ccf5dd357643d9a069e881a562420281a26c18ae5ee30a2da3268fb70177cb15` | `ccf5dd357643d9a069e881a562420281a26c18ae5ee30a2da3268fb70177cb15` |
| `FIX50.xml` | `quickfixj-messages/quickfixj-messages-fix50/src/main/resources/FIX50.xml` | `ac94003169972427ebe772977ce9f4a3c0a4face7097fe3f6ba009ea721ac29d` | `ac94003169972427ebe772977ce9f4a3c0a4face7097fe3f6ba009ea721ac29d` |
| `FIX50SP1.xml` | `quickfixj-messages/quickfixj-messages-fix50sp1/src/main/resources/FIX50SP1.xml` | `2a78196113d989d6b287cfdd31480a31497df37e6e4f982ca98d22f0f696f88d` | `2a78196113d989d6b287cfdd31480a31497df37e6e4f982ca98d22f0f696f88d` |
| `FIX50SP2.xml` | `quickfixj-messages/quickfixj-messages-fix50sp2/src/main/resources/FIX50SP2.xml` | `019b4bc17898fbf31d43d23d567794eab14275e47161572889d03d5bf6ad1dde` | `019b4bc17898fbf31d43d23d567794eab14275e47161572889d03d5bf6ad1dde` |
| `FIXT11.xml` | `quickfixj-messages/quickfixj-messages-fixt11/src/main/resources/FIXT11.xml` | `1104d3746ebcfd1e03cc437785acd7c196a36ea43c0692c40c6b5e6eeeb7e4b6` | `1104d3746ebcfd1e03cc437785acd7c196a36ea43c0692c40c6b5e6eeeb7e4b6` |

`FIXT11.xml`'s header, trailer and session messages are folded into
FIX.5.0SP2, the tables `8=FIXT.1.1` resolves to; FIX.5.0 and FIX.5.0SP1 keep
QuickFIX/J's empty session blocks.

## FIX Orchestra metadata — presentation, and what is standard

`standard/FIXLatest-metadata.xml` is derived: `tools/orchestra_projection.py`
projects it from the official FIX Latest Orchestra file and refuses any other
input.

- Repository: <https://github.com/FIXTradingCommunity/orchestrations>
- Commit: `cd24169a2abd8daba7c360987c7a46ca11873a12`
- Original path: `FIX Standard/OrchestraFIXLatest.xml`
- Retrieved: 2026-09-16
- Licence: Apache-2.0, © FIX Protocol Ltd. (see `../THIRD_PARTY_LICENSES.txt`)
- Source SHA-256: `1bf82a22423d6504fbb77c4c8c522a4efefb32aec19b630c138860ccddf8228d`
- Output SHA-256: `7f44682acd78a7352bddf948835637cf7cdf24bf18299788c87e29aa67681ab3`

The projection keeps only:

- field id, name, base type, `added`, `addedEP`, `deprecated`, `ownCodeSet`, and
  the first non-empty `SYNOPSIS`;
- per field, each code's value, name, `added`, `addedEP`, `deprecated`, and first
  non-empty `SYNOPSIS` (a code set shared by several fields is listed under each);
- message type, name, category, and first non-empty `SYNOPSIS`.

`addedEP` is written only when the source names a positive extension pack;
absent and `-1` both mean no pack is recorded, and the generator refuses `0`, any
other negative value, a non-numeric value, or one too large for a `u16`.

`ownCodeSet="true"` marks the one field a code set is named after — Orchestra
links a field to its set by type name, but gives a set that belongs to one field
that field's id. The generator refuses a source where a set carries a field's id
without that field referencing it. Other fields may share the set: 216 of the 694
own sets are also used elsewhere (MsgTypeCodeSet, id 35, is referenced by 35, 372
and 1619), which is why ownership is decided by id and not by how many fields use
the set.

The join is on id. MsgType values that name a message the version defines keep
that message's name. Orchestra describes FIX Latest, so `added` is the same in
every version and a `deprecated` later than a version is dropped from it.

**Enum values are the one thing the join adds.** A version lists what its
QuickFIX file declares plus the codes of the field's own set (`ownCodeSet`)
that Orchestra dates to that version or earlier and that no extension pack
brought in (`addedEP`). A shared set only relabels what is already there, and
`MsgType(35)` is never extended. This is why `PartySubIDType(803)` has 26
values under FIX.4.4 where FIX44.xml lists none. Restored per version: 4.0 +0,
4.1 +4, 4.2 +58, 4.3 +251, 4.4 +139, 5.0 +60, 5.0SP1 +0, 5.0SP2 +0. A value a
later extension pack added can still appear when the QuickFIX file lists it —
`803 = 28` from FIX.5.0 on — because that file is taken as the version.

To regenerate:

```sh
curl -fLo OrchestraFIXLatest.xml \
  'https://raw.githubusercontent.com/FIXTradingCommunity/orchestrations/cd24169a2abd8daba7c360987c7a46ca11873a12/FIX%20Standard/OrchestraFIXLatest.xml'
tools/orchestra_projection.py OrchestraFIXLatest.xml xml/standard/FIXLatest-metadata.xml
```

## Standard status — two witnesses

Hover under a custom dictionary says whether a tag is standard FIX for the
line's version (`src/dictionary/standard.rs`). Neither witness is the truth on
its own, so a claim — membership, introduction date, versioned link — is made
only where both agree or only one could know, and nothing is said where they
disagree. Where they differ, over the pinned files:

- **Only in FIX Latest: 4,751 tags**, every one added by EP98 or later
  (NoOrderAttributes(2593) EP222, BatchTotalMessages(50001) EP178). Standard,
  newer than any bundled dictionary.
- **Only in QuickFIX/J: 46 tags.** 40 below 5000 are retired standard tags
  (ExecTransType(20), Rule80A(47), …), which FIX Latest forgets. Six — MDElementName(5450), SimulatedTime(8000), NoNestedUserData(9000),
  NestedUserDataName(9001), NestedUserDataValue(9002), SpotValueDateForNDF(9995)
  — sit in the range FIX reserves for users,
  5000–39999 (<https://www.fixtrading.org/standards/user-defined-fields/>,
  retrieved 2026-09-20), are declared only in `FIX50.xml`, are used by no
  message there, and are absent from the `FIX50.xml` of QuickFIX C++ and
  QuickFIX/n (GitHub, fetched 2026-09-20): not standard. FIX Latest has no tag
  in that range; from 40000 up tags are standard again.
- **Dated differently: 59 (version, tag) pairs.** An `addedEP` is read as
  "applied on top of `added`, shipped in the next release", which holds for 518
  of the 522 EP-dated bundled tags. Hover dates a field by that release — the
  standing note and the lifecycle line share one `release_of` — and names
  neither the pack nor the `added` it was applied to. RepurchaseTerm(226) and 35 others in
  220–260 are dated FIX.4.2 but first appear in `FIX43.xml`, and the reference
  site agrees with QuickFIX/J (`4.3/tagNum_220` exists, `4.2/tagNum_220` does
  not; probed 2026-09-20). NoUsernames(809) is missing from `FIX44.xml` and `FIX50.xml`; 20
  session tags from `FIXT11.xml` are missing from `FIX50SP1.xml`, which keeps
  QuickFIX/J's empty session block; ApplReportType(1426) is in `FIX50SP1.xml`
  though dated FIX.5.0SP2. 42 tags therefore show no introduction date.

`build.rs` asserts the facts the classification leans on (an empty `added`
means FIX Latest lacks the tag; nothing FIX Latest-only is dated into a bundled
release; the user-defined range), and `cargo test dictionary::standard` pins
the counts above, so a refreshed source that moves them fails the build or the
tests rather than changing hover silently. Enum values are not classified yet:
the value masks above already mix the two witnesses.

## Test fixtures (`custom/`)

Synthetic dictionaries written for this project's tests (Apache-2.0), in
QuickFIX format and in the FIX Repository directory layout; see
`custom/README.md`, including how to run the parser over real broker
dictionaries you download yourself.
