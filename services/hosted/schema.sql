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
