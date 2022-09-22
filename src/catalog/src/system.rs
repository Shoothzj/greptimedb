use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use common_query::logical_plan::Expr;
use common_recordbatch::SendableRecordBatchStream;
use common_telemetry::debug;
use common_time::timestamp::Timestamp;
use common_time::util;
use datatypes::prelude::{ConcreteDataType, ScalarVector};
use datatypes::schema::{ColumnSchema, Schema, SchemaBuilder, SchemaRef};
use datatypes::vectors::{BinaryVector, TimestampVector, UInt8Vector};
use serde::{Deserialize, Serialize};
use snafu::{ensure, OptionExt, ResultExt};
use table::engine::{EngineContext, TableEngineRef};
use table::metadata::TableId;
use table::requests::{CreateTableRequest, InsertRequest, OpenTableRequest};
use table::{Table, TableRef};

use crate::consts::{
    INFORMATION_SCHEMA_NAME, SYSTEM_CATALOG_NAME, SYSTEM_CATALOG_TABLE_ID,
    SYSTEM_CATALOG_TABLE_NAME,
};
use crate::error::{
    CreateSystemCatalogSnafu, EmptyValueSnafu, Error, InvalidEntryTypeSnafu, InvalidKeySnafu,
    OpenSystemCatalogSnafu, Result, ValueDeserializeSnafu,
};

pub const ENTRY_TYPE_INDEX: usize = 0;
pub const KEY_INDEX: usize = 1;
pub const TIMESTAMP_INDEX: usize = 2;
pub const VALUE_INDEX: usize = 3;

pub struct SystemCatalogTable {
    schema: SchemaRef,
    pub table: TableRef,
}

#[async_trait::async_trait]
impl Table for SystemCatalogTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    async fn scan(
        &self,
        _projection: &Option<Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> table::Result<SendableRecordBatchStream> {
        panic!("System catalog table does not support scan!")
    }

    /// Insert values into table.
    async fn insert(&self, request: InsertRequest) -> table::error::Result<usize> {
        self.table.insert(request).await
    }
}

impl SystemCatalogTable {
    pub async fn new(engine: TableEngineRef) -> Result<Self> {
        let request = OpenTableRequest {
            catalog_name: SYSTEM_CATALOG_NAME.to_string(),
            schema_name: INFORMATION_SCHEMA_NAME.to_string(),
            table_name: SYSTEM_CATALOG_TABLE_NAME.to_string(),
            table_id: SYSTEM_CATALOG_TABLE_ID,
        };
        let schema = Arc::new(build_system_catalog_schema());
        let ctx = EngineContext::default();

        if let Some(table) = engine
            .open_table(&ctx, request)
            .await
            .context(OpenSystemCatalogSnafu)?
        {
            Ok(Self { table, schema })
        } else {
            // system catalog table is not yet created, try to create
            let request = CreateTableRequest {
                id: SYSTEM_CATALOG_TABLE_ID,
                catalog_name: Some(SYSTEM_CATALOG_NAME.to_string()),
                schema_name: Some(INFORMATION_SCHEMA_NAME.to_string()),
                table_name: SYSTEM_CATALOG_TABLE_NAME.to_string(),
                desc: Some("System catalog table".to_string()),
                schema: schema.clone(),
                primary_key_indices: vec![ENTRY_TYPE_INDEX, KEY_INDEX, TIMESTAMP_INDEX],
                create_if_not_exists: true,
                table_options: HashMap::new(),
            };

            let table = engine
                .create_table(&ctx, request)
                .await
                .context(CreateSystemCatalogSnafu)?;
            Ok(Self { table, schema })
        }
    }

    /// Create a stream of all entries inside system catalog table
    pub async fn records(&self) -> Result<SendableRecordBatchStream> {
        let full_projection = None;
        let stream = self.table.scan(&full_projection, &[], None).await.unwrap();
        Ok(stream)
    }
}

