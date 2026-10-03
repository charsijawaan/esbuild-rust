//! Native callback transport for pinned esbuild 0.28.1 JavaScript plugins.
//!
//! The host runs setup, sequential resolve/load fallback, and standalone
//! onEnd/onDispose callbacks. This module sends the ordered matching callback
//! IDs together and retains only opaque host stash indices. Context lifecycle
//! commands are owned by the dispatcher; registering this bridge enables none
//! of context, cancellation, watch, or serving by itself.

use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

use crate::{
    api,
    internal::{cli_helpers, config, helpers::quote_go_string, logger},
};

use super::{
    messages::{
        array_field, bool_field, decode_messages, decode_plugin_data, encode_messages,
        encode_plugin_data, string_array, text_field,
    },
    protocol::Value,
};

/// A blocking callback request. The service reader must remain available while
/// this waits, including for nested host-to-service resolve requests.
pub type SendRequest = Arc<dyn Fn(Value) -> io::Result<Value> + Send + Sync>;
pub type ResolveCallbacks = Arc<Mutex<HashMap<u32, api::ResolveCallback>>>;
type ResolveSlot = Arc<Mutex<Option<api::ResolveCallback>>>;

#[derive(Clone)]
struct FilteredCallback {
    filter: Arc<regex::Regex>,
    plugin_name: String,
    namespace: String,
    id: i64,
}

fn filtered_callbacks(plugin: &Value, kind: &str) -> Result<Vec<FilteredCallback>, String> {
    let name = text_field(plugin, "name")?;
    array_field(plugin, kind)?
        .iter()
        .map(|item| {
            let filter = text_field(item, "filter")?;
            Ok(FilteredCallback {
                filter: config::compile_filter_for_plugin(&name, kind, &filter).map_err(
                    |error| {
                        if filter.is_empty() {
                            error
                        } else {
                            format!(
                                "[{name}] {kind:?} filter is not a valid Go regular expression: {}",
                                quote_go_string(filter.as_bytes())
                            )
                        }
                    },
                )?,
                namespace: text_field(item, "namespace")?,
                id: integer_field(item, "id")?,
                plugin_name: name.clone(),
            })
        })
        .collect()
}

fn integer_field(value: &Value, key: &str) -> Result<i64, String> {
    value
        .get(key)
        .and_then(Value::as_int)
        .ok_or_else(|| format!("Invalid service request: expected {key:?} to be an integer"))
}

fn optional_text(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .map_or_else(|| Ok(String::new()), |_| text_field(value, key))
}

fn optional_strings(value: &Value, key: &str) -> Result<Vec<String>, String> {
    value.get(key).map_or_else(
        || Ok(Vec::new()),
        |_| string_array(array_field(value, key)?),
    )
}

fn optional_messages(
    value: &Value,
    key: &str,
    kind: api::MessageKind,
) -> Result<Vec<api::Message>, String> {
    value.get(key).map_or_else(
        || Ok(Vec::new()),
        |_| decode_messages(array_field(value, key)?, kind),
    )
}

fn string_map(value: Option<&Value>) -> Result<HashMap<String, String>, String> {
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| "Invalid service import attributes: expected an object".to_string())?;
    object
        .iter()
        .map(|(key, value)| {
            let key = String::from_utf8(key.clone())
                .map_err(|_| "Invalid service import attribute name".to_string())?;
            let value = value.as_str().ok_or_else(|| {
                "Invalid service import attribute: expected a UTF-8 string".to_string()
            })?;
            Ok((key, value.to_owned()))
        })
        .collect()
}

fn encode_string_map(values: &HashMap<String, String>) -> Value {
    Value::object(
        values
            .iter()
            .map(|(key, value)| (key, Value::from(value.clone()))),
    )
}

fn matching_ids(callbacks: &[FilteredCallback], path: &str, namespace: &str) -> Vec<Value> {
    let path = logger::Path {
        text: path.to_owned(),
        namespace: namespace.to_owned(),
        ..logger::Path::default()
    };
    callbacks
        .iter()
        .filter(|item| config::plugin_applies_to_path(&path, &item.filter, &item.namespace))
        .map(|item| Value::Int(item.id))
        .collect()
}

