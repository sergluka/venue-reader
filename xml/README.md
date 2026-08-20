# Bundled dictionary data

Everything under `standard/` is compiled into the binary by `build.rs`. The
manifest `standard/SHA256SUMS` lists every file; check it from that directory
with `sha256sum -c SHA256SUMS` (the test suite checks it too).

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

Each version's file alone decides which fields, enum values, messages and
groups that version has. `FIXT11.xml`'s header, trailer and session messages
are folded into FIX.5.0SP2, the tables `8=FIXT.1.1` resolves to; FIX.5.0 and
FIX.5.0SP1 keep QuickFIX/J's empty session blocks.

## FIX Orchestra metadata — presentation

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

It is joined on id and never adds a field, message or group — the version's
QuickFIX file alone decides those. Field names come from that file; enum
labels and the shown type spelling come from Orchestra when the id matches,
while the type class stays the version's. MsgType values that name a message
the version defines keep that message's name. Orchestra describes FIX Latest,
so `added` is the same in every version and a `deprecated` later than a version
is dropped from it.

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

## Test fixtures (`custom/`)

Synthetic dictionaries written for this project's tests (Apache-2.0), in
QuickFIX format and in the FIX Repository directory layout; see
`custom/README.md`, including how to run the parser over real broker
dictionaries you download yourself.
