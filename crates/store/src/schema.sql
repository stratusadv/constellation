-- Constellation SQLite schema.
--
-- One database holds every project's graph plus the cross-project edges that
-- connect them. project_id partitions the per-project graphs; an edge whose
-- source and target belong to different projects is a cross-project link.

CREATE TABLE IF NOT EXISTS projects (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    root_path     TEXT NOT NULL,
    indexed_at    INTEGER NOT NULL,
    -- The binary fingerprint that last fully indexed this project. A mismatch
    -- with the running binary forces a full re-extraction (the per-file
    -- content-hash skip is bypassed), so an extractor change lands without
    -- deleting the database. Empty until the first successful index.
    index_version TEXT NOT NULL DEFAULT '',
    -- A reference-only project is a full, queryable project whose nodes are
    -- excluded from cross-project link targets, so two indexed versions of one
    -- library never compete to win an ambiguous import. 0 = canonical.
    reference_only INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS files (
    path         TEXT NOT NULL,
    project_id   TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    language     TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    modified_at  INTEGER NOT NULL,
    indexed_at   INTEGER NOT NULL,
    node_count   INTEGER NOT NULL DEFAULT 0,
    errors       TEXT,
    PRIMARY KEY (project_id, path),
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS nodes (
    id             TEXT PRIMARY KEY,
    project_id     TEXT NOT NULL,
    kind           TEXT NOT NULL,
    name           TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    file_path      TEXT NOT NULL,
    language       TEXT NOT NULL,
    start_line     INTEGER NOT NULL,
    end_line       INTEGER NOT NULL,
    start_column   INTEGER NOT NULL,
    end_column     INTEGER NOT NULL,
    docstring      TEXT,
    signature      TEXT,
    visibility     TEXT,
    is_exported    INTEGER NOT NULL DEFAULT 0,
    is_async       INTEGER NOT NULL DEFAULT 0,
    is_static      INTEGER NOT NULL DEFAULT 0,
    is_abstract    INTEGER NOT NULL DEFAULT 0,
    decorators     TEXT,
    updated_at     INTEGER NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS edges (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    source     TEXT NOT NULL,
    target     TEXT NOT NULL,
    kind       TEXT NOT NULL,
    line       INTEGER,
    column     INTEGER,
    provenance TEXT,
    FOREIGN KEY (source) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY (target) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS unresolved_refs (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id     TEXT NOT NULL,
    from_node_id   TEXT NOT NULL,
    reference_name TEXT NOT NULL,
    reference_kind TEXT NOT NULL,
    line           INTEGER NOT NULL,
    column         INTEGER NOT NULL,
    file_path      TEXT NOT NULL,
    language       TEXT NOT NULL,
    candidates     TEXT,
    FOREIGN KEY (from_node_id) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_nodes_project    ON nodes(project_id);
CREATE INDEX IF NOT EXISTS idx_nodes_kind       ON nodes(kind);
CREATE INDEX IF NOT EXISTS idx_nodes_name       ON nodes(name);
CREATE INDEX IF NOT EXISTS idx_nodes_qualified  ON nodes(qualified_name);
CREATE INDEX IF NOT EXISTS idx_nodes_file       ON nodes(project_id, file_path);
CREATE INDEX IF NOT EXISTS idx_nodes_lower_name ON nodes(lower(name));

CREATE INDEX IF NOT EXISTS idx_edges_kind        ON edges(kind);
CREATE INDEX IF NOT EXISTS idx_edges_source_kind ON edges(source, kind);
CREATE INDEX IF NOT EXISTS idx_edges_target_kind ON edges(target, kind);
CREATE INDEX IF NOT EXISTS idx_edges_provenance  ON edges(provenance);

CREATE INDEX IF NOT EXISTS idx_files_project     ON files(project_id);
CREATE INDEX IF NOT EXISTS idx_unresolved_from   ON unresolved_refs(from_node_id);
CREATE INDEX IF NOT EXISTS idx_unresolved_name   ON unresolved_refs(reference_name);

-- Full-text search over node names, qualified names, docstrings, signatures.
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    id,
    name,
    qualified_name,
    docstring,
    signature,
    content='nodes',
    content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS nodes_ai AFTER INSERT ON nodes BEGIN
    INSERT INTO nodes_fts(rowid, id, name, qualified_name, docstring, signature)
    VALUES (NEW.rowid, NEW.id, NEW.name, NEW.qualified_name, NEW.docstring, NEW.signature);
END;

CREATE TRIGGER IF NOT EXISTS nodes_ad AFTER DELETE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, id, name, qualified_name, docstring, signature)
    VALUES ('delete', OLD.rowid, OLD.id, OLD.name, OLD.qualified_name, OLD.docstring, OLD.signature);
END;

CREATE TRIGGER IF NOT EXISTS nodes_au AFTER UPDATE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, id, name, qualified_name, docstring, signature)
    VALUES ('delete', OLD.rowid, OLD.id, OLD.name, OLD.qualified_name, OLD.docstring, OLD.signature);
    INSERT INTO nodes_fts(rowid, id, name, qualified_name, docstring, signature)
    VALUES (NEW.rowid, NEW.id, NEW.name, NEW.qualified_name, NEW.docstring, NEW.signature);
END;

CREATE TABLE IF NOT EXISTS project_metadata (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Per-file import bindings (local name -> exported name + module), used to
-- resolve aliased imports and the calls that reference them.
CREATE TABLE IF NOT EXISTS import_mappings (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id    TEXT NOT NULL,
    file_path     TEXT NOT NULL,
    local_name    TEXT NOT NULL,
    exported_name TEXT NOT NULL,
    source        TEXT NOT NULL,
    is_default    INTEGER NOT NULL DEFAULT 0,
    is_namespace  INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_import_mappings_file ON import_mappings(project_id, file_path);

-- Per-file event-channel observations (dispatch sites and listener
-- registrations) from JS/Alpine, correlated by event name to synthesize
-- dispatcher -> handler edges.
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  TEXT NOT NULL,
    file_path   TEXT NOT NULL,
    role        TEXT NOT NULL,
    event_name  TEXT NOT NULL,
    symbol      TEXT NOT NULL,
    line        INTEGER NOT NULL,
    column      INTEGER NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_events_project ON events(project_id);
CREATE INDEX IF NOT EXISTS idx_events_file ON events(project_id, file_path);
CREATE INDEX IF NOT EXISTS idx_events_name ON events(project_id, event_name);

-- Full-text index over each file's source content, so explore can seed its
-- structural ranking from files whose body matches a query (a name or
-- docstring search alone misses a symbol found only by an identifier in its
-- body). Porter stemming so "numbers" matches "number"; an external-content
-- FTS kept in sync by triggers, mirroring nodes/nodes_fts, so per-file delete
-- on re-index is reliable.
CREATE TABLE IF NOT EXISTS file_content (
    project_id TEXT NOT NULL,
    file_path  TEXT NOT NULL,
    content    TEXT NOT NULL,
    PRIMARY KEY (project_id, file_path),
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);
CREATE VIRTUAL TABLE IF NOT EXISTS file_content_fts USING fts5(
    content,
    content='file_content',
    content_rowid='rowid',
    tokenize='porter unicode61'
);
CREATE TRIGGER IF NOT EXISTS file_content_ai AFTER INSERT ON file_content BEGIN
    INSERT INTO file_content_fts(rowid, content) VALUES (NEW.rowid, NEW.content);
END;
CREATE TRIGGER IF NOT EXISTS file_content_ad AFTER DELETE ON file_content BEGIN
    INSERT INTO file_content_fts(file_content_fts, rowid, content) VALUES ('delete', OLD.rowid, OLD.content);
END;
CREATE TRIGGER IF NOT EXISTS file_content_au AFTER UPDATE ON file_content BEGIN
    INSERT INTO file_content_fts(file_content_fts, rowid, content) VALUES ('delete', OLD.rowid, OLD.content);
    INSERT INTO file_content_fts(rowid, content) VALUES (NEW.rowid, NEW.content);
END;
