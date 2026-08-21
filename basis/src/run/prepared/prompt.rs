//! What a turn is asked, when a line of text is not all of it.
//!
//! basis narrows mentra's `ContentBlock` rather than re-exporting it, and the
//! narrowing is the point. mentra's block is the vocabulary of a whole
//! transcript — tool calls, tool results, the model's own thinking — and a
//! *prompt* is none of those. A caller handed the full enum could submit a
//! tool result as a user turn and get a transcript no provider will accept, so
//! this type offers the two things a person actually sends.
//!
//! # Why this exists at all
//!
//! A screenshot is a convention every agent already speaks, and mentra carries
//! one on all three wires it serves: the Responses transport inlines it as a
//! `data:` URL, Anthropic as a base64 image source, Gemini as `inlineData`.
//! basis was the only layer narrowing a prompt to a `String`, which meant a
//! host embedding it — or an ACP client with an image on the clipboard — could
//! not send one at all.
//!
//! # Bytes, not a URL
//!
//! [`PromptPart::Image`] carries the bytes and the media type that describes
//! them. mentra's `ImageSource` also has a `Url` variant, and basis does not
//! offer it: Gemini rejects a URL image outright rather than fetching it, so a
//! prompt that worked on two providers would fail on the third with an error
//! about a flow the caller never asked for. Bytes work everywhere, and a caller
//! who has a URL has to decide who fetches it — which is a decision basis
//! should not make silently on their behalf.

use mentra::ContentBlock;

/// One piece of what a turn is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PromptPart {
    Text(String),
    /// Raw bytes and the media type describing them — `image/png`,
    /// `image/jpeg`, whatever the provider accepts.
    Image {
        media_type: String,
        data: Vec<u8>,
    },
}

impl PromptPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn image(media_type: impl Into<String>, data: Vec<u8>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            data,
        }
    }

    /// Whether this part would say nothing if it were sent.
    ///
    /// Whitespace counts as nothing for text, because a prompt of spaces is
    /// the shape a template with an unfilled argument produces. Zero bytes
    /// count as nothing for an image, because there is no image.
    fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) => text.trim().is_empty(),
            Self::Image { data, .. } => data.is_empty(),
        }
    }

    fn into_block(self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::text(text),
            Self::Image { media_type, data } => ContentBlock::image_bytes(media_type, data),
        }
    }
}

impl From<String> for PromptPart {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for PromptPart {
    fn from(text: &str) -> Self {
        Self::Text(text.to_string())
    }
}

/// Whether a whole prompt would say nothing.
///
/// One empty part among several does not make a prompt empty — a client that
/// sent an image and a blank caption sent an image.
pub(super) fn says_nothing(parts: &[PromptPart]) -> bool {
    parts.iter().all(PromptPart::is_empty)
}

pub(super) fn into_blocks(parts: Vec<PromptPart>) -> Vec<ContentBlock> {
    parts.into_iter().map(PromptPart::into_block).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_of_whitespace_says_nothing() {
        // The shape a template with an unfilled argument produces, and the
        // reason `RunError::EmptyPrompt` exists.
        assert!(says_nothing(&[PromptPart::text("   \n ")]));
        assert!(says_nothing(&[]));
    }

    #[test]
    fn an_image_with_a_blank_caption_still_says_something() {
        // A client that attached a screenshot and typed nothing sent a
        // prompt. Refusing it would be refusing the whole point of this type.
        assert!(!says_nothing(&[
            PromptPart::text(""),
            PromptPart::image("image/png", vec![1, 2, 3]),
        ]));
    }

    #[test]
    fn an_image_with_no_bytes_is_not_an_image() {
        assert!(says_nothing(&[PromptPart::image("image/png", Vec::new())]));
    }

    #[test]
    fn parts_reach_mentra_in_the_order_they_were_given() {
        // "look at this [image] and tell me what changed" reads differently
        // from the same three pieces in any other order, and some providers
        // are sensitive to which side of the image the question is on.
        let blocks = into_blocks(vec![
            PromptPart::text("before"),
            PromptPart::image("image/png", vec![7]),
            PromptPart::text("after"),
        ]);

        assert!(matches!(blocks[0], ContentBlock::Text { .. }));
        assert!(matches!(blocks[1], ContentBlock::Image { .. }));
        assert!(matches!(blocks[2], ContentBlock::Text { .. }));
    }

    #[test]
    fn a_bare_string_is_a_text_part() {
        // What keeps `send_parts` and `send` the same call with two shapes.
        assert_eq!(
            PromptPart::from("hello"),
            PromptPart::Text("hello".to_string())
        );
    }
}
