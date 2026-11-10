//! Named metrics written as SQL (a metric's `sql`), run by DataFusion over two in-memory tables of the reader's
//! own facts of the metric's `types`, as the query engine reads them for that reader: `facts`, every version they
//! may see (each `valid_to` including closures they may read), and `current`, what holds now (a newer value of a
//! one-valued predicate hiding the older). Nothing else is registered, and statements other than a query are
//! refused (no DDL, no `COPY`, no `SET`), so a metric reads those rows and nothing more. A `fact_ids` column (a
//! list of fact ids, or one) cites a row; it's resolved against those same rows, so an id the reader can't see
//! cites nothing.
//!
//! Columns of both tables: `fact_id`, `entity`, `entity_type`, `predicate`, `value` (text), `value_num`
//! (numbers and money), `value_ts` (dates, a timestamp), `value_ref` (the entity a reference names),
//! `valid_from`, `valid_to` (timestamps, UTC) and `quote`.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use datafusion::arrow::array::{
    ArrayRef, Float64Array, RecordBatch, StringArray, TimestampMicrosecondArray,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::datasource::MemTable;
use datafusion::execution::disk_manager::{DiskManagerBuilder, DiskManagerMode};
use datafusion::execution::memory_pool::FairSpillPool;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::prelude::{SQLOptions, SessionConfig, SessionContext};
use futures::StreamExt;
use serde_json::{Map, Value};

use crate::Fact;

/// The tables a metric reads.
pub const TABLES: [&str; 2] = ["facts", "current"];
/// The column that cites a row.
pub const CITES: &str = "fact_ids";
/// The most rows a metric may answer: a page shows a number or a short table.
const MAX_ROWS: usize = 10_000;

/// How long one metric may run, and how much memory it may hold while it does.
fn limits() -> (Duration, usize) {
    static L: LazyLock<(Duration, usize)> = LazyLock::new(|| {
        let n = |k: &str, d: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        (
            Duration::from_secs(n("MEMSTORE_SQL_METRIC_SECS", 30)),
            n("MEMSTORE_SQL_METRIC_MB", 512) as usize * 1024 * 1024,
        )
    });
    *L
}

