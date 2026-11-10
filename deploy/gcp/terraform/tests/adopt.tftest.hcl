# Plans the module offline against a mock provider (nothing is read or created): with the customer's own bucket and
# dataset (deploy/iam's outputs) it makes neither, and checks both. Run: terraform init -backend=false && terraform test

mock_provider "google" {}

variables {
  project = "example-project"
  image   = "us-docker.pkg.dev/vendor/memstore/engine:1.0.0"
}

run "a_public_customer_bucket_is_refused" {
  command = plan

  variables {
    existing_bucket  = "acme-memstore-memory"
    existing_dataset = "acme_api"
  }

  override_data {
    target = data.google_storage_bucket.existing[0]
    values = { name = "acme-memstore-memory", uniform_bucket_level_access = true, public_access_prevention = "inherited" }
  }
  override_data {
    target = data.google_bigquery_dataset.existing[0]
    values = { dataset_id = "acme_api", location = "us-central1" }
  }

  expect_failures = [data.google_storage_bucket.existing[0]]
}

run "a_customer_dataset_in_another_region_is_refused" {
  command = plan

  variables {
    existing_bucket  = "acme-memstore-memory"
    existing_dataset = "acme_api"
  }

  override_data {
    target = data.google_storage_bucket.existing[0]
    values = { name = "acme-memstore-memory", uniform_bucket_level_access = true, public_access_prevention = "enforced" }
  }
  override_data {
    target = data.google_bigquery_dataset.existing[0]
    values = { dataset_id = "acme_api", location = "EU" }
  }

  expect_failures = [data.google_bigquery_dataset.existing[0]]
}

# --- with the IAM template's accounts too: the vendor role alone ------------------------------------------------
