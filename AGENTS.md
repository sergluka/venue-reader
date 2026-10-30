# Venue Reader

An LSP server for FIX protocol logs: one binary, no editor-specific code. What
follows is what is easy to get wrong here and not visible from any one file.

## Editor compatibility

- The server is **generic**: it speaks standard LSP and must work with any client
  that does. No editor-specific code, tests, or CI in this repo.
- Where it adapts to a client, it adapts to that client's **advertised
  capabilities**, never to its identity. Anything that cannot be expressed that
  way is a documented limitation, not a special case.
- Editors still diverge — a capability nobody implements, a request shape that
  scales badly, a default that makes a response useless there. Treat each
  divergence as its own named problem and solve it **explicitly**: which editor,
  which request, what it does instead, what the server does about it, and whether
  that is an adaptation or an accepted limitation.
- Two things not to do: fix one editor by making the general path worse, and
  leave a divergence as an unexplained quirk in the code. If the reason is not
  obvious from the protocol, write it down where the next reader will look.

## Invariants

- **`codec/` is configuration-free.** Bytes and resolved dictionary sources in, a `LineAnalysis`
  out. Anything that needs `Config` or the `Registry` belongs in `ctx.rs`, which
  resolves transport/application identity for a line.
- **`LineAnalysis` holds no rendered strings** — only spans, dictionary indices,
  and the source owners they were resolved against. Names, enum labels, group paths,
  summaries and hover markdown are produced at request time. This is what keeps
  rendering options out of the cache key; putting a `String` in there quietly
  breaks that.
- **Rendering configuration lives in `render::Options`, not in `Ctx`.** It is
  carried beside the context in a `Snapshot`, because none of it can invalidate
  an analysis. That is what makes a `Ctx`'s *pointer* identity mean "these
  cached analyses are still valid" — `set_ctx` compares `Arc`s and nothing
  compares fingerprints twice. It holds because `ServerState` is the only thing
  that builds a context a document sees; build one elsewhere and every cached
  line is rebuilt.
- **Only viewport-bounded requests may populate a line's cache.** Hover, inlay
  hints and semantic tokens over a range may call `Snapshot::analysis`;
  anything that walks the whole document — diagnostics above all — calls
  `analyze` and drops the result. A fully populated cache costs several times
  the file size, and the budget is met by this rule rather than by the layout of
  any struct. The caching accessor is the one that carries the label, because it
  is the one that can break the budget.

  **The rule bounds who may cache; `limits.cachedLines` bounds how much
  accumulates.** The main loop sweeps every open document after every
  request, and only the main loop: a task populates and never evicts, so
  tasks on a pool cannot race over a slot. Second chance rather than LRU — a
  hit marks the line, a sweep clears the unmarked and resets the marks —
  because a screen is read as a block and left as a block. Measured
  2026-09-11: a whole-file scroll retains 1.83× the file against 4.60×
  unbounded, and the forty-screen sequence 1.77×, which the bound does not
  reach. `benches/rss_scroll.rs` gates the scroll as a budget.
- **A `FieldIdx`, `GroupIdx` or `EnumIdx` means nothing except against the `Dict`
  that produced it.** Every reference carries a transport/application owner;
  `LineAnalysis` retains both sources. FIX4 may use the same physical source
  twice; FIXT resolves application identity separately. Never pair an index
  with a dictionary from another source or line.
- **`xml/` is the source of truth for the bundled dictionaries.** `build.rs`
  generates the static tables into `OUT_DIR`; the generated code is never
  committed, and editing `xml/` is what changes them.
- **`parse.rs::MAX_ITEMS` is the contract the codec's packing rests on.** An
  index is stored in 16 bits with the top value reserved (`codec::Packed16`),
  so a dictionary collection that would overrun it is refused while parsing,
  where the message can name the file and the tag. `build.rs` asserts the same
  bound on the compressed application unions and standalone transport tables
  it generates. Source ownership consumes no packed-index bits.
