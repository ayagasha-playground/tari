CREATE TABLE peers (
    public_key BLOB PRIMARY KEY NOT NULL UNIQUE,
    peer       TEXT             NOT NULL
);
