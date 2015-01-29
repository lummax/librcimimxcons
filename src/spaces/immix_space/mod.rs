// Copyright (c) <2015> <lummax>
// Licensed under MIT (http://opensource.org/licenses/MIT)

mod block_info;
mod block_allocator;
mod collector;

pub use self::collector::ImmixCollector;
pub use self::collector::RCCollector;

use self::block_info::BlockInfo;
use self::block_allocator::BlockAllocator;

use std::collections::{RingBuf, HashSet, VecMap};
use std::{mem, ptr};

use constants::{BLOCK_SIZE, LINE_SIZE, NUM_LINES_PER_BLOCK, EVAC_HEADROOM,
                CICLE_TRIGGER_THRESHHOLD, EVAC_TRIGGER_THRESHHOLD};
use gc_object::{GCRTTI, GCObject, GCObjectRef};

type BlockTuple = (*mut BlockInfo, u16, u16);

pub struct ImmixSpace {
    block_allocator: BlockAllocator,
    object_map_backup: HashSet<GCObjectRef>,
    mark_histogram: VecMap<u8>,
    unavailable_blocks: RingBuf<*mut BlockInfo>,
    recyclable_blocks: RingBuf<*mut BlockInfo>,
    evac_headroom: RingBuf<*mut BlockInfo>,
    current_block: Option<BlockTuple>,
    overflow_block: Option<BlockTuple>,
    current_live_mark: bool,
    perform_evac: bool,
}

impl ImmixSpace {
    pub fn new() -> ImmixSpace {
        return ImmixSpace {
            block_allocator: BlockAllocator::new(),
            object_map_backup: HashSet::new(),
            mark_histogram: VecMap::with_capacity(NUM_LINES_PER_BLOCK),
            unavailable_blocks: RingBuf::new(),
            recyclable_blocks: RingBuf::new(),
            evac_headroom: RingBuf::new(),
            current_block: None,
            overflow_block: None,
            current_live_mark: false,
            perform_evac: false,
        };
    }

    pub fn set_gc_object(&mut self, object: GCObjectRef) {
        debug_assert!(self.is_in_space(object), "set_gc_object() on invalid space");
        unsafe{ (*self.get_block_ptr(object)).set_gc_object(object); }
    }

    pub fn unset_gc_object(&mut self, object: GCObjectRef) {
        debug_assert!(self.is_in_space(object), "unset_gc_object() on invalid space");
        unsafe{ (*self.get_block_ptr(object)).unset_gc_object(object); }
    }

    pub fn is_gc_object(&mut self, object: GCObjectRef) -> bool {
        if self.is_in_space(object) {
            return unsafe{ (*self.get_block_ptr(object)).is_gc_object(object) };
        }
        return false;
    }