- **`schema::pack` is the only producer of a `Schema`.** Header, trailer, every
  message and every group are spans over the same flat arrays, and `pack` is
  where the sorting, the `order` and `nested` tables, the name interning and the
  count bounds happen once; the parser makes every index with a checked
  conversion before `pack` sees it. `build.rs` includes `schema.rs` by `#[path]`
  like `model.rs` and `parse.rs`, so the bundled tables and a custom dictionary
  come out of the same function — and `schema.rs` stays std-only. Consumers read
  structure through `Dict::schema()` and the per-scope facts the parser derives
  from it (`GroupDef::delimiter_tag`); any other table built beside it (a reach
  set, a counter list) is a cache of the schema, never a source. The dictionary
  is the whole truth: a scope declared empty *is* empty, a file that would have
  left a part undescribed was refused or loaded with a `Fault`, and there is no
  completeness flag for a consumer to consult. Transport scopes are read from
  the transport source; application scopes are read from the application source.
- **Positions are bytes inside the crate** and become LSP `Position`s only at the
  protocol boundary, under the encoding negotiated in `initialize`. A span that
  leaks out unconverted is wrong for every non-ASCII line.

- **Rules are a pure function of `LineAnalysis` plus configuration** — the
  rule settings, and the name of the dictionary that decoded the line, which
  the caller passes because a name, like a rule setting, is not a
  `CtxFingerprint` input. That is why `diagnostics.rules.*` is not one either:
  toggling a rule, regrading its severity or renaming a dictionary invalidates
  no cached line. A rule that needs
  something the analysis does not carry is a signal to re-read the analysis,
  not to widen the rule.
- **Background results reach the client only through the main loop.** The
  diagnostics worker walks and reports back; the loop checks the document is
  still open and the generation still current, then sends. This is what makes a
  publish orderable against `didClose` — and against a configuration change,
  where the stale and fresh sets carry the same document version and a client
  cannot tell them apart. An edit made while a batch that includes its document
  is in flight is held and walked once at the commit, so no walk runs under
  rules the batch is about to replace.

## Release status

- Version `0.1.0` is published. Preserve compatibility for published CLI,
  configuration and LSP interfaces within the `0.1.x` series.
- Unpublished interfaces need no compatibility aliases, shims, deprecation periods,
  or automatic migrations.
- Document current behavior. Do not add migration or retirement guidance for
  unpublished features.

## Documentation

- README must describe current behavior, not introduced changes or fixes.
- Add README and wiki documentation only for completed features with settled behavior.
  Keep intermediate progress, temporary rollout stages, and unresolved decisions
  in local task records.
- After every task, review the sibling `../venue-reader.wiki` repository and
  update documentation affected by the task before marking it complete.
  Run the wiki documentation tests and link checker after documentation changes.

## Comments

- Document public structs and their public fields with brief `///` comments when
  their meaning is not clear from the names and types. Explain what the data
  represents, not merely what the declaration says.
- Skip comments for self-documenting code. For example, `pub tag: u32` needs no
  `// tag` comment, while `pub added: &'static str` needs to explain what was
  introduced and what the string represents, such as
  `/// FIX release that introduced this field.`

## Naming a tag

- Write a tag as its name followed by its number in parentheses —
  `MsgType(35)` — in doc comments, code comments, tests and documentation.
  Neither half alone: a bare number makes the reader open a dictionary, a bare
  name hides which field on the line is meant.

  ```rust
  /// Tag MsgType(35) is the one field whose value pulls a message definition
  /// in beside the enum label.
  ```

  The same for an enum value, which is a tag and its value: `Side(54) = 1`.

## Git

- Use Conventional Commits notation for every commit message.
- Keep every commit message to a single line.

## The gate

Green before every commit:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check          # after any dependency change
ruff check tools
python3 -m unittest discover -s tools   # the dictionary projection's own tests
```

`cargo bench` is outside the gate. It carries retention assertions of its own —
after a viewport, after a whole-document walk that keeps nothing, after a
diagnostics publish cycle, after a realistic reading sequence and after the
whole file — so a change that starts retaining more will fail there rather than
at a release. Some of those are budgets and some are regression baselines; each
bench says which it is and why.
