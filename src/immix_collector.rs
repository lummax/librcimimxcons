// Copyright (c) <2015> <lummax>
// Licensed under MIT (http://opensource.org/licenses/MIT)

use std::collections::RingBuf;

use gc_object::GCObjectRef;
use immix_space::ImmixSpace;

pub struct ImmixCollector;

impl ImmixCollector {
    pub fn collect(immix_space: &mut ImmixSpace, roots: &[GCObjectRef]) {
        let next_live_mark = !immix_space.current_live_mark();
        debug!("Start Immix collection with {} roots and next_live_mark: {}",
               roots.len(), next_live_mark);
        immix_space.prepare_immix_collection();
        let mut object_queue = RingBuf::new();
        for root in roots.iter().map(|o| *o) {
            unsafe{ (*root).set_pinned(true); }
            object_queue.push_back(root);
        }
        while let Some(object) =  object_queue.pop_front() {
            debug!("Process object {:p} in Immix closure", object);
            if !unsafe { (*object).set_marked(next_live_mark) } {
                immix_space.set_gc_object(object);
                immix_space.increment_lines(object);
                debug!("Object {:p} was unmarked: process children", object);
                let children = unsafe{ (*object).children() };
                for (num, mut child) in children.into_iter().enumerate() {
                    if let Some(new_child) = unsafe{ (*child).is_forwarded() } {
                        debug!("Child {:p} is forwarded to {:p}", child, new_child);
                        unsafe{ (*object).set_child(num, new_child); }
                    } else if !unsafe{ (*child).is_marked(next_live_mark) } {
                        if let Some(new_child) = immix_space.maybe_evacuate(child) {
                            debug!("Evacuated child {:p} to {:p}", child, new_child);
                            unsafe{ (*object).set_child(num, new_child); }
                            child = new_child;
                        }
                        debug!("Push child {:p} into object queue", child);
                        object_queue.push_back(child);
                    }
                }
            }
        }
        for root in roots.iter() {
            unsafe{ (**root).set_pinned(false); }
        }
        immix_space.complete_immix_collection();
        debug!("Complete collection");
    }
}
