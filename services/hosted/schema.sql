-- PackSync hosted storage: accounts, registration limits and pairing mailboxes.
CREATE TABLE IF NOT EXISTS accounts (
  id TEXT PRIMARY KEY,
  token_hash TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  last_seen_ms INTEGER NOT NULL,
  used INTEGER NOT NULL DEFAULT 0,
  quota INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS registrations (
  ip_hash TEXT NOT NULL,
  day INTEGER NOT NULL,
  n INTEGER NOT NULL,
  PRIMARY KEY (ip_hash, day)
);
CREATE TABLE IF NOT EXISTS pairings (
  nameplate TEXT PRIMARY KEY,
  account TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  a TEXT, b TEXT, c TEXT, d TEXT
);
-- Passkey recovery: the account's credentials and encryption key, sealed with a key only the passkey can
-- produce (WebAuthn PRF). The service cannot open it; it only hands it to whoever names the passkey.
CREATE TABLE IF NOT EXISTS passkeys (
  credential_id TEXT PRIMARY KEY,
  account TEXT NOT NULL,
  blob TEXT NOT NULL,
  label TEXT NOT NULL,
  created_ms INTEGER NOT NULL
);
-- The size of every stored object. Space is charged and released from this, in one transaction per
-- change, so overlapping writes or deletes of the same object never count it twice.
CREATE TABLE IF NOT EXISTS objects (
  account TEXT NOT NULL,
  key TEXT NOT NULL,
  size INTEGER NOT NULL,
  PRIMARY KEY (account, key)
);
-- Accounts whose objects are all in `objects` (older accounts are counted from the bucket once).
CREATE TABLE IF NOT EXISTS counted (account TEXT PRIMARY KEY);
