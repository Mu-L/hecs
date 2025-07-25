use crate::alloc::{vec, vec::Vec};
use core::any::TypeId;
use core::borrow::Borrow;
use core::convert::TryFrom;
use core::hash::{BuildHasherDefault, Hasher};
use spin::Mutex;

use core::{fmt, ptr};

#[cfg(feature = "std")]
use std::error::Error;

use hashbrown::hash_map::{Entry, HashMap};

use crate::alloc::boxed::Box;
use crate::archetype::{Archetype, TypeIdMap, TypeInfo};
use crate::entities::{Entities, EntityMeta, Location, ReserveEntitiesIterator};
#[cfg(feature = "std")]
use crate::query::QueryCache;
use crate::query::{assert_borrow, assert_distinct, CachedQuery};
use crate::{
    Bundle, ColumnBatch, ComponentRef, DynamicBundle, Entity, EntityRef, Fetch, MissingComponent,
    NoSuchEntity, Query, QueryBorrow, QueryMut, QueryOne, TakenEntity, View, ViewBorrow,
};

/// An unordered collection of entities, each having any number of distinctly typed components
///
/// Similar to `HashMap<Entity, Vec<Box<dyn Any>>>` where each `Vec` never contains two of the same
/// type, but far more efficient to traverse.
///
/// The components of entities who have the same set of component types are stored in contiguous
/// runs, allowing for extremely fast, cache-friendly iteration.
///
/// There is a maximum number of unique entity IDs, which means that there is a maximum number of live
/// entities. When old entities are despawned, their IDs will be reused on a future entity, and
/// old `Entity` values with that ID will be invalidated.
///
/// ### Collisions
///
/// If an entity is despawned and its `Entity` handle is preserved over the course of billions of
/// following spawns and despawns, that handle may, in rare circumstances, collide with a
/// newly-allocated `Entity` handle. Very long-lived applications should therefore limit the period
/// over which they may retain handles of despawned entities.
pub struct World {
    entities: Entities,
    archetypes: ArchetypeSet,
    /// Maps statically-typed bundle types to archetypes
    bundle_to_archetype: TypeIdMap<u32>,
    /// Maps source archetype and static bundle types to the archetype that an entity is moved to
    /// after inserting the components from that bundle.
    insert_edges: IndexTypeIdMap<InsertTarget>,
    /// Maps source archetype and static bundle types to the archetype that an entity is moved to
    /// after removing the components from that bundle.
    remove_edges: IndexTypeIdMap<u32>,
    #[cfg(feature = "std")]
    query_cache: QueryCache,
    id: u64,
}

impl World {
    /// Create an empty world
    pub fn new() -> Self {
        // AtomicU64 is unsupported on 32-bit MIPS and PPC architectures
        // For compatibility, use Mutex<u64>
        static ID: Mutex<u64> = Mutex::new(1);
        let id = {
            let mut id = ID.lock();
            let next = id.checked_add(1).unwrap();
            *id = next;
            next
        };
        Self {
            entities: Entities::default(),
            archetypes: ArchetypeSet::new(),
            bundle_to_archetype: HashMap::default(),
            insert_edges: HashMap::default(),
            remove_edges: HashMap::default(),
            #[cfg(feature = "std")]
            query_cache: QueryCache::default(),
            id,
        }
    }

    /// Create an entity with certain components
    ///
    /// Returns the ID of the newly created entity.
    ///
    /// Arguments can be tuples, structs annotated with [`#[derive(Bundle)]`](macro@Bundle), or the
    /// result of calling [`build`](crate::EntityBuilder::build) on an
    /// [`EntityBuilder`](crate::EntityBuilder), which is useful if the set of components isn't
    /// statically known. To spawn an entity with only one component, use a one-element tuple like
    /// `(x,)`.
    ///
    /// Any type that satisfies `Send + Sync + 'static` can be used as a component.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn((123, "abc"));
    /// let b = world.spawn((456, true));
    /// ```
    pub fn spawn(&mut self, components: impl DynamicBundle) -> Entity {
        // Ensure all entity allocations are accounted for so `self.entities` can realloc if
        // necessary
        self.flush();

        let entity = self.entities.alloc();

        self.spawn_inner(entity, components);

        entity
    }

    /// Create an entity with certain components and a specific [`Entity`] handle.
    ///
    /// See [`spawn`](Self::spawn).
    ///
    /// Despawns any existing entity with the same [`Entity::id`].
    ///
    /// Useful for easy handle-preserving deserialization. Be cautious resurrecting old `Entity`
    /// handles in already-populated worlds as it vastly increases the likelihood of collisions.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn((123, "abc"));
    /// let b = world.spawn((456, true));
    /// world.despawn(a);
    /// assert!(!world.contains(a));
    /// // all previous Entity values pointing to 'a' will be live again, instead pointing to the new entity.
    /// world.spawn_at(a, (789, "ABC"));
    /// assert!(world.contains(a));
    /// ```
    pub fn spawn_at(&mut self, handle: Entity, components: impl DynamicBundle) {
        // Ensure all entity allocations are accounted for so `self.entities` can realloc if
        // necessary
        self.flush();

        let loc = self.entities.alloc_at(handle);
        if let Some(loc) = loc {
            if let Some(moved) = unsafe {
                self.archetypes.archetypes[loc.archetype as usize].remove(loc.index, true)
            } {
                self.entities.meta[moved as usize].location.index = loc.index;
            }
        }

        self.spawn_inner(handle, components);
    }

    fn spawn_inner(&mut self, entity: Entity, components: impl DynamicBundle) {
        let archetype_id = match components.key() {
            Some(k) => {
                let archetypes = &mut self.archetypes;
                *self.bundle_to_archetype.entry(k).or_insert_with(|| {
                    components.with_ids(|ids| archetypes.get(ids, || components.type_info()))
                })
            }
            None => components.with_ids(|ids| self.archetypes.get(ids, || components.type_info())),
        };

        let archetype = &mut self.archetypes.archetypes[archetype_id as usize];
        unsafe {
            let index = archetype.allocate(entity.id);
            components.put(|ptr, ty| {
                archetype.put_dynamic(ptr, ty.id(), ty.layout().size(), index);
            });
            self.entities.meta[entity.id as usize].location = Location {
                archetype: archetype_id,
                index,
            };
        }
    }

