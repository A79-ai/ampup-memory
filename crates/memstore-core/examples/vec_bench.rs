//! Vector index benchmark on a copy of a real index part's embeddings: latency and recall against the exact top k.
//!
//!   vec_bench copy SRC DST          copy SRC's `vec` column (e.g. gs://…/open.lance) to a local dataset DST
//!   vec_bench build DST KIND        index DST's vectors: flat | pq<M> (IVF_PQ, M sub-vectors) | rq (IVF_RQ)
//!   vec_bench run DST Q LABEL NPROBES REFINE [K]
//!                                   search with each query in Q (JSON [{"vec": [...]}], plus 200 of DST's own
//!                                   rows); LABEL `truth` saves the hits, any other compares with them
use std::time::Instant;

use anyhow::Result;
use arrow_array::{Array, Float32Array, RecordBatch, RecordBatchIterator, cast::AsArray};
use futures::TryStreamExt;
use lance::dataset::{Dataset, WriteMode, WriteParams};
use lance::index::DatasetIndexExt;
use lance::index::vector::VectorIndexParams;
use lance_index::IndexType;
use lance_linalg::distance::MetricType;

#[tokio::main]
async fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "copy" => copy(&a[2], &a[3]).await,
        "build" => build(&a[2], &a[3]).await,
        "info" => {
            let ds = Dataset::open(&a[2]).await?;
            let kind = match ds.index_statistics("vec").await {
                Ok(s) => serde_json::from_str::<serde_json::Value>(&s)?["index_type"].to_string(),
                Err(_) => "none".into(),
            };
            println!(
                "{}: {} rows, vector index {kind}",
                a[2],
                ds.count_rows(None).await?
            );
            Ok(())
        }
        "run" => {
            let k = a.get(7).map_or(300, |k| k.parse().unwrap());
            let ef = a.get(8).map(|e| e.parse().unwrap());
            run(&a[2], &a[3], &a[4], a[5].parse()?, a[6].parse()?, k, ef).await
        }
        _ => anyhow::bail!("copy | build | run"),
    }
}

async fn copy(src: &str, dst: &str) -> Result<()> {
    let ds = Dataset::open(src).await?;
    println!("{src}: {} rows", ds.count_rows(None).await?);
    let mut scan = ds.scan();
    scan.project(&["vec"])?;
    let mut stream = scan.try_into_stream().await?;
    let (mut held, mut rows, mut first) = (vec![], 0usize, true);
    let t = Instant::now();
    loop {
        let b = stream.try_next().await?;
        if let Some(b) = &b {
            rows += b.num_rows();
            held.push(b.clone());
        }
        if held.len() >= 64 || (b.is_none() && !held.is_empty()) {
            let schema = held[0].schema();
            let reader =
                RecordBatchIterator::new(std::mem::take(&mut held).into_iter().map(Ok), schema);
            let mode = if first {
                WriteMode::Create
            } else {
                WriteMode::Append
            };
            Dataset::write(
                reader,
                dst,
                Some(WriteParams {
                    mode,
                    ..Default::default()
                }),
            )
            .await?;
            first = false;
            println!("  {rows} rows, {:.0} s", t.elapsed().as_secs_f64());
        }
        if b.is_none() {
            break;
        }
    }
    Ok(())
}

async fn build(dst: &str, kind: &str) -> Result<()> {
    let mut ds = Dataset::open(dst).await?;
    let n = ds.count_rows(None).await?;
    let parts = (n as f64).sqrt() as usize;
    use lance_index::vector::{
        hnsw::builder::HnswBuildParams, ivf::IvfBuildParams, sq::builder::SQBuildParams,
    };
    // `<kind>/<partitions>` overrides sqrt(rows).
    let (kind, parts) = match kind.split_once('/') {
        Some((k, p)) => (k, p.parse()?),
        None => (kind, parts),
    };
    let params = match kind {
        "flat" => VectorIndexParams::ivf_flat(parts, MetricType::L2),
        "rq" => VectorIndexParams::ivf_rq(parts, 1, MetricType::L2),
        "sq" => VectorIndexParams::with_ivf_sq_params(
            MetricType::L2,
            IvfBuildParams::new(parts),
            SQBuildParams::default(),
        ),
        "hnswsq" => VectorIndexParams::with_ivf_hnsw_sq_params(
            MetricType::L2,
            IvfBuildParams::new(parts),
            HnswBuildParams::default(),
            SQBuildParams::default(),
        ),
        k if k.starts_with("pq") => {
            VectorIndexParams::ivf_pq(parts, 8, k[2..].parse()?, MetricType::L2, 50)
        }
        _ => anyhow::bail!("flat | pq<M> | rq"),
    };
    let t = Instant::now();
    ds.create_index(
        &["vec"],
        IndexType::Vector,
        Some("vec".into()),
        &params,
        true,
    )
    .await?;
    println!(
        "{kind}: {n} rows, {parts} partitions, built in {:.0} s",
        t.elapsed().as_secs_f64()
    );
    let stats: serde_json::Value = serde_json::from_str(&ds.index_statistics("vec").await?)?;
    println!("  index_type {}", stats["index_type"]);
    Ok(())
}

