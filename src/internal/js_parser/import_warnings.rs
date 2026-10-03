use crate::internal::{
    config::Format,
    js_ast::{Expr, ExprData},
    js_lexer::range_of_identifier,
    logger::{MsgData, MsgId, MsgKind, Range},
};

use super::parser_core::ParserCore;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ImportNamespaceCallKind {
    Call,
    New,
    JsxTag,
}

// Port of warnAboutImportNamespaceCall in the pinned Go parser. Unlike many
// other parser warnings, this warning is also emitted inside node_modules.
pub(crate) fn warn_about_import_namespace_call(
    core: &mut ParserCore,
    target: &Expr,
    kind: ImportNamespaceCallKind,
) {
    if core.options.output_format == Format::Preserve {
        return;
    }
    let Some(ExprData::Identifier(identifier)) = target.data.as_deref() else {
        return;
    };
    let reference = identifier.reference;
    let Some(items) = core.import_items_for_namespace.get_mut(&reference) else {
        return;
    };
    if !items.warned_calls.insert(kind) {
        return;
    }

    let name = core.symbols[reference.inner_index as usize]
        .original_name
        .clone();
    // Go's %q prints combining marks literally. The two joiners are the only
    // non-printable characters that can appear in a valid JavaScript identifier.
    let quoted_name = format!(
        "\"{}\"",
        name.replace('\u{200c}', "\\u200c")
            .replace('\u{200d}', "\\u200d")
    );
    let member = core.module_scope.as_ref().and_then(|scope| {
        scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .members
            .get(&name)
            .copied()
    });
    let mut notes = Vec::new();
    if let Some(member) = member
        && member.reference == reference
    {
        let star = core.source.range_of_operator_before(member.loc, b"*");
        let as_keyword = core.source.range_of_operator_before(member.loc, b"as");
        if star.len > 0 && as_keyword.len > 0 && as_keyword.loc.start > star.loc.start {
            let mut note = core.tracker.msg_data(
                Range {
                    loc: star.loc,
                    len: range_of_identifier(&core.source, member.loc).end() - star.loc.start,
                },
                format!("Consider changing {quoted_name} to a default import instead:"),
            );
            if let Some(location) = &mut note.location {
                location.suggestion.clone_from(&name);
            }
            notes.push(note);
        }
    }
    if core.options.ts.parse {
        notes.push(MsgData {
            text: "Make sure to enable TypeScript's \"esModuleInterop\" setting so that TypeScript's type checker generates an error when you try to do this. You can read more about this setting here: https://www.typescriptlang.org/tsconfig#esModuleInterop".into(),
            ..MsgData::default()
        });
    }
    let (verb, context, noun) = match kind {
        ImportNamespaceCallKind::Call => ("Calling", "", "function"),
        ImportNamespaceCallKind::New => ("Constructing", "", "constructor"),
        ImportNamespaceCallKind::JsxTag => ("Using", " in a JSX expression", "component"),
    };
    if let Some(log) = core.log.clone() {
        log.add_id_with_notes(
            MsgId::JsCallImportNamespace,
            MsgKind::Warning,
            Some(&mut core.tracker),
            range_of_identifier(&core.source, target.loc),
            format!("{verb} {quoted_name}{context} will crash at run-time because it's an import namespace object, not a {noun}"),
            notes,
        );
    }
}