    pub fn is_in_space(&self, object: GCObjectRef) -> bool {
        return self.block_allocator.is_in_space(object);
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
            return Some(object);
        }
        return None;
    }

    pub fn maybe_evacuate(&mut self, object: GCObjectRef) -> Option<GCObjectRef> {
        let block_info = unsafe{ self.get_block_ptr(object) };
        let is_pinned = unsafe{ (*object).is_pinned() };
        let is_candidate = unsafe{ (*block_info).is_evacuation_candidate() };
        if is_pinned || !is_candidate {
            return None;
        }
        let size = unsafe{ (*object).object_size() };
        if let Some(new_object) = self.raw_allocate(size) {
            unsafe{
                ptr::copy_nonoverlapping_memory(new_object as *mut u8,
                                                object as *const u8, size);
                debug_assert!(*object == *new_object,
                              "Evacuated object was not copied correcty");
                (*object).set_forwarded(new_object);
                self.unset_gc_object(object);
            }
            debug!("Evacuated object {:p} from block {:p} to {:p}", object,
                   block_info, new_object);
            valgrind_freelike!(object);
            return Some(new_object);
        }
        debug!("Can't evacuation object {:p} from block {:p}", object, block_info);
        return None;
    }

    pub fn prepare_collection(&mut self, evacuation: bool, cycle_collect: bool) -> bool {
        self.unavailable_blocks.extend(self.recyclable_blocks.drain());
        self.unavailable_blocks.extend(self.current_block.take()
                                           .map(|b| b.0).into_iter());

        let available_blocks = self.block_allocator.available_blocks();
        let total_blocks = self.block_allocator.total_blocks();

        let evac_threshhold = ((total_blocks as f32) * EVAC_TRIGGER_THRESHHOLD) as usize;
        let available_evac_blocks = available_blocks + self.evac_headroom.len();
        if evacuation || available_evac_blocks < evac_threshhold {
            let hole_threshhold = self.establish_hole_threshhold();
            self.perform_evac = hole_threshhold > 0
                && hole_threshhold < NUM_LINES_PER_BLOCK as u8;
            if self.perform_evac {
                debug!("Performing evacuation with hole_threshhold={} and evac_headroom={}",
                       hole_threshhold, self.evac_headroom.len());
                for block in self.unavailable_blocks.iter_mut() {
                    unsafe{ (**block).set_evacuation_candidate(hole_threshhold); }
                }
            }
        }

        if !cycle_collect {
            let cycle_theshold = ((total_blocks as f32) * CICLE_TRIGGER_THRESHHOLD) as usize;
            return self.block_allocator.available_blocks() < cycle_theshold;
        }
        return true;
    }

    pub fn complete_collection(&mut self) {
        self.mark_histogram.clear();
        self.perform_evac = false;
        self.sweep_unavailable_blocks();
    }

    pub fn prepare_rc_collection(&mut self) {
        if cfg!(feature = "valgrind") {
            for block in self.unavailable_blocks.iter_mut() {
                let block_new_objects = unsafe{ (**block).get_new_objects() };
                self.object_map_backup.extend(block_new_objects.into_iter());
            }
        }

        for block in self.unavailable_blocks.iter_mut() {
            unsafe{ (**block).remove_new_objects_from_map(); }
        }
    }

    pub fn complete_rc_collection(&mut self) {
        if cfg!(feature = "valgrind") {
            let mut object_map = HashSet::new();
            for block in self.unavailable_blocks.iter_mut() {
                let block_object_map = unsafe{ (**block).get_object_map() };
                object_map.extend(block_object_map.into_iter());
            }
            for &object in self.object_map_backup.difference(&object_map) {
                valgrind_freelike!(object);
            }
            self.object_map_backup.clear();
        }
    }

    pub fn prepare_immix_collection(&mut self) {
        if cfg!(feature = "valgrind") {
            for block in self.unavailable_blocks.iter_mut() {
                let block_object_map = unsafe{ (**block).get_object_map() };
                self.object_map_backup.extend(block_object_map.into_iter());
            }
        }

        for block in self.unavailable_blocks.iter_mut() {
            unsafe{ (**block).clear_line_counts(); }
            unsafe{ (**block).clear_object_map(); }
        }
    }

    pub fn complete_immix_collection(&mut self) {
        self.current_live_mark = !self.current_live_mark;

        if cfg!(feature = "valgrind") {
            let mut object_map = HashSet::new();
            for block in self.unavailable_blocks.iter_mut() {
                let block_object_map = unsafe{ (**block).get_object_map() };
                object_map.extend(block_object_map.into_iter());
            }
            for &object in self.object_map_backup.difference(&object_map) {
                valgrind_freelike!(object);
            }
            self.object_map_backup.clear();
        }
    }
}

impl ImmixSpace {
    unsafe fn get_block_ptr(&mut self, object: GCObjectRef) -> *mut BlockInfo {
        let block_offset = object as usize % BLOCK_SIZE;
        let block = mem::transmute((object as *mut u8).offset(-(block_offset as isize)));
        debug!("Block for object {:p}: {:p} with offset: {}", object, block, block_offset);
        return block;
    }

    fn set_new_object(&mut self, object: GCObjectRef) {
        debug_assert!(self.is_in_space(object), "set_new_object() on invalid space");
        unsafe{ (*self.get_block_ptr(object)).set_new_object(object); }
    }

