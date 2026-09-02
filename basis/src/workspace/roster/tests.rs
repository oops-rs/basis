//! What each `ToolRoster` constructor actually produces, and how the per-mint
//! foreign-tool hide composes with either one. The prompt-independence half of
//! item (d) is pinned beside the memory index in `workspace::builder::tests`,
//! where the rendered prompt is assembled.

use super::*;

#[test]
fn default_hides_todays_exact_set_and_nothing_else() {
    let profile = ToolRoster::default().into_profile();

    assert_eq!(
        profile.allowed_tools, None,
        "hide is a denylist; an allow-list here would silently drop every \
         tool nobody thought to name"
    );
    for hidden in REPLACED_TOOLS.into_iter().chain(UNSURFACED_TOOLS) {
        assert!(!profile.allows(hidden), "{hidden} should be hidden");
    }
    for offered in ["read", "write", "edit", "ls", "grep", "glob", "compact"] {
        assert!(profile.allows(offered), "{offered} should be offered");
    }
}

#[test]
fn hide_extends_the_default_set_rather_than_replacing_it() {
    let profile = ToolRoster::hide(["my_extra_tool"]).into_profile();

    assert!(
        !profile.allows("my_extra_tool"),
        "the caller's own addition"
    );
    for hidden in REPLACED_TOOLS.into_iter().chain(UNSURFACED_TOOLS) {
        assert!(
            !profile.allows(hidden),
            "{hidden} must stay hidden even though the caller named nothing about it"
        );
    }
    assert!(profile.allows("read"), "hide never touches the rest");
}

/// Item (a): `only` maps straight to mentra's allow-list, which *does* stop
/// offering a file tool it does not name — but `only` cannot reach across to
/// the runtime and un-register anything itself: the roster is a per-workspace
/// fact, and which file tools the runtime carries at all is a per-runtime one.
/// A runtime built with the default `Split` profile still carries `read`
/// underneath a roster that never offers it. The genuinely new contract, since
/// `FileToolProfile::None` exists, is that a *runtime* built with it never
/// carries the file tools at all — that half is pinned here too, the one new
/// thing this test asserts.
#[tokio::test]
async fn only_hides_the_file_tools_but_cannot_reach_the_runtimes_profile() {
    let split_runtime = crate::runtime::Runtime::builder()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
        .build()
        .expect("builds offline");

    let profile = ToolRoster::only([crate::tools::SPAWN]).into_profile();

    assert!(
        split_runtime
            .mentra_runtime()
            .tools()
            .iter()
            .any(|tool| tool.provider.name == "read"),
        "the runtime was built with the default Split profile, so `read` is \
         still on its registry no matter what this workspace's roster says"
    );
    for file_tool in ["read", "ls", "grep", "glob", "write", "edit"] {
        assert!(
            !profile.allows(file_tool),
            "{file_tool} was not named, so `only` must not offer it"
        );
    }
    assert!(
        profile.allows(crate::tools::SPAWN),
        "what was named is offered"
    );

    let none_runtime = crate::runtime::Runtime::builder()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
        .with_file_tools(mentra::FileToolProfile::None)
        .build()
        .expect("builds offline");

    assert!(
        !none_runtime
            .mentra_runtime()
            .tools()
            .iter()
            .any(|tool| tool.provider.name == "read"),
        "a runtime built with FileToolProfile::None never registers the file \
         tools at all — the way to get them off the registry, not merely out \
         of one workspace's roster"
    );
}

/// Item (b): `only` does not smuggle `spawn` or `load_skill` in on a caller's
/// behalf. A set that omits either is a legitimate, if narrower, agent.
#[test]
fn only_does_not_imply_spawn_or_load_skill() {
    let profile = ToolRoster::only(["read"]).into_profile();

    assert!(!profile.allows(crate::tools::SPAWN));
    assert!(!profile.allows("load_skill"));
    assert!(
        profile.allows("read"),
        "what was actually named still works"
    );
}

/// Item (c), the `hide` half: a per-mint hide
/// ([`super::super::Workspace::minted_agent`]) inserts directly into
/// `hidden_tools`, which `ToolProfile::allows` checks unconditionally — so it
/// suppresses a name whether or not an allow-list is in play.
#[test]
fn a_per_mint_hide_composes_on_top_of_hide() {
    let mut profile = ToolRoster::default().into_profile();
    assert!(profile.allows("read"), "read starts out offered");

    profile.hidden_tools.insert("read".to_string());

    assert!(!profile.allows("read"), "the per-mint hide wins");
}

/// Item (c), the `only` half: the same insertion still suppresses a name even
/// when it was on the allow-list, because `allows` checks `hidden_tools` after
/// `allowed_tools` regardless of which populated the profile.
#[test]
fn a_per_mint_hide_composes_on_top_of_only() {
    let mut profile = ToolRoster::only(["spawn", "mcp__sibling__tool"]).into_profile();
    assert!(
        profile.allows("mcp__sibling__tool"),
        "named, so offered so far"
    );

    profile
        .hidden_tools
        .insert("mcp__sibling__tool".to_string());

    assert!(
        !profile.allows("mcp__sibling__tool"),
        "a sibling workspace's tool loses even though this roster named it"
    );
    assert!(
        profile.allows("spawn"),
        "the hide is per-name, not a reset of the allow-list"
    );
}