    /// Efficiently spawn a large number of entities with the same statically-typed components
    ///
    /// Faster than calling [`spawn`](Self::spawn) repeatedly with the same components, but requires
    /// that component types are known at compile time.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let entities = world.spawn_batch((0..1_000).map(|i| (i, "abc"))).collect::<Vec<_>>();
    /// for i in 0..1_000 {
    ///     assert_eq!(*world.get::<&i32>(entities[i]).unwrap(), i as i32);
    /// }
    /// ```
    pub fn spawn_batch<I>(&mut self, iter: I) -> SpawnBatchIter<'_, I::IntoIter>
    where
        I: IntoIterator,
        I::Item: Bundle + 'static,
    {
        // Ensure all entity allocations are accounted for so `self.entities` can realloc if
        // necessary
        self.flush();

        let iter = iter.into_iter();
        let (lower, upper) = iter.size_hint();
        let archetype_id = self.reserve_inner::<I::Item>(
            u32::try_from(upper.unwrap_or(lower)).expect("iterator too large"),
        );

        SpawnBatchIter {
            inner: iter,
            entities: &mut self.entities,
            archetype_id,
            archetype: &mut self.archetypes.archetypes[archetype_id as usize],
        }
    }

    /// Super-efficiently spawn the contents of a [`ColumnBatch`]
    ///
    /// The fastest, but most specialized, way to spawn large numbers of entities. Useful for high
    /// performance deserialization. Supports dynamic component types.
    pub fn spawn_column_batch(&mut self, batch: ColumnBatch) -> SpawnColumnBatchIter<'_> {
        self.flush();

        let archetype = batch.0;
        let entity_count = archetype.len();
        // Store component data
        let (archetype_id, base) = self.archetypes.insert_batch(archetype);

        let archetype = &mut self.archetypes.archetypes[archetype_id as usize];
        let id_alloc = self.entities.alloc_many(entity_count, archetype_id, base);

        // Fix up entity IDs
        let mut id_alloc_clone = id_alloc.clone();
        let mut index = base as usize;
        while let Some(id) = id_alloc_clone.next(&self.entities) {
            archetype.set_entity_id(index, id);
            index += 1;
        }

