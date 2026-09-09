-- ============================================================================
-- ElectronIx Trace -- migration 003: immutability and identity integrity.
--
-- The charter says traceability records are immutable and a scrapped UID is
-- retired forever. Application code already enforces both. This migration
-- enforces them again in the database, because "the application would never do
-- that" is not an answer an auditor accepts, and because psql exists.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Append-only enforcement.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION trace.deny_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION
        'trace.% is append-only: % is not permitted. Corrections are new rows '
        'that supersede old ones, carrying a reason and an operator.',
        TG_TABLE_NAME, TG_OP
        USING ERRCODE = 'restrict_violation';
END $$;

COMMENT ON FUNCTION trace.deny_mutation() IS
    'Refuses UPDATE and DELETE on append-only traceability tables.';

-- Captured evidence. Nothing here may ever be edited or removed.
CREATE TRIGGER unit_event_append_only
    BEFORE UPDATE OR DELETE ON trace.unit_event
    FOR EACH ROW EXECUTE FUNCTION trace.deny_mutation();

CREATE TRIGGER measurement_append_only
    BEFORE UPDATE OR DELETE ON trace.measurement
    FOR EACH ROW EXECUTE FUNCTION trace.deny_mutation();

CREATE TRIGGER genealogy_link_append_only
    BEFORE UPDATE OR DELETE ON trace.genealogy_link
    FOR EACH ROW EXECUTE FUNCTION trace.deny_mutation();

CREATE TRIGGER mark_record_append_only
    BEFORE UPDATE OR DELETE ON trace.mark_record
    FOR EACH ROW EXECUTE FUNCTION trace.deny_mutation();

CREATE TRIGGER test_result_append_only
    BEFORE UPDATE OR DELETE ON trace.test_result
    FOR EACH ROW EXECUTE FUNCTION trace.deny_mutation();

CREATE TRIGGER defect_append_only
    BEFORE UPDATE OR DELETE ON trace.defect
    FOR EACH ROW EXECUTE FUNCTION trace.deny_mutation();

CREATE TRIGGER scrap_record_append_only
    BEFORE UPDATE OR DELETE ON trace.scrap_record
    FOR EACH ROW EXECUTE FUNCTION trace.deny_mutation();

-- ---------------------------------------------------------------------------
-- A scrapped UID is retired forever and never reissued.
-- ---------------------------------------------------------------------------

-- Refuse to create a unit whose UID was previously retired.
CREATE OR REPLACE FUNCTION trace.reject_retired_uid() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM trace.retired_uid r WHERE r.uid = NEW.uid) THEN
        RAISE EXCEPTION
            'uid % is retired and can never be reissued', NEW.uid
            USING ERRCODE = 'unique_violation';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER unit_uid_not_retired
    BEFORE INSERT ON trace.unit
    FOR EACH ROW EXECUTE FUNCTION trace.reject_retired_uid();

-- Retire the UID the moment a unit is scrapped, and refuse to bring a scrapped
-- unit back. `unit` is intentionally mutable (state advances as it is built),
-- so the guard is on the transition rather than on the row.
CREATE OR REPLACE FUNCTION trace.enforce_scrap_is_terminal() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state = 'SCRAPPED' AND NEW.state <> 'SCRAPPED' THEN
        RAISE EXCEPTION
            'unit % is scrapped: that state is terminal and the uid is retired',
            OLD.uid
            USING ERRCODE = 'restrict_violation';
    END IF;

    IF NEW.state = 'SCRAPPED' AND OLD.state <> 'SCRAPPED' THEN
        INSERT INTO trace.retired_uid (uid, tenant_id, unit_id, reason)
        VALUES (NEW.uid, NEW.tenant_id, NEW.id, 'scrapped')
        ON CONFLICT (uid) DO NOTHING;
    END IF;

    -- The public identity is the one thing on a unit that must never change.
    IF NEW.uid <> OLD.uid THEN
        RAISE EXCEPTION 'unit uid is immutable (% -> %)', OLD.uid, NEW.uid
            USING ERRCODE = 'restrict_violation';
    END IF;

    RETURN NEW;
END $$;

CREATE TRIGGER unit_scrap_is_terminal
    BEFORE UPDATE ON trace.unit
    FOR EACH ROW EXECUTE FUNCTION trace.enforce_scrap_is_terminal();

-- ---------------------------------------------------------------------------
-- Genealogy walks.
--
-- Recursive CTEs are one of the reasons this product is on Postgres: an
-- N-level parent/child walk stays a single readable query.
-- ---------------------------------------------------------------------------

-- Backward trace: every descendant consumed into a unit, to any depth.
CREATE OR REPLACE FUNCTION trace.genealogy_descendants(root_unit_id BIGINT)
RETURNS TABLE (
    depth          INTEGER,
    parent_unit_id BIGINT,
    child_unit_id  BIGINT,
    lot_no         TEXT,
    qty            NUMERIC,
    operation_seq  INTEGER,
    recorded_at    TIMESTAMPTZ
) LANGUAGE sql STABLE AS $$
    WITH RECURSIVE walk AS (
        SELECT 1 AS depth, g.parent_unit_id, g.child_unit_id, g.lot_no,
               g.qty, g.operation_seq, g.recorded_at
          FROM trace.genealogy_link g
         WHERE g.parent_unit_id = root_unit_id
        UNION ALL
        SELECT w.depth + 1, g.parent_unit_id, g.child_unit_id, g.lot_no,
               g.qty, g.operation_seq, g.recorded_at
          FROM trace.genealogy_link g
          JOIN walk w ON g.parent_unit_id = w.child_unit_id
         -- Depth cap: a genealogy cycle would otherwise spin forever. The
         -- schema forbids self-links, but bad data must not hang a shift.
         WHERE w.depth < 32
    )
    SELECT * FROM walk;
$$;

-- Forward trace: every ancestor a unit was built into, to any depth.
CREATE OR REPLACE FUNCTION trace.genealogy_ancestors(leaf_unit_id BIGINT)
RETURNS TABLE (
    depth          INTEGER,
    parent_unit_id BIGINT,
    child_unit_id  BIGINT,
    operation_seq  INTEGER,
    recorded_at    TIMESTAMPTZ
) LANGUAGE sql STABLE AS $$
    WITH RECURSIVE walk AS (
        SELECT 1 AS depth, g.parent_unit_id, g.child_unit_id,
               g.operation_seq, g.recorded_at
          FROM trace.genealogy_link g
         WHERE g.child_unit_id = leaf_unit_id
        UNION ALL
        SELECT w.depth + 1, g.parent_unit_id, g.child_unit_id,
               g.operation_seq, g.recorded_at
          FROM trace.genealogy_link g
          JOIN walk w ON g.child_unit_id = w.parent_unit_id
         WHERE w.depth < 32
    )
    SELECT * FROM walk;
$$;

COMMENT ON FUNCTION trace.genealogy_ancestors(BIGINT) IS
    'Every unit this one was built into, to any depth. Combined with '
    'genealogy_lot_idx this is what makes the recall query fast.';
