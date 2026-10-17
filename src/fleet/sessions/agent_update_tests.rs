use super::*;
use crate::config::Inventory;

fn spec(mode: &str) -> MachineSpec {
    Inventory::parse(&format!(
        "[[machine]]\nname = \"Box\"\nhost = \"box\"\nagent_update = \"{mode}\"\n"
    ))
    .unwrap()
    .machines
    .remove(0)
}

#[test]
fn manual_mode_refuses_old_missing_or_broken_agents_before_the_install_block() {
    for installed in ["old", "none", "broken"] {
        let error = needs_install(&spec("manual"), "new", installed).unwrap_err();
        assert!(error.contains("Box is out of date"));
        assert!(error.contains(MANUAL_INSTALL_MARKER));
    }
}

#[test]
fn matching_agents_need_no_upload_in_either_mode() {
    for mode in ["auto", "manual"] {
        assert!(!needs_install(&spec(mode), "same", "same\n").unwrap());
    }
}

#[test]
fn auto_mode_installs_old_missing_or_broken_agents() {
    for installed in ["old", "none", "broken"] {
        assert!(needs_install(&spec("auto"), "new", installed).unwrap());
    }
}

#[test]
fn an_explicit_install_bypasses_manual_policy_for_only_its_cloned_spec() {
    let original = spec("manual");
    assert!(!original.agent_install_requested);
    let mut requested = original.clone();
    requested.agent_install_requested = true;
    assert!(needs_install(&requested, "new", "old").unwrap());
    assert!(needs_install(&original, "new", "old").is_err());
}
