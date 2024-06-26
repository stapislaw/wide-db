use std::{collections::LinkedList, ops::{Bound, Range, RangeBounds}, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc, Mutex, RwLock}};

use bytes::Bytes;
use dashmap::{iter::Iter, mapref::one::Ref, DashMap};
use itertools::kmerge;
use log::debug;

use crate::{cell::{Cell, CellType}, delete_tracker::DeleteTracker, key_value::KeyValue, kv_scanner::KVScanner, memtable::Memtable, row_lock::RowLockContext, storage_engine, utils::{hashed_bytes::HashedBytes, sstable::SSTable}, PersistanceLayer, StorageEngine};

use super::{table_family::TableFamily};

pub struct Table {
    id: u64,
    name: Bytes,
    families: DashMap<u64, TableFamily>,
    row_locks: DashMap<u64, RowLockContext>,
    families_lock: Mutex<()>,
    mvcc_read_point: AtomicU64,
    mvcc_write_point: AtomicU64,
    mvcc_write_queue: Mutex<LinkedList<Arc<MVCCWriteEntry>>>,
}

impl Table {
    pub fn new(id: u64, name: Bytes) -> Table {
        Table {
            id,
            name,
            families: DashMap::new(),
            row_locks: DashMap::new(),
            families_lock: Mutex::new(()),
            mvcc_read_point: AtomicU64::new(0),
            mvcc_write_point: AtomicU64::new(0),
            mvcc_write_queue: Mutex::new(LinkedList::new()),
        }
    }

    pub fn new_from_families_vec(id: u64, name: Bytes, mvcc_id: u64, families_data: Vec<(Bytes, Vec<SSTable>)>) -> Table {
        let families = DashMap::new();
        for family_data in families_data {
            let name = HashedBytes::from_bytes(family_data.0);
            let id = *name.hash_as_ref();
            
            families.insert(
                id, 
                TableFamily::new_from_segments_vec(id, name.bytes_as_ref().clone(), family_data.1)
            );
        }

        Table {
            id,
            name,
            families,
            row_locks: DashMap::new(),
            families_lock: Mutex::new(()),
            mvcc_read_point: AtomicU64::new(mvcc_id),
            mvcc_write_point: AtomicU64::new(mvcc_id),
            mvcc_write_queue: Mutex::new(LinkedList::new()),
        }
    }

    pub fn get_name(&self) -> Bytes {
        self.name.clone()
    }

    pub fn get_family(&self, name: &Bytes) -> Option<Ref<u64, TableFamily>> {
        let name = HashedBytes::from_bytes(name.clone());

        self.families.get(name.hash_as_ref())
    }

    pub fn get_families_iter(&self) -> Iter<u64, TableFamily> {
        self.families.iter()
    }

    pub fn create_family(&mut self, name: Bytes) -> Result<(), &'static str> {
        let name = HashedBytes::from_bytes(name);
        let id = *name.hash_as_ref();
        
        let _lock = self.families_lock.lock().unwrap();

        if self.families.contains_key(&id) {
            return Err("Family with this name already exists.")
        }

        let family = TableFamily::new(id, name.bytes_as_ref().clone());
        self.families.insert(id, family);

        Ok(())
    }
 
    pub fn get_row_lock(&self, row: &HashedBytes) -> dashmap::mapref::one::RefMut<u64, RowLockContext, std::hash::RandomState> {
        let hash = *row.hash_as_ref();
        
        let lock = self.row_locks.entry(hash).or_insert_with(|| RowLockContext {
            row: row.clone(),
            lock: RwLock::new(true),
        });
        lock
    }

    pub fn mvcc_new_write(&self) -> Arc<MVCCWriteEntry> {
        let prev = self.mvcc_write_point.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let write_num = prev + 1;
        let write_entry = Arc::new(MVCCWriteEntry {
            write_num,
            completed: AtomicBool::new(false),
        });
        self.mvcc_write_queue.lock().unwrap().push_back(write_entry.clone());
        write_entry
    }

    pub fn mvcc_get_read_point(&self) -> u64 {
        self.mvcc_read_point.load(Ordering::Relaxed)
    }

    pub fn mvcc_complete(&self, write_entry: Arc<MVCCWriteEntry>) {
        write_entry.mark_as_completed();
        let mut queue = self.mvcc_write_queue.lock().unwrap();

        let mut read_point = self.mvcc_get_read_point();
        while !queue.is_empty() {
            let first = queue.front().unwrap();
            debug!("Read point: {} ; Write point: {}", read_point, first.write_num);
            if read_point + 1 != first.write_num {
                panic!("MVCC has left the chat...");
            }

            if first.completed.load(Ordering::Relaxed) {
                read_point = first.write_num;
                queue.pop_front();
            } else {
                break;
            }
        }

        self.mvcc_read_point.store(read_point, Ordering::Relaxed);
        debug!("MVCC new read point: {}", read_point);
    }

    pub fn scan<P: PersistanceLayer>(&self, persitance: &P, start: Option<KeyValue>, end: Option<KeyValue>) -> impl Iterator<Item = KeyValue> + '_ {
        let read_point = self.mvcc_get_read_point();

        let mut iters = vec![];
        for family in self.families.iter() {
            iters.push(family.scan(persitance, start.clone(), end.clone(), Some(read_point)).collect::<Vec<KeyValue>>());
        }
        
        let merge_iter = kmerge(iters);

        let mut delete_tracker = DeleteTracker::new();

        let mut current_row: Vec<u8> = vec![];
        merge_iter.map(move |cell: KeyValue| {
            let row = cell.get_row();
            if row != current_row {
                delete_tracker.reset();
                current_row = row.to_vec();
            }

            delete_tracker.add(&cell);

            match cell.get_cell_type() {
                CellType::Put => {
                    if delete_tracker.is_deleted(&cell) {
                        return None;
                    }
                    return Some(cell);
                },
                _ => {},
            }
            return None;
        }).flatten()
    }
}

#[derive(Debug)]
pub struct MVCCWriteEntry {
    write_num: u64,
    completed: AtomicBool,
}

impl MVCCWriteEntry {
    pub fn get_write_num(&self) -> u64 {
        self.write_num
    }

    fn mark_as_completed(&self) {
        self.completed.store(true, Ordering::Relaxed);
    }
}