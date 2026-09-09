-- ============================================================================
-- ElectronIx Trace -- migration 001: schema, configuration tables, RLS.
--
-- Two rules are established here and must never be relaxed:
--
--   1. Everything lives in the `trace` schema, never `public`, so that a future
--      merge into the ElectronIx MES database is a schema attach rather than a
--      rename-and-pray migration.
--
--   2. Every table carries `tenant_id` and has Row Level Security enabled from
--      the moment it is created. v1 ships a single factory on one box, but
--      retrofitting tenancy into millions of measurement rows later is not
--      something we are going to do.
-- ============================================================================

CREATE SCHEMA IF NOT EXISTS trace;

-- ---------------------------------------------------------------------------
-- Tenant context.
--
-- Every request does `SET LOCAL app.tenant_id = '<id>'`. The `true` second
-- argument to current_setting means "return NULL rather than raise if unset",
-- which makes an unset context deny every row instead of erroring in a way a
-- caller might be tempted to catch and ignore.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION trace.current_tenant_id() RETURNS BIGINT
LANGUAGE sql STABLE AS $$
    SELECT NULLIF(current_setting('app.tenant_id', true), '')::BIGINT
$$;

COMMENT ON FUNCTION trace.current_tenant_id() IS
    'Tenant for the current transaction, from SET LOCAL app.tenant_id. '
    'NULL when unset, which denies all rows under every RLS policy.';

-- Applies the standard tenant isolation policy to a table.
-- Used by every table in this database. FORCE is what makes the policy apply
-- to the table owner too, without which a table-owning app role would silently
-- bypass isolation.
CREATE OR REPLACE FUNCTION trace.apply_tenant_rls(target regclass) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', target);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', target);
    EXECUTE format($p$
        CREATE POLICY tenant_isolation ON %s
            USING (tenant_id = trace.current_tenant_id())
            WITH CHECK (tenant_id = trace.current_tenant_id())
    $p$, target);
END $$;

-- ---------------------------------------------------------------------------
-- Hierarchy. Names, semantics and natural keys align with ElectronIx MES so a
-- future merge is a foreign-key join, not a data reconciliation project.
-- ---------------------------------------------------------------------------

CREATE TABLE trace.tenant (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    code        TEXT        NOT NULL,
    name        TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT tenant_code_key UNIQUE (code)
);

-- `tenant` is the one table whose row *is* the tenant, so its policy compares
-- the primary key rather than a tenant_id column.
ALTER TABLE trace.tenant ENABLE ROW LEVEL SECURITY;
ALTER TABLE trace.tenant FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON trace.tenant
    USING (id = trace.current_tenant_id())
    WITH CHECK (id = trace.current_tenant_id());

CREATE TABLE trace.plant (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id       BIGINT NOT NULL REFERENCES trace.tenant(id),
    code            TEXT   NOT NULL,
    name            TEXT   NOT NULL,
    -- Printed into every QR. Per-plant configuration, never a constant: on
    -- premise this is the edge box's LAN name.
    trace_base_url  TEXT   NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT plant_code_key UNIQUE (tenant_id, code)
);
SELECT trace.apply_tenant_rls('trace.plant');

CREATE TABLE trace.line (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id   BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id    BIGINT NOT NULL REFERENCES trace.plant(id),
    code        TEXT   NOT NULL,
    name        TEXT   NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT line_code_key UNIQUE (plant_id, code)
);
SELECT trace.apply_tenant_rls('trace.line');

CREATE TABLE trace.station (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id   BIGINT NOT NULL REFERENCES trace.tenant(id),
    line_id     BIGINT NOT NULL REFERENCES trace.line(id),
    code        TEXT   NOT NULL,
    name        TEXT   NOT NULL,
    -- Retired stations stop taking work but are never deleted: their
    -- historical events must still resolve.
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT station_code_key UNIQUE (line_id, code)
);
SELECT trace.apply_tenant_rls('trace.station');

