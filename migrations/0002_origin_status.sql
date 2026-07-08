-- The HTTP status the client actually received for the request this payment
-- bought: the origin's status when it answered, 502 when it was unreachable,
-- 413 when the gateway refused the request body. NULL on rows written before
-- this column existed (their outcome was not recorded).
--
-- This column exists because of the paid-but-failed policy: the gateway
-- settles payment before forwarding, so an origin failure after settlement
-- is possible. Such payments are not retried and not auto-refunded; they are
-- recorded here, and refunds are an operator decision:
--
--   SELECT tx_hash, payer, amount_micro_usdc, path, created_at
--     FROM payments
--    WHERE origin_status >= 500 OR origin_status IS NULL;
ALTER TABLE payments ADD COLUMN origin_status SMALLINT
    CHECK (origin_status IS NULL OR origin_status BETWEEN 100 AND 599);

CREATE INDEX payments_origin_status_idx ON payments (origin_status)
    WHERE origin_status >= 500;
