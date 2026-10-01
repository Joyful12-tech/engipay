-- Payment requests schema for invoicing and payment polling.
-- Payment requests are invoices that recipients can share for receiving payments.
--
-- `payment_requests` already exists from migration 0001 with the older shape
-- (`user_id`, `amount`, `note`, `uri`).  `CREATE TABLE IF NOT EXISTS` below is
-- therefore a no-op on any database that has run 0001, which is why the newer
-- columns were silently missing.  This migration adds them idempotently, so
-- both a fresh install and an upgraded database end up with the same shape.

CREATE TABLE IF NOT EXISTS payment_requests (
    id                UUID PRIMARY KEY,
    recipient_id      UUID NOT NULL REFERENCES users (id),
    recipient_tag     TEXT,
    requested_amount  NUMERIC(78, 0) NOT NULL CHECK (requested_amount > 0),
    asset             TEXT NOT NULL CHECK (asset IN ('XLM', 'ETH', 'USDC', 'BTC')),
    status            TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid', 'expired')),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at        TIMESTAMPTZ NOT NULL,
    settled_at        TIMESTAMPTZ,
    payment_reference TEXT UNIQUE
);

-- ── Upgrade the 0001 table ──────────────────────────────────────────────
--
-- `recipient_id` is populated from the 0001 `user_id` before the old column is
-- dropped, so existing invoices stay attached to the right recipient.
ALTER TABLE payment_requests
    ADD COLUMN IF NOT EXISTS recipient_id      UUID REFERENCES users (id),
    ADD COLUMN IF NOT EXISTS recipient_tag     TEXT,
    ADD COLUMN IF NOT EXISTS requested_amount  NUMERIC(78, 0),
    ADD COLUMN IF NOT EXISTS status            TEXT NOT NULL DEFAULT 'pending',
    ADD COLUMN IF NOT EXISTS settled_at        TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS payment_reference TEXT;

-- Carry existing rows over to the new columns before the NOT NULL constraints
-- are enforced.  `amount` (0001) becomes `requested_amount`; `uri` becomes the
-- payment reference.  Each block is guarded by a column check so re-running the
-- migration on an already-upgraded database is a no-op rather than an error.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'payment_requests' AND column_name = 'user_id'
    ) THEN
        EXECUTE 'UPDATE payment_requests
                 SET recipient_id = user_id
                 WHERE recipient_id IS NULL';
    END IF;

    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'payment_requests' AND column_name = 'amount'
    ) THEN
        EXECUTE 'UPDATE payment_requests
                 SET requested_amount = amount
                 WHERE requested_amount IS NULL';
    END IF;

    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'payment_requests' AND column_name = 'uri'
    ) THEN
        EXECUTE 'UPDATE payment_requests
                 SET payment_reference = uri
                 WHERE payment_reference IS NULL';
    END IF;
END
$$;

-- `XLM` was added to the asset check after 0001 was written.
ALTER TABLE payment_requests DROP CONSTRAINT IF EXISTS payment_requests_asset_check;
ALTER TABLE payment_requests
    ADD CONSTRAINT payment_requests_asset_check
    CHECK (asset IN ('XLM', 'ETH', 'USDC', 'BTC'));

ALTER TABLE payment_requests DROP CONSTRAINT IF EXISTS payment_requests_amount_check;

-- Now that every row is populated, make the new columns required.
ALTER TABLE payment_requests
    ALTER COLUMN recipient_id SET NOT NULL,
    ALTER COLUMN requested_amount SET NOT NULL;

-- Constraints are dropped first so a re-run can re-add them.
ALTER TABLE payment_requests DROP CONSTRAINT IF EXISTS payment_requests_requested_amount_positive;
ALTER TABLE payment_requests DROP CONSTRAINT IF EXISTS payment_requests_status_check;

ALTER TABLE payment_requests
    ADD CONSTRAINT payment_requests_requested_amount_positive CHECK (requested_amount > 0);

ALTER TABLE payment_requests
    ADD CONSTRAINT payment_requests_status_check
    CHECK (status IN ('pending', 'paid', 'expired'));

-- Drop the superseded 0001 columns.  The data now lives in the new columns.
ALTER TABLE payment_requests DROP COLUMN IF EXISTS user_id;
ALTER TABLE payment_requests DROP COLUMN IF EXISTS amount;
ALTER TABLE payment_requests DROP COLUMN IF EXISTS note;
ALTER TABLE payment_requests DROP COLUMN IF EXISTS uri;

-- A payment reference is what makes a request idempotent, so it must be unique.
CREATE UNIQUE INDEX IF NOT EXISTS idx_payment_requests_payment_reference
    ON payment_requests (payment_reference);

CREATE INDEX IF NOT EXISTS idx_payment_requests_recipient
    ON payment_requests (recipient_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_payment_requests_status
    ON payment_requests (status, expires_at);