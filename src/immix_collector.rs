// Copyright (c) <2015> <lummax>
// Licensed under MIT (http://opensource.org/licenses/MIT)

use std::collections::RingBuf;

use gc_object::GCObject;
use line_allocator::LineAllocator;

pub struct ImmixCollector;

impl ImmixCollector {
    pub fn collect(line_allocator: &mut LineAllocator, roots: &[*mut GCObject]) {
        let next_live_mark = !line_allocator.current_live_mark();
        debug!("Start Immix collection with {} roots and next_live_mark: {}",
               roots.len(), next_live_mark);
        line_allocator.clear_line_counts();
        line_allocator.clear_object_map();
        let mut object_queue = roots.iter().map(|o| *o)
                                    .collect::<RingBuf<*mut GCObject>>();
        loop {
            match object_queue.pop_front() {
                None => break,
                Some(object) => {
                    debug!("Process object {} in Immix closure", object);
                    if !unsafe { (*object).set_marked(next_live_mark) } {
                        debug!("Object {} was unmarked: process children", object);
                        line_allocator.set_gc_object(object);
                        line_allocator.increment_lines(object);
                        for child in unsafe{ (*object).children() }.into_iter() {
                            if !unsafe{ (*child).is_marked(next_live_mark) } {
                                debug!("Push child {} into object queue", child);
                                object_queue.push_back(child);
                            }
                        }
                    }
                }
            }
        }
        debug!("Complete collection");
        line_allocator.invert_live_mark();
        line_allocator.complete_collection();
    }
}
