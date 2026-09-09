-- ============================================================================
-- ElectronIx Trace -- migration 002: runtime tables, partitioning, indexes.
--
-- Every runtime row carries tenant_id, plant_id, station_id, operator_id,
-- recorded_at (device time) AND received_at (server time). RLS is applied to
-- each table as it is created, exactly as in migration 001.
--
-- The two queries this schema exists to serve:
--
--   backward trace  unit -> its complete history          (indexed by unit)
--   forward trace   lot or device -> every unit affected  (indexed by lot,
--                                                          and by device+time)
--
-- The forward trace is the recall query. It is the most valuable feature in
-- the product, so it is a fast indexed lookup and never a table scan.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Job cards. `external_ref` exists from the start so that when MES or an ERP
-- takes ownership of work orders, the join has somewhere to land.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.job_card (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id            BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id             BIGINT NOT NULL REFERENCES trace.plant(id),
    number               TEXT   NOT NULL,
    product_revision_id  BIGINT NOT NULL REFERENCES trace.product_revision(id),
    qty                  INTEGER NOT NULL CHECK (qty > 0),
    priority             INTEGER NOT NULL DEFAULT 0,
    due_date             TIMESTAMPTZ,
    external_ref         TEXT,
    source               TEXT   NOT NULL DEFAULT 'local',
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT job_card_number_key UNIQUE (tenant_id, number)
);
SELECT trace.apply_tenant_rls('trace.job_card');

-- ---------------------------------------------------------------------------
-- Units.
--
-- `uid` is the public ULID: the value printed on the part, encoded in the
-- code, and used in the /t/{ulid} URL. Stored as canonical 26-character text
-- so that lexicographic order equals time order, and so the value in the
-- database is byte-identical to the value on the part.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.unit (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    uid                  TEXT   NOT NULL,
    tenant_id            BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id             BIGINT NOT NULL REFERENCES trace.plant(id),
    product_revision_id  BIGINT NOT NULL REFERENCES trace.product_revision(id),
    job_card_id          BIGINT NOT NULL REFERENCES trace.job_card(id),
    serial               TEXT   NOT NULL,
    state                TEXT   NOT NULL DEFAULT 'BORN'
                         CHECK (state IN ('BORN','MARKED','IN_PROCESS','HELD',
                                          'QUARANTINED','REWORKING','COMPLETED',
                                          'SHIPPED','SCRAPPED')),
    current_operation    INTEGER,
    index_in_job         BIGINT NOT NULL DEFAULT 0,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Globally unique, not per-tenant: a UID must resolve unambiguously from a
    -- scan with no tenant context, which is exactly what /t/{ulid} does.
    CONSTRAINT unit_uid_key UNIQUE (uid),
    CONSTRAINT unit_uid_is_ulid CHECK (uid ~ '^[0-9A-HJKMNP-TV-Z]{26}$')
);
SELECT trace.apply_tenant_rls('trace.unit');
CREATE INDEX unit_job_idx      ON trace.unit (tenant_id, job_card_id, index_in_job);
CREATE INDEX unit_state_idx    ON trace.unit (tenant_id, state);
CREATE INDEX unit_revision_idx ON trace.unit (tenant_id, product_revision_id);
CREATE INDEX unit_serial_idx   ON trace.unit (tenant_id, serial);