fn callback_name(response: &Value, callbacks: &[FilteredCallback]) -> Result<String, String> {
    if response.get("pluginName").is_some() {
        return text_field(response, "pluginName");
    }
    Ok(callback_id_name(response, callbacks))
}

fn callback_id_name(response: &Value, callbacks: &[FilteredCallback]) -> String {
    response
        .get("id")
        .and_then(Value::as_int)
        .and_then(|id| callbacks.iter().find(|item| item.id == id))
        .map_or_else(String::new, |item| item.plugin_name.clone())
}

fn callback_failure(plugin_name: String, text: String) -> api::Message {
    api::Message {
        plugin_name,
        text,
        ..api::Message::default()
    }
}

fn receive(send_request: &SendRequest, value: Value) -> Result<Value, String> {
    let response = send_request(value).map_err(|_| "The service was stopped".to_string())?;
    if response.as_object().is_none() {
        return Err("The service was stopped".into());
    }
    Ok(response)
}

fn send(send_request: &SendRequest, value: Value) -> Result<Value, String> {
    let response = receive(send_request, value)?;
    if response.get("error").is_some() {
        return Err(text_field(&response, "error")?);
    }
    Ok(response)
}

fn register_callbacks(
    build: &mut api::PluginBuild<'_>,
    key: u32,
    on_resolve: &[FilteredCallback],
    on_load: &[FilteredCallback],
    send_request: &SendRequest,
) {
    // Always send on-start to clear the host's detail/pluginData stash.
    build.on_start({
        let send_request = send_request.clone();
        move || {
            let response = send(
                &send_request,
                Value::object([
                    ("command", Value::from("on-start")),
                    ("key", Value::Int(i64::from(key))),
                ]),
            )?;
            Ok(api::OnStartResult {
                errors: decode_messages(
                    array_field(&response, "errors")?,
                    api::MessageKind::Error,
                )?,
                warnings: decode_messages(
                    array_field(&response, "warnings")?,
                    api::MessageKind::Warning,
                )?,
            })
        }
    });
    if !on_resolve.is_empty() {
        build.on_resolve(
            api::OnResolveOptions {
                filter: ".*".into(),
                ..api::OnResolveOptions::default()
            },
            {
                let callbacks = on_resolve.to_vec();
                let send_request = send_request.clone();
                move |args| run_on_resolve(key, &send_request, &callbacks, args)
            },
        );
    }
    if !on_load.is_empty() {
        build.on_load(
            api::OnLoadOptions {
                filter: ".*".into(),
                ..api::OnLoadOptions::default()
            },
            {
                let callbacks = on_load.to_vec();
                let send_request = send_request.clone();
                move |args| run_on_load(key, &send_request, &callbacks, args)
            },
        );
    }
}

fn run_on_resolve(
    key: u32,
    send_request: &SendRequest,
    callbacks: &[FilteredCallback],
    args: api::OnResolveArgs,
) -> Result<api::OnResolveResult, api::PluginError> {
    let ids = matching_ids(callbacks, &args.path, &args.namespace);
    if ids.is_empty() {
        return Ok(api::OnResolveResult::default());
    }
    let response = receive(
        send_request,
        Value::object([
            ("command", Value::from("on-resolve")),
            ("key", Value::Int(i64::from(key))),
            ("ids", Value::Array(ids)),
            ("path", Value::from(args.path)),
            ("importer", Value::from(args.importer)),
            ("namespace", Value::from(args.namespace)),
            ("resolveDir", Value::from(args.resolve_dir)),
            ("kind", Value::from(resolve_kind_to_string(args.kind)?)),
            ("pluginData", encode_plugin_data(args.plugin_data.as_ref())),
            ("with", encode_string_map(&args.with)),
        ]),
    )?;
    if response.get("error").is_some() {
        let plugin_name = callback_id_name(&response, callbacks);
        return Ok(api::OnResolveResult {
            plugin_name: plugin_name.clone(),
            errors: vec![callback_failure(
                plugin_name,
                text_field(&response, "error")?,
            )],
            ..api::OnResolveResult::default()
        });
    }
    Ok(api::OnResolveResult {
        plugin_name: callback_name(&response, callbacks)?,
        path: optional_text(&response, "path")?,
        namespace: optional_text(&response, "namespace")?,
        suffix: optional_text(&response, "suffix")?,
        external: response
            .get("external")
            .map_or(Ok(false), |_| bool_field(&response, "external"))?,
        side_effects: if response
            .get("sideEffects")
            .map_or(Ok(true), |_| bool_field(&response, "sideEffects"))?
        {
            api::SideEffects::True
        } else {
            api::SideEffects::False
        },
        plugin_data: decode_plugin_data(response.get("pluginData"))?,
        errors: optional_messages(&response, "errors", api::MessageKind::Error)?,
        warnings: optional_messages(&response, "warnings", api::MessageKind::Warning)?,
        watch_files: optional_strings(&response, "watchFiles")?,
        watch_dirs: optional_strings(&response, "watchDirs")?,
    })
}

