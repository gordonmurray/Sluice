-- Settlement receipts, one row per settled x402 payment.
--
-- Provenance: tx_hash, network, payer, success come from the facilitator's
-- SettleResponse. amount_micro_usdc and pay_to are what the gateway charged
-- per its rules table — the same in-process table that produced the payment
-- requirements the facilitator verified, but not independent on-chain
-- evidence (the v2 SettleResponse carries no amount). Reconcile against the
-- chain if you need proof, not this table.
CREATE TABLE payments (
    id                BIGSERIAL PRIMARY KEY,
    tx_hash           TEXT        NOT NULL CHECK (tx_hash <> ''),
    network           TEXT        NOT NULL CHECK (network <> ''),
    payer             TEXT        NOT NULL CHECK (payer <> ''),
    pay_to            TEXT        NOT NULL CHECK (pay_to <> ''),
    amount_micro_usdc BIGINT      NOT NULL CHECK (amount_micro_usdc > 0),
    path              TEXT        NOT NULL CHECK (path <> ''),
    caller            TEXT,
    success           BOOLEAN     NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One settlement per transaction per network; hashes are not globally
    -- unique across chains.
    UNIQUE (network, tx_hash)
);

CREATE INDEX payments_payer_idx ON payments (payer);
CREATE INDEX payments_path_idx ON payments (path);
CREATE INDEX payments_created_at_idx ON payments (created_at);
