-- What sponsored transactions may still cost, between prepare and landing. The
-- quota and the breaker count these rows on top of `sponsorship_usage`.
CREATE TABLE IF NOT EXISTS sponsorship_holds (
    -- The signed quote's digest, or the transaction id when no quote exists.
    digest        BYTEA       NOT NULL,
    chain_id      BIGINT      NOT NULL,
    quota_subject TEXT        NOT NULL,
    -- What the transaction should cost, which the quota counts.
    amount        NUMERIC     NOT NULL,
    -- The most it can cost, which the breaker counts.
    max_amount    NUMERIC     NOT NULL,
    -- Set when the quote is sent, which it can only be once.
    claimed       BOOLEAN     NOT NULL DEFAULT FALSE,
    -- A hold past this time no longer counts: an unsent quote's TTL, or a bound on
    -- a sent transaction whose settlement never came.
    expires_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (digest, chain_id)
);

CREATE INDEX IF NOT EXISTS idx_sponsorship_holds_subject
    ON sponsorship_holds (quota_subject, chain_id);

CREATE INDEX IF NOT EXISTS idx_sponsorship_holds_chain_expiry
    ON sponsorship_holds (chain_id, expires_at);

-- Stores every hold or none of them, deciding each against the ledger and the live
-- holds. One call, so that holding a quote costs the prepare a single round trip.
CREATE OR REPLACE FUNCTION reserve_sponsorship_holds(
    digests      BYTEA[],
    chain_ids    BIGINT[],
    subjects     TEXT[],
    amounts      NUMERIC[],
    max_amounts  NUMERIC[],
    claims       BOOLEAN[],
    expiries     TIMESTAMPTZ[],
    quota_caps   NUMERIC[],
    breaker_caps NUMERIC[],
    windows      INT[]
) RETURNS BOOLEAN AS $$
DECLARE
    chain BIGINT;
    i     INT;
    spent NUMERIC;
BEGIN
    -- The breaker is one total per chain, so every reservation on a chain is decided
    -- in turn. Sorted, so that two multichain reservations cannot deadlock.
    FOR chain IN SELECT DISTINCT c FROM unnest(chain_ids) AS c ORDER BY c LOOP
        PERFORM pg_advisory_xact_lock(hashtextextended('sponsorship_holds:' || chain, 0));
        DELETE FROM sponsorship_holds WHERE chain_id = chain AND expires_at <= NOW();
    END LOOP;

    FOR i IN 1 .. COALESCE(array_length(digests, 1), 0) LOOP
        SELECT COALESCE(SUM(eth_spent), 0) INTO spent FROM sponsorship_usage
         WHERE quota_subject = subjects[i] AND chain_id = chain_ids[i]
           AND sponsored_at >= NOW() - make_interval(hours => windows[i]);
        IF spent + (SELECT COALESCE(SUM(amount), 0) FROM sponsorship_holds
                     WHERE quota_subject = subjects[i] AND chain_id = chain_ids[i])
           + amounts[i] > quota_caps[i] THEN
            RAISE EXCEPTION USING ERRCODE = 'SP001';
        END IF;

        SELECT COALESCE(SUM(eth_spent), 0) INTO spent FROM sponsorship_usage
         WHERE chain_id = chain_ids[i]
           AND sponsored_at >= NOW() - make_interval(hours => windows[i]);
        IF spent + (SELECT COALESCE(SUM(max_amount), 0) FROM sponsorship_holds
                     WHERE chain_id = chain_ids[i])
           + max_amounts[i] > breaker_caps[i] THEN
            RAISE EXCEPTION USING ERRCODE = 'SP001';
        END IF;

        INSERT INTO sponsorship_holds
            (digest, chain_id, quota_subject, amount, max_amount, claimed, expires_at)
        VALUES (digests[i], chain_ids[i], subjects[i], amounts[i], max_amounts[i], claims[i],
                expiries[i])
        ON CONFLICT (digest, chain_id) DO NOTHING;
    END LOOP;
    RETURN TRUE;
EXCEPTION WHEN SQLSTATE 'SP001' THEN
    -- Leaving through here undoes the rows this call already stored.
    RETURN FALSE;
END;
$$ LANGUAGE plpgsql;