fn run_on_load(
    key: u32,
    send_request: &SendRequest,
    callbacks: &[FilteredCallback],
    args: api::OnLoadArgs,
) -> Result<api::OnLoadResult, api::PluginError> {
    let ids = matching_ids(callbacks, &args.path, &args.namespace);
    if ids.is_empty() {
        return Ok(api::OnLoadResult::default());
    }
    let response = receive(
        send_request,
        Value::object([
            ("command", Value::from("on-load")),
            ("key", Value::Int(i64::from(key))),
            ("ids", Value::Array(ids)),
            ("path", Value::from(args.path)),
            ("namespace", Value::from(args.namespace)),
            ("suffix", Value::from(args.suffix)),
            ("pluginData", encode_plugin_data(args.plugin_data.as_ref())),
            ("with", encode_string_map(&args.with)),
        ]),
    )?;
    if response.get("error").is_some() {
        let plugin_name = callback_id_name(&response, callbacks);
        return Ok(api::OnLoadResult {
            plugin_name: plugin_name.clone(),
            errors: vec![callback_failure(
                plugin_name,
                text_field(&response, "error")?,
            )],
            ..api::OnLoadResult::default()
        });
    }
    let plugin_name = callback_name(&response, callbacks)?;
    let loader = if response.get("loader").is_some() {
        let loader = text_field(&response, "loader")?;
        match cli_helpers::parse_loader(&loader) {
            Ok(loader) => loader,
            Err(_) => {
                // Go returns the selected plugin name along
                // with this error. Native PluginError alone
                // would attribute it to the aggregate plugin.
                return Ok(api::OnLoadResult {
                    plugin_name: plugin_name.clone(),
                    errors: vec![callback_failure(
                        plugin_name,
                        format!(
                            "Invalid loader value: {}",
                            quote_go_string(loader.as_bytes())
                        ),
                    )],
                    ..api::OnLoadResult::default()
                });
            }
        }
    } else {
        api::Loader::None
    };
    let contents_bytes = response
        .get("contents")
        .map(|contents| {
            contents
                .as_bytes()
                .map(<[u8]>::to_vec)
                .ok_or_else(|| "Invalid service onLoad contents: expected bytes".to_string())
        })
        .transpose()?;
    Ok(api::OnLoadResult {
        plugin_name,
        contents_bytes,
        loader,
        resolve_dir: optional_text(&response, "resolveDir")?,
        plugin_data: decode_plugin_data(response.get("pluginData"))?,
        errors: optional_messages(&response, "errors", api::MessageKind::Error)?,
        warnings: optional_messages(&response, "warnings", api::MessageKind::Warning)?,
        watch_files: optional_strings(&response, "watchFiles")?,
        watch_dirs: optional_strings(&response, "watchDirs")?,
        ..api::OnLoadResult::default()
    })
}

/// One aggregate native plugin and its build-scoped resolve callback.
pub struct PluginBridge {
    key: u32,
    send_request: SendRequest,
    plugin: api::Plugin,
    resolve: ResolveSlot,
    resolve_callbacks: ResolveCallbacks,
    has_on_end: bool,
}

