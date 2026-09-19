use std::path::Path;

use fix_lsp::config::Config;
use fix_lsp::overrides::{modeline, parse_client, project};
use serde_json::Value;

fn examples(language: &str) -> Vec<&'static str> {
    let readme = include_str!("../README.md");
    let opening = format!("```{language}");
    readme
        .split(&opening)
        .skip(1)
        .filter_map(|section| {
            let section = section
                .strip_prefix("\r\n")
                .or_else(|| section.strip_prefix('\n'))?;
            section.split("```").next()
        })
        .collect()
}

#[test]
fn readme_toml_examples_match_current_source_parsers() {
    let blocks = examples("toml");
    assert_eq!(blocks.len(), 5);
    for example in blocks {
        // Dispatch on the project file name, which is fixed. The personal file's
        // location is not, so the example header must stay free to change.
        let header = format!("# {}", project::FILE_NAME);
        let parsed = if example.starts_with(&header) {
            project::parse(example, Path::new("/work/.fix-lsp.toml")).expect("project example")
        } else {
            assert!(example.starts_with("# "), "{example}");
            fix_lsp::config::user_file::parse(example, Path::new("/home/test/.config/fix-lsp.toml"))
                .expect("personal defaults example")
        };
        assert!(
            parsed.layer.warnings.is_empty(),
            "{:?}",
            parsed.layer.warnings
        );
    }
}

#[test]
fn readme_json_settings_match_the_global_parser() {
    let mut settings = 0;
    for example in examples("json") {
        let value: Value = serde_json::from_str(example).expect("valid JSON example");
        if value.get("command").is_some() {
            continue;
        }
        let section = value.get("fix").cloned().unwrap_or(value);
        let parsed = parse_client(section).expect("global settings example");
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        settings += 1;
    }
    assert_eq!(settings, 3);
}

#[test]
fn readme_defaults_match_the_complete_server_defaults() {
    let readme = include_str!("../README.md");
    let section = readme
        .split("### Default settings\n")
        .nth(1)
        .expect("defaults section");
    let example = section
        .split("```json\n")
        .nth(1)
        .and_then(|block| block.split("```").next())
        .expect("default settings example");
    let documented: Value = serde_json::from_str(example).expect("valid defaults JSON");
    assert_eq!(
        documented,
        fix_lsp::overrides::public_config(&Config::default())
    );
}

#[test]
fn readme_modelines_are_valid_together_at_one_level() {
    let examples = examples("text");
    let block = examples
        .iter()
        .find(|example| example.starts_with("# fix:"))
        .expect("modeline example");
    let lines: Vec<_> = block.lines().map(str::as_bytes).collect();
    let layer = modeline::parse(&lines, "file:///work/example.fix");
    assert!(layer.warnings.is_empty(), "{:?}", layer.warnings);
    assert_eq!(
        layer.get("dictionary"),
        Some(&Value::String("FIX.4.4".into()))
    );
}