fn table_schema() -> SchemaRef {
    let ts = || DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
    Arc::new(Schema::new(vec![
        Field::new("fact_id", DataType::Utf8, false),
        Field::new("entity", DataType::Utf8, false),
        Field::new("entity_type", DataType::Utf8, true),
        Field::new("predicate", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
        Field::new("value_num", DataType::Float64, true),
        Field::new("value_ts", ts(), true),
        Field::new("value_ref", DataType::Utf8, true),
        Field::new("valid_from", ts(), false),
        Field::new("valid_to", ts(), true),
        Field::new("quote", DataType::Utf8, true),
    ]))
}

/// `facts` as a table; `ty` gives each its entity's type (stored, or from its id).
fn batch(facts: &[Fact], ty: &(dyn Fn(&Fact) -> Option<String> + Sync)) -> Result<RecordBatch> {
    let s = |f: fn(&Fact) -> &str| -> ArrayRef {
        Arc::new(StringArray::from_iter_values(facts.iter().map(f)))
    };
    let ts = |v: Vec<Option<i64>>| -> ArrayRef {
        Arc::new(TimestampMicrosecondArray::from(v).with_timezone("UTC"))
    };
    let cols: Vec<ArrayRef> = vec![
        s(|f| &f.fact_id),
        s(|f| &f.entity),
        Arc::new(StringArray::from(facts.iter().map(ty).collect::<Vec<_>>())),
        s(|f| &f.predicate),
        s(|f| &f.value),
        Arc::new(Float64Array::from(
            facts.iter().map(|f| f.value_num).collect::<Vec<_>>(),
        )),
        ts(facts.iter().map(|f| f.value_ts).collect()),
        Arc::new(StringArray::from(
            facts
                .iter()
                .map(|f| f.value_ref.clone())
                .collect::<Vec<_>>(),
        )),
        ts(facts.iter().map(|f| Some(f.valid_from)).collect()),
        ts(facts.iter().map(|f| f.valid_to).collect()),
        Arc::new(StringArray::from(
            facts.iter().map(|f| f.quote.clone()).collect::<Vec<_>>(),
        )),
    ];
    Ok(RecordBatch::try_new(table_schema(), cols)?)
}

/// Functions that make one value or one call's rows as big as an argument says (`repeat('x', 2e9)`,
/// `array_resize(a, 2e9)`), out of the memory pool's sight and, with constant arguments, in a step no timeout
/// can interrupt: every function (alias included) whose name holds one of these. Table functions all go too.
const UNBOUNDED_FUNCTIONS: &[&str] = &["repeat", "resize", "pad", "range", "series", "fill"];
/// The most columns a metric may answer.
const MAX_COLUMNS: usize = 64;

/// A context holding only `facts` and `current`: its memory capped, nothing spilled to disk, no function
/// that makes a value as big as its argument.
fn context(facts: RecordBatch, current: RecordBatch) -> Result<SessionContext> {
    let (_, mem) = limits();
    let runtime = RuntimeEnvBuilder::new()
        .with_memory_pool(Arc::new(FairSpillPool::new(mem)))
        // A sort past the pool fails rather than writing the reader's facts to a temp directory.
        .with_disk_manager_builder(
            DiskManagerBuilder::default().with_mode(DiskManagerMode::Disabled),
        )
        .build_arc()?;
    let config = SessionConfig::new()
        // One partition: DataFusion spawns no tasks of its own onto the workers that serve reads.
        .with_target_partitions(1)
        .with_information_schema(false);
    let ctx = SessionContext::new_with_config_rt(config, runtime);
    let state = ctx.state();
    let unbounded = |n: &str| UNBOUNDED_FUNCTIONS.iter().any(|f| n.contains(f));
    for f in state.scalar_functions().keys().filter(|n| unbounded(n)) {
        ctx.deregister_udf(f);
    }
    for f in state.table_functions().keys() {
        ctx.deregister_udtf(f);
    }
    for (name, b) in TABLES.iter().zip([facts, current]) {
        let t = MemTable::try_new(table_schema(), vec![vec![b]])?;
        ctx.register_table(*name, Arc::new(t))?;
    }
    Ok(ctx)
}

/// Only a query: no DDL (`CREATE EXTERNAL TABLE` would read files), no DML (`COPY TO` would write them), no `SET`.
fn options() -> SQLOptions {
    SQLOptions::new()
        .with_allow_ddl(false)
        .with_allow_dml(false)
        .with_allow_statements(false)
}

/// Plan `sql` against empty tables: a mistake (a typo'd column, a statement that isn't a query) is refused
/// when the schema is set, not the first time a page asks.
pub fn check(sql: &str) -> Result<()> {
    let empty = || batch(&[], &|_| None);
    let ctx = context(empty()?, empty()?)?;
    // Planning reads nothing (the tables are in memory), so it finishes without a runtime to drive it.
    futures::executor::block_on(ctx.sql_with_options(sql, options()))
        .map_err(|e| anyhow!("sql: {e}"))?;
    Ok(())
}

/// Whether `sql` reads `facts`: when it doesn't, every visible version needn't be read to answer it, only what
/// holds now. One that doesn't parse: yes (running it says why it fails).
pub fn reads_facts(sql: &str) -> bool {
    let state = SessionContext::new().state();
    let dialect = state.config_options().sql_parser.dialect;
    state
        .sql_to_statement(sql, &dialect)
        .and_then(|st| state.resolve_table_references(&st))
        .map_or(true, |refs| refs.iter().any(|r| r.table() == TABLES[0]))
}

/// Run `sql` over `facts` (every visible version) and `current`; each row as a JSON object, in the query's
/// column order, with the column names.
pub async fn run(
    sql: &str,
    facts: &[Fact],
    current: &[Fact],
    ty: &(dyn Fn(&Fact) -> Option<String> + Sync),
) -> Result<(Vec<String>, Vec<Map<String, Value>>)> {
    // Two at a time on the whole server. Each runs on a blocking thread, so a step that never yields holds
    // that thread, not a worker serving reads; it keeps its turn until its work has really stopped, and a
    // metric that can't get a turn in time fails rather than queueing behind it.
    static RUNNING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let (secs, mem) = limits();
    let too_long = move || anyhow!("sql: ran longer than {}s", secs.as_secs());
    // The turn first: building the tables is a metric's biggest allocation.
    let turn = tokio::time::timeout(secs, RUNNING.acquire())
        .await
        .map_err(|_| too_long())??;
    let ctx = context(batch(facts, ty)?, batch(current, ty)?)?;
    let sql = sql.to_string();
    let handle = tokio::runtime::Handle::current();
    let work = async move {
        let df = ctx
            .sql_with_options(&sql, options())
            .await
            .map_err(|e| anyhow!("sql: {e}"))?;
        let fields = df.schema().fields();
        if fields.len() > MAX_COLUMNS {
            bail!("sql: answers more than {MAX_COLUMNS} columns");
        }
        // Only plain values, at any depth: an encoded one (dictionary, run-end, view) is small as Arrow and
        // large as JSON, and the budget counts Arrow first.
        if let Some(f) = fields.iter().find(|f| !plain(f.data_type())) {
            bail!(
                "sql: column {} is {}; answer text, numbers, booleans, times or lists of them",
                f.name(),
                f.data_type()
            );
        }
        let mut stream = df.execute_stream().await.map_err(|e| anyhow!("sql: {e}"))?;
        answer(&mut stream, mem / 4).await
    };
    tokio::time::timeout(
        secs,
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            handle.block_on(async {
                tokio::time::timeout(secs, work)
                    .await
                    .unwrap_or_else(|_| Err(too_long()))
            })
        }),
    )
    .await
    .map_err(|_| too_long())??
}

