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
