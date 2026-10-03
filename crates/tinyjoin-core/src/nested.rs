//! Queries that read the rows around them and gather rows into JSON: a select list's
//! `(SELECT ...)`, and a `LEFT JOIN LATERAL (SELECT ...) alias ON true`, whose rows `json_agg`,
//! `json_build_array`, `to_json`, and `coalesce` gather into one JSON value, as Drizzle's
//! relational queries and Kysely's `jsonArrayFrom` and `jsonObjectFrom` write them.
//!
//! Each level runs as the queries the engine already has: the rows of its table, or of the query
//! it reads from, and then, for each of those rows, the queries nested in it, with the row's
//! values bound in place of the columns they read from it.

use serde_json::Value;

use crate::{
    ColumnType, EngineError, NullOrder, OrderBy, OrderDirection, QueryResult, Result, ResultField,
    Row, StorageReader,
    query::{ParseMode, Token, identifier_name, is_keyword, names_repeat, position_name},
    storage::estimated_value_bytes,
};

/// The most queries one statement runs for the queries nested in it.
const MAX_NESTED_QUERIES: usize = 100_000;
/// The most estimated bytes of values those queries return, together.
const MAX_NESTED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct NestedPlan {
    pub(crate) tokens: Vec<Token>,
    pub(crate) params: Vec<Value>,
    /// Whether rows are read as arrays, so that outputs may repeat names.
    pub(crate) array_rows: bool,
}

/// Whether a `SELECT` nests a query in its select list, or joins one `LATERAL`.
pub(crate) fn is_nested(tokens: &[Token]) -> bool {
    let mut depth = 0usize;
    let mut listing = true;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::LParen => {
                if depth == 0 && listing && is_keyword(tokens.get(index + 1), "select") {
                    return true;
                }
                depth += 1;
            }
            Token::RParen => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ if is_keyword(Some(token), "from") => listing = false,
            _ if is_keyword(Some(token), "lateral") => return true,
            _ => {}
        }
    }
    false
}

pub(crate) fn execute(storage: &dyn StorageReader, plan: &NestedPlan) -> Result<QueryResult> {
    let Rows { fields, rows } = evaluate(
        storage,
        &plan.tokens,
        &plan.params,
        &mut Vec::new(),
        &mut Budget::default(),
    )?;
    let (keys, positional) = keys(&fields);
    if positional && !plan.array_rows {
        return Err(EngineError::invalid_query(
            "SELECT produces an output column more than once; use distinct AS aliases",
        ));
    }
    let mut result = Vec::with_capacity(rows.len());
    for values in rows {
        let mut row = Row::new();
        for (key, value) in keys.iter().zip(values) {
            row.insert(key.clone(), value);
        }
        result.push(row);
    }
    Ok(QueryResult {
        revision: storage.revision(),
        fields,
        rows: result,
    })
}

/// The rows a query gives, with each value in its field's place.
struct Rows {
    fields: Vec<ResultField>,
    rows: Vec<Vec<Value>>,
}

/// The rows around a nested query, by alias.
type Bindings = Vec<(String, Row)>;

/// An `alias.column`.
type Reference = (String, String);

/// A row of a level: the outputs of its own query, and the rows it binds for what it nests.
type BaseRow = (Vec<Value>, Bindings);

#[derive(Default)]
struct Budget {
    queries: usize,
    bytes: usize,
}

/// A query that runs for each row of the query around it: a `LATERAL` join, with its alias, or
/// a select list's subquery.
type Lateral = (Option<String>, Vec<Token>);

enum Item {
    /// An output of the level's own query.
    Plain,
    /// A column of the row a [`Lateral`] gives: a `LATERAL` join's, by name, or a subquery's only
    /// one.
    Lateral(usize, Option<String>),
    /// A JSON value made from each row.
    Each(Element),
    /// `json_agg`: an array of a JSON value made from each row, or, where there are none, NULL,
    /// or `[]` if `coalesce` gives that instead. It orders them by `alias.column`s, each of which
    /// its sort's rows hold under their position.
    Gathered {
        element: Element,
        order: Vec<Reference>,
        order_by: Vec<OrderBy>,
        coalesced: bool,
    },
}

enum Element {
    /// `alias.column`, which `json_agg` gathers.
    Column(Reference),
    /// `json_build_array(alias.column, ...)`.
    Array(Vec<Reference>),
    /// `to_json(alias)`, or an alias that `json_agg` gathers: its row, as an object.
    Object(String),
}

