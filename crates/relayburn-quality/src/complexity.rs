//! Per-function complexity metrics computed from a `syn` AST.
//!
//! Definitions (documented so the numbers are reproducible):
//!
//! * **Cyclomatic** — 1 + one per decision point: `if` / `if let`, `while` /
//!   `while let`, `for`, `&&`, `||`, `?`, and `arms - 1` per `match`.
//! * **Cognitive** — Sonar-style approximation: control-flow structures cost
//!   `1 + nesting`, `else` branches cost 1, each run of a logical operator
//!   (`a && b && c` is one run) costs 1, labeled `break`/`continue` cost 1,
//!   and nesting increases inside branches, loops, and closures.
//! * **Halstead difficulty** — `(n1 / 2) * (N2 / n2)` over the token stream
//!   of the function body, where keywords/punctuation are operators and
//!   idents/literals are operands.

use std::collections::HashSet;

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;

pub fn cyclomatic(block: &syn::Block) -> u32 {
    let mut v = CyclomaticVisitor { score: 1 };
    v.visit_block(block);
    v.score
}

struct CyclomaticVisitor {
    score: u32,
}

impl<'ast> Visit<'ast> for CyclomaticVisitor {
    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        match node {
            syn::Expr::If(_) | syn::Expr::While(_) | syn::Expr::ForLoop(_) | syn::Expr::Try(_) => {
                self.score += 1;
            }
            syn::Expr::Match(m) => {
                self.score += (m.arms.len() as u32).saturating_sub(1);
            }
            syn::Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) => {
                self.score += 1;
            }
            _ => {}
        }
        syn::visit::visit_expr(self, node);
    }
}

pub fn cognitive(block: &syn::Block) -> u32 {
    let mut v = CognitiveVisitor {
        score: 0,
        nesting: 0,
    };
    v.visit_block(block);
    v.score
}

struct CognitiveVisitor {
    score: u32,
    nesting: u32,
}

impl CognitiveVisitor {
    fn nested<F: FnOnce(&mut Self)>(&mut self, f: F) {
        self.nesting += 1;
        f(self);
        self.nesting -= 1;
    }

    /// One increment per run of the same logical operator.
    fn logical_runs(&mut self, expr: &syn::Expr, parent: Option<&syn::BinOp>) {
        if let syn::Expr::Binary(b) = expr {
            if matches!(b.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) {
                let same = matches!(
                    (parent, &b.op),
                    (Some(syn::BinOp::And(_)), syn::BinOp::And(_))
                        | (Some(syn::BinOp::Or(_)), syn::BinOp::Or(_))
                );
                if !same {
                    self.score += 1;
                }
                self.logical_runs(&b.left, Some(&b.op));
                self.logical_runs(&b.right, Some(&b.op));
                return;
            }
        }
        // Non-logical subtree: fall back to the general walk, which will
        // restart run detection inside it.
        syn::visit::visit_expr(self, expr);
    }
}

impl<'ast> Visit<'ast> for CognitiveVisitor {
    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        match node {
            syn::Expr::If(e) => {
                self.score += 1 + self.nesting;
                self.logical_runs(&e.cond, None);
                self.nested(|v| v.visit_block(&e.then_branch));
                if let Some((_, else_expr)) = &e.else_branch {
                    match else_expr.as_ref() {
                        // `else if` re-enters visit_expr and charges itself.
                        syn::Expr::If(_) => self.visit_expr(else_expr),
                        other => {
                            self.score += 1;
                            self.nested(|v| v.visit_expr(other));
                        }
                    }
                }
            }
            syn::Expr::Match(m) => {
                self.score += 1 + self.nesting;
                self.visit_expr(&m.expr);
                self.nested(|v| {
                    for arm in &m.arms {
                        if let Some((_, guard)) = &arm.guard {
                            v.logical_runs(guard, None);
                        }
                        v.visit_expr(&arm.body);
                    }
                });
            }
            syn::Expr::While(e) => {
                self.score += 1 + self.nesting;
                self.logical_runs(&e.cond, None);
                self.nested(|v| v.visit_block(&e.body));
            }
            syn::Expr::ForLoop(e) => {
                self.score += 1 + self.nesting;
                self.visit_expr(&e.expr);
                self.nested(|v| v.visit_block(&e.body));
            }
            syn::Expr::Loop(e) => {
                self.score += 1 + self.nesting;
                self.nested(|v| v.visit_block(&e.body));
            }
            syn::Expr::Closure(c) => {
                self.nested(|v| v.visit_expr(&c.body));
            }
            syn::Expr::Break(b) if b.label.is_some() => {
                self.score += 1;
                syn::visit::visit_expr(self, node);
            }
            syn::Expr::Continue(c) if c.label.is_some() => {
                self.score += 1;
            }
            syn::Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) => {
                self.logical_runs(node, None);
            }
            _ => syn::visit::visit_expr(self, node),
        }
    }
}

