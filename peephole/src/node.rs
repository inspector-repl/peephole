//! Live views into program memory for Rhai scripts.

use crate::render;
use bevy_reflect::{PartialReflect, ReflectMut, ReflectRef};
use rhai::{Dynamic, Engine, EvalAltResult, FnPtr, NativeCallContext};
use std::any::TypeId;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub type Res<T> = Result<T, Box<EvalAltResult>>;

/// State shared by all nodes of one stop.
#[derive(Default)]
pub struct Shared {
    /// Bumped by writes that may move memory nodes point into, like
    /// replacing a whole `Vec`.
    generation: Cell<u64>,
    /// Values returned by exposed functions, kept alive for the stop.
    owned: RefCell<Vec<Box<dyn PartialReflect>>>,
}

/// A reference to a value in the stopped program. Scripts work on the
/// program's memory, never on copies.
#[derive(Clone)]
pub struct Node {
    ptr: *mut dyn PartialReflect,
    /// False for values `inspect!` got as `&`, which must never be written.
    writable: bool,
    /// The generation it was created in, `None` for values that never
    /// move, like the program's locals.
    born: Option<u64>,
    shared: Rc<Shared>,
}

impl Node {
    /// # Safety
    /// `ptr` must stay valid while any clone of the node exists, and come
    /// from a `&mut` if `writable`. The session drops all nodes before
    /// `inspect!` returns.
    unsafe fn from_raw(
        ptr: *mut dyn PartialReflect,
        writable: bool,
        born: Option<u64>,
        shared: &Rc<Shared>,
    ) -> Self {
        Self {
            ptr,
            writable,
            born,
            shared: Rc::clone(shared),
        }
    }

    /// A node for a value whose address never changes during the stop.
    ///
    /// # Safety
    /// See [`Node::from_raw`].
    pub unsafe fn root(value: &mut dyn PartialReflect, shared: &Rc<Shared>) -> Self {
        // SAFETY: forwarded to the caller.
        unsafe { Self::from_raw(std::ptr::from_mut(value), true, None, shared) }
    }

    /// A read-only node for a value whose address never changes.
    ///
    /// # Safety
    /// See [`Node::from_raw`].
    pub unsafe fn root_shared(value: &dyn PartialReflect, shared: &Rc<Shared>) -> Self {
        // SAFETY: forwarded to the caller, and `writable` is false.
        unsafe { Self::from_raw(std::ptr::from_ref(value).cast_mut(), false, None, shared) }
    }

    fn check(&self) -> Res<()> {
        if self
            .born
            .is_some_and(|born| born != self.shared.generation.get())
        {
            return Err(
                "stale reference: a later assignment may have moved this value. \
                        Evaluate the expression again."
                    .into(),
            );
        }
        Ok(())
    }

    pub fn get(&mut self) -> Res<&dyn PartialReflect> {
        self.check()?;
        // SAFETY: valid per `from_raw`, and no write since `born` moved it.
        Ok(unsafe { &*self.ptr })
    }

    pub fn get_mut(&mut self) -> Res<&mut dyn PartialReflect> {
        self.check()?;
        if !self.writable {
            return Err("read-only: pass the variable to inspect! as `mut` to change it".into());
        }
        // SAFETY: as in `get`, and the pointer came from a `&mut`.
        Ok(unsafe { &mut *self.ptr })
    }

    /// A node for `value`, which lives inside this node's value.
    fn child(&self, value: *mut dyn PartialReflect) -> Dynamic {
        // SAFETY: values reachable from a node live as long as the node.
        // Children of read-only nodes stay read-only.
        present(unsafe {
            Self::from_raw(
                value,
                self.writable,
                Some(self.shared.generation.get()),
                &self.shared,
            )
        })
    }
}

fn is_option(value: &dyn PartialReflect) -> bool {
    value
        .reflect_type_path()
        .starts_with("core::option::Option<")
}

/// How scripts see a node: leaves as Rhai values, `Some(x)` as `x` and
/// `None` as `()`, everything else as the node itself.
pub fn present(mut node: Node) -> Dynamic {
    let Ok(value) = node.get() else {
        return Dynamic::from(node);
    };
    if let Some(leaf) = leaf(value) {
        return leaf;
    }
    if is_option(value)
        && let ReflectRef::Enum(option) = value.reflect_ref()
    {
        if option.variant_name() == "None" {
            return Dynamic::UNIT;
        }
        return step(&mut node, &Step::Index(0)).unwrap_or(Dynamic::UNIT);
    }
    Dynamic::from(node)
}

