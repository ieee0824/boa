//! Boa's implementation of ECMAScript's `Realm Records`
//!
//! Conceptually, a realm consists of a set of intrinsic objects, an ECMAScript global environment,
//! all of the ECMAScript code that is loaded within the scope of that global environment,
//! and other associated state and resources.
//!
//! A realm is represented in this implementation as a Realm struct with the fields specified from the spec.

use std::any::TypeId;

use crate::{
    Context, HostDefined, JsNativeError, JsObject, JsResult, JsString,
    class::Class,
    context::{
        HostHooks,
        intrinsics::{Intrinsics, StandardConstructor},
    },
    environments::DeclarativeEnvironment,
    module::ModuleEdge,
    object::shape::RootShape,
};
use boa_ast::scope::Scope;
use boa_engine::JsValue;
use boa_engine::property::{Attribute, PropertyDescriptor, PropertyKey};
use boa_gc::{Finalize, GcEdge, GcRef, GcRefCell, GcRefMut, Rooted, Trace};
use rustc_hash::FxHashMap;

/// Representation of a Realm.
///
/// In the specification these are called Realm Records.
#[doc(hidden)]
pub trait RealmHandle: Clone + std::ops::Deref<Target = RealmInner> {
    fn as_inner(&self) -> &RealmInner;
}

impl RealmHandle for Rooted<RealmInner> {
    fn as_inner(&self) -> &RealmInner {
        self
    }
}

impl RealmHandle for GcEdge<RealmInner> {
    fn as_inner(&self) -> &RealmInner {
        self
    }
}

/// Representation of a Realm.
///
/// The default handle is an explicit native root. Heap-owned records use an
/// internal edge specialization.
#[derive(Clone)]
pub struct Realm<H: RealmHandle = Rooted<RealmInner>> {
    inner: H,
}

pub(crate) type RealmEdge = Realm<GcEdge<RealmInner>>;

impl Finalize for RealmEdge {}

// SAFETY: Heap-owned realms contain only an explicitly traced GC edge.
unsafe impl Trace for RealmEdge {
    boa_gc::custom_trace!(this, mark, {
        mark(&this.inner);
    });
}

impl<H: RealmHandle> Eq for Realm<H> {}

impl<H: RealmHandle> PartialEq for Realm<H> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.inner.as_inner(), other.inner.as_inner())
    }
}

impl<H: RealmHandle> std::fmt::Debug for Realm<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Realm")
            .field("intrinsics", &self.inner.intrinsics)
            .field("environment", &self.inner.environment)
            .field("global_object", &self.inner.global_object)
            .field("global_this", &self.inner.global_this)
            .finish()
    }
}

#[doc(hidden)]
#[derive(Trace, Finalize)]
pub struct RealmInner {
    intrinsics: Intrinsics,

    /// The global declarative environment of this realm.
    environment: GcEdge<DeclarativeEnvironment>,

    /// The global scope of this realm.
    /// This is directly related to the global declarative environment.
    // Safety: Nothing in `Scope` needs tracing.
    #[unsafe_ignore_trace]
    scope: Scope,

    global_object: JsObject,
    global_this: JsObject,
    template_map: GcRefCell<FxHashMap<u64, JsObject>>,
    loaded_modules: GcRefCell<FxHashMap<JsString, ModuleEdge>>,
    host_classes: GcRefCell<FxHashMap<TypeId, StandardConstructor>>,

    host_defined: GcRefCell<HostDefined>,
}

impl std::fmt::Debug for RealmInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealmInner")
            .field("intrinsics", &self.intrinsics)
            .field("environment", &self.environment)
            .field("global_object", &self.global_object)
            .field("global_this", &self.global_this)
            .finish_non_exhaustive()
    }
}

impl Realm {
    /// Create a new [`Realm`].
    #[inline]
    pub fn create(hooks: &dyn HostHooks, root_shape: &RootShape) -> JsResult<Self> {
        // Bootstrap holds what it builds in locals and only links it into the heap at the
        // end: the intrinsics, global object and global `this` until `RealmInner` is
        // allocated as a root, and then each builtin's constructor, prototype and property
        // storage until `initialize` installs them. A collection in either window would
        // see no root for any of it. Creating a realm allocates a bounded amount, so
        // suspending collection across it is the cheapest correct answer.
        let _no_gc = boa_gc::NoGcScope::new();

        let realm = {
            let intrinsics = Intrinsics::uninit(root_shape).ok_or_else(|| {
                JsNativeError::typ().with_message("failed to create the realm intrinsics")
            })?;

            let global_object = hooks.create_global_object(&intrinsics);
            let global_this = hooks
                .create_global_this(&intrinsics)
                .unwrap_or_else(|| global_object.clone());
            let environment = GcEdge::new(DeclarativeEnvironment::global());
            let scope = Scope::new_global();

            Self {
                inner: Rooted::new(RealmInner {
                    intrinsics,
                    environment,
                    scope,
                    global_object,
                    global_this,
                    template_map: GcRefCell::default(),
                    loaded_modules: GcRefCell::default(),
                    host_classes: GcRefCell::default(),
                    host_defined: GcRefCell::default(),
                }),
            }
        };

        realm.initialize();

        Ok(realm)
    }

