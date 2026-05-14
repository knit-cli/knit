use crate::ids::slugify;
use crate::output as out;
use crate::store::{
    bundle_exists, clear_agent_active_bundle, current_agent_id, find_knit_root, load_agent_context,
    set_agent_active_bundle,
};
use anyhow::{bail, Context, Result};

pub fn show_agent_context() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let agent_id = current_agent_id().context(
        "No Knit agent id found. Set KNIT_AGENT, pass --agent, or run inside a Codex thread.",
    )?;
    println!("{} {}", out::heading("Agent:"), out::repo(&agent_id));
    match load_agent_context(&root, &agent_id)? {
        Some(context) => println!(
            "{} {}",
            out::heading("Active bundle:"),
            out::node(context.active_bundle)
        ),
        None => println!("{}", out::muted("No active bundle for this agent.")),
    }
    Ok(())
}

pub fn switch_agent_bundle(bundle_id: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let agent_id = current_agent_id().context(
        "No Knit agent id found. Set KNIT_AGENT, pass --agent, or run inside a Codex thread.",
    )?;
    let bundle_id = slugify(bundle_id);
    if !bundle_exists(&root, &bundle_id) {
        bail!("No Knit bundle named `{bundle_id}` found.");
    }
    let agent_id = set_agent_active_bundle(&root, &agent_id, &bundle_id)?;
    println!(
        "{} {} {}",
        out::heading("Agent bundle:"),
        out::node(bundle_id),
        out::muted(format!("agent {agent_id}"))
    );
    Ok(())
}

pub fn clear_agent_context() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let agent_id = current_agent_id().context(
        "No Knit agent id found. Set KNIT_AGENT, pass --agent, or run inside a Codex thread.",
    )?;
    clear_agent_active_bundle(&root, &agent_id)?;
    println!("{} {}", out::heading("Cleared agent:"), out::repo(agent_id));
    Ok(())
}
