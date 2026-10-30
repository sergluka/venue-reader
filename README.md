# Venue Reader

An language server for inspecting FIX (Financial Information eXchange®) protocol logs.
It explains fields and values, reconstructs repeating groups, and provides
annotations, navigation, and diagnostics through the
[Language Server Protocol](https://microsoft.github.io/language-server-protocol/).

All analysis runs locally. FIX logs are not uploaded to a decoding service.

> [!NOTE]
> The project is evolving; its features and settings may change.

- [Features](#features)
- [Build and launch](#build-and-launch)
- [Inspecting messages](#inspecting-messages)
- [Basic configuration](#basic-configuration)
- [Custom dictionaries](#custom-dictionaries)
- [Diagnostics and quick fixes](#diagnostics-and-quick-fixes)
- [Documentation](#documentation)

## Features

| Feature | What it provides |
| --- | --- |
| Inlay hints | Tag names and enum labels beside the original text, with optional repeating-group paths. |
| Semantic highlighting | Distinguish tags, enum values, datatypes, session fields, and repeating groups. |
| Message summaries | Configurable message titles above each line through code lens, or at the line start through inlay hints. |
| Document outline | Messages, fields, and nested repeating-group entries as a navigation tree. |
| Hover | Field name, type, description, enum meaning, and available lifecycle metadata. `MsgType(35)` also shows the message definition. |
| Selection expansion | Expand from a value to its field, group entry, enclosing groups, and message line. |
| Decoded selections | Return annotated fields for a client to display or copy. |
| Diagnostics and repairs | Report version and dictionary problems; check and repair `BodyLength(9)` and `CheckSum(10)` together. |

## Supported logs and FIX versions

Venue Reader reads one FIX message per line. It accepts SOH, pipe (`|`), and caret
(`^`) separators, can skip a log prefix before `BeginString(8)`, and ignores
blank lines and `#` comments. The parser selects one separator per line.

Bundled dictionaries cover FIX 4.0–4.4, FIX 5.0/SP1/SP2, and standalone FIXT 1.1.
Each line resolves its own transport and application, so one log can contain
messages from different FIX versions.

For conventional FIX4 messages, `BeginString(8)` selects the dictionary.
FIX5 application messages use `BeginString(8) = FIXT.1.1`, with
`ApplVerID(1128)` identifying the application release. For example,
`ApplVerID(1128) = 9` means FIX 5.0 SP2. If the message omits that field, select
a complete dictionary pair or configure `defaultApplVerID`. There is no built-in
application default, and the server does not infer one from another log line.

See [FIX versions](https://github.com/sergluka/venue-reader/wiki/FIX-Versions) for identity resolution and
[Inspecting logs](https://github.com/sergluka/venue-reader/wiki/Inspecting-Logs) for input-format limits.

## Build and launch

### Binary installers

The installers download a prebuilt binary. Rust and Cargo are not required.
The URLs select the release marked Latest and do not include drafts or
prereleases. For a specific version, replace `latest/download` with
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

Install into a custom directory without modifying PATH or shell configuration:

```sh
export VENUE_READER_UNMANAGED_INSTALL="$PWD/venue-reader-bin"
sh ./venue-reader-installer.sh
```

```powershell
$env:VENUE_READER_UNMANAGED_INSTALL = Join-Path $PWD 'venue-reader-bin'
powershell -NoProfile -ExecutionPolicy Bypass -File .\venue-reader-installer.ps1
```

Configure your LSP client with the installed executable's full path, or add the
directory to PATH yourself.

The standard cargo-dist shell installer checks archive SHA-256 only when
`sha256sum` is available; otherwise it skips that check. The PowerShell installer
does not check archive hashes. Use manual archive verification below when you
want an explicit checksum check.

### Manual archive installation

After a release, download the archive for your platform and its matching
`.sha256` file from the same [GitHub Release](https://github.com/sergluka/venue-reader/releases).
Verify the archive before extracting it.

Linux:

```sh
sha256sum --check venue-reader-x86_64-unknown-linux-gnu.tar.gz.sha256
```

macOS (Apple Silicon; use `x86_64-apple-darwin` for Intel):

```sh
shasum -a 256 --check venue-reader-aarch64-apple-darwin.tar.gz.sha256
```

Windows PowerShell:

```powershell
(Get-FileHash .\venue-reader-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash
```

Compare the Windows hash, ignoring letter case, with the first value in the
matching `.sha256` file. After verification, extract the archive (`.tar.gz` on
Linux/macOS, `.zip` on Windows) and put the executable on `PATH`, or configure
your LSP client with its full path.

### Installation with cargo-binstall

If you use [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), install
the prebuilt binary from this project's GitHub Release:

```sh
cargo binstall venue-reader --strategies crate-meta-data
```

This strategy excludes third-party quickinstall builds and compilation fallback.

### Release provenance

For a release built with GitHub artifact attestations, verify the downloaded
archive with the [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify venue-reader-x86_64-unknown-linux-gnu.tar.gz --repo sergluka/venue-reader
```

Use your platform's archive name. Review the reported source revision and release
workflow. This verifies the recorded provenance and digest; it does not audit the
code. Keep checksum verification for manual downloads.

### Cargo installation

Rust 1.91 or later is required only when building from source.
Install from crates.io:

```sh
cargo install venue-reader --locked
```

Install from the cloned repository:

```sh
cargo install --path . --locked
```

Or install directly from GitHub:

```sh
cargo install --git https://github.com/sergluka/venue-reader.git --locked
```

The local and GitHub commands do not require a crates.io release.
Cargo installs a release build in `~/.cargo/bin` by default. Add this directory
to `PATH` if needed. `--locked` uses the dependency versions in `Cargo.lock`.

Check the installation:

```sh
venue-reader --version
```

To build and run without installation:

```sh
cargo build --release
./target/release/venue-reader --stdio
```

Once the binary is on `PATH`, the launch command is:

```sh
venue-reader --stdio
```

| Option | Purpose / default |
| --- | --- |
| `--stdio` | Start the server over stdin/stdout; required. |
| `--log-level <level>` | `off`, `error`, `warn`, `info`, `debug`, or `trace`; default `info`. |
| `--log-file <path>` | Append logs to a file; default is stderr. |
| `--version` | Print the version and exit. |
| `--help`, `-h` | Print usage and exit. |

Only stdio transport is supported. Stdout is reserved for LSP traffic.
There is no `--config` option; use the configuration sources below.

Editor setup depends on the client. This repository does not yet provide
editor-specific installation recipes or an extension. See
[Running the server](https://github.com/sergluka/venue-reader/wiki/Running-the-Server) for more detail.

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

Message summaries use fields on the line to show time, sequence number,
sender, target, and message type. Code-lens summaries are enabled by default.
Their titles are display-only. Clients that do not show code lens can request
summaries through inlay hints instead.

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

**Limit:** The server supports hierarchical `DocumentSymbol` results only.
The client must advertise `hierarchicalDocumentSymbolSupport`. Clients that
accept only flat `SymbolInformation` lists are not offered document symbols.

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

<details>
<summary>Example: selection expansion and decoded text</summary>

With the cursor on the `0` in `MsgType(35)`, successive selection ranges are:

```text
0                                value
35=0                             field
8=FIX.4.4|9=61|35=0|...|10=149|    message line (shortened)

fix.decode on the selected field:
35(MsgType)=0(Heartbeat)
```

</details>

### Semantic highlighting

Semantic highlighting separates tags and values and can mark group nesting.
Basic types use standard LSP token names. Types such as `price`, `quantity`,
`timestamp`, and `group` need client/theme mappings.

<details>
<summary>Example: semantic token classifications</summary>

The server supplies token classifications; the editor chooses their colours.
The last two rows come from the grouped market-data example:

```text
Field                     Tag token  Value token
35=0                      property   keyword
34=1                      property   number
49=DEMO_SENDER            property   string
52=20260919-12:00:00      property   timestamp
270=100.50                property   price
271=10                    property   quantity
```

</details>

See [Inspecting logs](https://github.com/sergluka/venue-reader/wiki/Inspecting-Logs) for hover and outline
examples, label formats, and summary templates. See
[Semantic tokens](https://github.com/sergluka/venue-reader/wiki/Semantic-Tokens) for the token legend and
coloring requirements.

## Basic configuration

Settings apply from lowest to highest priority:

1. Built-in defaults.
2. Personal `config.toml`: `$XDG_CONFIG_HOME/venue-reader/config.toml`, or
   `~/.config/venue-reader/config.toml` if that variable is unset, empty, or relative.
3. Global LSP `fix` settings supplied by the editor.
4. Matching project `.venue-reader.toml` files, from workspace root to the document.
5. Document modelines: `# fix:` comments in the first or last five content lines.

Higher layers override only the settings they supply. Project files can be
nested; a nearer file can override an ancestor. Keep configuration files sparse.

The wiki has the complete default configuration in
[TOML for a personal file](https://github.com/sergluka/venue-reader/wiki/Configuration-Reference#toml-file)
and [JSON for an LSP client](https://github.com/sergluka/venue-reader/wiki/Configuration-Reference#json-for-lsp-clients).
Tag-name and enum hints are **on by default**. For example, this small personal
configuration turns them off:

```toml
# ~/.config/venue-reader/config.toml
[inlayHints.tagNames]
enabled = false

[inlayHints.enumValues]
enabled = false
```

A modeline can select a dictionary for one document:

```text
# fix: dictionary=FIX.4.4
```

File access, reference URLs, override controls, and resource limits are
personal/global settings. Project files and modelines cannot change them.
There is no filesystem watcher for TOML or dictionary files: send a
configuration-change notification or restart after changing those files.

See [Configuration](https://github.com/sergluka/venue-reader/wiki/Configuration) for source scopes,
project filters, modelines, and reload behavior. The
[Settings reference](https://github.com/sergluka/venue-reader/wiki/Configuration-Reference) lists all
defaults and resource limits.

## Custom dictionaries

Custom dictionaries use QuickFIX XML. They can define their own fields, enum
values, messages, and repeating groups. They do not inherit missing definitions
from bundled dictionaries.

**Requires explicit permission:** Set `allowDictionaryPath = true` in personal
TOML or global LSP settings before loading external XML. This permission is
false by default and cannot be enabled by a project or document.

For a conventional FIX 4.4 dictionary, register and select it in a project file:

```toml
# .venue-reader.toml
dictionary = "custom44"

[[dictionaries]]
name = "custom44"
path = "dict/custom-fix44.xml"
```

The path is relative to that project file. Registration and selection are
separate: `name` identifies the binding, and `dictionary` selects it. Selecting
a dictionary still checks the message's version.

FIX5 dictionaries may need separate transport and application files. A combined
FIXT-rooted file needs an explicit application release. Use the form that
matches the actual XML, not its filename or venue name.

See [Dictionaries](https://github.com/sergluka/venue-reader/wiki/Dictionaries) for binding forms, accepted
XML, path rules, and loader limits. [Venue examples](https://github.com/sergluka/venue-reader/wiki/Venue-Dictionaries)
show publisher-backed recipes for specific interfaces; they are not venue
compatibility certifications.

## Diagnostics and quick fixes

All five FIX rules are enabled by default:

| Rule | What it checks | Default severity |
| --- | --- | --- |
| `unknownVersion` | Missing, invalid, or unresolved transport/application identity. | Warning |
| `dictionaryVersion` | A selected dictionary conflicts with the message's version. | Warning |
| `integrity` | Missing `BeginString(8)`, `BodyLength(9)`, or `CheckSum(10)`; incorrect length or checksum. | Error |
| `unknownMsgType` | Missing `MsgType(35)`, or a message the dictionary does not define. | Error |
| `unknownTag` | A tag the resolved dictionary does not define. | Info |

For the `Heartbeat` above, change `BodyLength(9)` to `1` and `CheckSum(10)` to
`000` to see both integrity diagnostics. A quick fix repairs both values together
and keeps the line's separator. Integrity checks for missing envelope fields on any
line with parsed fields. Value calculations and repairs require `BeginString(8)`
and at least two parsed fields; they normalize separators to wire SOH.

<details>
<summary>Example: diagnostics and one repair</summary>

```text
Before (line shortened):
8=FIX.4.4|9=1|35=0|...|10=000|

BodyLength is 1, expected 61
CheckSum is 000, expected 149

Quick fix: Fix BodyLength and CheckSum

After (line shortened):
8=FIX.4.4|9=61|35=0|...|10=149|
```

</details>

Rules can be disabled or assigned a severity through `diagnostics.rules`.
Disabling diagnostics or the integrity rule also disables its quick fix.
Configuration errors remain visible when FIX diagnostics are disabled.

**Limit:** This is not full FIX schema validation. Other required fields, field
membership, enum values, group-count consistency, and sequence gaps are not
checked yet. See [Diagnostics](https://github.com/sergluka/venue-reader/wiki/Diagnostics) for rule conditions,
codes, and repair behavior.

## Client support and large logs

Features use the client's advertised capabilities. Document symbols require
hierarchical support; flat symbol lists are not supported. Quick fixes require
code-action literals and versioned document edits. Custom commands return data
that the client must display or use.

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

These commands do not reload configuration files. See
[LSP methods and commands](https://github.com/sergluka/venue-reader/wiki/LSP-Reference#custom-commands)
for arguments, result formats, and errors.

## Security and dictionary sources

The server analyzes logs offline. It can include reference links in hover and
resolved hints. Opening those links contacts the website through your editor
or browser. Set `documentLink.url = ""` to remove external reference links.

External dictionaries require the global permission described above. XML reads
are bounded, dictionaries must be regular files, and DTDs are rejected.
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
