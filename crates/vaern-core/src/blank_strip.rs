//! Line-preserving TypeScript type removal.
//!
//! Re-printing a module from its AST moves code: a multi-line interface
//! collapses to nothing and every later line shifts, so stack traces and
//! error locations point at the wrong place. This module removes TypeScript
//! syntax by overwriting it with spaces in the original text instead. Line
//! breaks survive, so every remaining token keeps its line and column.
//!
//! A blanked statement or class member starts with `;`, so the code around it
//! cannot merge across the gap under automatic semicolon insertion. Imports
//! used only as types are removed the way TypeScript removes them, by
//! checking whether a binding is ever referenced as a value.
//!
//! `strip` returns `None` for constructs it does not rewrite (parameter
//! properties, `import x = require()`, `export =`) and for blanks that could
//! change how the code parses. The caller then re-prints the module instead,
//! which runs correctly but loses line fidelity.

use std::collections::HashSet;
use swc_atoms::Atom;
use swc_common::{BytePos, Span, Spanned};
use swc_ecma_ast::*;
use swc_ecma_visit::{Visit, VisitWith};

/// Modifier keywords that exist only in TypeScript class members.
const TS_MEMBER_MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "readonly",
    "override",
    "declare",
    "abstract",
];

/// Removes TypeScript syntax from `source`, keeping every line break in place.
///
/// `module` must be the parse of `source`, and `base` the start position of
/// its source file.
pub fn strip(module: &Module, source: &str, base: BytePos) -> Option<String> {
    let mut refs = ValueRefs::default();
    module.visit_with(&mut refs);

    let mut blanker = Blanker {
        src: source.as_bytes(),
        base,
        value_refs: refs.names,
        type_names: top_level_type_names(module),
        ranges: Vec::new(),
        semicolons: Vec::new(),
        unsupported: false,
    };
    module.visit_with(&mut blanker);
    if blanker.unsupported {
        return None;
    }

    let mut out = source.as_bytes().to_vec();
    for (lo, hi) in &blanker.ranges {
        for byte in out.get_mut(*lo..*hi)? {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    for pos in &blanker.semicolons {
        *out.get_mut(*pos)? = b';';
    }
    String::from_utf8(out).ok()
}

/// Names declared only as types at the top level of the module.
fn top_level_type_names(module: &Module) -> HashSet<Atom> {
    let mut types = HashSet::new();
    let mut values = HashSet::new();
    for item in &module.body {
        let decl = match item {
            ModuleItem::Stmt(Stmt::Decl(decl)) => decl,
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => &export.decl,
            ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                for spec in &import.specifiers {
                    let (local, type_only) = match spec {
                        ImportSpecifier::Named(s) => (&s.local, s.is_type_only),
                        ImportSpecifier::Default(s) => (&s.local, false),
                        ImportSpecifier::Namespace(s) => (&s.local, false),
                    };
                    if import.type_only || type_only {
                        types.insert(local.sym.clone());
                    }
                }
                continue;
            }
            _ => continue,
        };
        match decl {
            Decl::TsInterface(d) => {
                types.insert(d.id.sym.clone());
            }
            Decl::TsTypeAlias(d) => {
                types.insert(d.id.sym.clone());
            }
            Decl::Class(d) => {
                values.insert(d.ident.sym.clone());
            }
            Decl::Fn(d) => {
                values.insert(d.ident.sym.clone());
            }
            Decl::Var(d) => {
                for declarator in &d.decls {
                    if let Pat::Ident(b) = &declarator.name {
                        values.insert(b.id.sym.clone());
                    }
                }
            }
            _ => {}
        }
    }
    types.retain(|name| !values.contains(name));
    types
}

/// Collects every identifier referenced outside a type position.
#[derive(Default)]
struct ValueRefs {
    names: HashSet<Atom>,
}

impl Visit for ValueRefs {
    fn visit_ident(&mut self, ident: &Ident) {
        self.names.insert(ident.sym.clone());
    }
    fn visit_import_decl(&mut self, _: &ImportDecl) {}
    fn visit_ts_type(&mut self, _: &TsType) {}
    fn visit_ts_type_ann(&mut self, _: &TsTypeAnn) {}
    fn visit_ts_type_param_decl(&mut self, _: &TsTypeParamDecl) {}
    fn visit_ts_type_param_instantiation(&mut self, _: &TsTypeParamInstantiation) {}
    fn visit_ts_expr_with_type_args(&mut self, _: &TsExprWithTypeArgs) {}
    fn visit_ts_interface_decl(&mut self, _: &TsInterfaceDecl) {}
    fn visit_ts_type_alias_decl(&mut self, _: &TsTypeAliasDecl) {}
}

struct Blanker<'a> {
    src: &'a [u8],
    base: BytePos,
    value_refs: HashSet<Atom>,
    type_names: HashSet<Atom>,
    /// Byte ranges to overwrite with spaces.
    ranges: Vec<(usize, usize)>,
    /// Byte offsets that become `;` after blanking.
    semicolons: Vec<usize>,
    unsupported: bool,
}

