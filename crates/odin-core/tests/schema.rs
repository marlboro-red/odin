//! Conformance guard for `docs/workflow.schema.json` — the editor-aid JSON Schema (autocomplete +
//! typo-catching in a YAML language server). It must accept EVERY shipped example workflow, so this
//! test fails the moment the schema drifts from a real, valid workflow. (The authoritative validator
//! is `odin validate`/`validate_source`, which additionally enforces the ODIN### semantic rules and
//! the exactly-one-of step kind — things a JSON Schema can't cleanly express.)

const SCHEMA: &str = include_str!("../../../docs/workflow.schema.json");

#[test]
fn the_schema_is_valid_and_accepts_every_example() {
    let schema: serde_json::Value = serde_json::from_str(SCHEMA).expect("schema is valid JSON");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");

    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    let mut checked = 0_usize;
    for entry in std::fs::read_dir(dir).expect("read examples dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let yaml = std::fs::read_to_string(&path).unwrap();
        // `recipe new` scaffolding templates carry `@@VAR@@` placeholders and aren't valid YAML
        // until rendered — skip them (the concrete, runnable examples are what the schema guards).
        if yaml.contains("@@") {
            continue;
        }
        // YAML -> JSON value, so the JSON Schema validator can check it.
        let instance: serde_json::Value = serde_yaml_ng::from_str(&yaml)
            .unwrap_or_else(|e| panic!("{}: YAML parse failed: {e}", path.display()));

        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|e| format!("  - {e} (at {})", e.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "{} does not conform to workflow.schema.json:\n{}",
            path.display(),
            errors.join("\n")
        );
        checked += 1;
    }
    assert!(
        checked >= 10,
        "expected to validate the full example set, only saw {checked}"
    );
}

#[test]
fn the_schema_rejects_malformed_workflows() {
    let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();

    // Each of these is malformed in one way the schema should catch (so it isn't vacuously
    // permissive). The authoritative `odin validate` catches more, but these are pure-shape errors.
    let bad = [
        // missing the required `name`
        serde_json::json!({ "steps": [{ "id": "a", "run": "true" }] }),
        // missing the required `steps`
        serde_json::json!({ "name": "x" }),
        // an unknown top-level field (a typo)
        serde_json::json!({ "name": "x", "steps": [], "stpes": [] }),
        // an unknown field on a step (a typo for `timeout`)
        serde_json::json!({ "name": "x", "steps": [{ "id": "a", "run": "true", "tmeout": "5m" }] }),
        // a param with an invalid type
        serde_json::json!({ "name": "x", "steps": [], "params": { "p": { "type": "date" } } }),
        // an unknown trigger type
        serde_json::json!({ "name": "x", "steps": [], "triggers": [{ "type": "kafka" }] }),
        // a judge threshold out of range
        serde_json::json!({ "name": "x", "steps": [{ "id": "a", "run": "true", "judge": { "provider": "c", "criteria": "ok", "threshold": 2 } }] }),
    ];
    for (i, instance) in bad.iter().enumerate() {
        assert!(
            !validator.is_valid(instance),
            "malformed case #{i} should have been rejected by the schema: {instance}"
        );
    }
}