/// Whether a metric may answer a column of `t`: text, numbers, booleans, times, or lists of them.
fn plain(t: &DataType) -> bool {
    match t {
        DataType::List(f) | DataType::LargeList(f) => plain(f.data_type()),
        t => {
            t.is_numeric()
                || t.is_temporal()
                || matches!(
                    t,
                    DataType::Utf8 | DataType::LargeUtf8 | DataType::Boolean | DataType::Null
                )
        }
    }
}

/// The rows `stream` answers as JSON objects (every column, in order) and the column names, refused past
/// `MAX_ROWS` rows or `budget` bytes, counted as they arrive and as JSON: the pool caps what operators hold,
/// not what a query answers.
async fn answer(
    stream: &mut datafusion::physical_plan::SendableRecordBatchStream,
    budget: usize,
) -> Result<(Vec<String>, Vec<Map<String, Value>>)> {
    let columns: Vec<String> = stream
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    let too_big = || {
        anyhow!(
            "sql: answers more than {MAX_ROWS} rows or {} MB",
            budget >> 20
        )
    };
    let (mut out, mut bytes) = (vec![], 0);
    while let Some(b) = stream.next().await {
        let b = b.map_err(|e| anyhow!("sql: {e}"))?;
        bytes += b.get_array_memory_size();
        if out.len() + b.num_rows() > MAX_ROWS || bytes > budget {
            return Err(too_big());
        }
        let mut buf = Vec::new();
        let mut w = datafusion::arrow::json::ArrayWriter::new(&mut buf);
        w.write(&b)?;
        w.finish()?;
        bytes += buf.len();
        if bytes > budget {
            return Err(too_big());
        }
        if buf.is_empty() {
            continue;
        }
        let rows: Vec<Map<String, Value>> =
            serde_json::from_slice(&buf).context("sql: reading the result")?;
        // The JSON writer leaves out nulls: put them back so every row has every column.
        out.extend(rows.into_iter().map(|mut r| {
            columns
                .iter()
                .map(|c| (c.clone(), r.remove(c).unwrap_or(Value::Null)))
                .collect::<Map<_, _>>()
        }));
    }
    Ok((columns, out))
}

/// A row's `fact_ids` (a list, or one id), taken out of it.
pub fn take_cites(row: &mut Map<String, Value>) -> Result<Vec<String>> {
    match row.remove(CITES) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => Ok(vec![s]),
        Some(Value::Array(a)) => Ok(a
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()),
        Some(_) => bail!("sql: {CITES} is a list of fact ids"),
    }
}
