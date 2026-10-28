# Changelog

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Added

- Standalone Language Server Protocol server for FIX logs over stdio.
- Bundled FIX 4.0–4.4, FIX 5.0/SP1/SP2 and FIXT 1.1 dictionaries, plus custom XML
  dictionaries and per-line transport/application dictionary selection.
- Hover documentation, tag-name and enum-value inlay hints, semantic highlighting,
  message summaries, document outlines and selection expansion.
- CodeLens summary resolution and commands to decode selections, select a
  dictionary, inspect overrides and describe a tag.
- Dictionary/version diagnostics and joint repairs for `BodyLength(9)` and
  `CheckSum(10)`.
- Personal, project and LSP configuration with per-file modelines.
- Local analysis without uploading log contents to a decoding service.
