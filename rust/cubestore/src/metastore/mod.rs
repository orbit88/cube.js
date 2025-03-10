pub mod schema;
pub mod table;
pub mod index;
pub mod partition;
pub mod chunks;
pub mod wal;
pub mod job;
pub mod listener;

use std::hash::{Hasher, Hash};
use std::{io::Cursor, sync::Arc, collections::{hash_map::DefaultHasher}, time, env};
use tokio::fs;
use rocksdb::{DB, WriteBatch, Options, DBIterator, WriteBatchIterator};
use tokio::sync::{RwLock, Notify};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use async_trait::async_trait;
use serde::{Deserialize, Serialize, Deserializer};
use log::{error, info};

use crate::CubeError;
use schema::{SchemaRocksTable, SchemaRocksIndex};
use table::{TableRocksIndex, TableRocksTable};
use index::{IndexRocksTable, IndexRocksIndex};
use partition::{PartitionRocksIndex, PartitionRocksTable};
use chunks::ChunkRocksTable;
use wal::WALRocksTable;
use parquet::{basic::{Type, LogicalType}, schema::types};
use crate::store::DataFrame;
use crate::table::{Row, TableValue};
use core::fmt;
use smallvec::alloc::fmt::Formatter;
use crate::metastore::index::IndexIndexKey;
use std::fmt::Debug;
use tokio::sync::broadcast::Sender;
use crate::metastore::job::{Job, JobRocksTable, JobRocksIndex, JobIndexKey, JobStatus};
use crate::metastore::partition::PartitionIndexKey;
use crate::metastore::chunks::{ChunkRocksIndex, ChunkIndexKey};
use crate::remotefs::{RemoteFs, LocalDirRemoteFs};
use std::path::{Path, PathBuf};
use std::time::{SystemTime};
use rocksdb::checkpoint::Checkpoint;
use arrow::datatypes::{Field, DataType};
use std::str::FromStr;
use itertools::Itertools;
use arrow::datatypes::TimeUnit::{Microsecond};
use parquet::basic::Repetition;
use tokio::fs::File;
use tokio::time::{Duration};
use regex::Regex;
use futures::future::join_all;
use table::Table;
use std::collections::HashMap;
use crate::metastore::table::{TablePath, TableIndexKey};
use crate::metastore::wal::{WALIndexKey, WALRocksIndex};

#[macro_export]
macro_rules! format_table_value {
    ($row:expr, $field:ident, $tt:ty) => { DataFrameValue::value(&$row.$field) };
}

