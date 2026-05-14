use anyhow::{bail, Result};

pub fn print_schema(name: &str) -> Result<()> {
    let schema = match name {
        "bundle" => include_str!("../../schemas/bundle.schema.json"),
        "project" => include_str!("../../schemas/project.schema.json"),
        "contexts" => include_str!("../../schemas/contexts.schema.json"),
        "merge-run" => include_str!("../../schemas/merge-run.schema.json"),
        "land-plan" => include_str!("../../schemas/land-plan.schema.json"),
        "land-run" => include_str!("../../schemas/land-run.schema.json"),
        "config" => include_str!("../../schemas/config.schema.json"),
        _ => bail!(
            "Unknown schema `{name}`. Use bundle, project, contexts, merge-run, land-plan, land-run, or config."
        ),
    };
    println!("{schema}");
    Ok(())
}
