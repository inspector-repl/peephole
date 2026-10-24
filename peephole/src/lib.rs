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
pub use peephole_macros::scope;

use bevy_reflect::func::{DynamicFunction, IntoFunction};
use std::sync::Mutex;

#[doc(hidden)]
pub struct Location {
    pub file: &'static str,
    /// `file` is relative to this or to its workspace.
    pub manifest_dir: &'static str,
    pub line: u32,
    pub function: &'static str,
}

pub(crate) enum Access<'a> {
    Shared(&'a dyn Reflect),
    Mut(&'a mut dyn Reflect),
    /// For borrows reflection can't hold, like `&str`.
    Copy(Box<dyn Reflect>),
}

pub struct Local<'a> {
    pub(crate) name: &'static str,
    pub(crate) type_name: &'static str,
    pub(crate) access: Access<'a>,
}

impl<'a> Local<'a> {
    pub fn shared<T: Reflect>(name: &'static str, value: &'a T) -> Self {
        Self {
            name,
            type_name: std::any::type_name::<T>(),
            access: Access::Shared(value),
        }
    }

    pub fn copied<T: Reflect>(name: &'static str, type_name: &'static str, value: T) -> Self {
        Self {
            name,
            type_name,
            access: Access::Copy(Box::new(value)),
        }
    }

    pub fn writable<T: Reflect>(name: &'static str, value: &'a mut T) -> Self {
        Self {
            name,
            type_name: std::any::type_name::<T>(),
            access: Access::Mut(value),
        }
    }
}

// Autoref specialization: method lookup tries the most `&`s first, so
// `&str` beats `&&T`, which beats `&T`/`&mut T`, which beat anything.
#[doc(hidden)]
pub struct Wrap<R>(std::cell::Cell<Option<R>>);

impl<R> Wrap<R> {
    pub fn new(borrow: R) -> Self {
        Self(std::cell::Cell::new(Some(borrow)))
    }

    fn take(&self) -> R {
        self.0.take().expect("inspect! takes each variable once")
    }
}

#[doc(hidden)]
pub trait ViaStr<'a> {
    fn local(&self, name: &'static str) -> Local<'a>;
}

impl<'a> ViaStr<'a> for &&&Wrap<&&str> {
    fn local(&self, name: &'static str) -> Local<'a> {
        Local::copied(name, "&str", (*self.take()).to_owned())
    }
}

#[doc(hidden)]
pub trait ViaSharedRef<'a> {
    fn local(&self, name: &'static str) -> Local<'a>;
}

impl<'a, T: Reflect> ViaSharedRef<'a> for &&Wrap<&'a &T> {
    fn local(&self, name: &'static str) -> Local<'a> {
        Local::shared(name, *self.take())
    }
}

#[doc(hidden)]
pub trait ViaShared<'a> {
    fn local(&self, name: &'static str) -> Local<'a>;
}

impl<'a, T: Reflect> ViaShared<'a> for &Wrap<&'a T> {
    fn local(&self, name: &'static str) -> Local<'a> {
        Local::shared(name, self.take())
    }
}

#[doc(hidden)]
pub trait ViaMut<'a> {
    fn local(&self, name: &'static str) -> Local<'a>;
}

impl<'a, T: Reflect> ViaMut<'a> for &Wrap<&'a mut T> {
    fn local(&self, name: &'static str) -> Local<'a> {
        Local::writable(name, self.take())
    }
}

#[doc(hidden)]
pub trait ViaOpaque<'a> {
    fn local(&self, name: &'static str) -> Local<'a>;
}

/// Stands in for values without `Reflect`, so they still show up.
#[derive(Reflect)]
#[reflect(type_path = false)]
struct Opaque;

impl bevy_reflect::TypePath for Opaque {
    fn type_path() -> &'static str {
        "opaque"
    }

    fn short_type_path() -> &'static str {
        "<no Reflect>"
    }
}

impl<'a, R> ViaOpaque<'a> for Wrap<R> {
    fn local(&self, name: &'static str) -> Local<'a> {
        let type_name = std::any::type_name::<R>()
            .trim_start_matches('&')
            .trim_start_matches("mut ")
            .trim_start_matches('&');
        Local::copied(name, type_name, Opaque)
    }
}

/// Stops here so a `peephole` REPL can inspect the listed variables.
/// Their types must derive [`Reflect`].
///
/// Variables are read-only unless marked `mut`, which needs a `mut`
/// binding like any `&mut` borrow:
///
/// ```ignore
/// fn checkout(order: &mut Order, config: &Config) {
///     let mut retries = 3;
///     peephole::inspect!(mut *order, config, mut retries, dir = config.dir());
/// }
/// ```
#[macro_export]
macro_rules! inspect {
    ($($args:tt)*) => {{
        #[allow(unused_imports)]
        use $crate::{ViaMut as _, ViaOpaque as _, ViaShared as _, ViaSharedRef as _, ViaStr as _};
        $crate::stop(
            &$crate::Location {
                file: ::core::file!(),
                manifest_dir: match ::core::option_env!("CARGO_MANIFEST_DIR") {
                    ::core::option::Option::Some(dir) => dir,
                    ::core::option::Option::None => "",
                },
                line: ::core::line!(),
                function: {
                    fn here() {}
                    let name = ::core::any::type_name_of_val(&here);
                    &name[..name.len() - "::here".len()]
                },
            },
            &mut $crate::__locals!([] $($args)*),
        );
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __locals {
    ([$($out:expr),*] $(,)?) => { [$($out),*] };
    ([$($out:expr),*] mut $value:expr $(, $($rest:tt)*)?) => {
        $crate::__locals!(
            [$($out,)* (&&&&$crate::Wrap::new(&mut $value)).local($crate::local_name(::core::stringify!($value)))]
            $($($rest)*)?
        )
    };
    ([$($out:expr),*] $name:ident = $value:expr $(, $($rest:tt)*)?) => {
        $crate::__locals!(
            [$($out,)* (&&&&$crate::Wrap::new(&$value)).local(::core::stringify!($name))]
            $($($rest)*)?
        )
    };
    ([$($out:expr),*] $value:expr $(, $($rest:tt)*)?) => {
        $crate::__locals!(
            [$($out,)* (&&&&$crate::Wrap::new(&$value)).local($crate::local_name(::core::stringify!($value)))]
            $($($rest)*)?
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
