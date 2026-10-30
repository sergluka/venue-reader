# Changelog

## [0.1.0] - 2026-10-30

### Added

- LSP server for FIX protocol logs
- resolve the user configuration file through XDG_CONFIG_HOME
- hover says how a tag stands against standard FIX
- hover and name hint for a tag the dictionary does not define
- fix.describeTag answers what a tag means without a line to hover
- wait for an unanswered configuration pull before answering without a document
- fix.describeTag answers what a tag means under no version
- validate messages against a scoped dictionary schema, report dictionary defects, and decode header groups
- support FIX5 with separate transport and application dictionaries

### Fixed

- keep markdown LF on checkout so README parsing tests pass on Windows
- use platform-native absolute paths in XDG tests
- date a field by the release that shipped its extension pack
- name the configuration file once in a rejected-file warning
- expect the canonical personal file path in the startup warning
- walk an edit once, after the configuration batch it waits for
- read a modeline from any file-scheme document, not only one whose path converts
- fix ruff issues + polishing
- omit code lenses with empty summaries
- update README and polish default settings.
- merge envelope presence checks into integrity