impl Blanker<'_> {
    fn off(&self, pos: BytePos) -> usize {
        pos.0.saturating_sub(self.base.0) as usize
    }

    /// Blanks a span inside an expression or signature. A blank that ends a
    /// line before `(`, `[`, a template or an operator could join two
    /// statements under ASI, so such a blank falls back to re-printing.
    fn blank(&mut self, span: Span) {
        let (lo, hi) = (self.off(span.lo), self.off(span.hi));
        let mut crossed_line = self.src.get(lo..hi).is_some_and(|s| s.contains(&b'\n'));
        let mut next = hi;
        while let Some(&b) = self.src.get(next) {
            if !b.is_ascii_whitespace() {
                break;
            }
            crossed_line |= b == b'\n';
            next += 1;
        }
        if crossed_line
            && matches!(
                self.src.get(next),
                Some(b'(' | b'[' | b'`' | b'+' | b'-' | b'/')
            )
        {
            self.unsupported = true;
        }
        self.ranges.push((lo, hi));
    }

    /// Blanks a whole statement or class member and leaves `;` in its place.
    fn blank_item(&mut self, span: Span) {
        let (lo, hi) = (self.off(span.lo), self.off(span.hi));
        self.ranges.push((lo, hi));
        self.semicolons.push(lo);
    }

    /// Blanks a single `?` or `!` that follows `after`, skipping whitespace.
    fn blank_marker_after(&mut self, after: BytePos) {
        let mut pos = self.off(after);
        while self.src.get(pos).is_some_and(|b| b.is_ascii_whitespace()) {
            pos += 1;
        }
        match self.src.get(pos) {
            Some(b'?' | b'!') => self.ranges.push((pos, pos + 1)),
            _ => self.unsupported = true,
        }
    }

    /// Blanks TypeScript-only modifier keywords between `lo` and `hi`.
    fn blank_modifiers(&mut self, lo: BytePos, hi: BytePos) {
        let (lo, hi) = (self.off(lo), self.off(hi));
        let mut pos = lo;
        while pos < hi {
            let start = pos;
            while pos < hi && self.src.get(pos).is_some_and(|b| b.is_ascii_alphabetic()) {
                pos += 1;
            }
            if pos == start {
                pos += 1;
                continue;
            }
            let word = self.src.get(start..pos).unwrap_or_default();
            if TS_MEMBER_MODIFIERS.iter().any(|m| m.as_bytes() == word) {
                self.ranges.push((start, pos));
            }
        }
    }

    /// Blanks an `abstract` keyword that starts at, or directly precedes, `lo`.
    fn blank_abstract_keyword(&mut self, lo: BytePos) {
        const WORD: &[u8] = b"abstract";
        let lo = self.off(lo);
        if self.src.get(lo..lo + WORD.len()) == Some(WORD) {
            self.ranges.push((lo, lo + WORD.len()));
            return;
        }
        let mut end = lo;
        while end > 0
            && self
                .src
                .get(end - 1)
                .is_some_and(|b| b.is_ascii_whitespace())
        {
            end -= 1;
        }
        match end.checked_sub(WORD.len()) {
            Some(start) if self.src.get(start..end) == Some(WORD) => {
                self.ranges.push((start, end));
            }
            _ => self.unsupported = true,
        }
    }

    /// Blanks one import or export specifier with the comma that separates it
    /// from its neighbours.
    fn blank_specifier(&mut self, span: Span) {
        let (lo, hi) = (self.off(span.lo), self.off(span.hi));
        self.ranges.push((lo, hi));
        let mut next = hi;
        while self.src.get(next).is_some_and(|b| b.is_ascii_whitespace()) {
            next += 1;
        }
        if self.src.get(next) == Some(&b',') {
            self.ranges.push((next, next + 1));
            return;
        }
        let mut prev = lo;
        while prev > 0
            && self
                .src
                .get(prev - 1)
                .is_some_and(|b| b.is_ascii_whitespace())
        {
            prev -= 1;
        }
        if prev > 0 && self.src.get(prev - 1) == Some(&b',') {
            self.ranges.push((prev - 1, prev));
        }
    }

    fn import(&mut self, import: &ImportDecl) {
        if import.type_only {
            return self.blank_item(import.span);
        }
        if import.specifiers.is_empty() {
            return;
        }
        let elided: Vec<Span> = import
            .specifiers
            .iter()
            .filter_map(|spec| {
                let (span, local, type_only) = match spec {
                    ImportSpecifier::Named(s) => (s.span, &s.local, s.is_type_only),
                    ImportSpecifier::Default(s) => (s.span, &s.local, false),
                    ImportSpecifier::Namespace(s) => (s.span, &s.local, false),
                };
                (type_only || !self.value_refs.contains(&local.sym)).then_some(span)
            })
            .collect();
        if elided.len() == import.specifiers.len() {
            return self.blank_item(import.span);
        }
        for span in elided {
            self.blank_specifier(span);
        }
    }

    fn named_export(&mut self, export: &NamedExport) {
        if export.type_only {
            return self.blank_item(export.span);
        }
        let elided: Vec<Span> = export
            .specifiers
            .iter()
            .filter_map(|spec| match spec {
                ExportSpecifier::Named(s) => {
                    let local_type = export.src.is_none()
                        && matches!(&s.orig, ModuleExportName::Ident(id) if self.type_names.contains(&id.sym));
                    (s.is_type_only || local_type).then_some(s.span)
                }
                _ => None,
            })
            .collect();
        if !elided.is_empty() && elided.len() == export.specifiers.len() {
            return self.blank_item(export.span);
        }
        for span in elided {
            self.blank_specifier(span);
        }
    }
}