        // Return iterator over new IDs
        SpawnColumnBatchIter {
            pending_end: id_alloc.pending_end,
            id_alloc,
            entities: &mut self.entities,
        }
    }

    /// Hybrid of [`spawn_column_batch`](Self::spawn_column_batch) and [`spawn_at`](Self::spawn_at)
    pub fn spawn_column_batch_at(&mut self, handles: &[Entity], batch: ColumnBatch) {
        let archetype = batch.0;
        assert_eq!(
            handles.len(),
            archetype.len() as usize,
            "number of entity IDs {} must match number of entities {}",
            handles.len(),
            archetype.len()
        );

        // Drop components of entities that will be replaced
        for &handle in handles {
            let loc = self.entities.alloc_at(handle);
            if let Some(loc) = loc {
                if let Some(moved) = unsafe {
                    self.archetypes.archetypes[loc.archetype as usize].remove(loc.index, true)
                } {
                    self.entities.meta[moved as usize].location.index = loc.index;
                }
            }
        }

        // Store components
        let (archetype_id, base) = self.archetypes.insert_batch(archetype);

        // Fix up entity IDs
        let archetype = &mut self.archetypes.archetypes[archetype_id as usize];
        for (&handle, index) in handles.iter().zip(base as usize..) {
            archetype.set_entity_id(index, handle.id());
            self.entities.meta[handle.id() as usize].location = Location {
                archetype: archetype_id,
                index: index as u32,
            };
        }
    }

    /// Allocate many entities ID concurrently
    ///
    /// Unlike [`spawn`](Self::spawn), this can be called concurrently with other operations on the
    /// [`World`] such as queries, but does not immediately create the entities. Reserved entities
    /// are not visible to queries or world iteration, but can be otherwise operated on
    /// freely. Operations that add or remove components or entities, such as `insert` or `despawn`,
    /// will cause all outstanding reserved entities to become real entities before proceeding. This
    /// can also be done explicitly by calling [`flush`](Self::flush).
    ///
    /// Useful for reserving an ID that will later have components attached to it with `insert`.
    pub fn reserve_entities(&self, count: u32) -> ReserveEntitiesIterator {
        self.entities.reserve_entities(count)
    }

    /// Allocate an entity ID concurrently
    ///
    /// See [`reserve_entities`](Self::reserve_entities).
    pub fn reserve_entity(&self) -> Entity {
        self.entities.reserve_entity()
    }

    /// Destroy an entity and all its components
    ///
    /// See also [`take`](Self::take).
    pub fn despawn(&mut self, entity: Entity) -> Result<(), NoSuchEntity> {
        self.flush();
        let loc = self.entities.free(entity)?;
        if let Some(moved) =
            unsafe { self.archetypes.archetypes[loc.archetype as usize].remove(loc.index, true) }
        {
            self.entities.meta[moved as usize].location.index = loc.index;
        }
        Ok(())
    }

    /// Ensure at least `additional` entities with exact components `T` can be spawned without reallocating
    pub fn reserve<T: Bundle + 'static>(&mut self, additional: u32) {
        self.reserve_inner::<T>(additional);
    }

    fn reserve_inner<T: Bundle + 'static>(&mut self, additional: u32) -> u32 {
        self.flush();
        self.entities.reserve(additional);

        let archetypes = &mut self.archetypes;
        let archetype_id = *self
            .bundle_to_archetype
            .entry(TypeId::of::<T>())
            .or_insert_with(|| {
                T::with_static_ids(|ids| {
                    archetypes.get(ids, || T::with_static_type_info(|info| info.to_vec()))
                })
            });

        self.archetypes.archetypes[archetype_id as usize].reserve(additional);
        archetype_id
    }

    /// Despawn all entities
    ///
    /// Preserves allocated storage for reuse but clears metadata so that [`Entity`] values will repeat (in contrast to [`despawn`][Self::despawn]).
    pub fn clear(&mut self) {
        for x in &mut self.archetypes.archetypes {
            x.clear();
        }
        self.entities.clear();
    }

    /// Whether `entity` still exists
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.contains(entity)
    }

    /// Efficiently iterate over all entities that have certain components, using dynamic borrow
    /// checking
    ///
    /// Prefer [`query_mut`](Self::query_mut) when concurrent access to the [`World`] is not required.
    ///
    /// Calling `iter` on the returned value yields `(Entity, Q)` tuples, where `Q` is some query
    /// type. A query type is any type for which an implementation of [`Query`] exists, e.g. `&T`,
    /// `&mut T`, a tuple of query types, or an `Option` wrapping a query type, where `T` is any
    /// component type. Components queried with `&mut` must only appear once. Entities which do not
    /// have a component type referenced outside of an `Option` will be skipped.
    ///
    /// Entities are yielded in arbitrary order.
    ///
    /// The returned [`QueryBorrow`] can be further transformed with combinator methods; see its
    /// documentation for details.
    ///
    /// Iterating a query will panic if it would violate an existing unique reference or construct
    /// an invalid unique reference. This occurs when two simultaneously-active queries could expose
    /// the same entity. Simultaneous queries can access the same component type if and only if the
    /// world contains no entities that have all components required by both queries, assuming no
    /// other component borrows are outstanding.
    ///
    /// Iterating a query yields references with lifetimes bound to the [`QueryBorrow`] returned
    /// here. To ensure those are invalidated, the return value of this method must be dropped for
    /// its dynamic borrows from the world to be released. Similarly, lifetime rules ensure that
    /// references obtained from a query cannot outlive the [`QueryBorrow`].
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn((123, true, "abc"));
    /// let b = world.spawn((456, false));
    /// let c = world.spawn((42, "def"));
    /// let entities = world.query::<(&i32, &bool)>()
    ///     .iter()
    ///     .map(|(e, (&i, &b))| (e, i, b)) // Copy out of the world
    ///     .collect::<Vec<_>>();
    /// assert_eq!(entities.len(), 2);
    /// assert!(entities.contains(&(a, 123, true)));
    /// assert!(entities.contains(&(b, 456, false)));
    /// ```
    pub fn query<Q: Query>(&self) -> QueryBorrow<'_, Q> {
        QueryBorrow::new(self)
    }

    /// Provide random access to any entity for a given Query.
    pub fn view<Q: Query>(&self) -> ViewBorrow<'_, Q> {
        ViewBorrow::new(self)
    }

    /// Provide random access to any entity for a given Query on a uniquely
    /// borrowed world. Like [`view`](Self::view), but faster because dynamic borrow checks can be skipped.
    pub fn view_mut<Q: Query>(&mut self) -> View<'_, Q> {
        assert_borrow::<Q>();
        let cache = CachedQuery::get(self);
        unsafe { View::<Q>::new(self.entities_meta(), self.archetypes_inner(), cache) }
    }

    /// Query a uniquely borrowed world
    ///
    /// Like [`query`](Self::query), but faster because dynamic borrow checks can be skipped. Note
    /// that, unlike [`query`](Self::query), this returns an `IntoIterator` which can be passed
    /// directly to a `for` loop.
    pub fn query_mut<Q: Query>(&mut self) -> QueryMut<'_, Q> {
        QueryMut::new(self)
    }

    pub(crate) fn memo(&self) -> (u64, u32) {
        (self.id, self.archetypes.generation())
    }

    #[inline(always)]
    pub(crate) fn entities_meta(&self) -> &[EntityMeta] {
        &self.entities.meta
    }

    #[inline(always)]
    pub(crate) fn archetypes_inner(&self) -> &[Archetype] {
        &self.archetypes.archetypes
    }

    #[cfg(feature = "std")]
    pub(crate) fn query_cache(&self) -> &QueryCache {
        &self.query_cache
    }

    /// Prepare a query against a single entity, using dynamic borrow checking
    ///
    /// Prefer [`query_one_mut`](Self::query_one_mut) when concurrent access to the [`World`] is not
    /// required.
    ///
    /// Call [`get`](QueryOne::get) on the resulting [`QueryOne`] to actually execute the query. The
    /// [`QueryOne`] value is responsible for releasing the dynamically-checked borrow made by
    /// `get`, so it can't be dropped while references returned by `get` are live.
    ///
    /// Handy for accessing multiple components simultaneously.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn((123, true, "abc"));
    /// // The returned query must outlive the borrow made by `get`
    /// let mut query = world.query_one::<(&mut i32, &bool)>(a).unwrap();
    /// let (number, flag) = query.get().unwrap();
    /// if *flag { *number *= 2; }
    /// assert_eq!(*number, 246);
    /// ```
    pub fn query_one<Q: Query>(&self, entity: Entity) -> Result<QueryOne<'_, Q>, NoSuchEntity> {
        let loc = self.entities.get(entity)?;
        Ok(unsafe {
            QueryOne::new(
                &self.archetypes.archetypes[loc.archetype as usize],
                loc.index,
            )
        })
    }

    /// Query a single entity in a uniquely borrowed world
    ///
    /// Like [`query_one`](Self::query_one), but faster because dynamic borrow checks can be
    /// skipped. Note that, unlike [`query_one`](Self::query_one), on success this returns the
    /// query's results directly.
    pub fn query_one_mut<Q: Query>(
        &mut self,
        entity: Entity,
    ) -> Result<Q::Item<'_>, QueryOneError> {
        assert_borrow::<Q>();

        let loc = self.entities.get(entity)?;
        let archetype = &self.archetypes.archetypes[loc.archetype as usize];
        let state = Q::Fetch::prepare(archetype).ok_or(QueryOneError::Unsatisfied)?;
        let fetch = Q::Fetch::execute(archetype, state);
        unsafe { Ok(Q::get(&fetch, loc.index as usize)) }
    }

    /// Query a fixed number of distinct entities in a uniquely borrowed world
    ///
    /// Like [`query_one_mut`](Self::query_one_mut), but for multiple entities, which would
    /// otherwise be forbidden by the unique borrow. Panics if the same entity occurs more than
    /// once.
    pub fn query_many_mut<Q: Query, const N: usize>(
        &mut self,
        entities: [Entity; N],
    ) -> [Result<Q::Item<'_>, QueryOneError>; N] {
        assert_borrow::<Q>();
        assert_distinct(&entities);

        entities.map(|entity| {
            let loc = self.entities.get(entity)?;
            let archetype = &self.archetypes.archetypes[loc.archetype as usize];
            let state = Q::Fetch::prepare(archetype).ok_or(QueryOneError::Unsatisfied)?;
            let fetch = Q::Fetch::execute(archetype, state);
            unsafe { Ok(Q::get(&fetch, loc.index as usize)) }
        })
    }

    /// Short-hand for [`entity`](Self::entity) followed by [`EntityRef::get`]
    pub fn get<'a, T: ComponentRef<'a>>(
        &'a self,
        entity: Entity,
    ) -> Result<T::Ref, ComponentError> {
        Ok(self
            .entity(entity)?
            .get::<T>()
            .ok_or_else(MissingComponent::new::<T::Component>)?)
    }

    /// Short-hand for [`entity`](Self::entity) followed by [`EntityRef::satisfies`]
    pub fn satisfies<Q: Query>(&self, entity: Entity) -> Result<bool, NoSuchEntity> {
        Ok(self.entity(entity)?.satisfies::<Q>())
    }

    /// Access an entity regardless of its component types
    ///
    /// Does not immediately borrow any component.
    pub fn entity(&self, entity: Entity) -> Result<EntityRef<'_>, NoSuchEntity> {
        let loc = self.entities.get(entity)?;
        unsafe {
            Ok(EntityRef::new(
                &self.archetypes.archetypes[loc.archetype as usize],
                entity,
                loc.index,
            ))
        }
    }

    /// Given an id obtained from [`Entity::id`], reconstruct the still-live [`Entity`].
    ///
    /// # Safety
    ///
    /// `id` must correspond to a currently live [`Entity`]. A despawned or never-allocated `id`
    /// will produce undefined behavior.
    pub unsafe fn find_entity_from_id(&self, id: u32) -> Entity {
        self.entities.resolve_unknown_gen(id)
    }

    /// Iterate over all entities in the world
    ///
    /// Entities are yielded in arbitrary order. Prefer [`query`](Self::query) for better
    /// performance when components will be accessed in predictable patterns.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let a = world.spawn(());
    /// let b = world.spawn(());
    /// let ids = world.iter().map(|entity_ref| entity_ref.entity()).collect::<Vec<_>>();
    /// assert_eq!(ids.len(), 2);
    /// assert!(ids.contains(&a));
    /// assert!(ids.contains(&b));
    /// ```
    pub fn iter(&self) -> Iter<'_> {
        Iter::new(&self.archetypes.archetypes, &self.entities)
    }

    /// Add `components` to `entity`
    ///
    /// Computational cost is proportional to the number of components `entity` has. If an entity
    /// already has a component of a certain type, it is dropped and replaced.
    ///
    /// When inserting a single component, see [`insert_one`](Self::insert_one) for convenience.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let e = world.spawn((123, "abc"));
    /// world.insert(e, (456, true));
    /// assert_eq!(*world.get::<&i32>(e).unwrap(), 456);
    /// assert_eq!(*world.get::<&bool>(e).unwrap(), true);
    /// ```
    pub fn insert(
        &mut self,
        entity: Entity,
        components: impl DynamicBundle,
    ) -> Result<(), NoSuchEntity> {
        self.flush();

        let loc = self.entities.get(entity)?;
        self.insert_inner(entity, components, loc.archetype, loc);
        Ok(())
    }

    /// The implementation backing [`insert`](Self::insert) exposed so that it can also be used by [`exchange`](Self::exchange).
    ///
    /// Note that `graph_origin` is always equal to `loc.archetype` during insertion. Only for exchange, `graph_origin` identifies
    /// the intermediate archetype which would be reached after removal and before insertion even though
    /// the actual component data still resides in `loc.archetype`.
    fn insert_inner(
        &mut self,
        entity: Entity,
        components: impl DynamicBundle,
        graph_origin: u32,
        loc: Location,
    ) {
        let target_storage;
        let target = match components.key() {
            None => {
                target_storage = self.archetypes.get_insert_target(graph_origin, &components);
                &target_storage
            }
            Some(key) => match self.insert_edges.entry((graph_origin, key)) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let target = self.archetypes.get_insert_target(graph_origin, &components);
                    entry.insert(target)
                }
            },
        };

        let source_arch = &mut self.archetypes.archetypes[loc.archetype as usize];
        unsafe {
            // Drop the components we're overwriting
            for &ty in &target.replaced {
                let ptr = source_arch
                    .get_dynamic(ty.id(), ty.layout().size(), loc.index)
                    .unwrap();
                ty.drop(ptr.as_ptr());
            }

            if target.index == loc.archetype {
                // Update components in the current archetype
                let arch = &mut self.archetypes.archetypes[loc.archetype as usize];
                components.put(|ptr, ty| {
                    arch.put_dynamic(ptr, ty.id(), ty.layout().size(), loc.index);
                });
                return;
            }

            let (source_arch, target_arch) = index2(
                &mut self.archetypes.archetypes,
                loc.archetype as usize,
                target.index as usize,
            );

            // Allocate storage in the archetype and update the entity's location to address it
            let target_index = target_arch.allocate(entity.id);
            let meta = &mut self.entities.meta[entity.id as usize];
            meta.location.archetype = target.index;
            meta.location.index = target_index;

            // Move the new components
            components.put(|ptr, ty| {
                target_arch.put_dynamic(ptr, ty.id(), ty.layout().size(), target_index);
            });

            // Move the components we're keeping
            for &ty in &target.retained {
                let src = source_arch
                    .get_dynamic(ty.id(), ty.layout().size(), loc.index)
                    .unwrap();
                target_arch.put_dynamic(src.as_ptr(), ty.id(), ty.layout().size(), target_index)
            }

            // Free storage in the old archetype
            if let Some(moved) = source_arch.remove(loc.index, false) {
                self.entities.meta[moved as usize].location.index = loc.index;
            }
        }
    }

    /// Add `component` to `entity`
    ///
    /// See [`insert`](Self::insert).
    pub fn insert_one(
        &mut self,
        entity: Entity,
        component: impl Component,
    ) -> Result<(), NoSuchEntity> {
        self.insert(entity, (component,))
    }

    /// Remove components from `entity`
    ///
    /// Computational cost is proportional to the number of components `entity` has. The entity
    /// itself is not removed, even if no components remain; use `despawn` for that. If any
    /// component in `T` is not present in `entity`, no components are removed and an error is
    /// returned.
    ///
    /// When removing a single component, see [`remove_one`](Self::remove_one) for convenience.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let e = world.spawn((123, "abc", true));
    /// assert_eq!(world.remove::<(i32, &str)>(e), Ok((123, "abc")));
    /// assert!(world.get::<&i32>(e).is_err());
    /// assert!(world.get::<&&str>(e).is_err());
    /// assert_eq!(*world.get::<&bool>(e).unwrap(), true);
    /// ```
    pub fn remove<T: Bundle + 'static>(&mut self, entity: Entity) -> Result<T, ComponentError> {
        self.flush();

        // Gather current metadata
        let loc = self.entities.get_mut(entity)?;
        let old_index = loc.index;
        let source_arch = &self.archetypes.archetypes[loc.archetype as usize];

        // Move out of the source archetype, or bail out if a component is missing
        let bundle = unsafe {
            T::get(|ty| source_arch.get_dynamic(ty.id(), ty.layout().size(), old_index))?
        };

        // Find the target archetype ID
        let target =
            Self::remove_target::<T>(&mut self.archetypes, &mut self.remove_edges, loc.archetype);

        // Store components to the target archetype and update metadata
        if loc.archetype != target {
            // If we actually removed any components, the entity needs to be moved into a new archetype
            let (source_arch, target_arch) = index2(
                &mut self.archetypes.archetypes,
                loc.archetype as usize,
                target as usize,
            );
            let target_index = unsafe { target_arch.allocate(entity.id) };
            loc.archetype = target;
            loc.index = target_index;
            if let Some(moved) = unsafe {
                source_arch.move_to(old_index, |src, ty, size| {
                    // Only move the components present in the target archetype, i.e. the non-removed ones.
                    if let Some(dst) = target_arch.get_dynamic(ty, size, target_index) {
                        ptr::copy_nonoverlapping(src, dst.as_ptr(), size);
                    }
                })
            } {
                self.entities.meta[moved as usize].location.index = old_index;
            }
        }

        Ok(bundle)
    }

    fn remove_target<T: Bundle + 'static>(
        archetypes: &mut ArchetypeSet,
        remove_edges: &mut IndexTypeIdMap<u32>,
        old_archetype: u32,
    ) -> u32 {
        match remove_edges.entry((old_archetype, TypeId::of::<T>())) {
            Entry::Occupied(entry) => *entry.into_mut(),
            Entry::Vacant(entry) => {
                let info = T::with_static_type_info(|removed| {
                    archetypes.archetypes[old_archetype as usize]
                        .types()
                        .iter()
                        .filter(|x| removed.binary_search(x).is_err())
                        .cloned()
                        .collect::<Vec<_>>()
                });
                let elements = info.iter().map(|x| x.id()).collect::<Box<_>>();
                let index = archetypes.get(&*elements, move || info);
                *entry.insert(index)
            }
        }
    }

    /// Remove the `T` component from `entity`
    ///
    /// See [`remove`](Self::remove).
    pub fn remove_one<T: Component>(&mut self, entity: Entity) -> Result<T, ComponentError> {
        self.remove::<(T,)>(entity).map(|(x,)| x)
    }

    /// Remove `S` components from `entity` and then add `components`
    ///
    /// This has the same effect as calling [`remove::<S>`](Self::remove) and then [`insert::<T>`](Self::insert),
    /// but is more efficient as the intermediate archetype after removal but before insertion is skipped.
    pub fn exchange<S: Bundle + 'static, T: DynamicBundle>(
        &mut self,
        entity: Entity,
        components: T,
    ) -> Result<S, ComponentError> {
        self.flush();

        // Gather current metadata
        let loc = self.entities.get(entity)?;

        // Move out of the source archetype, or bail out if a component is missing
        let source_arch = &self.archetypes.archetypes[loc.archetype as usize];

        let bundle = unsafe {
            S::get(|ty| source_arch.get_dynamic(ty.id(), ty.layout().size(), loc.index))?
        };

        // Find the intermediate archetype ID
        let intermediate =
            Self::remove_target::<S>(&mut self.archetypes, &mut self.remove_edges, loc.archetype);

        self.insert_inner(entity, components, intermediate, loc);

        Ok(bundle)
    }

    /// Remove the `S` component from `entity` and then add `component`
    ///
    /// See [`exchange`](Self::exchange).
    pub fn exchange_one<S: Component, T: Component>(
        &mut self,
        entity: Entity,
        component: T,
    ) -> Result<S, ComponentError> {
        self.exchange::<(S,), (T,)>(entity, (component,))
            .map(|(x,)| x)
    }

    /// Borrow a single component of `entity` without safety checks
    ///
    /// `T` must be a shared or unique reference to a component type.
    ///
    /// Should only be used as a building block for safe abstractions.
    ///
    /// # Safety
    ///
    /// `entity` must have been previously obtained from this [`World`], and no unique borrow of the
    /// same component of `entity` may be live simultaneous to the returned reference.
    pub unsafe fn get_unchecked<'a, T: ComponentRef<'a>>(
        &'a self,
        entity: Entity,
    ) -> Result<T, ComponentError> {
        let loc = self.entities.get(entity)?;
        let archetype = &self.archetypes.archetypes[loc.archetype as usize];
        let state = archetype
            .get_state::<T::Component>()
            .ok_or_else(MissingComponent::new::<T::Component>)?;
        Ok(T::from_raw(
            archetype
                .get_base::<T::Component>(state)
                .as_ptr()
                .add(loc.index as usize),
        ))
    }

    /// Convert all reserved entities into empty entities that can be iterated and accessed
    ///
    /// Invoked implicitly by operations that add or remove components or entities, i.e. all
    /// variations of `spawn`, `despawn`, `insert`, and `remove`.
    pub fn flush(&mut self) {
        let arch = &mut self.archetypes.archetypes[0];
        self.entities
            .flush(|id, location| location.index = unsafe { arch.allocate(id) });
    }

    /// Inspect the archetypes that entities are organized into
    ///
    /// Useful for dynamically scheduling concurrent queries by checking borrows in advance, and for
    /// efficient serialization.
    #[inline(always)]
    pub fn archetypes(&self) -> impl ExactSizeIterator<Item = &'_ Archetype> + '_ {
        self.archetypes_inner().iter()
    }

    /// Despawn `entity`, yielding a [`DynamicBundle`] of its components
    ///
    /// Useful for moving entities between worlds.
    pub fn take(&mut self, entity: Entity) -> Result<TakenEntity<'_>, NoSuchEntity> {
        self.flush();
        let loc = self.entities.get(entity)?;
        let archetype = &mut self.archetypes.archetypes[loc.archetype as usize];
        unsafe {
            Ok(TakenEntity::new(
                &mut self.entities,
                entity,
                archetype,
                loc.index,
            ))
        }
    }

    /// Returns a distinct value after `archetypes` is changed
    ///
    /// Store the current value after deriving information from [`archetypes`](Self::archetypes),
    /// then check whether the value returned by this function differs before attempting an
    /// operation that relies on its correctness. Useful for determining whether e.g. a concurrent
    /// query execution plan is still correct.
    ///
    /// The generation may be, but is not necessarily, changed as a result of adding or removing any
    /// entity or component.
    ///
    /// # Example
    /// ```
    /// # use hecs::*;
    /// let mut world = World::new();
    /// let initial_gen = world.archetypes_generation();
    /// world.spawn((123, "abc"));
    /// assert_ne!(initial_gen, world.archetypes_generation());
    /// ```
    pub fn archetypes_generation(&self) -> ArchetypesGeneration {
        ArchetypesGeneration(self.archetypes.generation())
    }

    /// Number of currently live entities
    #[inline]
    pub fn len(&self) -> u32 {
        self.entities.len()
    }

    /// Whether no entities are live
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entity handles that will be yielded by future spawns
    ///
    /// [`Entity`] handles will be allocated deterministically between different
    /// worlds (e.g. across serialization) if they have the same live entities
    /// and the same freelist.
    ///
    /// To deserialize a world that will match the entity allocation behavior of
    /// a previously serialized world, first create a new world, then use
    /// [`spawn_at`](Self::spawn_at) or similar to reconstruct the serialized
    /// world's entities in place, then call
    /// [`set_freelist`](Self::set_freelist) with the serialized world's
    /// freelist.
    pub fn freelist(&self) -> impl ExactSizeIterator<Item = Entity> + '_ {
        self.entities.freelist()
    }

    /// Designate entity IDs to be reused by future spawns
    ///
    /// See [`freelist`](Self::freelist). May only be called when the set of
    /// live entity IDs exactly matches those that were live when the supplied
    /// freelist was captured. For example, spawning, despawning, or reserving
    /// an entity ID would invalidate a previously captured freelist.
    pub fn set_freelist(&mut self, freelist: &[Entity]) {
        self.entities.set_freelist(freelist);
    }
}

