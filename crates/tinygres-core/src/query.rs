use std::str::FromStr;

use serde_json::{Map, Number, Value};
use sqlparser::{
    ast::{
        BinaryOperator, Expr, GroupByExpr, LimitClause, ObjectName, ObjectNamePart, Query, Select,
        SelectItem, SetExpr, Statement, TableFactor, Value as SqlValue,
    },
    dialect::PostgreSqlDialect,
    parser::Parser,
};

use crate::{
    Filter, FilterOperator, QueryPlan, QueryResult, Result, Row, StorageDriver, TinygresError,
};

const MAX_SQL_BYTES: usize = 64 * 1024;

pub(crate) fn execute<S: StorageDriver>(storage: &S, plan: &QueryPlan) -> Result<QueryResult> {
    if plan.table.trim().is_empty() {
        return Err(TinygresError::invalid_query(
            "A query must name exactly one table",
        ));
    }
    if plan.columns.as_ref().is_some_and(Vec::is_empty) {
        return Err(TinygresError::invalid_query(
            "A projection must contain at least one column",
        ));
    }

    let mut rows = Vec::new();
    for row in storage.scan_table(&plan.table)? {
        if !matches_filters(&row, &plan.filters, &plan.table)? {
            continue;
        }
        rows.push(project_row(row, plan.columns.as_deref(), &plan.table)?);
        if plan.limit.is_some_and(|limit| rows.len() >= limit) {
            break;
        }
    }

    Ok(QueryResult {
        revision: storage.revision(),
        rows,
    })
}

pub(crate) fn parse_sql(sql: &str, params: &[Value]) -> Result<QueryPlan> {
    if sql.len() > MAX_SQL_BYTES {
        return Err(TinygresError::invalid_query(format!(
            "SQL text exceeds the {MAX_SQL_BYTES}-byte limit"
        )));
    }

    let dialect = PostgreSqlDialect {};
    let statements = Parser::parse_sql(&dialect, sql)
        .map_err(|error| TinygresError::parse_error(error.to_string()))?;
    if statements.len() != 1 {
        return Err(TinygresError::unsupported_sql(
            "Exactly one SELECT statement is supported",
        ));
    }

    let statement = statements
        .into_iter()
        .next()
        .expect("statement length was checked");
    let Statement::Query(query) = statement else {
        return Err(TinygresError::unsupported_sql(
            "Only read-only SELECT statements are supported",
        ));
    };

    query_to_plan(*query, params)
}

fn query_to_plan(query: Query, params: &[Value]) -> Result<QueryPlan> {
    if query.with.is_some()
        || query.order_by.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return Err(unsupported_shape());
    }

    let limit = parse_limit(query.limit_clause.as_ref(), params)?;
    let SetExpr::Select(select) = *query.body else {
        return Err(unsupported_shape());
    };
    select_to_plan(*select, params, limit)
}

fn select_to_plan(select: Select, params: &[Value], limit: Option<usize>) -> Result<QueryPlan> {
    let group_by_is_empty = matches!(
        &select.group_by,
        GroupByExpr::Expressions(expressions, modifiers)
            if expressions.is_empty() && modifiers.is_empty()
    );
    if select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.into.is_some()
        || select.from.len() != 1
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !group_by_is_empty
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
    {
        return Err(unsupported_shape());
    }

    let from = &select.from[0];
    if !from.joins.is_empty() {
        return Err(unsupported_shape());
    }
    let table = parse_table_name(&from.relation)?;
    let columns = parse_projection(&select.projection)?;
    let filters = select
        .selection
        .as_ref()
        .map(|selection| parse_filters(selection, params))
        .transpose()?
        .unwrap_or_default();

    Ok(QueryPlan {
        table,
        columns,
        filters,
        limit,
    })
}

fn parse_table_name(table: &TableFactor) -> Result<String> {
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = table
    else {
        return Err(unsupported_shape());
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return Err(unsupported_shape());
    }
    normalize_object_name(name)
}

fn normalize_object_name(name: &ObjectName) -> Result<String> {
    if name.0.is_empty() || name.0.len() > 2 {
        return Err(TinygresError::unsupported_sql(
            "Only unqualified or schema-qualified table names are supported",
        ));
    }
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(identifier) => Ok(normalize_identifier(identifier)),
            _ => Err(unsupported_shape()),
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("."))
}

