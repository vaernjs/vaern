//! TypeScript parsing, AST validation, and type-stripping transform.
//!
//! This module owns stripping TypeScript types (interfaces, type aliases, generics,
//! `as` expressions, type annotations) to emit valid standard ECMAScript code.
//! It intentionally rejects constructs that cannot be erased without changing runtime
//! behaviour (enums, const enums, namespaces, decorators, auto-accessors) with
//! descriptive errors.
//!
//! Types are removed in place by `blank_strip`, so stack traces and error
//! locations match the `.ts` file. Modules that path cannot rewrite safely are
//! re-printed from the stripped AST instead, which runs the same code with
//! shifted line numbers.

use crate::error::{Result, VaernError};
use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap, Spanned, sync::Lrc};
use swc_ecma_ast::*;
use swc_ecma_codegen::{Config, Emitter, text_writer::JsWriter};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_ecma_transforms_typescript::strip;
use swc_ecma_visit::{Visit, VisitWith};

/// Configuration options for the type-stripping pipeline.
#[derive(Debug, Clone, Default)]
pub struct TranspileOptions {
    /// Source filename for error reporting and source mapping.
    pub filename: String,
}

/// Trait defining TypeScript type-stripping operations.
pub trait Transpiler: Send + Sync {
    /// Strips TypeScript syntax from input source, returning executable JavaScript.
    fn transpile(&self, filename: &str, source: &str) -> Result<String>;
}

/// Default SWC-based type-stripper implementation.
#[derive(Debug, Default, Clone)]
pub struct SwcTranspiler;

impl Transpiler for SwcTranspiler {
    fn transpile(&self, filename: &str, source: &str) -> Result<String> {
        let globals = Globals::new();
        GLOBALS.set(&globals, || {
            let cm: Lrc<SourceMap> = Default::default();
            let fm = cm.new_source_file(FileName::Real(filename.into()).into(), source.to_string());

            let syntax = Syntax::Typescript(TsSyntax {
                tsx: false,
                decorators: true,
                no_early_errors: false,
                ..Default::default()
            });

            let lexer = Lexer::new(syntax, EsVersion::latest(), StringInput::from(&*fm), None);

            let mut parser = Parser::new_from(lexer);

            let module = parser.parse_module().map_err(|err| {
                let loc = cm.lookup_char_pos(err.span().lo());
                VaernError::Syntax {
                    file: filename.to_string(),
                    line: loc.line,
                    col: loc.col_display + 1,
                    message: format!("{:?}", err.kind()),
                }
            })?;

            // Reject non-erasable syntax (enums, namespaces, decorators) before stripping
            let mut validator = UnsupportedValidator {
                cm: &cm,
                filename: filename.to_string(),
                error: None,
            };
            module.visit_with(&mut validator);
            if let Some(err) = validator.error {
                return Err(err);
            }

            if let Some(js) = crate::blank_strip::strip(&module, source, fm.start_pos)
                && parses_as_javascript(&cm, filename, &js)
            {
                return Ok(js);
            }

            // Re-print from the AST when the in-place path declined the module.
            let unresolved_mark = Mark::new();
            let top_level_mark = Mark::new();
            let mut program = Program::Module(module);
            let mut pass = strip(unresolved_mark, top_level_mark);
            pass.process(&mut program);
            let Program::Module(module) = program else {
                return Err(VaernError::Execution {
                    file: filename.to_string(),
                    message: "type stripping returned a script instead of a module".to_string(),
                });
            };

            // Emit stripped ECMAScript
            let mut buf = Vec::new();
            {
                let mut emitter = Emitter {
                    cfg: Config::default(),
                    cm: cm.clone(),
                    comments: None,
                    wr: JsWriter::new(cm.clone(), "\n", &mut buf, None),
                };
                emitter
                    .emit_module(&module)
                    .map_err(|err| VaernError::Execution {
                        file: filename.to_string(),
                        message: format!("code generator failure: {err}"),
                    })?;
            }

            String::from_utf8(buf).map_err(|err| VaernError::Execution {
                file: filename.to_string(),
                message: format!("emitted invalid UTF-8: {err}"),
            })
        })
    }
}

