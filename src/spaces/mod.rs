// Copyright (c) <2015> <lummax>
// Licensed under MIT (http://opensource.org/licenses/MIT)

mod immix_space;

pub use self::immix_space::ImmixSpace;
pub use self::immix_space::RCCollector;
pub use self::immix_space::ImmixCollector;

use constants::{BLOCK_SIZE, LINE_SIZE};
use gc_object::{GCRTTI, GCObjectRef};

pub struct Spaces {
    immix_space: ImmixSpace,
    rc_collector: RCCollector,
}

impl Spaces {
    pub fn new() -> Spaces {
        return Spaces {
            immix_space: ImmixSpace::new(),
            rc_collector: RCCollector::new(),
        };
    }

    pub fn allocate(&mut self, rtti: *const GCRTTI) -> Option<GCObjectRef>{
        // XXX use LOS if size > BLOCK_SIZE - LINE_SIZE
        assert!(unsafe{ (*rtti).object_size() } <= BLOCK_SIZE - LINE_SIZE);
        return self.immix_space.allocate(rtti)
                                  .or_else(|| { self.collect(true, true);
                                                self.allocate(rtti) });
    }

    pub fn collect(&mut self, evacuation: bool, cycle_collect: bool) {
        debug!("Requested collection (evacuation={}, cycle_collect={})",
               evacuation, cycle_collect);
        self.immix_space.collect(evacuation, cycle_collect, &mut self.rc_collector);
        valgrind_assert_no_leaks!();
    }

    pub fn write_barrier(&mut self, object: GCObjectRef) {
        if self.immix_space.is_gc_object(object) {
            self.rc_collector.write_barrier(object);
        }
    }
}

