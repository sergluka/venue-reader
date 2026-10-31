# Venue Reader — Language Server for FIX Protocol Logs

**Decode and inspect FIX protocol logs directly in your editor.**

Venue Reader is an open-source [language server](https://microsoft.github.io/language-server-protocol/)
written in Rust that helps editors display and explain Financial Information
eXchange (FIX) protocol logs.
It provides:

- Inline FIX tag names and decoded enum values
- Hover documentation for fields and messages
- Navigation through nested repeating groups
- Semantic syntax highlighting and message summaries
- Diagnostics and quick fixes
- Support for custom QuickFIX XML dictionaries

**Privacy:** All processing runs locally. No FIX logs are
uploaded to an external service.

> [!NOTE]
> The project is evolving; its features and settings may change.

## Supported logs and FIX versions

Read logs with one FIX message per line, using SOH, pipe (`|`), or caret (`^`)
separators. Log prefixes, blank lines, and `#` comments are supported.

Bundled dictionaries support FIX 4.0–4.4, FIX 5.0/SP1/SP2, and FIXT 1.1.
A single log can mix FIX versions. FIXT application messages need an application
version in the message or configuration.

See [FIX versions](https://github.com/sergluka/venue-reader/wiki/FIX-Versions)
for dictionary selection and
[Inspecting logs](https://github.com/sergluka/venue-reader/wiki/Inspecting-Logs)
for input-format details.

## Inspecting messages

Open a FIX log through your editor's LSP client. This synthetic `Heartbeat`
has a valid body length and checksum:

```text
8=FIX.4.4|9=61|35=0|49=DEMO_SENDER|56=DEMO_TARGET|34=1|52=20260919-12:00:00|10=149|
```

The expandable examples below show server data in text form. Popup layout,
placement and colours depend on the editor.

### Hover

Hover over `MsgType(35)` to see the `Heartbeat` definition.

<details>
<summary>Example: hover popup</summary>

Selected hover content for `MsgType(35)` in the Heartbeat above:

```text
+------------------------------------+
| MsgType (35) - String              |
| Introduced in FIX.2.7              |
| Value: 0 - Heartbeat               |
| Category: Session                  |
+------------------------------------+
```

The full hover also includes field/message descriptions and an optional
reference link.

</details>

### Inlay hints and repeating groups

With tag-name and enum hints enabled, the labels produce
`35(MsgType)=0(Heartbeat)` without editing the file. A field inside a repeating
group can also show a path such as `NoMDEntries/1/MDEntryType`.

<details>
<summary>Example: inline labels and group paths</summary>

The first line shows the Heartbeat. The remaining lines show selected fields
from a synthetic FIX 4.4 market-data snapshot (`MsgType(35) = W`) with two
`NoMDEntries(268)` entries:

```text
35(MsgType)=0(Heartbeat)

269(NoMDEntries/1/MDEntryType)=0(Bid)
270(NoMDEntries/1/MDEntryPx)=100.50
269(NoMDEntries/2/MDEntryType)=1(Offer)
270(NoMDEntries/2/MDEntryPx)=100.75
```

</details>

### Message summaries

Message summaries show time, sequence number, sender, target, and message type.
Choose either or both display channels in `messageSummary.channels`:

- `codeLens`: above each message line; enabled by default. Titles are display-only.
- `inlayHints`: at the start of each message line; disabled by default.

Choose the channel that works best in your preferred editor. Some editors do not
support code lens; others offer better formatting for inlay hints.

<details>
<summary>Example: summary above a message</summary>

The default code-lens title appears above the Heartbeat line. The log line
is shortened here:

```text
20260919-12:00:00: 1: DEMO_SENDER=>DEMO_TARGET | Heartbeat
8=FIX.4.4|9=61|35=0|...|10=149|
```

</details>

### Document outline

The outline shows messages, fields, and repeating-group entries. Select an
entry to navigate to its location. `documentSymbol.depth` accepts `messages`,
`fields`, or `full`; the default is `full`.

<details>
<summary>Example: repeating groups in the outline</summary>

With `documentSymbol.template = "{msgName} ({35})"`, the group portion of
the same market-data snapshot has this shape. Header and trailer fields are
omitted from this excerpt:

```text
MarketDataSnapshotFullRefresh (W)
|-- Symbol (55) = DEMO
`-- NoMDEntries (268) = 2
    |-- [1]
    |   |-- MDEntryType (269) = 0 (Bid)
    |   |-- MDEntryPx (270) = 100.50
    |   `-- MDEntrySize (271) = 10
    `-- [2]
        |-- MDEntryType (269) = 1 (Offer)
        |-- MDEntryPx (270) = 100.75
        `-- MDEntrySize (271) = 12
```

</details>

### Selection and decoding

Selection expansion follows the message structure:
value → field → group entry → group → message line.

`fix.decode` returns annotated text for a selection, such as
`35(MsgType)=0(Heartbeat)`.

### Semantic highlighting

Semantic highlighting separates tags and values and can mark group nesting.
Basic types use standard LSP token names. Types such as `price`, `quantity`,
`timestamp`, and `group` need client/theme mappings.

See [Inspecting logs](https://github.com/sergluka/venue-reader/wiki/Inspecting-Logs) for hover and outline
examples, label formats, and summary templates. See
[Semantic tokens](https://github.com/sergluka/venue-reader/wiki/Semantic-Tokens) for the token legend and
coloring requirements.

## Configuration

Settings apply in this order; later sources override earlier ones:

1. Built-in [defaults](https://github.com/sergluka/venue-reader/wiki/Configuration-Reference#built-in-defaults).
2. Personal `~/.config/venue-reader/config.toml` (or under `$XDG_CONFIG_HOME`).
3. The editor's global LSP `fix` settings.
4. Project `.venue-reader.toml` files, from workspace root to the document.
5. `# fix:` modelines in the first or last five content lines.

For example, turn off the default tag-name and enum hints in your personal file:

```toml
# ~/.config/venue-reader/config.toml
[inlayHints.tagNames]
enabled = false

[inlayHints.enumValues]
enabled = false
```

Select a dictionary for one document with a modeline:

```text
# fix: dictionary=FIX.4.4
```

See [Configuration](https://github.com/sergluka/venue-reader/wiki/Configuration)
for file locations, setting scopes, and reload behavior.

## Build and launch

### Binary installers

The installers download the latest stable prebuilt binary; Rust and Cargo are
not required. For a specific version, replace `latest/download` with
`download/v<version>` in either URL.

| Operating system | Architecture | Release baseline |
| --- | --- | --- |
| Linux | x86-64 | glibc 2.35 or later; excludes Alpine/musl and NixOS. |
| macOS | Intel x86-64, Apple Silicon | macOS 11 or later. |
| Windows | x86-64 | Windows 10 or later. |

Linux and macOS:

```sh
curl --proto '=https' --tlsv1.2 -LsSf -o venue-reader-installer.sh https://github.com/sergluka/venue-reader/releases/latest/download/venue-reader-installer.sh
sh ./venue-reader-installer.sh
```

Windows PowerShell:

```powershell
Invoke-WebRequest -UseBasicParsing -Uri https://github.com/sergluka/venue-reader/releases/latest/download/venue-reader-installer.ps1 -OutFile venue-reader-installer.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\venue-reader-installer.ps1
```

Installation is per user, in `~/.local/bin` on Linux/macOS and
`%USERPROFILE%\.local\bin` on Windows. The installers update your shell
configuration or user PATH. Open a new terminal after installation.
No administrator privileges are needed.

See [installation options](https://github.com/sergluka/venue-reader/wiki/Running-the-Server#installation)
for a custom directory or installation without PATH changes.

The standard cargo-dist shell installer checks archive SHA-256 only when
`sha256sum` is available; otherwise it skips that check. The PowerShell installer
does not check archive hashes. See
[manual archive installation](https://github.com/sergluka/venue-reader/wiki/Running-the-Server#manual-archive-installation)
for explicit checksum verification.

### Installation with cargo-binstall

If you use [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), install
the prebuilt binary from this project's GitHub Release:

```sh
cargo binstall venue-reader --strategies crate-meta-data
```

This strategy excludes third-party quickinstall builds and compilation fallback.

See [release provenance](https://github.com/sergluka/venue-reader/wiki/Running-the-Server#release-provenance)
for artifact attestation verification.

### Cargo installation

Rust 1.91 or later is required only when building from source.
Install from crates.io:

```sh
cargo install venue-reader --locked
```

Cargo installs a release build in `~/.cargo/bin` by default. Add this directory
to `PATH` if needed. `--locked` uses the dependency versions in `Cargo.lock`.

To build from a cloned repository without installation:

```sh
cargo build --release
```

Editor setup depends on the client. This repository does not yet provide
editor-specific installation recipes or an extension. See
[Running the server](https://github.com/sergluka/venue-reader/wiki/Running-the-Server) for more detail.

## Command-line parameters

| Option | Purpose / default |
| --- | --- |
| `--stdio` | Start the server over stdin/stdout; required. |
| `--log-level <level>` | `off`, `error`, `warn`, `info`, `debug`, or `trace`; default `info`. |
| `--log-file <path>` | Append logs to a file; default is stderr. |
| `--version` | Print the version and exit. |
| `--help`, `-h` | Print usage and exit. |

Only stdio transport is supported. Stdout is reserved for LSP traffic.
There is no `--config` option; use the sources in [Configuration](#configuration).

## Custom dictionaries

Load custom QuickFIX XML dictionaries for fields, enum values, messages, and
repeating groups.

**Requires explicit permission:** Set `allowDictionaryPath = true` in personal
TOML or global LSP settings. Loading is disabled by default to prevent untrusted
projects or modelines from loading local XML files; neither can enable it.

Register and select a FIX 4.4 dictionary in a project file:

```toml
# .venue-reader.toml
dictionary = "custom44"

[[dictionaries]]
name = "custom44"
path = "dict/custom-fix44.xml"
```

Relative paths resolve against the project file's directory. See
[Dictionaries](https://github.com/sergluka/venue-reader/wiki/Dictionaries)
for FIX5 bindings, XML requirements, and limits, or
[Venue examples](https://github.com/sergluka/venue-reader/wiki/Venue-Dictionaries)
for recipes based on public documentation.

## Diagnostics and quick fixes

Venue Reader flags version and dictionary problems, unknown messages and tags,
missing envelope fields, and incorrect `BodyLength(9)` or `CheckSum(10)`.
All rules are enabled by default; use `diagnostics.rules` to disable them or
change their severity.

A quick fix repairs length and checksum together while preserving the separator.
Try changing `BodyLength(9)` to `1` and `CheckSum(10)` to `000` in the Heartbeat
above to see it in action.

**Limit:** This is not full FIX schema validation. Other required fields, field
membership, enum values, group-count consistency, and sequence gaps are not
checked yet. See [Diagnostics](https://github.com/sergluka/venue-reader/wiki/Diagnostics) for rule conditions,
codes, and repair behavior.

## Client support and large logs

Features use the client's advertised capabilities. Document symbols require
`hierarchicalDocumentSymbolSupport`; flat `SymbolInformation` lists are not
supported. Quick fixes require code-action literals and versioned document edits.
Custom commands return data that the client must display or use.

Requests are bounded to keep large logs usable. By default, code-lens summaries
stop above 20,000 document lines, and full semantic tokens cover the first
2,000 lines. Range requests let clients request a smaller part of the document.
The outline reduces detail when its budget is exhausted; diagnostics are capped.
For summaries on larger logs, use inlay hints instead of code lens.

See [LSP reference](https://github.com/sergluka/venue-reader/wiki/LSP-Reference) for capabilities and request
requirements, [Settings reference](https://github.com/sergluka/venue-reader/wiki/Configuration-Reference)
for limits, and [Troubleshooting](https://github.com/sergluka/venue-reader/wiki/Troubleshooting) for editor
limitations and missing annotations.

## Commands

Clients invoke these commands through `workspace/executeCommand`:

| Command | Purpose |
| --- | --- |
| `fix.decode` | Decode selections from an open document into annotated text. |
| `fix.describeTag` | Describe a tag and optional enum value from bundled reference data; no open document is required. |
| `fix.useDictionary` | Select a bundled or globally registered dictionary for the session; project/modeline selectors take precedence. |
| `fix.describeOverrides` | Show a document's effective settings, their sources, warnings, and dictionary bindings. |

See
[LSP methods and commands](https://github.com/sergluka/venue-reader/wiki/LSP-Reference#custom-commands)
for request/response examples, arguments, and errors.

## Security and dictionary sources

Hover and resolved hints can include reference links. Opening those links
contacts the website through your editor or browser. Set `documentLink.url = ""`
to remove external reference links.

XML reads are bounded, dictionaries must be regular files, and DTDs are rejected.
**Debug and trace logs may contain JSON-RPC payloads**, including document text.
Treat those logs as potentially sensitive.

Bundled data comes from public QuickFIX/J dictionaries and FIX Trading Community
Orchestra metadata. QuickFIX/J supplies per-version structure; FIX Latest supplies
descriptions, lifecycle metadata, and selected enum values. Dictionaries are
compiled into the binary and are not downloaded at runtime.

See [Security](https://github.com/sergluka/venue-reader/wiki/Security) for trust controls and reference links,
[Dictionary sources](https://github.com/sergluka/venue-reader/wiki/Dictionary-Sources) for the data model,
and [dictionary provenance](https://github.com/sergluka/venue-reader/blob/master/xml/README.md) for pinned sources and regeneration.

## Documentation

The wiki contains the full reference, examples, and troubleshooting guides.
Start at [Wiki home](https://github.com/sergluka/venue-reader/wiki/Home); its navigation groups pages by
getting started, configuration, dictionaries, technical reference, and help.

Release maintainers can use the [release guide](https://github.com/sergluka/venue-reader/blob/master/RELEASING.md).

## Contributing

Use the [bug report form](https://github.com/sergluka/venue-reader/issues/new?template=bug_report.yml)
for reproducible problems. Include a minimal synthetic sample; do not submit
private logs or dictionaries. See the [contributor guide](https://github.com/sergluka/venue-reader/blob/master/CONTRIBUTING.md)
for source requirements and checks.

Report suspected vulnerabilities through the [security policy](https://github.com/sergluka/venue-reader/blob/master/SECURITY.md).

## License

[Apache-2.0](LICENSE). See [NOTICE](NOTICE) and
[THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) for bundled data licenses
and attribution.

This product includes software developed by quickfixengine.org
(<http://www.quickfixengine.org/>).
