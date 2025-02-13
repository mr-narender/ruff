use ruff_python_ast::{self as ast, Decorator, Expr, Parameters, Stmt};

use crate::checkers::ast::Checker;
use crate::importer::ImportRequest;
use ruff_diagnostics::{Diagnostic, Edit, Fix, FixAvailability, Violation};
use ruff_macros::{derive_message_formats, ViolationMetadata};
use ruff_python_ast::helpers::map_subscript;
use ruff_python_ast::identifier::Identifier;
use ruff_python_semantic::analyze;
use ruff_python_semantic::analyze::visibility::{is_abstract, is_final, is_overload};
use ruff_python_semantic::{ScopeKind, SemanticModel};
use ruff_text_size::{Ranged, TextRange};

/// ## What it does
/// Checks for methods that are annotated with a fixed return type which
/// should instead be returning `Self`.
///
/// ## Why is this bad?
/// If methods that generally return `self` at runtime are annotated with a
/// fixed return type, and the class is subclassed, type checkers will not be
/// able to infer the correct return type.
///
/// For example:
/// ```python
/// class Shape:
///     def set_scale(self, scale: float) -> Shape:
///         self.scale = scale
///         return self
///
/// class Circle(Shape):
///     def set_radius(self, radius: float) -> Circle:
///         self.radius = radius
///         return self
///
/// # Type checker infers return type as `Shape`, not `Circle`.
/// Circle().set_scale(0.5)
///
/// # Thus, this expression is invalid, as `Shape` has no attribute `set_radius`.
/// Circle().set_scale(0.5).set_radius(2.7)
/// ```
///
/// Specifically, this check enforces that the return type of the following
/// methods is `Self`:
///
/// 1. In-place binary-operation dunder methods, like `__iadd__`, `__imul__`, etc.
/// 1. `__new__`, `__enter__`, and `__aenter__`, if those methods return the
///    class name.
/// 1. `__iter__` methods that return `Iterator`, despite the class inheriting
///    directly from `Iterator`.
/// 1. `__aiter__` methods that return `AsyncIterator`, despite the class
///    inheriting directly from `AsyncIterator`.
///
/// ## Example
///
/// ```pyi
/// class Foo:
///     def __new__(cls, *args: Any, **kwargs: Any) -> Foo: ...
///     def __enter__(self) -> Foo: ...
///     async def __aenter__(self) -> Foo: ...
///     def __iadd__(self, other: Foo) -> Foo: ...
/// ```
///
/// Use instead:
///
/// ```pyi
/// from typing_extensions import Self
///
/// class Foo:
///     def __new__(cls, *args: Any, **kwargs: Any) -> Self: ...
///     def __enter__(self) -> Self: ...
///     async def __aenter__(self) -> Self: ...
///     def __iadd__(self, other: Foo) -> Self: ...
/// ```
/// ## References
/// - [Python documentation: `typing.Self`](https://docs.python.org/3/library/typing.html#typing.Self)
#[derive(ViolationMetadata)]
pub(crate) struct NonSelfReturnType {
    class_name: String,
    method_name: String,
}

impl Violation for NonSelfReturnType {
    const FIX_AVAILABILITY: FixAvailability = FixAvailability::Sometimes;

    #[derive_message_formats]
    fn message(&self) -> String {
        let NonSelfReturnType {
            class_name,
            method_name,
        } = self;

        if matches!(class_name.as_str(), "__new__") {
            "`__new__` methods usually return `self` at runtime".to_string()
        } else {
            format!("`{method_name}` methods in classes like `{class_name}` usually return `self` at runtime")
        }
    }

    fn fix_title(&self) -> Option<String> {
        Some("Use `Self` as return type".to_string())
    }
}

/// PYI034
pub(crate) fn non_self_return_type(
    checker: &mut Checker,
    stmt: &Stmt,
    is_async: bool,
    name: &str,
    decorator_list: &[Decorator],
    returns: Option<&Expr>,
    parameters: &Parameters,
) {
    let semantic = checker.semantic();

    let ScopeKind::Class(class_def) = semantic.current_scope().kind else {
        return;
    };

    if parameters.args.is_empty() && parameters.posonlyargs.is_empty() {
        return;
    }

    let Some(returns) = returns else {
        return;
    };

    // PEP 673 forbids the use of `typing(_extensions).Self` in metaclasses.
    if analyze::class::is_metaclass(class_def, semantic).into() {
        return;
    }

    // Skip any abstract or overloaded methods.
    if is_abstract(decorator_list, semantic) || is_overload(decorator_list, semantic) {
        return;
    }

    if is_async {
        if name == "__aenter__"
            && is_name(returns, &class_def.name)
            && !is_final(&class_def.decorator_list, semantic)
        {
            add_diagnostic(checker, stmt, returns, class_def, name);
        }
        return;
    }

    // In-place methods that are expected to return `Self`.
    if is_inplace_bin_op(name) {
        if !is_self(returns, checker) {
            add_diagnostic(checker, stmt, returns, class_def, name);
        }
        return;
    }

    if is_name(returns, &class_def.name) {
        if matches!(name, "__enter__" | "__new__") && !is_final(&class_def.decorator_list, semantic)
        {
            add_diagnostic(checker, stmt, returns, class_def, name);
        }
        return;
    }

    match name {
        "__iter__" => {
            if is_iterable_or_iterator(returns, semantic)
                && subclasses_iterator(class_def, semantic)
            {
                add_diagnostic(checker, stmt, returns, class_def, name);
            }
        }
        "__aiter__" => {
            if is_async_iterable_or_iterator(returns, semantic)
                && subclasses_async_iterator(class_def, semantic)
            {
                add_diagnostic(checker, stmt, returns, class_def, name);
            }
        }
        _ => {}
    }
}

