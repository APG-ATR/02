pub use self::name::Name;
use self::{
    scope::{Scope, ScopeKind},
    util::PatExt,
};
use super::Checker;
use crate::{
    builtin_types::Lib,
    errors::Error,
    loader::Load,
    ty::{Alias, Param, Type, TypeRefExt},
    util::IntoCow,
    Rule,
};
use fxhash::{FxHashMap, FxHashSet};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::{borrow::Cow, cell::RefCell, path::PathBuf, sync::Arc};
use swc_atoms::{js_word, JsWord};
use swc_common::{Span, Spanned, Visit, VisitWith};
use swc_ecma_ast::*;

mod control_flow;
pub mod export;
mod expr;
mod generic;
mod name;
mod scope;
mod type_facts;
mod util;

struct Analyzer<'a, 'b> {
    info: Info,
    resolved_imports: FxHashMap<JsWord, Arc<Type<'static>>>,
    errored_imports: FxHashSet<JsWord>,
    pending_exports: Vec<((JsWord, Span), Box<Expr>)>,
    inferred_return_types: RefCell<Vec<Type<'static>>>,
    scope: Scope<'a>,
    /// Used in variable declarartions
    declaring: Vec<JsWord>,
    path: Arc<PathBuf>,
    loader: &'b dyn Load,
    libs: &'b [Lib],
    rule: Rule,
}

impl<T> Visit<Vec<T>> for Analyzer<'_, '_>
where
    T: VisitWith<Self> + for<'any> VisitWith<ImportFinder<'any>> + Send + Sync,
    Vec<T>: VisitWith<Self>,
{
    fn visit(&mut self, items: &Vec<T>) {
        // We first load imports.

        let mut imports: Vec<ImportInfo> = vec![];

        items.iter().for_each(|item| {
            // EXtract imports
            item.visit_with(&mut ImportFinder { to: &mut imports });
            // item.visit_with(self);
        });

        let loader = self.loader;
        let path = self.path.clone();
        let import_results = imports
            .par_iter()
            .map(|import| {
                loader.load(path.clone(), &*import).map_err(|err| {
                    //
                    (import, err)
                })
            })
            .collect::<Vec<_>>();

        for res in import_results {
            match res {
                Ok(import) => {
                    self.resolved_imports.extend(import);
                }
                Err((import, mut err)) => {
                    match err {
                        Error::ModuleLoadFailed { ref mut errors, .. } => {
                            self.info.errors.append(errors);
                        }
                        _ => {}
                    }
                    // Mark errored imported types as any to prevent useless errors
                    self.errored_imports.extend(
                        import
                            .items
                            .iter()
                            .map(|&Specifier { ref local, .. }| local.0.clone()),
                    );

                    self.info.errors.push(err);
                }
            }
        }

        items.visit_children(self);

        self.handle_pending_exports();
    }
}

impl Visit<TsModuleDecl> for Analyzer<'_, '_> {
    fn visit(&mut self, decl: &TsModuleDecl) {
        // TODO: Uncomment the line below.
        // Uncommenting the line somehow returns without excuting subsequent codes.
        // decl.visit_children(self);

        // println!("after: visit<TsModuleDecl>: {:?}", decl.id);

        self.scope.register_type(
            match decl.id {
                TsModuleName::Ident(ref i) => i.sym.clone(),
                TsModuleName::Str(ref s) => s.value.clone(),
            },
            decl.clone().into(),
        );
    }
}

impl Visit<TsInterfaceDecl> for Analyzer<'_, '_> {
    fn visit(&mut self, decl: &TsInterfaceDecl) {
        self.scope
            .register_type(decl.id.sym.clone(), decl.clone().into());
    }
}

