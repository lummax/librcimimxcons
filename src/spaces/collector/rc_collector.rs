// Copyright (c) <2015> <lummax>
// Licensed under MIT (http://opensource.org/licenses/MIT)

use std::collections::VecDeque;

use spaces::immix_space::ImmixSpace;
use spaces::large_object_space::LargeObjectSpace;
use gc_object::GCObjectRef;
use spaces::CollectionType;
use constants::WRITE_BARRIER_COLLECT_THRESHOLD;

/// The `RCCollector` perform the steps for the deferred coalesced
/// conservative reference counting. The `write_barrier()` must be called
/// before an objects members are changed. The `collect()` function should be
/// called periodically to incrementally collect garbage.
pub struct RCCollector {
    /// The roots of the last collection.
    ///
    /// At the start of a reference counting collection a decrement is
    /// enqueued for every old root.
    old_root_buffer: Vec<GCObjectRef>,

    /// The enqueued decrements on objects.
    ///
    /// These are applied in the last step of the reference counting
    /// collection.
    decrement_buffer: VecDeque<GCObjectRef>,

    /// A buffer of modified objects.
    ///
    /// Objects are pushed into this buffer if they are encountered for the
    /// first time by the reference counting collector or marked as modified
    /// using the `write_barrier()`.
    modified_buffer: VecDeque<GCObjectRef>,

    /// Flag if this collection is a evacuating collection.
    perform_evac: bool,

    /// Counter for write barrie invocations since last collection.
    write_barrier_counter: usize,
}

impl RCCollector {
    /// Create a new `RCCollector`.
    pub fn new() -> RCCollector {
        RCCollector {
            old_root_buffer: Vec::new(),
            decrement_buffer: VecDeque::new(),
            modified_buffer: VecDeque::new(),
            perform_evac: false,
            write_barrier_counter: 0,
        }
    }

    /// Collect garbage using deferred coalesced conservative reference
    /// counting.
    ///
    /// The steps are:
    /// - process_old_roots()
    /// - process_current_roots()
    /// - process_los_new_objects()
    /// - process_mod_buffer()
    /// - process_decrement_buffer()
    pub fn collect(&mut self, collection_type: &CollectionType,
                   roots: &[GCObjectRef], immix_space: &mut ImmixSpace,
                   large_object_space: &mut LargeObjectSpace) {
        debug!("Start RC collection");
        self.perform_evac = collection_type.is_evac();
        self.process_old_roots();
        self.process_current_roots(immix_space, roots);
        self.process_los_new_objects(immix_space, large_object_space.get_new_objects());
        self.process_mod_buffer(immix_space);
        self.process_decrement_buffer(immix_space, large_object_space);
        self.write_barrier_counter = 0;
        debug!("Complete collection");
    }

    /// The write barrier for an object in deferred coalesced reference
    /// counting pushes the object into the modified buffer and enqueues a
    /// decrement for the old children.
    ///
    /// Returns if a collection should be triggered (see
    /// constants::WRITE_BARRIER_COLLECT_THRESHOLD).
    pub fn write_barrier(&mut self, object: GCObjectRef) -> bool {
        if !unsafe{ (*object).set_logged(true) } {
            debug!("Write barrier on object {:p}", object);
            self.modified(object);
            for child in unsafe{ (*object).children() } {
                self.decrement(child);
            }
            self.write_barrier_counter += 1;
        }
        WRITE_BARRIER_COLLECT_THRESHOLD > 0 &&
            self.write_barrier_counter >= WRITE_BARRIER_COLLECT_THRESHOLD
    }
}

impl RCCollector {
    /// Push an object into the modified buffer.
    fn modified(&mut self, object: GCObjectRef) {
        debug!("Push object {:p} into mod buffer", object);
        self.modified_buffer.push_back(object);
    }

    /// Enqueue a decrement for an object.
    fn decrement(&mut self, object: GCObjectRef) {
        debug!("Push object {:p} into dec buffer", object);
        self.decrement_buffer.push_back(object);
    }

