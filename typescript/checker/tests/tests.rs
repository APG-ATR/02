#![recursion_limit = "256"]
#![feature(vec_remove_item)]
#![feature(box_syntax)]
#![feature(box_patterns)]
#![feature(specialization)]
#![feature(test)]

extern crate env_logger;
extern crate serde;
extern crate serde_json;
extern crate swc_common;
extern crate swc_ecma_ast;
extern crate swc_ecma_parser;
extern crate swc_ts_checker;
extern crate test;
extern crate testing;
extern crate walkdir;

use serde::Deserialize;
use std::{
    collections::HashSet,
    env,
    fs::File,
    io::{self, Read},
    path::Path,
};
use swc_common::{comments::Comments, FileName, Fold, FoldWith, Span, Spanned, CM};
use swc_ecma_ast::{Module, *};
use swc_ecma_parser::{Parser, Session, SourceFileInput, Syntax, TsConfig};
use swc_ts_checker::{Lib, Rule};
use test::{test_main, DynTestFn, ShouldPanic::No, TestDesc, TestDescAndFn, TestName, TestType};
use testing::StdErr;
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Error {
    pub line: usize,
    pub column: usize,
    pub msg: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Error,
    Pass,
    Conformance,
}

#[test]
fn conformance() {
    let args: Vec<_> = env::args().collect();
    let mut tests = Vec::new();
    add_tests(&mut tests, Mode::Conformance).unwrap();
    test_main(&args, tests, Default::default());
}

#[test]
fn passes() {
    let args: Vec<_> = env::args().collect();
    let mut tests = Vec::new();
    add_tests(&mut tests, Mode::Pass).unwrap();
    test_main(&args, tests, Default::default());
}

#[test]
fn errors() {
    let args: Vec<_> = env::args().collect();
    let mut tests = Vec::new();
    add_tests(&mut tests, Mode::Error).unwrap();
    test_main(&args, tests, Default::default());
}

fn add_tests(tests: &mut Vec<TestDescAndFn>, mode: Mode) -> Result<(), io::Error> {
    let test_kind = match mode {
        Mode::Error => "errors",
        Mode::Conformance => "conformance",
        Mode::Pass => "pass",
    };

    let root = {
        let mut root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
        root.push("tests");
        root.push(test_kind);

        root
    };

    eprintln!("Loading tests from {}", root.display());

    let dir = root;

    for entry in WalkDir::new(&dir).into_iter() {
        let entry = entry?;
        let is_ts = entry.file_name().to_string_lossy().ends_with(".ts")
            || entry.file_name().to_string_lossy().ends_with(".tsx");
        if entry.file_type().is_dir() || !is_ts {
            continue;
        }

        let is_not_index = !entry.file_name().to_string_lossy().ends_with("index.d.ts")
            && !entry.file_name().to_string_lossy().ends_with("index.ts")
            && !entry.file_name().to_string_lossy().ends_with("index.tsx");
        if is_not_index && mode != Mode::Conformance {
            continue;
        }

        let file_name = entry
            .path()
            .strip_prefix(&dir)
            .expect("failed to strip prefix")
            .to_str()
            .unwrap()
            .to_string();

        let input = {
            let mut buf = String::new();
            File::open(entry.path())?.read_to_string(&mut buf)?;
            buf
        };

        let ignore = file_name.contains("circular")
            || input.contains("@filename")
            || input.contains("@Filename")
            || input.contains("@module")
            || (mode == Mode::Conformance
                && !file_name.contains(&env::var("TEST").ok().unwrap_or(String::from(""))));

        let dir = dir.clone();
        let name = format!("tsc::{}::{}", test_kind, file_name);
        add_test(tests, name, ignore, move || {
            if mode == Mode::Error || mode == Mode::Conformance {
                eprintln!(
                    "\n\n========== Running error reporting test {}\nSource:\n{}\n",
                    file_name, input
                );
            } else {
                eprintln!(
                    "\n\n========== Running test {}\nSource:\n{}\n",
                    file_name, input
                );
            }

            let path = dir.join(&file_name);
            do_test(false, &path, mode).unwrap();
        });
    }

    Ok(())
}