unsafe impl Send for World {}
unsafe impl Sync for World {}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> IntoIterator for &'a World {
    type IntoIter = Iter<'a>;
    type Item = EntityRef<'a>;
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

fn index2<T>(x: &mut [T], i: usize, j: usize) -> (&mut T, &mut T) {
    assert!(i != j);
    assert!(i < x.len());
    assert!(j < x.len());
    let ptr = x.as_mut_ptr();
    unsafe { (&mut *ptr.add(i), &mut *ptr.add(j)) }
}

/// Errors that arise when accessing components
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum ComponentError {
    /// The entity was already despawned
    NoSuchEntity,
    /// The entity did not have a requested component
    MissingComponent(MissingComponent),
}

#[cfg(feature = "std")]
impl Error for ComponentError {}

impl fmt::Display for ComponentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ComponentError::*;
        match *self {
            NoSuchEntity => f.write_str("no such entity"),
            MissingComponent(ref x) => x.fmt(f),
        }
    }
}

impl From<NoSuchEntity> for ComponentError {
    fn from(NoSuchEntity: NoSuchEntity) -> Self {
        ComponentError::NoSuchEntity
    }
}

impl From<MissingComponent> for ComponentError {
    fn from(x: MissingComponent) -> Self {
        ComponentError::MissingComponent(x)
    }
}