impl Node {}

macro_rules! leaf_to_dynamic {
    ($value:expr; lossless: $($small:ty),*; checked: $($big:ty),*) => {
        $(if let Some(x) = $value.try_downcast_ref::<$small>() { return Some(Dynamic::from(i64::from(*x))); })*
        $(if let Some(x) = $value.try_downcast_ref::<$big>() {
            return i64::try_from(*x).ok().map(Dynamic::from);
        })*
    };
}

/// Numbers, strings and the like as Rhai values, `None` for anything else.
pub fn leaf(value: &dyn PartialReflect) -> Option<Dynamic> {
    leaf_to_dynamic!(value; lossless: i8, i16, i32, i64, u8, u16, u32; checked: isize, usize, u64, i128, u128);
    if let Some(x) = value.try_downcast_ref::<f64>() {
        return Some(Dynamic::from(*x));
    }
    if let Some(x) = value.try_downcast_ref::<f32>() {
        return Some(Dynamic::from(f64::from(*x)));
    }
    if let Some(x) = value.try_downcast_ref::<bool>() {
        return Some(Dynamic::from(*x));
    }
    if let Some(x) = value.try_downcast_ref::<char>() {
        return Some(Dynamic::from(*x));
    }
    if let Some(x) = value.try_downcast_ref::<String>() {
        return Some(Dynamic::from(x.clone()));
    }
    if let Some(x) = value.try_downcast_ref::<&'static str>() {
        return Some(Dynamic::from(*x));
    }
    if let Some(()) = value.try_downcast_ref::<()>() {
        return Some(Dynamic::UNIT);
    }
    None
}

/// A node for memory an exposed function returned a `&mut` to.
fn keep_borrowed(value: &mut dyn PartialReflect, shared: &Rc<Shared>) -> Dynamic {
    // SAFETY: the function borrowed from a node argument, so the memory
    // belongs to the program. It may move, so the node is not a root.
    let node = unsafe {
        Node::from_raw(
            std::ptr::from_mut(value),
            true,
            Some(shared.generation.get()),
            shared,
        )
    };
    present(node)
}

/// Keeps a value returned by an exposed function alive for the stop.
fn keep(value: Box<dyn PartialReflect>, shared: &Rc<Shared>) -> Dynamic {
    let mut owned = shared.owned.borrow_mut();
    owned.push(value);
    let value = owned.last_mut().expect("just pushed");
    // SAFETY: the box keeps its address until the stop ends.
    present(unsafe { Node::root(value.as_mut(), shared) })
}

macro_rules! assign_int {
    ($target:expr, $value:expr; $($t:ty),*) => {
        $(if let Some(x) = $target.try_downcast_mut::<$t>() {
            let int = $value.as_int().map_err(|_| mismatch($value, stringify!($t)))?;
            *x = <$t>::try_from(int).map_err(|_| format!("{int} is out of range for {}", stringify!($t)))?;
            return Ok(());
        })*
    };
}

fn mismatch(value: &Dynamic, target: &str) -> String {
    format!("cannot assign {} to {target}", value.type_name())
}

/// Integers as floats, exact for the `i32` range only, so nothing rounds.
fn float_from_int(value: &Dynamic) -> Option<f64> {
    i32::try_from(value.as_int().ok()?).ok().map(f64::from)
}

/// `()` becomes `None`, anything else `Some(value)`.
fn assign_option(target: &mut dyn PartialReflect, value: &Dynamic, shared: &Shared) -> Res<()> {
    use bevy_reflect::TypeInfo;
    use bevy_reflect::enums::{DynamicEnum, DynamicVariant};
    use bevy_reflect::tuple::DynamicTuple;
    let option = if value.is_unit() {
        DynamicEnum::new("None", DynamicVariant::Unit)
    } else {
        let Some(TypeInfo::Enum(info)) = target.get_represented_type_info() else {
            return Err("unknown Option type".into());
        };
        let inner = info
            .variant("Some")
            .and_then(|variant| variant.as_tuple_variant().ok())
            .and_then(|variant| variant.field_at(0))
            .ok_or("unknown Option type")?;
        let mut tuple = DynamicTuple::default();
        tuple.insert_boxed(to_owned(value, inner.type_id(), inner.type_path())?);
        DynamicEnum::new("Some", DynamicVariant::Tuple(tuple))
    };
    target.try_apply(&option).map_err(|err| err.to_string())?;
    shared.generation.set(shared.generation.get() + 1);
    Ok(())
}