fn invalid(message: &str) -> EngineError {
    EngineError::unsupported_sql(format!("A nested query {message}"))
}

/// The index of the parenthesis that closes the one at `open`.
fn closing(tokens: &[Token], open: usize) -> Result<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::LParen => depth += 1,
            Token::RParen if depth == 1 => return Ok(index),
            Token::RParen => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Err(EngineError::parse_error("Expected `)`"))
}

/// The index of the first of `keywords` outside parentheses in `tokens`, from `start`.
fn top_level(tokens: &[Token], start: usize, keywords: &[&str]) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ if depth == 0
                && keywords
                    .iter()
                    .any(|keyword| is_keyword(Some(token), keyword)) =>
            {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

/// `tokens` split at each comma outside parentheses.
fn split(tokens: &[Token]) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => {
                parts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&tokens[start..]);
    parts
}

/// A `qualifier.column` reference at `index`.
fn reference(tokens: &[Token], index: usize) -> Option<Reference> {
    match tokens.get(index + 1) {
        Some(Token::Dot) => Some((
            identifier_name(tokens.get(index))?,
            identifier_name(tokens.get(index + 2))?,
        )),
        _ => None,
    }
}

/// The alias a source at `position` is read under, after `AS` or alone, moving past it.
fn alias(tokens: &[Token], position: &mut usize) -> Option<String> {
    let named = usize::from(is_keyword(tokens.get(*position), "as"));
    let token = tokens.get(*position + named);
    if named == 0
        && ["cross", "lateral"]
            .iter()
            .any(|word| is_keyword(token, word))
    {
        return None;
    }
    let alias = identifier_name(token)?;
    *position += named + 1;
    Some(alias)
}

/// A table at `position`, perhaps qualified, and the alias it is read under, moving past both.
fn table(tokens: &[Token], position: &mut usize) -> Option<(String, String)> {
    *position += if matches!(tokens.get(*position + 1), Some(Token::Dot)) {
        3
    } else {
        1
    };
    let table = identifier_name(tokens.get(*position - 1))?;
    Some((
        alias(tokens, position).unwrap_or_else(|| table.clone()),
        table,
    ))
}

/// A select list's item: what it gives, and the name it gives it under.
fn item(tokens: &[Token], laterals: &mut Vec<Lateral>) -> Result<(Item, Option<String>)> {
    let (body, name) = match tokens {
        [body @ .., keyword, name] if is_keyword(Some(keyword), "as") => {
            (body, identifier_name(Some(name)))
        }
        [.., Token::RParen, name @ Token::Identifier { .. }] => {
            (&tokens[..tokens.len() - 1], identifier_name(Some(name)))
        }
        _ => (tokens, None),
    };
    if matches!(body.first(), Some(Token::LParen)) && is_keyword(body.get(1), "select") {
        if closing(body, 0)? != body.len() - 1 {
            return Err(invalid("must be a whole item of a select list"));
        }
        laterals.push((None, body[1..body.len() - 1].to_vec()));
        return Ok((Item::Lateral(laterals.len() - 1, None), name));
    }
    if let (3, Some((alias, column))) = (body.len(), reference(body, 0))
        && let Some(lateral) = laterals
            .iter()
            .position(|(lateral, _)| lateral.as_ref() == Some(&alias))
    {
        return Ok((Item::Lateral(lateral, Some(column)), name));
    }
    let function = [
        "coalesce",
        "json_agg",
        "json_build_array",
        "to_json",
        "row_to_json",
    ]
    .iter()
    .any(|function| is_keyword(body.first(), function));
    if !function || !matches!(body.get(1), Some(Token::LParen)) {
        return Ok((Item::Plain, name));
    }
    let name = name.or_else(|| identifier_name(body.first()));
    let coalesced = is_keyword(body.first(), "coalesce");
    let body = if coalesced {
        match *split(arguments(body)?) {
            [gathered, [Token::String(empty)]] if empty == "[]" => gathered,
            [gathered, [Token::String(empty), Token::Cast, kind]]
                if empty == "[]"
                    && (is_keyword(Some(kind), "json") || is_keyword(Some(kind), "jsonb")) =>
            {
                gathered
            }
            _ => return Err(invalid("can only coalesce json_agg with '[]'")),
        }
    } else {
        body
    };
    if !is_keyword(body.first(), "json_agg") {
        if coalesced {
            return Err(invalid("can only coalesce json_agg with '[]'"));
        }
        return Ok((Item::Each(element(body)?), name));
    }
    let inner = arguments(body)?;
    let end = top_level(inner, 0, &["order"]).unwrap_or(inner.len());
    let element = element(&inner[..end])?;
    let (order, order_by) = match &inner[end..] {
        [] => Default::default(),
        [_, by, terms @ ..] if is_keyword(Some(by), "by") => order(terms)?,
        _ => return Err(invalid("orders json_agg only by qualified columns")),
    };
    Ok((
        Item::Gathered {
            element,
            order,
            order_by,
            coalesced,
        },
        name,
    ))
}

