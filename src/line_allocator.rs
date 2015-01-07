// Copyright (c) <2015> <lummax>
// Licensed under MIT (http://opensource.org/licenses/MIT)

use std::collections::{RingBuf, HashSet, VecMap};
use std::mem;
use std::ptr;

use block_allocator::BlockAllocator;
use block_info::BlockInfo;
use constants::{BLOCK_SIZE, LINE_SIZE, NUM_LINES_PER_BLOCK};
use gc_object::{GCRTTI, GCObject, GCObjectRef};

type BlockTuple = (*mut BlockInfo, u16, u16);

pub struct LineAllocator {
    block_allocator: BlockAllocator,
    object_map: HashSet<GCObjectRef>,
    object_map_backup: HashSet<GCObjectRef>,
    mark_histogram: VecMap<u8>,
    unavailable_blocks: RingBuf<*mut BlockInfo>,
    recyclable_blocks: RingBuf<*mut BlockInfo>,
    current_block: Option<BlockTuple>,
    overflow_block: Option<BlockTuple>,
    current_live_mark: bool,
}

impl LineAllocator {
    pub fn new(block_allocator: BlockAllocator) -> LineAllocator {
        return LineAllocator {
            block_allocator: block_allocator,
            object_map: HashSet::new(),
            object_map_backup: HashSet::new(),
            mark_histogram: VecMap::with_capacity(NUM_LINES_PER_BLOCK),
            unavailable_blocks: RingBuf::new(),
            recyclable_blocks: RingBuf::new(),
            current_block: None,
            overflow_block: None,
            current_live_mark: false,
        };
    }

    pub fn set_gc_object(&mut self, object: GCObjectRef) {
        self.object_map.insert(object);
    }

    pub fn unset_gc_object(&mut self, object: GCObjectRef) {
        self.object_map.remove(&object);
    }

    pub fn is_gc_object(&self, object: GCObjectRef) -> bool {
        return self.object_map.contains(&object);
    }

    pub fn current_live_mark(&self) -> bool {
        return self.current_live_mark;
    }

    pub fn decrement_lines(&mut self, object: GCObjectRef) {
        unsafe{ (*self.get_block_ptr(object)).decrement_lines(object); }
    }

    pub fn increment_lines(&mut self, object: GCObjectRef) {
        unsafe{ (*self.get_block_ptr(object)).increment_lines(object); }
    }

    pub fn allocate(&mut self, rtti: *const GCRTTI) -> Option<GCObjectRef> {
        let size = unsafe{ (*rtti).object_size() };
        debug!("Request to allocate an object of size {}", size);
        if let Some(object) = self.raw_allocate(size) {
            unsafe { ptr::write(object, GCObject::new(rtti, self.current_live_mark)); }
            valgrind_malloclike!(object, size);
            return Some(object);
        }
        return None;
    }

    pub fn prepare_collection(&mut self) -> bool {
        self.unavailable_blocks.extend(self.recyclable_blocks.drain());
        self.unavailable_blocks.extend(self.current_block.take()
                                           .map(|b| b.0).into_iter());

        let perform_cycle_collection = true;
        return perform_cycle_collection;
    }

    pub fn complete_collection(&mut self) {
        self.mark_histogram.clear();
        let mut unavailable_blocks = RingBuf::new();
        for block in self.unavailable_blocks.drain() {
            if unsafe{ (*block).is_empty() } {
                debug!("Return block {:p} to global block allocator", block);
                self.block_allocator.return_block(block);
            } else {
                let (holes, marked_lines) = unsafe{ (*block).count_holes_and_marked_lines() };
                if self.mark_histogram.contains_key(&(holes as uint)) {
                    if let Some(val) = self.mark_histogram.get_mut(&(holes as uint)) {
                        *val += marked_lines;
                    }
                } else { self.mark_histogram.insert(holes as uint, marked_lines); }
                debug!("Found {} holes and {} marked lines in block {}",
                       holes, marked_lines, block);
                match holes {
                    0 => {
                        debug!("Push block {:p} into unavailable_blocks", block);
                        unavailable_blocks.push_back(block);
                    },
                    _ => {
                        debug!("Push block {:p} into recyclable_blocks", block);
                        self.recyclable_blocks.push_back(block);
                    }
                }
            }
        }
        self.unavailable_blocks.extend(unavailable_blocks.into_iter());
    }

