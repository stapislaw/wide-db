use std::{io::{Read, Seek, SeekFrom}, iter, path::PathBuf};

use bytes::Bytes;
use crossbeam_skiplist::{map::Entry, SkipMap};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::key_value::KeyValue;

use super::{data_block::DataBlock, sstable_reader::SSTableReader};

#[derive(Serialize, Deserialize)]
pub struct SSTableFooter {
    pub index_pos: u64,
    pub index_size: u64,
}

#[derive(Serialize, Deserialize)]
pub struct SSTableIndex {
    pub row: String,
    pub column_name: String,
    pub timestamp: u64,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone)]
pub struct SSTableRow {
    pub row: String,
    pub column_name: String,
    pub timestamp: u64,
    pub data: Bytes,
}

pub struct SSTable {
    table: Bytes, 
    family: Bytes, 
    segment: Bytes,
    index: SkipMap<KeyValue, DataBlock>,
    max_mvcc_id: u64,
}

impl SSTable {
    pub fn new(table: &Bytes, family: &Bytes, segment: &Bytes, index: SkipMap<KeyValue, DataBlock>, max_mvcc_id: u64,) -> SSTable {
        SSTable { table: table.clone(), family: family.clone(), segment: segment.clone(), index, max_mvcc_id }
    }

    pub fn read<R: Read + Seek>(table: &Bytes, family: &Bytes, segment: &Bytes, r: R) -> SSTable {
        let mut reader = SSTableReader::new(r);
        
        let index = reader.read_index();
        SSTable::new(table, family, segment, index, reader.max_mvcc_id())
    }

    pub fn get_max_mvcc_id(&self) -> u64 {
        self.max_mvcc_id
    }

    pub fn get_blocks(&self, start: Option<KeyValue>, end: Option<KeyValue>) -> Vec<DataBlock> {
        let entry = match start {
            Some(start) => {
                self.index.lower_bound(std::ops::Bound::Included(&start))  
            },
            None => {
                self.index.front()
            },
        };

        let iter = SSTableIter::new(entry, end);
        iter.map(|entry| {
            entry.value().clone()
        }).collect_vec()
    }

    pub fn get_table(&self) -> &Bytes {
        &self.table
    }

    pub fn get_family(&self) -> &Bytes {
        &self.family
    }

    pub fn get_segment(&self) -> &Bytes {
        &self.segment
    }
}

impl Clone for SSTable {
    fn clone(&self) -> Self {
        SSTable { 
            table: self.table.clone(), 
            family: self.family.clone(), 
            segment: self.segment.clone(), 
            index: SkipMap::from_iter(
                self.index.iter().map(|entry| { 
                    (entry.key().clone(), entry.value().clone()) 
                })
            ),
            max_mvcc_id: self.max_mvcc_id.clone(),
        }
    }
}

struct SSTableIter<'a> {
    entry: Option<Entry<'a, KeyValue, DataBlock>>,
    end: Option<KeyValue>,
}

impl<'a> SSTableIter<'a> {
    fn new(entry: Option<Entry<'a, KeyValue, DataBlock>>, end: Option<KeyValue>) -> SSTableIter<'a> {
        SSTableIter { entry, end }
    }
}

impl<'a> Iterator for SSTableIter<'a> {
    type Item = Entry<'a, KeyValue, DataBlock>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.entry.clone() {
            None => None,
            Some(entry) => {
                if let Some(end) = &self.end {
                    if entry.key() > end {
                        return None;
                    }
                }

                self.entry = entry.next();
                Some(entry)
            },
        }
    }
}