#[macro_export]
macro_rules! data_frame_from {
    (
        $( #[$struct_attr:meta] )*
        pub struct $name:ident {
            $( $variant:ident : $tt:ty ),+
        }
    ) => {
        $( #[$struct_attr] )*
        pub struct $name {
            $( $variant : $tt ),+
        }

        impl From<Vec<IdRow<$name>>> for DataFrame {
            fn from(rows: Vec<IdRow<$name>>) -> Self {
                DataFrame::new(
                    vec![
                        Column::new("id".to_string(), ColumnType::Int, 0),
                        $( Column::new(std::stringify!($variant).to_string(), ColumnType::String, 1) ),+
                    ],
                    rows.iter().map(|r|
                        Row::new(vec![
                            TableValue::Int(r.id as i64),
                            $(
                                TableValue::String(format_table_value!(r.row, $variant, $tt))
                            ),+
                        ])
                    ).collect()
                )
            }
        }
    }
}

#[macro_export]
macro_rules! base_rocks_secondary_index {
    ($table: ty, $index: ty) => {
        impl BaseRocksSecondaryIndex<$table> for $index {
            fn index_key_by(&self, row: &$table) -> Vec<u8> {
                self.key_to_bytes(&self.typed_key_by(row))
            }

            fn get_id(&self) -> u32 {
                RocksSecondaryIndex::get_id(self)
            }

            fn is_unique(&self) -> bool {
                RocksSecondaryIndex::is_unique(self)
            }
        }
    }
}

#[macro_export]
macro_rules!
rocks_table_impl {
    ($table: ty, $rocks_table: ident, $table_id: expr, $indexes: block, $delete_event: tt) => {
        #[derive(Debug, Clone)]
        pub(crate) struct $rocks_table {
            db: Arc<DB>
        }

        impl $rocks_table {
            pub fn new(db: Arc<DB>) -> $rocks_table {
                $rocks_table {
                    db
                }
            }
        }

        impl RocksTable for $rocks_table {
            type T = $table;

            fn db(&self) -> Arc<DB> {
                self.db.clone()
            }

            fn table_id(&self) -> TableId {
                $table_id
            }

            fn index_id(&self, index_num: IndexId) -> IndexId {
                if index_num > 99 {
                    panic!("Too big index id: {}", index_num);
                }
                $table_id as IndexId + index_num
            }

            fn deserialize_row<'de, D>(&self, deserializer: D) -> Result<$table, <D as Deserializer<'de>>::Error> where
                D: Deserializer<'de> {
                <$table>::deserialize(deserializer)
            }

            fn indexes() -> Vec<Box<dyn BaseRocksSecondaryIndex<$table>>> {
                $indexes
            }

            fn delete_event(&self, row: IdRow<Self::T>) -> MetaStoreEvent {
                MetaStoreEvent::$delete_event(row)
            }
        }
    }
}

pub trait DataFrameValue<T> {
    fn value(v: &Self) -> T;
}

impl DataFrameValue<String> for String {
    fn value(v: &Self) -> String {
        v.to_string()
    }
}

impl DataFrameValue<String> for u64 {
    fn value(v: &Self) -> String {
        format!("{}", v)
    }
}

impl DataFrameValue<String> for bool {
    fn value(v: &Self) -> String {
        format!("{}", v)
    }
}

impl DataFrameValue<String> for Vec<Column> {
    fn value(v: &Self) -> String {
        serde_json::to_string(v).unwrap()
    }
}

impl DataFrameValue<String> for Option<String> {
    fn value(v: &Self) -> String {
        v.as_ref().map(|s| s.to_string()).unwrap_or("NULL".to_string())
    }
}

impl DataFrameValue<String> for Option<ImportFormat> {
    fn value(v: &Self) -> String {
        v.as_ref().map(|v| format!("{:?}", v)).unwrap_or("NULL".to_string())
    }
}

impl DataFrameValue<String> for Option<u64> {
    fn value(v: &Self) -> String {
        v.as_ref().map(|v| format!("{:?}", v)).unwrap_or("NULL".to_string())
    }
}

impl DataFrameValue<String> for Option<Row> {
    fn value(v: &Self) -> String {
        v.as_ref().map(|v| format!("({})", v.values().iter().map(|tv| match tv {
            TableValue::Null => "NULL".to_string(),
            TableValue::String(s) => format!("\"{}\"", s),
            TableValue::Int(i) => i.to_string(),
            TableValue::Timestamp(t) => format!("{:?}", t),
            TableValue::Bytes(b) => format!("{:?}", b),
            TableValue::Boolean(b) => format!("{:?}", b),
            TableValue::Decimal(v) => format!("{}", v),
        }).join(", "))).unwrap_or("NULL".to_string())
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub enum ColumnType {
    String,
    Int,
    Bytes,
    Timestamp,
    Decimal,
    Boolean
}

impl From<&Column> for parquet::schema::types::Type {
    fn from(column: &Column) -> Self {
        match column.get_column_type() {
            crate::metastore::ColumnType::String => {
                types::Type::primitive_type_builder(&column.get_name(), Type::BYTE_ARRAY)
                    .with_logical_type(LogicalType::UTF8)
                    .with_repetition(Repetition::OPTIONAL)
                    .build().unwrap()
            }
            crate::metastore::ColumnType::Int => {
                    types::Type::primitive_type_builder(&column.get_name(), Type::INT64)
                        .with_logical_type(LogicalType::INT_64)
                        .with_repetition(Repetition::OPTIONAL)
                        .build().unwrap()
            }
            crate::metastore::ColumnType::Decimal => {
                    types::Type::primitive_type_builder(&column.get_name(), Type::INT64)
                        //TODO DECIMAL?
                        .with_logical_type(LogicalType::DECIMAL)
                        .with_repetition(Repetition::OPTIONAL)
                        .build().unwrap()
            }
            crate::metastore::ColumnType::Bytes => {
                    types::Type::primitive_type_builder(&column.get_name(), Type::BYTE_ARRAY)
                        .with_logical_type(LogicalType::LIST)
                        .with_repetition(Repetition::OPTIONAL)
                        .build().unwrap()
            }
            crate::metastore::ColumnType::Timestamp => {
                    types::Type::primitive_type_builder(&column.get_name(), Type::INT64)
                        //TODO MICROS?
                        .with_logical_type(LogicalType::TIMESTAMP_MICROS)
                        .with_repetition(Repetition::OPTIONAL)
                        .build().unwrap()
            }
            crate::metastore::ColumnType::Boolean => {
                types::Type::primitive_type_builder(&column.get_name(), Type::BOOLEAN)
                    .with_repetition(Repetition::OPTIONAL)
                    .build().unwrap()
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct Column {
    name: String,
    column_type: ColumnType,
    column_index: usize
}

impl Into<Field> for Column {
    fn into(self) -> Field {
        Field::new(
            self.name.as_str(),
            match self.column_type {
                ColumnType::String => DataType::Utf8,
                ColumnType::Int => DataType::Int64,
                ColumnType::Timestamp => DataType::Timestamp(Microsecond, None),
                ColumnType::Boolean => DataType::Boolean,
                x => panic!("Unimplemented arrow type: {:?}", x)
            },
            false
        )
    }
}

impl fmt::Display for Column {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{} {}", self.name, match &self.column_type {
            ColumnType::String => "STRING",
            ColumnType::Int => "INT",
            ColumnType::Timestamp => "TIMESTAMP",
            ColumnType::Boolean => "BOOLEAN",
            x => panic!("TODO: {:?}", x)
        }))
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub enum ImportFormat {
    CSV
}

data_frame_from! {
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct Schema {
    name: String
}
}

data_frame_from! {
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct Index {
    name: String,
    table_id: u64,
    columns: Vec<Column>,
    sort_key_size: u64
}
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>
}

data_frame_from! {
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct Partition {
    index_id: u64,
    parent_partition_id: Option<u64>,
    min_value: Option<Row>,
    max_value: Option<Row>,
    active: bool,
    main_table_row_count: u64
}
}

data_frame_from! {
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct Chunk {
    partition_id: u64,
    row_count: u64,
    uploaded: bool,
    active: bool
}
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq, Hash)]
pub struct WAL {
    table_id: u64,
    row_count: u64,
    uploaded: bool
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct IdRow<T: Clone> {
    id: u64,
    row: T
}

impl<T: Clone> IdRow<T> {
    pub fn new(id: u64, row: T) -> IdRow<T> {
        IdRow { id, row }
    }
    pub fn get_id(&self) -> u64 {
        self.id
    }

    pub fn get_row(&self) -> &T {
        &self.row
    }

    pub fn into_row(self) -> T {
        self.row
    }
}

struct KeyVal {
    key: Vec<u8>,
    val: Vec<u8>
}

struct BatchPipe<'a> {
    db: &'a DB,
    write_batch: WriteBatch,
    events: Vec<MetaStoreEvent>
}

impl<'a> BatchPipe<'a> {
    fn new(db: &'a DB) -> BatchPipe<'a> {
        BatchPipe  {
            db,
            write_batch: WriteBatch::default(),
            events: Vec::new()
        }
    }

    fn batch(&mut self) -> &mut WriteBatch {
        &mut self.write_batch
    }

    fn add_event(&mut self, event: MetaStoreEvent) {
        self.events.push(event);
    }

    fn batch_write_rows(self) -> Result<Vec<MetaStoreEvent>, CubeError> {
        let db = self.db;
        db.write(self.write_batch)?;
        Ok(self.events)
    }
}

#[async_trait]
pub trait MetaStoreTable: Send + Sync {
    type T: Serialize + Clone + Debug;

    async fn all_rows(&self) -> Result<Vec<IdRow<Self::T>>, CubeError>;

    async fn row_by_id_or_not_found(&self, id: u64) -> Result<IdRow<Self::T>, CubeError>;

    async fn insert_row(&self, row: Self::T) -> Result<IdRow<Self::T>, CubeError>;
}

struct MetaStoreTableImpl<R: RocksTable + 'static, F: Fn(Arc<DB>) -> R + Send + Sync + Clone + 'static> {
    rocks_meta_store: RocksMetaStore,
    rocks_table_fn: F
}



#[async_trait]
impl<R: RocksTable + 'static, F: Fn(Arc<DB>) -> R + Send + Sync + Clone + 'static> MetaStoreTable for MetaStoreTableImpl<R, F> {
    type T = R::T;

    async fn all_rows(&self) -> Result<Vec<IdRow<Self::T>>, CubeError> {
        let table = self.rocks_table_fn.clone();
        self.rocks_meta_store.read_operation(move |db_ref| {
            Ok(table(db_ref).all_rows()?)
        }).await
    }

    async fn row_by_id_or_not_found(&self, id: u64) -> Result<IdRow<Self::T>, CubeError> {
        let table = self.rocks_table_fn.clone();
        self.rocks_meta_store.read_operation(move |db_ref| {
            Ok(table(db_ref).get_row_or_not_found(id)?)
        }).await
    }

    async fn insert_row(&self, row: Self::T) -> Result<IdRow<Self::T>, CubeError> {
        let table = self.rocks_table_fn.clone();
        self.rocks_meta_store.write_operation(move |db_ref, batch| {
            Ok(table(db_ref).insert(row, batch)?)
        }).await
    }
}

#[async_trait]
pub trait MetaStore: Send + Sync {
    async fn wait_for_current_seq_to_sync(&self) -> Result<(), CubeError>;
    fn schemas_table(&self) -> Box<dyn MetaStoreTable<T=Schema>>;
    async fn create_schema(&self, schema_name: String, if_not_exists: bool) -> Result<IdRow<Schema>, CubeError>;
    async fn get_schemas(&self) -> Result<Vec<IdRow<Schema>>, CubeError>;
    async fn get_schema_by_id(&self, schema_id: u64) -> Result<IdRow<Schema>, CubeError>;
    //TODO Option
    async fn get_schema_id(&self, schema_name: String) -> Result<u64, CubeError>;
    //TODO Option
    async fn get_schema(&self, schema_name: String) -> Result<IdRow<Schema>, CubeError>;
    async fn rename_schema(&self, old_schema_name: String, new_schema_name: String) -> Result<IdRow<Schema>, CubeError>;
    async fn rename_schema_by_id(&self, schema_id: u64, new_schema_name: String) -> Result<IdRow<Schema>, CubeError>;
    async fn delete_schema(&self, schema_name: String) -> Result<(), CubeError>;
    async fn delete_schema_by_id(&self, schema_id: u64) -> Result<(), CubeError>;

    fn tables_table(&self) -> Box<dyn MetaStoreTable<T=Table>>;
    async fn create_table(&self, schema_name: String, table_name: String, columns: Vec<Column>, location: Option<String>, import_format: Option<ImportFormat>, indexes: Vec<IndexDef>) -> Result<IdRow<Table>, CubeError>;
    async fn get_table(&self, schema_name: String, table_name: String) -> Result<IdRow<Table>, CubeError>;
    async fn get_table_by_id(&self, table_id: u64) -> Result<IdRow<Table>, CubeError>;
    async fn get_tables(&self) -> Result<Vec<IdRow<Table>>, CubeError>;
    async fn get_tables_with_path(&self) -> Result<Vec<TablePath>, CubeError>;
    async fn drop_table(&self, table_id: u64) -> Result<IdRow<Table>, CubeError>;

    fn partition_table(&self) -> Box<dyn MetaStoreTable<T=Partition>>;
    async fn create_partition(&self, partition: Partition) -> Result<IdRow<Partition>, CubeError>;
    async fn get_partition(&self, partition_id: u64) -> Result<IdRow<Partition>, CubeError>;
    async fn get_partition_for_compaction(&self, partition_id: u64) -> Result<(IdRow<Partition>, IdRow<Index>), CubeError>;
    async fn get_partition_chunk_sizes(&self, partition_id: u64) -> Result<u64, CubeError>;
    async fn swap_active_partitions(
        &self,
        current_active: Vec<u64>,
        new_active: Vec<u64>,
        compacted_chunk_ids: Vec<u64>,
        new_active_min_max: Vec<(u64, (Option<Row>, Option<Row>))>
    ) -> Result<(), CubeError>;

    fn index_table(&self) -> Box<dyn MetaStoreTable<T=Index>>;
    async fn get_default_index(&self, table_id: u64) -> Result<IdRow<Index>, CubeError>;
    async fn get_table_indexes(&self, table_id: u64) -> Result<Vec<IdRow<Index>>, CubeError>;
    async fn get_active_partitions_by_index_id(&self, index_id: u64) -> Result<Vec<IdRow<Partition>>, CubeError>;

    fn chunks_table(&self) -> Box<dyn MetaStoreTable<T=Chunk>>;
    async fn create_chunk(&self, partition_id: u64, row_count: usize) -> Result<IdRow<Chunk>, CubeError>;
    async fn get_chunk(&self, chunk_id: u64) -> Result<IdRow<Chunk>, CubeError>;
    async fn get_chunks_by_partition(&self, partition_id: u64) -> Result<Vec<IdRow<Chunk>>, CubeError>;
    async fn chunk_uploaded(&self, chunk_id: u64) -> Result<IdRow<Chunk>, CubeError>;
    async fn deactivate_chunk(&self, chunk_id: u64) -> Result<(), CubeError>;

    async fn create_wal(&self, table_id: u64, row_count: usize) -> Result<IdRow<WAL>, CubeError>;
    async fn get_wal(&self, wal_id: u64) -> Result<IdRow<WAL>, CubeError>;
    async fn delete_wal(&self, wal_id: u64) -> Result<(), CubeError>;
    async fn wal_uploaded(&self, wal_id: u64) -> Result<IdRow<WAL>, CubeError>;
    async fn get_wals_for_table(&self, table_id: u64) -> Result<Vec<IdRow<WAL>>, CubeError>;

    async fn add_job(&self, job: Job) -> Result<Option<IdRow<Job>>, CubeError>;
    async fn get_job(&self, job_id: u64) -> Result<IdRow<Job>, CubeError>;
    async fn delete_job(&self, job_id: u64) -> Result<IdRow<Job>, CubeError>;
    async fn start_processing_job(&self, server_name: String) -> Result<Option<IdRow<Job>>, CubeError>;
    async fn update_status(&self, job_id: u64, status: JobStatus) -> Result<IdRow<Job>, CubeError>;
    async fn update_heart_beat(&self, job_id: u64) -> Result<IdRow<Job>, CubeError>;
}

#[derive(Clone, Debug)]
pub enum MetaStoreEvent {
    Insert(TableId, u64),
    Update(TableId, u64),
    Delete(TableId, u64),
    DeleteChunk(IdRow<Chunk>),
    DeleteIndex(IdRow<Index>),
    DeleteJob(IdRow<Job>),
    DeletePartition(IdRow<Partition>),
    DeleteSchema(IdRow<Schema>),
    DeleteTable(IdRow<Table>),
    DeleteWal(IdRow<WAL>),
}

type SecondaryKey =  Vec<u8>;
type IndexId = u32;

#[derive(Debug, Clone, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub enum RowKey {
    Table(TableId, u64),
    Sequence(TableId),
    SecondaryIndex(IndexId, SecondaryKey, u64),
}

pub fn get_fixed_prefix() -> usize {
    13
}

impl RowKey {
    fn from_bytes(bytes: &[u8]) -> RowKey {
        let mut reader = Cursor::new(bytes);
        match reader.read_u8().unwrap() {
            1 => RowKey::Table(TableId::from(reader.read_u32::<BigEndian>().unwrap()), {
                // skip zero for fixed key padding
                reader.read_u64::<BigEndian>().unwrap();
                reader.read_u64::<BigEndian>().unwrap()
            }),
            2 => RowKey::Sequence(TableId::from(reader.read_u32::<BigEndian>().unwrap())),
            3 => {
                let table_id = IndexId::from(reader.read_u32::<BigEndian>().unwrap());
                let mut secondary_key: SecondaryKey = SecondaryKey::new();
                let sc_length = bytes.len() - 13;
                for _i in 0..sc_length {
                    secondary_key.push(reader.read_u8().unwrap());
                }
                let row_id = reader.read_u64::<BigEndian>().unwrap();

                RowKey::SecondaryIndex(table_id, secondary_key, row_id)
                },
            v => panic!("Unknown key prefix: {}", v)
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut wtr = vec![];
        match self {
            RowKey::Table(table_id, row_id) => {
                wtr.write_u8(1).unwrap();
                wtr.write_u32::<BigEndian>(*table_id as u32).unwrap();
                wtr.write_u64::<BigEndian>(0).unwrap();
                wtr.write_u64::<BigEndian>(row_id.clone()).unwrap();
            },
            RowKey::Sequence(table_id) => {
                wtr.write_u8(2).unwrap();
                wtr.write_u32::<BigEndian>(*table_id as u32).unwrap();
            },
            RowKey::SecondaryIndex(index_id, secondary_key, row_id) => {
                wtr.write_u8(3).unwrap();
                wtr.write_u32::<BigEndian>(*index_id as IndexId).unwrap();
                for &n in secondary_key {
                    wtr.write_u8(n).unwrap();
                }
                wtr.write_u64::<BigEndian>(row_id.clone()).unwrap();
            }
        }
        wtr
    }
}

macro_rules! enum_from_primitive_impl {
    ($name:ident, $( $variant:ident )*) => {
        impl From<u32> for $name {
            fn from(n: u32) -> Self {
                $( if n == $name::$variant as u32 {
                    $name::$variant
                } else )* {
                    panic!("Unknown {}: {}", stringify!($name), n);
                }
            }
        }
    };
}

#[macro_use(enum_from_primitive_impl)]
macro_rules! enum_from_primitive {
    (
        $( #[$enum_attr:meta] )*
        pub enum $name:ident {
            $( $( $( #[$variant_attr:meta] )* $variant:ident ),+ = $discriminator:expr ),*
        }
    ) => {
        $( #[$enum_attr] )*
        pub enum $name {
            $( $( $( #[$variant_attr] )* $variant ),+ = $discriminator ),*
        }
        enum_from_primitive_impl! { $name, $( $( $variant )+ )* }
    };
}

enum_from_primitive! {
    #[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize, Hash)]
    pub enum TableId {
        Schemas = 0x0100,
        Tables = 0x0200,
        Indexes = 0x0300,
        Partitions = 0x0400,
        Chunks = 0x0500,
        WALs = 0x0600,
        Jobs = 0x0700
    }
}

#[derive(Clone)]
pub struct RocksMetaStore {
    pub db: Arc<RwLock<Arc<DB>>>,
    listeners: Arc<RwLock<Vec<Sender<MetaStoreEvent>>>>,
    remote_fs: Arc<dyn RemoteFs>,
    last_checkpoint_time: Arc<RwLock<SystemTime>>,
    write_notify: Arc<Notify>,
    write_completed_notify: Arc<Notify>,
    last_upload_seq: Arc<RwLock<u64>>,
    last_check_seq: Arc<RwLock<u64>>,
    upload_loop_enabled: Arc<RwLock<bool>>
}

trait BaseRocksSecondaryIndex<T>: Debug {
    fn index_key_by(&self, row: &T) -> Vec<u8>;

    fn get_id(&self) -> u32;

    fn key_hash(&self, row: &T) -> u64 {
        let key_bytes = self.index_key_by(row);
        self.hash_bytes(&key_bytes)
    }

    fn hash_bytes(&self, key_bytes: &Vec<u8>) -> u64 {
        let mut hasher = DefaultHasher::new();
        key_bytes.hash(&mut hasher);
        hasher.finish()
    }

    fn is_unique(&self) -> bool;
}

trait RocksSecondaryIndex<T, K: Hash> : BaseRocksSecondaryIndex<T> {
    fn typed_key_by(&self, row: &T) -> K;

    fn key_to_bytes(&self, key: &K) -> Vec<u8>;

    fn typed_key_hash(&self, row_key: &K) -> u64 {
        let key_bytes = self.key_to_bytes(row_key);
        self.hash_bytes(&key_bytes)
    }

    fn index_key_by(&self, row: &T) -> Vec<u8> {
        self.key_to_bytes(&self.typed_key_by(row))
    }

    fn get_id(&self) -> u32;

    fn is_unique(&self) -> bool;
}

impl<T, I> BaseRocksSecondaryIndex<T> for I where I: RocksSecondaryIndex<T, String> {
    fn index_key_by(&self, row: &T) -> Vec<u8> {
        self.key_to_bytes(&self.typed_key_by(row))
    }

    fn get_id(&self) -> u32 {
        RocksSecondaryIndex::get_id(self)
    }

    fn is_unique(&self) -> bool {
        RocksSecondaryIndex::is_unique(self)
    }
}

struct TableScanIter<'a, RT: RocksTable + ?Sized> {
    table_id: TableId,
    table: &'a RT,
    iter: DBIterator<'a>
}

impl<'a, RT: RocksTable<T=T> + ?Sized, T> Iterator for TableScanIter<'a, RT>
    where T: Serialize + Clone + Debug + Send
{
    type Item = Result<IdRow<T>, CubeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let option = self.iter.next();
        if let Some((key, value)) = option {
            if let RowKey::Table(table_id, row_id) = RowKey::from_bytes(&key) {
                if table_id != self.table_id {
                    return None;
                }
                Some(self.table.deserialize_id_row(row_id, &value))
            } else {
                None
            }
        } else {
            None
        }
    }
}

trait RocksTable: Debug + Send + Sync + Clone {
    type T: Serialize + Clone + Debug + Send;
    fn delete_event(&self, row: IdRow<Self::T>) -> MetaStoreEvent;
    fn db(&self) -> Arc<DB>;
    fn index_id(&self, index_num: IndexId) -> IndexId;
    fn table_id(&self) -> TableId;
    fn deserialize_row<'de, D>(&self, deserializer: D) -> Result<Self::T, D::Error>
        where
            D: Deserializer<'de>;
    fn indexes() -> Vec<Box<dyn BaseRocksSecondaryIndex<Self::T>>>;

    fn insert(&self, row: Self::T, batch_pipe: &mut BatchPipe) -> Result<IdRow<Self::T>, CubeError> {
        let mut ser = flexbuffers::FlexbufferSerializer::new();
        row.serialize(&mut ser).unwrap();
        let serialized_row = ser.take_buffer();

        for index in Self::indexes().iter() {
            let hash = index.key_hash(&row);
            let index_val = index.index_key_by(&row);
            let existing_keys = self.get_row_from_index(index.get_id(), &index_val, &hash.to_be_bytes().to_vec())?;
            if index.is_unique() && existing_keys.len() > 0 {
                return Err(CubeError::user(
                    format!(
                        "Unique constraint violation: row {:?} has a key that already exists in {:?} index",
                        &row,
                        index
                    )
                ))
            }
        }

        let (row_id, inserted_row) = self.insert_row(serialized_row)?;
        batch_pipe.add_event(MetaStoreEvent::Insert(self.table_id(), row_id));
        batch_pipe.batch().put(inserted_row.key, inserted_row.val);

        let index_row = self.insert_index_row(&row, row_id)?;
        for row in index_row {
            batch_pipe.batch().put(row.key, row.val);
        }

        Ok(IdRow::new(row_id, row))
    }

    fn get_row_ids_by_index<K: Debug>(&self, row_key: &K, secondary_index: &impl RocksSecondaryIndex<Self::T, K>) -> Result<Vec<u64>, CubeError>
        where K: Hash
    {
        let hash = secondary_index.typed_key_hash(&row_key);
        let index_val = secondary_index.key_to_bytes(&row_key);
        let existing_keys = self.get_row_from_index(RocksSecondaryIndex::get_id(secondary_index), &index_val, &hash.to_be_bytes().to_vec())?;

        Ok(existing_keys)
    }

    fn get_rows_by_index<K: Debug>(&self, row_key: &K, secondary_index: &impl RocksSecondaryIndex<Self::T, K>) -> Result<Vec<IdRow<Self::T>>, CubeError>
        where K: Hash
    {
        let row_ids = self.get_row_ids_by_index(row_key, secondary_index)?;

        let mut res = Vec::new();

        for id in row_ids {
            res.push(self.get_row(id)?.ok_or(CubeError::internal(format!("Row exists in secondary index however missing in {:?} table: {}", self, id)))?)
        }

        if RocksSecondaryIndex::is_unique(secondary_index) && res.len() > 1 {
            return Err(CubeError::internal(format!("Unique index expected but found multiple values in {:?} table: {:?}", self, res)));
        }

        Ok(res)
    }

    fn get_single_row_by_index<K: Debug>(&self, row_key: &K, secondary_index: &impl RocksSecondaryIndex<Self::T, K>) -> Result<IdRow<Self::T>, CubeError>
        where K: Hash
    {
        let rows = self.get_rows_by_index(row_key, secondary_index)?;
        Ok(rows.into_iter().nth(0).ok_or(
            CubeError::internal(format!("One value expected in {:?} for {:?} but nothing found", self, row_key))
        )?)
    }

    fn update_with_fn(&self, row_id: u64, update_fn: impl FnOnce(&Self::T) -> Self::T, batch_pipe: &mut BatchPipe) -> Result<IdRow<Self::T>, CubeError> {
        let row = self.get_row_or_not_found(row_id)?;
        let new_row = update_fn(&row.get_row());
        self.update(row_id, new_row, &row.get_row(), batch_pipe)
    }

    fn update(&self, row_id: u64, new_row: Self::T, old_row: &Self::T, batch_pipe: &mut BatchPipe) -> Result<IdRow<Self::T>, CubeError> {
        let deleted_row = self.delete_index_row(&old_row, row_id)?;
        for row in deleted_row {
            batch_pipe.batch().delete(row.key);
        }

        let mut ser = flexbuffers::FlexbufferSerializer::new();
        new_row.serialize(&mut ser).unwrap();
        let serialized_row = ser.take_buffer();

        let updated_row = self.update_row(row_id, serialized_row)?;
        batch_pipe.add_event(MetaStoreEvent::Update(self.table_id(), row_id));
        batch_pipe.batch().put(updated_row.key, updated_row.val);

        let index_row = self.insert_index_row(&new_row, row_id)?;
        for row in index_row {
            batch_pipe.batch().put(row.key, row.val);
        }
        Ok(IdRow::new(row_id, new_row))
    }

    fn delete(&self, row_id: u64, batch_pipe: &mut BatchPipe) -> Result<IdRow<Self::T>, CubeError> {
        let row = self.get_row_or_not_found(row_id)?;
        let deleted_row = self.delete_index_row(row.get_row(), row_id)?;
        batch_pipe.add_event(MetaStoreEvent::Delete(self.table_id(), row_id));
        batch_pipe.add_event(self.delete_event(row.clone()));
        for row in deleted_row {
            batch_pipe.batch().delete(row.key);
        }

        batch_pipe.batch().delete(self.delete_row(row_id)?.key);

        Ok(row)
    }

    fn next_table_seq(&self) -> Result<u64, CubeError> {
        let ref db = self.db();
        let seq_key = RowKey::Sequence(self.table_id());
        let result = db.get(seq_key.to_bytes())?; // TODO merge
        let current_seq = match result {
            Some(v) => {
                let mut c = Cursor::new(v);
                c.read_u64::<BigEndian>().unwrap()
            },
            None => 0
        };
        let next_seq = current_seq + 1;
        let mut next_val = vec![];
        next_val.write_u64::<BigEndian>(next_seq)?;
        db.put(seq_key.to_bytes(), next_val)?;
        Ok(next_seq)
    }

    fn insert_row(&self, row: Vec<u8>) -> Result<(u64, KeyVal), CubeError> {
        let next_seq = self.next_table_seq()?;
        let t = RowKey::Table(self.table_id(), next_seq);
        let res = KeyVal {key: t.to_bytes(),
                                  val: row};
        Ok((next_seq, res))
    }

    fn update_row(&self, row_id: u64, row: Vec<u8>) -> Result<KeyVal, CubeError> {
        let t = RowKey::Table(self.table_id(), row_id);
        let res = KeyVal {key: t.to_bytes(),
                                  val: row};
        Ok(res)
    }

    fn delete_row(&self, row_id: u64) -> Result<KeyVal, CubeError> {
        let t = RowKey::Table(self.table_id(), row_id);
        let res = KeyVal {key: t.to_bytes(),
                                  val: vec![]};
        Ok(res)
    }

    fn get_row_or_not_found(&self, row_id: u64) -> Result<IdRow<Self::T>, CubeError> {
        self.get_row(row_id)?
            .ok_or(CubeError::user(format!("Row with id {} is not found for {:?}", row_id, self)))
    }

    fn get_row(&self, row_id: u64) -> Result<Option<IdRow<Self::T>>, CubeError> {
        let ref db = self.db();
        let res = db.get(RowKey::Table(self.table_id(), row_id).to_bytes())?;

        if let Some(buffer) = res {
            let row = self.deserialize_id_row(row_id, buffer.as_slice())?;
            return Ok(Some(row));
        }

        Ok(None)
    }

    fn deserialize_id_row(&self, row_id: u64, buffer: &[u8]) -> Result<IdRow<Self::T>, CubeError> {
        let r = flexbuffers::Reader::get_root(&buffer).unwrap();
        let row = self.deserialize_row(r)?;
        return Ok(IdRow::new(row_id, row))
    }

    fn insert_index_row(&self, row: &Self::T, row_id: u64) -> Result<Vec<KeyVal>, CubeError> {
        let mut res = Vec::new();
        for index in Self::indexes().iter() {
            let hash = index.key_hash(&row);
            let index_val = index.index_key_by(&row);
            let key = RowKey::SecondaryIndex(self.index_id( index.get_id()), hash.to_be_bytes().to_vec(), row_id);
            res.push( KeyVal {key: key.to_bytes(),
                              val: index_val});
        }
        Ok(res)
    }

    fn delete_index_row(&self, row: &Self::T, row_id: u64) -> Result<Vec<KeyVal>, CubeError> {
        let mut res = Vec::new();
        for index in Self::indexes().iter() {
            let hash = index.key_hash(&row);
            let key = RowKey::SecondaryIndex(self.index_id(index.get_id()), hash.to_be_bytes().to_vec(), row_id);
            res.push( KeyVal {key: key.to_bytes(),
                              val: vec![]});
        }

        Ok(res)
    }

    fn get_row_from_index(&self, secondary_id: u32, secondary_key_val: &Vec<u8>, secondary_key_hash: &Vec<u8>) -> Result<Vec<u64>, CubeError> {
        let ref db = self.db();
        let key_len = secondary_key_hash.len();
        let key_min = RowKey::SecondaryIndex(self.index_id(secondary_id), secondary_key_hash.clone(), 0);

        let mut res: Vec<u64> = Vec::new();
        let iter = db.prefix_iterator(&key_min.to_bytes()[0..(key_len+5)]);

        for (key, value) in iter {
            if let RowKey::SecondaryIndex(_, secondary_index_hash, row_id) = RowKey::from_bytes(&key) {

                if !secondary_index_hash.iter().zip(secondary_key_hash).all(|(a,b)| a == b) {
                    break;
                }

                if secondary_key_val.len() != value.len()
                || !value.iter().zip(secondary_key_val).all(|(a,b)| a == b) {
                    continue;
                }
                res.push(row_id);
            };
        };
        Ok(res)
    }

    fn all_rows(&self) -> Result<Vec<IdRow<Self::T>>, CubeError> {
        let mut res = Vec::new();
        let db = self.db();
        for row in self.table_scan(&db)? {
            res.push(row?);
        }
        Ok(res)
    }

    fn table_scan<'a>(&'a self, db: &'a DB) -> Result<TableScanIter<'a, Self>, CubeError> {
        let my_table_id = self.table_id();
        let key_min = RowKey::Table(my_table_id, 0);

        let iterator = db.prefix_iterator::<'a, 'a>(&key_min.to_bytes()[0..get_fixed_prefix()]);

        Ok(TableScanIter {
            table_id: my_table_id,
            iter: iterator,
            table: self
        })
    }

    fn build_path_rows<C: Clone, P>(
        &self,
        children: Vec<IdRow<C>>,
        mut parent_id_fn: impl FnMut(&IdRow<C>) -> u64,
        mut path_fn: impl FnMut(IdRow<C>, Arc<IdRow<Self::T>>) -> P
    ) -> Result<Vec<P>, CubeError> {
        let id_to_child = children.into_iter().map(|c| (parent_id_fn(&c), c)).collect::<Vec<_>>();
        let ids = id_to_child.iter().map(|(id, _)| *id).unique().collect::<Vec<_>>();
        let rows = ids.into_iter().map(|id| -> Result<(u64, Arc<IdRow<Self::T>>), CubeError> {
            Ok((id, Arc::new(self.get_row_or_not_found(id)?)))
        }).collect::<Result<HashMap<_, _>, _>>()?;
        Ok(id_to_child.into_iter().map(|(id, c)| path_fn(c, rows.get(&id).unwrap().clone()) ).collect::<Vec<_>>())
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum WriteBatchEntry {
    Put{ key: Box<[u8]>, value: Box<[u8]> },
    Delete { key: Box<[u8]> }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
struct WriteBatchContainer {
    entries: Vec<WriteBatchEntry>
}

impl WriteBatchContainer {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    fn write_batch(&self) -> WriteBatch {
        let mut batch = WriteBatch::default();
        for entry in self.entries.iter() {
            match entry {
                WriteBatchEntry::Put { key, value } => batch.put(key, value),
                WriteBatchEntry::Delete { key } => batch.delete(key)
            }
        }
        batch
    }

    async fn write_to_file(&self, file_name: &str) -> Result<(), CubeError> {
        let mut ser = flexbuffers::FlexbufferSerializer::new();
        self.serialize(&mut ser)?;
        let mut file = File::create(file_name).await?;
        Ok(tokio::io::AsyncWriteExt::write_all(&mut file, ser.view()).await?)
    }

    async fn read_from_file(file_name: &str) -> Result<Self, CubeError> {
        let mut file = File::open(file_name).await?;

        let mut buffer = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut buffer).await?;
        let r = flexbuffers::Reader::get_root(&buffer).unwrap();
        Ok(Self::deserialize(r)?)
    }
}

impl WriteBatchIterator for WriteBatchContainer {
    fn put(&mut self, key: Box<[u8]>, value: Box<[u8]>) {
        self.entries.push(WriteBatchEntry::Put { key, value });
    }

    fn delete(&mut self, key: Box<[u8]>) {
        self.entries.push(WriteBatchEntry::Delete { key });
    }
}

impl RocksMetaStore {
    pub fn with_listener(path: impl AsRef<Path>, listeners: Vec<Sender<MetaStoreEvent>>, remote_fs: Arc<dyn RemoteFs>) -> Arc<RocksMetaStore> {
        let meta_store = RocksMetaStore::with_listener_impl(path, listeners, remote_fs);
        Arc::new(meta_store)
    }

    pub fn with_listener_impl(path: impl AsRef<Path>, listeners: Vec<Sender<MetaStoreEvent>>, remote_fs: Arc<dyn RemoteFs>) -> RocksMetaStore {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(13));

        let db = DB::open(&opts, path).unwrap();
        let db_arc = Arc::new(db);

        let meta_store = RocksMetaStore {
            db: Arc::new(RwLock::new(db_arc.clone())),
            listeners: Arc::new(RwLock::new(listeners)),
            remote_fs,
            last_checkpoint_time: Arc::new(RwLock::new(SystemTime::now())),
            write_notify: Arc::new(Notify::new()),
            write_completed_notify: Arc::new(Notify::new()),
            last_upload_seq: Arc::new(RwLock::new(db_arc.latest_sequence_number())),
            last_check_seq: Arc::new(RwLock::new(db_arc.latest_sequence_number())),
            upload_loop_enabled: Arc::new(RwLock::new(true))
        };
        meta_store
    }

    pub fn new(path: impl AsRef<Path>, remote_fs: Arc<dyn RemoteFs>) -> Arc<RocksMetaStore> {
        Self::with_listener(path, vec![], remote_fs)
    }

    pub async fn load_from_remote(path: impl AsRef<Path>, remote_fs: Arc<dyn RemoteFs>) -> Result<Arc<RocksMetaStore>, CubeError> {
        if !fs::metadata(path.as_ref()).await.is_ok() {
            let re = Regex::new(r"^metastore-(\d+)").unwrap();

            if remote_fs.list("metastore-current").await?.iter().len() > 0 {
                info!("Downloading remote metastore");
                let current_metastore_file = remote_fs.local_file("metastore-current").await?;
                if fs::metadata(current_metastore_file.as_str()).await.is_ok() {
                    fs::remove_file(current_metastore_file.as_str()).await?;
                }
                remote_fs.download_file("metastore-current").await?;

                let mut file = File::open(current_metastore_file.as_str()).await?;
                let mut buffer = Vec::new();
                tokio::io::AsyncReadExt::read_to_end(&mut file, &mut buffer).await?;
                let last_metastore_snapshot = {
                    let parse_result = re.captures(&String::from_utf8(buffer)?)
                        .map(|c| c.get(1).unwrap().as_str())
                        .map(|p| u128::from_str(p));
                    if let Some(Ok(millis)) = parse_result {
                        Some(millis)
                    } else {
                        None
                    }
                };

                if let Some(snapshot) = last_metastore_snapshot {
                    let to_load = remote_fs.list(&format!("metastore-{}", snapshot)).await?;
                    let meta_store_path = remote_fs.local_file("metastore").await?;
                    fs::create_dir_all(meta_store_path.to_string()).await?;
                    for file in to_load.iter() {
                        remote_fs.download_file(file).await?;
                        let local = remote_fs.local_file(file).await?;
                        let path = Path::new(&local);
                        fs::copy(path, PathBuf::from(&meta_store_path).join(path.file_name().unwrap().to_str().unwrap())).await?;
                    }

                    let meta_store = Self::new(path.as_ref(), remote_fs.clone());

                    let logs_to_batch = remote_fs.list(&format!("metastore-{}-logs", snapshot)).await?;
                    for log_file in logs_to_batch.iter() {
                        let path_to_log = remote_fs.local_file(log_file).await?;
                        let batch = WriteBatchContainer::read_from_file(&path_to_log).await?;
                        let db = meta_store.db.write().await;
                        db.write(batch.write_batch())?;
                    }

                    return Ok(meta_store);
                }
            }
            info!("Creating metastore from scratch in {}", path.as_ref().as_os_str().to_string_lossy());
        } else {
            info!("Using existing metastore in {}", path.as_ref().as_os_str().to_string_lossy());
        }

        Ok(Self::new(path, remote_fs))
    }

    pub async fn add_listener(&self, listener: Sender<MetaStoreEvent>) {
        self.listeners.write().await.push(listener);
    }

    async fn write_operation<F, R>(&self, f: F) -> Result<R, CubeError>
        where
            F: FnOnce(Arc<DB>, &mut BatchPipe) -> Result<R, CubeError> + Send + 'static,
            R: Send + 'static,
    {
        let db = self.db.write().await.clone();
        let (spawn_res, events) = tokio::task::spawn_blocking(move || -> Result<(R, Vec<MetaStoreEvent>), CubeError> {
            let mut batch = BatchPipe::new(db.as_ref());
            let res = f(db.clone(), &mut batch)?;
            let write_result = batch.batch_write_rows()?;
            Ok((res, write_result))
        }).await??;

        self.write_notify.notify();

        for listener in self.listeners.read().await.clone().iter_mut() {
            for event in events.iter() {
                listener.send(event.clone())?;
            }
        }

        Ok(spawn_res)
    }

    pub async fn run_upload_loop(&self) {
        loop {
            if !*self.upload_loop_enabled.read().await {
                return;
            }
            if let Err(e) = self.run_upload().await {
                error!("Error in metastore upload loop: {}", e);
            }
        }
    }

    pub async fn stop_processing_loops(&self) {
        let mut upload_loop_enabled = self.upload_loop_enabled.write().await;
        *upload_loop_enabled = false;
    }

    pub async fn run_upload(&self) -> Result<(), CubeError> {
        let last_check_seq = self.last_check_seq().await;
        let last_db_seq = self.db.read().await.latest_sequence_number();
        if last_check_seq == last_db_seq {
            let _ = tokio::time::timeout(Duration::from_secs(5), self.write_notify.notified()).await; // TODO
        }
        let last_upload_seq = self.last_upload_seq().await;
        let (serializer, min, max) = {
            let updates = self.db.write().await.get_updates_since(last_upload_seq)?;
            let mut serializer = WriteBatchContainer::new();

            let mut seq_numbers = Vec::new();

            updates.into_iter().for_each(|(n, write_batch)| {
                seq_numbers.push(n);
                write_batch.iterate(&mut serializer);
            });
            (serializer, seq_numbers.iter().min().map(|v| *v), seq_numbers.iter().max().map(|v| *v))
        };

        if max.is_some() {
            let checkpoint_time = self.last_checkpoint_time.read().await;
            let log_name = format!("{}-logs/{}.flex", RocksMetaStore::meta_store_path(&checkpoint_time), min.unwrap());
            let file_name = self.remote_fs.local_file(&log_name).await?;
            serializer.write_to_file(&file_name).await?;
            self.remote_fs.upload_file(&log_name).await?;
            let mut seq = self.last_upload_seq.write().await;
            *seq = max.unwrap();
            self.write_completed_notify.notify();
        }

        let last_checkpoint_time: SystemTime = self.last_checkpoint_time.read().await.clone();
        if last_checkpoint_time + time::Duration::from_secs(60) < SystemTime::now() {
            self.upload_check_point().await?;
        }

        let mut check_seq = self.last_check_seq.write().await;
        *check_seq = last_db_seq;

        Ok(())
    }

    async fn upload_check_point(&self) -> Result<(), CubeError> {
        let mut check_point_time = self.last_checkpoint_time.write().await;
        let remote_fs = self.remote_fs.clone();
        let db = self.db.write().await.clone();
        *check_point_time = SystemTime::now();
        RocksMetaStore::upload_checkpoint(db, remote_fs, &check_point_time).await?;
        self.write_completed_notify.notify();
        Ok(())
    }

    async fn last_upload_seq(&self) -> u64 {
        *self.last_upload_seq.read().await
    }

    async fn last_check_seq(&self) -> u64 {
        *self.last_check_seq.read().await
    }

    async fn upload_checkpoint(db: Arc<DB>, remote_fs: Arc<dyn RemoteFs>, checkpoint_time: &SystemTime) -> Result<(), CubeError> {
        let remote_path = RocksMetaStore::meta_store_path(checkpoint_time);
        let checkpoint_path = db.path().join("..").join(remote_path.clone());
        let path_to_move = checkpoint_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), CubeError> {
            let checkpoint = Checkpoint::new(db.as_ref())?;
            checkpoint.create_checkpoint(path_to_move.as_path())?;
            Ok(())
        }).await??;

        let mut dir = fs::read_dir(checkpoint_path).await?;

        let mut files_to_upload = Vec::new();
        while let Some(file) = dir.next_entry().await? {
            let file = file.file_name();
            files_to_upload.push(format!("{}/{}", remote_path, file.to_string_lossy()));
        }
        for v in join_all(files_to_upload.iter().map(|f| remote_fs.upload_file(&f)).collect::<Vec<_>>()).await.into_iter() {
            v?;
        }

        let existing_metastore_files = remote_fs.list("metastore-").await?;
        let to_delete = existing_metastore_files.into_iter().filter_map(|existing| {
            let path = existing.split("/").nth(0).map(|p| u128::from_str(&p.replace("metastore-", "").replace("-logs", "")));
            if let Some(Ok(millis)) = path {
                if SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() - millis > 3 * 60 * 1000 {
                    return Some(existing);
                }
            }
            None
        }).collect::<Vec<_>>();
        for v in join_all(to_delete.iter().map(|f| remote_fs.delete_file(&f)).collect::<Vec<_>>()).await.into_iter() {
            v?;
        }

        let current_metastore_file = remote_fs.local_file("metastore-current").await?;

        {
            let mut file = File::create(current_metastore_file).await?;
            tokio::io::AsyncWriteExt::write_all(&mut file, remote_path.as_bytes()).await?;
        }

        remote_fs.upload_file("metastore-current").await?;

        Ok(())
    }

    fn meta_store_path(checkpoint_time: &SystemTime) -> String {
        format!("metastore-{}", checkpoint_time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis())
    }

    async fn read_operation<F, R>(&self, f: F) -> R
        where
            F: FnOnce(Arc<DB>) -> R + Send + 'static,
            R: Send + 'static,
    {
        let db = self.db.read().await.clone();
        tokio::task::spawn_blocking(move || {
            f(db)
        }).await.unwrap()
    }

    fn check_if_exists(name: &String, existing_keys_len: usize) -> Result<(), CubeError> {
        if existing_keys_len > 1 {
            let e = CubeError::user(format!("Schema with name '{}' has more than one id. Something went wrong.", name));
            return Err(e);
        } else if existing_keys_len == 0 {
            let e = CubeError::user(format!("Schema with name '{}' does not exist.", name));
            return Err(e);
        }
        Ok(())
    }

    pub fn prepare_test_metastore(test_name: &str) -> (Arc<LocalDirRemoteFs>, Arc<RocksMetaStore>) {
        let store_path = env::current_dir().unwrap().join(format!("test-{}-local", test_name));
        let remote_store_path = env::current_dir().unwrap().join(format!("test-{}-remote", test_name));
        let _ = std::fs::remove_dir_all(store_path.clone());
        let _ = std::fs::remove_dir_all(remote_store_path.clone());
        let remote_fs = LocalDirRemoteFs::new(store_path.clone(), remote_store_path.clone());
        let meta_store = RocksMetaStore::new(store_path.clone().join("metastore").as_path(), remote_fs.clone());
        (remote_fs, meta_store)
    }

    pub fn cleanup_test_metastore(test_name: &str) {
        let store_path = env::current_dir().unwrap().join(format!("test-{}-local", test_name));
        let remote_store_path = env::current_dir().unwrap().join(format!("test-{}-remote", test_name));
        let _ = std::fs::remove_dir_all(store_path.clone());
        let _ = std::fs::remove_dir_all(remote_store_path.clone());
    }

    async fn has_pending_changes(&self) -> Result<bool, CubeError> {
        let db = self.db.read().await;
        Ok(db.get_updates_since(self.last_upload_seq().await)?.next().is_some())
    }
}

#[async_trait]
impl MetaStore for RocksMetaStore {
    async fn wait_for_current_seq_to_sync(&self) -> Result<(), CubeError> {
        while self.has_pending_changes().await? {
            tokio::time::timeout(Duration::from_secs(30), self.write_completed_notify.notified()).await?;
        }
        Ok(())
    }

    fn schemas_table(&self) -> Box<dyn MetaStoreTable<T=Schema>> {
        Box::new(MetaStoreTableImpl {
            rocks_meta_store: self.clone(),
            rocks_table_fn: |db| SchemaRocksTable::new(db)
        })
    }

    async fn create_schema(&self, schema_name: String, if_not_exists: bool) -> Result<IdRow<Schema>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = SchemaRocksTable::new(db_ref.clone());
            if if_not_exists {
                let rows = table.get_rows_by_index(&schema_name, &SchemaRocksIndex::Name)?;
                if let Some(row) = rows.into_iter().nth(0) {
                    return Ok(row);
                }
            }
            let schema = Schema { name: schema_name.clone() };
            Ok(table.insert(schema, batch_pipe)?)
        }).await
    }

    async fn get_schemas(&self) -> Result<Vec<IdRow<Schema>>, CubeError> {
        self.read_operation(move |db_ref| {
            SchemaRocksTable::new(db_ref).all_rows()
        }).await
    }

    async fn get_schema_by_id(&self, schema_id: u64) -> Result<IdRow<Schema>, CubeError> {
        self.read_operation(move |db_ref| {
            let table = SchemaRocksTable::new(db_ref);
            table.get_row_or_not_found(schema_id)
        }).await
    }

    async fn get_schema_id(&self, schema_name: String) -> Result<u64, CubeError> {
        self.read_operation(move |db_ref| {
            let table = SchemaRocksTable::new(db_ref);
            let existing_keys = table.get_row_ids_by_index(&schema_name, &SchemaRocksIndex::Name)?;
            RocksMetaStore::check_if_exists(&schema_name, existing_keys.len())?;
            Ok(existing_keys[0])
        }).await
    }

    async fn get_schema(&self, schema_name: String) -> Result<IdRow<Schema>, CubeError> {
        self.read_operation(move |db_ref| {
            let table = SchemaRocksTable::new(db_ref);
            let existing_keys = table.get_row_ids_by_index(&schema_name, &SchemaRocksIndex::Name)?;
            RocksMetaStore::check_if_exists(&schema_name, existing_keys.len())?;

            let schema_id = existing_keys[0];
            if let Some(schema) = table.get_row(schema_id)?{
                return Ok(schema);
            }

            let e = CubeError::user(format!("Schema with name '{}' does not exist.", schema_name));
            Err(e)
        }).await
    }

    async fn rename_schema(&self, old_schema_name: String, new_schema_name: String) -> Result<IdRow<Schema>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = SchemaRocksTable::new(db_ref.clone());
            let existing_keys = table.get_row_ids_by_index(&old_schema_name, &SchemaRocksIndex::Name)?;
            RocksMetaStore::check_if_exists(&old_schema_name, existing_keys.len())?;

            let schema_id = existing_keys[0];

            let old_schema = table.get_row(schema_id)?.unwrap();
            let mut new_schema = old_schema.clone();
            new_schema.row.set_name(&new_schema_name);
            let id_row = table.update(schema_id, new_schema.row, &old_schema.row, batch_pipe)?;
            Ok(id_row)
        }).await
    }

    async fn rename_schema_by_id(&self, schema_id: u64, new_schema_name: String) -> Result<IdRow<Schema>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = SchemaRocksTable::new(db_ref.clone());

            let old_schema = table.get_row(schema_id)?.unwrap();
            let mut new_schema = old_schema.clone();
            new_schema.row.set_name(&new_schema_name);
            let id_row = table.update(schema_id, new_schema.row, &old_schema.row, batch_pipe)?;

            Ok(id_row)
        }).await
    }

    async fn delete_schema(&self, schema_name: String) -> Result<(), CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = SchemaRocksTable::new(db_ref.clone());
            let existing_keys = table.get_row_ids_by_index(&schema_name, &SchemaRocksIndex::Name)?;
            RocksMetaStore::check_if_exists(&schema_name, existing_keys.len())?;
            let schema_id = existing_keys[0];

            table.delete(schema_id, batch_pipe)?;

            Ok(())
        }).await
    }

    async fn delete_schema_by_id(&self, schema_id: u64) -> Result<(), CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = SchemaRocksTable::new(db_ref.clone());
            table.delete(schema_id, batch_pipe)?;

            Ok(())
        }).await
    }

    fn tables_table(&self) -> Box<dyn MetaStoreTable<T=Table>> {
        Box::new(MetaStoreTableImpl {
            rocks_meta_store: self.clone(),
            rocks_table_fn: |db| TableRocksTable::new(db)
        })
    }

    async fn create_table(&self, schema_name: String, table_name: String, columns: Vec<Column>, location: Option<String>, import_format: Option<ImportFormat>, indexes: Vec<IndexDef>) -> Result<IdRow<Table>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let rocks_table = TableRocksTable::new(db_ref.clone());
            let rocks_index = IndexRocksTable::new(db_ref.clone());
            let rocks_schema = SchemaRocksTable::new(db_ref.clone());
            let rocks_partition = PartitionRocksTable::new(db_ref.clone());

            let schema_id = rocks_schema.get_single_row_by_index(&schema_name, &SchemaRocksIndex::Name)?;
            let index_cols = columns.clone();
            let table = Table::new(table_name, schema_id.get_id(), columns, location, import_format);
            let table_id = rocks_table.insert(table, batch_pipe)?;
            let sort_key_size = index_cols.len() as u64;
            for index_def in indexes.into_iter() {
                let (mut sorted, mut unsorted) = index_cols.clone().into_iter().partition::<Vec<_>, _>(|c| index_def.columns.iter().find(|dc| c.name.as_str() == dc.as_str()).is_some());
                let sorted_key_size = sorted.len() as u64;
                sorted.append(&mut unsorted);
                let index = Index::new(index_def.name, table_id.get_id(), sorted.into_iter().enumerate().map(|(i,c)| c.replace_index(i)).collect::<Vec<_>>(), sorted_key_size);
                let index_id = rocks_index.insert(index, batch_pipe)?;
                let partition = Partition::new(index_id.id, None, None);
                let _ = rocks_partition.insert(partition, batch_pipe)?;
            }
            let index = Index::new("default".to_string(), table_id.get_id(), index_cols, sort_key_size);
            let index_id = rocks_index.insert(index, batch_pipe)?;
            let partition = Partition::new(index_id.id, None, None);
            let _ = rocks_partition.insert(partition, batch_pipe)?;

            Ok(table_id)
        }).await
    }

    async fn get_table(&self, schema_name: String, table_name: String) -> Result<IdRow<Table>, CubeError> {
        self.read_operation(move |db_ref| {
            let rocks_table = TableRocksTable::new(db_ref.clone());
            let rocks_schema = SchemaRocksTable::new(db_ref);
            let schema_id = rocks_schema.get_single_row_by_index(&schema_name, &SchemaRocksIndex::Name)?;
            let index_key = TableIndexKey::ByName(schema_id.get_id(), table_name.to_string());
            let table = rocks_table.get_single_row_by_index(&index_key, &TableRocksIndex::Name)?;
            Ok(table)
        }).await
    }

    async fn get_table_by_id(&self, table_id: u64) -> Result<IdRow<Table>, CubeError> {
        self.read_operation(move |db_ref| {
            TableRocksTable::new(db_ref.clone()).get_row_or_not_found(table_id)
        }).await
    }

    async fn get_tables(&self) -> Result<Vec<IdRow<Table>>, CubeError> {
        self.read_operation(|db_ref| {
            TableRocksTable::new(db_ref).all_rows()
        }).await
    }

    async fn get_tables_with_path(&self) -> Result<Vec<TablePath>, CubeError> {
        self.read_operation(|db_ref| {
            let tables = TableRocksTable::new(db_ref.clone()).all_rows()?;
            let schemas = SchemaRocksTable::new(db_ref);
            Ok(schemas.build_path_rows(
                tables,
                |t| t.get_row().get_schema_id(),
                |table, schema| TablePath { table, schema }
            )?)
        }).await
    }

    async fn drop_table(&self, table_id: u64) -> Result<IdRow<Table>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let tables_table = TableRocksTable::new(db_ref.clone());
            let indexes_table = IndexRocksTable::new(db_ref.clone());
            let partitions_table = PartitionRocksTable::new(db_ref.clone());
            let chunks_table = ChunkRocksTable::new(db_ref);

            let indexes = indexes_table.get_rows_by_index(&IndexIndexKey::TableId(table_id), &IndexRocksIndex::TableID)?;
            for index in indexes.into_iter() {
                let partitions = partitions_table.get_rows_by_index(&PartitionIndexKey::ByIndexId(index.get_id()), &PartitionRocksIndex::IndexId)?;
                for partition in partitions.into_iter() {
                    let chunks = chunks_table.get_rows_by_index(&ChunkIndexKey::ByPartitionId(partition.get_id()), &ChunkRocksIndex::PartitionId)?;
                    for chunk in chunks.into_iter() {
                        chunks_table.delete(chunk.get_id(), batch_pipe)?;
                    }
                    partitions_table.delete(partition.get_id(), batch_pipe)?;
                }
                indexes_table.delete(index.get_id(), batch_pipe)?;
            }
            Ok(tables_table.delete(table_id, batch_pipe)?)
        }).await
    }

    fn partition_table(&self) -> Box<dyn MetaStoreTable<T=Partition>> {
        Box::new(MetaStoreTableImpl {
            rocks_meta_store: self.clone(),
            rocks_table_fn: |db| PartitionRocksTable::new(db)
        })
    }

    async fn create_partition(&self, partition: Partition) -> Result<IdRow<Partition>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = PartitionRocksTable::new(db_ref.clone());
            let row_id = table.insert(partition, batch_pipe)?;
            Ok(row_id)
        }).await
    }

    async fn get_partition(&self, partition_id: u64) -> Result<IdRow<Partition>, CubeError> {
        self.read_operation(move |db_ref| {
            PartitionRocksTable::new(db_ref).get_row_or_not_found(partition_id)
        }).await
    }

    async fn get_partition_for_compaction(&self, partition_id: u64) -> Result<(IdRow<Partition>, IdRow<Index>), CubeError> {
        self.read_operation(move |db_ref| {
            let partition = PartitionRocksTable::new(db_ref.clone()).get_row(partition_id)?
                .ok_or(CubeError::internal(format!("Partition is not found: {}", partition_id)))?;
            let index = IndexRocksTable::new(db_ref.clone()).get_row(partition.get_row().get_index_id())?
                .ok_or(CubeError::internal(format!("Index {} is not found for partition: {}", partition.get_row().get_index_id(), partition_id)))?;
            if !partition.get_row().is_active() {
                return Err(CubeError::internal(format!("Cannot compact inactive partition: {:?}", partition.get_row())))
            }
            Ok((partition, index))
        }).await
    }

    async fn get_partition_chunk_sizes(&self, partition_id: u64) -> Result<u64, CubeError> {
        let chunks = self.get_chunks_by_partition(partition_id).await?;
        Ok(chunks.iter().map(|r| r.get_row().row_count).sum())
    }

    async fn swap_active_partitions(
        &self,
        current_active: Vec<u64>,
        new_active: Vec<u64>,
        compacted_chunk_ids: Vec<u64>,
        new_active_min_max: Vec<(u64, (Option<Row>, Option<Row>))>
    ) -> Result<(), CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = PartitionRocksTable::new(db_ref.clone());
            let chunk_table = ChunkRocksTable::new(db_ref.clone());

            for current in current_active.iter() {
                let current_partition = table.get_row(*current)?
                    .ok_or(CubeError::internal(format!("Current partition is not found during swap active: {}", current)))?;
                if !current_partition.get_row().is_active() {
                    return Err(CubeError::internal(format!("Current partition is not active: {:?}", current_partition.get_row())));
                }
                table.update(current_partition.get_id(), current_partition.get_row().to_active(false), current_partition.get_row(), batch_pipe)?;
            }

            for (new, (count, (min_value, max_value))) in new_active.iter().zip(new_active_min_max.into_iter()) {
                let new_partition = table.get_row(*new)?
                    .ok_or(CubeError::internal(format!("New partition is not found during swap active: {}", new)))?;
                if new_partition.get_row().is_active() {
                    return Err(CubeError::internal(format!("New partition is already active: {:?}", new_partition.get_row())));
                }
                table.update(new_partition.get_id(), new_partition.get_row().to_active(true).update_min_max_and_row_count(min_value, max_value, count), new_partition.get_row(), batch_pipe)?;
            }

            for chunk_id in compacted_chunk_ids.iter() {
                chunk_table.update_with_fn(*chunk_id, |row| row.deactivate(), batch_pipe)?;
            }

            Ok(())
        }).await
    }

    fn index_table(&self) -> Box<dyn MetaStoreTable<T=Index>> {
        Box::new(MetaStoreTableImpl {
            rocks_meta_store: self.clone(),
            rocks_table_fn: |db| IndexRocksTable::new(db)
        })
    }

    async fn get_default_index(&self, table_id: u64) -> Result<IdRow<Index>, CubeError> {
        self.read_operation(move |db_ref| {
            let index = IndexRocksTable::new(db_ref);
            let indexes = index.get_rows_by_index(&IndexIndexKey::Name(table_id, "default".to_string()), &IndexRocksIndex::Name)?;
            indexes.into_iter().nth(0).ok_or(CubeError::internal(format!("Missing default index for table {}", table_id)))
        }).await
    }

    async fn get_table_indexes(&self, table_id: u64) -> Result<Vec<IdRow<Index>>, CubeError> {
        self.read_operation(move |db_ref| {
            let index_table = IndexRocksTable::new(db_ref);
            Ok(index_table.get_rows_by_index(&IndexIndexKey::TableId(table_id), &IndexRocksIndex::TableID)?)
        }).await
    }

    async fn get_active_partitions_by_index_id(&self, index_id: u64) -> Result<Vec<IdRow<Partition>>, CubeError> {
        self.read_operation(move |db_ref| {
            let rocks_partition = PartitionRocksTable::new(db_ref);
            // TODO iterate over range
            Ok(rocks_partition.get_rows_by_index(
                &PartitionIndexKey::ByIndexId(index_id),
                &PartitionRocksIndex::IndexId
            )?.into_iter().filter(|r| r.get_row().active).collect::<Vec<_>>())
        }).await
    }

    async fn create_chunk(&self, partition_id: u64, row_count: usize) -> Result<IdRow<Chunk>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let rocks_chunk = ChunkRocksTable::new(db_ref.clone());

            let chunk = Chunk::new(partition_id, row_count);
            let id_row = rocks_chunk.insert(chunk, batch_pipe)?;

            Ok(id_row)
        }).await
    }

    async fn get_chunk(&self, chunk_id: u64) -> Result<IdRow<Chunk>, CubeError> {
        self.read_operation(move |db_ref| {
            ChunkRocksTable::new(db_ref).get_row_or_not_found(chunk_id)
        }).await
    }

    async fn get_chunks_by_partition(&self, partition_id: u64) -> Result<Vec<IdRow<Chunk>>, CubeError> {
        self.read_operation(move |db_ref| {
            let table = ChunkRocksTable::new(db_ref);
            Ok(table.get_rows_by_index(
                &ChunkIndexKey::ByPartitionId(partition_id),
                &ChunkRocksIndex::PartitionId
            )?.into_iter().filter(|c| c.get_row().uploaded() && c.get_row().active()).collect::<Vec<_>>())
        }).await
    }

    async fn chunk_uploaded(&self, chunk_id: u64) -> Result<IdRow<Chunk>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = ChunkRocksTable::new(db_ref.clone());
            let row = table.get_row_or_not_found(chunk_id)?;
            let id_row = table.update(chunk_id, row.get_row().set_uploaded(true), row.get_row(), batch_pipe)?;

            Ok(id_row)
        }).await
    }

    async fn deactivate_chunk(&self, chunk_id: u64) -> Result<(), CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            ChunkRocksTable::new(db_ref.clone()).update_with_fn(chunk_id, |row| row.deactivate(), batch_pipe)?;
            Ok(())
        }).await
    }

    fn chunks_table(&self) -> Box<dyn MetaStoreTable<T=Chunk>> {
        Box::new(MetaStoreTableImpl {
            rocks_meta_store: self.clone(),
            rocks_table_fn: |db| ChunkRocksTable::new(db)
        })
    }

    async fn create_wal(&self, table_id: u64, row_count: usize) -> Result<IdRow<WAL>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let rocks_wal = WALRocksTable::new(db_ref.clone());

            let wal = WAL::new(table_id, row_count);
            let id_row = rocks_wal.insert(wal, batch_pipe)?;

            Ok(id_row)
        }).await
    }

    async fn get_wal(&self, wal_id: u64) -> Result<IdRow<WAL>, CubeError> {
        self.read_operation(move |db_ref| {
            WALRocksTable::new(db_ref).get_row_or_not_found(wal_id)
        }).await
    }

    async fn get_wals_for_table(&self, table_id: u64) -> Result<Vec<IdRow<WAL>>, CubeError> {
        self.read_operation(move |db_ref| {
            WALRocksTable::new(db_ref).get_rows_by_index(&WALIndexKey::ByTable(table_id), &WALRocksIndex::TableID)
        }).await
    }

    async fn delete_wal(&self, wal_id: u64) -> Result<(), CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            WALRocksTable::new(db_ref.clone()).delete(wal_id, batch_pipe)?;
            Ok(())
        }).await
    }

    async fn wal_uploaded(&self, wal_id: u64) -> Result<IdRow<WAL>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = WALRocksTable::new(db_ref.clone());
            let row = table.get_row_or_not_found(wal_id)?;
            let id_row = table.update(wal_id, row.get_row().set_uploaded(true), row.get_row(), batch_pipe)?;

            Ok(id_row)
        }).await
    }


    async fn add_job(&self, job: Job) -> Result<Option<IdRow<Job>>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = JobRocksTable::new(db_ref.clone());

            let result = table.get_row_ids_by_index(
                &JobIndexKey::RowReference(job.row_reference().clone(), job.job_type().clone()),
                &JobRocksIndex::RowReference
            )?;
            if result.len() > 0 {
                return Ok(None);
            }

            let id_row = table.insert(job, batch_pipe)?;

            Ok(Some(id_row))
        }).await
    }

    async fn get_job(&self, job_id: u64) -> Result<IdRow<Job>, CubeError> {
        self.read_operation(move |db_ref| {
            Ok(JobRocksTable::new(db_ref).get_row_or_not_found(job_id)?)
        }).await
    }

    async fn delete_job(&self, job_id: u64) -> Result<IdRow<Job>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            Ok(JobRocksTable::new(db_ref.clone()).delete(job_id, batch_pipe)?)
        }).await
    }

    async fn start_processing_job(&self, server_name: String) -> Result<Option<IdRow<Job>>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            let table = JobRocksTable::new(db_ref);
            let next_job = table
                .get_rows_by_index(&JobIndexKey::ScheduledByShard(Some(server_name.to_string())), &JobRocksIndex::ByShard)?
                .into_iter().nth(0);
            if let Some(job) = next_job {
                if let JobStatus::ProcessingBy(node) = job.get_row().status() {
                    return Err(CubeError::internal(
                        format!("Job {:?} is already processing by {}", job, node)
                    ));
                }
                Ok(
                    Some(table.update_with_fn(job.get_id(), |row| row.start_processing(server_name), batch_pipe)?)
                )
            } else {
                Ok(None)
            }
        }).await
    }

    async fn update_heart_beat(&self, job_id: u64) -> Result<IdRow<Job>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            Ok(
                JobRocksTable::new(db_ref)
                    .update_with_fn(job_id, |row| row.update_heart_beat(), batch_pipe)?
            )
        }).await
    }

    async fn update_status(&self, job_id: u64, status: JobStatus) -> Result<IdRow<Job>, CubeError> {
        self.write_operation(move |db_ref, batch_pipe| {
            Ok(
                JobRocksTable::new(db_ref)
                    .update_with_fn(job_id, |row| row.update_status(status), batch_pipe)?
            )
        }).await
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::remotefs::LocalDirRemoteFs;
    use std::{env, fs};
    use crate::config::Config;

    #[test]
    fn macro_test() {
        let s = Schema { name: "foo".to_string() };
        assert_eq!(format_table_value!(s, name, String), "foo");
    }

    #[actix_rt::test]
    async fn schema_test() {

        let store_path = env::current_dir().unwrap().join("test-local");
        let remote_store_path = env::current_dir().unwrap().join("test-remote");
        let _ = fs::remove_dir_all(store_path.clone());
        let _ = fs::remove_dir_all(remote_store_path.clone());
        let remote_fs = LocalDirRemoteFs::new(store_path.clone(), remote_store_path.clone());

        {
            let meta_store = RocksMetaStore::new(store_path.join("metastore").as_path(), remote_fs);

            let schema_1 = meta_store.create_schema("foo".to_string(), false).await.unwrap();
            println!("New id: {}", schema_1.id);
            let schema_2 = meta_store.create_schema("bar".to_string(), false).await.unwrap();
            println!("New id: {}", schema_2.id);
            let schema_3 = meta_store.create_schema("boo".to_string(), false).await.unwrap();
            println!("New id: {}", schema_3.id);

            let schema_1_id = schema_1.id;
            let schema_2_id = schema_2.id;
            let schema_3_id = schema_3.id;

            assert!(meta_store.create_schema("foo".to_string(), false).await.is_err());

            assert_eq!(meta_store.get_schema("foo".to_string()).await.unwrap(), schema_1);
            assert_eq!(meta_store.get_schema("bar".to_string()).await.unwrap(), schema_2);
            assert_eq!(meta_store.get_schema("boo".to_string()).await.unwrap(), schema_3);

            assert_eq!(meta_store.get_schema_by_id(schema_1_id).await.unwrap(), schema_1);
            assert_eq!(meta_store.get_schema_by_id(schema_2_id).await.unwrap(), schema_2);
            assert_eq!(meta_store.get_schema_by_id(schema_3_id).await.unwrap(), schema_3);

            assert_eq!(meta_store.get_schemas().await.unwrap(), vec![
               IdRow::new(1, Schema { name: "foo".to_string() }),
               IdRow::new(2, Schema { name: "bar".to_string() }),
               IdRow::new(3, Schema { name: "boo".to_string() }),
            ]);

            assert_eq!(meta_store.rename_schema("foo".to_string(), "foo1".to_string()).await.unwrap(), IdRow::new(schema_1_id, Schema{name: "foo1".to_string()}));
            assert!(meta_store.get_schema("foo".to_string()).await.is_err());
            assert_eq!(meta_store.get_schema("foo1".to_string()).await.unwrap(), IdRow::new(schema_1_id, Schema{name: "foo1".to_string()}));
            assert_eq!(meta_store.get_schema_by_id(schema_1_id).await.unwrap(), IdRow::new(schema_1_id, Schema{name: "foo1".to_string()}));

            assert!(meta_store.rename_schema("boo1".to_string(), "foo1".to_string()).await.is_err());

            assert_eq!(meta_store.rename_schema_by_id(schema_2_id, "bar1".to_string()).await.unwrap(), IdRow::new(schema_2_id, Schema{name: "bar1".to_string()}));
            assert!(meta_store.get_schema("bar".to_string()).await.is_err());
            assert_eq!(meta_store.get_schema("bar1".to_string()).await.unwrap(), IdRow::new(schema_2_id, Schema{name: "bar1".to_string()}));
            assert_eq!(meta_store.get_schema_by_id(schema_2_id).await.unwrap(), IdRow::new(schema_2_id, Schema{name: "bar1".to_string()}));

            assert_eq!(meta_store.delete_schema("bar1".to_string()).await.unwrap(), ());
            assert!(meta_store.delete_schema("bar1".to_string()).await.is_err());
            assert!(meta_store.delete_schema("bar".to_string()).await.is_err());

            assert!(meta_store.get_schema("bar1".to_string()).await.is_err());
            assert!(meta_store.get_schema("bar".to_string()).await.is_err());

            assert_eq!(meta_store.delete_schema_by_id(schema_3_id).await.unwrap(), ());
            assert!(meta_store.delete_schema_by_id(schema_2_id).await.is_err());
            assert_eq!(meta_store.delete_schema_by_id(schema_1_id).await.unwrap(), ());
            assert!(meta_store.delete_schema_by_id(schema_1_id).await.is_err());
            assert!(meta_store.get_schema("foo".to_string()).await.is_err());
            assert!(meta_store.get_schema("foo1".to_string()).await.is_err());
            assert!(meta_store.get_schema("boo".to_string()).await.is_err());

        }
        let _ = fs::remove_dir_all(store_path.clone());
        let _ = fs::remove_dir_all(remote_store_path.clone());
    }

    #[actix_rt::test]
    async fn table_test() {
        let store_path = env::current_dir().unwrap().join("test-table-local");
        let remote_store_path = env::current_dir().unwrap().join("test-table-remote");
        let _ = fs::remove_dir_all(store_path.clone());
        let _ = fs::remove_dir_all(remote_store_path.clone());
        let remote_fs = LocalDirRemoteFs::new(store_path.clone(), remote_store_path.clone());
        {
            let meta_store = RocksMetaStore::new(store_path.clone().join("metastore").as_path(), remote_fs);

            let schema_1 = meta_store.create_schema( "foo".to_string(), false).await.unwrap();
            let mut columns =  Vec::new();
            columns.push(Column::new("col1".to_string(), ColumnType::Int, 0));
            columns.push(Column::new("col2".to_string(), ColumnType::String, 1));
            columns.push(Column::new("col3".to_string(), ColumnType::Decimal, 2));

            let table1 = meta_store.create_table("foo".to_string(), "boo".to_string(), columns.clone(), None, None, vec![]).await.unwrap();
            let table1_id = table1.id;

            assert!(schema_1.id == table1.get_row().get_schema_id());
            assert!(meta_store.create_table("foo".to_string(), "boo".to_string(), columns.clone(), None, None, vec![]).await.is_err());

            assert_eq!(meta_store.get_table("foo".to_string(), "boo".to_string()).await.unwrap(), table1);

            let expected_index = Index::new("default".to_string(), table1_id, columns.clone(), columns.len() as u64);
            let expected_res = vec![IdRow::new(1, expected_index)];
            assert_eq!(meta_store.get_table_indexes(1).await.unwrap(), expected_res);

        }
        let _ = fs::remove_dir_all(store_path.clone());
        let _ = fs::remove_dir_all(remote_store_path.clone());
    }

    #[tokio::test]
    async fn cold_start_test() {
        let config = Config::test("cold_start_test");

        let _ = fs::remove_dir_all(config.local_dir());
        let _ = fs::remove_dir_all(config.remote_dir());

        {
            {
                let services = config.configure().await;
                services.start_processing_loops().await.unwrap();
                services.meta_store.create_schema("foo1".to_string(), false).await.unwrap();
                services.meta_store.run_upload().await.unwrap();
                services.meta_store.create_schema("foo".to_string(), false).await.unwrap();
                services.meta_store.upload_check_point().await.unwrap();
                services.meta_store.create_schema("bar".to_string(), false).await.unwrap();
                services.meta_store.run_upload().await.unwrap();
                services.stop_processing_loops().await.unwrap();
            }
            tokio::time::delay_for(Duration::from_millis(1000)).await; // TODO logger init conflict
            fs::remove_dir_all(config.local_dir()).unwrap();

            let services2 = config.configure().await;
            services2.meta_store.get_schema("foo1".to_string()).await.unwrap();
            services2.meta_store.get_schema("foo".to_string()).await.unwrap();
            services2.meta_store.get_schema("bar".to_string()).await.unwrap();
        }

        fs::remove_dir_all(config.local_dir()).unwrap();
        fs::remove_dir_all(config.remote_dir()).unwrap();
    }
}