/// Errors that arise when querying a single entity
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum QueryOneError {
    /// The entity was already despawned
    NoSuchEntity,
    /// The entity exists but does not satisfy the query
    Unsatisfied,
}

#[cfg(feature = "std")]
impl Error for QueryOneError {}

impl fmt::Display for QueryOneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use QueryOneError::*;
        match *self {
            NoSuchEntity => f.write_str("no such entity"),
            Unsatisfied => f.write_str("unsatisfied"),
        }
    }
}

impl From<NoSuchEntity> for QueryOneError {
    fn from(NoSuchEntity: NoSuchEntity) -> Self {
        QueryOneError::NoSuchEntity
    }
}

/// Types that can be components, implemented automatically for all `Send + Sync + 'static` types
///
/// This is just a convenient shorthand for `Send + Sync + 'static`, and never needs to be
/// implemented manually.
pub trait Component: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> Component for T {}

/// Iterator over all of a world's entities
pub struct Iter<'a> {
    archetypes: core::slice::Iter<'a, Archetype>,
    entities: &'a Entities,
    current: Option<&'a Archetype>,
    index: u32,
}

impl<'a> Iter<'a> {
    fn new(archetypes: &'a [Archetype], entities: &'a Entities) -> Self {
        Self {
            archetypes: archetypes.iter(),
            entities,
            current: None,
            index: 0,
        }
    }
}

