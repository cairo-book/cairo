use cairo_lang_defs::ids::ModuleId;
use cairo_lang_defs::patcher::{PatchBuilder, RewriteNode};
use cairo_lang_defs::plugin::{
    MacroPlugin, MacroPluginMetadata, PluginDiagnostic, PluginGeneratedFile, PluginResult,
};
use cairo_lang_semantic::db::SemanticGroup;
use cairo_lang_semantic::plugin::{AnalyzerPlugin, PluginSuite};
use cairo_lang_semantic::{GenericArgumentId, Mutability, corelib};
use cairo_lang_syntax::attribute::consts::IMPLICIT_PRECEDENCE_ATTR;
use cairo_lang_syntax::node::db::SyntaxGroup;
use cairo_lang_syntax::node::helpers::{OptionWrappedGenericParamListHelper, QueryAttrs};
use cairo_lang_syntax::node::{TypedStablePtr, ast};
use indoc::formatdoc;
use itertools::Itertools;

pub const RUNNABLE_ATTR: &str = "runnable";
pub const RUNNABLE_RAW_ATTR: &str = "runnable_raw";
pub const RUNNABLE_PREFIX: &str = "__runnable_wrapper__";

/// Returns a plugin suite with the `RunnablePlugin` and `RawRunnableAnalyzer`.
pub fn runnable_plugin_suite() -> PluginSuite {
    std::mem::take(
        PluginSuite::default()
            .add_plugin::<RunnablePlugin>()
            .add_analyzer_plugin::<RawRunnableAnalyzer>(),
    )
}

const IMPLICIT_PRECEDENCE: &[&str] = &[
    "core::pedersen::Pedersen",
    "core::RangeCheck",
    "core::integer::Bitwise",
    "core::ec::EcOp",
    "core::poseidon::Poseidon",
    "core::circuit::RangeCheck96",
    "core::circuit::AddMod",
    "core::circuit::MulMod",
];

#[derive(Debug, Default)]
#[non_exhaustive]
struct RunnablePlugin;