-- ---------------------------------------------------------------------------
-- Operators, roles and skills. Login is local (badge or PIN) so a station can
-- authenticate with the network, the server and the internet all down.
-- ---------------------------------------------------------------------------

CREATE TABLE trace.app_user (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id     BIGINT NOT NULL REFERENCES trace.tenant(id),
    username      TEXT   NOT NULL,
    display_name  TEXT   NOT NULL,
    -- Credentials are stored hashed. Never store a badge number or PIN in
    -- clear: a badge dump is a forged-signature machine.
    badge_hash    TEXT,
    pin_hash      TEXT,
    active        BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT app_user_username_key UNIQUE (tenant_id, username)
);
SELECT trace.apply_tenant_rls('trace.app_user');

CREATE TABLE trace.role (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  BIGINT NOT NULL REFERENCES trace.tenant(id),
    code       TEXT   NOT NULL,
    name       TEXT   NOT NULL,
    CONSTRAINT role_code_key UNIQUE (tenant_id, code)
);
SELECT trace.apply_tenant_rls('trace.role');

CREATE TABLE trace.user_role (
    tenant_id  BIGINT NOT NULL REFERENCES trace.tenant(id),
    user_id    BIGINT NOT NULL REFERENCES trace.app_user(id),
    role_id    BIGINT NOT NULL REFERENCES trace.role(id),
    PRIMARY KEY (user_id, role_id)
);
SELECT trace.apply_tenant_rls('trace.user_role');

CREATE TABLE trace.skill_certification (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  BIGINT NOT NULL REFERENCES trace.tenant(id),
    code       TEXT   NOT NULL,
    name       TEXT   NOT NULL,
    CONSTRAINT skill_code_key UNIQUE (tenant_id, code)
);
SELECT trace.apply_tenant_rls('trace.skill_certification');

CREATE TABLE trace.user_skill (
    tenant_id   BIGINT NOT NULL REFERENCES trace.tenant(id),
    user_id     BIGINT NOT NULL REFERENCES trace.app_user(id),
    skill_id    BIGINT NOT NULL REFERENCES trace.skill_certification(id),
    valid_from  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Certifications expire. An expired certification must fail the
    -- OPERATOR_AUTH gate, so this is queried, not decorative.
    valid_to    TIMESTAMPTZ,
    PRIMARY KEY (user_id, skill_id)
);
SELECT trace.apply_tenant_rls('trace.user_skill');

-- ---------------------------------------------------------------------------
-- Products and BOMs.
-- ---------------------------------------------------------------------------

CREATE TABLE trace.product (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  BIGINT NOT NULL REFERENCES trace.tenant(id),
    model_no   TEXT   NOT NULL,
    name       TEXT   NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT product_model_key UNIQUE (tenant_id, model_no)
);
SELECT trace.apply_tenant_rls('trace.product');

CREATE TABLE trace.product_revision (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id   BIGINT NOT NULL REFERENCES trace.tenant(id),
    product_id  BIGINT NOT NULL REFERENCES trace.product(id),
    revision    TEXT   NOT NULL,
    material    TEXT   NOT NULL DEFAULT 'OTHER'
                CHECK (material IN ('ALUMINIUM','PLASTIC','STEEL','PCB','OTHER')),
    -- Mark strategy is per revision, not per plant: aluminium castings and
    -- plastic housings do not mark the same way, and "apply a label at
    -- operation 1 instead" is a legitimate engineering answer.
    mark_method TEXT   NOT NULL DEFAULT 'LABEL'
                CHECK (mark_method IN ('LASER_DPM','LABEL','BOTH','NONE')),
    -- ISO/IEC 29158 minimum acceptable grade, when a grading reader exists.
    min_mark_grade CHAR(1) CHECK (min_mark_grade IS NULL OR min_mark_grade IN ('A','B','C','D','E','F')),
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT product_revision_key UNIQUE (product_id, revision)
);
SELECT trace.apply_tenant_rls('trace.product_revision');

