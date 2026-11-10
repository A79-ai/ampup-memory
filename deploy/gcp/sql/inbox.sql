-- Text waiting to be remembered. Each row carries who may see it (ACL entries: *, user:<id>, group:<id>).
CREATE TABLE IF NOT EXISTS `${PROJECT}.${DATASET}.inbox` (
  source STRING, body STRING, occurred_at TIMESTAMP,
  added_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP(), acl ARRAY<STRING>)
