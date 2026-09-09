-- ============================================================================
-- ElectronIx Trace -- migration 004: tenant provisioning.
--
-- RLS creates a genuine bootstrap problem: the policy on trace.tenant checks
-- `id = current_tenant_id()`, but the id does not exist until the row is
-- inserted. There is therefore no way for the application, running under a
-- tenant context, to create a tenant.
--
-- That is the correct security posture rather than a bug to work around:
-- provisioning a tenant is an administrative act, not something a station
-- should be able to do. It gets one explicit, auditable entry point.
-- ============================================================================

CREATE OR REPLACE FUNCTION trace.provision_tenant(p_code TEXT, p_name TEXT)
RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = trace, pg_temp
AS $$
DECLARE
    new_id BIGINT;
BEGIN
    -- Take the identity value first so the row's own tenant context can be
    -- established before the insert is checked.
    new_id := nextval(pg_get_serial_sequence('trace.tenant', 'id'));

    -- Transaction-local: this does not leak into a pooled connection's next
    -- user. The caller's context IS replaced for the rest of this
    -- transaction, which is why provisioning belongs in its own transaction.
    PERFORM set_config('app.tenant_id', new_id::text, true);

    INSERT INTO trace.tenant (id, code, name)
    OVERRIDING SYSTEM VALUE
    VALUES (new_id, p_code, p_name);

    RETURN new_id;
END $$;

COMMENT ON FUNCTION trace.provision_tenant(TEXT, TEXT) IS
    'Create a tenant. Administrative entry point: RLS makes this impossible '
    'through the ordinary application path by design. NOTE: replaces '
    'app.tenant_id for the remainder of the calling transaction, so call it in '
    'a transaction of its own.';
