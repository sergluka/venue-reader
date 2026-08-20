Custom-dictionary fixtures for the tests.

`synthetic/` holds small QuickFIX-format dictionaries written for this project
(Apache-2.0, no third-party data). Each file reproduces one set of shapes the
runtime parser must handle — see the comment at the top of each file.
`synthetic/repository/` holds the same kind of fixture in the FIX Repository
directory layout (`<version>/Base/*.xml`), which the runtime parser also reads.

Real broker dictionaries are deliberately not vendored:
their publishers' terms grant no redistribution right. To exercise the parser on
them locally, download them yourself and run

    FIX_LSP_BROKER_DICTS=/path/to/dir cargo test --test broker_dictionaries -- --ignored

which parses every `*.xml` under that directory (recursively) as QuickFIX and
checks that a version is inferred.