unsafe impl Send for Iter<'_> {}
unsafe impl Sync for Iter<'_> {}

impl<'a> Iterator for Iter<'a> {
    type Item = EntityRef<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.current {
                None => {
                    self.current = Some(self.archetypes.next()?);
                    self.index = 0;
                }
                Some(current) => {
                    if self.index == current.len() {
                        self.current = None;
                        continue;
                    }
                    let index = self.index;
                    self.index += 1;
                    let id = current.entity_id(index);
                    return Some(unsafe {
                        EntityRef::new(
                            current,
                            Entity {
                                id,
                                generation: self.entities.meta[id as usize].generation,
                            },
                            index,
                        )
                    });
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len(), Some(self.len()))
    }
}

impl ExactSizeIterator for Iter<'_> {
    #[inline]
    fn len(&self) -> usize {
        self.entities.len() as usize
    }
}

impl<A: DynamicBundle> Extend<A> for World {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = A>,
    {
        for x in iter {
            self.spawn(x);
        }
    }
}

impl<A: DynamicBundle> core::iter::FromIterator<A> for World {
    fn from_iter<I: IntoIterator<Item = A>>(iter: I) -> Self {
        let mut world = World::new();
        world.extend(iter);
        world
    }
}

/// Determines freshness of information derived from [`World::archetypes`]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ArchetypesGeneration(u32);

