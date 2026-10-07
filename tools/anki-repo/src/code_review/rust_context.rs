//! Синтаксический контекст Rust для производной очереди ревью.
//!
//! Индекс получает полные тексты конкретного снимка, не читает файлы и не
//! разворачивает макросы. Корневое execution берётся только из однозначной
//! Production/Tests-поверхности; иначе оно остаётся `Unknown`.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use proc_macro2::Span;
use serde::{Deserialize, Serialize};
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use super::language::SourceFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustExecutionContext {
    Runtime,
    Test,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustCodeRole {
    Runtime,
    /// `unwrap`/`expect` над синтаксически известным внешним I/O вызовом.
    RuntimeBoundary,
    TestSetup,
    TestAssertion,
    TestHelper,
    Item,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustContextBasis {
    RustSyntax,
    AmbiguousLocation,
    ParseFailure,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustCallKind {
    Call,
    Method,
    Macro,
    Attribute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustCallContext {
    pub kind: RustCallKind,
    /// Синтаксический путь без type arguments и без попытки разрешить imports.
    pub path: String,
    pub argument_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustContext {
    pub execution: RustExecutionContext,
    pub code_role: RustCodeRole,
    pub basis: RustContextBasis,
    /// Навигационная подпись; не является ключом группировки разных тестов.
    pub enclosing_item: Option<String>,
    pub call_context: Option<RustCallContext>,
}

impl RustContext {
    /// Структурный ключ вызова без имени функции, содержащей candidate.
    #[must_use]
    pub fn call_signature(&self) -> Option<String> {
        let call = self.call_context.as_ref()?;
        let kind = match call.kind {
            RustCallKind::Call => "call",
            RustCallKind::Method => "method",
            RustCallKind::Macro => "macro",
            RustCallKind::Attribute => "attribute",
        };
        Some(match call.argument_index {
            Some(index) => format!("{kind}:{}:argument:{index}", call.path),
            None => format!("{kind}:{}", call.path),
        })
    }

    fn unknown(basis: RustContextBasis) -> Self {
        Self {
            execution: RustExecutionContext::Unknown,
            code_role: RustCodeRole::Unknown,
            basis,
            enclosing_item: None,
            call_context: None,
        }
    }

    fn root(execution: RustExecutionContext) -> Self {
        Self {
            execution,
            code_role: RustCodeRole::Item,
            basis: RustContextBasis::RustSyntax,
            enclosing_item: None,
            call_context: None,
        }
    }
}

#[derive(Debug)]
struct Region {
    range: Range<usize>,
    depth: usize,
    context: RustContext,
}

#[derive(Debug)]
struct Segment {
    range: Range<usize>,
    context: RustContext,
}

#[derive(Debug)]
struct FileContext {
    source: String,
    line_starts: Vec<usize>,
    segments: Vec<Segment>,
    failure: Option<RustContextBasis>,
}

/// Отдельный индекс нужен для каждого Git image: base и post нельзя смешивать.
#[derive(Debug, Default)]
pub struct RustContextIndex {
    files: BTreeMap<String, FileContext>,
}

impl RustContextIndex {
    #[must_use]
    pub fn from_sources(sources: &[SourceFile]) -> Self {
        let mut files = BTreeMap::new();
        for source in sources.iter().filter(|file| file.path.ends_with(".rs")) {
            if let Some(previous) = files.get_mut(&source.path) {
                // Два разных image одного пути не дают однозначного контекста.
                let previous: &mut FileContext = previous;
                if previous.source != source.content {
                    previous.failure = Some(RustContextBasis::Unavailable);
                }
                continue;
            }
            let (_, surfaces) = super::scope::classify_path(&source.path);
            let execution = match (
                surfaces.contains(&super::scope::FileSurface::Production),
                surfaces.contains(&super::scope::FileSurface::Tests),
            ) {
                (true, false) => RustExecutionContext::Runtime,
                (false, true) => RustExecutionContext::Test,
                _ => RustExecutionContext::Unknown,
            };
            files.insert(
                source.path.clone(),
                FileContext::parse(&source.content, execution),
            );
        }
        Self { files }
    }

    /// Строка и Unicode-колонка с единицы, как в language evidence.
    /// Без колонки неоднозначные роли разных выражений остаются видимыми.
    #[must_use]
    pub fn lookup(&self, path: &str, line: usize, column: Option<usize>) -> RustContext {
        let Some(file) = self.files.get(path) else {
            return RustContext::unknown(RustContextBasis::Unavailable);
        };
        if let Some(basis) = file.failure {
            return RustContext::unknown(basis);
        }
        let Some(&start) = line
            .checked_sub(1)
            .and_then(|index| file.line_starts.get(index))
        else {
            return RustContext::unknown(RustContextBasis::Unavailable);
        };
        let end = file
            .line_starts
            .get(line)
            .copied()
            .unwrap_or(file.source.len());
        if let Some(column) = column {
            let Some(index) = column.checked_sub(1) else {
                return RustContext::unknown(RustContextBasis::Unavailable);
            };
            let Some((offset, character)) = file.source[start..end].char_indices().nth(index)
            else {
                return RustContext::unknown(RustContextBasis::Unavailable);
            };
            return file.lookup_range(start + offset, start + offset + character.len_utf8());
        }
        // Пробелы вокруг выражения не создают искусственную неоднозначность.
        let text = &file.source[start..end];
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return RustContext::unknown(RustContextBasis::Unavailable);
        }
        let leading = text.len() - text.trim_start().len();
        file.lookup_range(start + leading, start + leading + trimmed.len())
    }

    /// Полуоткрытый диапазон исходных UTF-8 байтов language candidate.
    #[must_use]
    pub fn lookup_range(&self, path: &str, start: usize, end: usize) -> RustContext {
        self.files.get(path).map_or_else(
            || RustContext::unknown(RustContextBasis::Unavailable),
            |file| file.lookup_range(start, end),
        )
    }
}

/// Сохраняет смещения байтов исходника: `syn::parse_file` удаляет BOM и shebang перед разбором.
fn parse_rust_file_with_aligned_spans(source: &str) -> syn::Result<syn::File> {
    const BOM: &str = "\u{feff}";

    let has_bom = source.starts_with(BOM);
    let parsed = syn::parse_file(source)?;
    if !has_bom && parsed.shebang.is_none() {
        return Ok(parsed);
    }

    let mut masked = source.to_owned();
    if has_bom {
        masked.replace_range(..BOM.len(), &" ".repeat(BOM.len()));
    }
    if parsed.shebang.is_some() {
        let shebang_start = if has_bom { BOM.len() } else { 0 };
        let shebang_end = source[shebang_start..]
            .find('\n')
            .map_or(source.len(), |offset| shebang_start + offset);
        masked.replace_range(
            shebang_start..shebang_end,
            &" ".repeat(shebang_end - shebang_start),
        );
    }
    syn::parse_str(&masked)
}

impl FileContext {
    fn parse(source: &str, execution: RustExecutionContext) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(source.match_indices('\n').map(|(offset, _)| offset + 1));
        let mut file = Self {
            source: source.to_owned(),
            line_starts,
            segments: Vec::new(),
            failure: None,
        };
        let Ok(syntax) = parse_rust_file_with_aligned_spans(source) else {
            file.failure = Some(RustContextBasis::ParseFailure);
            return file;
        };
        let mut visitor = ContextVisitor {
            regions: Vec::new(),
            context: RustContext::root(execution),
            depth: 0,
        };
        apply_test_attributes(&mut visitor.context, &syntax.attrs);
        visitor.regions.push(Region {
            range: 0..source.len(),
            depth: 0,
            context: visitor.context.clone(),
        });
        visitor.visit_file(&syntax);
        file.segments = segments(visitor.regions, source.len());
        file
    }

    fn lookup_range(&self, start: usize, end: usize) -> RustContext {
        if let Some(basis) = self.failure {
            return RustContext::unknown(basis);
        }
        if start >= end
            || end > self.source.len()
            || !self.source.is_char_boundary(start)
            || !self.source.is_char_boundary(end)
        {
            return RustContext::unknown(RustContextBasis::Unavailable);
        }
        let index = self
            .segments
            .partition_point(|segment| segment.range.end <= start);
        let mut contexts = self.segments[index..]
            .iter()
            .take_while(|segment| segment.range.start < end)
            .map(|segment| &segment.context);
        let Some(first) = contexts.next() else {
            return RustContext::unknown(RustContextBasis::Unavailable);
        };
        let mut context = first.clone();
        let mut call_conflict = false;
        for next in contexts {
            if context.execution != next.execution {
                context.execution = RustExecutionContext::Unknown;
                context.basis = RustContextBasis::AmbiguousLocation;
            }
            if context.code_role != next.code_role {
                context.code_role = RustCodeRole::Unknown;
                context.basis = RustContextBasis::AmbiguousLocation;
            }
            if context.enclosing_item != next.enclosing_item {
                context.enclosing_item = None;
            }
            if let Some(call) = &next.call_context {
                if context
                    .call_context
                    .as_ref()
                    .is_some_and(|previous| previous != call)
                {
                    call_conflict = true;
                } else if context.call_context.is_none() {
                    context.call_context = Some(call.clone());
                }
            }
            if context.basis != next.basis {
                context.basis = RustContextBasis::AmbiguousLocation;
            }
        }
        if call_conflict || context.execution == RustExecutionContext::Unknown {
            context.call_context = None;
        }
        context
    }
}

/// Sweep по границам AST: построение O(n log n), lookup O(log n + пересечения).
fn segments(regions: Vec<Region>, length: usize) -> Vec<Segment> {
    let mut events = BTreeMap::<usize, (Vec<usize>, Vec<usize>)>::new();
    for (index, region) in regions.iter().enumerate() {
        if region.range.start < region.range.end && region.range.end <= length {
            events.entry(region.range.start).or_default().0.push(index);
            events.entry(region.range.end).or_default().1.push(index);
        }
    }
    let key = |index: usize| (regions[index].depth, index);
    let mut active: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut previous = 0;
    let mut result: Vec<Segment> = Vec::new();
    for (position, (starts, ends)) in events {
        if previous < position
            && let Some(&(_, index)) = active.last()
        {
            let context = &regions[index].context;
            if let Some(last) = result.last_mut().filter(|last| last.context == *context) {
                last.range.end = position;
            } else {
                result.push(Segment {
                    range: previous..position,
                    context: context.clone(),
                });
            }
        }
        for index in ends {
            active.remove(&key(index));
        }
        for index in starts {
            active.insert(key(index));
        }
        previous = position;
    }
    result
}

struct ContextVisitor {
    regions: Vec<Region>,
    context: RustContext,
    depth: usize,
}

impl ContextVisitor {
    fn within(&mut self, span: Span, context: RustContext, visit: impl FnOnce(&mut Self)) {
        let previous = std::mem::replace(&mut self.context, context);
        self.depth += 1;
        self.regions.push(Region {
            range: span.byte_range(),
            depth: self.depth,
            context: self.context.clone(),
        });
        visit(self);
        self.depth -= 1;
        self.context = previous;
    }

    fn function(
        &mut self,
        span: Span,
        name: &str,
        attrs: &[syn::Attribute],
        visit: impl FnOnce(&mut Self),
    ) {
        let mut context = self.context.clone();
        apply_test_attributes(&mut context, attrs);
        context.enclosing_item = Some(name.to_owned());
        context.call_context = None;
        context.code_role = match context.execution {
            RustExecutionContext::Test if attrs.iter().any(is_test_attribute) => {
                RustCodeRole::TestSetup
            }
            RustExecutionContext::Test => RustCodeRole::TestHelper,
            RustExecutionContext::Runtime => RustCodeRole::Runtime,
            RustExecutionContext::Unknown => RustCodeRole::Unknown,
        };
        self.within(span, context, visit);
    }

    fn call(
        &mut self,
        span: Span,
        kind: RustCallKind,
        path: String,
        visit: impl FnOnce(&mut Self),
    ) {
        let mut context = self.context.clone();
        context.call_context = Some(RustCallContext {
            kind,
            path,
            argument_index: None,
        });
        self.within(span, context, visit);
    }

    fn argument(&mut self, index: usize, expression: &syn::Expr) {
        let mut context = self.context.clone();
        if let Some(call) = &mut context.call_context {
            call.argument_index = Some(index);
        }
        self.within(expression.span(), context, |this| {
            this.visit_expr(expression)
        });
    }
}

impl<'ast> Visit<'ast> for ContextVisitor {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs = item_attributes(item);
        let mut context = self.context.clone();
        apply_test_attributes(&mut context, attrs);
        context.call_context = None;
        if matches!(item, syn::Item::Verbatim(_)) {
            context.code_role = RustCodeRole::Unknown;
            context.basis = RustContextBasis::Unavailable;
        }
        self.within(item.span(), context, |this| visit::visit_item(this, item));
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.function(
            item.span(),
            &item.sig.ident.to_string(),
            &item.attrs,
            |this| visit::visit_item_fn(this, item),
        );
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.function(
            item.span(),
            &item.sig.ident.to_string(),
            &item.attrs,
            |this| visit::visit_impl_item_fn(this, item),
        );
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.function(
            item.span(),
            &item.sig.ident.to_string(),
            &item.attrs,
            |this| visit::visit_trait_item_fn(this, item),
        );
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        let mut context = self.context.clone();
        apply_test_attributes(&mut context, &local.attrs);
        self.within(local.span(), context, |this| {
            visit::visit_local(this, local)
        });
    }

    fn visit_expr(&mut self, expression: &'ast syn::Expr) {
        let attrs = expression_attributes(expression);
        if matches!(expression, syn::Expr::Verbatim(_)) {
            let mut context = self.context.clone();
            context.code_role = RustCodeRole::Unknown;
            context.basis = RustContextBasis::Unavailable;
            self.within(expression.span(), context, |_| {});
        } else if attrs.iter().any(|attr| attr.path().is_ident("cfg")) {
            let mut context = self.context.clone();
            apply_test_attributes(&mut context, attrs);
            self.within(expression.span(), context, |this| {
                visit::visit_expr(this, expression)
            });
        } else {
            visit::visit_expr(self, expression);
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let path = match call.func.as_ref() {
            syn::Expr::Path(path) => path_string(&path.path),
            _ => "<expression>".to_owned(),
        };
        self.call(call.span(), RustCallKind::Call, path, |this| {
            for attr in &call.attrs {
                this.visit_attribute(attr);
            }
            this.visit_expr(&call.func);
            for (index, argument) in call.args.iter().enumerate() {
                this.argument(index, argument);
            }
        });
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let previous_role = self.context.code_role;
        let boundary = self.context.execution == RustExecutionContext::Runtime
            && self.context.code_role == RustCodeRole::Runtime
            && matches!(call.method.to_string().as_str(), "unwrap" | "expect")
            && is_boundary_result(&call.receiver);
        if boundary {
            self.context.code_role = RustCodeRole::RuntimeBoundary;
        }
        self.call(
            call.span(),
            RustCallKind::Method,
            call.method.to_string(),
            |this| {
                for attr in &call.attrs {
                    this.visit_attribute(attr);
                }
                let mut receiver_context = this.context.clone();
                receiver_context.code_role = previous_role;
                this.within(call.receiver.span(), receiver_context, |this| {
                    this.visit_expr(&call.receiver);
                });
                this.context.code_role = previous_role;
                for (index, argument) in call.args.iter().enumerate() {
                    this.argument(index, argument);
                }
            },
        );
        self.context.code_role = previous_role;
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let path = path_string(&mac.path);
        let name = mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_default();
        let assertion = matches!(
            name.as_str(),
            "assert"
                | "assert_eq"
                | "assert_ne"
                | "debug_assert"
                | "debug_assert_eq"
                | "debug_assert_ne"
        );
        let known_expression_macro = assertion
            || matches!(
                name.as_str(),
                "format"
                    | "format_args"
                    | "println"
                    | "print"
                    | "eprintln"
                    | "eprint"
                    | "write"
                    | "writeln"
                    | "panic"
                    | "vec"
                    | "concat"
                    | "env"
                    | "option_env"
            );
        let mut context = self.context.clone();
        context.call_context = Some(RustCallContext {
            kind: RustCallKind::Macro,
            path,
            argument_index: None,
        });
        if assertion && context.execution == RustExecutionContext::Test {
            context.code_role = RustCodeRole::TestAssertion;
        } else if !known_expression_macro {
            context.code_role = RustCodeRole::Unknown;
        }
        self.within(mac.span(), context, |this| {
            if known_expression_macro {
                let parser =
                    syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
                if let Ok(arguments) = parser.parse2(mac.tokens.clone()) {
                    for (index, argument) in arguments.iter().enumerate() {
                        this.argument(index, argument);
                    }
                }
            }
        });
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        self.call(
            attr.span(),
            RustCallKind::Attribute,
            path_string(attr.path()),
            |_| {},
        );
    }
}

fn path_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// Только доступная AST-форма: переменные с неизвестным типом и произвольные
/// функции не получают I/O роль по одному имени `send`, `open` или `output`.
/// Сокращённые имена типов также не доказывают принадлежность внешнему API.
/// Разрешение imports и фактическая семантика API остаются задачей reviewer.
fn is_boundary_result(expression: &syn::Expr) -> bool {
    match expression {
        syn::Expr::Paren(expr) => is_boundary_result(&expr.expr),
        syn::Expr::Group(expr) => is_boundary_result(&expr.expr),
        syn::Expr::Await(expr) => is_boundary_result(&expr.base),
        syn::Expr::Call(call) => {
            let syn::Expr::Path(path) = call.func.as_ref() else {
                return false;
            };
            matches!(
                path_string(&path.path).as_str(),
                "std::fs::read"
                    | "std::fs::read_to_string"
                    | "std::fs::write"
                    | "std::fs::canonicalize"
                    | "std::fs::read_dir"
                    | "std::fs::metadata"
                    | "std::fs::create_dir"
                    | "std::fs::create_dir_all"
                    | "std::fs::remove_file"
                    | "std::fs::remove_dir"
                    | "std::fs::remove_dir_all"
                    | "std::fs::copy"
                    | "std::fs::rename"
                    | "std::fs::File::open"
                    | "std::fs::File::create"
                    | "std::net::TcpStream::connect"
                    | "std::net::TcpListener::bind"
                    | "std::net::UdpSocket::bind"
            )
        }
        syn::Expr::MethodCall(call) => match call.method.to_string().as_str() {
            "output" | "status" | "spawn" => constructed_chain(
                &call.receiver,
                &["std::process::Command::new", "tokio::process::Command::new"],
                &[
                    "arg",
                    "args",
                    "env",
                    "envs",
                    "env_remove",
                    "env_clear",
                    "current_dir",
                    "stdin",
                    "stdout",
                    "stderr",
                    "kill_on_drop",
                ],
                &[],
                false,
            ),
            "send" => constructed_chain(
                &call.receiver,
                &["reqwest::Client::new", "reqwest::blocking::Client::new"],
                &[
                    "get",
                    "post",
                    "put",
                    "patch",
                    "delete",
                    "head",
                    "request",
                    "header",
                    "headers",
                    "body",
                    "json",
                    "query",
                    "timeout",
                    "basic_auth",
                    "bearer_auth",
                ],
                &["get", "post", "put", "patch", "delete", "head", "request"],
                false,
            ),
            _ => false,
        },
        _ => false,
    }
}

fn constructed_chain(
    expression: &syn::Expr,
    constructors: &[&str],
    methods: &[&str],
    required_methods: &[&str],
    has_required_method: bool,
) -> bool {
    match expression {
        syn::Expr::Paren(expr) => constructed_chain(
            &expr.expr,
            constructors,
            methods,
            required_methods,
            has_required_method,
        ),
        syn::Expr::Group(expr) => constructed_chain(
            &expr.expr,
            constructors,
            methods,
            required_methods,
            has_required_method,
        ),
        syn::Expr::Call(call) => match call.func.as_ref() {
            syn::Expr::Path(path) => {
                constructors.contains(&path_string(&path.path).as_str())
                    && (required_methods.is_empty() || has_required_method)
            }
            _ => false,
        },
        syn::Expr::MethodCall(call) => {
            let name = call.method.to_string();
            methods.contains(&name.as_str())
                && constructed_chain(
                    &call.receiver,
                    constructors,
                    methods,
                    required_methods,
                    has_required_method || required_methods.contains(&name.as_str()),
                )
        }
        _ => false,
    }
}

/// Пути известных атрибутов тестовых функций. Суффикс `test` сам по себе
/// не доказывает семантику произвольного процедурного макроса.
fn is_test_attribute(attr: &syn::Attribute) -> bool {
    matches!(
        path_string(attr.path()).as_str(),
        "test" | "tokio::test" | "async_std::test"
    )
}

fn apply_test_attributes(context: &mut RustContext, attrs: &[syn::Attribute]) {
    let proven_test = attrs.iter().any(|attr| {
        is_test_attribute(attr)
            || (attr.path().is_ident("cfg")
                && attr
                    .parse_args::<syn::Meta>()
                    .is_ok_and(|meta| requires_test(&meta)))
    });
    if proven_test {
        context.execution = RustExecutionContext::Test;
        if matches!(
            context.code_role,
            RustCodeRole::Runtime | RustCodeRole::RuntimeBoundary
        ) {
            context.code_role = RustCodeRole::TestSetup;
        }
    } else if context.execution != RustExecutionContext::Test
        && attrs.iter().any(|attr| {
            attr.path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "test")
        })
    {
        context.execution = RustExecutionContext::Unknown;
        context.code_role = RustCodeRole::Unknown;
    }
}

/// `all(test, ...)` доказывает test; `any(test, feature)` этого не доказывает.
fn requires_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => {
            let Ok(children) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            ) else {
                return false;
            };
            if list.path.is_ident("all") {
                children.iter().any(requires_test)
            } else if list.path.is_ident("any") {
                !children.is_empty() && children.iter().all(requires_test)
            } else {
                false
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

fn item_attributes(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

fn expression_attributes(expression: &syn::Expr) -> &[syn::Attribute] {
    match expression {
        syn::Expr::Array(expr) => &expr.attrs,
        syn::Expr::Assign(expr) => &expr.attrs,
        syn::Expr::Async(expr) => &expr.attrs,
        syn::Expr::Await(expr) => &expr.attrs,
        syn::Expr::Binary(expr) => &expr.attrs,
        syn::Expr::Block(expr) => &expr.attrs,
        syn::Expr::Break(expr) => &expr.attrs,
        syn::Expr::Call(expr) => &expr.attrs,
        syn::Expr::Cast(expr) => &expr.attrs,
        syn::Expr::Closure(expr) => &expr.attrs,
        syn::Expr::Const(expr) => &expr.attrs,
        syn::Expr::Continue(expr) => &expr.attrs,
        syn::Expr::Field(expr) => &expr.attrs,
        syn::Expr::ForLoop(expr) => &expr.attrs,
        syn::Expr::Group(expr) => &expr.attrs,
        syn::Expr::If(expr) => &expr.attrs,
        syn::Expr::Index(expr) => &expr.attrs,
        syn::Expr::Infer(expr) => &expr.attrs,
        syn::Expr::Let(expr) => &expr.attrs,
        syn::Expr::Lit(expr) => &expr.attrs,
        syn::Expr::Loop(expr) => &expr.attrs,
        syn::Expr::Macro(expr) => &expr.attrs,
        syn::Expr::Match(expr) => &expr.attrs,
        syn::Expr::MethodCall(expr) => &expr.attrs,
        syn::Expr::Paren(expr) => &expr.attrs,
        syn::Expr::Path(expr) => &expr.attrs,
        syn::Expr::Range(expr) => &expr.attrs,
        syn::Expr::RawAddr(expr) => &expr.attrs,
        syn::Expr::Reference(expr) => &expr.attrs,
        syn::Expr::Repeat(expr) => &expr.attrs,
        syn::Expr::Return(expr) => &expr.attrs,
        syn::Expr::Struct(expr) => &expr.attrs,
        syn::Expr::Try(expr) => &expr.attrs,
        syn::Expr::TryBlock(expr) => &expr.attrs,
        syn::Expr::Tuple(expr) => &expr.attrs,
        syn::Expr::Unary(expr) => &expr.attrs,
        syn::Expr::Unsafe(expr) => &expr.attrs,
        syn::Expr::While(expr) => &expr.attrs,
        syn::Expr::Yield(expr) => &expr.attrs,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(source: &str) -> RustContextIndex {
        RustContextIndex::from_sources(&[SourceFile {
            path: "src/example.rs".to_owned(),
            content: source.to_owned(),
        }])
    }

    fn at_text(index: &RustContextIndex, source: &str, text: &str) -> RustContext {
        let start = source.find(text).expect("вхождение fixture существует");
        index.lookup_range("src/example.rs", start, start + text.len())
    }

    #[test]
    fn inline_test_module_and_function_preserve_execution_and_roles() {
        let source = r#"
fn runtime() { open().unwrap(); }
#[cfg(test)] mod checks {
    fn helper() { prepare().unwrap(); }
    #[test] fn exercises() {
        let ready = setup().unwrap();
        assert_eq!(ready, expected(), "English diagnostic");
        opaque! { arbitrary syntax => tokens }
    }
}
"#;
        let index = index(source);
        let runtime = at_text(&index, source, "open()");
        assert_eq!(runtime.execution, RustExecutionContext::Runtime);
        assert_eq!(runtime.code_role, RustCodeRole::Runtime);
        assert_eq!(runtime.call_signature().as_deref(), Some("call:open"));
        let helper = at_text(&index, source, "prepare()");
        assert_eq!(helper.execution, RustExecutionContext::Test);
        assert_eq!(helper.code_role, RustCodeRole::TestHelper);
        let setup = at_text(&index, source, "setup()");
        assert_eq!(setup.code_role, RustCodeRole::TestSetup);
        let assertion = at_text(&index, source, "English diagnostic");
        assert_eq!(assertion.execution, RustExecutionContext::Test);
        assert_eq!(assertion.code_role, RustCodeRole::TestAssertion);
        assert_eq!(
            assertion.call_signature().as_deref(),
            Some("macro:assert_eq:argument:2")
        );
        let opaque = at_text(&index, source, "arbitrary syntax");
        assert_eq!(opaque.execution, RustExecutionContext::Test);
        assert_eq!(opaque.code_role, RustCodeRole::Unknown);
        assert_eq!(opaque.call_signature().as_deref(), Some("macro:opaque"));
    }

    #[test]
    fn method_argument_and_unicode_column_use_exact_source_positions() {
        let source = "fn runtime() { sink.report(\"Ошибка message\"); }";
        let index = index(source);
        let text = at_text(&index, source, "Ошибка message");
        assert_eq!(text.execution, RustExecutionContext::Runtime);
        assert_eq!(
            text.call_signature().as_deref(),
            Some("method:report:argument:0")
        );
        let prefix = source.split("Ошибка").next().expect("prefix");
        let column = prefix.chars().count() + 1;
        assert_eq!(index.lookup("src/example.rs", 1, Some(column)), text);
    }

    #[test]
    fn same_line_with_different_execution_is_ambiguous() {
        let source = "fn run() {} #[test] fn check() {}";
        let context = index(source).lookup("src/example.rs", 1, None);
        assert_eq!(context.execution, RustExecutionContext::Unknown);
        assert_eq!(context.code_role, RustCodeRole::Unknown);
        assert_eq!(context.basis, RustContextBasis::AmbiguousLocation);
    }

    #[test]
    fn line_lookup_keeps_one_call_but_does_not_choose_between_calls() {
        let source =
            "#[test] fn check() {\n    let item = value.unwrap();\n    setup().unwrap();\n}";
        let index = index(source);
        let single = index.lookup("src/example.rs", 2, None);
        assert_eq!(single.execution, RustExecutionContext::Test);
        assert_eq!(single.code_role, RustCodeRole::TestSetup);
        assert_eq!(single.call_signature().as_deref(), Some("method:unwrap"));
        let multiple = index.lookup("src/example.rs", 3, None);
        assert_eq!(multiple.execution, RustExecutionContext::Test);
        assert_eq!(multiple.code_role, RustCodeRole::TestSetup);
        assert_eq!(multiple.call_signature(), None);
        let exact = at_text(&index, source, "unwrap");
        assert_eq!(exact.call_signature().as_deref(), Some("method:unwrap"));
    }

    #[test]
    fn parser_failure_keeps_arbitrary_candidate_locations_addressable() {
        let index = index("#[cfg(test)] mod checks { fn broken( {");
        for context in [
            index.lookup("src/example.rs", 1, None),
            index.lookup_range("src/example.rs", 3, 12),
        ] {
            assert_eq!(context.execution, RustExecutionContext::Unknown);
            assert_eq!(context.code_role, RustCodeRole::Unknown);
            assert_eq!(context.basis, RustContextBasis::ParseFailure);
        }
    }

    #[test]
    fn unsupported_source_and_conflicting_images_are_unavailable() {
        let index = RustContextIndex::from_sources(&[
            SourceFile {
                path: "README.md".to_owned(),
                content: "Human prose".to_owned(),
            },
            SourceFile {
                path: "src/file.rs".to_owned(),
                content: "fn first() {}".to_owned(),
            },
            SourceFile {
                path: "src/file.rs".to_owned(),
                content: "fn second() {}".to_owned(),
            },
        ]);
        for path in ["README.md", "src/missing.rs", "src/file.rs"] {
            let context = index.lookup(path, 1, None);
            assert_eq!(context.basis, RustContextBasis::Unavailable);
            assert_eq!(context.execution, RustExecutionContext::Unknown);
        }
    }

    #[test]
    fn cfg_predicate_must_require_test_on_all_possible_branches() {
        let source = r#"
#[cfg(all(test, feature = "fixtures"))] fn helper() { setup(); }
#[cfg(any(test, feature = "runtime"))] fn shared() { serve(); }
"#;
        let index = index(source);
        assert_eq!(
            at_text(&index, source, "setup()").execution,
            RustExecutionContext::Test
        );
        assert_eq!(
            at_text(&index, source, "serve()").execution,
            RustExecutionContext::Runtime
        );
    }

    #[test]
    fn recognized_test_attributes_mark_setup_and_assertions_in_production_files() {
        for attribute in ["test", "tokio::test", "async_std::test"] {
            let source = format!(
                "#[{attribute}] async fn check() {{\n    std::fs::read(path).unwrap();\n    assert!(std::fs::read(path).expect(\"fixture\").is_empty());\n}}"
            );
            let index = index(&source);
            let setup = at_text(&index, &source, "unwrap");
            assert_eq!(setup.execution, RustExecutionContext::Test, "{attribute}");
            assert_eq!(setup.code_role, RustCodeRole::TestSetup, "{attribute}");
            let assertion = at_text(&index, &source, "expect");
            assert_eq!(
                assertion.execution,
                RustExecutionContext::Test,
                "{attribute}"
            );
            assert_eq!(
                assertion.code_role,
                RustCodeRole::TestAssertion,
                "{attribute}"
            );
        }
    }

    #[test]
    fn unknown_test_attribute_does_not_claim_test_or_production_execution() {
        let source = r#"
#[foo::test] fn uncertain() { std::fs::read(path).unwrap(); }
#[tokio::main] async fn main() { std::fs::write(path, data).expect("diagnostic"); }
#[cfg(test)] mod checks {
    #[foo::test] fn scoped() { fixture().unwrap(); }
}
"#;
        let index = index(source);
        let uncertain = at_text(&index, source, "std::fs::read(path).unwrap");
        assert_eq!(uncertain.execution, RustExecutionContext::Unknown);
        assert_eq!(uncertain.code_role, RustCodeRole::Unknown);
        let runtime = at_text(&index, source, "expect");
        assert_eq!(runtime.execution, RustExecutionContext::Runtime);
        assert_eq!(runtime.code_role, RustCodeRole::RuntimeBoundary);
        let scoped = at_text(&index, source, "fixture()");
        assert_eq!(scoped.execution, RustExecutionContext::Test);
        assert_eq!(scoped.code_role, RustCodeRole::TestHelper);
    }

    #[test]
    fn local_types_with_io_method_names_do_not_prove_a_boundary() {
        let source = r#"
struct File;
impl File { fn open(_: &str) -> Result<Self, ()> { Ok(Self) } }
fn custom() { File::open("fixture").unwrap(); }
fn standard() { std::fs::File::open("fixture").expect("diagnostic"); }
"#;
        let index = index(source);
        let custom = at_text(&index, source, "unwrap");
        assert_eq!(custom.execution, RustExecutionContext::Runtime);
        assert_eq!(custom.code_role, RustCodeRole::Runtime);
        let standard = at_text(&index, source, "expect");
        assert_eq!(standard.execution, RustExecutionContext::Runtime);
        assert_eq!(standard.code_role, RustCodeRole::RuntimeBoundary);
    }

    #[test]
    fn nested_modules_and_methods_preserve_test_attribute_and_cfg_proofs() {
        let source = r#"
#[cfg(all(feature = "fixtures", any(test, all(test, feature = "extra"))))]
mod outer {
    mod inner {
        struct Harness;
        impl Harness {
            #[tokio::test] async fn method() { prepare().unwrap(); }
            fn helper() { helper_value().unwrap(); }
        }
        trait Checks {
            #[async_std::test] async fn method() { verify().expect("fixture"); }
        }
    }
}
#[cfg(any(test, feature = "runtime"))] mod shared {
    fn runtime() { std::fs::read(path).unwrap(); }
}
"#;
        let index = index(source);
        for text in ["prepare()", "verify()"] {
            let context = at_text(&index, source, text);
            assert_eq!(context.execution, RustExecutionContext::Test, "{text}");
            assert_eq!(context.code_role, RustCodeRole::TestSetup, "{text}");
        }
        let helper = at_text(&index, source, "helper_value()");
        assert_eq!(helper.execution, RustExecutionContext::Test);
        assert_eq!(helper.code_role, RustCodeRole::TestHelper);
        let shared = at_text(&index, source, "std::fs::read(path)");
        assert_eq!(shared.execution, RustExecutionContext::Runtime);
        assert_eq!(shared.code_role, RustCodeRole::Runtime);
    }

    #[test]
    fn runtime_unwrap_and_expect_recognize_syntactic_io_boundaries() {
        for expression in [
            "std::fs::read(path).unwrap()",
            "std::fs::File::open(path).unwrap()",
            "std::fs::File::create(path).unwrap()",
            "std::process::Command::new(cmd).arg(flag).output().unwrap()",
            "std::process::Command::new(cmd).current_dir(dir).status().unwrap()",
            "std::net::TcpStream::connect(address).unwrap()",
            "std::net::UdpSocket::bind(address).unwrap()",
            "reqwest::blocking::Client::new().get(url).header(key, value).send().unwrap()",
            "reqwest::Client::new().post(url).send().await.unwrap()",
            "std::fs::write(path, data).expect(\"diagnostic\")",
        ] {
            let source = format!("async fn runtime() {{ {expression}; }}");
            let method = if expression.contains(".expect(") {
                "expect"
            } else {
                "unwrap"
            };
            let context = at_text(&index(&source), &source, method);
            assert_eq!(
                context.execution,
                RustExecutionContext::Runtime,
                "{expression}"
            );
            assert_eq!(
                context.code_role,
                RustCodeRole::RuntimeBoundary,
                "{expression}"
            );
            assert_eq!(context.basis, RustContextBasis::RustSyntax);
        }
    }

    #[test]
    fn bom_and_shebang_preserve_ast_source_offsets() {
        for prefix in [
            "\u{feff}",
            "#!/usr/bin/env rustx\n",
            "\u{feff}#!/usr/bin/env rustx\n",
        ] {
            let source = format!("{prefix}fn run() {{ std::fs::read(path).unwrap(); }}");
            let context = at_text(&index(&source), &source, "unwrap");

            assert_eq!(
                context.execution,
                RustExecutionContext::Runtime,
                "{prefix:?}"
            );
            assert_eq!(
                context.code_role,
                RustCodeRole::RuntimeBoundary,
                "{prefix:?}"
            );
            assert_eq!(context.basis, RustContextBasis::RustSyntax, "{prefix:?}");
        }

        let source = "\u{feff}#![cfg(test)]\n#[test] fn check() { setup().unwrap(); }";
        let context = at_text(&index(source), source, "unwrap");
        assert_eq!(context.execution, RustExecutionContext::Test);
        assert_eq!(context.code_role, RustCodeRole::TestSetup);
        assert_eq!(context.basis, RustContextBasis::RustSyntax);
    }

    #[test]
    fn unknown_receiver_and_nested_generic_unwrap_keep_runtime_role() {
        for expression in [
            "some_result.unwrap()",
            "custom::open(path).unwrap()",
            "File::open(path).unwrap()",
            "File::create(path).unwrap()",
            "TcpStream::connect(address).unwrap()",
            "TcpListener::bind(address).unwrap()",
            "UdpSocket::bind(address).unwrap()",
            "Command::new(cmd).arg(flag).output().unwrap()",
            "process.output().unwrap()",
            "client.get(url).send().unwrap()",
            "std::fs::read(resolve_path().unwrap()).expect(\"diagnostic\")",
        ] {
            let source = format!("fn runtime() {{ {expression}; }}");
            let context = at_text(&index(&source), &source, "unwrap");
            assert_eq!(
                context.execution,
                RustExecutionContext::Runtime,
                "{expression}"
            );
            assert_eq!(context.code_role, RustCodeRole::Runtime, "{expression}");
        }
    }

    #[test]
    fn syntactic_io_inside_tests_keeps_setup_and_assertion_roles() {
        let source = r#"
#[test] fn check() {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(std::fs::File::open(path).expect("fixture"), expected);
}
"#;
        let index = index(source);
        let setup = at_text(&index, source, "unwrap");
        assert_eq!(setup.execution, RustExecutionContext::Test);
        assert_eq!(setup.code_role, RustCodeRole::TestSetup);
        let assertion = at_text(&index, source, "expect");
        assert_eq!(assertion.execution, RustExecutionContext::Test);
        assert_eq!(assertion.code_role, RustCodeRole::TestAssertion);
    }
}
