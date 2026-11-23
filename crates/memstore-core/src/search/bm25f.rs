//! A fact row's keyword score from its own text, as the facts index's any-terms keyword query scores it: Lance's
//! combined-fields BM25F ([`CombinedFieldsQuery`]) over the keyword fields ([`super::FTS_FIELDS`]). A search scores the
//! candidates only its vector lists found this way, from the rows held in memory, rather than asking the index for
//! its whole any-terms list (every posting scored and every match returned) or for a second keyword query.
//!
//! The scores are Lance's bit for bit: the statistics come from the index itself
//! ([`build_combined_bm25_scorer`], the corpus-wide document counts, frequencies and average length every match of
//! the query is scored against, whatever its filter), the text is tokenized by the index's own tokenizer, and the
//! sums run in the order Lance runs them (each term's frequency and the row's length summed over the columns in the
//! query's order, the terms in the order the query names them first). A part the index doesn't cover whole has no
//! scorer: Lance blends the uncovered rows' statistics into its scores there, and the caller asks the index instead.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use lance::Dataset;
use lance::index::{DatasetIndexExt, DatasetIndexInternalExt};
use lance_index::scalar::inverted::query::{CombinedFieldsQuery, collect_query_tokens};
use lance_index::scalar::inverted::tokenizer::document_tokenizer::LanceTokenizer;
use lance_index::scalar::inverted::{
    CombinedCorpusStats, CombinedFieldColumn, CombinedFieldsBM25Scorer, InvertedIndex,
    build_combined_bm25_scorer,
};
use lance_tokenizer::TokenStream;

use super::FTS_FIELDS;

/// The any-terms keyword query's scorer for one index part.
pub(crate) struct KeywordScorer {
    scorer: CombinedFieldsBM25Scorer,
    /// The query's distinct tokens, in the order it names them first, each with its IDF.
    terms: Vec<(String, f32)>,
    /// Each of [`Self::terms`]' place in it, by its text.
    places: HashMap<String, usize>,
    /// The query's columns in its order: each one's slot in [`super::FTS_FIELDS`] and its weight.
    columns: Vec<(usize, f32)>,
    tokenizer: Box<dyn LanceTokenizer>,
}

impl KeywordScorer {
    /// The scorer of `query` over part `ds`; None when some file of the part is not in every keyword column's
    /// index (Lance then scores against statistics blended with those rows').
    pub(crate) async fn of(ds: &Dataset, query: &CombinedFieldsQuery) -> Result<Option<Self>> {
        let fragments: Vec<u32> = ds.fragments().iter().map(|f| f.id as u32).collect();
        let indices = ds.load_indices().await?;
        let mut columns = vec![];
        let mut slots = vec![];
        for (name, weight) in query.weighted_columns() {
            let Some(slot) = FTS_FIELDS.iter().position(|f| f.0 == name) else {
                return Ok(None);
            };
            let Some(field) = ds.schema().field(name) else {
                return Ok(None);
            };
            let mut segments = vec![];
            let mut covered = roaring::RoaringBitmap::new();
            for meta in indices.iter().filter(|m| m.fields == [field.id]) {
                let index = ds
                    .open_generic_index(
                        name,
                        &meta.uuid,
                        &lance_index::metrics::NoOpMetricsCollector,
                    )
                    .await?;
                let Some(inverted) = index.as_any().downcast_ref::<InvertedIndex>() else {
                    continue;
                };
                let Some(bitmap) = &meta.fragment_bitmap else {
                    return Ok(None);
                };
                covered |= bitmap;
                segments.push(Arc::new(inverted.clone()));
            }
            if segments.is_empty() || fragments.iter().any(|f| !covered.contains(*f)) {
                return Ok(None);
            }
            columns.push(CombinedFieldColumn {
                column: name.to_string(),
                weight,
                indices: segments,
                stale_rows: None,
            });
            slots.push((slot, weight));
        }
        let Some(first) = columns.iter().find_map(|c| c.indices.first()) else {
            return Ok(None);
        };
        let mut tokenizer = first.tokenizer();
        let tokens = collect_query_tokens(query.terms(), &mut tokenizer);
        let scorer =
            build_combined_bm25_scorer(&columns, &tokens, CombinedCorpusStats::IndexOnly, None)
                .await?;
        let mut seen = HashSet::new();
        let terms: Vec<(String, f32)> = (&tokens)
            .into_iter()
            .filter(|t| seen.insert(t.as_str()))
            .map(|t| (t.clone(), scorer.query_weight(t)))
            .collect();
        let places = terms
            .iter()
            .enumerate()
            .map(|(i, (t, _))| (t.clone(), i))
            .collect();
        Ok(Some(Self {
            scorer,
            terms,
            places,
            columns: slots,
            tokenizer,
        }))
    }

    /// The score of a row whose keyword fields read `texts` ([`super::fts_texts`]); None when it holds none of the
    /// query's terms (the query doesn't match it).
    pub(crate) fn score(&mut self, texts: &[String; 5]) -> Option<f32> {
        // Per column, its length in tokens and each term's count in it.
        let mut counts = vec![(0u32, vec![0u32; self.terms.len()]); self.columns.len()];
        for ((slot, _), (len, tf)) in self.columns.iter().zip(counts.iter_mut()) {
            let mut stream = self.tokenizer.token_stream_for_doc(&texts[*slot]);
            while stream.advance() {
                *len += 1;
                if let Some(i) = self.places.get(&stream.token().text) {
                    tf[*i] += 1;
                }
            }
        }
        // dl' and tf' summed over the columns in the query's order, a column without the term adding nothing.
        let dl: f32 = self
            .columns
            .iter()
            .zip(&counts)
            .map(|((_, w), (len, _))| w * *len as f32)
            .sum();
        let mut matched = false;
        let mut score = 0.0f32;
        for (i, (_, idf)) in self.terms.iter().enumerate() {
            let mut tf: Option<f32> = None;
            for ((_, w), (_, counts)) in self.columns.iter().zip(&counts) {
                if counts[i] > 0 {
                    *tf.get_or_insert(0.0) += w * counts[i] as f32;
                }
            }
            let Some(tf) = tf.filter(|tf| *tf > 0.0) else {
                continue;
            };
            matched = true;
            score += idf * self.scorer.doc_weight(tf, dl);
        }
        matched.then_some(score)
    }
}
