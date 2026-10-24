//! `#[peephole::scope]`: lets `inspect!()` find the variables in scope.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::visit_mut::{self, VisitMut};
use syn::{Block, Expr, FnArg, ItemFn, Macro, Pat, Stmt, Token, Type};

/// Fills in every argument-less `inspect!()` in the function with the
/// variables in scope at that point.
#[proc_macro_attribute]
pub fn scope(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[scope] takes no arguments",
        )
        .into_compile_error()
        .into();
    }
    expand(item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(item: TokenStream) -> syn::Result<TokenStream> {
    let mut function: ItemFn = syn::parse2(item)?;
    let mut scopes = Scopes::default();
    let mut params = Vec::new();
    for input in &function.sig.inputs {
        match input {
            FnArg::Receiver(receiver) => {
                let access = match (&receiver.reference, receiver.mutability) {
                    (Some(_), Some(_)) => Access::DerefMut,
                    (None, Some(_)) => Access::Mut,
                    _ => Access::Shared,
                };
                params.push(Binding {
                    name: quote!(self),
                    access,
                });
            }
            FnArg::Typed(typed) => bind_typed(&typed.pat, &typed.ty, &mut params),
        }
    }
    scopes.stack.push(params);
    scopes.visit_block_mut(&mut function.block);
    Ok(function.into_token_stream())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Shared,
    Mut,
    DerefMut,
    /// Borrowed or moved by an enclosing loop or match.
    Hidden,
}

struct Binding {
    name: TokenStream,
    access: Access,
}

fn bind(pat: &Pat, out: &mut Vec<Binding>) {
    match pat {
        Pat::Ident(ident) => {
            // `None` or `Foo` in a pattern are constants, not bindings.
            if ident.ident.to_string().starts_with(char::is_uppercase) {
                return;
            }
            let access = match (&ident.by_ref, &ident.mutability) {
                (Some(_), Some(_)) => Access::DerefMut,
                (None, Some(_)) => Access::Mut,
                _ => Access::Shared,
            };
            out.push(Binding {
                name: ident.ident.to_token_stream(),
                access,
            });
            if let Some((_, sub)) = &ident.subpat {
                bind(sub, out);
            }
        }
        Pat::Type(typed) => bind_typed(&typed.pat, &typed.ty, out),
        Pat::Reference(reference) => bind(&reference.pat, out),
        Pat::Paren(paren) => bind(&paren.pat, out),
        Pat::Tuple(tuple) => tuple.elems.iter().for_each(|p| bind(p, out)),
        Pat::TupleStruct(tuple) => tuple.elems.iter().for_each(|p| bind(p, out)),
        Pat::Slice(slice) => slice.elems.iter().for_each(|p| bind(p, out)),
        Pat::Struct(fields) => fields.fields.iter().for_each(|f| bind(&f.pat, out)),
        Pat::Or(or) => {
            if let Some(first) = or.cases.first() {
                bind(first, out);
            }
        }
        _ => {}
    }
}

/// `x: &mut T` can be changed through, like a `let mut`.
fn bind_typed(pat: &Pat, ty: &Type, out: &mut Vec<Binding>) {
    if let (Pat::Ident(ident), Type::Reference(reference)) = (pat, ty)
        && reference.mutability.is_some()
        && ident.by_ref.is_none()
    {
        out.push(Binding {
            name: ident.ident.to_token_stream(),
            access: Access::DerefMut,
        });
        return;
    }
    bind(pat, out);
}

/// Variables an expression names, which a loop or match over it may
/// borrow or move for its whole body.
fn hidden(expr: &Expr) -> Vec<Binding> {
    #[derive(Default)]
    struct Names(Vec<Binding>);
    impl Visit<'_> for Names {
        fn visit_expr_path(&mut self, path: &syn::ExprPath) {
            if let Some(ident) = path.path.get_ident() {
                self.0.push(Binding {
                    name: ident.to_token_stream(),
                    access: Access::Hidden,
                });
            }
        }

        fn visit_expr_closure(&mut self, _: &syn::ExprClosure) {}
    }
    let mut names = Names::default();
    names.visit_expr(expr);
    names.0
}

