use std::ffi::OsStr;
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use codex_api::ReqwestTransport;
use codex_api::SearchClient;
use codex_api::SearchCommands;
use codex_api::SearchQuery;
use codex_api::SearchRequest;
use codex_api::SearchResponse;
use codex_api::SearchSettings;
use codex_core::X_CODEX_TURN_METADATA_HEADER;
use codex_core::web_search_action_detail;
use codex_extension_api::ExtensionTurnItem;
use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_api::parse_tool_input_schema_without_compaction;
use codex_extension_items::ExtensionItem;
use codex_extension_items::web_search::WebSearchAction;
use codex_extension_items::web_search::WebSearchItem;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_login::default_client::ClientRedirectPolicy;
use codex_login::default_client::add_originator_header;
use codex_login::default_client::create_client_for_route_async;
use codex_login::default_client::create_transport_for_routes_async;
use codex_model_provider::ACCOUNT_ROUTING_HEADER;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::WorkspaceRoutingContext;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::WebSearchAction as CoreWebSearchAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolExposure;
use codex_tools::default_namespace_description;
use http::HeaderMap;
use http::HeaderValue;
use serde::Deserialize;
use url::Url;

use crate::history::recent_input;
use crate::image_output::ImageSearchOutput;
use crate::image_output::code_mode_result_schema;
use crate::output::SearchOutput;
use crate::schema::commands_schema;

pub(crate) const WEB_NAMESPACE: &str = "web";
pub(crate) const RUN_TOOL_NAME: &str = "run";
const WEB_RUN_DESCRIPTION: &str = include_str!("../web_run_description.md");
const RESULTS_PAYLOAD_BYTES_METRIC: &str = "codex.web_search.results.payload_bytes";
const MAX_SEARCH_ERROR_BYTES: usize = 1024;
// Answers web.run with a local program instead of OpenAI's alpha/search: it
// reads the SearchRequest JSON on stdin and prints a SearchResponse.
const WEB_RUN_COMMAND_ENV: &str = "CODEX_WEB_RUN_COMMAND";
const WEB_RUN_COMMAND_TIMEOUT: Duration = Duration::from_secs(180);

fn local_command() -> Option<OsString> {
    std::env::var_os(WEB_RUN_COMMAND_ENV).filter(|value| !value.is_empty())
}

pub(crate) struct WebSearchTool {
    pub(crate) session_id: String,
    pub(crate) http_client_factory: HttpClientFactory,
    pub(crate) provider: SharedModelProvider,
    pub(crate) workspace_routing: Arc<WorkspaceRoutingContext>,
    pub(crate) settings: SearchSettings,
    pub(crate) supports_literature_search: bool,
    pub(crate) originator: Option<String>,
}

impl ToolExecutor for WebSearchTool {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(WEB_NAMESPACE, RUN_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        // parse schema without compaction that removes field metadata/descriptions to match hosted tool definition
        let schema = commands_schema(self.supports_literature_search);
        let parameters = match parse_tool_input_schema_without_compaction(&schema) {
            Ok(parameters) => parameters,
            Err(err) => panic!("search command schema should parse: {err}"),
        };

        ToolSpec::Namespace(ResponsesApiNamespace {
            name: WEB_NAMESPACE.to_string(),
            description: default_namespace_description(WEB_NAMESPACE),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: RUN_TOOL_NAME.to_string(),
                description: WEB_RUN_DESCRIPTION.to_string(),
                strict: false,
                parameters,
                // Only a local command's answer can be anything but text.
                output_schema: local_command().map(|_| code_mode_result_schema().into()),
                defer_loading: None,
            })],
        })
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, call: ToolCall<'a>) -> codex_extension_api::ToolExecutorFuture<'a> {
        Box::pin(self.handle_call(call))
    }
}