/// The arguments of the function that is the whole of `body`.
fn arguments(body: &[Token]) -> Result<&[Token]> {
    if !matches!(body.get(1), Some(Token::LParen)) || closing(body, 1)? != body.len() - 1 {
        return Err(invalid("makes each output with one JSON function"));
    }
    Ok(&body[2..body.len() - 1])
}

/// What makes JSON of a row: a row's alias, a qualified column,
/// `json_build_array(alias.column, ...)`, or `to_json` of one of the first two.
fn element(body: &[Token]) -> Result<Element> {
    let refused = || {
        invalid("makes JSON only of rows and qualified columns, with to_json or json_build_array")
    };
    match body {
        [alias] => {
            return identifier_name(Some(alias))
                .map(Element::Object)
                .ok_or_else(refused);
        }
        [_, Token::Dot, _] => return reference(body, 0).map(Element::Column).ok_or_else(refused),
        _ => {}
    }
    let arguments = arguments(body)?;
    if is_keyword(body.first(), "json_build_array") {
        let mut references = Vec::new();
        for argument in split(arguments) {
            match element(argument)? {
                Element::Column(reference) => references.push(reference),
                _ => return Err(refused()),
            }
        }
        return Ok(Element::Array(references));
    }
    if is_keyword(body.first(), "to_json") || is_keyword(body.first(), "row_to_json") {
        return element(arguments);
    }
    Err(refused())
}

fn order(terms: &[Token]) -> Result<(Vec<Reference>, Vec<OrderBy>)> {
    let refused = || invalid("orders json_agg only by qualified columns");
    let mut order = Vec::new();
    let mut order_by = Vec::new();
    for term in split(terms) {
        order.push(reference(term, 0).ok_or_else(refused)?);
        let mut rest = &term[3..];
        let descending = is_keyword(rest.first(), "desc");
        if descending || is_keyword(rest.first(), "asc") {
            rest = &rest[1..];
        }
        let nulls = match rest {
            [] => NullOrder::Default,
            [nulls, place] if is_keyword(Some(nulls), "nulls") => {
                if is_keyword(Some(place), "first") {
                    NullOrder::First
                } else if is_keyword(Some(place), "last") {
                    NullOrder::Last
                } else {
                    return Err(refused());
                }
            }
            _ => return Err(refused()),
        };
        order_by.push(OrderBy {
            column: order_by.len().to_string(),
            direction: if descending {
                OrderDirection::Desc
            } else {
                OrderDirection::Asc
            },
            nulls,
        });
    }
    Ok((order, order_by))
}

/// The row bound to `alias`, which an outer reference reads.
fn bound<'a>(bindings: &'a Bindings, alias: &str) -> Result<&'a Row> {
    match bindings.iter().rev().find(|(name, _)| name == alias) {
        Some((_, row)) => Ok(row),
        None => Err(EngineError::invalid_query(format!(
            "No table of the query is named `{alias}`"
        ))),
    }
}

fn lookup(bindings: &Bindings, alias: &str, column: &str) -> Result<Value> {
    match bound(bindings, alias)?.get(column) {
        Some(value) => Ok(value.clone()),
        None => Err(EngineError::column_not_found(column, alias)),
    }
}

fn value(element: &Element, bindings: &Bindings) -> Result<Value> {
    Ok(match element {
        Element::Column((alias, column)) => lookup(bindings, alias, column)?,
        Element::Array(references) => {
            let mut values = Vec::with_capacity(references.len());
            for (alias, column) in references {
                values.push(lookup(bindings, alias, column)?);
            }
            Value::Array(values)
        }
        Element::Object(alias) => Value::Object(bound(bindings, alias)?.clone()),
    })
}

