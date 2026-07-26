use std::sync::Arc;

use regex::Regex;

use crate::internal::{
    compat::JsFeature,
    config::{
        self, Format, InjectedFile, JsxOptions, Mode, Platform, ProcessedDefines, TsAlwaysStrict,
        TsOptions,
    },
    js_ast::{ModuleTypeData, values_look_the_same},
    logger::PathStyle,
};

/// Parser options used as part of esbuild's incremental-build cache key.
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Options {
    pub injected_files: Vec<InjectedFile>,
    pub jsx: JsxOptions,
    pub ts_always_strict: Option<Arc<TsAlwaysStrict>>,
    pub mangle_props: Option<Arc<Regex>>,
    pub reserve_props: Option<Arc<Regex>>,
    pub drop_labels: Vec<String>,
    pub defines: Option<Arc<ProcessedDefines>>,

    pub original_target_env: String,
    pub module_type_data: ModuleTypeData,
    pub unsupported_js_features: JsFeature,
    pub unsupported_js_feature_overrides: JsFeature,
    pub unsupported_js_feature_overrides_mask: JsFeature,
    pub ts: TsOptions,
    pub mode: Mode,
    pub platform: Platform,
    pub output_format: Format,
    pub log_path_style: PathStyle,
    pub code_path_style: PathStyle,
    pub ascii_only: bool,
    pub keep_names: bool,
    pub minify_syntax: bool,
    pub minify_identifiers: bool,
    pub minify_whitespace: bool,
    pub omit_runtime_for_tests: bool,
    pub omit_jsx_runtime_for_tests: bool,
    pub ignore_dce_annotations: bool,
    pub tree_shaking: bool,
    pub drop_debugger: bool,
    pub mangle_quoted: bool,
    pub decode_hydrate_runtime_state_yarn_pnp: bool,
}

#[must_use]
pub fn options_for_yarn_pnp() -> Options {
    Options {
        decode_hydrate_runtime_state_yarn_pnp: true,
        ..Options::default()
    }
}

#[must_use]
pub fn options_from_config(options: &config::Options) -> Options {
    Options {
        injected_files: options.injected_files.clone(),
        jsx: options.jsx.clone(),
        defines: options.defines.clone(),
        ts_always_strict: options.ts_always_strict.clone(),
        mangle_props: options.mangle_props.clone(),
        reserve_props: options.reserve_props.clone(),
        drop_labels: options.drop_labels.clone(),
        unsupported_js_features: options.unsupported_js_features,
        unsupported_js_feature_overrides: options.unsupported_js_feature_overrides,
        unsupported_js_feature_overrides_mask: options.unsupported_js_feature_overrides_mask,
        original_target_env: options.original_target_environment.clone(),
        ts: options.ts.clone(),
        mode: options.mode,
        platform: options.platform,
        output_format: options.output_format,
        module_type_data: options.module_type_data.clone(),
        ascii_only: options.ascii_only,
        keep_names: options.keep_names,
        minify_syntax: options.minify_syntax,
        minify_identifiers: options.minify_identifiers,
        minify_whitespace: options.minify_whitespace,
        omit_runtime_for_tests: options.omit_runtime_for_tests,
        omit_jsx_runtime_for_tests: options.omit_jsx_runtime_for_tests,
        ignore_dce_annotations: options.ignore_dce_annotations,
        tree_shaking: options.tree_shaking,
        drop_debugger: options.drop_debugger,
        mangle_quoted: options.mangle_quoted,
        log_path_style: options.log_path_style,
        code_path_style: options.code_path_style,
        decode_hydrate_runtime_state_yarn_pnp: false,
    }
}

impl Options {
    /// Compare the cache-key-relevant option state.
    ///
    /// # Panics
    ///
    /// Panics if two non-null processed-define objects have different map
    /// lengths, which mirrors upstream's internal consistency assertion.
    #[must_use]
    pub fn equal(&self, other: &Self) -> bool {
        if !self.structurally_equal(other)
            || self.ts_always_strict.as_deref() != other.ts_always_strict.as_deref()
            || !same_regex(self.mangle_props.as_deref(), other.mangle_props.as_deref())
            || !same_regex(
                self.reserve_props.as_deref(),
                other.reserve_props.as_deref(),
            )
            || self.drop_labels != other.drop_labels
            || !injected_files_equal(&self.injected_files, &other.injected_files)
            || !jsx_options_equal(&self.jsx, &other.jsx)
        {
            return false;
        }

        if let (Some(left), Some(right)) = (&self.defines, &other.defines) {
            assert!(
                left.identifier_defines.len() == right.identifier_defines.len()
                    && left.dot_defines.len() == right.dot_defines.len(),
                "Internal error"
            );
        } else if self.defines.is_some() != other.defines.is_some() {
            panic!("Internal error");
        }
        true
    }

