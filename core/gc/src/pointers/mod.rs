//! Pointers represents the External types returned by the Boa Garbage Collector

mod edge;
mod ephemeron;
mod gc;
mod rooted;
mod weak;
mod weak_map;

pub use edge::GcEdge;
pub use ephemeron::{Ephemeron, EphemeronEdge};
pub use gc::{Gc, GcErased, GcErasedEdge};
pub use rooted::Rooted;
pub use weak::{WeakGc, WeakGcEdge};
pub use weak_map::{WeakMap, WeakMapEdge};

pub(crate) use gc::NonTraceable;
pub(crate) use weak_map::RawWeakMap;