/// Checks that in-place stripping left valid JavaScript. A miss here means a
/// construct `blank_strip` did not account for, and the caller re-prints.
fn parses_as_javascript(cm: &Lrc<SourceMap>, filename: &str, js: &str) -> bool {
    let fm = cm.new_source_file(
        FileName::Custom(format!("{filename} (stripped)")).into(),
        js.to_string(),
    );
    let lexer = Lexer::new(
        Syntax::Es(Default::default()),
        EsVersion::latest(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    parser.parse_module().is_ok() && parser.take_errors().is_empty()
}

/// AST visitor that detects and flags unsupported TypeScript syntax for M1a.
struct UnsupportedValidator<'a> {
    cm: &'a SourceMap,
    filename: String,
    error: Option<VaernError>,
}

impl<'a> Visit for UnsupportedValidator<'a> {
    // Enums declare runtime bindings. Dropping them would move the failure to a
    // distant `ReferenceError` at first use, so they are rejected at their source.
    fn visit_ts_enum_decl(&mut self, decl: &TsEnumDecl) {
        if self.error.is_none() {
            let loc = self.cm.lookup_char_pos(decl.span.lo);
            let (feature, construct) = if decl.is_const {
                ("const enum", "const enums")
            } else {
                ("enum", "enums")
            };
            self.error = Some(VaernError::UnsupportedFeature {
                file: self.filename.clone(),
                line: loc.line,
                col: loc.col_display + 1,
                feature: feature.to_string(),
                message: format!(
                    "TypeScript {construct} emit runtime values and cannot be erased; replace '{}' with a const object such as `const {} = {{ A: 0 }} as const`",
                    decl.id.sym, decl.id.sym
                ),
            });
        }
        decl.visit_children_with(self);
    }

    fn visit_ts_module_decl(&mut self, decl: &TsModuleDecl) {
        if self.error.is_none() {
            let loc = self.cm.lookup_char_pos(decl.span.lo);
            self.error = Some(VaernError::UnsupportedFeature {
                file: self.filename.clone(),
                line: loc.line,
                col: loc.col_display + 1,
                feature: "namespace".to_string(),
                message: "TypeScript namespaces are not supported in M1a runtime".to_string(),
            });
        }
        decl.visit_children_with(self);
    }

    // The engine cannot parse `accessor`, and erasing the keyword would turn
    // a getter/setter pair on the prototype into an own data field, so the
    // member is rejected rather than rewritten.
    fn visit_auto_accessor(&mut self, accessor: &AutoAccessor) {
        if self.error.is_none() {
            let loc = self.cm.lookup_char_pos(accessor.span.lo);
            self.error = Some(VaernError::UnsupportedFeature {
                file: self.filename.clone(),
                line: loc.line,
                col: loc.col_display + 1,
                feature: "accessor".to_string(),
                message: "auto-accessor fields are not supported by the engine and cannot be erased without changing behaviour; declare a getter and setter over a private field instead".to_string(),
            });
        }
        accessor.visit_children_with(self);
    }

    fn visit_decorator(&mut self, decorator: &Decorator) {
        if self.error.is_none() {
            let loc = self.cm.lookup_char_pos(decorator.span.lo);
            self.error = Some(VaernError::UnsupportedFeature {
                file: self.filename.clone(),
                line: loc.line,
                col: loc.col_display + 1,
                feature: "decorator".to_string(),
                message: "TypeScript decorators are not supported in M1a runtime".to_string(),
            });
        }
        decorator.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_interfaces_and_types() {
        let transpiler = SwcTranspiler;
        let ts = "interface Foo { x: number; }\ntype Bar = string;\nconst x: Bar = 'hello';";
        let js = transpiler.transpile("types.ts", ts).unwrap();
        assert!(!js.contains("interface"));
        assert!(!js.contains("type Bar"));
        assert!(js.contains("const x      = 'hello';"), "{js}");
    }

    #[test]
    fn test_strip_generics_and_as() {
        let transpiler = SwcTranspiler;
        let ts = "function id<T>(x: T): T { return x as T; }";
        let js = transpiler.transpile("generics.ts", ts).unwrap();
        assert!(!js.contains("<T>"));
        assert!(!js.contains("as T"));
        assert!(js.contains("function id   (x   )"), "{js}");
    }

    #[test]
    fn test_reject_enum() {
        let transpiler = SwcTranspiler;
        let ts = "const c = 1;\nenum Color { Red, Green, Blue }";
        let err = transpiler.transpile("enum.ts", ts).unwrap_err();
        match err {
            VaernError::UnsupportedFeature {
                feature,
                line,
                col,
                message,
                ..
            } => {
                assert_eq!(feature, "enum");
                assert_eq!(line, 2);
                assert_eq!(col, 1);
                assert!(message.contains("Color"));
            }
            _ => panic!("expected UnsupportedFeature for enum"),
        }
    }

    #[test]
    fn test_reject_const_enum() {
        let transpiler = SwcTranspiler;
        let ts = "export const enum Dir { Up, Down }";
        let err = transpiler.transpile("const_enum.ts", ts).unwrap_err();
        match err {
            VaernError::UnsupportedFeature { feature, line, .. } => {
                assert_eq!(feature, "const enum");
                assert_eq!(line, 1);
            }
            _ => panic!("expected UnsupportedFeature for const enum"),
        }
    }

    #[test]
    fn test_reject_nested_enum() {
        let transpiler = SwcTranspiler;
        let ts = "function f() {\n  enum Inner { A }\n  return Inner.A;\n}";
        let err = transpiler.transpile("nested_enum.ts", ts).unwrap_err();
        match err {
            VaernError::UnsupportedFeature {
                feature, line, col, ..
            } => {
                assert_eq!(feature, "enum");
                assert_eq!(line, 2);
                assert_eq!(col, 3);
            }
            _ => panic!("expected UnsupportedFeature for nested enum"),
        }
    }

    #[test]
    fn test_reject_namespace() {
        let transpiler = SwcTranspiler;
        let ts = "namespace MySpace { export const x = 1; }";
        let res = transpiler.transpile("ns.ts", ts);
        assert!(res.is_err());
        let err = res.unwrap_err();
        match err {
            VaernError::UnsupportedFeature { feature, line, .. } => {
                assert_eq!(feature, "namespace");
                assert_eq!(line, 1);
            }
            _ => panic!("expected UnsupportedFeature for namespace"),
        }
    }

    #[test]
    fn test_reject_decorator() {
        let transpiler = SwcTranspiler;
        let ts = "@sealed\nclass Greeter {}";
        let res = transpiler.transpile("dec.ts", ts);
        assert!(res.is_err());
        let err = res.unwrap_err();
        match err {
            VaernError::UnsupportedFeature { feature, line, .. } => {
                assert_eq!(feature, "decorator");
                assert_eq!(line, 1);
            }
            _ => panic!("expected UnsupportedFeature for decorator"),
        }
    }

    #[test]
    fn test_reject_auto_accessor_at_its_location() {
        let transpiler = SwcTranspiler;
        for (ts, col) in [
            ("class Box {\n  accessor x: number = 1;\n}", 3),
            ("class Box {\n    static accessor #y = 2;\n}", 5),
        ] {
            match transpiler.transpile("acc.ts", ts) {
                Err(VaernError::UnsupportedFeature {
                    feature,
                    line,
                    col: at,
                    message,
                    ..
                }) => {
                    assert_eq!((feature.as_str(), line, at), ("accessor", 2, col), "{ts}");
                    assert!(message.contains("getter and setter"), "{message}");
                }
                other => panic!("expected UnsupportedFeature for accessor, got {other:?}"),
            }
        }
    }

    /// Line and 1-based column of the first occurrence of `needle`.
    fn position(text: &str, needle: &str) -> (usize, usize) {
        let at = text.find(needle).expect("needle present");
        let before = &text[..at];
        let line = before.matches('\n').count() + 1;
        let col = at - before.rfind('\n').map_or(0, |i| i + 1) + 1;
        (line, col)
    }

    fn assert_same_position(ts: &str, js: &str, needle: &str) {
        assert_eq!(
            position(ts, needle),
            position(js, needle),
            "{needle} moved:\n{js}"
        );
    }

    #[test]
    fn test_multiline_types_keep_line_numbers() {
        let ts = "interface Config {\n  name: string;\n  retries: number;\n}\n\ntype Handler = (\n  input: string,\n) => void;\n\nfunction fail(config: Config): never {\n  throw new Error(config.name);\n}\nfail({ name: \"x\", retries: 1 });\n";
        let js = SwcTranspiler.transpile("lines.ts", ts).unwrap();
        assert_eq!(ts.lines().count(), js.lines().count(), "{js}");
        assert_same_position(ts, &js, "throw new Error");
        assert_same_position(ts, &js, "fail({");
        assert!(!js.contains("interface") && !js.contains("Handler"), "{js}");
    }

    #[test]
    fn test_multiline_expression_types_keep_columns() {
        let ts = "const items = [1, 2, 3] as\n  readonly number[];\nconst first = items[0]!;\nconst pick = <T,>(\n  list: readonly T[],\n): T | undefined => list[0];\nconsole.log(pick<number>(items), first satisfies number);\n";
        let js = SwcTranspiler.transpile("exprs.ts", ts).unwrap();
        assert_same_position(ts, &js, "console.log");
        assert_same_position(ts, &js, "list[0]");
        assert!(
            !js.contains("readonly") && !js.contains("satisfies") && !js.contains('!'),
            "{js}"
        );
    }

    #[test]
    fn test_class_members_keep_line_numbers() {
        let ts = "abstract class Base<T> implements Iterable<T>,\n    Disposable {\n  private readonly items: T[] = [];\n  declare tag: string;\n  [key: string]: unknown;\n  count?: number;\n  ready!: boolean;\n  abstract size(): number;\n  add(item: T): void;\n  add(item: T, again?: boolean): void {\n    this.items.push(item);\n  }\n  protected override toString(this: Base<T>): string {\n    return `${this.items.length}`;\n  }\n}\nexport { Base };\n";
        let js = SwcTranspiler.transpile("class.ts", ts).unwrap();
        assert_same_position(ts, &js, "this.items.push");
        assert_same_position(ts, &js, "return `");
        for word in [
            "abstract",
            "implements",
            "private",
            "readonly",
            "declare",
            "protected",
            "override",
            "?:",
            "!:",
        ] {
            assert!(!js.contains(word), "{word} left in:\n{js}");
        }
    }

    #[test]
    fn test_type_only_imports_are_removed() {
        let ts = "import type { Shape } from \"./shape.ts\";\nimport { type Point, area, unused } from \"./geo.ts\";\nimport Default, * as ns from \"./lib.ts\";\nexport type { Shape };\ninterface Local {}\nexport { Local, area };\nconst p: Point = { x: 1 };\nconsole.log(area(p), ns.version);\n";
        let js = SwcTranspiler.transpile("imports.ts", ts).unwrap();
        assert_same_position(ts, &js, "console.log");
        assert!(
            !js.contains("Shape") && !js.contains("Point") && !js.contains("unused"),
            "{js}"
        );
        assert!(!js.contains("Default") && !js.contains("Local"), "{js}");
        assert!(js.contains("area") && js.contains("* as ns"), "{js}");
    }

    #[test]
    fn test_blanked_statement_does_not_join_neighbours() {
        let ts = "let total = 1\ninterface Gap {}\n(globalThis as any).total = total\n";
        let js = SwcTranspiler.transpile("asi.ts", ts).unwrap();
        assert!(
            js.lines().nth(1).is_some_and(|l| l.starts_with(';')),
            "{js}"
        );
    }

    #[test]
    fn test_parameter_properties_still_transpile() {
        let ts = "class P {\n  constructor(private x: number) {}\n}\nnew P(1);\n";
        let js = SwcTranspiler.transpile("params.ts", ts).unwrap();
        assert!(js.contains("this.x = x"), "{js}");
    }
}
