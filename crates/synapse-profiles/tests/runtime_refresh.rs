use std::{fs, path::Path};

use synapse_core::ProfileUseScope;
use synapse_core::error_codes;
use synapse_profiles::{ForegroundWindow, ProfileRuntime, bundled_profiles_dir};
use tempfile::TempDir;

#[test]
fn runtime_refresh_add_replace_delete_and_preserve_prior_valid_profile()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let runtime = ProfileRuntime::spawn(temp.path())?;
    assert!(runtime.list(true)?.is_empty());

    let scratch = temp.path().join("scratch.toml");
    write_profile(&scratch, "scratch", "Scratch One", "scratch.exe")?;
    runtime.refresh()?;
    let list = runtime.list(true)?;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "scratch");

    write_profile(&scratch, "scratch", "Scratch Two", "scratch.exe")?;
    runtime.refresh()?;
    let Some(profile) = runtime.profile("scratch")? else {
        panic!("scratch profile missing after replacement");
    };
    assert_eq!(profile.label, "Scratch Two");

    fs::write(
        &scratch,
        r#"
id = "scratch"
label = "Scratch Broken"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
title_regex = "("
"#,
    )?;
    runtime.refresh()?;
    let Some(profile) = runtime.profile("scratch")? else {
        panic!("prior valid profile missing after invalid edit");
    };
    assert_eq!(profile.label, "Scratch Two");
    assert_eq!(
        runtime.last_errors()?[0].code,
        error_codes::PROFILE_PARSE_ERROR
    );

    fs::remove_file(&scratch)?;
    runtime.refresh()?;
    assert!(runtime.list(true)?.is_empty());
    Ok(())
}

#[test]
fn runtime_resolves_and_activates_foreground_profile() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = ProfileRuntime::spawn(bundled_profiles_dir())?;
    let Some(resolution) = runtime.activate_for_foreground(&ForegroundWindow {
        exe: Some("Code.exe".to_owned()),
        title: Some("agent - Visual Studio Code".to_owned()),
        steam_appid: None,
        window_class: None,
    })?
    else {
        panic!("VS Code profile did not match foreground context");
    };

    assert_eq!(resolution.profile_id, "vscode");
    assert_eq!(resolution.rank_name, "exe");
    assert_eq!(runtime.active_profile_id()?, Some("vscode".to_owned()));
    assert_eq!(runtime.list(false)?.len(), 1);
    Ok(())
}

#[test]
fn runtime_clears_active_profile_for_unmatched_foreground() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = TempDir::new()?;
    let profile_path = temp.path().join("notepad.toml");
    write_profile(&profile_path, "notepad", "Notepad", "notepad.exe")?;
    let runtime = ProfileRuntime::spawn(temp.path())?;

    let matched = ForegroundWindow {
        exe: Some("notepad.exe".to_owned()),
        title: Some("scratch.txt - Notepad".to_owned()),
        steam_appid: None,
        window_class: None,
    };
    runtime.activate_for_foreground(&matched)?;
    assert_eq!(runtime.active_profile_id()?, Some("notepad".to_owned()));

    let transition = runtime.reevaluate_foreground(&ForegroundWindow {
        exe: Some("unprofiled.exe".to_owned()),
        title: Some("No Profile".to_owned()),
        steam_appid: None,
        window_class: None,
    })?;

    assert_eq!(transition.previous_profile_id, Some("notepad".to_owned()));
    assert_eq!(transition.active_profile_id, None);
    assert_eq!(
        transition.previous_scope,
        Some(ProfileUseScope::Productivity)
    );
    assert_eq!(transition.active_scope, None);
    assert_eq!(
        transition.effective_previous_scope,
        ProfileUseScope::Productivity
    );
    assert_eq!(transition.effective_active_scope, ProfileUseScope::Unknown);
    assert!(transition.changed);
    assert!(transition.scope_changed);
    assert_eq!(runtime.active_profile_id()?, None);
    Ok(())
}

fn write_profile(
    path: &Path,
    id: &str,
    label: &str,
    exe: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(
        path,
        format!(
            r#"
id = "{id}"
label = "{label}"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "{exe}"
"#
        ),
    )?;
    Ok(())
}