/// One level of a nested query: a `SELECT` and what nests in it.
struct Level<'a> {
    /// Where `FROM` is, and where its source and alias end.
    from: usize,
    position: usize,
    /// The alias the level reads its source under.
    source: String,
    /// The query the level reads from, if it reads from one rather than a table.
    derived: Option<&'a [Token]>,
    /// The tables the level's own query reads, and the aliases it reads them under.
    tables: Vec<String>,
    aliases: Vec<String>,
    laterals: Vec<Lateral>,
    /// The level's own query after its source, without its `LATERAL` joins.
    rest: Vec<Token>,
    items: Vec<(Item, Option<String>, &'a [Token])>,
    /// How many items its own query gives, and how many `json_agg` gathers.
    plain: usize,
    gathering: usize,
}

fn level(tokens: &[Token]) -> Result<Level<'_>> {
    let from = top_level(tokens, 1, &["from"])
        .filter(|_| is_keyword(tokens.first(), "select") && !is_keyword(tokens.get(1), "distinct"))
        .ok_or_else(|| invalid("must be a SELECT, without DISTINCT, with FROM"))?;

    // The source: a table, or a query in parentheses.
    let mut position = from + 1;
    let mut tables = Vec::new();
    let mut aliases = Vec::new();
    let mut derived = None;
    let source = if matches!(tokens.get(position), Some(Token::LParen)) {
        let end = closing(tokens, position)?;
        derived = Some(&tokens[from + 2..end]);
        position = end + 1;
        alias(tokens, &mut position)
    } else {
        table(tokens, &mut position).map(|(alias, table)| {
            tables.push(table);
            aliases.push(alias.clone());
            alias
        })
    }
    .ok_or_else(|| invalid("reads a table, or a subquery with an alias"))?;

    // The level's `LATERAL` joins, which its own query leaves out.
    let mut laterals = Vec::new();
    let mut rest = Vec::new();
    let mut index = position;
    while index < tokens.len() {
        if !is_keyword(tokens.get(index + 2), "lateral") {
            if is_keyword(tokens.get(index), "lateral") {
                return Err(invalid("joins LATERAL only as LEFT JOIN LATERAL"));
            }
            rest.push(tokens[index].clone());
            index += 1;
            continue;
        }
        let open = index + 3;
        let end = match tokens.get(open) {
            Some(Token::LParen)
                if is_keyword(tokens.get(index), "left")
                    && is_keyword(tokens.get(index + 1), "join") =>
            {
                closing(tokens, open)?
            }
            _ => {
                return Err(invalid(
                    "joins LATERAL only as LEFT JOIN LATERAL (SELECT ...)",
                ));
            }
        };
        let mut next = end + 1;
        let name = alias(tokens, &mut next);
        if name.is_none()
            || !is_keyword(tokens.get(next), "on")
            || !is_keyword(tokens.get(next + 1), "true")
        {
            return Err(invalid("joins LATERAL only with an alias, ON true"));
        }
        laterals.push((name, tokens[open + 1..end].to_vec()));
        index = next + 2;
    }
    // Each table the level's own query joins.
    let mut joined = 0;
    while let Some(join) = top_level(&rest, joined, &["join"]) {
        joined = join + 1;
        let (alias, table) =
            table(&rest, &mut joined).ok_or_else(|| invalid("joins only tables or LATERAL"))?;
        tables.push(table);
        aliases.push(alias);
    }

    let mut items = Vec::new();
    let mut plain = 0;
    let mut gathering = 0;
    for item_tokens in split(&tokens[1..from]) {
        let (kind, name) = item(item_tokens, &mut laterals)?;
        plain += usize::from(matches!(kind, Item::Plain));
        gathering += usize::from(matches!(kind, Item::Gathered { .. }));
        items.push((kind, name, item_tokens));
    }
    if gathering > 0 && gathering < items.len() {
        return Err(invalid(
            "makes json_agg outputs only beside other json_agg outputs",
        ));
    }
    if derived.is_some() && (plain > 0 || !rest.is_empty()) {
        return Err(invalid(
            "reads a subquery in FROM only to make JSON of its rows",
        ));
    }
    Ok(Level {
        from,
        position,
        source,
        derived,
        tables,
        aliases,
        laterals,
        rest,
        items,
        plain,
        gathering,
    })
}