/// Build system catalog table schema.
/// A system catalog table consists of 6 columns, namely
/// - entry_type: type of entry in current row, can be any variant of [EntryType].
/// - key: a binary encoded key of entry, differs according to different entry type.
/// - timestamp: currently not used.
/// - value: JSON-encoded value of entry's metadata.
/// - gmt_created: create time of this metadata.
/// - gmt_modified: last updated time of this metadata.
fn build_system_catalog_schema() -> Schema {
    let cols = vec![
        ColumnSchema::new(
            "entry_type".to_string(),
            ConcreteDataType::uint8_datatype(),
            false,
        ),
        ColumnSchema::new(
            "key".to_string(),
            ConcreteDataType::binary_datatype(),
            false,
        ),
        ColumnSchema::new(
            "timestamp".to_string(),
            ConcreteDataType::timestamp_millis_datatype(),
            false,
        ),
        ColumnSchema::new(
            "value".to_string(),
            ConcreteDataType::binary_datatype(),
            false,
        ),
        ColumnSchema::new(
            "gmt_created".to_string(),
            ConcreteDataType::timestamp_millis_datatype(),
            false,
        ),
        ColumnSchema::new(
            "gmt_modified".to_string(),
            ConcreteDataType::timestamp_millis_datatype(),
            false,
        ),
    ];

    // The schema of this table must be valid.
    SchemaBuilder::try_from(cols)
        .unwrap()
        .timestamp_index(2)
        .build()
        .unwrap()
}

pub fn build_table_insert_request(full_table_name: String, table_id: TableId) -> InsertRequest {
    let mut columns_values = HashMap::with_capacity(6);
    columns_values.insert(
        "entry_type".to_string(),
        Arc::new(UInt8Vector::from_slice(&[EntryType::Table as u8])) as _,
    );

    columns_values.insert(
        "key".to_string(),
        Arc::new(BinaryVector::from_slice(&[full_table_name.as_bytes()])) as _,
    );

    // Timestamp in key part is intentionally left to 0
    columns_values.insert(
        "timestamp".to_string(),
        Arc::new(TimestampVector::from_slice(&[Timestamp::from_millis(0)])) as _,
    );

    columns_values.insert(
        "value".to_string(),
        Arc::new(BinaryVector::from_slice(&[serde_json::to_string(
            &TableEntryValue { table_id },
        )
        .unwrap()
        .as_bytes()])) as _,
    );

    columns_values.insert(
        "gmt_created".to_string(),
        Arc::new(TimestampVector::from_slice(&[Timestamp::from_millis(
            util::current_time_millis(),
        )])) as _,
    );

    columns_values.insert(
        "gmt_modified".to_string(),
        Arc::new(TimestampVector::from_slice(&[Timestamp::from_millis(
            util::current_time_millis(),
        )])) as _,
    );

    InsertRequest {
        table_name: SYSTEM_CATALOG_TABLE_NAME.to_string(),
        columns_values,
    }
}

