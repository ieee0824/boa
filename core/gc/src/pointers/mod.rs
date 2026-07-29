//! Pointers represents the External types returned by the Boa Garbage Collector

mod ephemeron;
mod edge;
mod gc;
mod rooted;
mod weak;
mod weak_map;

pub use ephemeron::Ephemeron;
pub use edge::GcEdge;
pub use gc::{Gc, GcErased};
pub use rooted::Rooted;
pub use weak::WeakGc;
pub use weak_map::WeakMap;

pub(crate) use gc::NonTraceable;
pub(crate) use weak_map::RawWeakMap;
