//! What a poll can tell apart: a file appearing, changing, and going.

use super::*;

#[test]
fn a_stamp_tells_a_missing_file_from_a_present_one_and_an_edit_from_neither() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a2a.json");
    let paths = vec![path.clone()];

    let missing = stamps(&paths);
    assert_eq!(missing, [Stamp::Missing]);

    std::fs::write(&path, "{}").unwrap();
    let present = stamps(&paths);
    assert!(matches!(present[0], Stamp::Present { len: 2, .. }));
    assert_eq!(stamps(&paths), present, "nothing changed, nothing differs");

    std::fs::write(&path, r#"{"agents":{}}"#).unwrap();
    assert_ne!(stamps(&paths), present, "an edit that changes the length");

    std::fs::remove_file(&path).unwrap();
    assert_eq!(stamps(&paths), missing, "and a file that goes");
}

fn present(len: u64) -> Vec<Stamp> {
    vec![Stamp::Present {
        len,
        modified: None,
        content: None,
    }]
}

#[test]
fn an_edit_that_keeps_the_length_and_the_modification_time_is_still_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a2a.json");
    let paths = vec![path.clone()];
    std::fs::write(&path, r#"{"agents":{"aaaa":{}}}"#).unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let before = stamps(&paths);

    std::fs::write(&path, r#"{"agents":{"bbbb":{}}}"#).unwrap();
    // Within one tick of a coarse clock, the second write would carry the
    // first one's time; pin it so the test does not depend on the clock.
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();

    assert_ne!(stamps(&paths), before);
}

#[test]
fn a_change_is_read_once_two_polls_agree_on_it() {
    let seen = present(10);
    let mut settling = Settling::default();

    assert!(!settling.observe(present(10), &seen), "nothing new");
    assert!(!settling.observe(present(12), &seen), "new, noted");
    assert!(
        settling.observe(present(12), &seen),
        "and still so: read it"
    );
}

#[test]
fn a_file_missing_for_one_poll_of_a_rename_and_write_save_is_never_read() {
    let seen = present(10);
    let mut settling = Settling::default();

    assert!(
        !settling.observe(vec![Stamp::Missing], &seen),
        "renamed away"
    );
    assert!(
        !settling.observe(present(14), &seen),
        "written anew: not yet"
    );
    assert!(
        settling.observe(present(14), &seen),
        "the new file, once settled"
    );
}

#[test]
fn a_change_undone_before_the_next_poll_is_forgotten() {
    let seen = present(10);
    let mut settling = Settling::default();

    assert!(!settling.observe(vec![Stamp::Missing], &seen));
    assert!(!settling.observe(present(10), &seen), "back as it was");
    assert!(
        !settling.observe(vec![Stamp::Missing], &seen),
        "a later disappearance starts the count again"
    );
}
