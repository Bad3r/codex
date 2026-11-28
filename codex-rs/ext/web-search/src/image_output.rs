use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseInputItem;
use codex_tools::CodeModeToolResult;
use serde_json::Value as JsonValue;
use serde_json::json;

/// A local command's answer that carries images: the model gets the answer's
/// content items, text and images in the command's order, in place of its text.
pub(crate) struct ImageSearchOutput {
    items: Vec<FunctionCallOutputContentItem>,
}

impl ImageSearchOutput {
    /// Returns the output for content that holds an image, and `None` for
    /// content without one, which the answer's text already covers.
    pub(crate) fn new(items: Vec<FunctionCallOutputContentItem>) -> Option<Self> {
        items
            .iter()
            .any(|item| matches!(item, FunctionCallOutputContentItem::InputImage { .. }))
            .then_some(Self { items })
    }
}

impl ImageSearchOutput {
    /// Returns the answer as text for the turn item: its text, with a marker
    /// where each image goes, so that no image data lands in a transcript.
    pub(crate) fn display_text(&self) -> String {
        self.items
            .iter()
            .map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => text.as_str(),
                FunctionCallOutputContentItem::InputImage { .. } => "[image]",
                FunctionCallOutputContentItem::InputAudio { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => "",
            })
            .collect()
    }
}

/// What `web.run` resolves to in Code Mode when a local command answers it: the
/// text as before, or the parts of an answer with images.
pub(crate) fn code_mode_result_schema() -> JsonValue {
    json!({
        "anyOf": [
            { "type": "string" },
            {
                "type": "array",
                "items": {
                    "anyOf": [
                        { "type": "string" },
                        {
                            "type": "object",
                            "properties": {
                                "image_url": {
                                    "type": "string",
                                    "description": "A page image that belongs to the text before it. Pass this object to image() to see it."
                                }
                            },
                            "required": ["image_url"]
                        }
                    ]
                }
            }
        ]
    })
}

impl ToolOutput for ImageSearchOutput {
    fn log_output(&self) -> String {
        "[standalone web search output with images]".to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn contains_external_context(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_content_items(self.items.clone()),
        }
    }

    /// Code Mode scripts emit what they return, so the images stay apart from
    /// the text: each is an object the script passes to `image()`.
    fn code_mode_result(&self, _payload: &ToolPayload) -> CodeModeToolResult {
        CodeModeToolResult::from(JsonValue::Array(
            self.items
                .iter()
                .filter_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        Some(JsonValue::String(text.clone()))
                    }
                    FunctionCallOutputContentItem::InputImage {
                        image: ImageReference::Inline { image_url },
                        detail,
                    } => Some(json!({ "image_url": image_url, "detail": detail })),
                    FunctionCallOutputContentItem::InputImage {
                        image: ImageReference::File { .. },
                        ..
                    }
                    | FunctionCallOutputContentItem::InputAudio { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                })
                .collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use codex_extension_api::ToolOutput;
    use codex_extension_api::ToolPayload;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::ImageDetail;
    use codex_protocol::models::ImageReference;
    use codex_protocol::models::ResponseInputItem;
    use codex_tools::CodeModeToolResult;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::ImageSearchOutput;

    fn text(text: &str) -> FunctionCallOutputContentItem {
        FunctionCallOutputContentItem::InputText {
            text: text.to_string(),
        }
    }

    fn image(image_url: &str) -> FunctionCallOutputContentItem {
        FunctionCallOutputContentItem::InputImage {
            image: ImageReference::Inline {
                image_url: image_url.to_string(),
            },
            detail: Some(ImageDetail::High),
        }
    }

    #[test]
    fn content_without_an_image_is_left_to_the_text_output() {
        assert!(ImageSearchOutput::new(vec![text("page")]).is_none());
        assert!(ImageSearchOutput::new(Vec::new()).is_none());
    }

    #[test]
    fn the_model_gets_text_and_images_in_the_commands_order() {
        let items = vec![
            text("first page"),
            image("data:image/png;base64,AAAA"),
            text("second page"),
            image("data:image/jpeg;base64,BBBB"),
        ];
        let output = ImageSearchOutput::new(items.clone()).expect("content with images");
        let payload = ToolPayload::Function {
            arguments: "{}".to_string(),
        };

        let ResponseInputItem::FunctionCallOutput {
            call_id,
            output: sent,
        } = output.to_response_item("call-1", &payload)
        else {
            panic!("expected a function call output");
        };
        assert_eq!(call_id, "call-1");
        assert_eq!(sent.content_items(), Some(items.as_slice()));
        assert_eq!(sent.success, None);

        assert_eq!(
            output.code_mode_result(&payload),
            CodeModeToolResult::from(json!([
                "first page",
                { "image_url": "data:image/png;base64,AAAA", "detail": "high" },
                "second page",
                { "image_url": "data:image/jpeg;base64,BBBB", "detail": "high" },
            ]))
        );
        assert!(output.contains_external_context());
        assert!(!output.log_output().contains("base64"));
    }
}