/// Add a diagnostic for the given method.
fn add_diagnostic(
    checker: &mut Checker,
    stmt: &Stmt,
    returns: &Expr,
    class_def: &ast::StmtClassDef,
    method_name: &str,
) {
    /// Return an [`Edit`] that imports `typing.Self` from `typing` or `typing_extensions`.
    fn import_self(checker: &Checker, range: TextRange) -> Option<Edit> {
        let target_version = checker.settings.target_version.as_tuple();
        let source_module = if target_version >= (3, 11) {
            "typing"
        } else {
            "typing_extensions"
        };

        let (importer, semantic) = (checker.importer(), checker.semantic());
        let request = ImportRequest::import_from(source_module, "Self");

        let (edit, ..) = importer
            .get_or_import_symbol(&request, range.start(), semantic)
            .ok()?;

        Some(edit)
    }

    /// Generate a [`Fix`] that replaces the return type with `Self`.
    fn replace_with_self(checker: &mut Checker, range: TextRange) -> Option<Fix> {
        let import_self = import_self(checker, range)?;
        let replace_with_self = Edit::range_replacement("Self".to_string(), range);
        Some(Fix::unsafe_edits(import_self, [replace_with_self]))
    }

    let mut diagnostic = Diagnostic::new(
        NonSelfReturnType {
            class_name: class_def.name.to_string(),
            method_name: method_name.to_string(),
        },
        stmt.identifier(),
    );
    if let Some(fix) = replace_with_self(checker, returns.range()) {
        diagnostic.set_fix(fix);
    }
    checker.diagnostics.push(diagnostic);
}

/// Returns `true` if the method is an in-place binary operator.
fn is_inplace_bin_op(name: &str) -> bool {
    matches!(
        name,
        "__iadd__"
            | "__isub__"
            | "__imul__"
            | "__imatmul__"
            | "__itruediv__"
            | "__ifloordiv__"
            | "__imod__"
            | "__ipow__"
            | "__ilshift__"
            | "__irshift__"
            | "__iand__"
            | "__ixor__"
            | "__ior__"
    )
}

/// Return `true` if the given expression resolves to the given name.
fn is_name(expr: &Expr, name: &str) -> bool {
    let Expr::Name(ast::ExprName { id, .. }) = expr else {
        return false;
    };
    id.as_str() == name
}

/// Return `true` if the given expression resolves to `typing.Self`.
fn is_self(expr: &Expr, checker: &Checker) -> bool {
    checker.match_maybe_stringized_annotation(expr, |expr| {
        checker.semantic().match_typing_expr(expr, "Self")
    })
}

/// Return `true` if the given class extends `collections.abc.Iterator`.
fn subclasses_iterator(class_def: &ast::StmtClassDef, semantic: &SemanticModel) -> bool {
    analyze::class::any_qualified_base_class(class_def, semantic, &|qualified_name| {
        matches!(
            qualified_name.segments(),
            ["typing", "Iterator"] | ["collections", "abc", "Iterator"]
        )
    })
}

/// Return `true` if the given expression resolves to `collections.abc.Iterable` or `collections.abc.Iterator`.
fn is_iterable_or_iterator(expr: &Expr, semantic: &SemanticModel) -> bool {
    semantic
        .resolve_qualified_name(map_subscript(expr))
        .is_some_and(|qualified_name| {
            matches!(
                qualified_name.segments(),
                ["typing", "Iterable" | "Iterator"]
                    | ["collections", "abc", "Iterable" | "Iterator"]
            )
        })
}

/// Return `true` if the given class extends `collections.abc.AsyncIterator`.
fn subclasses_async_iterator(class_def: &ast::StmtClassDef, semantic: &SemanticModel) -> bool {
    analyze::class::any_qualified_base_class(class_def, semantic, &|qualified_name| {
        matches!(
            qualified_name.segments(),
            ["typing", "AsyncIterator"] | ["collections", "abc", "AsyncIterator"]
        )
    })
}

/// Return `true` if the given expression resolves to `collections.abc.AsyncIterable` or `collections.abc.AsyncIterator`.
fn is_async_iterable_or_iterator(expr: &Expr, semantic: &SemanticModel) -> bool {
    semantic
        .resolve_qualified_name(map_subscript(expr))
        .is_some_and(|qualified_name| {
            matches!(
                qualified_name.segments(),
                ["typing", "AsyncIterable" | "AsyncIterator"]
                    | ["collections", "abc", "AsyncIterable" | "AsyncIterator"]
            )
        })
}
