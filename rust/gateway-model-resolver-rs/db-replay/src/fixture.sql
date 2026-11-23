
CREATE TABLE providerNodes (id TEXT PRIMARY KEY, type TEXT, data TEXT NOT NULL);
CREATE INDEX idx_pn_type ON providerNodes(type);
CREATE TABLE combos (name TEXT PRIMARY KEY, models TEXT NOT NULL);
CREATE TABLE kv (scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (scope,key));
CREATE TABLE providerConnections (provider TEXT NOT NULL, isActive INTEGER NOT NULL);
INSERT INTO providerNodes VALUES
 ('anthropic-first','anthropic-compatible','{"prefix":"loc"}'),
 ('openai-first','openai-compatible','{"prefix":"loc"}'),
 ('openai-second','openai-compatible','{"prefix":"loc"}'),
 ('embedding-first','custom-embedding','{"prefix":"loc"}'),
 ('embedding-only','custom-embedding','{"prefix":"embedding-test"}'),
 ('bad-node','openai-compatible','not-json'),
 ('reserved-cx','openai-compatible','{"prefix":"cx"}');
INSERT INTO combos VALUES
 ('Main','["cx/gpt-5.6","gemini/a"]'),
 ('Empty','[]'),
 ('Text','"a"'),
 ('Malformed','not-json'),
 ('Object','{"length":1}');
INSERT INTO kv VALUES
 ('modelAliases','Main','"gemini/shadowed"'),
 ('modelAliases','Empty','"gemini/shadowed"'),
 ('modelAliases','bad','not-json'),
 ('modelAliases','nested','"cx/vendor/model"'),
 ('modelAliases','provider-object','{"provider":"ag","model":"other"}'),
 ('modelAliases','claude-slot','"anthropic/claude-sonnet-4.6"'),
 ('modelAliases','local-alias','"loc/viewer"'),
 ('modelAliases','empty-model','"openai/"');
INSERT INTO providerConnections VALUES ('claude',1), ('anthropic',0);
