-- what a client reported about the WebAuthn credential behind an account key, so the next
-- sign-in knows which authenticators to offer. written once the bundle authorizing the key is
-- accepted, and read by wallet_getKeys only for keys the account holds.
create table credential_metadata (
    address bytea not null,
    key_hash bytea not null,
    credential_id text,
    attachment text,
    transports text[] not null default '{}',
    created_at timestamp not null default now(),
    primary key (address, key_hash)
);
