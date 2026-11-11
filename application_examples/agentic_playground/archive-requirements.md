# Archive service requirements

This fictional internal document is sample data for the Agentic API playground.

## Retention and region

Retain archived documents for 90 days after upload, then delete them automatically.
Store documents in the Azure Southeast Asia region. Any additional replica must
remain in that region.

## Access and integrity

Use Microsoft Entra ID with a managed identity for the Python upload service.
Disable anonymous blob access. Grant the uploader only the permissions it needs
for the archive container. Record a SHA-256 checksum for each uploaded document.

## Capacity

The monthly ingest limit for this pilot is 250 GiB. Alert the team when the
next-month forecast exceeds that limit. This is an ingest limit, not a limit on
total retained storage. Monthly ingest so far is 120, 150, 180, and 210 GiB.

## Review criteria

The review should connect the workload calculation, upload example, and retention
approach to these requirements. Flag requirements that the proposed approach does
not establish, including regional placement and access controls.