/// Writes `value` into `target`.
fn assign(target: &mut dyn PartialReflect, value: &Dynamic, shared: &Shared) -> Res<()> {
    if is_option(target) && !value.is::<Node>() {
        return assign_option(target, value, shared);
    }
    if let Some(mut source) = value.clone().try_cast::<Node>() {
        if std::ptr::addr_eq(source.ptr, std::ptr::from_mut(target)) {
            return Ok(()); // Rhai writes nested results back to their parent.
        }
        target
            .try_apply(source.get()?)
            .map_err(|err| err.to_string())?;
        shared.generation.set(shared.generation.get() + 1);
        return Ok(());
    }
    assign_int!(target, value; i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, i128, u128);
    if let Some(x) = target.try_downcast_mut::<f64>() {
        *x = value
            .as_float()
            .ok()
            .or_else(|| float_from_int(value))
            .ok_or_else(|| mismatch(value, "f64"))?;
        return Ok(());
    }
    if let Some(x) = target.try_downcast_mut::<f32>() {
        #[allow(clippy::cast_possible_truncation)]
        let float = value
            .as_float()
            .ok()
            .or_else(|| float_from_int(value))
            .ok_or_else(|| mismatch(value, "f32"))? as f32;
        *x = float;
        return Ok(());
    }
    if let Some(x) = target.try_downcast_mut::<bool>() {
        *x = value.as_bool().map_err(|_| mismatch(value, "bool"))?;
        return Ok(());
    }
    if let Some(x) = target.try_downcast_mut::<char>() {
        *x = value.as_char().map_err(|_| mismatch(value, "char"))?;
        return Ok(());
    }
    if let Some(x) = target.try_downcast_mut::<String>() {
        *x = value
            .clone()
            .into_string()
            .map_err(|_| mismatch(value, "String"))?;
        return Ok(());
    }
    Err(mismatch(value, target.reflect_short_type_path()).into())
}

/// Writes `value` into the program variable behind `node`.
pub fn store(node: &mut Node, value: &Dynamic) -> Res<()> {
    let shared = Rc::clone(&node.shared);
    assign(node.get_mut()?, value, &shared)
}

