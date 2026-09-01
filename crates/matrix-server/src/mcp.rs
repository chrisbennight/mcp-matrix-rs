//! The `2026-07-28` handler.
//!
//! Stateless by construction: the transport builds one of these per request, so it
//! holds nothing but a clone of the shared [`Engine`] handle. Every identifier a caller
//! gets back — an asset handle, a playback handle — is a key into that shared state and
//! is passed as an ordinary tool argument, which is the protocol's cross-call
//! mechanism.

use crate::state::Engine;
use crate::tools::{self, FileValue, RegionParam};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CacheScope, ListToolsResult, PaginatedRequestParams, Tool};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Binaries the media path shells out to, resolved once at startup.
#[derive(Debug, Clone)]
pub struct MediaBinaries {
    pub ffmpeg: String,
    pub ffprobe: String,
}

#[derive(Clone)]
pub struct MatrixHandler {
    engine: Arc<Engine>,
    binaries: MediaBinaries,
    files: Option<Arc<crate::files::FilePlane>>,
}

impl MatrixHandler {
    pub fn new(engine: Arc<Engine>, binaries: MediaBinaries) -> Self {
        Self {
            engine,
            binaries,
            files: None,
        }
    }

    /// Serve the transfer plane too. Without it this server is inline-only and answers
    /// the authorization method with method-not-found, which is what a file-aware
    /// intermediary reads as "no native file transfer".
    pub fn with_files(mut self, files: Option<Arc<crate::files::FilePlane>>) -> Self {
        self.files = files;
        self
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitParams {
    /// The media to normalize and hold.
    pub source: FileValue,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayParams {
    /// An asset handle returned by `matrix_submit_asset`.
    pub asset: String,
    /// Repeat the sequence until stopped. Defaults to false.
    #[serde(default)]
    pub looping: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopParams {
    /// A playback handle. Omitted stops whatever is playing; supplied and stale is
    /// refused rather than cancelling something the caller did not start.
    #[serde(default)]
    pub playback: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowTextParams {
    /// The text to display. Input is capped at 100 characters; scrolling text must
    /// also fit the configured canvas and frame budget. ASCII renders exactly;
    /// anything the font cannot draw becomes a visible `?`.
    pub text: String,
    /// Start playing immediately. Defaults to true; false holds the asset for a
    /// later `matrix_play`.
    #[serde(default = "default_true")]
    pub play: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowTextLayoutParams {
    /// The text regions to compose, at most 16. Rectangles must lie on the canvas
    /// and must not overlap; fixed text must fit its rectangle, and scrolling text
    /// must fit it across the axis it does not travel.
    #[schemars(length(min = 1, max = 16))]
    pub regions: Vec<RegionParam>,
    /// Start playing immediately. Defaults to true; false holds the asset for a
    /// later `matrix_play`.
    #[serde(default = "default_true")]
    pub play: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrightnessParams {
    /// 0 to 255.
    pub level: u8,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PowerParams {
    pub on: bool,
}

/// Implementation-defined JSON-RPC code for a domain refusal.
///
/// These are not invalid params — the arguments parsed fine and the server refused for
/// a reason the caller can act on. `-32050` sits in the implementation-defined range;
/// the stable machine contract is the string code carried in `data.code`.
const DOMAIN_REFUSAL: rmcp::model::ErrorCode = rmcp::model::ErrorCode(-32050);

fn fail(error: impl std::fmt::Display, code: &str) -> McpError {
    McpError::new(
        DOMAIN_REFUSAL,
        format!("{code}: {error}"),
        Some(serde_json::json!({ "code": code })),
    )
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, McpError> {
    serde_json::to_string(value).map_err(|e| fail(e, "matrix_serialization_failed"))
}

#[tool_router]
impl MatrixHandler {
    #[tool(
        description = "Read the panel: identity, firmware, dimensions, the framerate it \
                       is achieving, and its power draw against any configured ceiling. \
                       Polling also refreshes the framerate that paces playback.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn matrix_describe_device(&self) -> Result<String, McpError> {
        let report = tools::describe_device(&self.engine)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&report)
    }

    #[tool(
        description = "Report what is playing, how many assets are held, and the \
                       framerate the panel last reported.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn matrix_status(&self) -> Result<String, McpError> {
        json(&tools::status(&self.engine).await)
    }

    #[tool(
        description = "Normalize media into frames the panel can display and hold it, \
                       returning an asset handle. Accepts a base64 data: URI up to 16 KiB \
                       — content at or near the panel's resolution. Larger media needs a \
                       reference obtained through this server's file transfer, which is \
                       available only where the operator has configured it. Returns the \
                       frame count and duration so a caller knows what it got without \
                       playing it.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_submit_asset(
        &self,
        Parameters(SubmitParams { source }): Parameters<SubmitParams>,
    ) -> Result<String, McpError> {
        let report = tools::submit_asset(
            &self.engine,
            &source,
            self.files.as_ref(),
            &self.binaries.ffmpeg,
            &self.binaries.ffprobe,
        )
        .await
        .map_err(|e| fail(&e, e.code()))?;
        json(&report)
    }

    #[tool(
        description = "List the assets currently held, newest handles last.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn matrix_list_assets(&self) -> Result<String, McpError> {
        json(&tools::list_assets(&self.engine).await)
    }

    #[tool(
        description = "Play a held asset on the panel, replacing whatever was playing. \
                       Returns a playback handle. Playback is paced by the framerate the \
                       panel reports and each frame is clamped to its power ceiling.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_play(
        &self,
        Parameters(PlayParams { asset, looping }): Parameters<PlayParams>,
    ) -> Result<String, McpError> {
        let handle = tools::play(&self.engine, &asset, looping)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({ "playback": handle }))
    }

    #[tool(
        description = "Display a line of text on the panel. Text that fits shows as a \
                       centered still; longer text scrolls as a marquee until stopped. \
                       Returns the asset handle and, when played, the playback handle.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_show_text(
        &self,
        Parameters(ShowTextParams { text, play }): Parameters<ShowTextParams>,
    ) -> Result<String, McpError> {
        let (asset, playback) = tools::show_text(&self.engine, &text, play)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        let visible = tools::text_visible_chars(&self.engine);
        json(&serde_json::json!({
            "asset": asset,
            "playback": playback,
            "visible_chars": visible,
            "scrolls": asset.frames > 1,
        }))
    }

    #[tool(
        description = "Compose fixed and scrolling text regions into one animated \
                       package. Each region has a rectangle, text, style, and either \
                       fixed alignment or a scroll path (four canonical paths, each \
                       reversible) with a speed in pixels per second. The longest \
                       single crossing sets the package length and looping repeats \
                       the whole package. By default a scroller crosses once and \
                       parks; `repeat: true` re-enters for as many evenly spaced \
                       crossings as fit the package, and `phase` (0 to below 1, \
                       requires repeat) offsets where in its cycle a region starts, \
                       so regions animate continuously and independently. Rectangles \
                       must not overlap. Returns the asset handle and, when played, \
                       the playback handle.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_show_text_layout(
        &self,
        Parameters(ShowTextLayoutParams { regions, play }): Parameters<ShowTextLayoutParams>,
    ) -> Result<String, McpError> {
        let (asset, playback) = tools::show_text_layout(&self.engine, &regions, play)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({
            "asset": asset,
            "playback": playback,
            "regions": regions.len(),
            "scrolls": asset.frames > 1,
        }))
    }

    #[tool(
        description = "Stop playback. The panel returns to its configured ambient \
                       behaviour once the frame stream stops.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn matrix_stop(
        &self,
        Parameters(StopParams { playback }): Parameters<StopParams>,
    ) -> Result<String, McpError> {
        let stopped = tools::stop(&self.engine, playback.as_deref())
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({ "stopped": stopped }))
    }

    #[tool(
        description = "Set panel brightness, 0 to 255.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn matrix_set_brightness(
        &self,
        Parameters(BrightnessParams { level }): Parameters<BrightnessParams>,
    ) -> Result<String, McpError> {
        tools::set_brightness(&self.engine, level)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({ "brightness": level }))
    }

    #[tool(
        description = "Turn the panel on or off.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn matrix_power(
        &self,
        Parameters(PowerParams { on }): Parameters<PowerParams>,
    ) -> Result<String, McpError> {
        tools::set_power(&self.engine, on)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        json(&serde_json::json!({ "on": on }))
    }
}

