#![allow(dead_code)]

use crate::internal::{
    ast::Ref,
    js_ast::{Binding, BindingData, Expr, ExprData, PropertyFlags, Stmt, StmtData},
};

use super::parser_core::ParserCore;

pub(crate) fn visit_top_level_statements(core: &mut ParserCore, statements: &mut [Stmt]) {
    for statement in statements {
        match statement.data.as_deref_mut() {
            Some(StmtData::Expr(expression)) => visit_expr(core, &mut expression.value),
            Some(StmtData::Local(local)) => {
                for declaration in &mut local.declarations {
                    visit_binding_initializers(core, &mut declaration.binding);
                    visit_expr(core, &mut declaration.value_or_nil);
                }
            }
            Some(StmtData::ExportDefault(export)) => {
                if let Some(StmtData::Expr(expression)) = export.value.data.as_deref_mut() {
                    visit_expr(core, &mut expression.value);
                }
            }
            Some(StmtData::If(statement)) => visit_expr(core, &mut statement.test),
            Some(StmtData::DoWhile(statement)) => visit_expr(core, &mut statement.test),
            Some(StmtData::While(statement)) => visit_expr(core, &mut statement.test),
            Some(StmtData::With(statement)) => visit_expr(core, &mut statement.value),
            Some(StmtData::Throw(statement)) => visit_expr(core, &mut statement.value),
            _ => {}
        }
    }
}

fn visit_binding_initializers(core: &mut ParserCore, binding: &mut Binding) {
    match binding.data.as_deref_mut() {
        Some(BindingData::Array(array)) => {
            for item in &mut array.items {
                visit_binding_initializers(core, &mut item.binding);
                visit_expr(core, &mut item.default_value_or_nil);
            }
        }
        Some(BindingData::Object(object)) => {
            for property in &mut object.properties {
                if property.is_computed {
                    visit_expr(core, &mut property.key);
                }
                visit_binding_initializers(core, &mut property.value);
                visit_expr(core, &mut property.default_value_or_nil);
            }
        }
        Some(BindingData::Missing | BindingData::Identifier(_)) | None => {}
    }
}

#[allow(clippy::too_many_lines)]
fn visit_expr(core: &mut ParserCore, expression: &mut Expr) {
    let Some(data) = expression.data.as_deref_mut() else {
        return;
    };
    match data {
        ExprData::Identifier(identifier) => {
            if is_stored_name_ref(identifier.reference) {
                let name = String::from_utf8_lossy(core.load_name_from_ref(identifier.reference))
                    .into_owned();
                let result = core.find_symbol(expression.loc, &name);
                identifier.reference = result.reference;
                identifier.must_keep_due_to_with_stmt = result.is_inside_with_scope;
            } else {
                core.record_usage(identifier.reference);
            }
        }
        ExprData::ImportIdentifier(identifier) => core.record_usage(identifier.reference),
        ExprData::Array(array) => {
            for item in &mut array.items {
                visit_expr(core, item);
            }
        }
        ExprData::Unary(unary) => visit_expr(core, &mut unary.value),
        ExprData::Binary(binary) => {
            visit_expr(core, &mut binary.left);
            visit_expr(core, &mut binary.right);
        }
        ExprData::New(new) => {
            visit_expr(core, &mut new.target);
            for argument in &mut new.args {
                visit_expr(core, argument);
            }
        }
        ExprData::Call(call) => {
            visit_expr(core, &mut call.target);
            for argument in &mut call.args {
                visit_expr(core, argument);
            }
        }
        ExprData::Dot(dot) => visit_expr(core, &mut dot.target),
        ExprData::Index(index) => {
            visit_expr(core, &mut index.target);
            visit_expr(core, &mut index.index);
        }
        ExprData::Object(object) => {
            for property in &mut object.properties {
                if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                    visit_expr(core, &mut property.key);
                }
                visit_expr(core, &mut property.value_or_nil);
                visit_expr(core, &mut property.initializer_or_nil);
                for decorator in &mut property.decorators {
                    visit_expr(core, &mut decorator.value);
                }
            }
        }
        ExprData::Spread(spread) => visit_expr(core, &mut spread.value),
        ExprData::Template(template) => {
            visit_expr(core, &mut template.tag_or_nil);
            for part in &mut template.parts {
                visit_expr(core, &mut part.value);
            }
        }
        ExprData::InlinedEnum(inlined) => visit_expr(core, &mut inlined.value),
        ExprData::Annotation(annotation) => visit_expr(core, &mut annotation.value),
        ExprData::Await(await_expression) => visit_expr(core, &mut await_expression.value),
        ExprData::Yield(yield_expression) => {
            visit_expr(core, &mut yield_expression.value_or_nil);
        }
        ExprData::If(if_expression) => {
            visit_expr(core, &mut if_expression.test);
            visit_expr(core, &mut if_expression.yes);
            visit_expr(core, &mut if_expression.no);
        }
        ExprData::ImportCall(import) => {
            visit_expr(core, &mut import.expr);
            visit_expr(core, &mut import.options_or_nil);
        }
        ExprData::Arrow(_)
        | ExprData::Function(_)
        | ExprData::Class(_)
        | ExprData::Boolean(_)
        | ExprData::Super
        | ExprData::Null
        | ExprData::Undefined
        | ExprData::This
        | ExprData::NewTarget(_)
        | ExprData::ImportMeta(_)
        | ExprData::PrivateIdentifier(_)
        | ExprData::NameOfSymbol(_)
        | ExprData::JsxElement(_)
        | ExprData::JsxText(_)
        | ExprData::Missing
        | ExprData::Number(_)
        | ExprData::BigInt(_)
        | ExprData::String(_)
        | ExprData::RegExp(_)
        | ExprData::RequireString(_)
        | ExprData::RequireResolveString(_)
        | ExprData::ImportString(_) => {}
    }
}

fn is_stored_name_ref(reference: Ref) -> bool {
    reference.source_index & 0x8000_0000 != 0
}
