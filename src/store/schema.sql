BEGIN;

CREATE TABLE IF NOT EXISTS action (
    id           TEXT NOT NULL PRIMARY KEY,
    kind         TEXT NOT NULL,
    turn         TEXT,
    step         INTEGER,
    requested_by TEXT,
    call_id      TEXT,
    service      TEXT,
    interaction  TEXT,
    root         TEXT,
    input        TEXT NOT NULL,
    output       TEXT,
    hazardness   TEXT,
    reasoning    TEXT,
    state        TEXT NOT NULL,
    requested_at TEXT NOT NULL,
    ended_at     TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS action_receive_interaction
    ON action (interaction) WHERE kind = 'receive';

CREATE INDEX IF NOT EXISTS action_turn_id ON action (turn, id);

CREATE INDEX IF NOT EXISTS action_unfinished
    ON action (id) WHERE state IN ('requested', 'running');

CREATE TABLE IF NOT EXISTS turn (
    trigger   TEXT NOT NULL PRIMARY KEY,
    root      TEXT NOT NULL,
    state     TEXT NOT NULL,
    steps     INTEGER NOT NULL,
    opened_at TEXT NOT NULL,
    ended_at  TEXT
);

CREATE TABLE IF NOT EXISTS mark (
    target TEXT NOT NULL,
    key    TEXT NOT NULL,
    value  TEXT NOT NULL,
    set_by TEXT NOT NULL,
    set_at TEXT NOT NULL,
    PRIMARY KEY (target, key)
);

CREATE VIEW IF NOT EXISTS v1_action AS
SELECT id, kind, turn, step, requested_by, call_id, service, interaction, root,
       input, output, hazardness, reasoning, state, requested_at, ended_at
FROM action;

CREATE VIEW IF NOT EXISTS v1_turn AS
SELECT trigger, root, state, steps, opened_at, ended_at FROM turn;

CREATE VIEW IF NOT EXISTS v1_mark AS
SELECT target, key, value, set_by, set_at FROM mark;

COMMIT;
