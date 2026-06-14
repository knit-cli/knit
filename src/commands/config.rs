use crate::commands::remote::configured_sync_remote_names;
use crate::ids::slugify;
use crate::model::KnitConfig;
use crate::output as out;
use crate::store::{
    find_knit_root, global_config_path, load_config, load_effective_config, load_global_config,
    merge_effective_config, save_config, save_global_config,
};
use anyhow::{bail, Context, Result};

pub fn set_config_value(key: &str, value: &str, global: bool) -> Result<()> {
    if global {
        let mut config = load_global_config()?;
        apply_config_value(&mut config, key, value)?;
        ensure_sync_remotes_in_config(&config)?;
        save_global_config(&config)?;
        print_config_value(key, &config, "global")?;
        return Ok(());
    }

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let mut config = load_config(&root)?;
    apply_config_value(&mut config, key, value)?;
    ensure_sync_remotes_exist(&root, &config)?;
    save_config(&root, &config)?;
    print_config_value(key, &config, "workspace")?;
    Ok(())
}

pub fn show_config(global: bool) -> Result<()> {
    if global {
        let path = global_config_path()?;
        let config = load_global_config()?;
        print_config_section("Global config", &path, &config);
        return Ok(());
    }

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let global_path = global_config_path()?;
    let global_config = load_global_config()?;

    let Some(root) = find_knit_root(&cwd) else {
        print_config_section("Global config", &global_path, &global_config);
        return Ok(());
    };

    let workspace_path = root.join(".knit/config.json");
    let workspace_config = load_config(&root)?;
    let effective = load_effective_config(&root)?;

    print_config_section("Global config", &global_path, &global_config);
    println!();
    print_config_section("Workspace config", &workspace_path, &workspace_config);
    println!();
    print_config_section("Effective config", &workspace_path, &effective);
    Ok(())
}

fn apply_config_value(config: &mut KnitConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "advice" => {
            config.advice = parse_bool(value)?;
            Ok(())
        }
        "push-sync" | "push_sync" => {
            config.push_sync = parse_bool(value)?;
            Ok(())
        }
        "sync-remote" | "sync_remote" => {
            if is_clear_value(value) {
                config.sync_remote = None;
                config.sync_remotes.clear();
                return Ok(());
            }
            let remote_name = parse_remote_name(value)?;
            config.sync_remote = Some(remote_name.clone());
            config.sync_remotes = vec![remote_name];
            Ok(())
        }
        "sync-remotes" | "sync_remotes" => {
            if is_clear_value(value) {
                config.sync_remote = None;
                config.sync_remotes.clear();
                return Ok(());
            }
            let remote_names = parse_remote_names(value)?;
            config.sync_remote = remote_names.first().cloned();
            config.sync_remotes = remote_names;
            Ok(())
        }
        _ => bail!(
            "Unknown config key `{key}`. Supported: advice, push-sync, sync-remote, sync-remotes."
        ),
    }
}

fn print_config_value(key: &str, config: &KnitConfig, scope: &str) -> Result<()> {
    match key {
        "advice" | "push-sync" | "push_sync" => {
            let label = if key == "advice" {
                "advice"
            } else {
                "push-sync"
            };
            let value = if key == "advice" {
                config.advice
            } else {
                config.push_sync
            };
            println!(
                "{} {scope} {label}={}",
                out::heading("Config:"),
                if value { "true" } else { "false" }
            );
        }
        "sync-remote" | "sync_remote" => {
            let remote = config
                .sync_remote
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| "none".to_string());
            println!("{} {scope} sync-remote={remote}", out::heading("Config:"));
        }
        "sync-remotes" | "sync_remotes" => {
            let remotes = if config.sync_remotes.is_empty() {
                "none".to_string()
            } else {
                config.sync_remotes.join(",")
            };
            println!("{} {scope} sync-remotes={remotes}", out::heading("Config:"));
        }
        _ => {}
    }
    Ok(())
}

fn print_config_section(title: &str, path: &std::path::Path, config: &KnitConfig) {
    println!("{} {}", out::heading(title), out::muted(path.display()));
    if let Some(project) = config.active_project.as_deref() {
        println!("{} {}", out::heading("Project:"), out::repo(project));
    }
    if let Some(bundle) = config.active_bundle.as_deref() {
        println!("{} {}", out::heading("Bundle:"), out::repo(bundle));
    }
    println!(
        "{} {}",
        out::heading("Push sync:"),
        if config.push_sync { "true" } else { "false" }
    );
    let sync_remotes = configured_sync_remote_names(config);
    println!(
        "{} {}",
        out::heading("Sync remotes:"),
        if sync_remotes.is_empty() {
            out::muted("none").to_string()
        } else {
            sync_remotes.join(",")
        }
    );
    if config.remotes.is_empty() {
        println!("{} {}", out::heading("Remotes:"), out::muted("none"));
        return;
    }
    println!("{}", out::heading("Remotes:"));
    for (name, remote) in &config.remotes {
        println!(
            "  {} {} {}",
            out::repo(name),
            remote.url,
            out::muted(if remote.token.is_some() {
                "stored token"
            } else {
                "no stored token"
            })
        );
    }
}

fn ensure_sync_remotes_exist(root: &std::path::Path, config: &KnitConfig) -> Result<()> {
    let effective = merge_effective_config(load_global_config()?, config.clone());
    ensure_sync_remotes_in_config(&effective)?;
    let _ = root;
    Ok(())
}

fn ensure_sync_remotes_in_config(config: &KnitConfig) -> Result<()> {
    for remote_name in configured_sync_remote_names(config) {
        if !config.remotes.contains_key(&remote_name) {
            bail!("No KnitHub remote named `{remote_name}`. Run `knit remote add {remote_name} <url>` or `knit remote add --global {remote_name} <url>` first.");
        }
    }
    Ok(())
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
        bail!("Expected at least one remote name, for example `prod,staging`.");
    }
    Ok(names)
}

fn is_clear_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "clear" | "none" | "off" | "false"
    )
}
