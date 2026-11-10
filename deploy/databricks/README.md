# The memory on Databricks (Unity Catalog)

The engine talks to Unity Catalog's Iceberg REST endpoint (`MEMSTORE_PLATFORM=databricks`), so the
memory's tables are managed Iceberg tables any Databricks engine (SQL, notebooks, Genie) can read.

## Workspace requirements

- A catalog on **customer-owned storage** (an external location on your bucket). Catalogs on Databricks
  default storage (e.g. Free Edition, serverless defaults) get no credential vending, so an external engine
  can't write their files.
- **External data access** enabled on the metastore.
- Predictive optimization enabled (required to create managed Iceberg tables).
- A service principal with `USE CATALOG`, `USE SCHEMA`, `CREATE SCHEMA`, `CREATE TABLE`, `MODIFY`, `SELECT`
  and `EXTERNAL USE SCHEMA` on the memory's catalog.

```sql
CREATE CATALOG memory MANAGED LOCATION 'gs://<bucket>/memory';   -- or s3:// / abfss://
GRANT USE CATALOG, CREATE SCHEMA, USE SCHEMA, CREATE TABLE, MODIFY, SELECT, EXTERNAL USE SCHEMA
  ON CATALOG memory TO `<service-principal-application-id>`;
```

## Run

```bash
MEMSTORE_PLATFORM=databricks DATABRICKS_HOST=<workspace-host> DATABRICKS_CATALOG=memory \
  DATABRICKS_CLIENT_ID=<id> DATABRICKS_CLIENT_SECRET_FILE=<file> memstore smoke org
```

`DATABRICKS_TOKEN` (or `_FILE`) takes a PAT instead of a service principal. Any `<VAR>` can be given as
`<VAR>_FILE` (a mounted secret).

## Not verified yet

Run against a workspace before relying on it: that Unity Catalog lets the engine set
`commit.retry.num-retries` on the ledger, that vended table credentials cover `bodies/` under the
episodes table, and the SQL surface (`ai_query` for answers; Databricks SQL can't call Cloud Run-style
remote functions, so search from SQL needs an App or a UC HTTP connection).