impl PluginBridge {
    /// Decode the descriptors already produced by the original host wrapper.
    /// Setup has run in JavaScript and its option changes are already in flags.
    ///
    /// # Errors
    /// Returns filter compilation or malformed wire descriptor errors.
    pub fn new(
        key: u32,
        plugins: &Value,
        send_request: SendRequest,
        resolve_callbacks: ResolveCallbacks,
    ) -> Result<Self, String> {
        let plugins = plugins
            .as_array()
            .ok_or_else(|| "Invalid service plugins: expected an array".to_string())?;
        let mut on_resolve = Vec::new();
        let mut on_load = Vec::new();
        let mut has_on_end = false;
        for plugin in plugins {
            has_on_end |= bool_field(plugin, "onEnd")?;
            on_resolve.extend(filtered_callbacks(plugin, "onResolve")?);
            on_load.extend(filtered_callbacks(plugin, "onLoad")?);
        }
        let resolve: ResolveSlot = Arc::new(Mutex::new(None));
        let plugin = api::Plugin::new("JavaScript plugins", {
            let resolve = resolve.clone();
            let resolve_callbacks = resolve_callbacks.clone();
            let send_request = send_request.clone();
            move |build| {
                *resolve
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(build.resolve.clone());
                resolve_callbacks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key, build.resolve.clone());
                register_callbacks(build, key, &on_resolve, &on_load, &send_request);
                build.on_dispose({
                    let resolve = resolve.clone();
                    let resolve_callbacks = resolve_callbacks.clone();
                    move || {
                        deactivate(key, &resolve, &resolve_callbacks);
                    }
                });
                Ok(())
            }
        });
        Ok(Self {
            key,
            send_request,
            plugin,
            resolve,
            resolve_callbacks,
            has_on_end,
        })
    }

    #[must_use]
    pub fn plugin(&self) -> api::Plugin {
        self.plugin.clone()
    }

    #[must_use]
    pub fn has_on_end(&self) -> bool {
        self.has_on_end
    }

    /// Native onEnd hook for a context owner to attach after the callback bridge.
    /// Standalone builds must not use this: the wrapper runs their onEnd locally.
    #[must_use]
    pub fn on_end_plugin<F>(&self, result_to_response: F) -> api::Plugin
    where
        F: Fn(&api::BuildResult) -> Value + Send + Sync + 'static,
    {
        self.on_end_plugin_if(|| true, result_to_response)
    }

    /// Skip automatic-build notifications when JavaScript has no observer.
    #[must_use]
    pub(super) fn on_end_plugin_if<F, G>(
        &self,
        should_send: G,
        result_to_response: F,
    ) -> api::Plugin
    where
        F: Fn(&api::BuildResult) -> Value + Send + Sync + 'static,
        G: Fn() -> bool + Send + Sync + 'static,
    {
        let send_request = self.send_request.clone();
        let key = self.key;
        let result_to_response = Arc::new(result_to_response);
        let should_send = Arc::new(should_send);
        api::Plugin::new("onEnd", move |build| {
            let send_request = send_request.clone();
            let result_to_response = result_to_response.clone();
            let should_send = should_send.clone();
            build.on_end(move |result| {
                if !should_send() {
                    return Ok(api::OnEndResult::default());
                }
                let Value::Object(mut request) = result_to_response(result) else {
                    return Err(api::PluginError::new(
                        "Invalid service onEnd response: expected an object",
                    ));
                };
                request.insert(b"command".to_vec(), Value::from("on-end"));
                request.insert(b"key".to_vec(), Value::Int(i64::from(key)));
                let response = send(&send_request, Value::Object(request))?;
                Ok(api::OnEndResult {
                    errors: decode_messages(
                        array_field(&response, "errors")?,
                        api::MessageKind::Error,
                    )?,
                    warnings: decode_messages(
                        array_field(&response, "warnings")?,
                        api::MessageKind::Warning,
                    )?,
                })
            });
            Ok(())
        })
    }
}

