use convert_case::{Case, Casing};
use handlebars::{Context, Handlebars, Helper, HelperResult, Output, RenderContext};
use rustc_hash::FxHashMap;
use serde::Deserialize;
use turbopack_binding::swc::core::{
    bundler::ModuleRecord,
    cached::regex::CachedRegex,
    common::{SyntaxContext, DUMMY_SP},
    ecma::{
        ast::{
            CallExpr, Callee, Decl, Expr, Id, Ident, Lit, MemberProp, Module, ModuleItem, Pat,
            Script, Stmt, VarDecl, VarDeclKind, VarDeclarator,
        },
        atoms::{Atom, JsWord},
        utils::{prepend_stmts, private_ident, ExprFactory, IdentRenamer},
        visit::{as_folder, noop_visit_mut_type, Fold, VisitMut, VisitMutWith},
    },
};

pub fn cjs_optimizer(config: Config, unresolved_ctxt: SyntaxContext) -> impl Fold + VisitMut {
    let mut folder = CjsOptimizer {
        data: Data::default(),
        renderer: handlebars::Handlebars::new(),
        packages: vec![],
        unresolved_ctxt,
    };
    folder
        .renderer
        .register_helper("lowerCase", Box::new(helper_lower_case));
    folder
        .renderer
        .register_helper("upperCase", Box::new(helper_upper_case));
    folder
        .renderer
        .register_helper("camelCase", Box::new(helper_camel_case));
    folder
        .renderer
        .register_helper("kebabCase", Box::new(helper_kebab_case));
    for (mut k, v) in config.packages {
        // XXX: Should we keep this hack?
        if !k.starts_with('^') && !k.ends_with('$') {
            k = format!("^{}$", k);
        }
        folder.packages.push((
            CachedRegex::new(&k).expect("transform-imports: invalid regex"),
            v,
        ));
    }

    as_folder(folder)
}

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub packages: FxHashMap<String, PackageConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageConfig {
    pub transform: String,
    #[serde(default)]
    pub prevent_full_import: bool,
    #[serde(default)]
    pub skip_default_conversion: bool,
}

struct Rewriter<'a> {
    renderer: &'a handlebars::Handlebars<'static>,
    key: &'a str,
    config: &'a PackageConfig,
    group: Vec<&'a str>,
}

struct CjsOptimizer {
    data: Data,
    renderer: handlebars::Handlebars<'static>,
    packages: Vec<(CachedRegex, PackageConfig)>,
    unresolved_ctxt: SyntaxContext,
}

#[derive(Debug, Default)]
struct Data {
    /// List of `require` calls **which should be replaced**.
    ///
    ///  `(identifier): (module_record)`
    imports: FxHashMap<Id, ImportRecord>,

    /// `(module_specifier, property): (identifier)`
    replaced: FxHashMap<(Atom, JsWord), Id>,

    extra_stmts: Vec<Stmt>,

    rename_map: FxHashMap<Id, Id>,
}

#[derive(Debug)]
struct ImportRecord {
    module_specifier: Atom,
}

impl CjsOptimizer {
    fn should_rewrite<'a>(&'a self, name: &'a str) -> Option<Rewriter<'a>> {
        for (regex, config) in &self.packages {
            let group = regex.captures(name);
            if let Some(group) = group {
                let group = group
                    .iter()
                    .map(|x| x.map(|x| x.as_str()).unwrap_or_default())
                    .collect::<Vec<&str>>();
                return Some(Rewriter {
                    renderer: &self.renderer,
                    key: name,
                    config,
                    group,
                });
            }
        }
        None
    }
}

impl VisitMut for CjsOptimizer {
    noop_visit_mut_type!();

    fn visit_mut_expr(&mut self, e: &mut Expr) {
        e.visit_mut_children_with(self);

        if let Expr::Member(n) = e {
            if let MemberProp::Ident(prop) = &n.prop {
                if let Expr::Ident(obj) = &*n.obj {
                    if let Some(record) = self.data.imports.get(&obj.to_id()) {
                        let new_id = self
                            .data
                            .replaced
                            .entry((record.module_specifier.clone(), prop.sym.clone()))
                            .or_insert_with(|| private_ident!(prop.sym.clone()).to_id())
                            .clone();

                        // TODO: Adjust module specifier
                        let var = VarDeclarator {
                            span: DUMMY_SP,
                            name: Pat::Ident(new_id.clone().into()),
                            init: Some(Box::new(Expr::Call(CallExpr {
                                span: DUMMY_SP,
                                callee: Ident::new(
                                    "require".into(),
                                    DUMMY_SP.with_ctxt(self.unresolved_ctxt),
                                )
                                .as_callee(),
                                args: vec![Expr::Lit(Lit::Str(
                                    record.module_specifier.clone().into(),
                                ))
                                .as_arg()],
                                type_args: None,
                            }))),
                            definite: false,
                        };

                        self.data
                            .extra_stmts
                            .push(Stmt::Decl(Decl::Var(Box::new(VarDecl {
                                span: DUMMY_SP,
                                kind: VarDeclKind::Const,
                                declare: false,
                                decls: vec![var],
                            }))));

                        *e = Expr::Ident(new_id.into());
                    }
                }
            }
        }
    }

    fn visit_mut_var_declarator(&mut self, n: &mut VarDeclarator) {
        n.visit_mut_children_with(self);

        // Find `require('foo')`
        if let Some(Expr::Call(CallExpr {
            callee: Callee::Expr(callee),
            args,
            ..
        })) = n.init.as_deref()
        {
            if let Expr::Ident(ident) = &**callee {
                if ident.span.ctxt == self.unresolved_ctxt && ident.sym == *"require" {
                    if let Some(arg) = args.get(0) {
                        if let Expr::Lit(Lit::Str(v)) = &*arg.expr {
                            // TODO: Config

                            if let Pat::Ident(name) = &n.name {
                                self.data.imports.insert(
                                    name.to_id(),
                                    ImportRecord {
                                        module_specifier: v.value.clone().into(),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn visit_mut_module(&mut self, n: &mut Module) {
        n.visit_mut_children_with(self);

        prepend_stmts(
            &mut n.body,
            self.data.extra_stmts.drain(..).map(ModuleItem::Stmt),
        );

        n.visit_mut_children_with(&mut IdentRenamer::new(&self.data.rename_map));
    }

    fn visit_mut_script(&mut self, n: &mut Script) {
        n.visit_mut_children_with(self);

        prepend_stmts(&mut n.body, self.data.extra_stmts.drain(..));

        n.visit_mut_children_with(&mut IdentRenamer::new(&self.data.rename_map));
    }
}

fn helper_lower_case(
    h: &Helper<'_, '_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    // get parameter from helper or throw an error
    let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");
    out.write(param.to_lowercase().as_ref())?;
    Ok(())
}

fn helper_upper_case(
    h: &Helper<'_, '_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    // get parameter from helper or throw an error
    let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");
    out.write(param.to_uppercase().as_ref())?;
    Ok(())
}

fn helper_camel_case(
    h: &Helper<'_, '_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    // get parameter from helper or throw an error
    let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");

    out.write(param.to_case(Case::Camel).as_ref())?;
    Ok(())
}

fn helper_kebab_case(
    h: &Helper<'_, '_>,
    _: &Handlebars<'_>,
    _: &Context,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn Output,
) -> HelperResult {
    // get parameter from helper or throw an error
    let param = h.param(0).and_then(|v| v.value().as_str()).unwrap_or("");

    out.write(param.to_case(Case::Kebab).as_ref())?;
    Ok(())
}
