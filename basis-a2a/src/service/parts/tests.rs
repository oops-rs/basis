use a2a::error_code;
use serde_json::json;

use super::*;

fn message(parts: Vec<Part>) -> Message {
    Message::new(Role::User, parts)
}

#[test]
fn text_parts_are_joined_into_one_prompt() {
    let rendered = render(&message(vec![Part::text("one"), Part::text("two")])).unwrap();
    assert_eq!(rendered.prompt, "one\n\ntwo");
    assert!(rendered.only_text);
}

#[test]
fn a_data_part_reaches_the_model_as_labeled_json() {
    let rendered = render(&message(vec![
        Part::text("summarize this"),
        Part::data(json!({"rows": [1, 2]})),
    ]))
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
    let rendered = render(&message(vec![part])).unwrap();
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
    let rendered = render(&message(vec![part])).unwrap();
    assert!(rendered.prompt.contains("````\n```\nnot the end\n````"));
}

#[test]
fn a_url_binary_bytes_or_an_untyped_file_is_refused_not_dropped() {
    for part in [
        Part::url("https://example.com/a.txt").with_media_type("text/plain"),
        Part::raw(vec![0, 1, 2]).with_media_type("image/png"),
        Part::raw(b"hello".to_vec()),
    ] {
        let error = render(&message(vec![Part::text("see attached"), part])).unwrap_err();
        assert_eq!(error.code, error_code::CONTENT_TYPE_NOT_SUPPORTED);
    }
}

#[test]
fn a_textual_file_that_is_not_utf8_is_invalid() {
    let part = Part::raw(vec![0xff, 0xfe]).with_media_type("text/plain");
    let error = render(&message(vec![part])).unwrap_err();
    assert_eq!(error.code, error_code::INVALID_PARAMS);
}

#[test]
fn an_oversized_prompt_is_the_callers_error() {
    let error = render(&message(vec![Part::text("x".repeat(MAX_PROMPT + 1))])).unwrap_err();
    assert_eq!(error.code, error_code::INVALID_PARAMS);
}

#[test]
fn an_agent_role_or_an_empty_message_is_invalid() {
    let agent = Message::new(Role::Agent, vec![Part::text("hi")]);
    assert_eq!(render(&agent).unwrap_err().code, error_code::INVALID_PARAMS);
    let blank = message(vec![Part::text("  ")]);
    assert_eq!(render(&blank).unwrap_err().code, error_code::INVALID_PARAMS);
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
