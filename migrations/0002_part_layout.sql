-- storlite migration 0002: persist the part layout of assembled multipart
-- objects so GetObject/HeadObject can serve `partNumber` reads (needed by the
-- Ruby SDK's default download_file mode). NULL for single-part objects.
ALTER TABLE blobs ADD COLUMN part_sizes_json TEXT
    CHECK(part_sizes_json IS NULL OR json_valid(part_sizes_json));
