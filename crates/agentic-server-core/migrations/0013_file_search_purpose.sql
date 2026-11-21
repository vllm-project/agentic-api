-- Files are listed by purpose, so the filter has its own portable column. The
-- backfill matches the compact JSON written for every uploaded FileObject; the
-- purpose values are a fixed enumerated set, so a portable LIKE is sufficient.
ALTER TABLE file_search_files ADD COLUMN purpose TEXT NOT NULL DEFAULT '';

UPDATE file_search_files SET purpose = 'fine-tune' WHERE data LIKE '%"purpose":"fine-tune"%';
UPDATE file_search_files SET purpose = 'assistants' WHERE data LIKE '%"purpose":"assistants"%';
UPDATE file_search_files SET purpose = 'batch' WHERE data LIKE '%"purpose":"batch"%';
UPDATE file_search_files SET purpose = 'user_data' WHERE data LIKE '%"purpose":"user_data"%';
UPDATE file_search_files SET purpose = 'vision' WHERE data LIKE '%"purpose":"vision"%';
UPDATE file_search_files SET purpose = 'evals' WHERE data LIKE '%"purpose":"evals"%';
