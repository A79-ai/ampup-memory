-- Answer a question from the memory with Gemini. With use_sources (pass TRUE; FALSE = facts only, cheaper) the
-- answer also reads excerpts of the episodes the top facts cite: up to 5, ~4k characters each, centred on each
-- fact's quote. Mirrors the Snowflake app's core.ask.
--   CALL memstore_api.ask('Who owns the export button?', TRUE);
CREATE OR REPLACE PROCEDURE `${PROJECT}.${DATASET}.ask`(q STRING, use_sources BOOL)
BEGIN
  DECLARE facts JSON;
  DECLARE sources STRING DEFAULT '';
  SET facts = (SELECT `${PROJECT}.${DATASET}.search`(q, 15));
  IF use_sources THEN
    SET sources = (
      WITH cited AS (
        SELECT JSON_VALUE(e) AS eid, MIN(o) AS rnk,
               ARRAY_AGG(JSON_VALUE(h.quote) IGNORE NULLS ORDER BY o LIMIT 1)[SAFE_OFFSET(0)] AS quote
        FROM UNNEST(JSON_QUERY_ARRAY(facts)) AS h WITH OFFSET o, UNNEST(JSON_QUERY_ARRAY(h.source_episodes)) AS e
        GROUP BY eid ORDER BY rnk LIMIT 5),
      excerpts AS (SELECT eid, rnk, `${PROJECT}.${DATASET}.episode`(eid, quote, 4000) AS txt FROM cited)
      SELECT IFNULL(STRING_AGG(CONCAT('[episode ', LEFT(eid, 12), ']\n', txt), '\n\n' ORDER BY rnk), '')
      FROM excerpts);
  END IF;
  SELECT AI.GENERATE(CONCAT(
      'Answer the question from the team memory below: facts (JSON, best match first) and, when present, ',
      'excerpts of the source episodes the facts cite. Use the excerpts for detail and reasons; cite them as ',
      '[episode <id>]. Be concise and name the people, features and dates involved. If the memory does not ',
      'answer it, say "unknown".\n\nFacts: ', TO_JSON_STRING(facts),
      IF(sources = '', '', CONCAT('\n\nSource excerpts:\n', sources)), '\n\nQuestion: ', q),
    connection_id => '${CONNECTION}', endpoint => '${MODEL}').result AS answer;
END
