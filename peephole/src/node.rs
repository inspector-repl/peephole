//! Live views into program memory for Rhai scripts.

use crate::render;
use bevy_reflect::{PartialReflect, ReflectMut, ReflectRef, TypeInfo};
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

/// A value in the stopped program. Scripts work on its memory, never on copies.
#[derive(Clone)]
pub struct Node {
    ptr: *mut dyn PartialReflect,
    /// False for values `inspect!` got as `&`.
    writable: bool,
    /// The generation it was created in, `None` for values that never move.
    born: Option<u64>,
    shared: Rc<Shared>,
}

impl Node {
    /// # Safety
    /// `ptr` must stay valid while any clone of the node exists, and come
    /// from a `&mut` if `writable`. The session drops all nodes before
    /// `inspect!` returns.
    unsafe fn new(
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

    /// A node for a value that keeps its address during the stop.
    ///
    /// # Safety
    /// See [`Node::new`].
    pub unsafe fn root(value: &mut dyn PartialReflect, shared: &Rc<Shared>) -> Self {
        unsafe { Self::new(std::ptr::from_mut(value), true, None, shared) }
    }

    /// # Safety
    /// See [`Node::new`].
    pub unsafe fn root_shared(value: &dyn PartialReflect, shared: &Rc<Shared>) -> Self {
        unsafe { Self::new(std::ptr::from_ref(value).cast_mut(), false, None, shared) }
    }

    /// A node for a value that a write to a container may move.
    ///
    /// # Safety
    /// See [`Node::new`].
    unsafe fn moving(ptr: *mut dyn PartialReflect, writable: bool, shared: &Rc<Shared>) -> Self {
        unsafe { Self::new(ptr, writable, Some(shared.generation.get()), shared) }
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
        // SAFETY: valid per `new`, and `check` rules out moves.
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
}

fn is_option(value: &dyn PartialReflect) -> bool {
    value
        .reflect_type_path()
        .starts_with("core::option::Option<")
}

/// How scripts see a node: leaves as Rhai values, `Some(x)` as `x`,
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

macro_rules! leaf_to_dynamic {
    ($value:expr; lossless: $($small:ty),*; checked: $($big:ty),*) => {
        $(if let Some(x) = $value.try_downcast_ref::<$small>() { return Some(Dynamic::from(i64::from(*x))); })*
        $(if let Some(x) = $value.try_downcast_ref::<$big>() {
            return i64::try_from(*x).ok().map(Dynamic::from);
        })*
    };
}

/// Numbers, strings and the like as Rhai values.
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

/// Keeps a value returned by an exposed function alive for the stop.
fn keep(value: Box<dyn PartialReflect>, shared: &Rc<Shared>) -> Dynamic {
    let mut owned = shared.owned.borrow_mut();
    owned.push(value);
    let value = owned.last_mut().expect("just pushed");
    // SAFETY: the box keeps its address until the stop ends.
    present(unsafe { Node::root(value.as_mut(), shared) })
}

fn mismatch(value: &Dynamic, target: &str) -> String {
    format!("cannot assign {} to {target}", value.type_name())
}

/// Exact for the `i32` range only, so nothing rounds.
fn float_from_int(value: &Dynamic) -> Option<f64> {
    i32::try_from(value.as_int().ok()?).ok().map(f64::from)
}

fn float(value: &Dynamic, target: &str) -> Res<f64> {
    Ok(value
        .as_float()
        .ok()
        .or_else(|| float_from_int(value))
        .ok_or_else(|| mismatch(value, target))?)
}

macro_rules! convert_int {
    ($value:expr, $ty:expr; $($t:ty),*) => {
        $(if $ty == TypeId::of::<$t>() {
            let int = $value.as_int().map_err(|_| mismatch($value, stringify!($t)))?;
            let x = <$t>::try_from(int).map_err(|_| format!("{int} is out of range for {}", stringify!($t)))?;
            return Ok(Box::new(x));
        })*
    };
}

/// `value` as the Rust type `ty`.
fn convert(value: &Dynamic, ty: TypeId, type_path: &str) -> Res<Box<dyn PartialReflect>> {
    convert_int!(value, ty; i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, i128, u128);
    if ty == TypeId::of::<f64>() {
        return Ok(Box::new(float(value, "f64")?));
    }
    if ty == TypeId::of::<f32>() {
        #[allow(clippy::cast_possible_truncation)]
        return Ok(Box::new(float(value, "f32")? as f32));
    }
    if ty == TypeId::of::<bool>() {
        return Ok(Box::new(
            value.as_bool().map_err(|_| mismatch(value, "bool"))?,
        ));
    }
    if ty == TypeId::of::<char>() {
        return Ok(Box::new(
            value.as_char().map_err(|_| mismatch(value, "char"))?,
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

/// `()` as `None`, anything else as `Some(value)`.
fn convert_option(info: &TypeInfo, value: &Dynamic) -> Res<Box<dyn PartialReflect>> {
    use bevy_reflect::enums::{DynamicEnum, DynamicVariant};
    use bevy_reflect::tuple::DynamicTuple;
    if value.is_unit() {
        return Ok(Box::new(DynamicEnum::new("None", DynamicVariant::Unit)));
    }
    let inner = info
        .as_enum()
        .ok()
        .and_then(|info| info.variant("Some"))
        .and_then(|variant| variant.as_tuple_variant().ok())
        .and_then(|variant| variant.field_at(0))
        .ok_or("unknown Option type")?;
    let mut tuple = DynamicTuple::default();
    tuple.insert_boxed(convert(value, inner.type_id(), inner.type_path())?);
    Ok(Box::new(DynamicEnum::new(
        "Some",
        DynamicVariant::Tuple(tuple),
    )))
}

pub fn store(node: &mut Node, value: &Dynamic) -> Res<()> {
    if let Some(source) = value.clone().try_cast::<Node>()
        && std::ptr::addr_eq(source.ptr, node.ptr)
    {
        // Rhai writes properties back after calling methods on them.
        return Ok(());
    }
    let shared = Rc::clone(&node.shared);
    let target = node.get_mut()?;
    let info = target
        .get_represented_type_info()
        .ok_or_else(|| mismatch(value, target.reflect_short_type_path()))?;
    let new = if is_option(target) && !value.is::<Node>() {
        convert_option(info, value)?
    } else {
        convert(value, info.type_id(), info.type_path())?
    };
    // Leaves own no memory that other nodes could point into.
    let moves = leaf(target).is_none();
    target
        .try_apply(new.as_ref())
        .map_err(|err| err.to_string())?;
    if moves {
        shared.generation.set(shared.generation.get() + 1);
    }
    Ok(())
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
    // Errors come from the shared path, which needs no second borrow.
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

fn child(node: &mut Node, step: &Step<'_>) -> Res<Node> {
    let ptr = if node.writable {
        std::ptr::from_mut(locate_mut(node.get_mut()?, step)?)
    } else {
        std::ptr::from_ref(locate(node.get()?, step)?).cast_mut()
    };
    // SAFETY: values inside a node live as long as the node.
    Ok(unsafe { Node::moving(ptr, node.writable, &node.shared) })
}

/// The value one `step` below `node`, live and as writable as `node`.
pub fn step(node: &mut Node, step: &Step<'_>) -> Res<Dynamic> {
    child(node, step).map(present)
}

fn set(node: &mut Node, step: &Step<'_>, value: &Dynamic) -> Res<()> {
    store(&mut child(node, step)?, value)
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
                // SAFETY: as in `child`.
                out.extend(
                    ptr.map(|ptr| {
                        present(unsafe { Node::moving(ptr, node.writable, &node.shared) })
                    }),
                );
            }
            return Ok(out);
        }
        _ => return Err(format!("{} is not a collection", value.reflect_short_type_path()).into()),
    };
    (0..count)
        .map(|i| step(node, &Step::Index(i64::try_from(i).unwrap_or(i64::MAX))))
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
                list.push_boxed(convert(arg, info.type_id(), info.type_path())?);
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
            // SAFETY: borrowed from a node argument, so it is program memory.
            (Return::Mut(value), Some(shared)) => {
                present(unsafe { Node::moving(std::ptr::from_mut(value), true, shared) })
            }
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

pub fn register(engine: &mut Engine) {
    engine
        .register_type_with_name::<Node>("value")
        .register_indexer_get(|node: &mut Node, name: &str| step(node, &Step::Member(name)))
        .register_indexer_get(|node: &mut Node, index: i64| step(node, &Step::Index(index)))
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

        let mut items = child(&mut target, &Step::Member("items")).unwrap();
        let mut first = child(&mut items, &Step::Index(0)).unwrap();
        store(&mut target, &Dynamic::from(source)).unwrap();

        let err = first.get().map(|_| ()).unwrap_err().to_string();
        assert!(err.contains("stale reference"), "{err}");
        let mut fresh = child(&mut target, &Step::Member("items")).unwrap();
        assert_eq!(len(&mut fresh).unwrap(), 3);
        drop((target, items, fresh));
        assert_eq!(small.items.len(), 3);
    }
}
