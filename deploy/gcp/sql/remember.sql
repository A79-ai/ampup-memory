-- Queue text for the next ingest (scheduled, or CALL-free: the memstore-ingest job).
--   CALL memstore_api.remember('slack:#eng', 'We decided ...', NULL, ['group:eng']);
CREATE OR REPLACE PROCEDURE `${PROJECT}.${DATASET}.remember`(
  source STRING, body STRING, occurred_at TIMESTAMP, acl ARRAY<STRING>)
BEGIN
  INSERT `${PROJECT}.${DATASET}.inbox` (source, body, occurred_at, acl)
  VALUES (source, body, IFNULL(occurred_at, CURRENT_TIMESTAMP()), acl);
  SELECT 'queued' AS status;
END