impl WebSearchTool {
    async fn handle_call(
        &self,
        call: ToolCall<'_>,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let commands = parse_commands(call.function_arguments()?, self.supports_literature_search)?;
        let command_action = command_action(&commands);
        let local_command = local_command();
        let client = if local_command.is_some() {
            None
        } else {
            let setup = codex_model_provider::resolve_request(
                self.provider.as_ref(),
                /*scope*/ None,
                Some(&self.workspace_routing),
                self.http_client_factory.clone(),
            )
            .await
            .map_err(|err| FunctionCallError::RespondToModel(search_error_message(err)))?;
            let transport = if setup.redirect_policy == ClientRedirectPolicy::Reject
                || setup
                    .api_provider
                    .headers
                    .contains_key(ACCOUNT_ROUTING_HEADER)
            {
                create_client_for_route_async(
                    setup.http_client_factory,
                    setup.api_provider.url_for_path("alpha/search"),
                    ClientRouteClass::Api,
                    ClientRedirectPolicy::Reject,
                )
                .await
                .map(ReqwestTransport::from_http_client)
            } else {
                create_transport_for_routes_async(
                    setup.http_client_factory,
                    ClientRouteClass::Api,
                    setup.redirect_policy,
                )
                .await
            }
            .map_err(|err| FunctionCallError::RespondToModel(search_error_message(err)))?;
            Some(SearchClient::new(
                transport,
                setup.api_provider,
                setup.api_auth,
            ))
        };
        let request = SearchRequest {
            id: self.session_id.clone(),
            model: call.model.clone(),
            reasoning: None,
            input: recent_input(call.conversation_history.items()),
            commands: Some(commands),
            settings: Some(self.settings.clone()),
            max_output_tokens: Some(
                u64::try_from(call.truncation_policy.token_budget()).unwrap_or(u64::MAX),
            ),
        };
        let extra_headers = search_request_headers(
            self.originator.as_deref(),
            call.codex_turn_metadata.as_deref(),
        );
        call.turn_item_emitter
            .emit_started(extension_turn_item(
                WebSearchItem {
                    id: call.call_id.clone(),
                    query: String::new(),
                    action: None,
                    results: None,
                    output: None,
                },
                EventMsg::WebSearchBegin(WebSearchBeginEvent {
                    call_id: call.call_id.clone(),
                }),
            ))
            .await;
        let searched = match (&local_command, &client) {
            (Some(command), _) => run_local_search(command, &request).await,
            (None, Some(client)) => client
                .search(&request, extra_headers)
                .await
                .map(|response| (response, None))
                .map_err(|err| err.to_string()),
            (None, None) => Err("web.run has no search backend".to_string()),
        };
        let (output, results, content) = match searched {
            Ok((response, content)) => (Ok(response.output), response.results, content),
            Err(err) => (Err(search_error_message(err)), None, None),
        };
        if let Some(results) = results.as_ref()
            && let Some(metrics) = codex_otel::global()
            && let Ok(payload) = serde_json::to_vec(results)
        {
            let payload_bytes = i64::try_from(payload.len()).unwrap_or(i64::MAX);
            let _ = metrics.histogram(RESULTS_PAYLOAD_BYTES_METRIC, payload_bytes, &[]);
        }
        let (output, shown) = search_output(output, content);
        let legacy_action = match &command_action {
            WebSearchAction::Search { query, queries } => CoreWebSearchAction::Search {
                query: query.clone(),
                queries: queries.clone(),
            },
            WebSearchAction::OpenPage { url } => CoreWebSearchAction::OpenPage { url: url.clone() },
            WebSearchAction::FindInPage { url, pattern } => CoreWebSearchAction::FindInPage {
                url: url.clone(),
                pattern: pattern.clone(),
            },
            WebSearchAction::Other => CoreWebSearchAction::Other,
        };
        let query = web_search_action_detail(&legacy_action);
        call.turn_item_emitter
            .emit_completed(extension_turn_item(
                WebSearchItem {
                    id: call.call_id.clone(),
                    query: query.clone(),
                    action: Some(command_action),
                    results: results.clone(),
                    output: Some(shown),
                },
                EventMsg::WebSearchEnd(WebSearchEndEvent {
                    call_id: call.call_id.clone(),
                    query,
                    action: legacy_action,
                    results,
                }),
            ))
            .await;

        Ok(output)
    }
}