impl Visit<TsTypeAliasDecl> for Analyzer<'_, '_> {
    fn visit(&mut self, decl: &TsTypeAliasDecl) {
        let ty: Type<'_> = decl.type_ann.clone().into();

        let ty = if decl.type_params.is_none() {
            match self.expand_type(decl.span(), ty.owned()) {
                Ok(ty) => ty.to_static(),
                Err(err) => {
                    self.info.errors.push(err);
                    Type::any(decl.span())
                }
            }
        } else {
            ty
        };

        self.scope.register_type(
            decl.id.sym.clone(),
            Type::Alias(Alias {
                span: decl.span(),
                ty: box ty.owned(),
                type_params: decl.type_params.clone().map(From::from),
            }),
        );

        // TODO: Validate type
    }
}

#[derive(Debug)]
struct ImportFinder<'a> {
    to: &'a mut Vec<ImportInfo>,
}

/// Extracts require('foo')
impl Visit<CallExpr> for ImportFinder<'_> {
    fn visit(&mut self, expr: &CallExpr) {
        let span = expr.span();

        match expr.callee {
            ExprOrSuper::Expr(box Expr::Ident(ref i)) if i.sym == js_word!("require") => {
                let src = expr
                    .args
                    .iter()
                    .map(|v| match *v.expr {
                        Expr::Lit(Lit::Str(Str { ref value, .. })) => value.clone(),
                        _ => unimplemented!("error reporting for dynamic require"),
                    })
                    .next()
                    .unwrap();
                self.to.push(ImportInfo {
                    span,
                    all: true,
                    items: vec![],
                    src,
                });
            }
            _ => return,
        }
    }
}

impl Visit<ImportDecl> for ImportFinder<'_> {
    fn visit(&mut self, import: &ImportDecl) {
        let span = import.span();
        let mut items = vec![];
        let mut all = false;

        for s in &import.specifiers {
            match *s {
                ImportSpecifier::Default(ref default) => items.push(Specifier {
                    export: (js_word!("default"), default.span),
                    local: (default.local.sym.clone(), default.local.span),
                }),
                ImportSpecifier::Specific(ref s) => {
                    items.push(Specifier {
                        export: (
                            s.imported
                                .clone()
                                .map(|v| v.sym)
                                .unwrap_or_else(|| s.local.sym.clone()),
                            s.span,
                        ),
                        local: (s.local.sym.clone(), s.local.span),
                    });
                }
                ImportSpecifier::Namespace(..) => all = true,
            }
        }

        if !items.is_empty() {
            self.to.push(ImportInfo {
                span,
                items,
                all,
                src: import.src.value.clone(),
            });
        }
    }
}

impl<'a, 'b> Analyzer<'a, 'b> {
    pub fn new(
        libs: &'b [Lib],
        rule: Rule,
        scope: Scope<'a>,
        path: Arc<PathBuf>,
        loader: &'b dyn Load,
    ) -> Self {
        Analyzer {
            libs,
            rule,
            scope,
            info: Default::default(),
            inferred_return_types: Default::default(),
            path,
            declaring: vec![],
            resolved_imports: Default::default(),
            errored_imports: Default::default(),
            pending_exports: Default::default(),
            loader,
        }
    }
}

