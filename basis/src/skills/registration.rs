//! Holding a workspace's skills roots on the runtime it borrows, and handing
//! them back when the workspace goes.
//!
//! The shape is [`DeclaredTools`](crate::tools::declared)'s and
//! `mcp::connections`', because the problem is the third instance of one
//! problem: the registry is the *runtime's* and single (ADR-0018), while what
//! is registered on it came out of one repository's directories and belongs to
//! that repository. So the hold is counted on the runtime, and a root goes
//! only when its last holder does.
//!
//! What is basis's here and not mentra's is exactly that count. mentra owns
//! loading, layering and `load_skill`; `register_skills_dirs` is atomic and
//! `unregister_skills_dirs` is its inverse (mentra 0.24). Neither knows that
//! four roots came from one `Workspace::open`, or that two open workspaces
//! reach the same `~/.agents/skills` — which is precisely the fact a host
//! closing one repository needs somebody to have kept.

use std::{path::PathBuf, sync::Arc};

use crate::{error::RunError, runtime::Runtime};

/// One workspace's skills roots, registered on a runtime it may share.
///
/// Paths only, so `Debug` carries nothing a `SKILL.md` said: the skills
/// themselves live on the runtime's registry, and what a workspace reports is
/// [`Workspace::skills`](crate::Workspace::skills).
#[derive(Debug)]
pub(crate) struct SkillRoots {
    runtime: Arc<Runtime>,
    /// The roots this open registered, most specific first — released on drop.
    dirs: Vec<PathBuf>,
}

impl SkillRoots {
    /// Registers every root, most specific first, and holds them.
    ///
    /// Constructed on the stack in [`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open)
    /// before the tool manifests are claimed and moved into the `Workspace`
    /// only if the open reaches its end, so an open refused *after* the skills
    /// load hands them back on the way out. That is the half mentra's
    /// all-or-nothing registration cannot cover: upstream atomicity is about
    /// one call, and an open is a dozen of them.
    pub(crate) fn register(runtime: Arc<Runtime>, dirs: Vec<PathBuf>) -> Result<Self, RunError> {
        runtime.register_skill_roots(&dirs)?;
        Ok(Self { runtime, dirs })
    }

    /// A workspace that registered nothing — discovery off, or four roots that
    /// do not exist.
    ///
    /// Spelled out rather than left as an empty `Vec` at each call site,
    /// because a workspace always has one of these and *no roots* is a state
    /// the type should be able to say.
    pub(crate) fn none(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            dirs: Vec::new(),
        }
    }

    /// The roots registered, in the order [`discover`](super::discover)
    /// returns them — which is the precedence order mentra applies.
    pub(crate) fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }
}

impl Drop for SkillRoots {
    fn drop(&mut self) {
        self.runtime.release_skill_roots(&self.dirs);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
        std::fs::write(path, body).expect("write file");
    }

    fn skill_root(dir: &Path, name: &str) -> PathBuf {
        write(
            &dir.join(name).join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: a skill\n---\nSteps."),
        );
        dir.to_path_buf()
    }

    fn runtime() -> Arc<Runtime> {
        Arc::new(
            Runtime::builder()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_api_key("test-key")
                .with_ephemeral_history()
                .build()
                .expect("the runtime builds"),
        )
    }

    fn names(runtime: &Runtime) -> Vec<String> {
        let mut names: Vec<String> = runtime
            .mentra_runtime()
            .skills()
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        names.sort();
        names
    }

    /// The uniformity a private runtime cannot demonstrate for itself: it is
    /// dropped with its workspace, so nothing is left to ask. The guard makes
    /// no distinction, and this is where that is pinned — one rule for both
    /// ownership shapes is one thing to reason about, and a host that took the
    /// runtime out through `mentra_runtime()` and kept it alive gets the same
    /// answer either way.
    #[test]
    fn dropping_the_guard_takes_the_roots_off_the_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = skill_root(dir.path(), "release");
        let runtime = runtime();

        let held =
            SkillRoots::register(Arc::clone(&runtime), vec![root.clone()]).expect("roots register");
        assert_eq!(held.dirs(), [root]);
        assert_eq!(names(&runtime), ["release"]);

        drop(held);

        assert!(names(&runtime).is_empty());
    }

    /// Two holders, one root: the arithmetic the shared-runtime case rests on,
    /// asserted without opening a workspace.
    #[test]
    fn a_root_two_guards_hold_goes_with_the_second_of_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = skill_root(dir.path(), "personal");
        let runtime = runtime();

        let first = SkillRoots::register(Arc::clone(&runtime), vec![root.clone()])
            .expect("first registers");
        let second =
            SkillRoots::register(Arc::clone(&runtime), vec![root]).expect("second registers");

        drop(first);
        assert_eq!(names(&runtime), ["personal"], "the second holder remains");

        drop(second);
        assert!(names(&runtime).is_empty());
    }

    /// A guard that failed to register holds nothing, so its drop cannot take
    /// a co-holder's root away.
    #[test]
    fn a_refused_registration_holds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = skill_root(&dir.path().join("good"), "release");
        let bad = dir.path().join("bad");
        write(
            &bad.join("broken").join("SKILL.md"),
            "---\nname: [not a string\n---\nbody",
        );
        let runtime = runtime();

        let held = SkillRoots::register(Arc::clone(&runtime), vec![good.clone()])
            .expect("the good root registers");

        let error = SkillRoots::register(Arc::clone(&runtime), vec![good, bad])
            .expect_err("a root mentra cannot load refuses the batch");
        assert!(error.to_string().contains("frontmatter"), "{error}");

        assert_eq!(
            names(&runtime),
            ["release"],
            "the refused batch neither registered nor released anything"
        );
        drop(held);
        assert!(names(&runtime).is_empty());
    }

    /// Registration is keyed the way mentra matches a root, so two spellings
    /// of one directory are one hold rather than two.
    #[test]
    fn two_spellings_of_one_root_are_one_hold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = skill_root(&dir.path().join("skills"), "release");
        let detour = dir.path().join("skills").join("..").join("skills");
        let runtime = runtime();

        let first =
            SkillRoots::register(Arc::clone(&runtime), vec![root]).expect("first registers");
        let second =
            SkillRoots::register(Arc::clone(&runtime), vec![detour]).expect("second registers");

        drop(first);
        assert_eq!(
            names(&runtime),
            ["release"],
            "the detour spelling is the same root and still holds it"
        );

        drop(second);
        assert!(names(&runtime).is_empty());
    }
}