fn evaluate(
    storage: &dyn StorageReader,
    tokens: &[Token],
    params: &[Value],
    bindings: &mut Bindings,
    budget: &mut Budget,
) -> Result<Rows> {
    let tokens = tokens.strip_suffix(&[Token::Semicolon]).unwrap_or(tokens);
    let level = level(tokens)?;
    let Level {
        derived,
        laterals,
        items,
        plain,
        gathering,
        ..
    } = &level;
    // A level that nests nothing is a query the engine has.
    if derived.is_none() && laterals.is_empty() && *plain == items.len() {
        return run(storage, tokens, params, bindings, &level.aliases, budget);
    }
    let (fields, base) = base(storage, tokens, &level, params, bindings, budget)?;

    // Each output's field, once it is known, and what each json_agg gathers.
    let mut outputs = vec![None; items.len()];
    let mut gathered = vec![Vec::new(); items.len()];
    let mut rows = Vec::new();
    let outside = bindings.len();
    for (values, bound) in base {
        bindings.truncate(outside);
        bindings.extend(bound);
        let mut read = Vec::new();
        for (name, inner) in laterals {
            let Rows { fields, rows } = evaluate(storage, inner, params, bindings, budget)?;
            if name.is_none() && (fields.len() != 1 || rows.len() > 1) {
                return Err(EngineError::invalid_query(
                    "A subquery in a select list must return one column of at most one row",
                ));
            }
            let row = rows
                .into_iter()
                .next()
                .unwrap_or_else(|| vec![Value::Null; fields.len()]);
            if let Some(name) = name {
                bindings.push((name.clone(), named(&fields, &row)));
            }
            read.push((fields, row));
        }
        let mut values = values.into_iter();
        let mut row = Vec::with_capacity(items.len());
        for (index, (item, ..)) in items.iter().enumerate() {
            row.push(match item {
                Item::Plain => values.next().unwrap_or(Value::Null),
                Item::Lateral(lateral, column) => {
                    let (fields, row) = &read[*lateral];
                    let Some(at) = fields.iter().position(|field| {
                        column.as_ref().is_none_or(|column| field.name == *column)
                    }) else {
                        return Err(EngineError::column_not_found(
                            column.as_deref().unwrap_or_default(),
                            "LATERAL subquery",
                        ));
                    };
                    outputs[index] = Some(fields[at].clone());
                    row[at].clone()
                }
                Item::Each(element) => value(element, bindings)?,
                Item::Gathered { element, order, .. } => {
                    // The value, under no name, and what orders it, under their positions.
                    let mut entry = Row::new();
                    entry.insert(String::new(), value(element, bindings)?);
                    for (position, (alias, column)) in order.iter().enumerate() {
                        entry.insert(position.to_string(), lookup(bindings, alias, column)?);
                    }
                    gathered[index].push(entry);
                    continue;
                }
            });
        }
        if *gathering == 0 {
            rows.push(row);
        }
    }
    bindings.truncate(outside);
    if *gathering > 0 {
        let mut row = Vec::with_capacity(items.len());
        for ((item, ..), mut gathered) in items.iter().zip(gathered) {
            if let Item::Gathered {
                order_by,
                coalesced,
                ..
            } = item
            {
                crate::query::sort_rows_by(&mut gathered, order_by);
                row.push(if gathered.is_empty() && !coalesced {
                    Value::Null
                } else {
                    Value::Array(gathered.iter_mut().map(|row| row[""].take()).collect())
                });
            }
        }
        rows.push(row);
    }

    // Each output's field: the level's own query's, the one it reads from a subquery, or JSON.
    let mut plain_fields = fields.into_iter();
    let mut result = Vec::with_capacity(items.len());
    for ((item, name, _), output) in items.iter().zip(outputs) {
        let field = match item {
            Item::Plain => plain_fields.next(),
            _ => output,
        };
        let mut field = field.unwrap_or_else(|| ResultField::new("?column?", ColumnType::Json));
        if let Some(name) = name {
            field.name.clone_from(name);
        }
        result.push(field);
    }
    Ok(Rows {
        fields: result,
        rows,
    })
}