pub fn decode_system_catalog(
    entry_type: Option<u8>,
    key: Option<&[u8]>,
    value: Option<&[u8]>,
) -> Result<Entry> {
    debug!(
        "Decode system catalog entry: {:?}, {:?}, {:?}",
        entry_type, key, value
    );
    let entry_type = entry_type.context(InvalidKeySnafu { key: None })?;
    let key = String::from_utf8_lossy(key.context(InvalidKeySnafu { key: None })?);

    match EntryType::try_from(entry_type)? {
        EntryType::Catalog => {
            // As for catalog entry, the key is a string with format: `<catalog_name>`
            // and the value is current not used.
            let catalog_name = key.to_string();
            Ok(Entry::Catalog(CatalogEntry { catalog_name }))
        }
        EntryType::Schema => {
            // As for schema entry, the key is a string with format: `<catalog_name>.<schema_name>`
            // and the value is current not used.
            let schema_parts = key.split('.').collect::<Vec<_>>();
            ensure!(
                schema_parts.len() == 2,
                InvalidKeySnafu {
                    key: Some(key.to_string())
                }
            );
            Ok(Entry::Schema(SchemaEntry {
                catalog_name: schema_parts[0].to_string(),
                schema_name: schema_parts[1].to_string(),
            }))
        }

        EntryType::Table => {
            // As for table entry, the key is a string with format: `<catalog_name>.<schema_name>.<table_name>`
            // and the value is a JSON string with format: `{"table_id": <table_id>}`
            let table_parts = key.split('.').collect::<Vec<_>>();
            ensure!(
                table_parts.len() >= 3,
                InvalidKeySnafu {
                    key: Some(key.to_string())
                }
            );
            let value = value.context(EmptyValueSnafu)?;
            debug!("Table meta value: {}", String::from_utf8_lossy(value));
            let table_meta: TableEntryValue =
                serde_json::from_slice(value).context(ValueDeserializeSnafu)?;
            Ok(Entry::Table(TableEntry {
                catalog_name: table_parts[0].to_string(),
                schema_name: table_parts[1].to_string(),
                table_name: table_parts[2].to_string(),
                table_id: table_meta.table_id,
            }))
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum EntryType {
    Catalog = 1,
    Schema = 2,
    Table = 3,
}

impl TryFrom<u8> for EntryType {
    type Error = Error;

    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            b if b == Self::Catalog as u8 => Ok(Self::Catalog),
            b if b == Self::Schema as u8 => Ok(Self::Schema),
            b if b == Self::Table as u8 => Ok(Self::Table),
            b => InvalidEntryTypeSnafu {
                entry_type: Some(b),
            }
            .fail(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Entry {
    Catalog(CatalogEntry),
    Schema(SchemaEntry),
    Table(TableEntry),
}

#[derive(Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub catalog_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SchemaEntry {
    pub catalog_name: String,
    pub schema_name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TableEntry {
    pub catalog_name: String,
    pub schema_name: String,
    pub table_name: String,
    pub table_id: TableId,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableEntryValue {
    pub table_id: TableId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub fn test_decode_catalog_enrty() {
        let entry = decode_system_catalog(
            Some(EntryType::Catalog as u8),
            Some("some_catalog".as_bytes()),
            None,
        )
        .unwrap();
        if let Entry::Catalog(e) = entry {
            assert_eq!("some_catalog", e.catalog_name);
        } else {
            panic!("Unexpected type: {:?}", entry);
        }
    }

    #[test]
    pub fn test_decode_schema_entry() {
        let entry = decode_system_catalog(
            Some(EntryType::Schema as u8),
            Some("some_catalog.some_schema".as_bytes()),
            None,
        )
        .unwrap();

        if let Entry::Schema(e) = entry {
            assert_eq!("some_catalog", e.catalog_name);
            assert_eq!("some_schema", e.schema_name);
        } else {
            panic!("Unexpected type: {:?}", entry);
        }
    }

    #[test]
    pub fn test_decode_table() {
        let entry = decode_system_catalog(
            Some(EntryType::Table as u8),
            Some("some_catalog.some_schema.some_table".as_bytes()),
            Some("{\"table_id\":42}".as_bytes()),
        )
        .unwrap();

        if let Entry::Table(e) = entry {
            assert_eq!("some_catalog", e.catalog_name);
            assert_eq!("some_schema", e.schema_name);
            assert_eq!("some_table", e.table_name);
            assert_eq!(42, e.table_id);
        } else {
            panic!("Unexpected type: {:?}", entry);
        }
    }

    #[test]
    #[should_panic]
    pub fn test_decode_mismatch() {
        decode_system_catalog(
            Some(EntryType::Table as u8),
            Some("some_catalog.some_schema.some_table".as_bytes()),
            None,
        )
        .unwrap();
    }

    #[test]
    pub fn test_entry_type() {
        assert_eq!(EntryType::Catalog, EntryType::try_from(1).unwrap());
        assert_eq!(EntryType::Schema, EntryType::try_from(2).unwrap());
        assert_eq!(EntryType::Table, EntryType::try_from(3).unwrap());
        assert!(EntryType::try_from(4).is_err());
    }
}