/// Device capability data changes only on a reflash or a settings change, and the tool
/// list is fixed for a build, so a short freshness hint would spend requests re-reading
/// constants.
const TOOL_LIST_TTL_MS: u64 = 300_000;

/// Mark `source` as a file-valued input, so an intermediary knows it may deliver one.
///
/// The annotation goes on the `source` subschema, which is the object shape this server
/// has always accepted. `transferModes` names both paths deliberately: inline stays a
/// first-class way to submit a small still, and the ceiling advertised here is the
/// decoder's source ceiling rather than the inline cap, because the inline cap is a
/// separate and much lower bound this server enforces itself.
fn annotate_file_input(tool: &mut rmcp::model::Tool) {
    let mut schema = (*tool.input_schema).clone();
    let Some(source) = schema
        .get_mut("properties")
        .and_then(|p| p.get_mut("source"))
        .and_then(|s| s.as_object_mut())
    else {
        // The schema is derived, so this cannot happen for a build that compiles; saying
        // nothing is still better than publishing an annotation on the wrong property.
        tracing::warn!("matrix_submit_asset has no source property to annotate");
        return;
    };

    source.insert(
        "x-mcp-file".into(),
        serde_json::json!({
            "transferModes": ["inline", "upload"],
            "maxSize": matrix_media::Limits::default().max_source_bytes,
        }),
    );
    tool.input_schema = Arc::new(schema);

    // The prose has to agree with the annotation. A tool that declares an upload mode
    // while telling callers references are unavailable is publishing two contracts.
    tool.description = Some(
        "Normalize media into frames the panel can display and hold it, returning an \
         asset handle. Accepts a base64 data: URI up to 16 KiB — content at or near the \
         panel's resolution — or a reference to media already delivered through this \
         server's file transfer, which is how anything larger arrives. Returns the frame \
         count and duration so a caller knows what it got without playing it."
            .into(),
    );
}

