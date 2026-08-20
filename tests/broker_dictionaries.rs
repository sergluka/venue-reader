//! Opt-in check against real broker dictionaries that are not vendored (their
//! publishers grant no redistribution right). Point `FIX_LSP_BROKER_DICTS` at a
//! directory of downloaded QuickFIX XML files and run with `-- --ignored`.

use std::path::{Path, PathBuf};

use fix_lsp::dictionary::parse_quickfix;

fn xml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            xml_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "xml") {
            out.push(path);
        }
    }
}

#[test]
#[ignore = "needs FIX_LSP_BROKER_DICTS pointing at locally downloaded dictionaries"]
fn every_downloaded_broker_dictionary_parses_with_a_version() {
    let dir = std::env::var_os("FIX_LSP_BROKER_DICTS").expect("set FIX_LSP_BROKER_DICTS");
    let mut files = Vec::new();
    xml_files(Path::new(&dir), &mut files);
    assert!(
        !files.is_empty(),
        "no *.xml under {}",
        Path::new(&dir).display()
    );
    for path in files {
        let dict = parse_quickfix(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            dict.version.is_some(),
            "{}: no version inferred",
            path.display()
        );
        assert!(
            dict.field(8).is_some(),
            "{}: no BeginString",
            path.display()
        );
        eprintln!(
            "{}: {} fields, {} messages, {} groups, version {}",
            path.display(),
            dict.fields.len(),
            dict.messages.len(),
            dict.groups.len(),
            dict.version.as_deref().unwrap()
        );
    }
}
