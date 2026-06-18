//! Asserts the implemented clap surface matches specs/cli-phase1.yaml.
//! If this test fails, either the code or the catalog drifted — fix both.
use clap::{CommandFactory, Parser};

#[test]
fn implemented_commands_match_catalog() {
    // Every catalog command must be a real clap subcommand.
    let cmd = hermes_cli::Cli::command();
    let subs: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
    for required in ["chat", "config", "model"] {
        assert!(subs.contains(&required), "missing subcommand: {required}");
    }

    // config subcommands must be get/set/list.
    let config = cmd.find_subcommand("config").unwrap();
    let cfg_subs: Vec<&str> = config.get_subcommands().map(|s| s.get_name()).collect();
    for required in ["get", "set", "list"] {
        assert!(cfg_subs.contains(&required), "missing config subcommand: {required}");
    }

    // Unknown commands must remain rejected.
    assert!(hermes_cli::Cli::try_parse_from(["hermes-rs", "dashboard"]).is_err());
    assert!(hermes_cli::Cli::try_parse_from(["hermes-rs", "web"]).is_err());
}
