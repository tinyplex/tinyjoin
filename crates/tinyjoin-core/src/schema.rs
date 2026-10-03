//! Making a database's tables, columns, keys, and indexes match a schema an application declares,
//! in one atomic change made of the statements its DDL would run.

use std::rc::Rc;

use serde_json::{Map, Value};

use crate::{
    ColumnDefinition, ColumnType, EngineError, ForeignKeyDefinition, IndexDefinition, Result,
    TableDefinition,
    statement::{TableChange, WriteStatement},
    storage::{validate_index_columns_for_schema, validate_index_definition_shape, validate_schema},
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// A schema for [`crate::PagedEngine::set_schema`] to make the database's.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaDefinition {
    /// The schema's version, which the database's may not already exceed, and which it takes.
    pub version: u64,
    pub tables: Vec<TableTarget>,
}

/// A table of a [`SchemaDefinition`], with the names it and its columns may have had before.
#[derive(Clone, Debug, PartialEq)]
pub struct TableTarget {
    pub definition: TableDefinition,
    /// Every index the table has, which replace those it had.
    pub indexes: Vec<IndexDefinition>,
    /// A name the table had, which it is renamed from while it still has it.
    pub renamed_from: Option<String>,
    /// A name each column, by position, had, which it is renamed from while it still has it.
    pub column_renames: Vec<Option<String>>,
}

impl SchemaDefinition {
    /// Reads a schema as the JavaScript API writes one: `getSchema()`'s result, whose tables and
    /// columns may also name what they were renamed from.
    pub fn from_json(value: &Value) -> Result<Self> {
        let schema = object(value, "A schema", &["version", "tables"])?;
        let version = schema
            .get("version")
            .and_then(Value::as_u64)
            .filter(|version| *version <= MAX_SAFE_INTEGER)
            .ok_or_else(|| invalid("A schema's version must be a non-negative safe integer"))?;
        let mut tables = Vec::new();
        for table in items(schema, "tables", "A schema")? {
            tables.push(table_target(table)?);
        }
        Ok(Self { version, tables })
    }
}

fn table_target(value: &Value) -> Result<TableTarget> {
    let table = object(
        value,
        "A table",
        &[
            "name",
            "columns",
            "primaryKey",
            "indexes",
            "foreignKeys",
            "renamedFrom",
        ],
    )?;
    let name = text(table, "name", "A table")?;
    let primary_key = texts(table, "primaryKey", "A table")?;
    let mut columns = Vec::new();
    let mut column_renames = Vec::new();
    for value in items(table, "columns", "A table")? {
        let column = object(
            value,
            "A column",
            &[
                "name",
                "type",
                "nullable",
                "default",
                "maxLength",
                "renamedFrom",
            ],
        )?;
        let column_name = text(column, "name", "A column")?;
        let data_type = match text(column, "type", "A column")?.as_str() {
            "boolean" => ColumnType::Boolean,
            "integer" => ColumnType::Integer,
            "float" => ColumnType::Float,
            "text" => ColumnType::Text,
            "json" => ColumnType::Json,
            _ => {
                return Err(invalid(
                    "A column's type must be boolean, integer, float, text, or json",
                ));
            }
        };
        let max_length = match column.get("maxLength") {
            None => None,
            Some(length) => Some(
                length
                    .as_u64()
                    .and_then(|length| u32::try_from(length).ok())
                    .filter(|length| *length > 0 && data_type == ColumnType::Text)
                    .ok_or_else(|| invalid("Only a text column has a maxLength, from 1"))?,
            ),
        };
        // A key column is NOT NULL, as CREATE TABLE makes it.
        let nullable = flag(column, "nullable", "A column")? && !primary_key.contains(&column_name);
        columns.push(ColumnDefinition {
            name: column_name,
            data_type,
            nullable,
            default: column.get("default").cloned(),
            max_length,
        });
        column_renames.push(optional_text(column, "renamedFrom", "A column")?);
    }
    let mut indexes = Vec::new();
    for value in items(table, "indexes", "A table")? {
        let index = object(value, "An index", &["name", "columns", "unique"])?;
        indexes.push(IndexDefinition {
            name: text(index, "name", "An index")?,
            table: name.clone(),
            columns: texts(index, "columns", "An index")?,
            unique: flag(index, "unique", "An index")?,
        });
    }
    let mut foreign_keys = Vec::new();
    for value in items(table, "foreignKeys", "A table")? {
        object(
            value,
            "A foreign key",
            &[
                "name",
                "columns",
                "references",
                "referencedColumns",
                "onDelete",
                "onUpdate",
            ],
        )?;
        foreign_keys.push(ForeignKeyDefinition::from_json(value).ok_or_else(|| {
            invalid(
                "A foreign key needs a name, columns, references, referencedColumns, and an \
                 onDelete and onUpdate of no action, restrict, cascade, set null, or set default",
            )
        })?);
    }
    Ok(TableTarget {
        renamed_from: optional_text(table, "renamedFrom", "A table")?,
        definition: TableDefinition {
            name,
            primary_key,
            columns,
            foreign_keys,
        },
        indexes,
        column_renames,
    })
}