-- A scrapped UID is retired forever and never reissued. Enforced by trigger in
-- migration 003; this is the ledger it consults.
CREATE TABLE trace.retired_uid (
    uid         TEXT PRIMARY KEY,
    tenant_id   BIGINT NOT NULL REFERENCES trace.tenant(id),
    unit_id     BIGINT NOT NULL,
    reason      TEXT   NOT NULL,
    retired_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
SELECT trace.apply_tenant_rls('trace.retired_uid');

-- ---------------------------------------------------------------------------
-- unit_event: append-only, hash chained, partitioned by month.
--
-- `id` is the event ULID and doubles as the idempotency key, so a station
-- replaying its offline spool after an edge reboot cannot create duplicates.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.unit_event (
    id             TEXT   NOT NULL,
    tenant_id      BIGINT NOT NULL,
    plant_id       BIGINT NOT NULL,
    unit_id        BIGINT NOT NULL,
    kind           TEXT   NOT NULL,
    operation_seq  INTEGER,
    station_id     BIGINT,
    operator_id    BIGINT,
    detail         TEXT,
    recorded_at    TIMESTAMPTZ NOT NULL,
    received_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Set when |recorded_at - received_at| exceeded the configured threshold.
    -- The event is still stored: we never drop production data over a clock
    -- problem, we mark it so an auditor can see it.
    clock_skewed   BOOLEAN NOT NULL DEFAULT FALSE,
    chain_seq      BIGINT NOT NULL,
    prev_hash      TEXT   NOT NULL,
    row_hash       TEXT   NOT NULL,
    PRIMARY KEY (id, recorded_at)
) PARTITION BY RANGE (recorded_at);

SELECT trace.apply_tenant_rls('trace.unit_event');
CREATE INDEX unit_event_unit_idx    ON trace.unit_event (tenant_id, unit_id, chain_seq);
CREATE INDEX unit_event_station_idx ON trace.unit_event (tenant_id, station_id, recorded_at);
CREATE INDEX unit_event_kind_idx    ON trace.unit_event (tenant_id, kind, recorded_at);

-- ---------------------------------------------------------------------------
-- measurement: the big table. Append-only, hash chained, partitioned monthly.
--
-- Volume estimate: 500 units/day x 40 measurements is roughly 7M rows per year
-- on one plant. Partitioning is cheap now and painful later.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.measurement (
    id             TEXT   NOT NULL,
    tenant_id      BIGINT NOT NULL,
    plant_id       BIGINT NOT NULL,
    unit_id        BIGINT NOT NULL,
    dcp_id         BIGINT NOT NULL,
    operation_seq  INTEGER NOT NULL,
    station_id     BIGINT,
    operator_id    BIGINT,
    device_id      BIGINT,
    datatype       TEXT   NOT NULL,
    num_value      DOUBLE PRECISION,
    text_value     TEXT,
    bool_value     BOOLEAN,
    verdict        TEXT   NOT NULL CHECK (verdict IN ('PASS','FAIL')),
    source         TEXT   NOT NULL CHECK (source IN ('DEVICE','MANUAL')),
    -- The exact bytes the device returned, before parsing. When a customer
    -- disputes a reading in two years, this is the answer.
    raw_payload    TEXT,
    recorded_at    TIMESTAMPTZ NOT NULL,
    received_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    clock_skewed   BOOLEAN NOT NULL DEFAULT FALSE,
    chain_seq      BIGINT NOT NULL,
    prev_hash      TEXT   NOT NULL,
    row_hash       TEXT   NOT NULL,
    -- Records are never updated or deleted. A correction is a new row that
    -- points at the one it supersedes, with a reason and an operator.
    supersedes        TEXT,
    supersede_reason  TEXT,
    PRIMARY KEY (id, recorded_at)
) PARTITION BY RANGE (recorded_at);

SELECT trace.apply_tenant_rls('trace.measurement');
CREATE INDEX measurement_unit_idx   ON trace.measurement (tenant_id, unit_id, chain_seq);
CREATE INDEX measurement_dcp_idx    ON trace.measurement (tenant_id, dcp_id, recorded_at);
-- Forward trace by machine: "which units did this device touch between 14:00
-- and 16:00 while it was drifting out of calibration".
CREATE INDEX measurement_device_time_idx
    ON trace.measurement (tenant_id, device_id, recorded_at)
    WHERE device_id IS NOT NULL;
CREATE INDEX measurement_fail_idx
    ON trace.measurement (tenant_id, recorded_at)
    WHERE verdict = 'FAIL';

-- ---------------------------------------------------------------------------
-- Tests: a group of measurements with one overall verdict.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.test_result (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id      BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id       BIGINT NOT NULL REFERENCES trace.plant(id),
    unit_id        BIGINT NOT NULL REFERENCES trace.unit(id),
    operation_seq  INTEGER NOT NULL,
    test_name      TEXT   NOT NULL,
    verdict        TEXT   NOT NULL CHECK (verdict IN ('PASS','FAIL')),
    station_id     BIGINT REFERENCES trace.station(id),
    operator_id    BIGINT REFERENCES trace.app_user(id),
    recorded_at    TIMESTAMPTZ NOT NULL,
    received_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
SELECT trace.apply_tenant_rls('trace.test_result');
CREATE INDEX test_result_unit_idx ON trace.test_result (tenant_id, unit_id);

-- ---------------------------------------------------------------------------
-- Genealogy: what went into what.
--
-- Serialised children and lot consumption are separate columns rather than a
-- JSONB blob, precisely so the recall query can be indexed.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.genealogy_link (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id       BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id        BIGINT NOT NULL REFERENCES trace.plant(id),
    parent_unit_id  BIGINT NOT NULL REFERENCES trace.unit(id),
    bom_line_id     BIGINT REFERENCES trace.bom_line(id),
    -- Exactly one of these two is populated.
    child_unit_id   BIGINT REFERENCES trace.unit(id),
    lot_no          TEXT,
    qty             NUMERIC(18,6),
    operation_seq   INTEGER NOT NULL,
    station_id      BIGINT REFERENCES trace.station(id),
    operator_id     BIGINT REFERENCES trace.app_user(id),
    recorded_at     TIMESTAMPTZ NOT NULL,
    received_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT genealogy_one_kind CHECK (
        (child_unit_id IS NOT NULL AND lot_no IS NULL)
     OR (child_unit_id IS NULL     AND lot_no IS NOT NULL)
    ),
    CONSTRAINT genealogy_lot_has_qty CHECK (lot_no IS NULL OR qty IS NOT NULL),
    -- A unit cannot be its own component.
    CONSTRAINT genealogy_no_self CHECK (child_unit_id IS NULL OR child_unit_id <> parent_unit_id)
);
SELECT trace.apply_tenant_rls('trace.genealogy_link');

-- Backward trace: walk down from a parent.
CREATE INDEX genealogy_parent_idx ON trace.genealogy_link (tenant_id, parent_unit_id);
-- Forward trace by serialised child: walk up to every parent.
CREATE INDEX genealogy_child_idx
    ON trace.genealogy_link (tenant_id, child_unit_id)
    WHERE child_unit_id IS NOT NULL;
-- THE RECALL QUERY: "every unit that consumed lot X".
CREATE INDEX genealogy_lot_idx
    ON trace.genealogy_link (tenant_id, lot_no)
    WHERE lot_no IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Non-conformance.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.defect (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id      BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id       BIGINT NOT NULL REFERENCES trace.plant(id),
    unit_id        BIGINT NOT NULL REFERENCES trace.unit(id),
    operation_seq  INTEGER,
    -- Which gate produced it, e.g. COMPONENT_VERIFY.
    gate           TEXT,
    code           TEXT   NOT NULL,
    detail         TEXT,
    station_id     BIGINT REFERENCES trace.station(id),
    operator_id    BIGINT REFERENCES trace.app_user(id),
    recorded_at    TIMESTAMPTZ NOT NULL,
    received_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
SELECT trace.apply_tenant_rls('trace.defect');
CREATE INDEX defect_unit_idx ON trace.defect (tenant_id, unit_id);
CREATE INDEX defect_code_idx ON trace.defect (tenant_id, code, recorded_at);

CREATE TABLE trace.rework_order (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id      BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id       BIGINT NOT NULL REFERENCES trace.plant(id),
    unit_id        BIGINT NOT NULL REFERENCES trace.unit(id),
    defect_id      BIGINT REFERENCES trace.defect(id),
    to_operation   INTEGER NOT NULL,
    -- Operations whose completion this rework voids. They must be performed
    -- again, and the loop stays visible in the final trace.
    invalidates    INTEGER[] NOT NULL DEFAULT '{}',
    authorised_by  BIGINT REFERENCES trace.app_user(id),
    recorded_at    TIMESTAMPTZ NOT NULL,
    received_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
SELECT trace.apply_tenant_rls('trace.rework_order');

CREATE TABLE trace.scrap_record (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id      BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id       BIGINT NOT NULL REFERENCES trace.plant(id),
    unit_id        BIGINT NOT NULL REFERENCES trace.unit(id),
    defect_id      BIGINT REFERENCES trace.defect(id),
    reason         TEXT   NOT NULL,
    authorised_by  BIGINT REFERENCES trace.app_user(id),
    recorded_at    TIMESTAMPTZ NOT NULL,
    received_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
SELECT trace.apply_tenant_rls('trace.scrap_record');

-- ---------------------------------------------------------------------------
-- Marks: every label print and mark attempt, successful or not.
--
-- Reprinting is a controlled event. Duplicate labels in the field are a
-- genuine traceability failure, so a reason code and a supervisor are
-- recorded. Treat this as a security feature, not a convenience.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.mark_record (
    id                   TEXT PRIMARY KEY,
    tenant_id            BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id             BIGINT NOT NULL REFERENCES trace.plant(id),
    unit_id              BIGINT NOT NULL REFERENCES trace.unit(id),
    template_version_id  BIGINT REFERENCES trace.label_template_version(id),
    device_id            BIGINT REFERENCES trace.device(id),
    operation_seq        INTEGER,
    attempt              INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    outcome              TEXT   NOT NULL
                         CHECK (outcome IN ('VERIFIED','VERIFY_FAILED',
                                            'DEVICE_FAULT','APPLIED_UNVERIFIED')),
    read_back            TEXT,
    grade                CHAR(1),
    -- The base URL encoded into the code, stored per record: labels printed in
    -- year one must still resolve in year five, so we always know what was
    -- actually printed.
    trace_base_url       TEXT   NOT NULL,
    is_reprint           BOOLEAN NOT NULL DEFAULT FALSE,
    reason               TEXT,
    authorised_by        BIGINT REFERENCES trace.app_user(id),
    payload_sent         TEXT,
    station_id           BIGINT REFERENCES trace.station(id),
    operator_id          BIGINT REFERENCES trace.app_user(id),
    recorded_at          TIMESTAMPTZ NOT NULL,
    received_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- A reprint without a reason code and an authoriser is exactly the
    -- uncontrolled duplicate label we are trying to prevent.
    CONSTRAINT mark_reprint_is_controlled CHECK (
        NOT is_reprint OR (reason IS NOT NULL AND authorised_by IS NOT NULL)
    )
);
SELECT trace.apply_tenant_rls('trace.mark_record');
CREATE INDEX mark_record_unit_idx ON trace.mark_record (tenant_id, unit_id, attempt);

-- ---------------------------------------------------------------------------
-- Outbox: populated from day one even though nothing drains it until the cloud
-- phase. The table, the ULIDs and the idempotency keys exist now so enabling
-- cloud sync later is a config change rather than a rewrite.
-- ---------------------------------------------------------------------------
CREATE TABLE trace.outbox (
    id            TEXT PRIMARY KEY,
    tenant_id     BIGINT NOT NULL REFERENCES trace.tenant(id),
    plant_id      BIGINT NOT NULL REFERENCES trace.plant(id),
    topic         TEXT   NOT NULL,
    payload       JSONB  NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    published_at  TIMESTAMPTZ,
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_error    TEXT
);
SELECT trace.apply_tenant_rls('trace.outbox');
-- Partial index: the drain only ever looks for unpublished rows, and this
-- keeps that scan proportional to the backlog rather than to history.
CREATE INDEX outbox_pending_idx
    ON trace.outbox (tenant_id, created_at)
    WHERE published_at IS NULL;

-- ---------------------------------------------------------------------------
-- Monthly partition management.
-- ---------------------------------------------------------------------------

-- Create the partition covering `month` for a partitioned table, if absent.
CREATE OR REPLACE FUNCTION trace.ensure_month_partition(tbl TEXT, month DATE)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    start_date DATE := date_trunc('month', month)::date;
    end_date   DATE := (date_trunc('month', month) + INTERVAL '1 month')::date;
    part_name  TEXT := format('%s_%s', tbl, to_char(start_date, 'YYYYMM'));
BEGIN
    IF to_regclass(format('trace.%I', part_name)) IS NULL THEN
        EXECUTE format(
            'CREATE TABLE trace.%I PARTITION OF trace.%I FOR VALUES FROM (%L) TO (%L)',
            part_name, tbl, start_date, end_date
        );
    END IF;
END $$;

COMMENT ON FUNCTION trace.ensure_month_partition(TEXT, DATE) IS
    'Idempotently create the monthly partition covering `month`. Called ahead '
    'of time by the edge service so a shift never lands on a missing partition.';

-- Create partitions from `from_month` for `count` months, on both big tables.
CREATE OR REPLACE FUNCTION trace.ensure_partitions(from_month DATE, count INTEGER)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE i INTEGER;
BEGIN
    FOR i IN 0..(count - 1) LOOP
        PERFORM trace.ensure_month_partition(
            'unit_event', (date_trunc('month', from_month) + (i || ' month')::INTERVAL)::date);
        PERFORM trace.ensure_month_partition(
            'measurement', (date_trunc('month', from_month) + (i || ' month')::INTERVAL)::date);
    END LOOP;
END $$;

-- A default partition means a row with an unexpected timestamp is still
-- captured rather than rejected. Losing production data because a partition
-- was missing would be a far worse failure than an untidy default.
CREATE TABLE trace.unit_event_default  PARTITION OF trace.unit_event  DEFAULT;
CREATE TABLE trace.measurement_default PARTITION OF trace.measurement DEFAULT;

-- Cover the previous month through the next twelve.
SELECT trace.ensure_partitions((date_trunc('month', now()) - INTERVAL '1 month')::date, 14);