async fn run_local_search(
    command: &OsStr,
    request: &SearchRequest,
) -> Result<(SearchResponse, Option<Vec<FunctionCallOutputContentItem>>), String> {
    use tokio::io::AsyncWriteExt;

    let name = command.to_string_lossy();
    let body = serde_json::to_vec(request).map_err(|err| format!("web.run request: {err}"))?;
    let mut child = tokio::process::Command::new(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("{name}: {err}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{name}: no stdin"))?;
    // The command may stop reading, or fill an output pipe, before it has read
    // the whole request. Its output is read while the request is written, and
    // the timeout covers both.
    let send = async {
        let sent = stdin.write_all(&body).await;
        drop(stdin);
        sent
    };
    let (sent, output) = tokio::time::timeout(WEB_RUN_COMMAND_TIMEOUT, async {
        tokio::join!(send, child.wait_with_output())
    })
    .await
    .map_err(|_| {
        format!(
            "{name}: no answer within {}s",
            WEB_RUN_COMMAND_TIMEOUT.as_secs()
        )
    })?;
    let output = output.map_err(|err| format!("{name}: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.trim();
        return Err(if reason.is_empty() {
            format!("{name}: {}", output.status)
        } else {
            reason.to_string()
        });
    }
    sent.map_err(|err| format!("{name}: {err}"))?;
    parse_local_response(&output.stdout).map_err(|err| format!("{name}: invalid response: {err}"))
}

const CONTENT_SHAPE_ERROR: &str = "content: expected a list of input_text items and input_image items with an inline data: image URL";

/// What a local command adds to the service's SearchResponse.
#[derive(Deserialize)]
struct LocalContent {
    #[serde(default)]
    content: Option<Vec<LocalContentItem>>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LocalContentItem {
    InputText { text: String },
    InputImage { image_url: String },
}

/// Reads a local command's answer: the service's SearchResponse and, when the
/// answer carries `content`, the items the model gets in place of `output`.
fn parse_local_response(
    stdout: &[u8],
) -> Result<(SearchResponse, Option<Vec<FunctionCallOutputContentItem>>), String> {
    let response: SearchResponse = serde_json::from_slice(stdout).map_err(|err| err.to_string())?;
    // SearchResponse is the service's type and has no place for `content`, so a
    // second pass reads that field alone. Its error names the shape and never
    // the value, which may be image data.
    let LocalContent { content } =
        serde_json::from_slice(stdout).map_err(|_| CONTENT_SHAPE_ERROR.to_string())?;
    let content = content
        .map(|items| {
            items
                .into_iter()
                .map(|item| match item {
                    LocalContentItem::InputText { text } => {
                        Some(FunctionCallOutputContentItem::InputText { text })
                    }
                    LocalContentItem::InputImage { image_url } => is_inline_image(&image_url)
                        .then_some(FunctionCallOutputContentItem::InputImage {
                            image: ImageReference::Inline { image_url },
                            detail: Some(DEFAULT_IMAGE_DETAIL),
                        }),
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| CONTENT_SHAPE_ERROR.to_string())
        })
        .transpose()?;
    Ok((response, content))
}

/// Tells whether the URL is `data:image/<type>;base64,<data>` for an image type
/// the model takes, with padded base64 data.
fn is_inline_image(url: &str) -> bool {
    let Some((head, data)) = url.split_once(',') else {
        return false;
    };
    let head = head.to_ascii_lowercase();
    let known_type = head
        .strip_prefix("data:image/")
        .and_then(|rest| rest.strip_suffix(";base64"))
        .is_some_and(|media_type| matches!(media_type, "png" | "jpeg" | "gif" | "webp"));
    let digits = data.trim_end_matches('=');
    known_type
        && !digits.is_empty()
        && data.len().is_multiple_of(4)
        && data.len() - digits.len() <= 2
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

/// Chooses what the model gets, and the text that stands for it in the turn
/// item: the content of an answer with images, and otherwise the text or the
/// error as before.
fn search_output(
    output: Result<String, String>,
    content: Option<Vec<FunctionCallOutputContentItem>>,
) -> (Box<dyn ToolOutput>, String) {
    let images = match &output {
        Ok(_) => content.and_then(ImageSearchOutput::new),
        Err(_) => None,
    };
    match images {
        Some(images) => {
            let shown = images.display_text();
            (Box::new(images), shown)
        }
        None => {
            let shown = match &output {
                Ok(text) | Err(text) => text.clone(),
            };
            (Box::new(SearchOutput::new(output)), shown)
        }
    }
}

fn search_error_message(error: impl std::fmt::Display) -> String {
    let mut message = error.to_string();
    if message.len() > MAX_SEARCH_ERROR_BYTES {
        const SUFFIX: &str = "… [truncated]";
        message.truncate(message.floor_char_boundary(MAX_SEARCH_ERROR_BYTES - SUFFIX.len()));
        message.push_str(SUFFIX);
    }
    message
}

fn search_request_headers(originator: Option<&str>, turn_metadata: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(turn_metadata) = turn_metadata
        && let Ok(header_value) = HeaderValue::from_str(turn_metadata)
    {
        headers.insert(X_CODEX_TURN_METADATA_HEADER, header_value);
    }

    if let Some(originator) = originator {
        add_originator_header(&mut headers, originator);
    }
    headers
}

fn parse_commands(
    arguments: &str,
    supports_literature_search: bool,
) -> Result<SearchCommands, FunctionCallError> {
    if arguments.trim().is_empty() {
        return Ok(SearchCommands::default());
    }

    let commands: SearchCommands = serde_json::from_str(arguments)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    if !supports_literature_search && commands.literature_search_query.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "literature_search_query is unavailable for this model".to_string(),
        ));
    }
    Ok(commands)
}

fn command_action(commands: &SearchCommands) -> WebSearchAction {
    commands
        .search_query
        .as_deref()
        .and_then(query_action)
        .or_else(|| {
            commands
                .literature_search_query
                .as_deref()
                .and_then(query_action)
        })
        .or_else(|| commands.image_query.as_deref().and_then(query_action))
        .or_else(|| {
            commands
                .open
                .as_deref()
                .and_then(|operations| operations.first())
                .and_then(|operation| {
                    literal_url(&operation.ref_id)
                        .map(|url| WebSearchAction::OpenPage { url: Some(url) })
                })
        })
        .or_else(|| {
            commands
                .find
                .as_deref()
                .and_then(|operations| operations.first())
                .map(|operation| WebSearchAction::FindInPage {
                    url: literal_url(&operation.ref_id),
                    pattern: Some(operation.pattern.clone()),
                })
        })
        .unwrap_or(WebSearchAction::Other)
}

fn query_action(queries: &[SearchQuery]) -> Option<WebSearchAction> {
    match queries {
        [] => None,
        [query] => Some(WebSearchAction::Search {
            query: Some(query.q.clone()),
            queries: None,
        }),
        queries => Some(WebSearchAction::Search {
            query: None,
            queries: Some(queries.iter().map(|query| query.q.clone()).collect()),
        }),
    }
}

fn literal_url(ref_id: &str) -> Option<String> {
    Url::parse(ref_id).is_ok().then(|| ref_id.to_string())
}

fn extension_turn_item(item: WebSearchItem, legacy_event: EventMsg) -> ExtensionTurnItem {
    ExtensionTurnItem {
        item: ExtensionItem::WebSearch(item),
        legacy_events: vec![legacy_event],
    }
}

#[cfg(test)]
mod tests {
    use codex_api::SearchCommands;
    use codex_extension_items::web_search::WebSearchAction;
    use pretty_assertions::assert_eq;

    use super::CONTENT_SHAPE_ERROR;
    use super::command_action;
    use super::parse_commands;
    use super::parse_local_response;
    use super::search_error_message;
    use super::search_output;
    use super::search_request_headers;
    use codex_core::X_CODEX_TURN_METADATA_HEADER;
    use codex_extension_api::FunctionCallError;
    use codex_extension_api::ToolOutput;
    use codex_extension_api::ToolPayload;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::ImageDetail;
    use codex_protocol::models::ImageReference;
    use codex_protocol::models::ResponseInputItem;
    use codex_tools::CodeModeToolResult;
    use serde_json::json;

    #[test]
    fn local_response_content_carries_text_and_inline_images() {
        let (response, content) = parse_local_response(
            br#"{"encrypted_output":null,"output":"page","results":[],"content":[
                {"type":"input_text","text":"page"},
                {"type":"input_image","image_url":"data:image/png;base64,AAAA","detail":"low"},
                {"type":"input_image","image_url":"DATA:image/jpeg;base64,BBBB"}
            ]}"#,
        )
        .expect("a response with content");
        assert_eq!(response.output, "page");
        // The command's own detail is not taken: `low` would turn the image into a placeholder.
        let image = |image_url: &str| FunctionCallOutputContentItem::InputImage {
            image: ImageReference::Inline {
                image_url: image_url.to_string(),
            },
            detail: Some(ImageDetail::High),
        };
        assert_eq!(
            content,
            Some(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "page".to_string(),
                },
                image("data:image/png;base64,AAAA"),
                image("DATA:image/jpeg;base64,BBBB"),
            ])
        );

        let (response, content) =
            parse_local_response(br#"{"encrypted_output":null,"output":"page"}"#)
                .expect("a response without content");
        assert_eq!(response.output, "page");
        assert_eq!(content, None);
    }

    #[test]
    fn the_typed_parse_still_decides_what_a_valid_answer_is() {
        assert!(
            parse_local_response(br#"{"encrypted_output":null,"output":"a","output":"b"}"#)
                .is_err_and(|err| err.contains("duplicate field"))
        );
    }

    #[test]
    fn fields_the_typed_parse_skips_do_not_disturb_the_content() {
        // The service's parser skips unknown fields however deep they nest, and so
        // does the content pass: the images beside such a field still arrive.
        let nested = |content: &str| {
            format!(
                r#"{{"encrypted_output":null,"output":"page","extra":{}{}{content}}}"#,
                "[".repeat(200),
                "]".repeat(200)
            )
        };
        let (response, content) =
            parse_local_response(nested("").as_bytes()).expect("an answer without content");
        assert_eq!((response.output.as_str(), content), ("page", None));
        let image =
            r#","content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]"#;
        let (_, content) =
            parse_local_response(nested(image).as_bytes()).expect("an answer with content");
        assert_eq!(content.map(|items| items.len()), Some(1));
    }

    #[test]
    fn a_repeated_content_field_is_refused() {
        // Keeping the last one would let a valid list hide an invalid one.
        let response = concat!(
            r#"{"encrypted_output":null,"output":"page","content":"data:image/png;base64,AAAA","#,
            r#""content":[{"type":"input_text","text":"page"}]}"#
        );
        assert_eq!(
            parse_local_response(response.as_bytes()).map(|_| ()),
            Err(CONTENT_SHAPE_ERROR.to_string())
        );
    }

    #[test]
    fn local_response_content_rejects_anything_but_text_and_inline_images() {
        for content in [
            r#"[{"type":"input_image","image_url":"https://example.com/page.png"}]"#,
            r#"[{"type":"input_image","image_url":"data:text/html;base64,AAAA"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64,"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/;base64,AAAA"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png,AAAA"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64,%%%%"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64,AAA"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64,A=AA"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64,===="}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64,A==="}]"#,
            r#"[{"type":"input_image","image_url":"data:image/svg+xml;base64,AAAA"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/p ng;base64,AAAA"}]"#,
            r#"[{"type":"input_image","image_url":7}]"#,
            r#"[7]"#,
            r#"[{"type":"input_image","file_id":"file-1"}]"#,
            r#"[{"type":"input_audio","audio_url":"data:audio/wav;base64,AAAA"}]"#,
            r#"[{"type":"encrypted_content","encrypted_content":"AAAA"}]"#,
            r#"[{"type":"input_text"}]"#,
            r#"["data:image/png;base64,AAAA"]"#,
            r#""data:image/png;base64,AAAA""#,
        ] {
            let response =
                format!(r#"{{"encrypted_output":null,"output":"page","content":{content}}}"#);
            // The same words for every violation: none of the value reaches the model.
            assert_eq!(
                parse_local_response(response.as_bytes()).map(|_| ()),
                Err(CONTENT_SHAPE_ERROR.to_string()),
                "{content}"
            );
        }
    }

    #[test]
    fn an_answer_with_images_gives_the_model_its_content() {
        let payload = ToolPayload::Function {
            arguments: "{}".to_string(),
        };
        let text = |text: &str| FunctionCallOutputContentItem::InputText {
            text: text.to_string(),
        };
        let image = FunctionCallOutputContentItem::InputImage {
            image: ImageReference::Inline {
                image_url: "data:image/png;base64,AAAA".to_string(),
            },
            detail: Some(ImageDetail::High),
        };
        let sent = |output: Box<dyn ToolOutput>| {
            let ResponseInputItem::FunctionCallOutput { output: sent, .. } =
                output.to_response_item("call-1", &payload)
            else {
                panic!("expected a function call output");
            };
            sent
        };

        let content = vec![text("from content"), image.clone(), text(" after")];
        let (output, shown) = search_output(Ok("from output".to_string()), Some(content.clone()));
        assert_eq!(
            output.code_mode_result(&payload),
            CodeModeToolResult::from(json!([
                "from content",
                { "image_url": "data:image/png;base64,AAAA", "detail": "high" },
                " after",
            ]))
        );
        assert_eq!(sent(output).content_items(), Some(content.as_slice()));
        // The turn item names the image and holds none of its data.
        assert_eq!(shown, "from content[image] after");

        // Without an image, or without content, the text stands as before.
        for content in [Some(vec![text("from content")]), None] {
            let (output, shown) = search_output(Ok("from output".to_string()), content);
            assert_eq!(
                output.code_mode_result(&payload),
                CodeModeToolResult::from(json!("from output"))
            );
            assert_eq!(
                sent(output).content_items(),
                Some([text("from output")].as_slice())
            );
            assert_eq!(shown, "from output");
        }

        // A failed call carries no content.
        let (output, shown) = search_output(Err("failed".to_string()), Some(vec![image]));
        assert_eq!(
            output.code_mode_result(&payload),
            CodeModeToolResult::from(json!("failed"))
        );
        let sent = sent(output);
        assert_eq!(
            (sent.text_content(), sent.success),
            (Some("failed"), Some(false))
        );
        assert_eq!(shown, "failed");
    }

    #[test]
    fn search_errors_preserve_short_messages_and_bound_utf8_output() {
        let fits = "x".repeat(/*n*/ 1024);
        assert_eq!(search_error_message(&fits), fits);

        let oversized = "🦀".repeat(/*n*/ 512);
        let message = search_error_message(&oversized);
        assert!(message.len() <= 1024);
        assert!(oversized.starts_with(message.strip_suffix("… [truncated]").unwrap()));
    }

    #[test]
    fn search_request_headers_forward_thread_originator_and_turn_metadata() {
        let headers = search_request_headers(Some("chatgpt_cca"), Some("turn-metadata"));
        assert_eq!(
            headers
                .get("originator")
                .and_then(|value| value.to_str().ok()),
            Some("chatgpt_cca")
        );
        assert_eq!(
            headers
                .get(X_CODEX_TURN_METADATA_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("turn-metadata")
        );
    }

    #[test]
    fn rejects_literature_search_without_the_model_capability() {
        let arguments =
            r#"{"search_query":[{"q":"regular"}],"literature_search_query":[{"q":"papers"}]}"#;
        assert!(matches!(
            parse_commands(arguments, /*supports_literature_search*/ false),
            Err(FunctionCallError::RespondToModel(message))
                if message == "literature_search_query is unavailable for this model"
        ));
    }

    #[test]
    fn command_action_reports_queries_and_navigation_detail() {
        let cases = [
            (
                r#"{"image_query":[{"q":"waterfalls"},{"q":"mountains"}]}"#,
                WebSearchAction::Search {
                    query: None,
                    queries: Some(vec!["waterfalls".to_string(), "mountains".to_string()]),
                },
            ),
            (
                r#"{"open":[{"ref_id":"https://example.com/docs"}]}"#,
                WebSearchAction::OpenPage {
                    url: Some("https://example.com/docs".to_string()),
                },
            ),
            (
                r#"{"find":[{"ref_id":"https://example.com/docs","pattern":"install"}]}"#,
                WebSearchAction::FindInPage {
                    url: Some("https://example.com/docs".to_string()),
                    pattern: Some("install".to_string()),
                },
            ),
            (
                r#"{"find":[{"ref_id":"turn0search0","pattern":"install"}]}"#,
                WebSearchAction::FindInPage {
                    url: None,
                    pattern: Some("install".to_string()),
                },
            ),
            (
                r#"{"open":[{"ref_id":"turn0search0"}]}"#,
                WebSearchAction::Other,
            ),
        ];

        for (arguments, expected) in cases {
            let commands: SearchCommands =
                serde_json::from_str(arguments).expect("valid search command arguments");
            assert_eq!(command_action(&commands), expected);
        }
    }
}
