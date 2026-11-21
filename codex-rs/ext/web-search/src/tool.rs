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
// Set for the local command: its response may carry `content`, the items given
// to the model in place of `output`, so that an answer can include images.
const WEB_RUN_IMAGES_ENV: &str = "CODEX_WEB_RUN_IMAGES";

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

impl<'call> ToolExecutor<ToolCall<'call>> for WebSearchTool {
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

    fn handle<'a>(&'a self, call: ToolCall<'call>) -> codex_extension_api::ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
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
                    output: Some(match &output {
                        Ok(text) | Err(text) => text.clone(),
                    }),
                },
                EventMsg::WebSearchEnd(WebSearchEndEvent {
                    call_id: call.call_id.clone(),
                    query,
                    action: legacy_action,
                    results,
                }),
            ))
            .await;

        Ok(search_output(output, content))
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
        .env(WEB_RUN_IMAGES_ENV, "1")
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

/// Reads a local command's answer: the service's SearchResponse and, when the
/// answer carries `content`, the items the model gets in place of `output`.
fn parse_local_response(
    stdout: &[u8],
) -> Result<(SearchResponse, Option<Vec<FunctionCallOutputContentItem>>), String> {
    let response: SearchResponse = serde_json::from_slice(stdout).map_err(|err| err.to_string())?;
    // SearchResponse is the service's type and has no place for `content`, so a
    // second pass reads it; the first decides alone whether the answer is valid.
    let content = match serde_json::from_slice::<serde_json::Value>(stdout) {
        Ok(serde_json::Value::Object(mut fields)) => fields.remove("content"),
        Ok(_) | Err(_) => None,
    };
    // The error names the shape and never the value, which may be image data.
    let content = content
        .map(|content| parse_content(&content).ok_or_else(|| CONTENT_SHAPE_ERROR.to_string()))
        .transpose()?;
    Ok((response, content))
}

fn parse_content(content: &serde_json::Value) -> Option<Vec<FunctionCallOutputContentItem>> {
    content
        .as_array()?
        .iter()
        .map(|item| match item.get("type")?.as_str()? {
            "input_text" => Some(FunctionCallOutputContentItem::InputText {
                text: item.get("text")?.as_str()?.to_string(),
            }),
            "input_image" => {
                let image_url = item.get("image_url")?.as_str()?;
                is_inline_image(image_url).then(|| FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: image_url.to_string(),
                    },
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                })
            }
            _ => None,
        })
        .collect()
}

fn is_inline_image(url: &str) -> bool {
    const PREFIX: &str = "data:image/";
    url.split_once(',').is_some_and(|(head, data)| {
        !data.is_empty()
            && head
                .get(..PREFIX.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
    })
}

/// Chooses what the model gets: the content of an answer with images, and
/// otherwise the text or the error as before.
fn search_output(
    output: Result<String, String>,
    content: Option<Vec<FunctionCallOutputContentItem>>,
) -> Box<dyn ToolOutput> {
    let images = match &output {
        Ok(_) => content.and_then(ImageSearchOutput::new),
        Err(_) => None,
    };
    match images {
        Some(images) => Box::new(images),
        None => Box::new(SearchOutput::new(output)),
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
    use super::run_local_search;
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
    fn local_response_without_content_is_read_as_before() {
        // The typed parse alone decides: a repeated field still fails, and a field
        // the second pass cannot read does not.
        assert!(
            parse_local_response(br#"{"encrypted_output":null,"output":"a","output":"b"}"#)
                .is_err_and(|err| err.contains("duplicate field"))
        );
        let nested = format!(
            r#"{{"encrypted_output":null,"output":"page","extra":{}{}}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        let (response, content) =
            parse_local_response(nested.as_bytes()).expect("deep ignored fields are skipped");
        assert_eq!(response.output, "page");
        assert_eq!(content, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_command_is_told_that_content_is_accepted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let command = dir.path().join("web-run");
        std::fs::write(
            &command,
            concat!(
                "#!/bin/sh\ncat >/dev/null\n",
                r#"printf '{"encrypted_output":null,"output":"images=%s"}' "$CODEX_WEB_RUN_IMAGES""#,
                "\n",
            ),
        )
        .expect("write the command");
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755))
            .expect("make the command executable");
        let request = codex_api::SearchRequest {
            id: "session".to_string(),
            model: "model".to_string(),
            reasoning: None,
            input: None,
            commands: None,
            settings: None,
            max_output_tokens: None,
        };

        let (response, content) = run_local_search(command.as_os_str(), &request)
            .await
            .expect("the command's response");
        assert_eq!(response.output, "images=1");
        assert_eq!(content, None);
    }

    #[test]
    fn local_response_content_rejects_anything_but_text_and_inline_images() {
        for content in [
            r#"[{"type":"input_image","image_url":"https://example.com/page.png"}]"#,
            r#"[{"type":"input_image","image_url":"data:text/html;base64,AAAA"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64"}]"#,
            r#"[{"type":"input_image","image_url":"data:image/png;base64,"}]"#,
            r#"[{"type":"input_image","file_id":"file-1"}]"#,
            r#"[{"type":"input_audio","audio_url":"data:audio/wav;base64,AAAA"}]"#,
            r#"[{"type":"encrypted_content","encrypted_content":"AAAA"}]"#,
            r#"[{"type":"input_text"}]"#,
            r#"["data:image/png;base64,AAAA"]"#,
            r#""data:image/png;base64,AAAA""#,
            "null",
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

        let content = vec![text("from content"), image.clone()];
        let output = search_output(Ok("from output".to_string()), Some(content.clone()));
        assert_eq!(
            output.code_mode_result(&payload),
            json!(["from content", { "image_url": "data:image/png;base64,AAAA", "detail": "high" }])
        );
        assert_eq!(sent(output).content_items(), Some(content.as_slice()));

        // Without an image, or without content, the text stands as before.
        for content in [Some(vec![text("from content")]), None] {
            let output = search_output(Ok("from output".to_string()), content);
            assert_eq!(output.code_mode_result(&payload), json!("from output"));
            assert_eq!(
                sent(output).content_items(),
                Some([text("from output")].as_slice())
            );
        }

        // A failed call carries no content.
        let output = search_output(Err("failed".to_string()), Some(vec![image]));
        assert_eq!(output.code_mode_result(&payload), json!("failed"));
        let sent = sent(output);
        assert_eq!(
            (sent.text_content(), sent.success),
            (Some("failed"), Some(false))
        );
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