/// Entity IDs created by [`World::spawn_batch`]
pub struct SpawnBatchIter<'a, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    inner: I,
    entities: &'a mut Entities,
    archetype_id: u32,
    archetype: &'a mut Archetype,
}

impl<I> Drop for SpawnBatchIter<'_, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    fn drop(&mut self) {
        for _ in self {}
    }
}

impl<I> Iterator for SpawnBatchIter<'_, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    type Item = Entity;

    fn next(&mut self) -> Option<Entity> {
        let components = self.inner.next()?;
        let entity = self.entities.alloc();
        let index = unsafe { self.archetype.allocate(entity.id) };
        unsafe {
            components.put(|ptr, ty| {
                self.archetype
                    .put_dynamic(ptr, ty.id(), ty.layout().size(), index);
            });
        }
        self.entities.meta[entity.id as usize].location = Location {
            archetype: self.archetype_id,
            index,
        };
        Some(entity)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<I, T> ExactSizeIterator for SpawnBatchIter<'_, I>
where
    I: ExactSizeIterator<Item = T>,
    T: Bundle,
{
    fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Iterator over [`Entity`]s spawned by [`World::spawn_column_batch()`]
pub struct SpawnColumnBatchIter<'a> {
    pending_end: usize,
    id_alloc: crate::entities::AllocManyState,
    entities: &'a mut Entities,
}

impl Iterator for SpawnColumnBatchIter<'_> {
    type Item = Entity;

    fn next(&mut self) -> Option<Entity> {
        let id = self.id_alloc.next(self.entities)?;
        Some(unsafe { self.entities.resolve_unknown_gen(id) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len(), Some(self.len()))
    }
}

impl ExactSizeIterator for SpawnColumnBatchIter<'_> {
    fn len(&self) -> usize {
        self.id_alloc.len(self.entities)
    }
}

impl Drop for SpawnColumnBatchIter<'_> {
    fn drop(&mut self) {
        // Consume used freelist entries
        self.entities.finish_alloc_many(self.pending_end);
    }
}

struct ArchetypeSet {
    /// Maps sorted component type sets to archetypes
    index: HashMap<Box<[TypeId]>, u32>,
    archetypes: Vec<Archetype>,
}

impl ArchetypeSet {
    fn new() -> Self {
        // `flush` assumes archetype 0 always exists, representing entities with no components.
        Self {
            index: Some((Box::default(), 0)).into_iter().collect(),
            archetypes: vec![Archetype::new(Vec::new())],
        }
    }

    /// Find the archetype ID that has exactly `components`
    fn get<T: Borrow<[TypeId]> + Into<Box<[TypeId]>>>(
        &mut self,
        components: T,
        info: impl FnOnce() -> Vec<TypeInfo>,
    ) -> u32 {
        self.index
            .get(components.borrow())
            .copied()
            .unwrap_or_else(|| self.insert(components.into(), info()))
    }

    fn insert(&mut self, components: Box<[TypeId]>, info: Vec<TypeInfo>) -> u32 {
        let x = self.archetypes.len() as u32;
        self.archetypes.push(Archetype::new(info));
        let old = self.index.insert(components, x);
        debug_assert!(old.is_none(), "inserted duplicate archetype");
        x
    }

