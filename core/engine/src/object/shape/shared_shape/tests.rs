use crate::{JsObject, JsSymbol, object::shape::slot::SlotAttributes, property::PropertyKey};

use super::{SharedShape, TransitionKey};

#[test]
fn test_prune_property_on_counter_limit() {
    let shape = SharedShape::root();

    for i in 0..255 {
        assert_eq!(
            shape.forward_transitions().property_transitions_count(),
            (i, i as u8)
        );

        shape.insert_property_transition(TransitionKey {
            property_key: PropertyKey::Symbol(JsSymbol::new(None).unwrap()),
            attributes: SlotAttributes::all(),
        });
    }

    assert_eq!(
        shape.forward_transitions().property_transitions_count(),
        (255, 255)
    );

    boa_gc::force_collect();

    {
        shape.insert_property_transition(TransitionKey {
            property_key: PropertyKey::Symbol(JsSymbol::new(None).unwrap()),
            attributes: SlotAttributes::all(),
        });
    }

    assert_eq!(
        shape.forward_transitions().property_transitions_count(),
        (1, 0)
    );

    {
        shape.insert_property_transition(TransitionKey {
            property_key: PropertyKey::Symbol(JsSymbol::new(None).unwrap()),
            attributes: SlotAttributes::all(),
        });
    }

    assert_eq!(
        shape.forward_transitions().property_transitions_count(),
        (2, 1)
    );

    boa_gc::force_collect();

    assert_eq!(
        shape.forward_transitions().property_transitions_count(),
        (2, 1)
    );
}

#[test]
fn test_prune_prototype_on_counter_limit() {
    let shape = SharedShape::root();

    assert_eq!(
        shape.forward_transitions().prototype_transitions_count(),
        (0, 0)
    );

    // Keep lookup identities distinct even if an automatic collection runs
    // during the loop; the weak transition targets can still be reclaimed.
    let prototypes: Vec<_> = (0..255)
        .map(|_| JsObject::with_null_proto().root())
        .collect();
    for (i, prototype) in prototypes.iter().enumerate() {
        assert_eq!(
            shape.forward_transitions().prototype_transitions_count(),
            (i, i as u8)
        );

        shape.change_prototype_transition(Some(prototype.to_edge()));
    }

    boa_gc::force_collect();

    assert_eq!(
        shape.forward_transitions().prototype_transitions_count(),
        (255, 255)
    );

    {
        shape.change_prototype_transition(Some(JsObject::with_null_proto()));
    }

    assert_eq!(
        shape.forward_transitions().prototype_transitions_count(),
        (1, 0)
    );

    {
        shape.change_prototype_transition(Some(JsObject::with_null_proto()));
    }

    assert_eq!(
        shape.forward_transitions().prototype_transitions_count(),
        (2, 1)
    );

    boa_gc::force_collect();

    assert_eq!(
        shape.forward_transitions().prototype_transitions_count(),
        (2, 1)
    );
}

#[test]
fn prototype_transition_cache_keeps_only_live_shape_prototypes() {
    use boa_gc::{Rooted, WeakGcEdge};

    let root = SharedShape::root();
    let prototype = JsObject::with_null_proto().root();
    let weak = Rooted::new(WeakGcEdge::new_rooted(&prototype.root_inner()));
    let transition = root.change_prototype_transition(Some(prototype.to_edge()));
    let cached = root.change_prototype_transition(Some(prototype.to_edge()));
    assert!(Rooted::ptr_eq(&transition.inner, &cached.inner));
    drop(cached);
    drop(prototype);
    boa_gc::force_collect();
    assert!(
        weak.is_upgradable(),
        "a live shape keeps its actual prototype alive"
    );
    drop(transition);
    boa_gc::force_collect();
    assert!(
        !weak.is_upgradable(),
        "a cached weak transition must not retain its prototype"
    );

    for _ in 0..32 {
        let prototype = JsObject::with_null_proto().root();
        let transition = root.change_prototype_transition(Some(prototype.to_edge()));
        assert_eq!(transition.prototype(), Some(prototype.to_edge()));
        drop(transition);
        drop(prototype);
        boa_gc::force_collect();
    }
}