fn do_test(treat_error_as_bug: bool, file_name: &Path, mode: Mode) -> Result<(), StdErr> {
    let _ = env_logger::try_init();

    let fname = file_name.display().to_string();
    let mut ref_errors = match mode {
        Mode::Conformance => {
            let fname = file_name.file_name().unwrap();
            let errors_file =
                file_name.with_file_name(format!("{}.errors.json", fname.to_string_lossy()));
            if !errors_file.exists() {
                println!("errors file does not exists: {}", errors_file.display());
                Some(vec![])
            } else {
                let errors: Vec<Error> = serde_json::from_reader(
                    File::open(errors_file).expect("failed to open error sfile"),
                )
                .expect("failed to parse errors.txt.json");

                // TODO: Match column and message

                Some(
                    errors
                        .into_iter()
                        .map(|e| (e.line, e.column))
                        .collect::<Vec<_>>(),
                )
            }
        }
        _ => None,
    };
    let all_ref_errors = ref_errors.clone();
    let ref_err_cnt = ref_errors.as_ref().map(Vec::len).unwrap_or(0);

    let (libs, rule, ts_config) = ::testing::run_test(treat_error_as_bug, |cm, handler| {
        Ok(match mode {
            Mode::Pass | Mode::Error => (
                vec![Lib::Es5, Lib::Dom],
                Default::default(),
                Default::default(),
            ),
            Mode::Conformance => {
                // We parse files twice. At first, we read comments and detect
                // configurations for following parse.

                let session = Session { handler: &handler };

                let fm = cm.load_file(file_name).expect("failed to read file");
                let comments = Comments::default();

                let mut parser = Parser::new(
                    session,
                    Syntax::Typescript(TsConfig {
                        tsx: fname.contains("tsx"),
                        ..Default::default()
                    }),
                    SourceFileInput::from(&*fm),
                    Some(&comments),
                );

                let module = parser.parse_module().map_err(|mut e| {
                    e.emit();
                    ()
                })?;
                let module = if mode == Mode::Conformance {
                    make_test(&comments, module)
                } else {
                    module
                };

                let mut libs = vec![Lib::Es5];
                let mut rule = Rule::default();
                let ts_config = TsConfig::default();

                let span = module.span;
                let cmts = comments.leading_comments(span.lo());
                match cmts {
                    Some(ref cmts) => {
                        for cmt in cmts.iter() {
                            let s = cmt.text.trim();
                            if !s.starts_with("@") {
                                continue;
                            }
                            let s = &s[1..]; // '@'

                            if s.starts_with("target:") || s.starts_with("Target:") {
                                libs = Lib::load(&s["target:".len()..].trim());
                            } else if s.starts_with("strict:") {
                                let strict = s["strict:".len()..].trim().parse().unwrap();
                                rule.no_implicit_any = strict;
                                rule.no_implicit_this = strict;
                                rule.always_strict = strict;
                                rule.strict_null_checks = strict;
                                rule.strict_function_types = strict;
                            } else if s.starts_with("noLib:") {
                                let v = s["noLib:".len()..].trim().parse().unwrap();
                                if v {
                                    libs = vec![];
                                }
                            } else if s.starts_with("noImplicitAny:") {
                                let v = s["noImplicitAny:".len()..].trim().parse().unwrap();
                                rule.no_implicit_any = v;
                            } else if s.starts_with("noImplicitReturns:") {
                                let v = s["noImplicitReturns:".len()..].trim().parse().unwrap();
                                rule.no_implicit_returns = v;
                            } else if s.starts_with("declaration") {
                                // TODO: Create d.ts
                            } else if s.starts_with("stripInternal:") {
                                // TODO: Create d.ts
                            } else if s.starts_with("traceResolution") {
                                // no-op
                            } else if s.starts_with("allowUnusedLabels:") {
                                let v = s["allowUnusedLabels:".len()..].trim().parse().unwrap();
                                rule.allow_unused_labels = v;
                            } else if s.starts_with("noEmitHelpers") {
                                // TODO
                            } else if s.starts_with("downlevelIteration: ") {
                                // TODO
                            } else if s.starts_with("sourceMap:") || s.starts_with("sourcemap:") {
                                // TODO
                            } else if s.starts_with("isolatedModules:") {
                                // TODO
                            } else if s.starts_with("lib:") {
                                let mut ls = HashSet::<_>::default();
                                for v in s["lib:".len()..].trim().split(",") {
                                    ls.extend(Lib::load(v))
                                }
                                libs = ls.into_iter().collect()
                            } else if s.starts_with("allowUnreachableCode:") {
                                let v = s["allowUnreachableCode:".len()..].trim().parse().unwrap();
                                rule.allow_unreachable_code = v;
                            } else if s.starts_with("strictNullChecks:") {
                                let v = s["strictNullChecks:".len()..].trim().parse().unwrap();
                                rule.strict_null_checks = v;
                            } else if s.starts_with("noImplicitThis:") {
                                let v = s["noImplicitThis:".len()..].trim().parse().unwrap();
                                rule.no_implicit_this = v;
                            } else {
                                panic!("Comment is not handled: {}", s);
                            }
                        }
                    }
                    None => {}
                }

                (libs, rule, ts_config)
            }
        })
    })
    .ok()
    .unwrap_or_default();

    let res = ::testing::run_test(treat_error_as_bug, |cm, handler| {
        CM.set(&cm.clone(), || {
            let checker = swc_ts_checker::Checker::new(
                cm.clone(),
                handler,
                libs,
                rule,
                TsConfig {
                    tsx: fname.contains("tsx"),
                    ..ts_config
                },
            );

            let errors = ::swc_ts_checker::errors::Error::flatten(checker.check(file_name.into()));
            if let Some(ref mut ref_errors) = ref_errors {
                assert_eq!(mode, Mode::Conformance);
                // Line of errors (actual result)
                let actual_errors = errors
                    .iter()
                    .map(|e| {
                        let span = e.span();
                        let cp = cm.lookup_char_pos(span.lo());

                        return (cp.line, cp.col.0 + 1);
                    })
                    .collect::<Vec<_>>();

                // We only emit errors which has wrong line.
                if *ref_errors != actual_errors {
                    checker.run(|| {
                        for (e, line_col) in errors.into_iter().zip(actual_errors) {
                            if let None = ref_errors.remove_item(&line_col) {
                                e.emit(&handler);
                            }
                        }
                    });
                    return Err(());
                }
            }

            let res = if errors.is_empty() { Ok(()) } else { Err(()) };

            checker.run(|| {
                for e in errors {
                    e.emit(&handler);
                }
            });

            res
        })
    });

    match mode {
        Mode::Error => {
            let err = res.expect_err("should fail, but parsed as");
            if err
                .compare_to_file(format!("{}.stderr", file_name.display()))
                .is_err()
            {
                panic!()
            }
        }
        Mode::Pass => {
            res.expect("should be parsed and validated");
        }
        Mode::Conformance => {
            let err = match res {
                Ok(_) => StdErr::from(String::from("")),
                Err(err) => err,
            };

            // TODO: filter line correctly
            let mut err_lines = err.lines().enumerate().filter(|(_, l)| l.contains("$DIR"));

            let err_count = err_lines.clone().count();
            let error_line_columns = err_lines
                .clone()
                .map(|(_, s)| {
                    let mut s = s.split(":");
                    s.next();
                    let line = s
                        .next()
                        .unwrap()
                        .parse::<usize>()
                        .expect("failed to parse line");
                    let column = s
                        .next()
                        .unwrap()
                        .parse::<usize>()
                        .expect("failed to parse column");

                    (line, column)
                })
                .collect::<Vec<_>>();

            let all = err_lines.all(|(_, v)| {
                for (l, column) in ref_errors.as_ref().unwrap() {
                    if v.contains(&format!("{}:{}", l, column)) {
                        return true;
                    }
                }
                false
            });

            if err_count != ref_errors.as_ref().unwrap().len() || !all {
                panic!(
                    "\n============================================================\n{:?}
============================================================\n{} unmatched errors out of {} \
                     errors. Got {} extra errors.\nExpected: {:?}\nActual: {:?}\nRequired errors: \
                     {:?}",
                    err,
                    ref_errors.as_ref().unwrap().len(),
                    ref_err_cnt,
                    err_count,
                    ref_errors.as_ref().unwrap(),
                    error_line_columns,
                    all_ref_errors.as_ref().unwrap()
                );
            }

            if err
                .compare_to_file(format!("{}.stderr", file_name.display()))
                .is_err()
            {
                panic!()
            }
        }
    }

    Ok(())
}

