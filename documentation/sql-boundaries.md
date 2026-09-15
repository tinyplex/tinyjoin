# SQL maintenance boundaries

The SQL dialect is deliberately small. Shared machinery belongs at a boundary
where the same contract applies; grammar and execution rules stay with their
statement family.

## Parsing map

| Layer | Owner | Responsibility |
| --- | --- | --- |
| Script boundaries | `sql_script.rs` | Bound text, lexical regions and statement count; preserve original SQL for diagnostics |
| Tokens and values | `query.rs` | Lexer, bounded SQL and JSON inputs, parameter expansion accounting, parameter markers, common predicates |
| Statement dispatch | `statement::parse_tokens` | One expansion check; classify SELECT, aggregate, join or mutation and transfer one token vector |
| Prepared layout | `prepared_statement.rs` | Tokenize once, inspect placeholder layout, create internal markers, transfer tokens to the same dispatch; bind the retained plan on execution |
| Family grammar | `query.rs`, `aggregate.rs`, `join.rs`, `statement.rs` | Consume tokens into the family plan |
| Catalog and type checks | Family executors plus `storage.rs` | Validate against the current catalog before reads/writes, including empty inputs and prepared reuse |

Before this refactor, direct SELECT dispatch lexed once to classify and again
inside the selected family. Preparation lexed for the placeholder layout, again
for dispatch, and again for SELECT parsing. Each family also repeated input and
parameter expansion validation. Runtime paths now tokenize exactly once, and
share the expansion check immediately before dispatch. Prepared input bounds
still precede lexer allocation; generated internal marker values are validated
before plan creation. The test-only family entry points retain their input checks
so unit tests can exercise a specific grammar independently.

## Intentionally local

- Identifier consumption, ordering, projection, and pagination syntax remain in
  each family parser. Joins resolve qualified references and output aliases;
  aggregate order refers to grouped/projected values; mutation `RETURNING` has
  different shape and work budgets. A general cursor/AST rewrite would hide
  these differences and is not required to remove repeated tokenization.
- Bind walkers retain their family plans. They already share predicate/value and
  nonnegative pagination binding in `query.rs`; DML also preserves `DEFAULT`.
- Type/catalog validators share named-column and predicate checks where their
  contracts agree. Join relation resolution, aggregate legality, and write-set
  constraints remain local. Validation cannot move entirely into preparation:
  schema and bound parameter values can change before execution.
- The script splitter scans lexical regions separately because it must preserve
  original statements and enforce a script-wide budget before publication.

No new grammar, dependency, public API, query strategy or storage representation
is introduced. Run native semantic, prepared, script and recovery tests, followed
by the WASM contract and packed-browser gates. Compare the built raw/gzip WASM and
whole runtime payload as well as behavior; reduced passes are not a claim of a
specific timing improvement.

## Payload observation

The launch build measured WASM at 737,145 bytes raw / 277,336 bytes gzip (level 9),
versus committed pre-refactor metadata of 738,352 / 277,465: a reduction of 1,207
raw and 129 gzip bytes. The complete default runtime was 792,039 / 297,883 bytes.
The baseline was retained build metadata, not independently rebuilt during this
change. The measured WASM SHA-256 is
`c4a5996edb590e4a44a1b2509e6835cab8a83114a61bf7171ea2346336935f2b`.
Native corpus/reducer modules are test-only and are not included in that payload.