/// A level's rows: the outputs of its own query, and the rows of its sources, by alias, for
/// what it nests to read.
fn base(
    storage: &dyn StorageReader,
    tokens: &[Token],
    level: &Level<'_>,
    params: &[Value],
    bindings: &mut Bindings,
    budget: &mut Budget,
) -> Result<(Vec<ResultField>, Vec<BaseRow>)> {
    let mut base = Vec::new();
    if let Some(inner) = level.derived {
        let inner = evaluate(storage, inner, params, bindings, budget)?;
        for row in &inner.rows {
            base.push((
                Vec::new(),
                vec![(level.source.clone(), named(&inner.fields, row))],
            ));
        }
        return Ok((Vec::new(), base));
    }
    let mut select = vec![tokens[0].clone()];
    for (item, _, item_tokens) in &level.items {
        if matches!(item, Item::Plain) {
            select.extend_from_slice(item_tokens);
            select.push(Token::Comma);
        }
    }
    // Every column of each source, after the outputs.
    let mut counts = Vec::new();
    for (alias, table) in level.aliases.iter().zip(&level.tables) {
        select.extend([
            Token::Identifier {
                value: alias.clone(),
                quoted: true,
            },
            Token::Dot,
            Token::Star,
            Token::Comma,
        ]);
        counts.push(storage.table_schema(table)?.columns.len());
    }
    select.pop();
    select.extend_from_slice(&tokens[level.from..level.position]);
    select.extend_from_slice(&level.rest);
    let own = run(storage, &select, params, bindings, &level.aliases, budget)?;
    let mut fields = own.fields;
    let hidden = fields.split_off(level.plain);
    for mut values in own.rows {
        let mut start = 0;
        let mut bound = Vec::new();
        for (alias, count) in level.aliases.iter().zip(&counts) {
            let row = &values[level.plain + start..level.plain + start + count];
            bound.push((alias.clone(), named(&hidden[start..], row)));
            start += count;
        }
        values.truncate(level.plain);
        base.push((values, bound));
    }
    Ok((fields, base))
}

/// A row's values, each under its field's name.
fn named(fields: &[ResultField], values: &[Value]) -> Row {
    let mut row = Row::new();
    for (field, value) in fields.iter().zip(values) {
        row.insert(field.name.clone(), value.clone());
    }
    row
}

/// The names that rows key their values by: the fields' names, each prefixed with its position
/// if they repeat, as the engine keys rows that are read as arrays. And whether they repeat.
fn keys(fields: &[ResultField]) -> (Vec<String>, bool) {
    let positional = names_repeat(fields.len(), &|index| &fields[index].name);
    let mut keys = Vec::with_capacity(fields.len());
    for (index, field) in fields.iter().enumerate() {
        let mut key = field.name.clone();
        if positional {
            position_name(index, &mut key);
        }
        keys.push(key);
    }
    (keys, positional)
}

/// Runs a query the engine has, with each column it reads from the rows around it, rather than
/// from its own `sources`, bound as a parameter.
fn run(
    storage: &dyn StorageReader,
    tokens: &[Token],
    params: &[Value],
    bindings: &Bindings,
    sources: &[String],
    budget: &mut Budget,
) -> Result<Rows> {
    budget.queries += 1;
    if budget.queries > MAX_NESTED_QUERIES {
        return Err(EngineError::new(
            "QUERY_WORK_LIMIT_EXCEEDED",
            format!("A statement cannot run more than {MAX_NESTED_QUERIES} nested queries"),
        ));
    }
    storage.charge_work(1)?;
    let mut params = params.to_vec();
    let mut bound = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        if let Some((alias, column)) = reference(tokens, index)
            && !sources.contains(&alias)
            && bindings.iter().any(|(name, _)| *name == alias)
        {
            params.push(lookup(bindings, &alias, &column)?);
            bound.push(Token::Placeholder(params.len().to_string()));
            index += 3;
            continue;
        }
        bound.push(tokens[index].clone());
        index += 1;
    }
    let mut statement = crate::statement::parse_tokens(bound, &params, ParseMode::Bound)?;
    statement.position_outputs()?;
    let result = crate::statement::run_query(storage, statement)?;
    let (keys, _) = keys(&result.fields);
    let mut rows = Vec::with_capacity(result.rows.len());
    for mut row in result.rows {
        let mut values = Vec::with_capacity(keys.len());
        for key in &keys {
            let value = row.get_mut(key).map_or(Value::Null, Value::take);
            budget.bytes = budget.bytes.saturating_add(estimated_value_bytes(&value)?);
            values.push(value);
        }
        rows.push(values);
    }
    if budget.bytes > MAX_NESTED_BYTES {
        return Err(EngineError::new(
            "QUERY_WORK_LIMIT_EXCEEDED",
            format!(
                "A statement's nested queries cannot return more than {MAX_NESTED_BYTES} bytes"
            ),
        ));
    }
    Ok(Rows {
        fields: result.fields,
        rows,
    })
}
