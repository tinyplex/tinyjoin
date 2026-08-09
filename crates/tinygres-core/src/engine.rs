use serde_json::Value;

use crate::{
    ApplyOutcome, ChangeBatch, InMemoryStorage, QueryPlan, QueryResult, Result, Row, StorageDriver,
    TableSchema,
};

#[derive(Clone, Debug)]
pub struct Engine<S = InMemoryStorage> {
    storage: S,
}

impl Default for Engine<InMemoryStorage> {
    fn default() -> Self {
        Self::new(InMemoryStorage::default())
    }
}

impl<S: StorageDriver> Engine<S> {
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    pub fn define_table(&mut self, schema: TableSchema) -> Result<()> {
        self.storage.define_table(schema)
    }

    pub fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome> {
        self.storage.replace_table(table, rows)
    }

    pub fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome> {
        self.storage.apply_batch(batch)
    }

    pub fn query(&self, plan: &QueryPlan) -> Result<QueryResult> {
        crate::query::execute(&self.storage, plan)
    }

    pub fn query_sql(&self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        let plan = crate::query::parse_sql(sql, params)?;
        self.query(&plan)
    }

    pub fn revision(&self) -> u64 {
        self.storage.revision()
    }

    pub fn into_storage(self) -> S {
        self.storage
    }
}