    fn structurally_equal(&self, other: &Self) -> bool {
        self.original_target_env == other.original_target_env
            && module_type_data_equal(&self.module_type_data, &other.module_type_data)
            && self.unsupported_js_features == other.unsupported_js_features
            && self.unsupported_js_feature_overrides == other.unsupported_js_feature_overrides
            && self.unsupported_js_feature_overrides_mask
                == other.unsupported_js_feature_overrides_mask
            && self.ts == other.ts
            && self.mode == other.mode
            && self.platform == other.platform
            && self.output_format == other.output_format
            && self.log_path_style == other.log_path_style
            && self.code_path_style == other.code_path_style
            && self.ascii_only == other.ascii_only
            && self.keep_names == other.keep_names
            && self.minify_syntax == other.minify_syntax
            && self.minify_identifiers == other.minify_identifiers
            && self.minify_whitespace == other.minify_whitespace
            && self.omit_runtime_for_tests == other.omit_runtime_for_tests
            && self.omit_jsx_runtime_for_tests == other.omit_jsx_runtime_for_tests
            && self.ignore_dce_annotations == other.ignore_dce_annotations
            && self.tree_shaking == other.tree_shaking
            && self.drop_debugger == other.drop_debugger
            && self.mangle_quoted == other.mangle_quoted
            && self.decode_hydrate_runtime_state_yarn_pnp
                == other.decode_hydrate_runtime_state_yarn_pnp
    }
}

fn same_regex(left: Option<&Regex>, right: Option<&Regex>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.as_str() == right.as_str(),
        _ => false,
    }
}

fn injected_files_equal(left: &[InjectedFile], right: &[InjectedFile]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.source == right.source
                && left.define_name == right.define_name
                && left.exports.len() == right.exports.len()
                && left
                    .exports
                    .iter()
                    .zip(&right.exports)
                    .all(|(left, right)| left.alias == right.alias && left.loc == right.loc)
        })
}

fn jsx_options_equal(left: &JsxOptions, right: &JsxOptions) -> bool {
    left.parse == right.parse
        && define_expr_equal(&left.factory, &right.factory)
        && define_expr_equal(&left.fragment, &right.fragment)
}

fn define_expr_equal(left: &config::DefineExpr, right: &config::DefineExpr) -> bool {
    if left.parts != right.parts || left.injected_define_index != right.injected_define_index {
        return false;
    }
    match (
        left.constant.data.as_deref(),
        right.constant.data.as_deref(),
    ) {
        (None, None) => true,
        (Some(left), Some(right)) => values_look_the_same(Some(left), Some(right)),
        _ => false,
    }
}

fn module_type_data_equal(left: &ModuleTypeData, right: &ModuleTypeData) -> bool {
    left.source == right.source
        && left.range == right.range
        && left.module_type == right.module_type
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regex::Regex;

    use super::{Options, options_for_yarn_pnp, options_from_config};
    use crate::internal::{
        config::{self, DefineExpr},
        js_ast::{Expr, ExprData},
        logger::Loc,
    };

    #[test]
    fn translates_config_options() {
        let config = config::Options {
            minify_syntax: true,
            tree_shaking: true,
            drop_labels: vec!["DEV".into()],
            mangle_props: Some(Arc::new(Regex::new("^_").expect("valid regex"))),
            ..config::Options::default()
        };
        let options = options_from_config(&config);
        assert!(options.minify_syntax);
        assert!(options.tree_shaking);
        assert_eq!(options.drop_labels, ["DEV"]);
        assert_eq!(
            options.mangle_props.as_deref().map(Regex::as_str),
            Some("^_")
        );
    }

    #[test]
    fn cache_key_equality_compares_semantics_not_pointers() {
        let mut left = Options::default();
        let mut right = Options::default();
        left.mangle_props = Some(Arc::new(Regex::new("^x").expect("valid regex")));
        right.mangle_props = Some(Arc::new(Regex::new("^x").expect("valid regex")));
        left.jsx.factory = DefineExpr {
            constant: Expr::new(Loc::default(), ExprData::Number(1.0)),
            ..DefineExpr::default()
        };
        right.jsx.factory = left.jsx.factory.clone();
        assert!(left.equal(&right));
        right.keep_names = true;
        assert!(!left.equal(&right));
    }

    #[test]
    fn yarn_pnp_options_enable_only_runtime_hydration() {
        let options = options_for_yarn_pnp();
        assert!(options.decode_hydrate_runtime_state_yarn_pnp);
        let mut baseline = Options::default();
        assert!(!options.equal(&baseline));
        baseline.decode_hydrate_runtime_state_yarn_pnp = true;
        assert!(options.equal(&baseline));
    }
}
