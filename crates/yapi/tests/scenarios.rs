//! yapi against pi's recorded behavior on every end-to-end scenario.
//!
//! The goldens in `tests/fixtures/scenarios` are pi's normalized output, written
//! by `cargo xtask e2e --record-pi`.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers; a panic is a test failure"
)]

use yapi_mock::scenario::{
    Program, first_difference, fixtures_dir, load_scenarios, normalize, run,
};

#[tokio::test(flavor = "multi_thread")]
async fn scenarios_match_pi() {
    let yapi = Program::Yapi(env!("CARGO_BIN_EXE_yapi").into());
    let mut failures = Vec::new();
    for scenario in load_scenarios().unwrap() {
        if let Some(reason) = scenario.skip_reason() {
            eprintln!("skipped {}: {reason}", scenario.name);
            continue;
        }
        let golden = fixtures_dir()
            .join("scenarios")
            .join(format!("{}.json", scenario.name));
        let expected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&golden).unwrap()).unwrap();
        let outcome = run(&scenario, &yapi).await.unwrap();
        let actual = normalize(&outcome);
        if let Some(diff) = first_difference(&expected, &actual) {
            failures.push(format!(
                "{}: {diff}\nstderr: {}",
                scenario.name, outcome.stderr
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