fn catalog_tools(files_enabled: bool) -> Vec<Tool> {
    let mut tools = MatrixHandler::tool_router().list_all();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    for tool in &mut tools {
        normalize_tool_schemas(tool);
        if files_enabled && tool.name == "matrix_submit_asset" {
            annotate_file_input(tool);
        }
    }
    tools
}

fn normalize_tool_schemas(tool: &mut Tool) {
    tool.input_schema = portable_schema_map(&tool.input_schema);
    if let Some(output_schema) = tool.output_schema.as_ref() {
        tool.output_schema = Some(portable_schema_map(output_schema));
    }
}

fn portable_schema_map(schema: &Arc<Map<String, Value>>) -> Arc<Map<String, Value>> {
    let mut schema = Value::Object((**schema).clone());
    normalize_portable_schema(&mut schema, None);
    match schema {
        Value::Object(object) => Arc::new(object),
        _ => unreachable!("an MCP root schema is an object"),
    }
}

/// Publish semantically equivalent object schemas where strict MCP clients do
/// not consume legal JSON Schema boolean schemas or array-valued type unions.
fn normalize_portable_schema(schema: &mut Value, parent_keyword: Option<&str>) {
    if let Value::Bool(accepts_everything) = schema {
        if parent_keyword.is_some_and(|keyword| BOOLEAN_SCHEMA_KEYWORDS.contains(&keyword)) {
            return;
        }
        *schema = if *accepts_everything {
            json!({
                "anyOf": JSON_SCHEMA_TYPES
                    .iter()
                    .map(|name| json!({"type": name}))
                    .collect::<Vec<_>>()
            })
        } else {
            json!({"not": {}})
        };
        return;
    }

    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let portable_types = object.get("type").and_then(|value| {
        let values = value.as_array()?;
        let mut types: Vec<String> = Vec::with_capacity(values.len());
        for value in values {
            let name = value.as_str()?;
            if !JSON_SCHEMA_TYPES.contains(&name) || types.iter().any(|item| item == name) {
                return None;
            }
            types.push(name.to_owned());
        }
        (!types.is_empty()).then_some(types)
    });
    if let Some(types) = portable_types {
        object.remove("type");
        let branches = Value::Array(
            types
                .into_iter()
                .map(|name| json!({"type": name}))
                .collect(),
        );
        if object.contains_key("anyOf") {
            object
                .entry("allOf")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .expect("a generated allOf schema must be an array")
                .push(json!({"anyOf": branches}));
        } else {
            object.insert("anyOf".to_owned(), branches);
        }
    }

    for keyword in [
        "$defs",
        "definitions",
        "properties",
        "patternProperties",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(children) = object.get_mut(keyword).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                normalize_portable_schema(child, Some(keyword));
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(keyword).and_then(Value::as_array_mut) {
            for child in children {
                normalize_portable_schema(child, Some(keyword));
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        if let Some(children) = items.as_array_mut() {
            for child in children {
                normalize_portable_schema(child, Some("items"));
            }
        } else {
            normalize_portable_schema(items, Some("items"));
        }
    }
    for keyword in [
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "contentSchema",
        "additionalProperties",
        "unevaluatedProperties",
        "additionalItems",
        "unevaluatedItems",
    ] {
        if let Some(child) = object.get_mut(keyword) {
            normalize_portable_schema(child, Some(keyword));
        }
    }
}

const BOOLEAN_SCHEMA_KEYWORDS: [&str; 4] = [
    "additionalProperties",
    "unevaluatedProperties",
    "additionalItems",
    "unevaluatedItems",
];
const JSON_SCHEMA_TYPES: [&str; 7] = [
    "null", "boolean", "object", "array", "number", "string", "integer",
];

#[tool_handler(
    name = "matrix-server",
    // The macro takes a literal, so this cannot read the crate metadata directly. It
    // said 0.2.0 through the whole of 0.3.0 for exactly the reason a version in two
    // places always drifts — only one of them got updated. `the_advertised_version_is_
    // the_crate_version` fails the build when they disagree, which is the part that
    // stops it happening again.
    version = "0.4.1",
    instructions = "Renders media to a WLED LED matrix. Submit media to get an asset \
                    handle, then play that handle. Playback is paced by the framerate \
                    the panel reports and each frame is clamped to the panel's power \
                    ceiling. Stopping returns the panel to its configured ambient \
                    behaviour."
)]
impl ServerHandler for MatrixHandler {
    /// Serve the draft's upload authorization when the transfer plane is configured.
    ///
    /// Everything else — including this method on an inline-only server — falls through
    /// to the library's method-not-found. That answer is the contract: a file-aware
    /// intermediary reads `-32601` as "this upstream has no native file transfer" and
    /// stops asking, so an unconfigured deployment needs no advertisement to say so.
    ///
    /// The draft is unmerged, so its spellings live in one module rather than being
    /// spread across the handler.
    async fn on_custom_request(
        &self,
        request: rmcp::model::CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CustomResult, McpError> {
        use crate::files::AuthorizeUploadParams;

        if request.method != crate::files::AUTHORIZE_UPLOAD {
            return Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ));
        }
        let Some(files) = self.files.as_ref() else {
            return Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ));
        };

        // Absent params are an authorization with nothing declared, which is legitimate:
        // every field of the draft's request is optional.
        let params: AuthorizeUploadParams = match request.params {
            None => AuthorizeUploadParams::default(),
            Some(value) => serde_json::from_value(value).map_err(|e| {
                McpError::new(
                    rmcp::model::ErrorCode::INVALID_PARAMS,
                    format!("matrix_file_bad_params: {e}"),
                    Some(serde_json::json!({ "code": "matrix_file_bad_params" })),
                )
            })?,
        };

        let authorized = files
            .authorize_upload(params)
            .await
            .map_err(|e| fail(&e, e.code()))?;
        let value = serde_json::to_value(&authorized)
            .map_err(|e| fail(e, "matrix_serialization_failed"))?;
        Ok(rmcp::model::CustomResult::new(value))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Deterministic order so a client cache and an LLM prompt cache both hit; the
        // router yields registration order, which is stable per build but not sorted.
        // The file annotation appears only where the transfer plane can actually
        // receive one. An inline-only deployment therefore does not advertise an
        // upload capability it would then refuse.
        let tools = catalog_tools(self.files.is_some());

        Ok(ListToolsResult {
            tools,
            ..ListToolsResult::default()
        }
        .with_ttl_ms(TOOL_LIST_TTL_MS)
        // Private: the catalog belongs to one server instance. A shared intermediary
        // must not serve one instance's tools as another's.
        .with_cache_scope(CacheScope::Private))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const CONSTRAINING_KEYWORDS: [&str; 43] = [
        "type",
        "enum",
        "const",
        "multipleOf",
        "maximum",
        "exclusiveMaximum",
        "minimum",
        "exclusiveMinimum",
        "maxLength",
        "minLength",
        "pattern",
        "format",
        "contentMediaType",
        "contentEncoding",
        "contentSchema",
        "maxItems",
        "minItems",
        "uniqueItems",
        "maxContains",
        "minContains",
        "maxProperties",
        "minProperties",
        "required",
        "dependentRequired",
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "items",
        "prefixItems",
        "contains",
        "additionalItems",
        "unevaluatedItems",
        "properties",
        "patternProperties",
        "additionalProperties",
        "unevaluatedProperties",
        "propertyNames",
        "dependentSchemas",
        "dependencies",
        "$ref",
        "$dynamicRef",
        "$recursiveRef",
    ];

    fn for_each_subschema(node: &Map<String, Value>, mut visit: impl FnMut(&Value, &str)) {
        for keyword in [
            "properties",
            "patternProperties",
            "dependentSchemas",
            "dependencies",
            "$defs",
            "definitions",
        ] {
            if let Some(children) = node.get(keyword).and_then(Value::as_object) {
                for child in children.values() {
                    visit(child, keyword);
                }
            }
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(children) = node.get(keyword).and_then(Value::as_array) {
                for child in children {
                    visit(child, keyword);
                }
            }
        }
        for keyword in [
            "items",
            "contains",
            "not",
            "propertyNames",
            "if",
            "then",
            "else",
            "additionalProperties",
            "unevaluatedProperties",
            "additionalItems",
            "unevaluatedItems",
            "contentSchema",
        ] {
            let Some(child) = node.get(keyword) else {
                continue;
            };
            if keyword == "items"
                && let Some(children) = child.as_array()
            {
                for child in children {
                    visit(child, keyword);
                }
                continue;
            }
            visit(child, keyword);
        }
    }

    fn schema_declares_id(schema: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        let Some(node) = schema.as_object() else {
            return false;
        };
        if node
            .get("$id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        {
            return true;
        }
        let mut found = false;
        for_each_subschema(node, |child, _| {
            found |= schema_declares_id(child, depth + 1);
        });
        found
    }

    fn inspector_findings(schema: &Value) -> Vec<&'static str> {
        fn walk(
            schema: &Value,
            parent_keyword: Option<&str>,
            depth: usize,
            has_embedded_ids: bool,
            findings: &mut Vec<&'static str>,
        ) {
            if depth > 64 {
                return;
            }
            if schema.is_boolean() {
                if parent_keyword.is_none_or(|keyword| !BOOLEAN_SCHEMA_KEYWORDS.contains(&keyword))
                {
                    findings.push("boolean-schema");
                }
                return;
            }
            let Some(node) = schema.as_object() else {
                return;
            };
            if node
                .get("type")
                .and_then(Value::as_array)
                .is_some_and(|types| {
                    !types.is_empty()
                        && types.iter().all(|item| {
                            item.as_str()
                                .is_some_and(|name| JSON_SCHEMA_TYPES.contains(&name))
                        })
                        && types
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<HashSet<_>>()
                            .len()
                            == types.len()
                })
            {
                findings.push("type-union");
            }
            if !has_embedded_ids
                && node
                    .get("$ref")
                    .and_then(Value::as_str)
                    .is_some_and(|reference| !reference.is_empty() && !reference.starts_with('#'))
            {
                findings.push("remote-ref");
            }
            let constrains = node
                .keys()
                .any(|keyword| CONSTRAINING_KEYWORDS.contains(&keyword.as_str()))
                || (node.contains_key("if")
                    && (node.contains_key("then") || node.contains_key("else")));
            if !constrains && parent_keyword != Some("not") {
                findings.push("untyped-schema");
            }
            for_each_subschema(node, |child, keyword| {
                walk(child, Some(keyword), depth + 1, has_embedded_ids, findings);
            });
        }

        let has_embedded_ids = schema_declares_id(schema, 0);
        let mut findings = Vec::new();
        walk(schema, None, 0, has_embedded_ids, &mut findings);
        findings
    }

    fn validates(schema: &Value, instance: &Value) -> bool {
        jsonschema::validator_for(schema)
            .expect("published schema compiles")
            .is_valid(instance)
    }

    fn portable_schema<T: schemars::JsonSchema>() -> (Value, Value) {
        let raw = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
        let mut portable = raw.clone();
        normalize_portable_schema(&mut portable, None);
        (raw, portable)
    }

    /// The version a client is told must be the version that was built.
    ///
    /// `#[tool_handler]` takes a literal, so the crate version cannot be interpolated
    /// into it and the two are maintained separately. They drifted once already — the
    /// handler advertised 0.2.0 for the whole of 0.3.0 — and a caller has no way to
    /// notice, because the wrong answer is a perfectly well-formed one. Failing here
    /// makes a release that forgets this impossible to ship.
    #[test]
    fn the_advertised_version_is_the_crate_version() {
        let engine = crate::state::Engine::new(
            matrix_frame::Canvas::new(64, 64).expect("valid"),
            matrix_frame::Rate::new(25).expect("valid"),
            matrix_device::WledClient::new(
                "http://127.0.0.1:1".to_string(),
                std::time::Duration::from_millis(1),
            )
            .expect("valid base"),
            "127.0.0.1:4048".parse().expect("addr"),
        );
        let handler = MatrixHandler::new(
            engine,
            MediaBinaries {
                ffmpeg: "ffmpeg".into(),
                ffprobe: "ffprobe".into(),
            },
        );

        assert_eq!(
            handler.get_info().server_info.version,
            env!("CARGO_PKG_VERSION"),
        );
    }

    #[test]
    fn catalog_schemas_pass_mcp_inspector_portability_rules() {
        let mut findings = Vec::new();
        for files_enabled in [false, true] {
            for tool in catalog_tools(files_enabled) {
                let mut schemas =
                    vec![("inputSchema", Value::Object((*tool.input_schema).clone()))];
                if let Some(output) = tool.output_schema {
                    schemas.push(("outputSchema", Value::Object((*output).clone())));
                }
                for (kind, schema) in schemas {
                    for rule in inspector_findings(&schema) {
                        findings.push(format!(
                            "files={files_enabled} {}.{kind}: {rule}",
                            tool.name
                        ));
                    }
                }
            }
        }
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn portable_schemas_preserve_nullable_request_domains() {
        let (raw_stop, portable_stop) = portable_schema::<StopParams>();
        for request in [
            json!({}),
            json!({"playback": null}),
            json!({"playback": "play_123"}),
        ] {
            assert!(validates(&raw_stop, &request), "raw stop: {request}");
            assert!(
                validates(&portable_stop, &request),
                "portable stop: {request}"
            );
        }
        for request in [json!({"playback": 7}), json!({"unexpected": true})] {
            assert_eq!(
                validates(&raw_stop, &request),
                validates(&portable_stop, &request),
                "stop normalization changed the value domain for {request}"
            );
        }

        let (raw_submit, portable_submit) = portable_schema::<SubmitParams>();
        for request in [
            json!({"source": "data:image/png;base64,eA=="}),
            json!({"source": {"uri": "data:image/png;base64,eA=="}}),
            json!({
                "source": {
                    "uri": "mcp-file://matrix/asset",
                    "name": null,
                    "mimeType": null,
                    "size": null
                }
            }),
            json!({
                "source": {
                    "uri": "mcp-file://matrix/asset",
                    "name": "clip.gif",
                    "mimeType": "image/gif",
                    "size": 42
                }
            }),
        ] {
            assert!(validates(&raw_submit, &request), "raw submit: {request}");
            assert!(
                validates(&portable_submit, &request),
                "portable submit: {request}"
            );
        }
        for request in [
            json!({}),
            json!({"source": {"name": "clip.gif"}}),
            json!({"source": {"uri": "mcp-file://matrix/asset", "size": "42"}}),
        ] {
            assert_eq!(
                validates(&raw_submit, &request),
                validates(&portable_submit, &request),
                "submit normalization changed the value domain for {request}"
            );
        }
    }

    #[test]
    fn portable_schema_normalization_preserves_edge_case_domains() {
        for original in [Value::Bool(true), Value::Bool(false)] {
            let mut portable = original.clone();
            normalize_portable_schema(&mut portable, None);
            for instance in [
                json!(null),
                json!(true),
                json!(7),
                json!("value"),
                json!([1, 2]),
                json!({"nested": true}),
            ] {
                assert_eq!(
                    validates(&original, &instance),
                    validates(&portable, &instance),
                    "boolean schema normalization changed the value domain for {instance}"
                );
            }
        }

        let original = json!({
            "type": ["string", "null"],
            "anyOf": [{"maxLength": 3}]
        });
        let mut portable = original.clone();
        normalize_portable_schema(&mut portable, None);
        for instance in [json!(null), json!("ok"), json!("long"), json!(7), json!({})] {
            assert_eq!(
                validates(&original, &instance),
                validates(&portable, &instance),
                "type-union normalization changed the value domain for {instance}"
            );
        }
        assert!(portable.get("type").is_none());
        assert!(portable["allOf"][0]["anyOf"].is_array());
    }
}