    pub(crate) fn to_edge(&self) -> RealmEdge {
        RealmEdge {
            inner: self.inner.clone().into_edge(),
        }
    }
}

impl<H: RealmHandle> Realm<H> {
    /// Gets the intrinsics of this `Realm`.
    #[inline]
    #[must_use]
    pub fn intrinsics(&self) -> &Intrinsics {
        &self.inner.intrinsics
    }

    /// Returns an immutable reference to the [`ECMAScript specification`][spec] defined
    /// [`\[\[\HostDefined]\]`][`HostDefined`] field of the [`Realm`].
    ///
    /// [spec]: https://tc39.es/ecma262/#table-realm-record-fields
    ///
    /// # Panics
    ///
    /// Panics if [`HostDefined`] field is mutably borrowed.
    #[inline]
    #[must_use]
    pub fn host_defined(&self) -> GcRef<'_, HostDefined> {
        self.inner.host_defined.borrow()
    }

    /// Returns a mutable reference to [`ECMAScript specification`][spec] defined
    /// [`\[\[\HostDefined]\]`][`HostDefined`] field of the [`Realm`].
    ///
    /// [spec]: https://tc39.es/ecma262/#table-realm-record-fields
    ///
    /// # Panics
    ///
    /// Panics if [`HostDefined`] field is borrowed.
    #[inline]
    #[must_use]
    pub fn host_defined_mut(&self) -> GcRefMut<'_, HostDefined> {
        self.inner.host_defined.borrow_mut()
    }

    /// Checks if this `Realm` has the class `C` registered into its class map.
    #[must_use]
    pub fn has_class<C: Class>(&self) -> bool {
        self.inner
            .host_classes
            .borrow()
            .contains_key(&TypeId::of::<C>())
    }

    /// Gets the constructor and prototype of the class `C` if it is registered in the class map.
    #[must_use]
    pub fn get_class<C: Class>(&self) -> Option<StandardConstructor> {
        self.inner
            .host_classes
            .borrow()
            .get(&TypeId::of::<C>())
            .cloned()
    }

    pub(crate) fn environment(&self) -> Rooted<DeclarativeEnvironment> {
        self.inner.environment.clone().root()
    }

    /// Returns the scope of this realm.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.inner.scope
    }

    pub(crate) fn global_object(&self) -> &JsObject {
        &self.inner.global_object
    }

    pub(crate) fn global_this(&self) -> &JsObject {
        &self.inner.global_this
    }

    pub(crate) fn loaded_modules(&self) -> &GcRefCell<FxHashMap<JsString, ModuleEdge>> {
        &self.inner.loaded_modules
    }

    /// Resizes the number of bindings on the global environment.
    pub(crate) fn resize_global_env(&self) {
        let binding_number = self.scope().num_bindings();
        let environment = self.environment();
        let env = environment
            .kind()
            .as_global()
            .expect("Realm should only store global environments")
            .poisonable_environment();
        let mut bindings = env.bindings().borrow_mut();

        if bindings.len() < binding_number as usize {
            bindings.resize(binding_number as usize, None);
        }
    }

    pub(crate) fn push_template(&self, site: u64, template: JsObject) {
        self.inner.template_map.borrow_mut().insert(site, template);
    }

    pub(crate) fn lookup_template(&self, site: u64) -> Option<JsObject> {
        self.inner.template_map.borrow().get(&site).cloned()
    }

    /// Register a property on the global object of this realm.
    ///
    /// It will return an error if the property is already defined.
    pub fn register_property<K, V>(
        &self,
        key: K,
        value: V,
        attribute: Attribute,
        context: &mut Context,
    ) -> JsResult<()>
    where
        K: Into<PropertyKey>,
        V: Into<JsValue>,
    {
        self.global_object().define_property_or_throw(
            key,
            PropertyDescriptor::builder()
                .value(value)
                .writable(attribute.writable())
                .enumerable(attribute.enumerable())
                .configurable(attribute.configurable()),
            context,
        )?;
        Ok(())
    }

    /// Register a class `C` in this realm.
    pub fn register_class<C: Class>(&self, spec: StandardConstructor) {
        self.inner
            .host_classes
            .borrow_mut()
            .insert(TypeId::of::<C>(), spec);
    }

    /// Unregister a class `C` in this realm.
    #[must_use]
    pub fn unregister_class<C: Class>(&self) -> Option<StandardConstructor> {
        self.inner
            .host_classes
            .borrow_mut()
            .remove(&TypeId::of::<C>())
    }

    pub(crate) fn addr(&self) -> *const () {
        let ptr: *const _ = self.inner.as_inner();
        ptr.cast()
    }
}

impl RealmEdge {
    pub(crate) fn to_rooted(&self) -> Realm {
        Realm {
            inner: self.inner.clone().root(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Context;

    #[test]
    fn global_environment_root_survives_forced_collection() {
        let environment = {
            let context = Context::default();
            context.realm().environment()
        };

        boa_gc::force_collect();

        assert!(environment.kind().as_global().is_some());
    }
}