fn add_test<F: FnOnce() + Send + 'static>(
    tests: &mut Vec<TestDescAndFn>,
    name: String,
    ignore: bool,
    f: F,
) {
    tests.push(TestDescAndFn {
        desc: TestDesc {
            test_type: TestType::UnitTest,
            name: TestName::DynTestName(name),
            ignore,
            should_panic: No,
            allow_fail: false,
        },
        testfn: DynTestFn(box f),
    });
}

fn make_test(c: &Comments, module: Module) -> Module {
    let mut m = TestMaker {
        c,
        stmts: Default::default(),
    };

    module.fold_with(&mut m)
}

struct TestMaker<'a> {
    c: &'a Comments,
    stmts: Vec<Stmt>,
}

impl Fold<Vec<ModuleItem>> for TestMaker<'_> {
    fn fold(&mut self, stmts: Vec<ModuleItem>) -> Vec<ModuleItem> {
        let mut ss = vec![];
        for stmt in stmts {
            let stmt = stmt.fold_with(self);
            ss.push(stmt);
            ss.extend(self.stmts.drain(..).map(ModuleItem::Stmt));
        }

        ss
    }
}

impl Fold<Vec<Stmt>> for TestMaker<'_> {
    fn fold(&mut self, stmts: Vec<Stmt>) -> Vec<Stmt> {
        let mut ss = vec![];
        for stmt in stmts {
            let stmt = stmt.fold_with(self);
            ss.push(stmt);
            ss.extend(self.stmts.drain(..));
        }

        ss
    }
}