impl MacroPlugin for RunnablePlugin {
    fn generate_code(
        &self,
        db: &dyn SyntaxGroup,
        item_ast: ast::ModuleItem,
        _metadata: &MacroPluginMetadata<'_>,
    ) -> PluginResult {
        let ast::ModuleItem::FreeFunction(item) = item_ast else {
            return PluginResult::default();
        };
        if !item.has_attr(db, RUNNABLE_ATTR) {
            return PluginResult::default();
        }
        let mut diagnostics = vec![];
        let mut builder = PatchBuilder::new(db, &item);
        let declaration = item.declaration(db);
        let generics = declaration.generic_params(db);
        if !generics.is_empty(db) {
            diagnostics.push(PluginDiagnostic::error(
                &generics,
                "Runnable functions cannot have generic params.".to_string(),
            ));
            return PluginResult { code: None, diagnostics, remove_original_item: false };
        }
        let name = declaration.name(db);
        let implicits_precedence =
            RewriteNode::Text(format!("#[{IMPLICIT_PRECEDENCE_ATTR}({})]", {
                IMPLICIT_PRECEDENCE.iter().join(", ")
            }));
        builder.add_modified(RewriteNode::interpolate_patched(&formatdoc! {"

                $implicit_precedence$
                #[{RUNNABLE_RAW_ATTR}]
                fn {RUNNABLE_PREFIX}$function_name$(mut input: Span<felt252>, ref output: Array<felt252>) {{\n
            "},
            &[
                ("implicit_precedence".into(), implicits_precedence,),
                ("function_name".into(), RewriteNode::from_ast(&name))
            ].into()
        ));
        let params = declaration.signature(db).parameters(db).elements(db);
        for (param_idx, param) in params.iter().enumerate() {
            builder.add_modified(
                RewriteNode::Text(format!(
                    "    let __param{RUNNABLE_PREFIX}{param_idx} = Serde::deserialize(ref \
                     input).expect('Failed to deserialize param #{param_idx}');\n"
                ))
                .mapped(db, param),
            );
        }
        builder.add_str(
            "    assert(core::array::SpanTrait::is_empty(input), 'Input too long for params.');\n",
        );
        builder.add_modified(RewriteNode::interpolate_patched(
            "    let __result = @$function_name$(\n",
            &[("function_name".into(), RewriteNode::from_ast(&name))].into(),
        ));
        for (param_idx, param) in params.iter().enumerate() {
            builder.add_modified(
                RewriteNode::Text(format!("        __param{RUNNABLE_PREFIX}{param_idx},\n"))
                    .mapped(db, param),
            );
        }
        builder.add_str("    );\n");
        let mut serialize_node = RewriteNode::text("    Serde::serialize(__result, ref output);\n");
        if let ast::OptionReturnTypeClause::ReturnTypeClause(clause) =
            declaration.signature(db).ret_ty(db)
        {
            serialize_node = serialize_node.mapped(db, &clause);
        }
        builder.add_modified(serialize_node);
        builder.add_str("}\n");
        let (content, code_mappings) = builder.build();
        PluginResult {
            code: Some(PluginGeneratedFile {
                name: "runnable".into(),
                content,
                code_mappings,
                aux_data: None,
            }),
            diagnostics,
            remove_original_item: false,
        }
    }

    fn declared_attributes(&self) -> Vec<String> {
        vec![RUNNABLE_ATTR.to_string(), RUNNABLE_RAW_ATTR.to_string()]
    }

    fn executable_attributes(&self) -> Vec<String> {
        vec![RUNNABLE_RAW_ATTR.to_string()]
    }
}

/// Plugin to add diagnostics on bad `#[runnable_raw]` annotations.
#[derive(Default, Debug)]
struct RawRunnableAnalyzer;

impl AnalyzerPlugin for RawRunnableAnalyzer {
    fn diagnostics(&self, db: &dyn SemanticGroup, module_id: ModuleId) -> Vec<PluginDiagnostic> {
        let syntax_db = db.upcast();
        let mut diagnostics = vec![];
        let Ok(free_functions) = db.module_free_functions(module_id) else {
            return diagnostics;
        };
        for (id, item) in free_functions.iter() {
            if !item.has_attr(syntax_db, RUNNABLE_RAW_ATTR) {
                continue;
            }
            let Ok(signature) = db.free_function_signature(*id) else {
                continue;
            };
            if signature.return_type != corelib::unit_ty(db) {
                diagnostics.push(PluginDiagnostic::error(
                    &signature.stable_ptr.lookup(syntax_db).ret_ty(syntax_db),
                    "Invalid return type for `#[runnable_raw]` function, expected `()`."
                        .to_string(),
                ));
            }
            let [input, output] = &signature.params[..] else {
                diagnostics.push(PluginDiagnostic::error(
                    &signature.stable_ptr.lookup(syntax_db).parameters(syntax_db),
                    "Invalid number of params for `#[runnable_raw]` function, expected 2."
                        .to_string(),
                ));
                continue;
            };
            if input.ty
                != corelib::get_core_ty_by_name(db, "Span".into(), vec![GenericArgumentId::Type(
                    corelib::core_felt252_ty(db),
                )])
            {
                diagnostics.push(PluginDiagnostic::error(
                    input.stable_ptr.untyped(),
                    "Invalid first param type for `#[runnable_raw]` function, expected \
                     `Span<felt252>`."
                        .to_string(),
                ));
            }
            if input.mutability == Mutability::Reference {
                diagnostics.push(PluginDiagnostic::error(
                    input.stable_ptr.untyped(),
                    "Invalid first param mutability for `#[runnable_raw]` function, got \
                     unexpected `ref`."
                        .to_string(),
                ));
            }
            if output.ty != corelib::core_array_felt252_ty(db) {
                diagnostics.push(PluginDiagnostic::error(
                    output.stable_ptr.untyped(),
                    "Invalid second param type for `#[runnable_raw]` function, expected \
                     `Array<felt252>`."
                        .to_string(),
                ));
            }
            if output.mutability != Mutability::Reference {
                diagnostics.push(PluginDiagnostic::error(
                    output.stable_ptr.untyped(),
                    "Invalid second param mutability for `#[runnable_raw]` function, expected \
                     `ref`."
                        .to_string(),
                ));
            }
        }
        diagnostics
    }
}