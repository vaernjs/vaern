//! TypeScript parsing, AST validation, and type-stripping transform.
//!
//! This module owns stripping TypeScript types (interfaces, type aliases, generics,
//! `as` expressions, type annotations) to emit valid standard ECMAScript code.
//! It intentionally rejects constructs that cannot be erased without changing runtime
//! behaviour (enums, const enums, namespaces, decorators) with descriptive errors.

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

            // Apply pure TypeScript type-stripping
            let unresolved_mark = Mark::new();
            let top_level_mark = Mark::new();
            let mut program = Program::Module(module);
            let mut pass = strip(unresolved_mark, top_level_mark);
            pass.process(&mut program);
            let module = match program {
                Program::Module(m) => m,
                Program::Script(_) => unreachable!(),
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
        assert!(js.contains("const x = 'hello';") || js.contains("const x = \"hello\";"));
    }

    #[test]
    fn test_strip_generics_and_as() {
        let transpiler = SwcTranspiler;
        let ts = "function id<T>(x: T): T { return x as T; }";
        let js = transpiler.transpile("generics.ts", ts).unwrap();
        assert!(!js.contains("<T>"));
        assert!(!js.contains("as T"));
        assert!(js.contains("function id(x)"));
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
}