fn invalid(message: impl Into<String>) -> EngineError {
    EngineError::invalid_schema(message)
}

fn object<'a>(value: &'a Value, what: &str, keys: &[&str]) -> Result<&'a Map<String, Value>> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid(format!("{what} must be an object")))?;
    match object.keys().find(|key| !keys.contains(&key.as_str())) {
        Some(key) => Err(invalid(format!("{what} has no property `{key}`"))),
        None => Ok(object),
    }
}

fn items<'a>(object: &'a Map<String, Value>, key: &str, what: &str) -> Result<&'a Vec<Value>> {
    object
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("{what} needs an array of `{key}`")))
}

fn text(object: &Map<String, Value>, key: &str, what: &str) -> Result<String> {
    optional_text(object, key, what)?.ok_or_else(|| invalid(format!("{what} needs a `{key}`")))
}

fn optional_text(object: &Map<String, Value>, key: &str, what: &str) -> Result<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(invalid(format!("{what}'s `{key}` must be a string"))),
    }
}

fn texts(object: &Map<String, Value>, key: &str, what: &str) -> Result<Vec<String>> {
    let mut texts = Vec::new();
    for value in items(object, key, what)? {
        match value {
            Value::String(text) => texts.push(text.clone()),
            _ => return Err(invalid(format!("{what}'s `{key}` must hold strings"))),
        }
    }
    Ok(texts)
}

fn flag(object: &Map<String, Value>, key: &str, what: &str) -> Result<bool> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid(format!("{what} needs a boolean `{key}`")))
}

