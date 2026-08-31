//! Source-level metrics for Rust production code: per-function cyclomatic
//! complexity, cognitive complexity (Sonar-style approximation), Halstead
//! difficulty, and per-file logical lines of code.
//!
//! Test code (`tests/` dirs, `*_tests.rs` / `tests.rs` files, `#[cfg(test)]`
//! modules, `#[test]` functions) is excluded: the benchmark measures the
//! shipping surface, and test tables/fixtures legitimately trade these
//! metrics for readability.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use syn::spanned::Spanned;
use syn::visit::Visit;

#[derive(Debug, Clone, serde::Serialize)]
pub struct FunctionMetrics {
    /// Repo-relative file path.
    pub file: String,
    /// Qualified name within the file, e.g. `LedgerHandle::summary`.
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub cyclomatic: u32,
    pub cognitive: u32,
    pub halstead_difficulty: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FileMetrics {
    pub file: String,
    /// Non-blank, non-comment lines, test modules excluded.
    pub loc: usize,
}

#[derive(Debug, Default)]
pub struct RustMetrics {
    pub files: Vec<FileMetrics>,
    pub functions: Vec<FunctionMetrics>,
}

/// True for files that hold test code rather than shipping code.
fn is_test_path(rel: &str) -> bool {
    rel.contains("/tests/")
        || rel.contains("/benches/")
        || rel.ends_with("/tests.rs")
        || rel.ends_with("_tests.rs")
}

/// Collect metrics for every production `.rs` file under the given roots.
pub fn collect(repo_root: &Path, source_roots: &[String]) -> Result<RustMetrics> {
    let mut out = RustMetrics::default();
    let mut rust_files = Vec::new();
    for root in source_roots {
        collect_rust_files(&repo_root.join(root), &mut rust_files)?;
    }
    rust_files.sort();

    for path in rust_files {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if is_test_path(&rel) {
            continue;
        }
        let src = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let ast: syn::File =
            syn::parse_file(&src).with_context(|| format!("parsing {}", path.display()))?;

        out.files.push(FileMetrics {
            file: rel.clone(),
            loc: count_loc(&src, &ast),
        });

        let mut visitor = FnVisitor {
            file: rel,
            scope: Vec::new(),
            functions: &mut out.functions,
        };
        visitor.visit_file(&ast);
    }
    Ok(out)
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "target" || name == "node_modules" || name.starts_with('.') {
                continue;
            }
            collect_rust_files(&path, out)?;
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Non-blank, non-comment lines, minus lines inside `#[cfg(test)]` modules.
fn count_loc(src: &str, ast: &syn::File) -> usize {
    let mut code_lines = crate::loc::code_lines(src);
    for item in &ast.items {
        if let syn::Item::Mod(m) = item {
            if is_cfg_test(&m.attrs) {
                let start = m.span().start().line;
                let end = m.span().end().line;
                code_lines.retain(|&l| l < start || l > end);
            }
        }
    }
    code_lines.len()
}

/// `#[cfg(test)]` (or any cfg predicate mentioning `test`).
fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| match &a.meta {
        syn::Meta::List(l) if l.path.is_ident("cfg") => {
            l.tokens.clone().into_iter().any(|t| match t {
                proc_macro2::TokenTree::Ident(i) => i == "test",
                _ => false,
            })
        }
        _ => false,
    })
}

/// `#[test]`, `#[tokio::test]`, and friends.
fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"))
        || is_cfg_test(attrs)
}

struct FnVisitor<'a> {
    file: String,
    scope: Vec<String>,
    functions: &'a mut Vec<FunctionMetrics>,
}

impl FnVisitor<'_> {
    fn record(
        &mut self,
        name: &str,
        attrs: &[syn::Attribute],
        block: &syn::Block,
        span: proc_macro2::Span,
    ) {
        if has_test_attr(attrs) {
            return;
        }
        let qualified = if self.scope.is_empty() {
            name.to_string()
        } else {
            format!("{}::{}", self.scope.join("::"), name)
        };
        self.functions.push(FunctionMetrics {
            file: self.file.clone(),
            name: qualified,
            start_line: span.start().line,
            end_line: span.end().line,
            cyclomatic: crate::complexity::cyclomatic(block),
            cognitive: crate::complexity::cognitive(block),
            halstead_difficulty: crate::complexity::halstead_difficulty(block),
        });
    }
}

impl<'ast> Visit<'ast> for FnVisitor<'_> {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if is_cfg_test(&node.attrs) {
            return;
        }
        self.scope.push(node.ident.to_string());
        syn::visit::visit_item_mod(self, node);
        self.scope.pop();
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        self.scope.push(type_name_of(&node.self_ty));
        syn::visit::visit_item_impl(self, node);
        self.scope.pop();
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        // No recursion: closures and nested fns score as part of this body.
        self.record(
            &node.sig.ident.to_string(),
            &node.attrs,
            &node.block,
            node.span(),
        );
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.record(
            &node.sig.ident.to_string(),
            &node.attrs,
            &node.block,
            node.span(),
        );
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if let Some(block) = &node.default {
            self.record(&node.sig.ident.to_string(), &node.attrs, block, node.span());
        }
    }
}

fn type_name_of(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "impl".into()),
        syn::Type::Reference(r) => type_name_of(&r.elem),
        _ => "impl".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics_for(src: &str) -> Vec<FunctionMetrics> {
        let ast: syn::File = syn::parse_file(src).unwrap();
        let mut functions = Vec::new();
        let mut v = FnVisitor {
            file: "test.rs".into(),
            scope: Vec::new(),
            functions: &mut functions,
        };
        v.visit_file(&ast);
        functions
    }

    #[test]
    fn skips_cfg_test_modules_and_test_fns() {
        let fns = metrics_for(
            r#"
            fn shipped() {}
            #[cfg(test)]
            mod tests {
                fn helper() {}
            }
            #[test]
            fn a_test() {}
            "#,
        );
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "shipped");
    }

    #[test]
    fn qualifies_impl_methods() {
        let fns = metrics_for("struct S; impl S { fn go(&self) {} }");
        assert_eq!(fns[0].name, "S::go");
    }

    #[test]
    fn straight_line_fn_has_base_complexity() {
        let fns = metrics_for("fn f() { let x = 1; }");
        assert_eq!(fns[0].cyclomatic, 1);
        assert_eq!(fns[0].cognitive, 0);
    }
}
