#![cfg(unix)]

mod harness;

use std::fs;
use std::path::Path;

use harness::{file_uri, TestClient, Workspace};
use serde_json::{json, Value};

fn client(workspace: &Workspace) -> TestClient {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "rootUri": file_uri(&workspace.0.join("workspace")),
        "capabilities": {},
        "initializationOptions": {"fix": {"diagnostics": {"enabled": false}}}
    }));
    client
}

fn open_report(client: &mut TestClient, path: &Path) -> Value {
    client.did_open(&file_uri(path), "8=FIX.4.4|35=D|54=1|", 1)
}

#[test]
fn symlinked_config_without_filters_applies_inside_its_discovery_directory() {
    let fixture = Workspace::new("review-project");
    let target = fixture.0.join("shared.toml");
    fs::write(&target, "hover.enabled = false").unwrap();
    fixture.link("workspace", ".venue-reader.toml", &target);
    let mut client = client(&fixture);
    let report = open_report(&mut client, &fixture.0.join("workspace/orders.fix"));
    assert_eq!(report["effective"]["hover.enabled"], false);
    assert!(report["warnings"].as_array().unwrap().is_empty());
    client.shutdown();
}

#[test]
fn symlinked_config_filters_are_relative_to_its_discovery_directory() {
    let fixture = Workspace::new("review-project");
    let target = fixture.0.join("shared.toml");
    fs::write(&target, "files = ['logs/*.fix']\nhover.enabled = false").unwrap();
    fixture.link("workspace", ".venue-reader.toml", &target);
    let mut client = client(&fixture);
    for (path, enabled) in [
        ("workspace/logs/orders.fix", false),
        ("workspace/other/orders.fix", true),
        ("workspace/logs/nested/orders.fix", true),
    ] {
        let report = open_report(&mut client, &fixture.0.join(path));
        assert_eq!(report["effective"]["hover.enabled"], enabled, "{path}");
    }
    client.shutdown();
}

#[test]
fn cached_target_can_apply_in_independent_project_chains() {
    let fixture = Workspace::new("review-project");
    let target = fixture.0.join("shared.toml");
    fs::write(&target, "files = ['*.fix']\nhover.enabled = false").unwrap();
    fixture.link("workspace/left", ".venue-reader.toml", &target);
    fixture.link("workspace/right", ".venue-reader.toml", &target);
    let mut client = client(&fixture);
    for directory in ["left", "right"] {
        let report = open_report(
            &mut client,
            &fixture
                .0
                .join("workspace")
                .join(directory)
                .join("orders.fix"),
        );
        assert_eq!(report["effective"]["hover.enabled"], false, "{directory}");
    }
    client.shutdown();
}

#[test]
fn one_target_discovered_at_two_levels_keeps_both_applicability_scopes() {
    let fixture = Workspace::new("review-project");
    let target = fixture.0.join("shared.toml");
    fs::write(&target, "files = ['*.fix']\nhover.enabled = false").unwrap();
    fixture.link("workspace", ".venue-reader.toml", &target);
    fixture.link("workspace/logs", ".venue-reader.toml", &target);
    let mut client = client(&fixture);
    let report = open_report(&mut client, &fixture.0.join("workspace/logs/orders.fix"));
    assert_eq!(report["effective"]["hover.enabled"], false);
    client.shutdown();
}
