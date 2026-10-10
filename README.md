# fix-lsp

The project is evolving; its functionality and configuration may change.

An editor-independent language server for inspecting FIX protocol logs.
It resolves fields and enum values against FIX dictionaries, reconstructs
repeating groups, and provides annotations, navigation, and message integrity
checks through the [Language Server Protocol](https://microsoft.github.io/language-server-protocol/).

All analysis runs locally. FIX messages are not uploaded to a decoding service.

- [Functionality](#functionality)
- [Installation](#installation)
- [Running the server](#running-the-server)
- [Configuration](#configuration)
- [Custom dictionaries](#custom-dictionaries)
- [LSP capabilities](#lsp-capabilities)
- [Custom commands](#custom-commands)
- [Semantic tokens](#semantic-tokens)
- [Diagnostics](#diagnostics)
- [Bundled dictionary sources](#bundled-dictionary-sources)
- [Security](#security)
- [Contributing](#contributing)
- [License](#license)

## Functionality

fix-lsp processes one FIX message per line. It accepts SOH, pipe (`|`), and
caret (`^`) separators, and can skip a log prefix before BeginString(8).
Blank lines and `#` comments are supported.

Bundled dictionaries cover FIX 4.0, 4.1, 4.2, 4.3, 4.4, 5.0, 5.0 SP1, and
5.0 SP2. FIXT 1.1 resolves to the bundled FIX 5.0 SP2 dictionary, which includes
the FIXT session definitions. Versions are resolved independently for each
line from BeginString(8), so one log can contain messages from different FIX
versions. Custom XML dictionaries can define their own fields, values,
messages, and groups.

### Inspecting messages

- **Hover:** field name, number, type, description, enum meaning, and available
  introduction/deprecation metadata. MsgType(35) also shows the message
  description and category. Fields inside repeating groups include their path.
  Under a custom dictionary, hover also says when a field is not what the
  version's standard dictionary defines.
- **Inlay hints:** tag names and enum labels alongside the original text.
  Group paths can identify which entry a field belongs to.
- **Message summaries:** configurable summaries above each line through code
  lens, or at the start of the line through inlay hints. Optional details
  describe selected message types using fields on that line.
- **Document symbols (`textDocument/documentSymbol`):** an outline of messages,
  fields, and nested repeating-group entries.
  `documentSymbol.depth` selects `messages`, `fields`, or `full`.
- **Expand selection:** value → field → group entry → group → enclosing
  groups → message line.
- **Decoded selections:** `fix.decode` returns annotated text for an editor
  to display or copy.
- **Diagnostics and quick fixes:** identify missing or incorrect
  BodyLength(9) and CheckSum(10), and repair both together.

### Usage

Open a FIX log through your editor's LSP client with language ID `fix`.
For example, this synthetic Heartbeat has a valid length and checksum:

```text
8=FIX.4.4|9=61|35=0|49=DEMO_SENDER|56=DEMO_TARGET|34=1|52=20260919-12:00:00|10=149|
```

Hover over MsgType(35) to inspect the message definition. Enable tag and enum
hints to display `35(MsgType)=0(Heartbeat)` without changing the file.
Use the outline to navigate messages and groups, or `fix.decode` to obtain
annotated text for a selection. If CheckSum(10) is changed to `000`, the
editor offers an integrity diagnostic and a quick fix.

### Document symbols

The editor's outline exposes each message as a tree. Selecting a message,
field, group, or group entry navigates to its location in the log.
`documentSymbol.depth` controls the detail: `messages` shows message titles,
`fields` adds fields, and `full` nests repeating groups and their entries.

With `documentSymbol.depth = "full"` and
`documentSymbol.template = "{msgName} ({35})"`, a synthetic FIX 4.4
NewOrderSingle excerpt with two parties and one nested party sub-ID produces:

```text
NewOrderSingle (D)
├── BeginString (8) = FIX.4.4
├── MsgType (35) = D (NewOrderSingle)
├── NoPartyIDs (453) = 2
│   ├── [1]
│   │   ├── PartyID (448) = A
│   │   ├── PartyIDSource (447) = D (Proprietary)
│   │   ├── PartyRole (452) = 1 (ExecutingFirm)
│   │   └── NoPartySubIDs (802) = 1
│   │       └── [1]
│   │           ├── PartySubID (523) = S1
│   │           └── PartySubIDType (803) = 1 (Firm)
│   └── [2]
│       ├── PartyID (448) = B
│       ├── PartyIDSource (447) = D (Proprietary)
│       └── PartyRole (452) = 2 (BrokerOfCredit)
└── Side (54) = 1 (Buy)
```

Entry numbers such as `[1]` are local to each group. This feature requires the
LSP client to advertise hierarchical document-symbol support.

### Labels and summaries

`inlayHints.tagNames.format` and `inlayHints.enumValues.format` use `{}`
for the resolved name or label. For example, `"[{}]"` produces `[MsgType]`.
Write literal braces as `{{` and `}}`. An unresolved placeholder suppresses
the label; a format containing only literal text applies to every field.
A tag the selected dictionary does not define, but standard FIX names, is
labelled with the standard name and a `?` mark — `RejectText?` for
RejectText(1328) on a FIX.5.0 line — meaning the name is standard FIX's, not
the dictionary's; the hint's tooltip says which dictionary lacks it and, where
the version has bundled tables, which version has it. A tag neither knows has
no label.
With group paths enabled, a tag name can become
`NoMDEntries/1/MDEntryType`, identifying entry 1 of NoMDEntries(268) and its
MDEntryType(269) field.

Message summaries and outline message titles use a separate template grammar:

| Form | Meaning |
| --- | --- |
| `{35}` | Raw value of MsgType(35). Any numeric tag is accepted. |
| `{35:name}` | Message name for MsgType(35); enum label for other tags, falling back to the raw value. |
| `{52\|???}` | SendingTime(52), or the fallback text when absent or empty. |
| `{35:name\|???}` | Combine a format and fallback. |
| `{msgName}`, `{sender}`, `{target}` | Aliases for the message name, SenderCompID(49), and TargetCompID(56). |
| `{possDup}` | Append `[POSS DUP]` when PossDupFlag(43) is `Y`. |
| `{details}` | Place the available message-specific detail text here. |
| `{{`, `}}` | Literal braces. |

The default summary includes SendingTime(52), MsgSeqNum(34),
SenderCompID(49), TargetCompID(56), and MsgType(35). With
`messageSummary.details = true`, available detail text is appended unless
the template already contains `{details}`.

Detail renderers cover NewOrderSingle, ExecutionReport, Logout, ResendRequest,
and SequenceReset. These summarize fields from an individual message; they do
not reconstruct order lifecycles or correlate session history.

### Hover

Hover over a tag or its value to inspect the field's dictionary definition,
datatype, description, and decoded enum value. Available metadata shows when
the field or value was introduced, and when the field was deprecated or
removed. A field an extension pack introduced is dated by the release that
shipped that pack: `FIX.5.0 EP55` reads FIX.5.0SP1. A value's date is the one
FIX Latest records, which for a value an extension pack added is the release
the pack applied to. Fields inside repeating groups include their group path;
MsgType(35) also includes the message description and category.

For example, hovering over `35=0` (MsgType(35)) in the Heartbeat above displays:

> **MsgType** (35) — String
>
> Defines message type ALWAYS THIRD FIELD IN MESSAGE. (Always unencrypted)
>
> *Introduced in FIX.2.7*
>
> ---
>
> Value: `0` — **Heartbeat**
>
> *Introduced in FIX.2.7*
>
> The Heartbeat monitors the status of the communication link and identifies when the last of a string of messages was not received.
>
> *Category: Session*
>
> ---
>
> [FIX Reference: onixs.biz](https://www.onixs.biz/fix-dictionary/4.4/tagNum_35.html)

The popup's appearance depends on the editor and theme. Reference links can
be disabled or redirected through `documentLink.url`.

Under a custom dictionary, hover also says how each field stands against
standard FIX for the line's version, in one line:

| Field | Note |
| --- | --- |
| Standard in this version | *Standard FIX.4.4* |
| Standard, but added later | *Not in FIX.4.4, introduced in FIX.5.0SP1* |
| Standard, but spelled differently by the dictionary | *Not in FIX.4.2, introduced in FIX.4.3 as "SecurityListRequestType"* |
| Retired from the standard | *Not in FIX.4.4, retired after FIX.4.3* |
| Not standard at all | *Custom field, introduced in "desk"* |

Versions are FIX releases; an extension pack only says how a field reached one.
A custom field shows no introduction or deprecation dates, and under a "Not in"
note the dictionary's own dates are left out. Only a field standard in that
version links into the reference site's pages for it. Where the two sources
disagree about a field, hover says nothing about it;
[Standard status](#standard-status) explains how this is decided.

A tag the selected dictionary does not define is still named and placed
against standard FIX when FIX Latest or a bundled version knows it: `1328=…`
on a FIX.5.0 line reads **RejectText** (1328), *Not in FIX.5.0, introduced in
FIX.5.0SP1*, and the raw value. Under a custom dictionary a line above the
note says the dictionary lacks it — *Not defined by "desk"*. Nothing else is
claimed: no datatype, description or lifecycle line, and a versioned link only
where the tag is standard in the line's version. A tag neither source knows,
or one the sources dispute, has no hover; the `unknownTag` diagnostic
([Diagnostics](#diagnostics)) still says the dictionary lacks it.

For a tag no line carries, [`fix.describeTag`](#fixdescribetag) answers from
the newest version that defines it, with that tag's lifespan in place of a
note about any one version.

### Large logs and editor support

Requests are bounded to keep large logs usable. The default limits are listed
under [Configuration](#default-settings).

- Code lens returns no summaries for documents above `limits.codeLensLines`.
  For larger logs, use `messageSummary.channels.codeLens = false` and
  `messageSummary.channels.inlayHints = true` to request summaries by range.
- Full semantic highlighting covers only the first
  `limits.semanticTokensFullLines` lines. Range-capable clients can request
  other portions of the document.
- The outline reduces detail when its symbol budget is exhausted, then
  truncates message entries with a marker indicating how many remain.
- Diagnostics are capped, with a final entry reporting omitted diagnostics.

Features depend on the client's advertised capabilities. In particular, a
hierarchical outline requires hierarchical document-symbol support; quick
fixes require versioned document edits. Custom token colors depend on the
editor's theme configuration.

The project has recorded a Helix compatibility limitation: unavailable code
lens and clipped line-start inlay summaries, while tag and enum hints remained
usable. Recheck summary display against the installed Helix version; the server
has no editor-specific workaround. Helix 25.07 also discards the result of a
`workspace/executeCommand`, so `fix.decode` and `fix.describeTag` have no
visible output there — an accepted limitation.

## Installation

TODO: provide installation instructions and a configuration or add-on for each
editor. The first candidates are:

- **Neovim:** configuration for the built-in LSP client.
- **VS Code:** an extension connecting FIX files to the server and declaring
  custom semantic token types.

## Running the server

Build from source with Rust 1.91 or later:

```sh
cargo build --release
./target/release/fix-lsp --stdio
```

The binary is a standalone LSP process. An editor or another LSP client starts
it and exchanges framed JSON-RPC messages over stdin/stdout. It does not accept
raw FIX logs on stdin as a command-line decoder. Only stdio transport is
supported; there is no TCP listener.

Once the binary is on `PATH`:

```sh
fix-lsp --stdio --log-level debug --log-file /tmp/fix-lsp.log
fix-lsp --version
fix-lsp --help
```

| Option | Purpose / default |
| --- | --- |
| `--stdio` | Required to run the LSP server. |
| `--log-level <level>` | `off`, `error`, `warn`, `info`, `debug`, or `trace`; default `info`. |
| `--log-file <path>` | Append server logs to a file; default is stderr. |
| `--version` | Print the version and exit. |
| `--help`, `-h` | Print usage and exit. |

Stdout is reserved for LSP traffic. There is no `--config` option; settings
come from the sources below.

## Configuration

Settings use the same canonical names across LSP, TOML, and modelines. Only
explicitly supplied values override lower-priority values, including when an
explicit value equals a built-in default. Objects merge per setting;
dictionary declarations accumulate subject to name and version-default rules.

### Sources and precedence

From lowest to highest priority:

| Priority | Source | Scope |
| --- | --- | --- |
| 1 | Built-in defaults | All documents. |
| 2 | Personal TOML (user config directory) | Personal defaults for the server user. |
| 3 | Global LSP `fix` settings | The editor's server session. |
| 4 | Matching `.fix-lsp.toml` files | Workspace root to document directory; nearer files override ancestors. |
| 5 | Document modelines | Settings embedded in edge comments. |

The `fix.useDictionary` command changes only the session's dictionary selection.
Project configuration and modelines take precedence over this selection; the
command does not change any other settings.

Project discovery starts at the longest containing workspace folder, falling
back to a containing `rootUri`. Without either, only the document's directory
is searched. Non-file buffers use global settings and the session selection,
without project discovery or modelines.

### LSP settings

Supply settings under `initializationOptions.fix` when starting the server.
For push updates, use `workspace/didChangeConfiguration` with this
`settings` object:

```json
{
  "fix": {
    "inlayHints": {
      "tagNames": { "enabled": true },
      "enumValues": { "enabled": true }
    },
    "messageSummary": {
      "channels": { "codeLens": false, "inlayHints": true }
    }
  }
}
```

If the client advertises `workspace.configuration`, the server requests
`workspace/configuration` on initialization and configuration changes,
using `{"section":"fix"}` without `scopeUri`. The response is a one-element
array containing the section object itself, without a second `fix` wrapper.

Use nested JSON objects. Literal dotted JSON keys such as
`"inlayHints.tagNames.enabled"` are not expanded.

### Personal and project TOML

Personal settings live in `fix-lsp.toml` inside the server user's
configuration directory: `$XDG_CONFIG_HOME/fix-lsp.toml`, or
`~/.config/fix-lsp.toml` when that variable is unset, empty, or relative.

```toml
# ~/.config/fix-lsp.toml
[inlayHints.tagNames]
enabled = true

[inlayHints.enumValues]
enabled = true
```

Project files are named `.fix-lsp.toml` and may be placed in nested directories,
not just at the workspace root. For example, `logs/archive/.fix-lsp.toml` can
override settings from `logs/.fix-lsp.toml` and the root `.fix-lsp.toml` for
documents under `logs/archive/`. Matching configurations are merged from root
to leaf; a nested file overrides only the settings it supplies.

Example configuration:

```toml
# .fix-lsp.toml
files = ["*.fix", "logs/**/*.fix"]

[inlayHints.tagNames]
format = "[{}]"

[messageSummary.channels]
codeLens = false
inlayHints = true
```

The optional `files` array filters documents relative to that configuration
file. Patterns are case-sensitive and anchored; `*` does not cross directory
separators, while `**` does. Omit `files` to apply settings to all descendant
documents. Empty arrays, absolute patterns, and `..` components are rejected.
A nonmatching configuration does not prevent other ancestors or descendants
from contributing settings.

TOML has no format-version field. Unknown server keys reject the file's
settings. Known editor-owned settings are ignored; duplicate TOML keys are
invalid. See the [personal schema](schema/fix-lsp-user.schema.json) and
[project schema](schema/fix-lsp-project.schema.json).

### Modelines

Place `# fix:` comments within the first or last five content lines.
A terminal newline does not add a line to that scan:

```text
# fix: inlayHints.tagNames.enabled=true: inlayHints.tagNames.format="[{}]":
# fix: dictionary=FIX.4.4 diagnostics.rules.integrity.severity=warning
```

Assignments use `key=value`, separated by whitespace or colons. Quote values
containing spaces or colons; `\:` also escapes a colon. A trailing colon is
allowed. Only comment lines are scanned, never FIX payload fields.
The `fix: set` form is not supported.

`unset` removes that modeline setting and reveals the lower-priority value;
`"unset"` is a literal string. All scanned modelines form one scope:
duplicate keys reject that key's modeline override and report an error.
Other valid assignments remain usable. TOML has no `unset` value;
remove the key instead.

### Setting scope

| Settings | Personal TOML / global LSP | Project TOML | Modelines |
| --- | --- | --- | --- |
| `hover.*`, `semanticTokens.*`, `inlayHints.*`, `messageSummary.*`, `documentSymbol.*`, `diagnostics.*` | Yes | Yes | Yes |
| `defaultVersion`, `allowDictionaryPath`, `documentLink.url`, `overrides.*`, `limits.*` | Yes | No | No |
| `dictionaries[]` | Yes | Yes | No |
| `dictionary` selector | Session command only | Yes | Yes |
| `files` filter | No | Yes | No |

Global-only settings in project files or modelines are rejected with a
configuration warning. A document cannot grant itself dictionary file access
or redirect reference links.

### Default settings

These are the complete built-in configuration values, expressed as the LSP
`fix` section. TOML and modelines inherit the same defaults for settings
allowed in their scope; they do not have separate default sets. Keep actual
configuration files sparse so higher layers only override intended settings.

```json
{
  "allowDictionaryPath": false,
  "defaultVersion": "FIX.4.4",
  "dictionaries": [],
  "overrides": { "projectConfig": true, "modeline": true },
  "hover": { "enabled": true },
  "semanticTokens": { "enabled": true },
  "inlayHints": {
    "tagNames": { "enabled": false, "format": "({})" },
    "enumValues": { "enabled": false, "format": "({})" },
    "groupPaths": { "enabled": true }
  },
  "messageSummary": {
    "enabled": true,
    "template": "{52|???}: {34|???}: {49|???}=>{56|???} | {35:name|???}{possDup}",
    "details": true,
    "channels": { "codeLens": true, "inlayHints": false }
  },
  "documentSymbol": {
    "enabled": true,
    "depth": "full",
    "template": "#{34} {49}→{56} {35:name} ({35})"
  },
  "documentLink": {
    "url": "https://www.onixs.biz/fix-dictionary/{version}/tagNum_{tag}.html"
  },
  "diagnostics": { "enabled": true, "rules": {} },
  "limits": {
    "dictionaryBytes": 16777216,
    "diagnosticsPerDocument": 10000,
    "rangeLines": 2000,
    "codeLensLines": 20000,
    "documentSymbolItems": 50000,
    "semanticTokensFullLines": 2000,
    "cachedLines": 4000
  }
}
```

An empty `diagnostics.rules` uses each rule's defaults: all six rules are
enabled, with severities listed under [Diagnostics](#diagnostics).
`inlayHints.groupPaths.enabled` changes tag labels; it produces no hints
while tag-name hints are disabled.

There is no explicit `dictionary` selection by default: BeginString(8)
selects the version, using `defaultVersion` when it cannot.
`files` has no filter by default. A dictionary entry defaults to
`default = false`; `name` and `path` are required.

| Limit | Effect |
| --- | --- |
| `dictionaryBytes` | Maximum bytes per external XML file; default 16 MiB. |
| `diagnosticsPerDocument` | Maximum FIX diagnostics per document. Filled by severity: a less severe problem never keeps a more severe one out; within one severity the first in line order are kept. |
| `rangeLines` | Maximum lines for hints, range tokens, and code actions; total selected lines for `fix.decode`; distinct cursor lines for selection expansion. |
| `codeLensLines` | Maximum document size for code-lens summaries. |
| `documentSymbolItems` | Outline symbol budget, including fields and groups. |
| `semanticTokensFullLines` | Prefix covered by full/delta semantic tokens. |
| `cachedLines` | Target bound on retained line analyses per open document. |

Above `limits.rangeLines`, hints and range tokens return no data; explicitly
requested code actions and `fix.decode` fail rather than return partial
results. Automatic code-action requests return no actions. Selection expansion
still answers every cursor, but returns whole-line ranges only.

The [global settings schema](schema/fix-lsp.schema.json) describes accepted
values. `documentSymbol.depth` accepts `messages`, `fields`, or `full`;
diagnostic severity accepts `error`, `warning`, `info`, or `hint`.
`defaultVersion` must resolve to a bundled or configured version.

### Applying changes and troubleshooting

There is no filesystem watcher or reload command. Personal TOML is read at
startup and global configuration refresh. Project TOML is checked on document
open, configuration changes, and workspace-folder changes. Modelines are
reevaluated on edits; those edits do not reload unchanged TOML files.
Send a configuration-change notification or restart the server after editing
configuration or dictionary files.

Use `fix.describeOverrides` to inspect committed effective settings and
their winning sources. It does not reload anything.

Updates commit atomically. Existing documents retain their previous state
while an update is pending. A new document's configuration-dependent requests
can return `RequestFailed` until its first valid configuration commits.
Annotation refresh requests are sent only when the client supports them.

Configuration errors remain visible even when FIX diagnostics are disabled.
Unknown rule IDs are reported and omitted; valid sibling settings still apply.
Malformed TOML rejects that file's layer; invalid LSP settings preserve the
last committed configuration. Unknown global JSON keys are not treated like
unknown TOML keys, so use the schema to catch misspellings.

Missing optional configuration files are normal. Missing configured XML,
permission errors, other configuration I/O failures, and a failed LSP
configuration request are fatal. A configuration batch also has a 500 ms
deadline covering client response, file loading, and preparation; exceeding
it terminates the server. Check paths, file access, and client responsiveness
before restarting.

## Custom dictionaries

Custom dictionaries are standalone, not overlays on bundled dictionaries.
Fields, enum values, and message or group structures missing from a custom
dictionary are not inherited from a bundled one. Include the standard
definitions you need alongside your custom definitions.

Hover compares each field of a custom dictionary with standard FIX for the
line's version and marks the ones that version does not define; see
[Hover](#hover).

### Supported format

A custom dictionary is one QuickFIX XML file rooted at `<fix>`: fields,
enums, header and trailer with their own groups, messages, components, and
nested groups, with component references expanded. A directory is refused.

The dictionary is read as the whole truth about what it declares. A message
or group listed with no members is complete and empty, not missing
information; an empty `<group/>` is still a group, so its counter is marked
as a count. The first member of a group defines its entry delimiter.

A dictionary with one of these defects is refused. The error starts with the
file's path and says where the defect is. Other dictionaries in the same
configuration still load.

| Defect | Error |
|---|---|
| Malformed XML | the XML parser's message |
| No `<fix>` root, or no `<fields>` section | `expected <fix> root element`, `expected <fields> root element` |
| A field number that is not a number | `tag is not a number: "abc"` |
| A field numbered 0 or 4294967295 | `field A has number 0, outside the tag range 1 to 4294967294` |
| A group counter or component that the file does not define, referenced from any component, used or not | `component AttrbGrp refers to group counter "NoInstrAttrib", which the file does not define` |
| A component that contains itself | `component A contains itself (A → B → A)` |
| A message without `msgtype`, without `name`, or without both | `message NewOrderSingle has no msgtype`, `the message with MsgType(35) = D has no name`, `a message has neither a name nor a msgtype` |
| No `<messages>` section, or an empty one | `declares no messages` |
| Components and groups nested deeper than 32 levels | `component/group nesting deeper than 32` |
| A dictionary that expands to more than 1 048 576 member declarations | `expands to more than 1048576 member declarations` |
| More than 65 535 fields, messages or groups, or as many members or component rows in one scope | `too many fields (more than 65535)` |
| More than 65 535 enum values in one field | `field 35 has too many enum values (more than 65535)` |

A dictionary with one of these defects loads, and each defect is reported.
It is an error when the server drops or chooses something the file says,
and a warning when nothing the file says is lost:
`dictionary "x.xml" loaded with an error: …` or
`dictionary "x.xml" loaded with a warning: …`. Up to 20 are listed, errors
first, then `… and N more`, as severe as the worst one it counts.

| Defect | What the server does | Severity | Message |
|---|---|---|---|
| A reference to a field that the file does not define, from any component, used or not | Skips the reference | Error | `message MarketDataRequest refers to field "PriceAggregation", which the file does not define; skipped` |
| A field name defined with two numbers | The name refers to the last number; both tags stay defined | Error | `field TargetStrategy is defined with two numbers, TargetStrategy(847) and TargetStrategy(50001); the name refers to TargetStrategy(50001)` |
| A message type defined more than once | Merges every definition, named after the first | Warning under one name, error when a definition has another name | `message NewOrderSingle, MsgType(35) = D, is defined twice; merged into one`, `message NewOrderSingle, MsgType(35) = D, is defined 3 times, also as OrderSingle, NOS; merged into one under NewOrderSingle` |
| A tag listed twice in one header, trailer, message or group entry, directly or through components | Keeps one member at the first position, with the strictest declaration's requiredness, group and components; a field marked `repeating="Y"` may be listed again, identically, without a report | Warning when every declaration is the same, error when they differ | `message RegistrationInstructions, MsgType(35) = o, lists OwnershipType(517) twice; merged into one` |
| A group counter listed in one scope both as a plain field and as a group | Keeps the group, with its requiredness and components, at the first position | Warning | `message ExecutionReport, MsgType(35) = 8, lists NoPartyIDs(453) as a plain field and as a group; the group is kept` |

One discrepancy loads without a report: a tag number defined under two
names keeps the last definition, and both names resolve.

The FIX version comes from XML metadata, for example
`<fix type="FIX" major="4" minor="4">`. It is not inferred from filenames,
and there is no `dictionaries[].version` option. Direct loading of FIX
Orchestra XML is not supported; Orchestra is used to build bundled metadata.

A working synthetic example is
[included](xml/custom/synthetic/oe-fix44.xml).

### Registering and selecting dictionaries

First enable `allowDictionaryPath` in personal TOML or global LSP settings.
The following is a global LSP `fix` section:

```json
{
  "allowDictionaryPath": true,
  "dictionaries": [
    {
      "name": "custom44",
      "path": "/path/to/custom-fix44.xml",
      "default": true
    }
  ]
}
```

Every definition requires a nonblank, unique `name` and a `path`.

`default = true` makes a dictionary the fallback for messages of its
declared FIX version. Registration alone does not replace the bundled
dictionary: `default` is false unless specified.

To keep multiple dictionaries available and select one per project:

```toml
# .fix-lsp.toml
dictionary = "custom44"

[[dictionaries]]
name = "custom44"
path = "dict/custom-fix44.xml"

[[dictionaries]]
name = "venue44"
path = "dict/venue-fix44.xml"
```

This is an alternative to global registration, not an additional declaration
of the same names. Names must be unique across the applicable global/project
chain and cannot shadow bundled names. Within one array, duplicate defaults
for the same version are errors. A stronger layer can replace a version
default with a differently named dictionary.

The same definition arrays are supported in personal TOML, but a top-level
`dictionary` selector is not. Selection can come from project TOML, a
modeline such as `# fix: dictionary=custom44`, or `fix.useDictionary` for a
globally registered name. Bundled names such as `FIX.4.4` always select
bundled data even when a custom version default exists.

A project or modeline selector can also be a path when file access is enabled.
Absolute paths and prefixes `./`, `../`, or `~/` identify path selectors;
other strings are dictionary names. Modelines cannot declare dictionary
arrays. `fix.useDictionary` accepts names only and does not load XML.

Selection does not change the message version. The selected dictionary must
match the version resolved from BeginString(8); FIX 5.0 SP2 dictionaries also
accept FIXT 1.1 messages. A mismatch falls back to the resolved version's
default dictionary and is reported by `dictionaryVersion`.
Selecting a FIX 4.2 dictionary does not change the default version of a
fragment without BeginString(8).

### Path resolution

| Source | Relative dictionary paths resolve against |
| --- | --- |
| Personal/project TOML definition | The directory containing that TOML file. |
| Modeline path selector | The document's directory. |
| Global LSP definition | The initialization root: first workspace folder, otherwise `rootUri`. |

Only a leading `~/` is expanded. There is no shell, environment-variable,
or glob expansion. Global relative paths without an initialization root are
rejected; the process working directory is not used as a fallback.
Paths refer to the server's filesystem.

## LSP capabilities

| Method | Function / requirement |
| --- | --- |
| `textDocument/didOpen`, `didChange`, `didClose` | Incremental document synchronization. |
| `textDocument/hover` | Dictionary descriptions and field context. |
| `textDocument/inlayHint`, `inlayHint/resolve` | Labels, group paths, summaries, and resolved hover content. |
| `textDocument/codeLens`, `codeLens/resolve` | Message-summary titles. Disabled if the client explicitly cannot resolve the `command` property. |
| `textDocument/selectionRange` | Structural selection expansion. |
| `textDocument/documentSymbol` | Nested outline; requires `hierarchicalDocumentSymbolSupport`. |
| `textDocument/codeAction` | Integrity quick fixes; requires code-action literals and `workspaceEdit.documentChanges`. |
| `textDocument/semanticTokens/range` | Advertised when the client declares range requests. |
| `textDocument/semanticTokens/full`, `textDocument/semanticTokens/full/delta` | Advertised instead of range mode when range requests are not declared. |
| `textDocument/publishDiagnostics` | Server-pushed diagnostics. |
| `workspace/executeCommand` | The four commands below. |
| `workspace/configuration`, `workspace/didChangeConfiguration` | Global settings retrieval and refresh. |
| `workspace/didChangeWorkspaceFolders` | Update project configuration discovery. |

The server negotiates UTF-8 positions when supported and otherwise uses
UTF-16. It supports the standard initialization/shutdown lifecycle,
`$/cancelRequest`, `$/setTrace`, and diagnostic work-done progress.
It requests inlay-hint, code-lens, and semantic-token refreshes only when
supported by the client.

There is no completion, formatting, rename, definition/reference navigation,
pull diagnostics, or `source.fixAll`. Despite the `documentLink.url`
setting name, `textDocument/documentLink` is not a capability: reference
links are embedded in hover and hint content.

## Custom commands

Commands are invoked through `workspace/executeCommand`. The examples below
are its `params` objects; document URIs must refer to open documents.

### `fix.decode`

Arguments: `[uri, ranges]`, with a nonempty array of LSP ranges.
Returns a single string of decoded fields:

```json
{
  "command": "fix.decode",
  "arguments": [
    "file:///work/example.fix",
    [
      {
        "start": { "line": 0, "character": 15 },
        "end": { "line": 0, "character": 19 }
      }
    ]
  ]
}
```

For the Heartbeat example, this selects MsgType(35) and returns
`35(MsgType)=0(Heartbeat)` with the default label formats.
Output uses `|` between fields and newlines between lines.

Every field touched by a selection is returned in full, even if the selection
covers only part of its tag or value.

Ranges are processed independently in the supplied order; overlapping ranges
repeat output. An empty range selects the field under the cursor. Send
multiple ranges for rectangular or multicursor selections. The total selected
line count must fit `limits.rangeLines`; oversized selections fail rather
than return partial text.

The command uses tag/enum label formats and group-path settings regardless
of whether the corresponding inlay hints are enabled. A tag the dictionary
does not define but standard FIX names carries the same marked name the hint
draws: `1328(RejectText?)=…`.

### `fix.describeTag`

Arguments: `[tag]` or `[tag, value]` — a tag number, and optionally one value
of it as a string. Returns the text hover would show for that tag, as one
markdown string:

```json
{ "command": "fix.describeTag", "arguments": [150, "F"] }
```

No document is involved, so the command works with none open: a client may
start the server, send `initialize` and `initialized`, and ask. In Neovim,
`vim.lsp.start(config, { attach = false })` starts such a client and
`client:exec_cmd()` sends the command.

The command names no version, because the question is what a tag means rather
than whether some version has it. The newest bundled version that defines the
tag answers — the current meaning, and the fullest code set — and the answer
carries that tag's own lifespan: when it entered the protocol, when it was
deprecated, and for a tag the protocol dropped, the version that removed it.
Hover's "not in this version" note belongs to a line and is not shown here.
Neither the session's dictionary selection nor `fix.defaultVersion` is
consulted, and only bundled tables are read, so a custom dictionary's own
definitions never answer. The reference link points at the pages of the version
that answered.

With a value, hover's value paragraph for it follows: the enum label, its
description and date, and for MsgType the message it names. A tag no bundled
version defines but FIX Latest knows is answered with its name and when it
arrived, which is all that source carries.

A tag no bundled version and not FIX Latest defines fails with
`fix.describeTag: no bundled dictionary defines tag N`. Until the client's
configuration has been answered and committed the command fails with
`configuration is not ready, retry later`, because the reference link is
configurable; a client that fires it on `initialized` retries.

### `fix.useDictionary`

Arguments: one bundled or committed global dictionary name:

```json
{ "command": "fix.useDictionary", "arguments": ["FIX.4.4"] }
```

Returns `{name, version, source}` and selects that dictionary for the session,
below project and modeline selectors. Personal and global LSP definitions are
available; project-only names and paths are not. The command selects from the
committed registry without reading or reloading XML.

The choice survives configuration updates while its name remains registered.
Removing that name clears the selection; adding it back does not restore the
old choice. Restarting also clears it.

### `fix.describeOverrides`

Arguments: one open document URI:

```json
{
  "command": "fix.describeOverrides",
  "arguments": ["file:///work/example.fix"]
}
```

Returns:

| Field | Contents |
| --- | --- |
| `effective` | Effective settings under canonical dotted keys, including defaults. |
| `sources` | Winning layer and source for each key; modelines also include the assignment's LSP range. |
| `warnings` | Applicable configuration warnings. |
| `dictionaries` | Available dictionary definitions and their source information. |

It reports the last committed configuration, including while an update is
pending. A document with no committed configuration returns `RequestFailed`.
The response can contain canonical filesystem paths, but not document or XML
contents.

## Semantic tokens

The server emits token types and modifiers; the client chooses their visual
appearance. BeginString(8) and MsgType(35) have dedicated classifications.
Other values use a known enum label first, then the dictionary's datatype.

### Standard types

| Type | Applied to |
| --- | --- |
| `property` | Tag numbers. |
| `namespace` | BeginString(8) values. |
| `keyword` | MsgType(35) values. |
| `enumMember` | Values resolved to enum labels. |
| `number` | Numeric values, including lengths, sequence numbers, and group counts. |
| `string` | Text, character, boolean, raw-data, and otherwise unclassified values. |
| `comment` | Comment lines and log prefixes. |

### Custom types

| Type | Applied to |
| --- | --- |
| `price` | Price values. |
| `quantity` | Quantity values. |
| `money` | Amount and price-offset values. |
| `timestamp` | Date and time values. |
| `group` | An overlay spanning a repeating-group entry. |

Custom types need client/theme mappings. Do not assume that a theme's standard
numeric or string styling will automatically apply to them.

### Modifiers and repeating groups

| Modifier | Meaning |
| --- | --- |
| `deprecated` | Standard modifier: the dictionary marks the field deprecated. |
| `header`, `trailer` | Field belongs to the dictionary's session header or trailer. |
| `session` | Administrative message. |
| `possDup` | PossDupFlag(43) is `Y`. |
| `unknown` | Field is absent from the selected dictionary. |
| `count` | Repeating-group counter, including a zero count. |
| `group1a`, `group1b` | First nesting level, alternating by entry. |
| `group2a`, `group2b` | Second nesting level. |
| `group3a`, `group3b` | Third and deeper nesting levels. |

Every modifier except `deprecated` is custom. Group fields carry their
entry's modifier on both tag and value tokens. Whole-entry `group` overlays
are emitted only when the client advertises `overlappingTokenSupport`.
Style either those overlays or the individual group-modified fields to avoid
applying the same group background twice.

Group structure comes from the message definition, including expanded
components, and from the header and trailer: a group declared there, such as
NoHops(627), is reconstructed like a message group. When the message and the
header declare a group with the same counter, the message's group is used.

Range requests above `limits.rangeLines` return empty token data and a log
message. Full/delta requests cover a bounded prefix, controlled by
`limits.semanticTokensFullLines`. A range-only client may not provide
whole-file minimap coloring. `semanticTokens.enabled = false` disables token
output without reconnecting.

## Diagnostics

Six FIX rules are implemented. All are enabled by default:

| Rule | Default severity | Diagnostic code(s) | Condition |
| --- | --- | --- | --- |
| `beginString` | `warning` | `beginString` | Recognized fields without BeginString(8). |
| `unknownVersion` | `warning` | `unknownVersion` | BeginString(8) cannot select its declared version; decoding uses the fallback. |
| `dictionaryVersion` | `warning` | `dictionaryVersion` | The selected dictionary is incompatible with the resolved message version. Reported once per mismatching version in a document. |
| `integrity` | `error` | `bodyLength`, `checkSum` | Missing or incorrect BodyLength(9) or CheckSum(10). |
| `unknownMsgType` | `info` | `unknownMsgType` | The line's dictionary does not list its MsgType(35). |
| `unknownTag` | `info` | `unknownTag` | The line's dictionary does not define a tag. Every occurrence is reported. |

Each `diagnostics.rules.<rule>` accepts a boolean or an object with
`enabled` and `severity`. For example:

```toml
# .fix-lsp.toml
[diagnostics.rules]
beginString = false

[diagnostics.rules.integrity]
enabled = true
severity = "warning"
```

Integrity checks require BeginString(8). They normalize separators to wire
SOH and exclude log prefixes and trailing non-field text from the calculation.
A quick fix corrects or inserts both dependent values together, preserves the
line's separator, and uses a versioned edit. Disabling diagnostics or the
integrity rule also disables its quick fix.

`unknownTag` and `unknownMsgType` state what the line's dictionary lacks, by
its configured name (else its file name) or, for bundled tables, by version: `FIX.4.4 does not
define tag 9000`, `"desk" does not define MsgType(35) = U1`. After a tag
standard FIX knows comes how it stands there, in hover's words:
`FIX.5.0 does not define RejectText(1328); introduced in FIX.5.0SP1`. Both
run only on a line whose dictionary was chosen — by its BeginString(8), or,
on a fragment without one, by an explicit selection that decoded it. A line
decoded by fallback, such as `8=FIX.9.9`, is not checked against it.

Known limits:

- A `8=FIXT.1.1` message is always checked as FIX.5.0SP2; ApplVerID(1128)
  is not consulted. The bundled
  FIX.5.0 and FIX.5.0SP1 tables have no session layer, so under them
  `unknownMsgType` stays silent and session fields such as HeartBtInt(108)
  are reported by `unknownTag`.
- A custom dictionary is taken as the full list of messages, session
  messages included. One that declares only application messages reports
  every session message, such as MsgType(35) = A, as unknown; a custom FIX 5
  dictionary cannot yet be paired with a separate session dictionary.

This is not full FIX schema validation: there are no rules for required
fields, tags not allowed in their message or group, enum values, group-count
consistency, or sequence gaps, and a tag repeated outside a group is not
reported. Configuration errors are reported separately from these rules and
are not suppressed by `diagnostics.enabled`.

## Bundled dictionary sources

The dictionaries are built from public sources and compiled into the binary:

| Source | Contribution | License |
| --- | --- | --- |
| [QuickFIX/J](https://github.com/quickfix-j/quickfixj) | Per-version fields, messages, header/trailer definitions, repeating groups, and declared enum values. | QuickFIX Software License 1.0. |
| [FIX Trading Community Orchestra](https://github.com/FIXTradingCommunity/orchestrations) | Descriptions, enum labels, lifecycle metadata, and selected missing enum values. | Apache-2.0, © FIX Protocol Ltd. |

QuickFIX/J defines which fields, messages, and groups belong to each version.
The bundled FIXT 1.1 session definitions are folded into the FIX 5.0 SP2
tables. Metadata is projected from the FIX Latest Orchestra file; descriptions
therefore come from that source rather than an exact historical manual for
every version.

Enum enrichment adds codes from a field's own code set only when Orchestra
dates them to that version or earlier and does not attribute them to an
extension pack. Shared code sets only relabel existing values. MsgType(35)
is never extended beyond the version's message definitions.
QuickFIX/J values already present remain, even where they reflect a later
extension pack.

Pinned commits, source hashes, projection rules, and regeneration instructions
are in [Bundled dictionary data](xml/README.md). Licensing and attribution are
preserved in [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) and
[NOTICE](NOTICE). Dictionaries are not downloaded at runtime.

### Standard status

The two sources disagree in places, and neither is always right. FIX Latest
says what is standard FIX; the QuickFIX/J files say what each version
contained. Hover states only what both support or what only one could know, and
says nothing where they disagree:

- 4,751 extension-pack tags are only in FIX Latest: standard, newer than any
  bundled dictionary.
- 40 retired tags are only in QuickFIX/J: standard once, removed since.
- 6 tags in the user-defined range (5000–39999) are only in QuickFIX/J's FIX 5.0
  file: not standard. They read *Custom field* even under the bundled FIX 5.0
  dictionary.
- 59 (version, tag) pairs are dated differently by the two: no claim is made,
  and 42 tags show no introduction date.
- Enum values: QuickFIX/J lists them unevenly and the bundled tables complete
  them from FIX Latest in 114 (version, field) places, so no claim is made about
  values yet.

Examples and how the counts are pinned are in
[Bundled dictionary data](xml/README.md#standard-status--two-witnesses).

## Security

### Offline analysis and reference links

The server works entirely offline. Parsing, dictionary lookup, group
reconstruction, annotations, and diagnostics run in the local process.
fix-lsp does not upload FIX logs to external services for decoding and makes
no outbound network requests.

Hover content and resolved hints can contain a link to the OnixS FIX
reference. The server constructs the link locally; it does not query OnixS.
Opening the link through the editor or browser contacts that website.
The default URL contains only the FIX version and tag number, not message
values or the log contents.

To remove external reference links while keeping hover information, configure:

```toml
# ~/.config/fix-lsp.toml
[documentLink]
url = ""
```

Alternatively, set `documentLink.url` to your own HTTP(S) reference template.
Supported substitutions are `{tag}`, `{version}` (OnixS-style version
segment: `4.4`, `5.0`, `5.0.SP1`, `5.0.SP2`), and `{fixVersion}` (the full FIX
version). Other URL schemes are rejected. Project files and modelines cannot
change this global setting.

A template that uses `{version}` points into one version's pages, so a tag is
linked only when the bundled dictionary for that version defines it and FIX
Latest does not dispute it. Any other template links every tag, including a
custom dictionary's own. The default site publishes 4.0 to 5.0.SP2. A desk
whose own template uses `{version}` loses links on its custom tags; use
`{fixVersion}` for those.

For fully offline operation, keep configuration and dictionary files on local
storage and disable external reference links. Editor extensions and
network-mounted filesystems have their own network behavior.

### Dictionary file access

To ignore configuration supplied by a project or log, set
`overrides.projectConfig = false` and/or `overrides.modeline = false` in
personal TOML or global LSP settings. These independently disable project TOML
discovery and modeline overrides. They do not disable personal/global settings
or replace the separate permission for external XML dictionaries below.

`allowDictionaryPath` defaults to **false**. It gates every external XML
dictionary load: personal/global definitions, project declarations, and direct
project/modeline path selectors. Bundled dictionaries need no permission.

Set it to `true` only in personal TOML or global LSP settings when you trust
the project configuration and document modelines. Project files and modelines
cannot enable it themselves. The permission is global: it is not a directory
allowlist or an administrator-enforced sandbox.

Disabling it removes external dictionary bindings and selections without
reading their XML files. Registered definitions can become effective again
after a configuration refresh that re-enables access. The setting does not
prevent reading TOML configuration files.

Paths are canonicalized, dictionary input must be a regular file, and DTDs
are rejected. XML reads are bounded by `limits.dictionaryBytes` per file.
Configuration TOML has a fixed 1 MiB limit. FIFOs and devices are refused.

### Local logging

Logs go to stderr unless `--log-file` is supplied. New log files are created
with mode `0600` on Unix. Document text and JSON-RPC bodies are not logged
below `trace`; treat trace logs as potentially sensitive.
The client can enable trace logging through `$/setTrace`.

## Contributing

Issues and pull requests are welcome. Include the server/editor versions,
reproduction steps, expected behavior, and a minimal synthetic sample.
Discuss substantial changes before implementation.

Protocol behavior, dictionaries, and test cases must come from public
specifications or independently created synthetic data. Include source links
for protocol changes; do not submit private logs or dictionaries.
Keep server behavior independent of editor identity and adapt through
advertised LSP capabilities.

Run the repository checks before submitting changes:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 -m unittest discover -s tools
```

Run `cargo deny check` after dependency changes.

## License

[Apache-2.0](LICENSE). See [NOTICE](NOTICE) and
[THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) for bundled data licenses
and attribution.

This product includes software developed by quickfixengine.org
(<http://www.quickfixengine.org/>).