    /// Perform an increment for an object.
    ///
    /// If this is the first time the reference counting collector encounters
    /// this object, it will be pushed into the modified buffer.
    ///
    /// If `try_evacuate` is set, the object is new an in the immix space and
    /// the collectors performs an opportunistic evacuation, this function
    /// tries to evacuate the object into a free block.
    fn increment(&mut self, immix_space: &mut ImmixSpace,
                 object: GCObjectRef, try_evacuate: bool) -> Option<GCObjectRef> {
        debug!("Increment object {:p}", object);
        if unsafe{ (*object).increment() } {
            if try_evacuate && self.perform_evac && immix_space.is_gc_object(object) {
                if let Some(new_object) = immix_space.maybe_evacuate(object) {
                    debug!("Evacuated object {:p} to {:p}", object, new_object);
                    immix_space.decrement_lines(object);
                    self.modified(new_object);
                    return Some(new_object);
                }
            }
            self.modified(object);
        }
        None
    }

    /// The old roots are enqueued for a decrement.
    fn process_old_roots(&mut self) {
        debug!("Process old roots (size {})", self.old_root_buffer.len());
        self.decrement_buffer.extend(self.old_root_buffer.drain(..));
    }

    /// The current roots are incremented (but never evacuated) and stored as
    /// old roots.
    fn process_current_roots(&mut self, immix_space: &mut ImmixSpace,
                             roots: &[GCObjectRef]) {
        debug!("Process current roots (size {})", roots.len());
        for root in roots.iter().map(|o| *o) {
            debug!("Process root object: {:p}", root);
            self.increment(immix_space, root, false);
            self.old_root_buffer.push(root);
        }
    }

    /// Objects (roots) in the large object space are temporarily incremented.
    fn process_los_new_objects(&mut self, immix_space: &mut ImmixSpace,
                               new_objects: Vec<GCObjectRef>) {
        debug!("Process los new_objects (size {})", new_objects.len());
        for object in new_objects {
            self.increment(immix_space, object, false);
            self.decrement(object);
        }
    }

    /// For deferred coalesced reference counting every remembered object will
    /// be processed to increment (and potentially evacuate) the members.
    fn process_mod_buffer(&mut self, immix_space: &mut ImmixSpace) {
        debug!("Process mod buffer (size {})", self.modified_buffer.len());
        while let Some(object) = self.modified_buffer.pop_front() {
            debug!("Process object {:p} in mod buffer", object);
            unsafe { (*object).set_logged(false); }
            if immix_space.is_in_space(object) {
                immix_space.set_gc_object(object);
                immix_space.increment_lines(object);
            }
            let children = unsafe{ (*object).children() };
            for (num, child) in children.enumerate() {
                if let Some(new_child) = unsafe{ (*child).is_forwarded() } {
                    debug!("Child {:p} is forwarded to {:p}", child, new_child);
                    unsafe{ (*object).set_member(num, new_child); }
                    self.increment(immix_space, child, false);
                } else {
                    if let Some(new_child) = self.increment(immix_space,
                                                            child, true) {
                        unsafe{ (*object).set_member(num, new_child); }
                    }
                }
            }
        }
    }

    /// The enqueued decrements are applied.
    ///
    /// If the reference counter drops to zero the memory is reclaimed and the
    /// members are enqueued for a decrement.
    fn process_decrement_buffer(&mut self, immix_space: &mut ImmixSpace,
                                large_object_space: &mut LargeObjectSpace) {
        debug!("Process dec buffer (size {})", self.decrement_buffer.len());
        while let Some(object) =  self.decrement_buffer.pop_front() {
            debug!("Process object {:p} in dec buffer", object);
            if unsafe{ (*object).decrement() && !(*object).is_pinned() }  {
                for child in unsafe{ (*object).children() } {
                    self.decrement(child);
                }
                if immix_space.is_gc_object(object) {
                    immix_space.decrement_lines(object);
                    immix_space.unset_gc_object(object);
                    valgrind_freelike!(object);
                } else if large_object_space.is_gc_object(object) {
                    large_object_space.enqueue_free(object);
                }
            }
        }
    }
}