CREATE TABLE trace.bom (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id            BIGINT NOT NULL REFERENCES trace.tenant(id),
    product_revision_id  BIGINT NOT NULL REFERENCES trace.product_revision(id),
    revision             TEXT   NOT NULL,
    effective_from       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT bom_revision_key UNIQUE (product_revision_id, revision)
);
SELECT trace.apply_tenant_rls('trace.bom');

CREATE TABLE trace.bom_line (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  BIGINT NOT NULL REFERENCES trace.tenant(id),
    bom_id     BIGINT NOT NULL REFERENCES trace.bom(id),
    position   TEXT   NOT NULL,
    part_no    TEXT   NOT NULL,
    qty        NUMERIC(18,6) NOT NULL CHECK (qty > 0),
    tracking   TEXT   NOT NULL
               CHECK (tracking IN ('SERIALISED','LOT_TRACKED','NON_TRACKED')),
    CONSTRAINT bom_line_position_key UNIQUE (bom_id, position)
);
SELECT trace.apply_tenant_rls('trace.bom_line');
CREATE INDEX bom_line_part_idx ON trace.bom_line (tenant_id, part_no);

-- ---------------------------------------------------------------------------
-- Devices: every physical data source.
-- ---------------------------------------------------------------------------

CREATE TABLE trace.device (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id   BIGINT NOT NULL REFERENCES trace.plant(id),
    station_id BIGINT REFERENCES trace.station(id),
    code       TEXT   NOT NULL,
    name       TEXT   NOT NULL,
    -- Driver discriminator, e.g. SERIAL_ASCII, MODBUS_TCP, ZEBRA_ZPL, MANUAL.
    -- Deliberately not an enum: adding a driver must not need a migration.
    kind       TEXT   NOT NULL,
    -- Driver-specific settings. JSONB so a new driver ships without DDL.
    config     JSONB  NOT NULL DEFAULT '{}'::jsonb,
    active     BOOLEAN NOT NULL DEFAULT TRUE,
    CONSTRAINT device_code_key UNIQUE (tenant_id, code)
);
SELECT trace.apply_tenant_rls('trace.device');

-- ---------------------------------------------------------------------------
-- Operations, routes and data collection points.
-- ---------------------------------------------------------------------------

CREATE TABLE trace.operation_def (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id         BIGINT NOT NULL REFERENCES trace.tenant(id),
    name              TEXT   NOT NULL,
    label             TEXT   NOT NULL,
    required_skill_id BIGINT REFERENCES trace.skill_certification(id),
    CONSTRAINT operation_def_name_key UNIQUE (tenant_id, name)
);
SELECT trace.apply_tenant_rls('trace.operation_def');

-- A station serves a SET of operations. Never bind an operation to exactly one
-- station: a small line may run an entire route on one terminal, and a busy
-- line may duplicate one operation across several stations for capacity.
CREATE TABLE trace.station_operation (
    tenant_id         BIGINT NOT NULL REFERENCES trace.tenant(id),
    station_id        BIGINT NOT NULL REFERENCES trace.station(id),
    operation_def_id  BIGINT NOT NULL REFERENCES trace.operation_def(id),
    PRIMARY KEY (station_id, operation_def_id)
);
SELECT trace.apply_tenant_rls('trace.station_operation');
CREATE INDEX station_operation_by_op_idx
    ON trace.station_operation (tenant_id, operation_def_id);

CREATE TABLE trace.route (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id            BIGINT NOT NULL REFERENCES trace.tenant(id),
    product_revision_id  BIGINT NOT NULL REFERENCES trace.product_revision(id),
    line_id              BIGINT NOT NULL REFERENCES trace.line(id),
    revision             TEXT   NOT NULL,
    active               BOOLEAN NOT NULL DEFAULT TRUE,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT route_key UNIQUE (product_revision_id, line_id, revision)
);
SELECT trace.apply_tenant_rls('trace.route');

