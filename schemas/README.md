# Vendored schemas

`render-blueprint.schema.json` is a validation-equivalent snapshot of Render's official
[`render.yaml` schema](https://render.com/schema/render.yaml.json), fetched on 2026-07-23. Descriptive-only fields were
removed to keep the repository copy compact; validation keywords and constraints are unchanged.

To update it, download the official schema, review the upstream diff, remove the `description`, `title`, and
`deprecated` annotations, and run:

```console
SKIP=no-commit-to-branch pre-commit run check-jsonschema --all-files
```