async fn queries(ds: &Dataset, path: &str) -> Result<Vec<Float32Array>> {
    let given: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let mut qs: Vec<Float32Array> = given
        .iter()
        .map(|q| {
            Float32Array::from(
                q["vec"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_f64().unwrap() as f32)
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    // 200 of the part's own rows, spread evenly: facts as queries, beside the real asks.
    let n = ds.count_rows(None).await? as u64;
    let picks: Vec<u64> = (0..200).map(|i| i * n / 200 + 7).collect();
    let b = ds.take(&picks, ds.schema().project(&["vec"])?).await?;
    let col = b.column(0).as_fixed_size_list();
    for i in 0..col.len() {
        qs.push(
            col.value(i)
                .as_primitive::<arrow_array::types::Float32Type>()
                .clone(),
        );
    }
    Ok(qs)
}

async fn run(
    dst: &str,
    qpath: &str,
    label: &str,
    nprobes: usize,
    refine: u32,
    k: usize,
    ef: Option<usize>,
) -> Result<()> {
    let ds = Dataset::open(dst).await?;
    let qs = queries(&ds, qpath).await?;
    let real = qs.len() - 200;
    let truth_path = std::env::var("VB_TRUTH").unwrap_or_else(|_| format!("{dst}.truth{k}.json"));
    let mut hits: Vec<Vec<u64>> = vec![];
    let (mut cold, mut warm) = (vec![], vec![]);
    for pass in 0..2 {
        for q in &qs {
            let mut scan = ds.scan();
            scan.project::<&str>(&[])?.with_row_id();
            scan.nearest("vec", q, k)?.nprobes(nprobes);
            if refine > 0 {
                scan.refine(refine);
            }
            if let Some(ef) = ef {
                scan.ef(ef);
            }
            let t = Instant::now();
            let batches: Vec<RecordBatch> = scan.try_into_stream().await?.try_collect().await?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let ids: Vec<u64> = batches
                .iter()
                .flat_map(|b| {
                    b.column_by_name("_rowid")
                        .unwrap()
                        .as_primitive::<arrow_array::types::UInt64Type>()
                        .values()
                        .to_vec()
                })
                .collect();
            if pass == 0 {
                cold.push(ms);
                hits.push(ids);
            } else {
                warm.push(ms);
            }
        }
    }
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[((v.len() - 1) as f64 * p) as usize]
    };
    let mut line = format!("first query {:.0} ms; {label:8}", cold[0],);
    line += &format!(
        " nprobes {nprobes:4} refine {refine:2} ef {ef:?} k {k}: first pass p50 {:7.1} p95 {:7.1} ms; again p50 {:7.1} p95 {:7.1} ms",
        pct(&mut cold.clone(), 0.5),
        pct(&mut cold, 0.95),
        pct(&mut warm.clone(), 0.5),
        pct(&mut warm, 0.95)
    );
    if label == "truth" {
        std::fs::write(&truth_path, serde_json::to_string(&hits)?)?;
    } else {
        let truth: Vec<Vec<u64>> = serde_json::from_str(&std::fs::read_to_string(&truth_path)?)?;
        let recall = |range: std::ops::Range<usize>, at: usize| {
            let r: Vec<f64> = range
                .map(|i| {
                    let want: std::collections::HashSet<_> = truth[i].iter().take(at).collect();
                    let got = hits[i].iter().take(at).filter(|h| want.contains(h)).count();
                    got as f64 / want.len().max(1) as f64
                })
                .collect();
            r.iter().sum::<f64>() / r.len() as f64
        };
        line += &format!(
            "; recall@15 asks {:.3} facts {:.3}; recall@{k} asks {:.3} facts {:.3}",
            recall(0..real, 15),
            recall(real..qs.len(), 15),
            recall(0..real, k),
            recall(real..qs.len(), k)
        );
    }
    println!("{line}");
    Ok(())
}
