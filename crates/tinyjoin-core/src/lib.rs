#![deny(unreachable_pub)]

mod aggregate;
mod btree;
mod cache;
mod checksum;
#[cfg(test)]
mod corpus_support;
mod device;
#[cfg(test)]
mod engine;
mod error;
mod hash;
mod join;
mod model;
mod page;
mod paged_codec;
mod paged_engine;
mod paged_script;
mod paged_storage;
mod paged_transaction;
mod pager;
mod prepared_statement;
mod query;
#[cfg(test)]
mod recovery_property_tests;
mod revision;
#[cfg(test)]
mod semantic_property_tests;
mod sql_script;
mod statement;
mod storage;

pub(crate) use btree::{Btree, MAX_BTREE_KEY_BYTES, MAX_BTREE_VALUE_BYTES, TreeId};
#[cfg(test)]
pub(crate) use cache::MAX_PAGE_CACHE_BYTES;
pub(crate) use cache::{CandidateId, DEFAULT_PAGE_CACHE_BYTES, PageCache};
#[cfg(test)]
pub(crate) use device::MemoryPageDevice;
pub use device::PageDevice;
#[cfg(test)]
pub(crate) use engine::Engine;
pub use error::{EngineError, Result};
pub use model::{ApplyOutcome, ExecuteResult, ResultField, Row};
pub(crate) use model::{
    ColumnDefinition, ColumnType, ComparisonOperator, IndexDefinition, NullOrder, OrderBy,
    OrderDirection, Predicate, QueryResult, RowChange, SelectPlan, TableDefinition,
};
#[cfg(test)]
pub(crate) use page::BitmapSlot;
pub(crate) use page::{
    AllocationBitmap, FIRST_DATA_PAGE_ID, MAX_PAGE_PAYLOAD_SIZE, Page, PageType, RawMetadataSlot,
    RecoveredMetadata, SUPERBLOCK_PAGE_COUNT, Superblock, SuperblockSlot, build_next_metadata,
    recover_metadata,
};
pub use page::{MAX_PAGE_COUNT, PAGE_SIZE, PageId};
pub use paged_engine::PagedEngine;
pub(crate) use paged_storage::PagedStorage;
pub(crate) use pager::{Pager, PagerWriteTransaction};
pub use prepared_statement::PreparedStatementId;
#[cfg(test)]
pub(crate) use storage::InMemoryStorage;
#[cfg(test)]
pub(crate) use storage::StorageDriver;
pub(crate) use storage::{StorageReader, VisitControl, VisitOutcome};