pub fn halstead_difficulty(block: &syn::Block) -> f64 {
    use quote::ToTokens;
    let mut counts = HalsteadCounts::default();
    let stream = block.to_token_stream();
    count_tokens(stream, &mut counts);
    counts.difficulty()
}

#[derive(Default)]
struct HalsteadCounts {
    operators: HashSet<String>,
    operands: HashSet<String>,
    total_operators: u64,
    total_operands: u64,
}

impl HalsteadCounts {
    fn operator(&mut self, tok: String) {
        self.operators.insert(tok);
        self.total_operators += 1;
    }

    fn operand(&mut self, tok: String) {
        self.operands.insert(tok);
        self.total_operands += 1;
    }

    fn difficulty(&self) -> f64 {
        let n1 = self.operators.len() as f64;
        let n2 = self.operands.len() as f64;
        if n2 == 0.0 {
            return 0.0;
        }
        (n1 / 2.0) * (self.total_operands as f64 / n2)
    }
}

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "trait", "type", "unsafe", "use", "where", "while", "yield",
];

fn count_tokens(stream: TokenStream, counts: &mut HalsteadCounts) {
    for tree in stream {
        match tree {
            TokenTree::Group(g) => {
                counts.operator(format!("{:?}", g.delimiter()));
                count_tokens(g.stream(), counts);
            }
            TokenTree::Ident(i) => {
                let s = i.to_string();
                if KEYWORDS.contains(&s.as_str()) {
                    counts.operator(s);
                } else {
                    counts.operand(s);
                }
            }
            TokenTree::Punct(p) => counts.operator(p.as_char().to_string()),
            TokenTree::Literal(l) => counts.operand(l.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(src: &str) -> syn::Block {
        syn::parse_str(&format!("{{ {src} }}")).unwrap()
    }

    #[test]
    fn cyclomatic_counts_branches() {
        // 1 base + if + && + match(3 arms - 1) = 5
        let b = block("if a && b { } match x { 1 => {}, 2 => {}, _ => {} }");
        assert_eq!(cyclomatic(&b), 5);
    }

    #[test]
    fn cyclomatic_counts_try_operator() {
        assert_eq!(cyclomatic(&block("let x = f()?;")), 2);
    }

    #[test]
    fn cognitive_charges_nesting() {
        // outer if: 1, inner if: 2 (nested), else: 1 => 4
        let b = block("if a { if b { } } else { }");
        assert_eq!(cognitive(&b), 4);
    }

    #[test]
    fn cognitive_counts_logical_runs_not_operators() {
        // if: 1, one && run: 1, one || run after the switch: 1 => 3
        let b = block("if a && b && c || d { }");
        assert_eq!(cognitive(&b), 3);
    }

    #[test]
    fn cognitive_closure_increases_nesting() {
        // for: 1; closure body's if: 1 + 2 (for + closure nesting) = 3 => 4
        let b = block("for x in y { f(|| if z { } ); }");
        assert_eq!(cognitive(&b), 4);
    }

    #[test]
    fn halstead_difficulty_is_positive_for_real_code() {
        let b = block("let x = a + b; let y = x * x;");
        assert!(halstead_difficulty(&b) > 0.0);
    }

    #[test]
    fn halstead_empty_block_is_zero() {
        assert_eq!(halstead_difficulty(&block("")), 0.0);
    }
}
