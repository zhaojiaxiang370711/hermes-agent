use std::path::PathBuf;

const FIXTURE: &str = "\
model:
  default: example-model
  provider: example-provider
agent:
  max_turns: 60
  gateway_timeout: 1800
";

fn tmp_config() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hermes-rs-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, FIXTURE).unwrap();
    path
}

#[test]
fn get_returns_scalar() {
    let path = tmp_config();
    let action = hermes_cli::ConfigAction::Get { key: "agent.max_turns".into() };
    hermes_cli::run_config_at(&path, action).unwrap();
}

#[test]
fn set_persists_to_disk() {
    let path = tmp_config();
    hermes_cli::run_config_at(&path, hermes_cli::ConfigAction::Set {
        key: "agent.max_turns".into(), value: "12".into()
    }).unwrap();
    let doc = hermes_config::ConfigDoc::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc.get("agent.max_turns").unwrap(), "12");
}

#[test]
fn set_unknown_key_is_preserved() {
    let path = tmp_config();
    hermes_cli::run_config_at(&path, hermes_cli::ConfigAction::Set {
        key: "agent.max_turns".into(), value: "12".into()
    }).unwrap();
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("gateway_timeout: 1800"));
}