/// Also in `&&` chains.
fn lets(cond: &Expr) -> Vec<&syn::ExprLet> {
    match cond {
        Expr::Let(binding) => vec![binding],
        Expr::Binary(binary) if matches!(binary.op, syn::BinOp::And(_)) => {
            let mut out = lets(&binary.left);
            out.extend(lets(&binary.right));
            out
        }
        Expr::Paren(paren) => lets(&paren.expr),
        _ => Vec::new(),
    }
}

/// Bindings a condition introduces, after hiding what its scrutinees use.
fn condition_bindings(cond: &Expr) -> Vec<Binding> {
    let mut out = Vec::new();
    for binding in lets(cond) {
        out.extend(hidden(&binding.expr));
        bind(&binding.pat, &mut out);
    }
    out
}

#[derive(Default)]
struct Scopes {
    stack: Vec<Vec<Binding>>,
}

impl Scopes {
    fn visible(&self) -> Vec<&Binding> {
        let mut out: Vec<&Binding> = Vec::new();
        for binding in self.stack.iter().flatten() {
            let name = binding.name.to_string();
            out.retain(|known| known.name.to_string() != name);
            if binding.access != Access::Hidden {
                out.push(binding);
            }
        }
        out
    }

    fn within(&mut self, frame: Vec<Binding>, visit: impl FnOnce(&mut Self)) {
        self.stack.push(frame);
        visit(self);
        self.stack.pop();
    }

    /// Closures and async blocks capture what they name, so they only
    /// see their own variables.
    fn isolated(&mut self, frame: Vec<Binding>, visit: impl FnOnce(&mut Self)) {
        let outer = std::mem::replace(&mut self.stack, vec![frame]);
        visit(self);
        self.stack = outer;
    }