impl Fold<TsTypeAliasDecl> for TestMaker<'_> {
    fn fold(&mut self, decl: TsTypeAliasDecl) -> TsTypeAliasDecl {
        let cmts = self.c.trailing_comments(decl.span.hi());

        match cmts {
            Some(cmts) => {
                assert!(cmts.len() == 1);
                let cmt = cmts.iter().next().unwrap();
                let t = cmt.text.trim().replace("\n", "").replace("\r", "");

                let cmt_type = match parse_type(cmt.span, &t) {
                    Some(ty) => ty,
                    None => return decl,
                };

                //  {
                //      let _value: ty = (Object as any as Alias)
                //  }
                //
                //
                let span = decl.span();
                self.stmts.push(Stmt::Block(BlockStmt {
                    span,
                    stmts: vec![Stmt::Decl(Decl::Var(VarDecl {
                        span,
                        decls: vec![VarDeclarator {
                            span,
                            name: Pat::Ident(Ident {
                                span,
                                sym: "_value".into(),
                                type_ann: Some(TsTypeAnn {
                                    span,
                                    type_ann: box cmt_type,
                                }),
                                optional: false,
                            }),
                            init: Some(box Expr::TsAs(TsAsExpr {
                                span,
                                expr: box Expr::TsAs(TsAsExpr {
                                    span,
                                    expr: box Expr::Ident(Ident::new("Object".into(), span)),
                                    type_ann: box TsType::TsKeywordType(TsKeywordType {
                                        span,
                                        kind: TsKeywordTypeKind::TsAnyKeyword,
                                    }),
                                }),
                                type_ann: box TsType::TsTypeRef(TsTypeRef {
                                    span,
                                    type_name: TsEntityName::Ident(decl.id.clone()),
                                    type_params: None,
                                }),
                            })),
                            definite: false,
                        }],
                        kind: VarDeclKind::Const,
                        declare: false,
                    }))],
                }));
            }
            None => {}
        }

        decl
    }
}

fn parse_type(span: Span, s: &str) -> Option<TsType> {
    let s = s.trim();

    if s.starts_with("error") || s.starts_with("Error") {
        return None;
    }

    let ty = ::testing::run_test(true, |cm, handler| {
        let session = Session { handler: &handler };

        let fm = cm.new_source_file(FileName::Anon, s.into());

        let mut parser = Parser::new(
            session,
            Syntax::Typescript(Default::default()),
            SourceFileInput::from(&*fm),
            None,
        );
        let ty = match parser.parse_type() {
            Ok(v) => v,
            Err(..) => return Err(()),
        };
        Ok(ty)
    });

    let mut spanner = Spanner { span };

    Some(*ty.ok()?.fold_with(&mut spanner))
}

struct Spanner {
    span: Span,
}

impl Fold<Span> for Spanner {
    fn fold(&mut self, _: Span) -> Span {
        self.span
    }
}
