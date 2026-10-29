# Contributing

Use the [bug report form](https://github.com/sergluka/venue-reader/issues/new?template=bug_report.yml)
for reproducible failures. Include server/client versions, operating system,
installation method, expected/actual behavior and a minimal synthetic example.
Discuss substantial changes in an issue before implementation.

Protocol behavior, dictionaries and tests must come from public specifications
or independently created synthetic data. Include source links. Do not submit
private logs or dictionaries, even with names and values removed.
The server must remain independent of editor identity; adapt to advertised LSP
capabilities.

Report suspected vulnerabilities using the [security policy](SECURITY.md).
Debug and trace output can contain document contents; do not attach it by default.

Run the source checks before submitting a change:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
ruff check tools
python3 -m unittest discover -s tools
```

Run `cargo deny check` after dependency changes and refresh the dependency
notices.

Keep affected README/wiki documentation consistent with current behavior.
The [contributor reference](https://github.com/sergluka/venue-reader/wiki/Contributing)
covers pull-request preparation, documentation tests and dependency notices.
