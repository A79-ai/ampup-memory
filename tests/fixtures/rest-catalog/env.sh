# Settings for tests/it/rest_catalog.rs on the fixture in this directory: `source tests/fixtures/rest-catalog/env.sh`.
# Without AMPUP_MEMORY_TEST_REST_URI those tests skip. The others default to these values.
export AMPUP_MEMORY_TEST_REST_URI=http://localhost:18181
export AMPUP_MEMORY_TEST_REST_WAREHOUSE=s3://memstore/warehouse
export AMPUP_MEMORY_TEST_S3_ENDPOINT=http://localhost:19000
export AMPUP_MEMORY_TEST_S3_BUCKET=memstore
export AMPUP_MEMORY_TEST_S3_KEY=admin
export AMPUP_MEMORY_TEST_S3_SECRET=password
