//! Start native watch mode on a reader-admitted context operation.
//!
//! Source: pinned `cmd/esbuild/service.go:323-361`. The native context owns
//! polling, initial/background builds, coalescing, and watcher disposal.

use std::sync::Arc;

use super::{
    contexts::{Context, Operation},
    messages::log_messages,
    plugins::PluginBridge,
    protocol::{Object, Value},
};
use crate::api;

pub(super) fn start(operation: &Operation, request: &Value) -> Result<Value, String> {
    let delay = request.get("delay").map_or(Ok(0), |value| {
        value
            .as_int()
            .ok_or_else(|| "Invalid service watch delay: expected an integer".to_string())
    })?;
    operation
        .native
        .watch(api::WatchOptions { delay })
        .map_err(|error| error.to_string())?;
    Ok(Value::Object(Object::new()))
}

// Go only sends automatic onEnd results if JavaScript has an observer or is
// receiving stdout. Manual rebuild admissions always need the result/ack.
pub(super) fn context_plugins<F>(
    context: &Arc<Context>,
    bridge: &PluginBridge,
    write_to_stdout: bool,
    result_to_response: F,
) -> [api::Plugin; 2]
where
    F: Fn(&api::BuildResult) -> Value + Send + Sync + 'static,
{
    let weak_context = Arc::downgrade(context);
    let has_on_end = bridge.has_on_end();
    [
        bridge.on_end_plugin_if(
            move || {
                has_on_end
                    || write_to_stdout
                    || weak_context
                        .upgrade()
                        .is_some_and(|context| context.has_rebuild())
            },
            result_to_response,
        ),
        background_diagnostics(context),
    ]
}

// Manual rebuild callers log after their native operation returns. Automatic
// builds have no request worker, so print their final diagnostics here, after
// the existing onEnd bridge has incorporated the JavaScript acknowledgement.
// A weak context avoids a cycle through its retained native plugin callbacks.
fn background_diagnostics(context: &Arc<Context>) -> api::Plugin {
    let context = Arc::downgrade(context);
    api::Plugin::new("service background diagnostics", move |build| {
        let context = context.clone();
        build.on_end(move |result| {
            if let Some(context) = context.upgrade().filter(|context| !context.has_rebuild()) {
                log_messages(&context.log_settings, &result.errors, &result.warnings);
            }
            Ok(api::OnEndResult::default())
        });
        Ok(())
    })
}
