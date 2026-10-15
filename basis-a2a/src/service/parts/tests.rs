use a2a::error_code;
use serde_json::json;

use super::*;

fn message(parts: Vec<Part>) -> Message {
    Message::new(Role::User, parts)
}

#[test]
fn text_parts_are_joined_into_one_prompt() {
    let rendered = render(&message(vec![Part::text("one"), Part::text("two")]), None).unwrap();
    assert_eq!(rendered.prompt, "one\n\ntwo");
    assert!(rendered.only_text);
}

#[test]
fn a_data_part_reaches_the_model_as_labeled_json() {
    let rendered = render(
        &message(vec![
            Part::text("summarize this"),
            Part::data(json!({"rows": [1, 2]})),
        ]),
        None,
    )
    .unwrap();
    assert!(!rendered.only_text);
    assert!(
        rendered
            .prompt
            .contains("Structured data from the caller (JSON):\n```json\n{"),
        "{}",
        rendered.prompt
    );
    assert!(rendered.prompt.contains("\"rows\""));
}

#[test]
fn an_inline_textual_file_is_decoded_under_its_name() {
    let part = Part::raw(b"a,b\n1,2".to_vec())
        .with_media_type("text/csv")
        .with_filename("table.csv");
    let rendered = render(&message(vec![part]), None).unwrap();
    assert!(
        rendered
            .prompt
            .starts_with("File `table.csv` from the caller (text/csv, 7 bytes):\n```\na,b"),
        "{}",
        rendered.prompt
    );
}

#[test]
fn a_fence_inside_a_file_cannot_close_it() {
    let part = Part::raw(b"```\nnot the end".to_vec()).with_media_type("text/markdown");
    let rendered = render(&message(vec![part]), None).unwrap();
    assert!(rendered.prompt.contains("````\n```\nnot the end\n````"));
}

#[test]
fn a_file_by_url_is_refused_not_dropped() {
    let part = Part::url("https://example.com/a.txt").with_media_type("text/plain");
    let error = render(&message(vec![Part::text("see attached"), part]), None).unwrap_err();
    assert_eq!(error.code, error_code::CONTENT_TYPE_NOT_SUPPORTED);
}

#[test]
fn a_file_that_is_not_text_is_saved_in_the_inbox_and_named_in_the_prompt() {
    let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
    let rendered = render(
        &message(vec![
            Part::text("why is the button red?"),
            Part::raw(png.clone()).with_filename("../../shot.png"),
            Part::raw(vec![0, 1]).with_filename("shot.png"),
        ]),
        Some(".basis/inbox/m1"),
    )
    .unwrap();
    assert!(!rendered.only_text);
    assert_eq!(
        rendered.files,
        vec![
            Received {
                path: ".basis/inbox/m1/shot.png".into(),
                data: png,
            },
            Received {
                path: ".basis/inbox/m1/shot-2.png".into(),
                data: vec![0, 1],
            },
        ],
        "a name cannot climb out, and two of one name do not collide"
    );
    assert!(
        rendered.prompt.contains(
            "File `shot.png` from the caller (image/png, 12 bytes), saved at \
             `.basis/inbox/m1/shot.png`"
        ),
        "{}",
        rendered.prompt
    );
}

#[test]
fn a_host_is_told_of_a_file_and_handed_the_parts() {
    let rendered = render(
        &message(vec![
            Part::raw(vec![0, 1, 2]).with_media_type("application/pdf"),
        ]),
        None,
    )
    .unwrap();
    assert!(rendered.files.is_empty());
    assert_eq!(
        rendered.prompt,
        "File `file-1.pdf` from the caller (application/pdf, 3 bytes)."
    );
}

#[test]
fn an_untyped_file_is_typed_by_its_bytes() {
    let rendered = render(&message(vec![Part::raw(b"hello".to_vec())]), None).unwrap();
    assert!(
        rendered
            .prompt
            .starts_with("File `untitled` from the caller (text/plain, 5 bytes):"),
        "{}",
        rendered.prompt
    );
}

#[test]
fn a_textual_file_that_is_not_utf8_is_invalid() {
    let part = Part::raw(vec![0xff, 0xfe]).with_media_type("text/plain");
    let error = render(&message(vec![part]), None).unwrap_err();
    assert_eq!(error.code, error_code::INVALID_PARAMS);
}

#[test]
fn an_oversized_prompt_is_the_callers_error() {
    let error = render(&message(vec![Part::text("x".repeat(MAX_PROMPT + 1))]), None).unwrap_err();
    assert_eq!(error.code, error_code::INVALID_PARAMS);
}

#[test]
fn an_agent_role_or_an_empty_message_is_invalid() {
    let agent = Message::new(Role::Agent, vec![Part::text("hi")]);
    assert_eq!(
        render(&agent, None).unwrap_err().code,
        error_code::INVALID_PARAMS
    );
    let blank = message(vec![Part::text("  ")]);
    assert_eq!(
        render(&blank, None).unwrap_err().code,
        error_code::INVALID_PARAMS
    );
}

#[test]
fn textual_media_types_are_text_json_xml_and_yaml() {
    for textual in [
        "text/plain",
        "TEXT/Markdown; charset=utf-8",
        "application/json",
        "application/ld+json",
        "application/xml",
        "application/atom+xml",
        "application/yaml",
        "application/x-yaml",
        "application/openapi+yaml",
    ] {
        assert!(is_textual(textual), "{textual}");
    }
    for binary in [
        "image/png",
        "application/pdf",
        "application/octet-stream",
        "application/+json",
        "text",
        "text/",
        "",
    ] {
        assert!(!is_textual(binary), "{binary}");
    }
}
