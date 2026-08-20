# fix-lsp

An editor-independent [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
server for **FIX protocol** logs. It decodes tag/value fields, message summaries,
groups, and validation diagnostics for any LSP client (Neovim, VS Code, Helix,
Zed, JetBrains via LSP4IJ).

## Status

Pre-1.0. Stage 1 provides the transport, main loop, incremental document sync,
configuration pull, and a hover stub. Stage 2 adds the dictionaries:
QuickFIX/J's FIX.4.0 – FIX.5.0SP2 dictionaries, with descriptions from FIX
Orchestra, compiled into the binary (one merged tag table with exact
per-version views; `FIXT.1.1` resolves to `FIX.5.0SP2`, which also carries the
FIXT.1.1 session layer — see [where the dictionaries come
from](#where-the-dictionaries-come-from) and [bundled dictionary
data](xml/README.md)),
plus custom QuickFIX or Repository XML dictionaries configured through
`fix.dictionaries` (`path` and `name` required, plus optional
`mode: auto|quickfix|repository` and `default`). The XML is the only source of
the dictionary's FIX version. `<component>` references inside QuickFIX messages
are expanded, so groups defined in components are recognised. `default` is off unless
asked for: registering a dictionary does not by itself change how every log of
its version is read. Several venues share a FIX version but not its
user-defined tag space, and only one dictionary can answer for that version --
so mark that one `default: true` and leave the rest optional, to be selected
per document by name.
Stage 3 adds the codec. A line is tokenized, decoded against the dictionary
its BeginString selects, its repeating groups and group paths reconstructed,
and its BodyLength and CheckSum recomputed from the bytes; the result is cached
per line, dropped when the dictionaries or `defaultVersion` change, and swept
past `fix.limits.cachedLines` (4000 lines, about 1.8 MB) so a long scroll does
not keep the whole file analysed. Message
summaries render from the `fix.messageSummary.template` grammar.

Stage 4a-1 turns that into the first thing a user sees. Opening a FIX log
publishes diagnostics for it: a BodyLength or CheckSum that disagrees with the
bytes, a line that carries no BeginString and so is not a FIX message at all,
and a BeginString no dictionary answers for. They are computed on a background
thread, re-published as you type, and capped by
`fix.limits.diagnosticsPerDocument` with one entry saying how many were left
out. Each rule can be switched off or regraded under `fix.diagnostics.rules`.

Stage 4a-2 makes those diagnostics actionable. Put the cursor on a line whose
BodyLength or CheckSum disagrees with its bytes, invoke code actions, and one
entry corrects it -- inserting a missing tag with the separator that line
actually uses, and carrying the document version so it cannot be applied to
text that has moved. A client that cannot take a versioned code-action literal
is offered no provider at all rather than an edit it did not agree to.

Stage 4a-3 finishes Stage 4a with hover. Put the cursor on a tag or its value
and get what the dictionary knows: the field's name, type and description, the
enum label and what it means, the message description and category for
MsgType(35),
the group path when the field sits inside a repeating group, when the tag
entered the protocol and when it left it, and a link to the reference -- which
is `fix.documentLink.url`, so it can point at your own dictionary browser
instead. Hovers are asserted whole, not in fragments: the order of the
paragraphs is as much the contract as their text.

Stage 4b draws the dictionary's answer beside the bytes. With
`fix.inlayHints.tagNames` and `fix.inlayHints.enumValues` switched on, every tag
is labelled with its name -- or with its full path when it sits inside a
repeating group -- and every value with its enum label, and hovering the hint
gives the same markdown hover does.

Both labels are yours to write. `fix.inlayHints.tagNames.format` and
`.enumValues.format` are literal text plus a nameless hole, `{}`, which is the
tag's name for the first and the value's meaning for the second. The hole needs
no name because each setting draws one thing, and writing it twice simply
repeats the value. An empty hole draws nothing at all, so a tag the dictionary
does not define is passed over rather than labelled `()`; by the same rule a
format with no hole, such as `"!"`, is drawn on every field. Write a literal
brace by doubling it.

The one-line message summary rides one of
two channels, selected by `fix.messageSummary.channels`: a code lens above the
line, or an inlay hint at its start. Both are bounded -- the inlay channel by
the range in its request, the lens channel by lazy resolve over a list that only
classifies lines -- so neither reads the whole document to answer. A
configuration change that alters what they would say asks the client to
re-request them.

Two limitations worth knowing before you switch a channel on. **Code lens does
not scale**: `CodeLensParams` carries no range, so the list is whole-document by
protocol and past `fix.limits.codeLensLines` the answer is empty rather than
slow. And **in Helix the summary has no working channel**: Helix implements no
code lens, and an inlay hint at the start of a line is clipped to its last few
characters there with no way to scroll to it. Tag and value hints work normally
in Helix; the summary is for VS Code and Neovim.

Stage 4c-1 adds **expand-selection** that knows the shape of a line: from
inside a value it grows to the value, then the whole `tag=value`, then the line.

Stage 4c-2 adds the **outline**: one entry per message, its fields under it,
and a repeating group as a list of entries holding their own fields, nested as
deep as the message goes. `fix.documentSymbol.depth` chooses how far down it
goes -- `messages`, `fields` or `full` -- and `fix.documentSymbol.template`
names the message row. The same stage gives expand-selection the levels it was
missing: a field inside a repeating group now grows through its entry, then the
whole group, then its parent's, before reaching the line.

An outline is counted in symbols, and a log carries far more of them than it
carries lines: a 10 000-line log at 62 fields a line is 642 000 symbols, which
no editor will draw. So the answer degrades by steps rather than emptying --
every message first, then fields while `fix.limits.documentSymbolItems` lasts,
then a final entry saying how many messages were left out, and one log line
naming the setting that would show more.

The repository ships no broker dictionaries;
[custom dictionary fixtures](xml/custom/README.md) explain how to run the
parser over ones you download yourself.

Stage 5 adds personal and project TOML configuration, comment modelines,
per-document dictionary selection, and commands for selecting a session
dictionary and inspecting the effective settings. The editor and server run
on the same host: the server reads configuration and XML files itself. Clients
provide global LSP settings, not discovered or merged TOML files.

## Colours

Highlighting is sent as semantic tokens: the server names a type and a set of
modifiers for each range, and the client decides what colour that is. Two tags
are read for what they are rather than for their datatype, because a QuickFIX
dictionary invents an enum for every message type and would otherwise leave
nothing spelled as a keyword.

Seven of the token types are the standard LSP names, so they colour under any
theme:

| type | what carries it |
| --- | --- |
| `property` | every tag |
| `namespace` | the value of BeginString(8), the FIX version |
| `keyword` | the value of MsgType(35), the message type |
| `enumMember` | a value the dictionary resolves to a named constant |
| `number` | a sequence number, a length, a percentage, a group counter |
| `string` | text, characters, booleans and raw data |
| `comment` | a comment line, and a log prefix before the first tag |

Five are our own, because no standard name describes them. A theme has to map
these before they take a colour of their own; until it does they keep whatever
the editor's grammar already gave them.

| type | what carries it | from |
| --- | --- | --- |
| `price` | a price per unit | `Price` |
| `quantity` | a number of units | `Qty` |
| `money` | an amount rather than a level: a total, or a difference read against a price | `Amt`, `PriceOffset` |
| `timestamp` | any date or time | `UTCTimestamp`, `LocalMktDate`, and the rest |
| `group` | one whole entry of a repeating group | — |

The last of those is the only token that overlaps another, and it is sent only
to a client that says it can render overlapping tokens. **A group entry's own
fields carry the same group modifier the overlay does, so colour one of the two
and not both.**

The modifiers say what the dictionary knows beyond the type. `deprecated` is
the standard name; the rest are FIX facts.

| modifier | meaning |
| --- | --- |
| `deprecated` | the dictionary marks the tag deprecated |
| `header`, `trailer` | the tag belongs to the session layer, as that dictionary declares it |
| `session` | the whole line is administrative traffic rather than an order |
| `possDup` | the line repeats a message sent before |
| `unknown` | the tag is not in the dictionary the line selected |
| `count` | the field counts the entries of a repeating group |
| `group1a` … `group3b` | nesting depth, alternating between two markers so adjacent entries differ |

Groups nested deeper than three share the deepest pair of markers. One
limitation: a repeating group declared inside a QuickFIX `<header>` marks its
fields as header fields, but is not recognised as a group, because groups are
read only from message definitions.

**Mapping the custom names.** In an editor whose highlight groups are named
after the legend, a rule per name is enough — `price`, `quantity`, `money`,
`timestamp` and `group`, optionally with a modifier, and each is addressable
together with a modifier as well. In VS Code the five are unknown types, so an
extension has to declare each with a `superType` a theme already colours
(`number` for the first three, `string` for `timestamp`, `variable` for
`group`); without that declaration a theme cannot reach them.

**How much is coloured.** Which of two requests a client gets depends on what it
says it will send. A client that asks about a viewport is answered about a
viewport, bounded by `fix.limits.rangeLines` (2000), and past that bound the
answer is empty with one line in the log rather than a shorter answer it could
not tell from an uncoloured file. A client that asks about the whole document is
answered about the first `fix.limits.semanticTokensFullLines` lines (2000) and
then keeps up through edits. That number is set by the client rather than by the
server: colouring a whole 10 000-line log takes half a second here, but the
2000-line answer is already 255 000 tokens, and a client rebuilds every one of
them per response. `fix.semanticTokens.enabled` switches the whole thing off
without a reconnect.

## Commands

The server advertises exactly three commands over `workspace/executeCommand`:
`fix.decode`, `fix.useDictionary`, and `fix.describeOverrides`.

`fix.decode` takes two arguments -- a document URI and a list of ranges -- and
answers with the decoded text as a single string: every selected field as
`tag(Name)=value(Label)`, fields joined by `|` and lines by newlines. Names and
enum labels are written by the same two format settings the inlay hints use, so
a decoded field reads the way the hint beside it does. The hint *switches* do
not reach here: how a field is spelled is one choice, whether it is drawn on
screen is another, and a client that asked for a decode and got the bytes back
unchanged was given nothing.

Each range is read the way LSP means a range: from the start position to the
end position, so the first line is taken from its start column, the last to its
end column, and the ones between whole. There is no mode argument. A rectangle
is the one selection two positions cannot describe, so a blockwise selection is
sent as one range per line, and so is a multi-cursor selection. Ranges answer
independently and in the order given; an overlap decodes twice, because merging
them would be a guess about what was selected. An empty range is a cursor and
answers with the field under it.

A selection over `fix.limits.rangeLines` in total is refused rather than
truncated: the answer is headed for a register or a clipboard, where half a
selection is worse than being told the selection is too big.

`fix.useDictionary` takes one non-empty dictionary name. It sets a session-wide
choice below project/modeline selectors, not `defaultVersion`:

```json
{"command":"fix.useDictionary","arguments":["venue-a"]}
```

Names come from bundled, personal, or global LSP definitions; project-only
names are unavailable to this URI-less command. Before the first global
configuration commits, only bundled names are available. The command selects
an entry from the last committed global registry: it does not read or reload
XML and does not accept paths. A representative result is:

```json
{"name":"venue-a","version":"FIX.4.4","source":"/work/dict/venue-a.xml"}
```

The result's `name` is always a string. The choice survives global settings
updates while the name remains registered. If a later update removes the name,
the server clears the choice and reports a warning; adding it back does not
restore the old choice. Restart also clears it.

`fix.describeOverrides` takes one open document URI:

```json
{"command":"fix.describeOverrides","arguments":["file:///work/logs/orders.fix"]}
```

It returns `effective` (all settings under canonical dotted keys, including
defaults and global controls), `sources` (the winning source for every key),
`warnings`, and the applicable `dictionaries`. For example, `effective` may contain
`"inlayHints.tagNames.enabled": true` and `"dictionary": "venue-a"`.
Each `sources` entry contains its layer and source; a modeline source also has
the exact LSP `range` of the assignment. TOML sources identify the file rather
than an approximate key position.
The command returns data for the client to display; no editor-specific API is
needed. It never reads files, reloads dictionaries, applies configuration, or
scans document text. During an update it returns the last committed state;
before a document has usable configuration it returns `RequestFailed`.
Canonical local dictionary paths are visible, but document text and XML
contents are not. Normal analysis-cache eviction after requests is unaffected.

## Configuration

All settings are optional. The schemas describe [global LSP settings](schema/fix-lsp.schema.json),
[personal TOML](schema/fix-lsp-user.schema.json), and
[project TOML](schema/fix-lsp-project.schema.json); the
[settings catalog](src/overrides/catalog.rs) defines known paths and their scope.
There is no system/admin configuration file or `--config` option.

Settings merge from weakest to strongest:

1. Built-in defaults.
2. Personal `~/.config/fix-lsp.toml`.
3. Global LSP `fix` settings.
4. `fix.useDictionary` session choice, for the dictionary selector only.
5. Matching project `.fix-lsp.toml` files, workspace root to document directory.
6. Modelines in the document's first and last five lines.

Only explicit values override weaker settings, even when equal to defaults.
Removing a value reveals the weaker one; an empty object does not erase its
unspecified children. There is no scoped LSP configuration or
`fix.documentOverrides`. Non-file buffers use only the global base and session
choice, without project discovery or modelines.

### Personal defaults and global LSP settings

Create `~/.config/fix-lsp.toml`, if desired:

```toml
[inlayHints.tagNames]
enabled = true

[diagnostics.rules.integrity]
severity = "warning"
```

The location is resolved from the server user's home directory, not the
workspace or working directory. It is independent of project discovery and
still loads when `overrides.projectConfig = false`. No TOML file has a
format-version field: it must match the current server's schema. Unknown server
keys reject the entire file with an Error; known editor-owned keys are ignored,
and schema-permitted opaque rule options are preserved.

The client supplies global settings under `fix` in `initializationOptions` or
`workspace/didChangeConfiguration.settings`. For example:

```json
{"fix":{"inlayHints":{"tagNames":{"enabled":false}}}}
```

This overrides the personal tag-name gate but retains the personal integrity
severity. If the client supports `workspace/configuration`, the server pulls
only `{"section":"fix"}`, without `scopeUri`, on `initialized` and
`didChangeConfiguration`. Its response item is the section itself, without a
second `fix` wrapper. JSON uses nested properties; literal dotted properties,
old aliases, and `fix_*` names are not expanded. Unknown global JSON keys are
preserved, unlike unknown TOML keys.

### Project configuration

Each directory can contain `.fix-lsp.toml`; matching files merge per setting:

```text
work/
├── .fix-lsp.toml
└── logs/
    └── venue-a/
        ├── .fix-lsp.toml
        └── orders.fix
```

For example, the root file enables tag labels and selects a summary channel:

```toml
[inlayHints.tagNames]
enabled = true

[messageSummary.channels]
codeLens = false
inlayHints = true
```

The child file changes only the label format for matching files:

```toml
files = ["*.fix"]

[inlayHints.tagNames]
format = "[{}]"
```

`orders.fix` inherits the enabled labels and summary channel, with `[Name]`
labels. Without `files`, a file applies to every descendant document. When
present, `files` is a non-empty list of anchored, case-sensitive globs relative
to that configuration's directory: `*` and `?` do not cross `/`, while `**`
does. Absolute patterns and escaping `..` are rejected. A non-matching file
does not stop ancestor/child merging.

Discovery starts at the longest containing workspace folder (or a containing
`rootUri`). Without either boundary, only the document's directory is checked.
Symlinked roots and unsaved paths use canonical ancestor containment. Workspace
folder changes update discovery; they do not change the initialization root
used by relative global dictionary paths.

### Modelines

Only `#` comment lines in the first/last five lines are scanned. Interior blank
lines count; the synthetic empty line after a final newline does not. Short
documents are scanned without counting overlapping windows twice.

```text
# fix: inlayHints.tagNames.enabled=true: inlayHints.tagNames.format="[{}]":
# fix: dictionary=FIX.4.4 diagnostics.rules.integrity.severity=warning
```

The `fix:` marker must start the comment body or follow a space/tab. Assignments
use canonical dotted keys, separated by whitespace or colons. Quoted strings
can contain whitespace and colons; `\:` escapes a literal colon. Empty segments
and a trailing colon are ignored. Only this form is supported, not `fix: set`.
Markers inside FIX payloads are never settings. `unset` removes a modeline
candidate to reveal a weaker value; `"unset"` is an ordinary string. TOML has
no `unset` extension: remove the key instead.

Duplicate setting keys at one level are errors even when equal. A TOML file
is one level and native duplicate keys reject it. All scanned modelines form
one level: a duplicate rejects that setting's candidates across those lines,
while unrelated settings remain usable. Different files can override ordinary
settings without being duplicates.

### Settings and scope

| Settings | Personal TOML / global LSP | Project TOML / modeline |
| --- | --- | --- |
| `inlayHints.tagNames.{enabled,format}`, `inlayHints.enumValues.{enabled,format}`, `inlayHints.groupPaths.enabled` | yes | yes |
| `messageSummary.{enabled,template,details}`, `messageSummary.channels.{codeLens,inlayHints}` | yes | yes |
| `diagnostics.enabled`, `diagnostics.rules.*` | yes | yes |
| `hover.enabled`, `semanticTokens.enabled` | yes | yes |
| `documentSymbol.{enabled,depth,template}` | yes | yes |
| `defaultVersion`, `documentLink.url`, `allowDictionaryPath` | yes | no |
| `limits.*`, `overrides.modeline`, `overrides.projectConfig` | yes | no |
| `dictionaries[]` | yes, path permission required | project TOML only, path permission required |
| `dictionary` selector | session command only | yes |

A project cannot grant itself filesystem access or redirect reference links.
Attempts to set global-only controls there produce configuration warnings and
leave weaker values in force. Switching a source off removes its values and
diagnostics and stops scanning/discovery for it. Presentation changes cannot
enable a provider or refresh method the client did not negotiate.

Editor-owned `render.summary.position`, `render.group.highlight.{enabled,target}`,
`formatter.*`, `annotate.title.position`, and `annotate.group.highlight.enabled`
are silently ignored and omitted from effective-settings reports. How an
editor displays these is entirely its choice.

### Default values

```json
{
  "defaultVersion": "FIX.4.4",
  "allowDictionaryPath": false,
  "dictionaries": [],

  "hover":          { "enabled": true },
  "semanticTokens": { "enabled": true },
  "documentSymbol": { "enabled": true, "depth": "full",
                      "template": "#{34} {49}→{56} {35:name} ({35})" },

  "inlayHints": {
    "tagNames":   { "enabled": false, "format": "({})" },
    "enumValues": { "enabled": false, "format": "({})" },
    "groupPaths": { "enabled": true }
  },

  "messageSummary": {
    "enabled": true,
    "channels": { "codeLens": true, "inlayHints": false },
    "template": "{52|???}: {34|???}: {49|???}=>{56|???} | {35:name|???}{possDup}",
    "details": true
  },

  "diagnostics": {
    "enabled": true,
    "rules": { "beginString": true, "unknownVersion": true,
               "dictionaryVersion": { "enabled": true, "severity": "warning" },
               "integrity": { "enabled": true, "severity": "error" } }
  },

  "documentLink": { "url": "https://www.onixs.biz/fix-dictionary/{version}/tagNum_{tag}.html" },

  "overrides": { "modeline": true, "projectConfig": true },

  "limits": {
    "rangeLines": 2000, "semanticTokensFullLines": 2000, "codeLensLines": 20000,
    "documentSymbolItems": 50000, "diagnosticsPerDocument": 10000,
    "dictionaryBytes": 16777216, "cachedLines": 4000
  }
}
```

Two hints are off by default because they double the width of every line; the
summary is on. A section written but left empty keeps its own default rather
than the type's, so `"tagNames": {}` does not turn anything on. The two
structural rules and dictionary-version mismatch run at `warning`; integrity
runs at `error`.

**A section that is malformed is refused whole and named**, so the server keeps
the configuration it had rather than half-applying a new one, and the message
says which key it could not read. A section the server does not *know*, though,
is simply kept unread -- so a misspelled `inlayHnts` costs you the hints with
no complaint. Check a key against the schema rather than against the log.

### Custom XML dictionaries

Each custom dictionary needs a path and a non-blank, unique name. Enable
`allowDictionaryPath` in personal TOML or global LSP before registering any
external XML, including personal/global definitions:

```json
{
  "allowDictionaryPath": true,
  "dictionaries": [
    {
      "name": "venue-a",
      "path": "/path/to/FIX44-venue-a.xml",
      "mode": "quickfix",
      "default": false
    }
  ]
}
```

`mode` is `auto`, `quickfix`, or `repository`. In `auto` mode, a directory or a
path ending in `Fields.xml` is a FIX Repository dictionary; any other file is a
QuickFIX dictionary. A definition has no `version` field: the XML must declare
the FIX version, and missing/invalid metadata rejects the entry. `default`
controls whether it answers for lines of that version without an explicit
selector. It is false unless supplied.

To register multiple dictionaries in a project file and select one:

```toml
dictionary = "venue-a"

[[dictionaries]]
name = "venue-a"
path = "dict/venue-a.xml"
mode = "quickfix"

[[dictionaries]]
name = "venue-b"
path = "dict/venue-b.xml"
mode = "quickfix"
```

The same arrays are valid in personal TOML, but its top-level `dictionary`
selector is not: use `default = true` there, a project/modeline selector, or the
session command. A child project file can select `dictionary = "venue-b"`, or
the document can contain `# fix: dictionary=venue-b`. A modeline cannot declare
a structured dictionary array; an allowed path selector is sufficient for a
one-off dictionary. Selector paths must be platform-native absolute paths or
start with `./`, `../`, or `~/`; any other selector is a dictionary name. This
restriction does not apply to `dictionaries[].path`, whose relative paths
continue to resolve beside the file that defines them.

Names must be unique across the applicable chain, including global definitions
and bundled names. A conflict is an Error naming both sources; the later entry
is rejected entirely, including its default binding, while unrelated settings
remain usable. Independent project subtrees can reuse names. Within one array,
duplicate version defaults are Errors: the first default wins, but a later
entry's distinct name remains selectable. A stronger layer can replace a
version default using a different name; a stronger default never erases a
weaker explicit selector.

Bundled selectors `FIX.4.0` through `FIX.4.4`, `FIX.5.0`, `FIX.5.0SP1`,
`FIX.5.0SP2`, and `FIXT.1.1` always select immutable bundled data, even when a
custom dictionary replaces a version default. `FIXT.1.1` selects the bundled
FIX.5.0SP2 tables. Reserved, known-unbundled `FIX.2.7` and `FIX.3.0` selectors
are rejected, never interpreted as file paths. Custom names for those versions
can still be selected.

One file is expected to use one FIX version. BeginString(8) resolves the message version
first; absent, malformed, or unknown values use global `defaultVersion`.
Selection applies only if its dictionary matches that resolved version;
FIX.5.0SP2 also matches FIXT.1.1, not vice versa. A mismatch uses the resolved
version's default dictionary, not a weaker selector. Thus selecting a FIX.4.2
dictionary does not change a tag-8-free fragment's default FIX.4.4 version.
`diagnostics.rules.dictionaryVersion` reports the first mismatch for each
resolved version per diagnostic pass, with normal rule/global gates, severity,
and diagnostic limits. It is absent when diagnostics are disabled.

#### QuickFIX format

Use one XML file rooted at `<fix>`. The practical format reference is
[QuickFIX/n: Custom Fields, Groups, and Messages](https://quickfixengine.org/n/documentation/custom-fields-groups-messages.html),
and the QuickFIX project provides a
[complete FIX 4.4 dictionary](https://github.com/quickfix/quickfix/blob/master/spec/FIX44.xml).
This repository also contains a smaller
[custom FIX 4.4 example](xml/custom/synthetic/oe-fix44.xml).

Its structure is:

```xml
<fix type="FIX" major="4" minor="4" servicepack="0">
  <header>...</header>
  <trailer>...</trailer>
  <messages>
    <message name="NewOrderSingle" msgtype="D" msgcat="app">
      <field name="ClOrdID" required="Y"/>
      <component name="Instrument" required="Y"/>
      <group name="NoPartyIDs" required="N">...</group>
    </message>
  </messages>
  <components>...</components>
  <fields>
    <field number="50001" name="SyntheticMode" type="INT">
      <value enum="1" description="MODE_ALPHA"/>
    </field>
  </fields>
</fix>
```

`<fields>` is required. Header, trailer, messages, components, and groups are
optional. Every field referenced by name from another section must be declared
under `<fields>`. A group's first child is its delimiter field, and nested
groups are allowed.

#### FIX Repository 2010 Edition

The authoritative format definition is the FIX Trading Community's
[FIX Repository project and XSDs](https://github.com/FIXTradingCommunity/fix-repository/tree/master/2010_edition).
Point `path` at the version directory, its `Fields.xml`, or a parent directory
containing `Base/` with this layout:

```text
Base/
├── Fields.xml       required
├── Enums.xml        required
├── Messages.xml     optional
├── MsgContents.xml  optional
└── Components.xml   optional
```

The three optional structure files are used only when all three are present;
otherwise the dictionary still provides fields and enum values but no message
or group structure. The [synthetic Repository fixture](xml/custom/synthetic/repository/FIX.4.4/Base/)
is a small working example of every file the parser reads.

QuickFIX takes its version from `<fix type="FIX" major="4" minor="4">`;
Repository metadata supplies it in the `Fields` element's `version` attribute,
for example `<Fields version="FIX.4.4">`. No filename or directory name is
used as a version fallback.

### Reloading and troubleshooting

Saving TOML alone does not reload it. The server checks personal TOML at startup,
on `initialized`, and on `didChangeConfiguration`; it discovers/rechecks project
files on document open, configuration changes, and workspace-folder changes.
Modelines are rescanned on relevant edge-line edits or changes in line count.
There is no filesystem watcher or reload command. If your client cannot send
the relevant event, restart the server after editing configuration files.
`fix.describeOverrides` is inspection only and never triggers a reload.

Changes commit atomically. Already configured documents keep their previous
state while a batch is pending; a new document is not processed until its first
valid configuration commits. Its early configuration-dependent requests return
`RequestFailed`. If validation rejects the update, it waits for a later valid
update, while edits and close events are still accepted. Batch membership is
fixed at launch: documents opened later wait for the following successful
batch with its own deadline, rather than shortening their load time.

The first ready commit publishes diagnostics and requests supported annotation
refreshes even when settings equal defaults. Refresh requests are sent only
when supported. Clients without the relevant refresh support may not retry an
early refused request; annotations appear on their next explicit request.

Configuration errors remain visible even with `diagnostics.enabled = false`
and do not consume the FIX diagnostic cap. Modeline errors point at the
offending assignment; project errors name the source file. Personal-file errors
are reported globally, not once for every open document. Correcting/removing a
bad source clears its errors at the next configuration refresh.

An unknown `diagnostics.rules.*` identifier is a non-fatal configuration Error.
That entry is omitted from the effective configuration and ignored, while valid
sibling settings still apply.

| Problem | Behavior and remedy |
| --- | --- |
| External dictionary refused | Set global `allowDictionaryPath = true`; a project/modeline cannot enable it. |
| Unknown key, malformed TOML, obsolete `version` field | The file contributes no settings and reports Error; correct it to match the current schema. |
| Duplicate name or setting | Correct the conflicting source; names must be unique across the applicable chain. |
| Missing optional personal/project TOML | Normal absence; no layer is added. |
| Missing configured XML, permission/I/O failure, source disappears after discovery | Fatal: the server exits nonzero, even if an old dictionary exists. Fix the path/access before restarting. A path typo is also fatal. |
| Configuration takes longer than 500 ms | Fatal: one hardcoded whole-batch budget includes client response, loader queue, reads, XML parsing, and result preparation; it is not renewed per file. Check the client response and file access. |

Readable validation errors follow the source-specific rejection rules, but
unavailable dependencies cannot be skipped. Fatal shutdown logs the cause and
does not wait for blocked file reads or an LSP shutdown exchange.

## Build

```sh
cargo build --release
```

The binary is `target/release/fix-lsp`.

## Run

```sh
fix-lsp --stdio [--log-level <off|error|warn|info|debug|trace>] [--log-file <path>]
fix-lsp --version
```

The server speaks LSP over stdin/stdout (`--stdio`). Attach it from your editor's
LSP client with `filetypes = ["fix"]`.

## Security

`allowDictionaryPath` defaults to false and applies to every external XML load,
including personal/global definitions and direct project/modeline selectors.
Bundled dictionaries need no permission. Turning it off removes external
bindings/selections without reading their files; configured definitions can
become effective again after a later configuration refresh. The flag does
not prevent reading configuration TOML itself. Enable it only when the local
project configuration is trusted; it is a global permission, not a directory
allowlist or an administrator boundary.

Dictionary paths are plain strings: no shell, glob, or environment expansion,
only leading `~/`. Relative paths in personal/project TOML resolve beside that
file; modeline paths resolve beside the document. Global LSP definitions and
session command paths use the fixed initialization root (first workspace
folder, otherwise `rootUri`), never the process working directory. Without a
usable root, relative URI-less paths are rejected. Network-mounted paths use
the same filesystem rules and deadline, with no remote transfer protocol.

Paths are canonicalized before identity and regular-file checks. XML files
are bounded by `limits.dictionaryBytes` (16 MiB by default), separately for
each Repository file; configuration TOML has a fixed 1 MiB ceiling. Bounded
reads recheck growth beyond those limits. Non-regular files such as FIFOs and
devices are refused without reading them; XML with DTDs is rejected.

`documentLink.url` is global-only, defaults to OnixS, and accepts only HTTP(S)
templates or an empty string to disable links. Tag/version substitutions are
percent-encoded. A valid update changes hover and resolved hint links without
invalidating analysis caches; a project/modeline cannot redirect them.

## Where the dictionaries come from

The bundled tables are built from two public sources, and the reason there are
two is that no single public source has all three of the things a bundled
dictionary needs: complete data, per-version binding, and a licence that
permits redistribution.

- The FIX Trading Community's own repository packages are complete and
  versioned, but the site's terms allow downloads for use inside your
  organisation and forbid distributing the content, so they cannot ship here.
- The community's [Orchestra files](https://github.com/FIXTradingCommunity/orchestrations)
  are Apache-2.0 and complete, but exist per version only for 4.2 and 4.4;
  4.3 and 5.0–5.0SP2 were never converted.
- [QuickFIX/J](https://github.com/quickfix-j/quickfixj) covers every version
  under a redistributable licence, but carries no descriptions at all and
  omits enum values the protocol had — FIX44.xml lists no values for
  `PartySubIDType(803)`, which had 26 of them.

**What each source decides.** The version's QuickFIX/J file alone decides which
tags, messages and groups that version has: it is the version binding, and
nothing is added to it. FIX Orchestra supplies what they mean — field and
message descriptions, value names, the type spelling, when a field was added or
deprecated, and the extension pack recorded against that, when the source names
a positive one. This is the project's reproducible join rule, not authoritative
historical membership: no public source provides that for these versions.

**How enum values are joined.** A version lists what its QuickFIX/J file
declares, plus the Orchestra codes that belong to the field's *own* code set,
that Orchestra dates to that version or earlier, and that no extension pack
brought in. A code set shared between fields only relabels values the version
already has — its codes belong to the field it is named after.
`MsgType(35)` is never extended: its values are the version's own messages.
Worked example: under FIX.4.4, `PartySubIDType(803)` comes out as exactly 26
values, `1` through `26`; `28 = MarketMaker` arrived with extension pack 26 and
appears only from FIX.5.0 on. The restored values per version are 4.0 +0,
4.1 +4, 4.2 +58, 4.3 +251, 4.4 +139, 5.0 +60, 5.0SP1 +0, 5.0SP2 +0.

One limit stays: a value some later extension pack added to a base version can
still appear when the QuickFIX/J file lists it, because that file is taken as
the version. Prose the historical FIX Repository carried outside field and
value descriptions — reserved-range notes, for instance — is not reproduced.

Per-file provenance, hashes and the regeneration recipe are in
[xml/README.md](xml/README.md); the licences are
[THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt), with the QuickFIX
acknowledgment under [Licence](#licence).

## Licence

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). The bundled
dictionaries are QuickFIX/J files under the QuickFIX Software License 1.0, and
their descriptions are projected from FIX Orchestra, © FIX Protocol Limited,
under Apache-2.0; see [THIRD_PARTY_LICENSES.txt](THIRD_PARTY_LICENSES.txt) and
[xml/README.md](xml/README.md).

This product includes software developed by quickfixengine.org
(http://www.quickfixengine.org/).