/// The statements that make the database's tables match `target`, ordered so that each finds
/// what it needs: tables are renamed, then dropped; each kept table's indexes are dropped, then
/// its columns renamed, then added, altered, and dropped, then its indexes created; and new
/// tables are created last. A table or column the target leaves out is dropped only if `drop`.
pub(crate) fn schema_statements(
    current: &[(Rc<TableDefinition>, Vec<IndexDefinition>)],
    version: u64,
    target: &SchemaDefinition,
    drop: bool,
) -> Result<Vec<WriteStatement>> {
    if target.version < version {
        return Err(EngineError::new(
            "SCHEMA_OUTDATED",
            format!(
                "The database's schema is at version {version}, past this schema's {}",
                target.version
            ),
        ));
    }
    let mut index_names = Vec::new();
    for (position, table) in target.tables.iter().enumerate() {
        validate_schema(&table.definition)?;
        if target.tables[..position]
            .iter()
            .any(|other| other.definition.name == table.definition.name)
        {
            return Err(invalid(format!(
                "The schema declares table `{}` more than once",
                table.definition.name
            )));
        }
        for index in &table.indexes {
            validate_index_definition_shape(index)?;
            validate_index_columns_for_schema(index, &table.definition)?;
            if index_names.contains(&&index.name) {
                return Err(EngineError::index_already_exists(&index.name));
            }
            index_names.push(&index.name);
        }
    }

    let find = |name: &str| current.iter().find(|(table, _)| table.name == name);
    let declared = |name: &str| {
        target
            .tables
            .iter()
            .any(|table| table.definition.name == name)
    };
    let alter = |table: &str, change| WriteStatement::AlterTable {
        table: table.to_owned(),
        change,
    };
    // The statements of each phase, in the order the phases run: tables renamed, foreign keys
    // and tables dropped, indexes dropped, columns renamed, then added, altered, and dropped,
    // indexes created, tables created, and foreign keys added once every table they reference is.
    let mut phases: [Vec<WriteStatement>; 9] = Default::default();
    let mut kept = Vec::new();
    let mut table_renames = Vec::new();
    let mut column_renames = Vec::new();
    let mut kept_tables = Vec::new();
    for target in &target.tables {
        let name = &target.definition.name;
        let existing = match (find(name), &target.renamed_from) {
            (Some(existing), _) => Some(existing),
            (None, Some(old)) if !declared(old) => find(old).map(|existing| {
                // A rename keeps the schema that qualifies a name, so the new name must too.
                let to = name.rsplit('.').next().unwrap_or(name);
                phases[0].push(alter(old, TableChange::RenameTable(to.to_owned())));
                table_renames.push((old.as_str(), name.as_str()));
                existing
            }),
            _ => None,
        };
        let Some((table, indexes)) = existing else {
            phases[7].push(WriteStatement::CreateTable {
                schema: TableDefinition {
                    foreign_keys: vec![],
                    ..target.definition.clone()
                },
                indexes: target.indexes.clone(),
                if_not_exists: false,
            });
            for key in &target.definition.foreign_keys {
                phases[8].push(alter(name, TableChange::AddForeignKey(key.clone())));
            }
            continue;
        };
        kept_tables.push((target, table));
        if crate::statement::renamed_table(&table.name, name.rsplit('.').next().unwrap_or(name))
            != *name
        {
            return Err(EngineError::unsupported_sql(format!(
                "Table `{}` cannot move to another schema as `{name}`",
                table.name
            )));
        }
        kept.push(&table.name);

        // Columns are renamed from names the table has but the target does not declare.
        let mut columns = table.columns.clone();
        let mut primary_key = table.primary_key.clone();
        let mut renamed_indexes = indexes.clone();
        for (column, old) in target.definition.columns.iter().zip(&target.column_renames) {
            let Some(old) = old else { continue };
            if columns.iter().any(|existing| existing.name == column.name)
                || target.definition.columns.iter().any(|other| other.name == *old)
            {
                continue;
            }
            let Some(existing) = columns.iter_mut().find(|existing| existing.name == *old) else {
                continue;
            };
            existing.name.clone_from(&column.name);
            for renamed in primary_key
                .iter_mut()
                .chain(renamed_indexes.iter_mut().flat_map(|index| &mut index.columns))
            {
                if renamed == old {
                    renamed.clone_from(&column.name);
                }
            }
            column_renames.push((name.as_str(), old.as_str(), column.name.as_str()));
            phases[4].push(alter(
                name,
                TableChange::RenameColumn {
                    from: old.clone(),
                    to: column.name.clone(),
                },
            ));
        }
        if primary_key != target.definition.primary_key {
            return Err(EngineError::unsupported_sql(format!(
                "The primary key of `{name}` cannot change"
            )));
        }

        for (index, renamed) in indexes.iter().zip(&renamed_indexes) {
            if !target.indexes.iter().any(|wanted| {
                wanted.name == index.name
                    && wanted.columns == renamed.columns
                    && wanted.unique == index.unique
            }) {
                phases[3].push(WriteStatement::DropIndex {
                    name: index.name.clone(),
                    if_exists: false,
                    cascade: false,
                });
            }
        }
        for wanted in &target.indexes {
            if !renamed_indexes.iter().any(|index| {
                index.name == wanted.name
                    && index.columns == wanted.columns
                    && index.unique == wanted.unique
            }) {
                phases[6].push(WriteStatement::CreateIndex {
                    definition: wanted.clone(),
                    if_not_exists: false,
                });
            }
        }

        for column in &target.definition.columns {
            let Some(existing) = columns.iter().find(|existing| existing.name == column.name)
            else {
                phases[5].push(WriteStatement::AddColumn {
                    table: name.clone(),
                    column: column.clone(),
                    if_not_exists: false,
                });
                continue;
            };
            if existing.data_type != column.data_type {
                return Err(EngineError::unsupported_sql(format!(
                    "Column `{}` of `{name}` cannot change its runtime type",
                    column.name
                )));
            }
            let mut changes = Vec::new();
            if existing.max_length != column.max_length {
                changes.push(TableChange::RestateType {
                    column: column.name.clone(),
                    data_type: column.data_type,
                    max_length: column.max_length,
                });
            }
            if existing.default != column.default {
                changes.push(TableChange::SetDefault {
                    column: column.name.clone(),
                    default: column.default.clone(),
                });
            }
            if existing.nullable != column.nullable {
                changes.push(TableChange::SetNullable {
                    column: column.name.clone(),
                    nullable: column.nullable,
                });
            }
            phases[5].extend(changes.into_iter().map(|change| alter(name, change)));
        }
        if drop {
            for existing in &columns {
                if !target
                    .definition
                    .columns
                    .iter()
                    .any(|column| column.name == existing.name)
                {
                    phases[5].push(alter(
                        name,
                        TableChange::DropColumn {
                            column: existing.name.clone(),
                            if_exists: false,
                            cascade: false,
                        },
                    ));
                }
            }
        }
    }
    // A foreign key is the target's when, with the tables and columns it names renamed, it is
    // one the target declares.
    let renamed_table = |table: &str| {
        table_renames
            .iter()
            .find(|(old, _)| *old == table)
            .map_or(table.to_owned(), |(_, new)| (*new).to_owned())
    };
    let renamed_columns = |table: &str, columns: &[String]| {
        columns
            .iter()
            .map(|column| {
                column_renames
                    .iter()
                    .find(|(renamed, old, _)| *renamed == table && *old == column)
                    .map_or(column.clone(), |(_, _, new)| (*new).to_owned())
            })
            .collect::<Vec<_>>()
    };
    for (target, table) in kept_tables {
        let name = &target.definition.name;
        let mut existing = Vec::new();
        for key in &table.foreign_keys {
            let references = renamed_table(&key.references);
            let renamed = ForeignKeyDefinition {
                columns: renamed_columns(name, &key.columns),
                referenced_columns: renamed_columns(&references, &key.referenced_columns),
                references,
                ..key.clone()
            };
            if !target.definition.foreign_keys.contains(&renamed) {
                phases[1].push(alter(
                    name,
                    TableChange::DropConstraint {
                        name: key.name.clone(),
                        if_exists: false,
                        cascade: false,
                    },
                ));
            }
            existing.push(renamed);
        }
        for key in &target.definition.foreign_keys {
            if !existing.contains(key) {
                phases[8].push(alter(name, TableChange::AddForeignKey(key.clone())));
            }
        }
    }
    if drop {
        for (table, _) in current {
            if !kept.contains(&&table.name) {
                phases[2].push(WriteStatement::DropTable {
                    table: table.name.clone(),
                    if_exists: false,
                    cascade: true,
                });
            }
        }
    }
    let mut statements = phases.into_iter().flatten().collect::<Vec<_>>();
    if target.version != version {
        statements.push(WriteStatement::SetSchemaVersion {
            version: target.version,
        });
    }
    Ok(statements)
}