// Called on reader admission, before worker scheduling. Never retain a
// registry lock while resolving or waiting on JavaScript.
pub(super) fn prepare_resolve(
    resolve_callbacks: &ResolveCallbacks,
    request: &Value,
) -> Result<PreparedResolve, String> {
    let key = build_key(request)?;
    let resolve = resolve_callbacks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
        .ok_or_else(|| "Cannot call \"resolve\" on an inactive build".to_string())?;
    let kind = request
        .get("kind")
        .map_or(Ok(api::ResolveKind::None), |_| {
            let kind = text_field(request, "kind")?;
            string_to_resolve_kind(&kind)
                .ok_or_else(|| format!("Invalid kind: {}", quote_go_string(kind.as_bytes())))
        })?;
    Ok(PreparedResolve {
        resolve,
        path: text_field(request, "path")?,
        options: api::ResolveOptions {
            plugin_name: optional_text(request, "pluginName")?,
            importer: optional_text(request, "importer")?,
            namespace: optional_text(request, "namespace")?,
            resolve_dir: optional_text(request, "resolveDir")?,
            kind,
            plugin_data: decode_plugin_data(request.get("pluginData"))?,
            with: string_map(request.get("with"))?,
        },
    })
}

impl Drop for PluginBridge {
    fn drop(&mut self) {
        deactivate(self.key, &self.resolve, &self.resolve_callbacks);
    }
}

fn deactivate(key: u32, slot: &ResolveSlot, resolve_callbacks: &ResolveCallbacks) {
    let Some(resolve) = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let mut callbacks = resolve_callbacks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if callbacks
        .get(&key)
        .is_some_and(|callback| Arc::ptr_eq(callback, &resolve))
    {
        callbacks.remove(&key);
    }
}

pub(super) fn build_key(request: &Value) -> Result<u32, String> {
    u32::try_from(integer_field(request, "key")?)
        .map_err(|_| "Invalid service build key".to_string())
}

pub(super) struct PreparedResolve {
    resolve: api::ResolveCallback,
    path: String,
    options: api::ResolveOptions,
}

impl PreparedResolve {
    pub(super) fn run(self) -> Value {
        let result = (self.resolve)(&self.path, self.options);
        Value::object([
            ("errors", encode_messages(&result.errors)),
            ("warnings", encode_messages(&result.warnings)),
            ("path", Value::from(result.path)),
            ("external", Value::Bool(result.external)),
            ("sideEffects", Value::Bool(result.side_effects)),
            ("namespace", Value::from(result.namespace)),
            ("suffix", Value::from(result.suffix)),
            (
                "pluginData",
                encode_plugin_data(result.plugin_data.as_ref()),
            ),
        ])
    }
}

fn resolve_kind_to_string(kind: api::ResolveKind) -> Result<&'static str, String> {
    Ok(match kind {
        api::ResolveKind::EntryPoint => "entry-point",
        api::ResolveKind::ImportStatement => "import-statement",
        api::ResolveKind::RequireCall => "require-call",
        api::ResolveKind::DynamicImport => "dynamic-import",
        api::ResolveKind::RequireResolve => "require-resolve",
        api::ResolveKind::CssImportRule => "import-rule",
        api::ResolveKind::CssComposesFrom => "composes-from",
        api::ResolveKind::CssUrlToken => "url-token",
        api::ResolveKind::None => return Err("Internal error: resolve callback has no kind".into()),
    })
}

fn string_to_resolve_kind(kind: &str) -> Option<api::ResolveKind> {
    Some(match kind {
        "entry-point" => api::ResolveKind::EntryPoint,
        "import-statement" => api::ResolveKind::ImportStatement,
        "require-call" => api::ResolveKind::RequireCall,
        "dynamic-import" => api::ResolveKind::DynamicImport,
        "require-resolve" => api::ResolveKind::RequireResolve,
        "import-rule" => api::ResolveKind::CssImportRule,
        "composes-from" => api::ResolveKind::CssComposesFrom,
        "url-token" => api::ResolveKind::CssUrlToken,
        _ => return None,
    })
}