    fn fill(&self, mac: &mut Macro) {
        if mac
            .path
            .segments
            .last()
            .is_none_or(|s| s.ident != "inspect")
        {
            return;
        }
        let parser = Punctuated::<Expr, Token![,]>::parse_terminated;
        let Ok(args) = mac.parse_body_with(parser) else {
            return;
        };
        let mut excluded = Vec::new();
        for arg in &args {
            match arg {
                Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Not(_)) => {
                    excluded.push(unary.expr.to_token_stream().to_string());
                }
                _ => return, // an explicit list
            }
        }
        let filled = self
            .visible()
            .into_iter()
            .filter(|b| !excluded.contains(&b.name.to_string()))
            .map(|Binding { name, access }| match access {
                Access::Mut => quote!(mut #name),
                Access::DerefMut => quote!(mut *#name),
                _ => quote!(#name),
            });
        mac.tokens = quote!(#(#filled),*);
    }
}

impl VisitMut for Scopes {
    fn visit_block_mut(&mut self, block: &mut Block) {
        self.within(Vec::new(), |scopes| {
            for stmt in &mut block.stmts {
                match stmt {
                    Stmt::Local(local) => {
                        let Some(init) = &mut local.init else {
                            continue; // uninitialised, unusable until assigned
                        };
                        scopes.visit_expr_mut(&mut init.expr);
                        if let Some((_, diverge)) = &mut init.diverge {
                            scopes.visit_expr_mut(diverge);
                        }
                        let mut frame = Vec::new();
                        bind(&local.pat, &mut frame);
                        scopes.stack.last_mut().expect("in a block").extend(frame);
                    }
                    Stmt::Macro(mac) => scopes.fill(&mut mac.mac),
                    Stmt::Expr(expr, _) => scopes.visit_expr_mut(expr),
                    Stmt::Item(_) => {}
                }
            }
        });
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        match expr {
            Expr::Macro(mac) => self.fill(&mut mac.mac),
            Expr::Closure(closure) => {
                let mut frame = Vec::new();
                closure.inputs.iter().for_each(|p| bind(p, &mut frame));
                self.isolated(frame, |s| s.visit_expr_mut(&mut closure.body));
            }
            Expr::Async(block) => {
                self.isolated(Vec::new(), |s| s.visit_block_mut(&mut block.block));
            }
            Expr::ForLoop(each) => {
                self.visit_expr_mut(&mut each.expr);
                let mut frame = hidden(&each.expr);
                bind(&each.pat, &mut frame);
                self.within(frame, |s| s.visit_block_mut(&mut each.body));
            }
            Expr::If(branch) => {
                self.visit_expr_mut(&mut branch.cond);
                let frame = condition_bindings(&branch.cond);
                self.within(frame, |s| s.visit_block_mut(&mut branch.then_branch));
                if let Some((_, otherwise)) = &mut branch.else_branch {
                    self.visit_expr_mut(otherwise);
                }
            }
            Expr::While(each) => {
                self.visit_expr_mut(&mut each.cond);
                let frame = condition_bindings(&each.cond);
                self.within(frame, |s| s.visit_block_mut(&mut each.body));
            }
            Expr::Match(choice) => {
                self.visit_expr_mut(&mut choice.expr);
                for arm in &mut choice.arms {
                    let mut frame = hidden(&choice.expr);
                    bind(&arm.pat, &mut frame);
                    self.within(frame, |s| {
                        if let Some((_, guard)) = &mut arm.guard {
                            s.visit_expr_mut(guard);
                        }
                        s.visit_expr_mut(&mut arm.body);
                    });
                }
            }
            _ => visit_mut::visit_expr_mut(self, expr),
        }
    }

    fn visit_item_mut(&mut self, _: &mut syn::Item) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arguments of each `inspect!` after expansion.
    fn args(code: &str) -> Vec<String> {
        let out = expand(code.parse().unwrap()).unwrap().to_string();
        out.split("inspect ! (")
            .skip(1)
            .map(|rest| {
                let mut depth = 0;
                let end = rest
                    .char_indices()
                    .find(|&(_, c)| {
                        match c {
                            '(' => depth += 1,
                            ')' if depth == 0 => return true,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        false
                    })
                    .map_or(rest.len(), |(i, _)| i);
                rest[..end].trim().to_owned()
            })
            .collect()
    }

    #[test]
    fn collects_parameters_and_lets() {
        let code = "fn f(order: &mut Order, config: &Config, mut n: u32, (a, b): (u8, u8)) {
            let mut retries = 3;
            let names = load();
            let later;
            peephole::inspect!();
            later = 1;
        }";
        assert_eq!(
            args(code),
            ["mut * order , config , mut n , a , b , mut retries , names"]
        );
    }

    #[test]
    fn follows_shadowing_and_block_ends() {
        let code = r#"fn f() {
            let a = 1;
            { let b = 2; }
            let a = "x";
            inspect!();
        }"#;
        assert_eq!(args(code), ["a"]);
    }

    #[test]
    fn closures_only_see_their_own_variables() {
        let code = "fn f() {
            let outer = 1;
            let g = |x: u8, y| { let z = x; inspect!(); };
        }";
        assert_eq!(args(code), ["x , y , z"]);
    }

    #[test]
    fn hides_what_loops_and_matches_borrow() {
        let code = "fn f(items: &mut Vec<Item>, opt: Option<u8>) {
            let count = 0;
            for (i, item) in items.iter_mut().enumerate() {
                if let Some(v) = item.v {
                    inspect!();
                }
            }
            match opt {
                Some(ref mut inner) if inner > 0 => inspect!(),
                None => {}
            }
        }";
        // `item.v` borrows from `item` in the branch, the match from `opt`.
        assert_eq!(
            args(code),
            ["opt , count , i , v", "mut * items , count , mut * inner"]
        );
    }

    #[test]
    fn methods_and_exclusions() {
        let code = "fn m(&mut self, v: Vec<u8>, w: u8) {
            drop(v);
            fn nested() { let hidden = 1; inspect!(); }
            inspect!(!v);
            inspect!(w);
        }";
        assert_eq!(args(code), ["", "mut * self , w", "w"]);
    }
}