CREATE TABLE trace.route_operation (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id         BIGINT NOT NULL REFERENCES trace.tenant(id),
    route_id          BIGINT NOT NULL REFERENCES trace.route(id),
    seq               INTEGER NOT NULL,
    operation_def_id  BIGINT NOT NULL REFERENCES trace.operation_def(id),
    requirement       TEXT   NOT NULL DEFAULT 'REQUIRED'
                      CHECK (requirement IN ('REQUIRED','OPTIONAL')),
    -- Gates and the failure path are configuration, held as JSONB so adding a
    -- gate type never requires a migration. Shape is validated in trace-core.
    gates             JSONB  NOT NULL DEFAULT '[]'::jsonb,
    failure_path      JSONB  NOT NULL DEFAULT '{"kind":"QUARANTINE"}'::jsonb,
    CONSTRAINT route_operation_seq_key UNIQUE (route_id, seq)
);
SELECT trace.apply_tenant_rls('trace.route_operation');

-- Explicit DAG edges. This is what lets a route express parallel branches and
-- optional steps without any of it being special-cased in code.
CREATE TABLE trace.route_operation_predecessor (
    tenant_id          BIGINT NOT NULL REFERENCES trace.tenant(id),
    route_operation_id BIGINT NOT NULL REFERENCES trace.route_operation(id),
    predecessor_seq    INTEGER NOT NULL,
    PRIMARY KEY (route_operation_id, predecessor_seq)
);
SELECT trace.apply_tenant_rls('trace.route_operation_predecessor');

CREATE TABLE trace.data_collection_point (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id         BIGINT NOT NULL REFERENCES trace.tenant(id),
    operation_def_id  BIGINT NOT NULL REFERENCES trace.operation_def(id),
    name              TEXT   NOT NULL,
    label             TEXT   NOT NULL,
    unit              TEXT,
    datatype          TEXT   NOT NULL
                      CHECK (datatype IN ('NUMERIC','TEXT','BOOLEAN','BARCODE')),
    -- Every limit in the product lives here as data, never as a constant.
    min_value         DOUBLE PRECISION,
    max_value         DOUBLE PRECISION,
    nominal_value     DOUBLE PRECISION,
    sample_rule       TEXT   NOT NULL DEFAULT 'EVERY'
                      CHECK (sample_rule IN ('EVERY','FIRST_OFF','EVERY_NTH')),
    sample_n          INTEGER,
    mandatory         BOOLEAN NOT NULL DEFAULT TRUE,
    device_id         BIGINT REFERENCES trace.device(id),
    CONSTRAINT dcp_name_key UNIQUE (operation_def_id, name),
    -- Incoherent limits are a configuration bug that must not reach a shift.
    CONSTRAINT dcp_limits_coherent
        CHECK (min_value IS NULL OR max_value IS NULL OR min_value <= max_value),
    CONSTRAINT dcp_sample_n_present
        CHECK (sample_rule <> 'EVERY_NTH' OR (sample_n IS NOT NULL AND sample_n > 0))
);
SELECT trace.apply_tenant_rls('trace.data_collection_point');

-- ---------------------------------------------------------------------------
-- Label templates, versioned and bound with an effective-from date so template
-- changes are auditable.
-- ---------------------------------------------------------------------------

CREATE TABLE trace.label_template (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  BIGINT NOT NULL REFERENCES trace.tenant(id),
    code       TEXT   NOT NULL,
    name       TEXT   NOT NULL,
    CONSTRAINT label_template_code_key UNIQUE (tenant_id, code)
);
SELECT trace.apply_tenant_rls('trace.label_template');

CREATE TABLE trace.label_template_version (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id    BIGINT NOT NULL REFERENCES trace.tenant(id),
    template_id  BIGINT NOT NULL REFERENCES trace.label_template(id),
    version      INTEGER NOT NULL,
    -- Canvas size in millimetres plus the element list, as JSONB.
    definition   JSONB  NOT NULL,
    effective_from TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT label_template_version_key UNIQUE (template_id, version)
);
SELECT trace.apply_tenant_rls('trace.label_template_version');
