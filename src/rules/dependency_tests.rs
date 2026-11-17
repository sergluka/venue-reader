use std::collections::HashSet;

use super::repair::{Provider, RepairPolicy};
use super::{Diag, LineCtx, Precondition, Rule, BUILTINS};
use crate::config::Severity;

fn validate_dependencies(rules: &[&dyn Rule]) -> Result<(), String> {
    fn visit(
        rule: &dyn Rule,
        rules: &[&dyn Rule],
        visiting: &mut Vec<&'static str>,
        checked: &mut HashSet<&'static str>,
    ) -> Result<(), String> {
        if visiting.contains(&rule.id()) {
            return Err(format!("Cyclic repair dependency: {}", rule.id()));
        }
        if checked.contains(rule.id()) {
            return Ok(());
        }
        let RepairPolicy::Safe(provider) = rule.repair_policy() else {
            return Ok(());
        };
        visiting.push(rule.id());
        for dependency in provider.dependencies {
            let target = rules
                .iter()
                .find(|target| target.id() == *dependency)
                .ok_or_else(|| {
                    format!("Unknown repair dependency: {} -> {dependency}", rule.id())
                })?;
            if !matches!(target.repair_policy(), RepairPolicy::Safe(_)) {
                return Err(format!(
                    "Dependency has no repair provider: {} -> {dependency}",
                    rule.id()
                ));
            }
            visit(*target, rules, visiting, checked)?;
        }
        visiting.pop();
        checked.insert(rule.id());
        Ok(())
    }

    let mut checked = HashSet::new();
    for rule in rules {
        visit(*rule, rules, &mut Vec::new(), &mut checked)?;
    }
    Ok(())
}

#[test]
fn builtin_repair_dependencies_are_resolvable_and_acyclic() {
    validate_dependencies(BUILTINS).unwrap();
}

struct Fixture {
    name: &'static str,
    dependencies: Option<&'static [&'static str]>,
}

impl Rule for Fixture {
    fn id(&self) -> &'static str {
        self.name
    }
    fn precondition(&self) -> Precondition {
        Precondition::Fields
    }
    fn default_severity(&self) -> Severity {
        Severity::Warning
    }
    fn repair_policy(&self) -> RepairPolicy {
        match self.dependencies {
            None => RepairPolicy::DiagnosticOnly,
            Some(dependencies) => RepairPolicy::Safe(Provider {
                repair: |_| panic!("dependency validation needs no message"),
                eligible: |_| panic!("dependency validation ignores input eligibility"),
                claims: |_, _| panic!("dependency validation needs no findings"),
                dependencies,
                finalizer: false,
            }),
        }
    }
    fn check(&self, _: &LineCtx, _: Severity) -> Vec<Diag> {
        panic!("dependency validation needs no message")
    }
}

#[test]
fn dependency_validation_accepts_shared_dependencies_and_diagnostic_only_roots() {
    let fixtures = [
        Fixture {
            name: "root",
            dependencies: Some(&["left", "right"]),
        },
        Fixture {
            name: "left",
            dependencies: Some(&["leaf"]),
        },
        Fixture {
            name: "right",
            dependencies: Some(&["leaf"]),
        },
        Fixture {
            name: "leaf",
            dependencies: Some(&[]),
        },
        Fixture {
            name: "diagnostic",
            dependencies: None,
        },
    ];
    let rules: Vec<&dyn Rule> = fixtures.iter().map(|rule| rule as &dyn Rule).collect();
    validate_dependencies(&rules).unwrap();
}

#[test]
fn dependency_validation_rejects_invalid_graphs_without_running_rules() {
    for (fixtures, expected) in [
        (
            vec![Fixture {
                name: "root",
                dependencies: Some(&["missing"]),
            }],
            "Unknown repair dependency: root -> missing",
        ),
        (
            vec![
                Fixture {
                    name: "root",
                    dependencies: Some(&["diagnostic"]),
                },
                Fixture {
                    name: "diagnostic",
                    dependencies: None,
                },
            ],
            "Dependency has no repair provider: root -> diagnostic",
        ),
        (
            vec![Fixture {
                name: "root",
                dependencies: Some(&["root"]),
            }],
            "Cyclic repair dependency: root",
        ),
        (
            vec![
                Fixture {
                    name: "root",
                    dependencies: Some(&["child"]),
                },
                Fixture {
                    name: "child",
                    dependencies: Some(&["root"]),
                },
            ],
            "Cyclic repair dependency: root",
        ),
        (
            vec![
                Fixture {
                    name: "root",
                    dependencies: Some(&["child"]),
                },
                Fixture {
                    name: "child",
                    dependencies: Some(&["missing"]),
                },
            ],
            "Unknown repair dependency: child -> missing",
        ),
    ] {
        let rules: Vec<&dyn Rule> = fixtures.iter().map(|rule| rule as &dyn Rule).collect();
        assert_eq!(validate_dependencies(&rules).unwrap_err(), expected);
    }
}