fn parse_projection(items: &[SelectItem]) -> Result<Option<Vec<String>>> {
    if items.len() == 1 && matches!(items[0], SelectItem::Wildcard(_)) {
        return Ok(None);
    }

    let mut columns = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SelectItem::UnnamedExpr(Expr::Identifier(identifier)) => {
                columns.push(normalize_identifier(identifier));
            }
            _ => return Err(unsupported_shape()),
        }
    }
    if columns.is_empty() {
        return Err(unsupported_shape());
    }
    Ok(Some(columns))
}

fn parse_filters(expression: &Expr, params: &[Value]) -> Result<Vec<Filter>> {
    match expression {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut filters = parse_filters(left, params)?;
            filters.extend(parse_filters(right, params)?);
            Ok(filters)
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let Expr::Identifier(identifier) = left.as_ref() else {
                return Err(unsupported_shape());
            };
            Ok(vec![Filter {
                column: normalize_identifier(identifier),
                operator: FilterOperator::Eq,
                value: parse_value(right, params)?,
            }])
        }
        _ => Err(unsupported_shape()),
    }
}

fn parse_limit(limit: Option<&LimitClause>, params: &[Value]) -> Result<Option<usize>> {
    let Some(limit) = limit else {
        return Ok(None);
    };
    let LimitClause::LimitOffset {
        limit: Some(expression),
        offset: None,
        limit_by,
    } = limit
    else {
        return Err(unsupported_shape());
    };
    if !limit_by.is_empty() {
        return Err(unsupported_shape());
    }
    let value = parse_value(expression, params)?;
    let Value::Number(number) = value else {
        return Err(TinygresError::invalid_query(
            "LIMIT must be a non-negative integer",
        ));
    };
    number
        .as_u64()
        .and_then(|number| usize::try_from(number).ok())
        .map(Some)
        .ok_or_else(|| TinygresError::invalid_query("LIMIT is too large"))
}

fn parse_value(expression: &Expr, params: &[Value]) -> Result<Value> {
    let Expr::Value(value) = expression else {
        return Err(unsupported_shape());
    };
    match &value.value {
        SqlValue::Null => Ok(Value::Null),
        SqlValue::Boolean(value) => Ok(Value::Bool(*value)),
        SqlValue::SingleQuotedString(value)
        | SqlValue::EscapedStringLiteral(value)
        | SqlValue::UnicodeStringLiteral(value) => Ok(Value::String(value.clone())),
        SqlValue::Number(value, _) => Number::from_str(value)
            .map(Value::Number)
            .map_err(|_| TinygresError::invalid_query(format!("Invalid number literal `{value}`"))),
        SqlValue::Placeholder(placeholder) => bind_parameter(placeholder, params),
        _ => Err(unsupported_shape()),
    }
}

fn bind_parameter(placeholder: &str, params: &[Value]) -> Result<Value> {
    let Some(index) = placeholder.strip_prefix('$') else {
        return Err(TinygresError::bind_error(format!(
            "Only PostgreSQL-style placeholders are supported, received `{placeholder}`"
        )));
    };
    let index = index.parse::<usize>().map_err(|_| {
        TinygresError::bind_error(format!("Invalid parameter placeholder `{placeholder}`"))
    })?;
    if index == 0 {
        return Err(TinygresError::bind_error(
            "PostgreSQL parameter indexes start at $1",
        ));
    }
    params.get(index - 1).cloned().ok_or_else(|| {
        TinygresError::bind_error(format!("No value was provided for `{placeholder}`"))
    })
}