#[derive(Debug, Default)]
pub struct Info {
    pub exports: FxHashMap<JsWord, Arc<Type<'static>>>,
    pub errors: Vec<Error>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImportInfo {
    pub span: Span,
    pub items: Vec<Specifier>,
    pub all: bool,
    pub src: JsWord,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Specifier {
    pub local: (JsWord, Span),
    pub export: (JsWord, Span),
}

impl Visit<TsEnumDecl> for Analyzer<'_, '_> {
    fn visit(&mut self, e: &TsEnumDecl) {
        e.visit_children(self);

        self.scope.register_type(e.id.sym.clone(), e.clone().into());
    }
}

impl Visit<ClassExpr> for Analyzer<'_, '_> {
    fn visit(&mut self, c: &ClassExpr) {
        let ty = match self.validate_type_of_class(&c.class) {
            Ok(ty) => ty,
            Err(err) => {
                self.info.errors.push(err);
                Type::any(c.span()).into()
            }
        };

        self.scope.this = Some(ty.clone());

        if let Some(ref i) = c.ident {
            self.scope.register_type(i.sym.clone(), ty.clone());

            self.scope.declare_var(
                VarDeclKind::Var,
                i.sym.clone(),
                Some(ty),
                // initialized = true
                true,
                // declare Class does not allow multiple declarations.
                false,
            );
        }

        c.visit_children(self);

        self.scope.this = None;
    }
}

impl Visit<ClassDecl> for Analyzer<'_, '_> {
    fn visit(&mut self, c: &ClassDecl) {
        let ty = match self.validate_type_of_class(&c.class) {
            Ok(ty) => ty,
            Err(err) => {
                self.info.errors.push(err);
                Type::any(c.span()).into()
            }
        };

        self.scope.this = Some(ty.clone());

        self.scope.register_type(c.ident.sym.clone(), ty.clone());

        self.scope.declare_var(
            VarDeclKind::Var,
            c.ident.sym.clone(),
            Some(ty),
            // initialized = true
            true,
            // declare Class does not allow multiple declarations.
            false,
        );

        c.visit_children(self);

        self.scope.this = None;
    }
}

impl Analyzer<'_, '_> {
    /// TODO: Handle recursive funciton
    fn visit_fn(&mut self, name: Option<&Ident>, f: &Function) -> Type<'static> {
        let fn_ty = self.with_child(ScopeKind::Fn, Default::default(), |child| {
            if let Some(name) = name {
                // We use `typeof function` to infer recursive function's return type.
                child.scope.declare_var(
                    VarDeclKind::Var,
                    name.sym.clone(),
                    Some(Type::Simple(Cow::Owned(
                        TsTypeQuery {
                            span: f.span,
                            expr_name: TsEntityName::Ident(name.clone()),
                        }
                        .into(),
                    ))),
                    // value is initialized
                    true,
                    // Allow overriding
                    true,
                );
            }

            match f.type_params {
                Some(TsTypeParamDecl { ref params, .. }) => {
                    params.iter().for_each(|param| {
                        let ty = Type::Param(Param {
                            span: param.span,
                            name: param.name.sym.clone(),
                            constraint: param.constraint.as_ref().map(|v| box v.clone().into_cow()),
                            default: param.default.as_ref().map(|v| box v.clone().into_cow()),
                        });

                        child
                            .scope
                            .facts
                            .types
                            .insert(param.name.sym.clone().into(), ty);
                    });
                }
                None => {}
            }

            f.params.iter().for_each(|pat| {
                {
                    child.declaring = vec![];

                    let mut visitor = VarVisitor {
                        names: &mut child.declaring,
                    };

                    pat.visit_with(&mut visitor);
                }

                child.declare_vars(VarDeclKind::Let, pat)
            });

            f.visit_children(child);

            let fn_ty = child.type_of_fn(f)?;

            Ok(fn_ty)
        });

        match fn_ty {
            Ok(ty) => ty.to_static(),
            Err(err) => {
                self.info.errors.push(err);
                Type::any(f.span)
            }
        }
    }
}

impl Visit<FnDecl> for Analyzer<'_, '_> {
    /// NOTE: This method **should not call f.visit_children(self)**
    fn visit(&mut self, f: &FnDecl) {
        println!("Visiting {}", f.ident.sym);
        let fn_ty = self.visit_fn(Some(&f.ident), &f.function);

        match self
            .scope
            .override_var(VarDeclKind::Var, f.ident.sym.clone(), fn_ty)
        {
            Ok(()) => {}
            Err(err) => {
                self.info.errors.push(err);
            }
        }
    }
}

impl Visit<FnExpr> for Analyzer<'_, '_> {
    /// NOTE: This method **should not call f.visit_children(self)**
    fn visit(&mut self, f: &FnExpr) {
        self.visit_fn(f.ident.as_ref(), &f.function);
    }
}

impl Visit<Function> for Analyzer<'_, '_> {
    fn visit(&mut self, f: &Function) {
        self.visit_fn(None, f);
    }
}

impl Visit<ArrowExpr> for Analyzer<'_, '_> {
    fn visit(&mut self, f: &ArrowExpr) {
        self.with_child(ScopeKind::Fn, Default::default(), |child| {
            match f.type_params {
                Some(TsTypeParamDecl { ref params, .. }) => {
                    params.iter().for_each(|param| {
                        let ty = Type::Param(Param {
                            span: param.span,
                            name: param.name.sym.clone(),
                            constraint: param.constraint.as_ref().map(|v| box v.clone().into_cow()),
                            default: param.default.as_ref().map(|v| box v.clone().into_cow()),
                        });

                        child
                            .scope
                            .facts
                            .types
                            .insert(param.name.sym.clone().into(), ty);
                    });
                }
                None => {}
            }

            f.params
                .iter()
                .for_each(|pat| child.declare_vars(VarDeclKind::Let, pat));

            f.visit_children(child);

            match f.body {
                BlockStmtOrExpr::Expr(ref expr) => {
                    child.visit_return_arg(expr.span(), Some(expr));
                }
                _ => {}
            }
        });
    }
}

impl Visit<BlockStmt> for Analyzer<'_, '_> {
    fn visit(&mut self, stmt: &BlockStmt) {
        self.with_child(ScopeKind::Block, Default::default(), |analyzer| {
            stmt.visit_children(analyzer);
        })
    }
}

impl Visit<AssignExpr> for Analyzer<'_, '_> {
    fn visit(&mut self, expr: &AssignExpr) {
        let span = expr.span();

        let rhs_ty = match self
            .type_of(&expr.right)
            .and_then(|ty| self.expand_type(span, ty))
        {
            Ok(rhs_ty) => rhs_ty.to_static(),
            Err(err) => {
                self.info.errors.push(err);
                return;
            }
        };
        if expr.op == op!("=") {
            self.try_assign(&expr.left, &rhs_ty);
        }
    }
}

impl Visit<VarDecl> for Analyzer<'_, '_> {
    fn visit(&mut self, var: &VarDecl) {
        let kind = var.kind;

        var.decls.iter().for_each(|v| {
            if let Some(ref init) = v.init {
                let span = init.span();

                v.visit_with(self);

                //  Check if v_ty is assignable to ty
                let value_ty = match self
                    .type_of(&init)
                    .and_then(|ty| self.expand_type(span, ty))
                {
                    Ok(ty) => ty,
                    Err(err) => {
                        self.info.errors.push(err);
                        return;
                    }
                };

                match v.name.get_ty() {
                    Some(ty) => {
                        let ty = Type::from(ty.clone());
                        let ty = match self.expand_type(span, Cow::Owned(ty)) {
                            Ok(ty) => ty,
                            Err(err) => {
                                self.info.errors.push(err);
                                return;
                            }
                        };
                        let error = value_ty.assign_to(&ty, v.span());
                        let ty = ty.to_static();
                        match error {
                            Ok(()) => {
                                match self.scope.declare_complex_vars(kind, &v.name, ty) {
                                    Ok(()) => {}
                                    Err(err) => {
                                        self.info.errors.push(err);
                                    }
                                }
                                return;
                            }
                            Err(err) => {
                                self.info.errors.push(err);
                            }
                        }
                    }
                    None => {
                        // infer type from value.

                        let ty = value_ty.to_static();

                        match self.scope.declare_complex_vars(kind, &v.name, ty) {
                            Ok(()) => {}
                            Err(err) => {
                                self.info.errors.push(err);
                            }
                        }
                        return;
                    }
                }
            } else {
                if !var.declare {
                    let (sym, ty) = match v.name {
                        Pat::Ident(Ident {
                            span,
                            ref sym,
                            ref type_ann,
                            ..
                        }) => (
                            sym.clone(),
                            match type_ann.as_ref().map(|t| Type::from(t.type_ann.clone())) {
                                Some(ty) => match self.expand_type(span, ty.into_cow()) {
                                    Ok(ty) => Some(ty.to_static()),
                                    Err(err) => {
                                        self.info.errors.push(err);
                                        return;
                                    }
                                },
                                None => None,
                            },
                        ),
                        _ => unreachable!(
                            "complex pattern without initializer is invalid syntax and parser \
                             should handle it"
                        ),
                    };
                    self.scope.declare_var(
                        kind,
                        sym,
                        ty,
                        // initialized
                        false,
                        // allow_multiple
                        kind == VarDeclKind::Var,
                    );
                    return;
                }
            }

            self.declare_vars(kind, &v.name);
        });
    }
}

impl Analyzer<'_, '_> {
    fn try_assign(&mut self, lhs: &PatOrExpr, ty: Cow<TsType>) {
        match *lhs {
            PatOrExpr::Expr(ref expr) | PatOrExpr::Pat(box Pat::Expr(ref expr)) => match **expr {
                // TODO(kdy1): Validate
                Expr::Member(MemberExpr { .. }) => return,
                _ => unimplemented!(
                    "assign: {:?} = {:?}\nFile: {}",
                    expr,
                    ty,
                    self.path.display()
                ),
            },

            PatOrExpr::Pat(ref pat) => {
                // Update variable's type
                match **pat {
                    Pat::Ident(ref i) => {
                        if let Some(var_info) = self.scope.vars.get_mut(&i.sym) {
                            // Variable is declared.

                            let var_ty = if let Some(ref var_ty) = var_info.ty {
                                // let foo: string;
                                // let foo = 'value';

                                let errors = ty.assign_to(&var_ty);
                                if errors.is_none() {
                                    Some(ty.into_owned())
                                } else {
                                    self.info.errors.extend(errors);
                                    None
                                }
                            } else {
                                // let v = foo;
                                // v = bar;
                                None
                            };
                            if let Some(var_ty) = var_ty {
                                if var_info.ty.is_none() || !var_info.ty.as_ref().unwrap().is_any()
                                {
                                    var_info.ty = Some(var_ty);
                                }
                            }
                        } else {
                            let var_info = if let Some(var_info) = self.scope.search_parent(&i.sym)
                            {
                                VarInfo {
                                    ty: if var_info.ty.is_some()
                                        && var_info.ty.as_ref().unwrap().is_any()
                                    {
                                        Some(any(var_info.ty.as_ref().unwrap().span()))
                                    } else {
                                        Some(ty.into_owned())
                                    },
                                    copied: true,
                                    ..var_info.clone()
                                }
                            } else {
                                // undefined symbol
                                self.info
                                    .errors
                                    .push(Error::UndefinedSymbol { span: i.span });
                                return;
                            };
                            // Variable is defined on parent scope.
                            //
                            // We copy varinfo with enhanced type.
                            self.scope.vars.insert(i.sym.clone(), var_info);
                        }
                    }

                    _ => unimplemented!("assignment with complex pattern"),
                }
            }
        }
    }
}

/// Analyzes a module.
///
/// Constants are propagated, and
impl Checker<'_> {
    pub fn analyze_module(&self, rule: Rule, path: Arc<PathBuf>, m: &Module) -> Info {
        ::swc_common::GLOBALS.set(&self.globals, || {
            let mut a = Analyzer::new(&self.libs, rule, Scope::root(), path, &self);
            m.visit_with(&mut a);

            a.info
        })
    }
}

struct VarVisitor<'a> {
    pub names: &'a mut Vec<JsWord>,
}

impl Visit<Expr> for VarVisitor<'_> {
    fn visit(&mut self, _: &Expr) {}
}

impl Visit<Ident> for VarVisitor<'_> {
    fn visit(&mut self, i: &Ident) {
        self.names.push(i.sym.clone())
    }
}

fn _assert_types() {
    fn is_sync<T: Sync>() {}
    fn is_send<T: Send>() {}
    is_sync::<Info>();
    is_send::<Info>();
}