    fn raw_allocate(&mut self, size: usize) -> Option<GCObjectRef> {
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
             valgrind_malloclike!(object, size);
             self.set_gc_object(object);
             self.set_new_object(object);
             object
         });
    }

    fn scan_for_hole(&mut self, size: usize, block_tuple: BlockTuple)
        -> Option<BlockTuple> {
            let (block, low, high) = block_tuple;
            return match (high - low) as usize >= size {
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

    fn scan_recyclables(&mut self, size: usize) -> Option<BlockTuple> {
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

    fn allocate_from_block(&mut self, size: usize, block_tuple: BlockTuple)
        -> (BlockTuple, GCObjectRef) {
            let (block, low, high) = block_tuple;
            let object = unsafe { (*block).offset(low as usize) };
            debug!("Allocated object {:p} of size {} in {:p} (object={})",
                   object, size, block, size >= LINE_SIZE);
            return ((block, low + size as u16, high), object);
        }

    fn get_new_block(&mut self) -> Option<BlockTuple> {
        return if self.perform_evac {
            debug!("Request new block in evacuation");
            self.evac_headroom.pop_front()
        } else {
            debug!("Request new block");
            self.block_allocator.get_block()
        }.map(|b| unsafe{ (*b).set_allocated(); b })
         .map(|block| (block, LINE_SIZE as u16, (BLOCK_SIZE - 1) as u16));
    }

    fn sweep_unavailable_blocks(&mut self) {
        let mut unavailable_blocks = RingBuf::new();
        for block in self.unavailable_blocks.drain() {
            if unsafe{ (*block).is_empty() } {
                if cfg!(feature = "valgrind") {
                    let block_object_map = unsafe{ (*block).get_object_map() };
                    for &object in block_object_map.iter() {
                        valgrind_freelike!(object);
                    }
                }
                unsafe{ (*block).reset() ;}

                // XXX We should not use a constant here, but something that
                // XXX changes dynamically (see rcimmix: MAX heuristic).
                if self.evac_headroom.len() < EVAC_HEADROOM {
                    debug!("Buffer free block {:p} for evacuation", block);
                    self.evac_headroom.push_back(block);
                } else {
                    debug!("Return block {:p} to global block allocator", block);
                    self.block_allocator.return_block(block);
                }
            } else {
                unsafe{ (*block).count_holes(); }
                let (holes, marked_lines) = unsafe{ (*block).count_holes_and_marked_lines() };
                if self.mark_histogram.contains_key(&(holes as usize)) {
                    if let Some(val) = self.mark_histogram.get_mut(&(holes as usize)) {
                        *val += marked_lines;
                    }
                } else { self.mark_histogram.insert(holes as usize, marked_lines); }
                debug!("Found {} holes and {} marked lines in block {:p}",
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

    fn establish_hole_threshhold(&self) -> u8 {
        let mut available_histogram : VecMap<u8> = VecMap::with_capacity(NUM_LINES_PER_BLOCK);
        for block in self.unavailable_blocks.iter() {
            let (holes, free_lines) = unsafe{ (**block).count_holes_and_available_lines() };
            if available_histogram.contains_key(&(holes as usize)) {
                if let Some(val) = available_histogram.get_mut(&(holes as usize)) {
                    *val += free_lines;
                }
            } else { available_histogram.insert(holes as usize, free_lines); }
        }
        let mut required_lines = 0 as u8;
        let mut available_lines = (self.evac_headroom.len() * (NUM_LINES_PER_BLOCK - 1)) as u8;

        for threshold in (0..NUM_LINES_PER_BLOCK) {
            required_lines += *self.mark_histogram.get(&threshold).unwrap_or(&0);
            available_lines -= *available_histogram.get(&threshold).unwrap_or(&0);
            if available_lines <= required_lines {
                return threshold as u8;
            }
        }
        return NUM_LINES_PER_BLOCK as u8;
    }
}