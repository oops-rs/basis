//! Prompt templates end to end: a workspace on disk, the templates it yields,
//! and the prompt a chosen one produces.
//!
//! The unit tests cover each piece in isolation. What is worth checking from
//! outside the crate is that the pieces agree — that precedence between roots
//! survives loading, and that a broken file stops a run instead of quietly
//! shrinking the list. What those names become on the wire is `basis-acp`'s edge,
//! and is tested there.

use std::path::{Path, PathBuf};

use basis::ContextScope;
use basis::templates::{
    self, DEFAULT_GLOBAL_TEMPLATES_DIR, DEFAULT_WORKSPACE_TEMPLATES_DIR, TemplateError,
    TemplatesConfig,
};

/// A config with no global root unless the test asks for one, so nothing on the
/// developer's own machine can change an outcome here.
fn config(global: Option<PathBuf>) -> TemplatesConfig {
    TemplatesConfig {
        workspace_subdir: PathBuf::from(DEFAULT_WORKSPACE_TEMPLATES_DIR),
        global_dir: global,
    }
}

fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create dir");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write template");
    path
}

fn workspace_templates(root: &Path) -> PathBuf {
    root.join(DEFAULT_WORKSPACE_TEMPLATES_DIR)
}

#[test]
fn a_workspace_without_templates_loads_nothing_and_says_no_more() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let loaded = templates::load(tmp.path(), &config(None)).expect("absent is not an error");

    assert!(loaded.is_empty());
}

#[test]
fn a_template_travels_from_disk_to_a_rendered_prompt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &workspace_templates(tmp.path()),
        "review.md",
        "---\ndescription: Review a change\nargument-hint: <path>\n---\n\
         Review $1 and report anything that would break in production.\n",
    );

    let loaded = templates::load(tmp.path(), &config(None)).expect("load succeeds");

    assert_eq!(loaded.len(), 1);
    let prompt = loaded[0].render("src/auth.rs");
    assert_eq!(
        prompt,
        "Review src/auth.rs and report anything that would break in production.\n"
    );
}

#[test]
fn a_workspace_template_overrides_a_global_one_without_hiding_the_rest() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let global = tmp.path().join("config");

    write(
        &workspace_templates(tmp.path()),
        "review.md",
        "---\ndescription: Review, this project's way\n---\nProject review of $ARGUMENTS.\n",
    );
    write(
        &global.join(DEFAULT_GLOBAL_TEMPLATES_DIR),
        "review.md",
        "---\ndescription: Review, my way\n---\nPersonal review of $ARGUMENTS.\n",
    );
    write(
        &global.join(DEFAULT_GLOBAL_TEMPLATES_DIR),
        "scratch.md",
        "---\ndescription: A personal note\n---\nScratch.\n",
    );

    let loaded = templates::load(tmp.path(), &config(Some(global))).expect("load succeeds");

    let names: Vec<&str> = loaded.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["review", "scratch"]);

    let review = &loaded[0];
    assert_eq!(review.scope, ContextScope::Workspace);
    assert!(review.render("the diff").starts_with("Project review"));

    // The weaker root still contributes everything the stronger one did not
    // claim — shadowing is per name, not per root.
    assert_eq!(loaded[1].scope, ContextScope::Global);
}

#[test]
fn a_template_with_no_placeholders_still_receives_what_was_typed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &workspace_templates(tmp.path()),
        "explain.md",
        "---\ndescription: Explain something\n---\nExplain this clearly.\n",
    );

    let loaded = templates::load(tmp.path(), &config(None)).expect("load succeeds");

    assert_eq!(
        loaded[0].render("the retry loop"),
        "Explain this clearly.\n\nthe retry loop"
    );
    assert_eq!(loaded[0].render(""), "Explain this clearly.\n");
}

#[test]
fn malformed_frontmatter_stops_the_load_and_names_the_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = workspace_templates(tmp.path());
    write(&dir, "good.md", "---\ndescription: fine\n---\nbody\n");
    let broken = write(
        &dir,
        "broken.md",
        "---\ndescription: [unclosed\n---\nbody\n",
    );

    let error = templates::load(tmp.path(), &config(None)).expect_err("rejected");

    // The good template must not mask the broken one: a command list that
    // silently lost an entry is worse than a run that refuses to start.
    assert!(matches!(error, TemplateError::InvalidFrontmatter { .. }));
    assert!(error.to_string().contains(&broken.display().to_string()));
}

#[test]
fn a_template_without_a_description_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &workspace_templates(tmp.path()),
        "nameless.md",
        "just a prompt with no frontmatter at all\n",
    );

    let error = templates::load(tmp.path(), &config(None)).expect_err("rejected");

    assert!(matches!(error, TemplateError::MissingDescription { .. }));
}

#[test]
fn a_custom_workspace_subdir_is_honoured() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(
        &tmp.path().join("prompts"),
        "hello.md",
        "---\ndescription: greet\n---\nSay hello to $1.\n",
    );

    let loaded = templates::load(
        tmp.path(),
        &TemplatesConfig {
            workspace_subdir: PathBuf::from("prompts"),
            global_dir: None,
        },
    )
    .expect("load succeeds");

    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].render("world"), "Say hello to world.\n");
}