/// True for declarations that produce no runtime code.
fn is_type_only_decl(decl: &Decl) -> bool {
    match decl {
        Decl::TsInterface(_) | Decl::TsTypeAlias(_) => true,
        Decl::TsModule(d) => d.declare,
        Decl::Var(d) => d.declare,
        Decl::Fn(d) => d.declare || d.function.body.is_none(),
        Decl::Class(d) => d.declare,
        _ => false,
    }
}

impl Visit for Blanker<'_> {
    fn visit_module_item(&mut self, item: &ModuleItem) {
        match item {
            ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => self.import(import),
            ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(export)) => self.named_export(export),
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export))
                if is_type_only_decl(&export.decl) =>
            {
                self.blank_item(export.span)
            }
            ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(export))
                if match &export.decl {
                    DefaultDecl::TsInterfaceDecl(_) => true,
                    DefaultDecl::Fn(f) => f.function.body.is_none(),
                    DefaultDecl::Class(_) => false,
                } =>
            {
                self.blank_item(export.span)
            }
            ModuleItem::ModuleDecl(ModuleDecl::ExportAll(export)) if export.type_only => {
                self.blank_item(export.span)
            }
            ModuleItem::ModuleDecl(
                ModuleDecl::TsImportEquals(_)
                | ModuleDecl::TsExportAssignment(_)
                | ModuleDecl::TsNamespaceExport(_),
            ) => self.unsupported = true,
            _ => item.visit_children_with(self),
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl(decl) if is_type_only_decl(decl) => self.blank_item(decl.span()),
            _ => stmt.visit_children_with(self),
        }
    }

    // Type subtrees are blanked whole and never descended into.
    fn visit_ts_type_ann(&mut self, ann: &TsTypeAnn) {
        self.blank(ann.span);
    }
    fn visit_ts_type_param_decl(&mut self, params: &TsTypeParamDecl) {
        self.blank(params.span);
    }
    fn visit_ts_type_param_instantiation(&mut self, args: &TsTypeParamInstantiation) {
        self.blank(args.span);
    }
    fn visit_ts_type(&mut self, _: &TsType) {}
    fn visit_ts_expr_with_type_args(&mut self, _: &TsExprWithTypeArgs) {}

    fn visit_ts_as_expr(&mut self, e: &TsAsExpr) {
        self.blank(Span::new(e.expr.span().hi, e.span.hi));
        e.expr.visit_with(self);
    }
    fn visit_ts_satisfies_expr(&mut self, e: &TsSatisfiesExpr) {
        self.blank(Span::new(e.expr.span().hi, e.span.hi));
        e.expr.visit_with(self);
    }
    fn visit_ts_const_assertion(&mut self, e: &TsConstAssertion) {
        self.blank(Span::new(e.expr.span().hi, e.span.hi));
        e.expr.visit_with(self);
    }
    fn visit_ts_non_null_expr(&mut self, e: &TsNonNullExpr) {
        self.blank(Span::new(e.span.hi - BytePos(1), e.span.hi));
        e.expr.visit_with(self);
    }
    fn visit_ts_type_assertion(&mut self, e: &TsTypeAssertion) {
        let prefix = Span::new(e.span.lo, e.expr.span().lo);
        let (lo, hi) = (self.off(prefix.lo), self.off(prefix.hi));
        // `return <T>\n x` would become `return` followed by a new statement.
        if self.src.get(lo..hi).is_some_and(|s| s.contains(&b'\n')) {
            self.unsupported = true;
        }
        self.blank(prefix);
        e.expr.visit_with(self);
    }
    fn visit_ts_instantiation(&mut self, e: &TsInstantiation) {
        self.blank(e.type_args.span);
        e.expr.visit_with(self);
    }

    fn visit_binding_ident(&mut self, b: &BindingIdent) {
        if b.id.optional {
            self.blank_marker_after(b.id.span.lo + BytePos(b.id.sym.len() as u32));
        }
        b.type_ann.visit_with(self);
    }

    fn visit_var_declarator(&mut self, d: &VarDeclarator) {
        if d.definite {
            match &d.name {
                Pat::Ident(b) => {
                    self.blank_marker_after(b.id.span.lo + BytePos(b.id.sym.len() as u32))
                }
                _ => self.unsupported = true,
            }
        }
        d.visit_children_with(self);
    }

    fn visit_function(&mut self, f: &Function) {
        if let Some(this) = &f.this_param {
            let end = f.params.first().map_or(this.span.hi, |p| p.span.lo);
            self.ranges.push((self.off(this.span.lo), self.off(end)));
        }
        f.params.visit_with(self);
        f.type_params.visit_with(self);
        f.return_type.visit_with(self);
        f.body.visit_with(self);
    }

    fn visit_class(&mut self, c: &Class) {
        if c.is_abstract {
            self.blank_abstract_keyword(c.span.lo);
        }
        if let (Some(first), Some(last)) = (c.implements.first(), c.implements.last()) {
            const WORD: &[u8] = b"implements";
            let lo = self.off(first.span.lo);
            let start = self
                .src
                .get(..lo)
                .and_then(|head| head.windows(WORD.len()).rposition(|w| w == WORD));
            match start {
                Some(start) => self.ranges.push((start, self.off(last.span.hi))),
                None => self.unsupported = true,
            }
        }
        c.super_class.visit_with(self);
        c.type_params.visit_with(self);
        c.super_type_params.visit_with(self);
        c.body.visit_with(self);
    }

    fn visit_class_member(&mut self, member: &ClassMember) {
        match member {
            ClassMember::TsIndexSignature(s) => self.blank_item(s.span),
            ClassMember::ClassProp(p) if p.declare || p.is_abstract => self.blank_item(p.span),
            ClassMember::Method(m) if m.is_abstract || m.function.body.is_none() => {
                self.blank_item(m.span)
            }
            ClassMember::PrivateMethod(m) if m.is_abstract || m.function.body.is_none() => {
                self.blank_item(m.span)
            }
            ClassMember::Constructor(c) if c.body.is_none() => self.blank_item(c.span),
            ClassMember::Constructor(c) => {
                if c.params
                    .iter()
                    .any(|p| matches!(p, ParamOrTsParamProp::TsParamProp(_)))
                {
                    self.unsupported = true;
                }
                self.blank_modifiers(c.span.lo, c.key.span().lo);
                c.params.visit_with(self);
                c.body.visit_with(self);
            }
            ClassMember::ClassProp(p) => {
                self.blank_modifiers(p.span.lo, p.key.span().lo);
                if p.is_optional || p.definite {
                    self.blank_marker_after(p.key.span().hi);
                }
                p.key.visit_with(self);
                p.type_ann.visit_with(self);
                p.value.visit_with(self);
            }
            ClassMember::PrivateProp(p) => {
                self.blank_modifiers(p.span.lo, p.key.span.lo);
                if p.is_optional || p.definite {
                    self.blank_marker_after(p.key.span.hi);
                }
                p.type_ann.visit_with(self);
                p.value.visit_with(self);
            }
            ClassMember::Method(m) => {
                self.blank_modifiers(m.span.lo, m.key.span().lo);
                if m.is_optional {
                    self.blank_marker_after(m.key.span().hi);
                }
                m.key.visit_with(self);
                m.function.visit_with(self);
            }
            ClassMember::PrivateMethod(m) => {
                self.blank_modifiers(m.span.lo, m.key.span.lo);
                if m.is_optional {
                    self.blank_marker_after(m.key.span.hi);
                }
                m.function.visit_with(self);
            }
            ClassMember::AutoAccessor(a) => {
                self.blank_modifiers(a.span.lo, a.key.span().lo);
                a.type_ann.visit_with(self);
                a.value.visit_with(self);
            }
            _ => member.visit_children_with(self),
        }
    }
}
