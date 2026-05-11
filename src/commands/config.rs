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
        _ => bail!("Unknown config key `{key}`. Currently supported: advice."),
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("Expected a boolean value: true or false."),
    }
}
