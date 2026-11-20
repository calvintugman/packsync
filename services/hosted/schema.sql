-- PackSync hosted storage: accounts, multi-device sessions, rate/op limits, and pairing mailboxes.
CREATE TABLE IF NOT EXISTS accounts (
  id TEXT PRIMARY KEY,
  token_hash TEXT NOT NULL,
  ms_hash TEXT UNIQUE,
  wrapped_key TEXT,
  created_ms INTEGER NOT NULL,
  last_seen_ms INTEGER NOT NULL,
  used INTEGER NOT NULL DEFAULT 0,
  quota INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS accounts_used ON accounts(used);
CREATE INDEX IF NOT EXISTS accounts_last_seen ON accounts(last_seen_ms);
CREATE TABLE IF NOT EXISTS sessions (
  account TEXT NOT NULL,
  token_hash TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  last_seen_ms INTEGER NOT NULL,
  PRIMARY KEY (account, token_hash)
);
CREATE INDEX IF NOT EXISTS sessions_by_account ON sessions(account);
CREATE TABLE IF NOT EXISTS registrations (
  ip_hash TEXT NOT NULL,
  day INTEGER NOT NULL,
  n INTEGER NOT NULL,
  PRIMARY KEY (ip_hash, day)
);
CREATE TABLE IF NOT EXISTS op_usage (
  account TEXT NOT NULL,
  day INTEGER NOT NULL,
  writes INTEGER NOT NULL DEFAULT 0,
  reads INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (account, day)
);
CREATE TABLE IF NOT EXISTS pairings (
  nameplate TEXT PRIMARY KEY,
  account TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  a TEXT, b TEXT, c TEXT, d TEXT
);
CREATE INDEX IF NOT EXISTS pairings_by_account ON pairings(account);
CREATE INDEX IF NOT EXISTS pairings_by_created ON pairings(created_ms);
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
