# Venue Reader

An editor-independent language server for inspecting FIX protocol logs.
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

See [FIX versions](../venue-reader.wiki/FIX-Versions.md) for identity resolution and
[Inspecting logs](../venue-reader.wiki/Inspecting-Logs.md) for input-format limits.

## Build and launch

### Binary installers

No public release is available yet. The installer commands below apply after
the first stable release is published. Until then, use the source installation below.

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

Release archives (`.tar.gz` for Linux/macOS, `.zip` for Windows) remain available
for manual installation. Extract the archive and put the executable on `PATH`.
Archive SHA-256 files are included in the release assets.

### Cargo installation

Rust 1.91 or later is required only when building from source.
Install from the cloned repository:

```sh
cargo install --path . --locked
```

Or install directly from GitHub:

```sh
cargo install --git ssh://git@github.com/sergluka/venue-reader.git --locked
```

Both commands build from source and do not require a crates.io release.
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
[Running the server](../venue-reader.wiki/Running-the-Server.md) for more detail.

## Inspecting messages

Open a FIX log through your editor's LSP client. This synthetic `Heartbeat`
has a valid body length and checksum:

```text
8=FIX.4.4|9=61|35=0|49=DEMO_SENDER|56=DEMO_TARGET|34=1|52=20260919-12:00:00|10=149|
```

TODO: The screenshot placeholders below will be filled when editor integration is
available.

### Hover

Hover over `MsgType(35)` to see the `Heartbeat` definition.

TODO: Add a screenshot of a field hover with its definition and enum meaning.

### Inlay hints and repeating groups

With tag-name and enum hints enabled, the labels produce
`35(MsgType)=0(Heartbeat)` without editing the file. A field inside a repeating
group can also show a path such as `NoMDEntries/1/MDEntryType`.

TODO: Add a screenshot of tag names, enum labels, and repeating-group paths.

### Message summaries

Message summaries use fields on the line to show time, sequence number,
sender, target, and message type. Code-lens summaries are enabled by default.
Their titles are display-only. Clients that do not show code lens can request
summaries through inlay hints instead.

TODO: Add a screenshot of message summaries above or beside FIX lines.

### Document outline

The outline shows messages, fields, and repeating-group entries. Select an
entry to navigate to its location. `documentSymbol.depth` accepts `messages`,
`fields`, or `full`; the default is `full`.

**Limit:** The server supports hierarchical `DocumentSymbol` results only.
The client must advertise `hierarchicalDocumentSymbolSupport`. Clients that
accept only flat `SymbolInformation` lists are not offered document symbols.

TODO: Add a screenshot of the outline with expanded messages and nested groups.

### Selection and decoding

Selection expansion follows the message structure:
value → field → group entry → group → message line.

`fix.decode` returns annotated text for a selection, such as
`35(MsgType)=0(Heartbeat)`.

TODO: Add a screenshot of a selection and its decoded fields.

### Semantic highlighting

Semantic highlighting separates tags and values and can mark group nesting.
Basic types use standard LSP token names. Types such as `price`, `quantity`,
`timestamp`, and `group` need client/theme mappings.

TODO: Add a screenshot of field highlighting and nested repeating groups.

See [Inspecting logs](../venue-reader.wiki/Inspecting-Logs.md) for hover and outline
examples, label formats, and summary templates. See
[Semantic tokens](../venue-reader.wiki/Semantic-Tokens.md) for the token legend and
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
[TOML for a personal file](../venue-reader.wiki/Configuration-Reference.md#toml-file)
and [JSON for an LSP client](../venue-reader.wiki/Configuration-Reference.md#json-for-lsp-clients).
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

See [Configuration](../venue-reader.wiki/Configuration.md) for source scopes,
project filters, modelines, and reload behavior. The
[Settings reference](../venue-reader.wiki/Configuration-Reference.md) lists all
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

See [Dictionaries](../venue-reader.wiki/Dictionaries.md) for binding forms, accepted
XML, path rules, and loader limits. [Venue examples](../venue-reader.wiki/Venue-Dictionaries.md)
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

For the `Heartbeat` above, change `CheckSum(10)` to `000` to see an integrity
diagnostic. A quick fix repairs both length and checksum together and keeps
the line's separator. Integrity checks for missing envelope fields on any
line with parsed fields. Value calculations and repairs require `BeginString(8)`
and at least two parsed fields; they normalize separators to wire SOH.

TODO: Add a screenshot of integrity diagnostics and the quick-fix action.

Rules can be disabled or assigned a severity through `diagnostics.rules`.
Disabling diagnostics or the integrity rule also disables its quick fix.
Configuration errors remain visible when FIX diagnostics are disabled.

**Limit:** This is not full FIX schema validation. Other required fields, field
membership, enum values, group-count consistency, and sequence gaps are not
checked. See [Diagnostics](../venue-reader.wiki/Diagnostics.md) for rule conditions,
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

See [LSP reference](../venue-reader.wiki/LSP-Reference.md) for capabilities and request
requirements, [Settings reference](../venue-reader.wiki/Configuration-Reference.md)
for limits, and [Troubleshooting](../venue-reader.wiki/Troubleshooting.md) for editor
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
[LSP methods and commands](../venue-reader.wiki/LSP-Reference.md#custom-commands)
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

See [Security](../venue-reader.wiki/Security.md) for trust controls and reference links,
[Dictionary sources](../venue-reader.wiki/Dictionary-Sources.md) for the data model,
and [xml/README.md](xml/README.md) for pinned sources and regeneration.

## Documentation

The wiki contains the full reference, examples, and troubleshooting guides.
Start at [Wiki home](../venue-reader.wiki/Home.md); its navigation groups pages by
getting started, configuration, dictionaries, technical reference, and help.

## Contributing

Issues and pull requests are welcome. Include server/editor versions,
reproduction steps, expected behavior, and a minimal synthetic sample.
Discuss substantial changes before implementation.

Protocol behavior and tests must come from public specifications or independently
created synthetic data. Include source links; do not submit private logs or
dictionaries. See [Contributing](../venue-reader.wiki/Contributing.md) for repository
checks and documentation tests.

## License

[Apache-2.0](LICENSE). See [NOTICE](NOTICE) and
[THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) for bundled data licenses
and attribution.

This product includes software developed by quickfixengine.org
(<http://www.quickfixengine.org/>).