    /// Returns archetype ID and starting location index
    fn insert_batch(&mut self, archetype: Archetype) -> (u32, u32) {
        let ids = archetype
            .types()
            .iter()
            .map(|info| info.id())
            .collect::<Box<_>>();

        match self.index.entry(ids) {
            Entry::Occupied(x) => {
                // Duplicate of existing archetype
                let existing = &mut self.archetypes[*x.get() as usize];
                let base = existing.len();
                unsafe {
                    existing.merge(archetype);
                }
                (*x.get(), base)
            }
            Entry::Vacant(x) => {
                // Brand new archetype
                let id = self.archetypes.len() as u32;
                self.archetypes.push(archetype);
                x.insert(id);
                (id, 0)
            }
        }
    }

    fn generation(&self) -> u32 {
        self.archetypes.len() as u32
    }

    fn get_insert_target(&mut self, src: u32, components: &impl DynamicBundle) -> InsertTarget {
        // Assemble Vec<TypeInfo> for the final entity
        let arch = &mut self.archetypes[src as usize];
        let mut info = arch.types().to_vec();
        let mut replaced = Vec::new(); // Elements in both archetype.types() and components.type_info()
        let mut retained = Vec::new(); // Elements in archetype.types() but not components.type_info()

        // Because both `components.type_info()` and `arch.types()` are
        // ordered, we can identify elements in one but not the other efficiently with parallel
        // iteration.
        let mut src_ty = 0;
        for ty in components.type_info() {
            while src_ty < arch.types().len() && arch.types()[src_ty] <= ty {
                if arch.types()[src_ty] != ty {
                    retained.push(arch.types()[src_ty]);
                }
                src_ty += 1;
            }
            if arch.has_dynamic(ty.id()) {
                replaced.push(ty);
            } else {
                info.push(ty);
            }
        }
        info.sort_unstable();
        retained.extend_from_slice(&arch.types()[src_ty..]);

        // Find the archetype it'll live in
        let elements = info.iter().map(|x| x.id()).collect::<Box<_>>();
        let index = self.get(elements, move || info);
        InsertTarget {
            replaced,
            retained,
            index,
        }
    }
}

/// Metadata cached for inserting components into entities from this archetype
struct InsertTarget {
    /// Components from the current archetype that are replaced by the insert
    replaced: Vec<TypeInfo>,
    /// Components from the current archetype that are moved by the insert
    retained: Vec<TypeInfo>,
    /// ID of the target archetype
    index: u32,
}

type IndexTypeIdMap<V> = HashMap<(u32, TypeId), V, BuildHasherDefault<IndexTypeIdHasher>>;

#[derive(Default)]
struct IndexTypeIdHasher(u64);

impl Hasher for IndexTypeIdHasher {
    fn write_u32(&mut self, index: u32) {
        self.0 ^= u64::from(index);
    }

    fn write_u64(&mut self, type_id: u64) {
        self.0 ^= type_id;
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!()
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_empty() {
        let mut world = World::new();
        let a = world.spawn(());
        world.despawn(a).unwrap();
        let b = world.spawn(());
        assert_eq!(a.id, b.id);
        assert_ne!(a.generation, b.generation);
    }

    #[test]
    fn clear_repeats_entity_id() {
        let mut world = World::new();
        let a = world.spawn(());
        world.clear();
        let b = world.spawn(());
        assert_eq!(a.id, b.id);
        assert_eq!(a.generation, b.generation);
    }

    #[test]
    fn spawn_at() {
        let mut world = World::new();
        let a = world.spawn(());
        world.despawn(a).unwrap();
        let b = world.spawn(());
        assert!(world.contains(b));
        assert_eq!(a.id, b.id);
        assert_ne!(a.generation, b.generation);
        world.spawn_at(a, ());
        assert!(!world.contains(b));
        assert_eq!(b.id, a.id);
        assert_ne!(b.generation, a.generation);
    }

    #[test]
    fn reuse_populated() {
        let mut world = World::new();
        let a = world.spawn((42,));
        assert_eq!(*world.get::<&i32>(a).unwrap(), 42);
        world.despawn(a).unwrap();
        let b = world.spawn((true,));
        assert_eq!(a.id, b.id);
        assert_ne!(a.generation, b.generation);
        assert!(world.get::<&i32>(b).is_err());
        assert!(*world.get::<&bool>(b).unwrap());
    }

    #[test]
    fn remove_nothing() {
        let mut world = World::new();
        let a = world.spawn(("abc", 123));
        world.remove::<()>(a).unwrap();
    }

    #[test]
    fn bad_insert() {
        let mut world = World::new();
        assert!(world.insert_one(Entity::DANGLING, ()).is_err());
    }

    #[test]
    fn deterministic_ids() {
        let mut world = World::new();
        let entities = world.spawn_batch([(); 10]).collect::<Vec<_>>();
        const DESPAWNS: [usize; 3] = [4, 0, 7];
        for index in DESPAWNS {
            world.despawn(entities[index]).unwrap();
        }

        let mut world2 = World::new();
        for (i, entity) in entities.iter().copied().enumerate() {
            if DESPAWNS.contains(&i) {
                continue;
            }
            world2.spawn_at(entity, ());
        }
        world2.set_freelist(&world.freelist().collect::<Vec<_>>());

        assert_eq!(
            world
                .spawn_batch([(); DESPAWNS.len() + 2])
                .collect::<Vec<_>>(),
            world2
                .spawn_batch([(); DESPAWNS.len() + 2])
                .collect::<Vec<_>>()
        );
    }

    // Covers off-by-one errors in freelist handling
    #[test]
    fn single_element_freelist() {
        let mut world = World::new();
        let a = world.spawn(());
        world.despawn(a).unwrap();
        let freelist = world.freelist().collect::<Vec<_>>();

        let mut world2 = World::new();
        world2.set_freelist(&freelist);
    }
}
