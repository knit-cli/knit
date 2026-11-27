use clap::CommandFactory;

#[test]
fn full_reference_names_every_top_level_command() {
    let reference = include_str!("../docs/reference.md");
    let command = knit::Cli::command();

    for subcommand in command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
    {
        let name = subcommand.get_name();
        assert!(
            reference.contains(&format!("knit {name}")),
            "docs/reference.md does not name top-level command `knit {name}`"
        );
    }
}

#[test]
fn authoritative_guides_do_not_teach_removed_spellings() {
    let reference = include_str!("../docs/reference.md");
    let architecture = include_str!("../docs/architecture.md");
    let agents_template = include_str!("../src/commands/init.rs");

    for (name, text) in [
        ("docs/reference.md", reference),
        ("src/commands/init.rs", agents_template),
    ] {
        assert!(!text.contains("knit publish create --base"), "{name}");
        assert!(!text.contains("knit fetch --all"), "{name}");
        assert!(!text.contains("knit fetch --bundles"), "{name}");
        assert!(!text.contains("\nknit prune"), "{name}");
    }

    assert!(architecture.contains("Historically the routine families"));
    assert!(
        !architecture.contains("`knit fetch --bundles` / `knit pull --bundles` pull recorded"),
        "architecture must not teach the removed fetch spelling as a current command"
    );
    assert!(
        reference.contains("knit bundle prune --apply --delete --remote-bundles"),
        "remote deletion guidance must include the required --delete switch"
    );
}

#[test]
fn release_notes_live_under_docs_releases() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    for entry in std::fs::read_dir(&docs).unwrap() {
        let name = entry.unwrap().file_name();
        let name = name.to_string_lossy();
        assert!(
            !(name.starts_with("release-") && name.ends_with(".md")),
            "release notes belong in docs/releases/, not docs/{name}"
        );
    }

    let version = env!("CARGO_PKG_VERSION");
    let notes = docs.join("releases").join(format!("release-{version}.md"));
    assert!(
        notes.is_file(),
        "missing release notes for the current version at {}",
        notes.display()
    );
}