    pub fn prepare_immix_collection(&mut self) {
        for block in self.unavailable_blocks.iter_mut() {
            unsafe{ (**block).clear_line_counts(); }
        }

        if cfg!(feature = "valgrind") {
            self.object_map_backup = self.object_map.clone();
        }
        self.object_map.clear();
    }

    pub fn complete_immix_collection(&mut self) {
        self.current_live_mark = !self.current_live_mark;
        if cfg!(feature = "valgrind") {
            for &object in self.object_map_backup.difference(&self.object_map) {
                valgrind_freelike!(object);
            }
            self.object_map_backup.clear();
        }
    }
}

impl LineAllocator {
    unsafe fn get_block_ptr(&mut self, object: GCObjectRef) -> *mut BlockInfo {
        let block_offset = object as uint % BLOCK_SIZE;
        return mem::transmute((object as *mut u8).offset(-(block_offset as int)));
    }

    fn raw_allocate(&mut self, size: uint) -> Option<GCObjectRef> {
        return if size < LINE_SIZE {
            self.current_block.take()
                              .and_then(|tp| self.scan_for_hole(size, tp))
        } else {
            self.overflow_block.take()
                               .and_then(|tp| self.scan_for_hole(size, tp))
                               .or_else(|| self.get_new_block())
        }.or_else(|| self.scan_recyclables(size))
         .or_else(|| self.get_new_block())
         .map(|tp| self.allocate_from_block(size, tp))
         .map(|(tp, object)| {
             if size < LINE_SIZE { self.current_block = Some(tp);
             } else { self.overflow_block = Some(tp); }
             object
         });
    }

    fn scan_for_hole(&mut self, size: uint, block_tuple: BlockTuple)
        -> Option<BlockTuple> {
            let (block, low, high) = block_tuple;
            return match (high - low) as uint >= size {
                true => {
                    debug!("Found hole in block {:p}", block);
                    Some(block_tuple)
                },
                false => match unsafe{ (*block).scan_block(high) } {
                    None => {
                        debug!("Push block {:p} into unavailable_blocks", block);
                        self.unavailable_blocks.push_back(block);
                        None
                    },
                    Some((low, high)) =>
                        self.scan_for_hole(size, (block, low, high)),
                }
            };
        }

    fn scan_recyclables(&mut self, size: uint) -> Option<BlockTuple> {
        return match self.recyclable_blocks.pop_front() {
            None => None,
            Some(block) => match unsafe{ (*block).scan_block((LINE_SIZE - 1) as u16) } {
                None => {
                    debug!("Push block {:p} into unavailable_blocks", block);
                    self.unavailable_blocks.push_back(block);
                    self.scan_recyclables(size)
                },
                Some((low, high)) => self.scan_for_hole(size, (block, low, high))
                                         .or_else(|| self.scan_recyclables(size)),
            }
        };
    }

    fn get_new_block(&mut self) -> Option<BlockTuple> {
        debug!("Request new block from block_allocator");
        return self.block_allocator.get_block()
                   .map(|block| (block, LINE_SIZE as u16, (BLOCK_SIZE - 1) as u16));
    }

    fn allocate_from_block(&mut self, size: uint, block_tuple: BlockTuple)
        -> (BlockTuple, GCObjectRef) {
            let (block, low, high) = block_tuple;
            let object = unsafe { (*block).offset(low as uint) };
            self.set_gc_object(object);
            debug!("Allocated object {} of size {} in {} (object={})",
            object, size, block, size >= LINE_SIZE);
            return ((block, low + size as u16, high), object);
        }
}