fn matches_filters(row: &Row, filters: &[Filter], table: &str) -> Result<bool> {
    for filter in filters {
        let Some(actual) = row.get(&filter.column) else {
            return Err(TinygresError::column_not_found(&filter.column, table));
        };
        let matched = match filter.operator {
            FilterOperator::Eq => {
                actual != &Value::Null && filter.value != Value::Null && actual == &filter.value
            }
        };
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

fn project_row(mut row: Row, columns: Option<&[String]>, table: &str) -> Result<Row> {
    let Some(columns) = columns else {
        return Ok(row);
    };
    let mut projected = Map::new();
    for column in columns {
        let value = row
            .remove(column)
            .ok_or_else(|| TinygresError::column_not_found(column, table))?;
        projected.insert(column.clone(), value);
    }
    Ok(projected)
}

fn normalize_identifier(identifier: &sqlparser::ast::Ident) -> String {
    if identifier.quote_style.is_none() {
        identifier.value.to_lowercase()
    } else {
        identifier.value.clone()
    }
}

fn unsupported_shape() -> TinygresError {
    TinygresError::unsupported_sql(
        "The initial SQL subset supports SELECT columns FROM one table, ANDed equality filters, and LIMIT",
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{Engine, InMemoryStorage, TableSchema};

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn engine() -> Engine<InMemoryStorage> {
        let mut engine = Engine::default();
        engine
            .define_table(TableSchema {
                name: "posts".to_owned(),
                primary_key: vec!["id".to_owned()],
            })
            .unwrap();
        engine
            .replace_table(
                "posts",
                vec![
                    row(json!({"id": 1, "title": "one", "user_id": 7, "deleted": null})),
                    row(json!({"id": 2, "title": "two", "user_id": 8, "deleted": null})),
                    row(json!({"id": 3, "title": "three", "user_id": 7, "deleted": true})),
                ],
            )
            .unwrap();
        engine
    }

    #[test]
    fn executes_projection_parameter_filter_and_limit() {
        let result = engine()
            .query_sql(
                "SELECT id, title FROM posts WHERE user_id = $1 LIMIT 1",
                &[json!(7)],
            )
            .unwrap();

        assert_eq!(result.revision, 1);
        assert_eq!(result.rows, vec![row(json!({"id": 1, "title": "one"}))]);
    }

    #[test]
    fn supports_anded_equality_filters() {
        let result = engine()
            .query_sql(
                "SELECT * FROM posts WHERE user_id = $1 AND deleted = $2",
                &[json!(7), json!(true)],
            )
            .unwrap();

        assert_eq!(
            result.rows,
            vec![row(
                json!({"id": 3, "title": "three", "user_id": 7, "deleted": true})
            )]
        );
    }

    #[test]
    fn folds_unquoted_identifiers_but_preserves_quoted_ones() {
        assert_eq!(
            parse_sql("SELECT ID FROM POSTS", &[]).unwrap(),
            QueryPlan {
                table: "posts".to_owned(),
                columns: Some(vec!["id".to_owned()]),
                filters: vec![],
                limit: None,
            }
        );
        assert_eq!(
            parse_sql("SELECT \"ID\" FROM \"Posts\"", &[]).unwrap(),
            QueryPlan {
                table: "Posts".to_owned(),
                columns: Some(vec!["ID".to_owned()]),
                filters: vec![],
                limit: None,
            }
        );
    }

    #[test]
    fn null_equality_never_matches() {
        let result = engine()
            .query_sql("SELECT * FROM posts WHERE deleted = NULL", &[])
            .unwrap();
        assert!(result.rows.is_empty());
    }

    #[test]
    fn rejects_missing_bind_values() {
        let error = engine()
            .query_sql("SELECT * FROM posts WHERE user_id = $1", &[])
            .unwrap_err();
        assert_eq!(error.code, "BIND_ERROR");
    }

    #[test]
    fn rejects_unknown_columns() {
        let error = engine()
            .query_sql("SELECT missing FROM posts", &[])
            .unwrap_err();
        assert_eq!(error.code, "COLUMN_NOT_FOUND");
    }

    #[test]
    fn rejects_every_unimplemented_query_shape() {
        for sql in [
            "SELECT * FROM posts ORDER BY id",
            "SELECT count(*) FROM posts",
            "SELECT * FROM posts JOIN users ON posts.user_id = users.id",
            "SELECT * FROM posts WHERE id > 1",
            "SELECT DISTINCT * FROM posts",
            "SELECT * FROM posts; SELECT * FROM posts",
            "UPDATE posts SET title = 'no'",
        ] {
            let error = engine().query_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, "UNSUPPORTED_SQL", "query was `{sql}`");
        }
    }
}
