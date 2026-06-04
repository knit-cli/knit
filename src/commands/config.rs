use crate::ids::slugify;
use crate::model::KnitConfig;
use crate::output as out;
use crate::store::{find_knit_root, load_config, save_config};
use anyhow::{bail, Context, Result};

pub fn set_config_value(key: &str, value: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let mut config = load_config(&root)?;
    match key {
        "advice" => {
            config.advice = parse_bool(value)?;
            save_config(&root, &config)?;
            println!(
                "{} advice={}",
                out::heading("Config:"),
                if config.advice { "true" } else { "false" }
            );
            Ok(())
        }
        "push-sync" | "push_sync" => {
            config.push_sync = parse_bool(value)?;
            save_config(&root, &config)?;
            println!(
                "{} push-sync={}",
                out::heading("Config:"),
                if config.push_sync { "true" } else { "false" }
            );
            Ok(())
        }
        "sync-remote" | "sync_remote" => {
            if is_clear_value(value) {
                config.sync_remote = None;
                config.sync_remotes.clear();
                save_config(&root, &config)?;
                println!("{} sync-remote={}", out::heading("Config:"), out::muted("none"));
                return Ok(());
            }
            let remote_name = parse_remote_name(value)?;
            ensure_remote_exists(&config, &remote_name)?;
            config.sync_remote = Some(remote_name.clone());
            config.sync_remotes = vec![remote_name.clone()];
            save_config(&root, &config)?;
            println!("{} sync-remote={remote_name}", out::heading("Config:"));
            Ok(())
        }
        "sync-remotes" | "sync_remotes" => {
            if is_clear_value(value) {
                config.sync_remote = None;
                config.sync_remotes.clear();
                save_config(&root, &config)?;
                println!("{} sync-remotes={}", out::heading("Config:"), out::muted("none"));
                return Ok(());
            }
            let remote_names = parse_remote_names(value)?;
            for remote_name in &remote_names {
                ensure_remote_exists(&config, remote_name)?;
            }
            config.sync_remote = remote_names.first().cloned();
            config.sync_remotes = remote_names.clone();
            save_config(&root, &config)?;
            println!(
                "{} sync-remotes={}",
                out::heading("Config:"),
                remote_names.join(",")
            );
            Ok(())
        }
        _ => bail!("Unknown config key `{key}`. Currently supported: advice, push-sync, sync-remote, sync-remotes."),
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("Expected a boolean value: true or false."),
    }
}

fn parse_remote_name(value: &str) -> Result<String> {
    let remote_name = slugify(value);
    if remote_name.is_empty() {
        bail!("Expected a remote name.");
    }
    Ok(remote_name)
}

fn parse_remote_names(value: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for part in value.split(',') {
        let remote_name = slugify(part);
        if remote_name.is_empty() {
            continue;
        }
        if !names.contains(&remote_name) {
            names.push(remote_name);
        }
    }
    if names.is_empty() {
        bail!("Expected at least one remote name, for example `local,knithub`.");
    }
    Ok(names)
}

fn ensure_remote_exists(config: &KnitConfig, remote_name: &str) -> Result<()> {
    if config.remotes.contains_key(remote_name) {
        Ok(())
    } else {
        bail!("No KnitHub remote named `{remote_name}`. Run `knit remote add {remote_name} <url>` first.")
    }
}

fn is_clear_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "clear" | "none" | "off" | "false"
    )
}
