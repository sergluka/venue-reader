use std::path::Path;

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
    assert_eq!(blocks.len(), 4);
    fix_lsp::config::user_file::parse(blocks[0], Path::new("/home/test/.config/fix-lsp.toml"))
        .expect("personal defaults example");
    for example in &blocks[1..] {
        let parsed =
            project::parse(example, Path::new("/work/.fix-lsp.toml")).expect("project example");
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
        if value.get("command").is_some() || value.get("source").is_some() {
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
fn readme_modelines_are_valid_together_at_one_level() {
    let examples = examples("text");
    let block = examples
        .iter()
        .find(|example| example.starts_with("# fix:"))
        .expect("modeline example");
    let lines: Vec<_> = block.lines().map(str::as_bytes).collect();
    let layer = modeline::parse(&lines, "file:///work/orders.fix");
    assert!(layer.warnings.is_empty(), "{:?}", layer.warnings);
    assert_eq!(
        layer.get("dictionary"),
        Some(&Value::String("FIX.4.4".into()))
    );
}
