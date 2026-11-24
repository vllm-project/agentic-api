-- A conversation turn appends to whatever the conversation holds when it is stored, as OpenAI does, so the
-- conversation `revision` that rejected stale turns is removed.
ALTER TABLE conversations DROP COLUMN revision;