/// Names reachable with `.name`, with their type.
pub fn members(value: &dyn PartialReflect) -> Vec<(String, String)> {
    let typed = |name: &str, field: &dyn PartialReflect| {
        (
            name.to_owned(),
            render::short_type(field.reflect_type_path()),
        )
    };
    match value.reflect_ref() {
        ReflectRef::Struct(s) => (0..s.field_len())
            .filter_map(|i| Some(typed(s.name_at(i)?, s.field_at(i)?)))
            .collect(),
        ReflectRef::Enum(e) => (0..e.field_len())
            .filter_map(|i| Some(typed(e.name_at(i)?, e.field_at(i)?)))
            .collect(),
        ReflectRef::Map(m) => m
            .iter()
            .filter_map(|(key, value)| {
                let key = key.try_downcast_ref::<String>()?;
                Some(typed(key, value))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn did_you_mean(name: &str, value: &dyn PartialReflect) -> String {
    let best = members(value)
        .into_iter()
        .map(|(member, _)| (distance(name, &member), member))
        .filter(|(dist, member)| *dist <= member.len().max(name.len()) / 2)
        .min();
    best.map_or_else(String::new, |(_, member)| {
        format!(". Did you mean `{member}`?")
    })
}

fn distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (i, lc) in left.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, rc) in right.iter().enumerate() {
            let next = (diagonal + usize::from(lc != *rc))
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[right.len()]
}

pub enum Step<'s> {
    Member(&'s str),
    Index(i64),
}

fn no_member(value: &dyn PartialReflect, name: &str) -> String {
    format!(
        "no field `{name}` on {}{}",
        value.reflect_short_type_path(),
        did_you_mean(name, value)
    )
}

/// The position of `index` in `value`, negative counting from the end.
fn position(value: &dyn PartialReflect, index: i64) -> Res<usize> {
    let len = match value.reflect_ref() {
        ReflectRef::List(l) => l.len(),
        ReflectRef::Array(a) => a.len(),
        ReflectRef::Tuple(t) => t.field_len(),
        ReflectRef::TupleStruct(t) => t.field_len(),
        ReflectRef::Enum(e) => e.field_len(),
        _ => return Err(format!("{} cannot be indexed", value.reflect_short_type_path()).into()),
    };
    let at = if index < 0 {
        usize::try_from(index.unsigned_abs())
            .ok()
            .and_then(|back| len.checked_sub(back))
    } else {
        usize::try_from(index).ok()
    };
    at.filter(|at| *at < len)
        .ok_or_else(|| format!("index {index} is out of bounds for length {len}").into())
}

fn locate<'a>(value: &'a dyn PartialReflect, step: &Step<'_>) -> Res<&'a dyn PartialReflect> {
    let found = match *step {
        Step::Member(name) => match value.reflect_ref() {
            ReflectRef::Struct(s) => s.field(name),
            ReflectRef::Enum(e) => e.field(name),
            ReflectRef::Map(m) => m.get(&name.to_owned()),
            _ => None,
        }
        .ok_or_else(|| no_member(value, name))?,
        Step::Index(index) => {
            let at = position(value, index)?;
            match value.reflect_ref() {
                ReflectRef::List(l) => l.get(at),
                ReflectRef::Array(a) => a.get(at),
                ReflectRef::Tuple(t) => t.field(at),
                ReflectRef::TupleStruct(t) => t.field(at),
                ReflectRef::Enum(e) => e.field_at(at),
                _ => None,
            }
            .ok_or("index out of bounds")?
        }
    };
    Ok(found)
}

fn locate_mut<'a>(
    value: &'a mut dyn PartialReflect,
    step: &Step<'_>,
) -> Res<&'a mut dyn PartialReflect> {
    // Checked on the shared path first, so errors don't need a second borrow.
    locate(value, step)?;
    let found = match *step {
        Step::Member(name) => match value.reflect_mut() {
            ReflectMut::Struct(s) => s.field_mut(name),
            ReflectMut::Enum(e) => e.field_mut(name),
            ReflectMut::Map(m) => m.get_mut(&name.to_owned()),
            _ => None,
        },
        Step::Index(index) => {
            let at = position(value, index)?;
            match value.reflect_mut() {
                ReflectMut::List(l) => l.get_mut(at),
                ReflectMut::Array(a) => a.get_mut(at),
                ReflectMut::Tuple(t) => t.field_mut(at),
                ReflectMut::TupleStruct(t) => t.field_mut(at),
                ReflectMut::Enum(e) => e.field_at_mut(at),
                _ => None,
            }
        }
    };
    found.ok_or_else(|| "not found".into())
}

/// The value one `step` below `node`, live and as writable as `node`.
pub fn step(node: &mut Node, step: &Step<'_>) -> Res<Dynamic> {
    let ptr = if node.writable {
        std::ptr::from_mut(locate_mut(node.get_mut()?, step)?)
    } else {
        std::ptr::from_ref(locate(node.get()?, step)?).cast_mut()
    };
    Ok(node.child(ptr))
}

pub fn get_member(node: &mut Node, name: &str) -> Res<Dynamic> {
    step(node, &Step::Member(name))
}

pub fn get_element(node: &mut Node, index: i64) -> Res<Dynamic> {
    step(node, &Step::Index(index))
}

fn set(node: &mut Node, step: &Step<'_>, value: &Dynamic) -> Res<()> {
    if let Some(source) = value.clone().try_cast::<Node>() {
        let target = locate(node.get()?, step)?;
        if std::ptr::addr_eq(source.ptr, std::ptr::from_ref(target)) {
            return Ok(()); // Rhai writes nested results back to their parent.
        }
    }
    let shared = Rc::clone(&node.shared);
    assign(locate_mut(node.get_mut()?, step)?, value, &shared)
}

/// The elements of a list-like value, or the values of a map.
pub fn elements(node: &mut Node) -> Res<Vec<Dynamic>> {
    let value = node.get()?;
    let count = match value.reflect_ref() {
        ReflectRef::List(l) => l.len(),
        ReflectRef::Array(a) => a.len(),
        ReflectRef::Set(s) => return Ok(s.iter().filter_map(leaf).collect()),
        ReflectRef::Map(m) => {
            let keys: Vec<Box<dyn PartialReflect>> = m
                .iter()
                .filter_map(|(key, _)| key.to_dynamic().ok())
                .collect();
            let mut out = Vec::new();
            for key in keys {
                let ptr = if node.writable {
                    let ReflectMut::Map(m) = node.get_mut()?.reflect_mut() else {
                        unreachable!()
                    };
                    m.get_mut(key.as_ref()).map(std::ptr::from_mut)
                } else {
                    let ReflectRef::Map(m) = node.get()?.reflect_ref() else {
                        unreachable!()
                    };
                    m.get(key.as_ref())
                        .map(|v| std::ptr::from_ref(v).cast_mut())
                };
                out.extend(ptr.map(|ptr| node.child(ptr)));
            }
            return Ok(out);
        }
        _ => return Err(format!("{} is not a collection", value.reflect_short_type_path()).into()),
    };
    (0..count)
        .map(|i| get_element(node, i64::try_from(i).unwrap_or(i64::MAX)))
        .collect()
}

fn len(node: &mut Node) -> Res<i64> {
    let value = node.get()?;
    let len = match value.reflect_ref() {
        ReflectRef::List(l) => l.len(),
        ReflectRef::Array(a) => a.len(),
        ReflectRef::Map(m) => m.len(),
        ReflectRef::Set(s) => s.len(),
        ReflectRef::Tuple(t) => t.field_len(),
        _ => return Err(format!("{} has no length", value.reflect_short_type_path()).into()),
    };
    Ok(i64::try_from(len).unwrap_or(i64::MAX))
}

impl IntoIterator for Node {
    type Item = Dynamic;
    type IntoIter = std::vec::IntoIter<Dynamic>;

    fn into_iter(mut self) -> Self::IntoIter {
        elements(&mut self).unwrap_or_default().into_iter()
    }
}

macro_rules! dynamic_to_owned {
    ($value:expr, $ty:expr; $($t:ty),*) => {
        $(if $ty == TypeId::of::<$t>() {
            let int = $value.as_int().map_err(|_| mismatch($value, stringify!($t)))?;
            let x = <$t>::try_from(int).map_err(|_| format!("{int} is out of range for {}", stringify!($t)))?;
            return Ok(Box::new(x));
        })*
    };
}

fn to_owned(value: &Dynamic, ty: TypeId, type_path: &str) -> Res<Box<dyn PartialReflect>> {
    dynamic_to_owned!(value, ty; i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, i128, u128);
    if ty == TypeId::of::<f64>() {
        return Ok(Box::new(
            value
                .as_float()
                .ok()
                .or_else(|| float_from_int(value))
                .ok_or_else(|| mismatch(value, "f64"))?,
        ));
    }
    if ty == TypeId::of::<bool>() {
        return Ok(Box::new(
            value.as_bool().map_err(|_| mismatch(value, "bool"))?,
        ));
    }
    if ty == TypeId::of::<String>() {
        return Ok(Box::new(
            value
                .clone()
                .into_string()
                .map_err(|_| mismatch(value, "String"))?,
        ));
    }
    if let Some(mut node) = value.clone().try_cast::<Node>() {
        return node
            .get()?
            .reflect_clone()
            .map(<dyn bevy_reflect::Reflect>::into_partial_reflect)
            .map_err(|err| err.to_string().into());
    }
    Err(mismatch(value, type_path).into())
}

/// Calls an exposed function with Rhai arguments.
fn call(
    function: &bevy_reflect::func::DynamicFunction<'static>,
    args: &[&mut Dynamic],
) -> Res<Dynamic> {
    use bevy_reflect::func::args::Ownership;
    use bevy_reflect::func::{ArgList, Return};
    let signature = function.info().base();
    if signature.arg_count() != args.len() {
        return Err(format!(
            "expects {} arguments, got {}",
            signature.arg_count(),
            args.len()
        )
        .into());
    }
    let mut nodes: Vec<Option<Node>> = args
        .iter()
        .map(|arg| (**arg).clone().try_cast::<Node>())
        .collect();
    let shared = nodes
        .iter()
        .flatten()
        .next()
        .map(|node| Rc::clone(&node.shared));
    let mut list = ArgList::new();
    for ((info, arg), node) in signature.args().iter().zip(args).zip(&mut nodes) {
        match (info.ownership(), node) {
            (Ownership::Ref, Some(node)) => list.push_ref(node.get()?),
            (Ownership::Mut, Some(node)) => list.push_mut(node.get_mut()?),
            (Ownership::Owned, _) => {
                list.push_boxed(to_owned(arg, info.type_id(), info.type_path())?);
            }
            _ => {
                return Err(format!(
                    "expects a {} argument, got {}",
                    info.type_path(),
                    arg.type_name()
                )
                .into());
            }
        }
    }
    let copy =
        |value: &dyn PartialReflect| leaf(value).unwrap_or_else(|| render::flat(value).into());
    Ok(
        match (function.call(list).map_err(|err| err.to_string())?, &shared) {
            (Return::Owned(value), Some(shared)) => keep(value, shared),
            (Return::Mut(value), Some(shared)) => keep_borrowed(value, shared),
            (Return::Owned(value), None) => copy(value.as_ref()),
            (Return::Ref(value), _) => copy(value),
            (Return::Mut(value), None) => copy(value),
        },
    )
}

fn map(context: &NativeCallContext, node: &mut Node, function: &FnPtr) -> Res<rhai::Array> {
    elements(node)?
        .into_iter()
        .map(|item| function.call_within_context(context, (item,)))
        .collect()
}

fn filter(context: &NativeCallContext, node: &mut Node, function: &FnPtr) -> Res<rhai::Array> {
    let mut out = Vec::new();
    for item in elements(node)? {
        if function.call_within_context::<bool>(context, (item.clone(),))? {
            out.push(item);
        }
    }
    Ok(out)
}

/// Registers `Node` and every exposed function with `engine`.
pub fn register(engine: &mut Engine) {
    engine
        .register_type_with_name::<Node>("value")
        .register_indexer_get(get_member)
        .register_indexer_get(get_element)
        .register_indexer_set(|node: &mut Node, name: &str, value: Dynamic| {
            set(node, &Step::Member(name), &value)
        })
        .register_indexer_set(|node: &mut Node, index: i64, value: Dynamic| {
            set(node, &Step::Index(index), &value)
        })
        .register_iterator::<Node>()
        .register_fn("len", len)
        .register_fn("to_array", |node: &mut Node| elements(node))
        .register_fn(
            "map",
            |context: NativeCallContext, node: &mut Node, function: FnPtr| {
                map(&context, node, &function)
            },
        )
        .register_fn(
            "filter",
            |context: NativeCallContext, node: &mut Node, function: FnPtr| {
                filter(&context, node, &function)
            },
        )
        .register_fn("keys", |node: &mut Node| -> Res<rhai::Array> {
            Ok(members(node.get()?)
                .into_iter()
                .map(|(name, _)| name.into())
                .collect())
        })
        .register_fn("variant", |node: &mut Node| -> Res<String> {
            match node.get()?.reflect_ref() {
                ReflectRef::Enum(e) => Ok(e.variant_name().to_owned()),
                _ => Err("not an enum".into()),
            }
        })
        .register_fn("type_name", |node: &mut Node| -> Res<String> {
            Ok(render::short_type(node.get()?.reflect_type_path()))
        })
        .register_fn("to_string", |node: &mut Node| -> Res<String> {
            Ok(render::flat(node.get()?))
        })
        .register_fn("to_debug", |node: &mut Node| -> Res<String> {
            Ok(render::flat(node.get()?))
        });

    let functions = crate::FUNCTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (name, function) in functions.iter() {
        let function = function.clone();
        let arity = function.info().base().arg_count();
        engine.register_raw_fn(
            name.as_str(),
            vec![TypeId::of::<Dynamic>(); arity],
            move |_: NativeCallContext, args: &mut [&mut Dynamic]| call(&function, args),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_reflect::Reflect;

    #[derive(Reflect)]
    struct Bag {
        items: Vec<(u32,)>,
    }

    #[test]
    fn writes_that_move_memory_invalidate_nodes() {
        let shared = Rc::new(Shared::default());
        let mut small = Bag { items: vec![(1,)] };
        let mut large = Bag {
            items: vec![(1,), (2,), (3,)],
        };
        // SAFETY: both bags outlive the nodes.
        let mut target = unsafe { Node::root(&mut small, &shared) };
        let source = unsafe { Node::root(&mut large, &shared) };

        let mut items = get_member(&mut target, "items").unwrap().cast::<Node>();
        let mut first = get_element(&mut items, 0).unwrap().cast::<Node>();
        store(&mut target, &Dynamic::from(source)).unwrap();

        let err = first.get().map(|_| ()).unwrap_err().to_string();
        assert!(err.contains("stale reference"), "{err}");
        let mut fresh = get_member(&mut target, "items").unwrap().cast::<Node>();
        assert_eq!(len(&mut fresh).unwrap(), 3);
        drop((target, items, fresh));
        assert_eq!(small.items.len(), 3);
    }
}
