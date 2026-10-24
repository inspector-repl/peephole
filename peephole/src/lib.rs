//! Stop a running program at [`inspect!`] and explore its variables from
//! the `peephole` REPL.
//!
//! ```ignore
//! #[derive(peephole::Reflect)]
//! struct Order { id: u64 }
//!
//! let mut order = Order { id: 7 };
//! peephole::inspect!(order);
//! ```
//!
//! Without a listening client `inspect!` returns at once, unless
//! `PEEPHOLE_WAIT` is set.

mod complete;
mod node;
mod render;
mod session;

pub use bevy_reflect::{self, Reflect};

use bevy_reflect::func::{DynamicFunction, IntoFunction};
use std::sync::Mutex;

/// Where `inspect!` stopped.
#[doc(hidden)]
pub struct Location {
    pub file: &'static str,
    pub line: u32,
    pub function: &'static str,
}

/// A variable handed to the REPL.
pub struct Local<'a> {
    name: &'static str,
    type_name: &'static str,
    value: &'a mut dyn Reflect,
}

impl<'a> Local<'a> {
    pub fn new<T: Reflect>(name: &'static str, value: &'a mut T) -> Self {
        Self {
            name,
            type_name: std::any::type_name::<T>(),
            value,
        }
    }
}

// `inspect!` takes both locals and `&mut` references to reflected values.
// Method lookup tries `&&Wrap` before `&Wrap`, so references win.
#[doc(hidden)]
pub struct Wrap<'a, T>(std::cell::Cell<Option<&'a mut T>>);

impl<'a, T> Wrap<'a, T> {
    pub fn new(value: &'a mut T) -> Self {
        Self(std::cell::Cell::new(Some(value)))
    }

    fn take(&self) -> &'a mut T {
        self.0.take().expect("inspect! takes each variable once")
    }
}

#[doc(hidden)]
pub trait ViaRef<'a> {
    fn local(&self, name: &'static str) -> Local<'a>;
}

impl<'a, T: Reflect> ViaRef<'a> for &Wrap<'a, &mut T> {
    fn local(&self, name: &'static str) -> Local<'a> {
        Local::new(name, &mut **self.take())
    }
}

#[doc(hidden)]
pub trait ViaValue<'a> {
    fn local(&self, name: &'static str) -> Local<'a>;
}

impl<'a, T: Reflect> ViaValue<'a> for Wrap<'a, T> {
    fn local(&self, name: &'static str) -> Local<'a> {
        Local::new(name, self.take())
    }
}

/// Stops here so a `peephole` REPL can inspect and change the listed
/// variables. Their types must derive [`Reflect`]. Owned locals need a
/// `mut` binding. Pass `&mut` parameters dereferenced: `inspect!(*order)`.
#[macro_export]
macro_rules! inspect {
    ($($var:expr),* $(,)?) => {
        $crate::stop(
            &$crate::Location {
                file: ::core::file!(),
                line: ::core::line!(),
                function: {
                    fn here() {}
                    let name = ::core::any::type_name_of_val(&here);
                    &name[..name.len() - "::here".len()]
                },
            },
            &mut [$({
                #[allow(unused_imports)]
                use $crate::{ViaRef as _, ViaValue as _};
                (&&$crate::Wrap::new(&mut $var)).local($crate::local_name(::core::stringify!($var)))
            }),*],
        )
    };
}

pub(crate) static FUNCTIONS: Mutex<Vec<(String, DynamicFunction<'static>)>> =
    Mutex::new(Vec::new());

/// Makes `function` callable from the REPL. A function whose first
/// parameter is a reflected type is also callable as a method on it, e.g.
/// `order.total()`.
pub fn expose<F, Marker>(name: &str, function: F)
where
    F: IntoFunction<'static, Marker>,
{
    let mut functions = FUNCTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    functions.retain(|(existing, _)| existing != name);
    functions.push((name.to_owned(), function.into_function()));
}

/// `*order` → `order`, so `&mut` parameters keep their name.
#[doc(hidden)]
#[must_use]
pub fn local_name(expr: &'static str) -> &'static str {
    expr.trim_start_matches(['*', ' '])
}

#[doc(hidden)]
pub fn stop(location: &Location, locals: &mut [Local<'_>]) {
    session::run(location, locals);
}
