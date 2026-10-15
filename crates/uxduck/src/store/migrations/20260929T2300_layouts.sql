-- S6: each operator's layout, as JSON (the map placement and which panels
-- are open). Operators projected before this have the default, Ask.

ALTER TABLE operators ADD COLUMN layout TEXT NOT NULL DEFAULT '{"map":"hidden","panel":false,"chatDrawer":false}';
