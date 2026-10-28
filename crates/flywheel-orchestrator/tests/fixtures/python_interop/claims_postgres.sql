-- Golden fixture: an orchestrator claim-store schema written by the Python
-- PostgresClaimStore (the claims_contract interop seed), dumped with
-- `pg_dump --schema=fw_golden_claims --inserts --no-owner --no-privileges`.
-- claims_contract.rs rewrites the schema name and loads it into a fresh schema.
--
-- PostgreSQL database dump
--


-- Dumped from database version 16.15 (Debian 16.15-1.pgdg13+2)
-- Dumped by pg_dump version 16.15 (Debian 16.15-1.pgdg13+2)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: fw_golden_claims; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA fw_golden_claims;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: graph_snapshot_items; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.graph_snapshot_items (
    snapshot_id bigint NOT NULL,
    task_id text NOT NULL,
    source_kind text,
    source_ref text,
    source_url text,
    source_version text,
    priority integer DEFAULT 0 NOT NULL,
    required_capabilities_json jsonb DEFAULT '[]'::jsonb NOT NULL,
    conflict_keys_json jsonb DEFAULT '[]'::jsonb NOT NULL,
    state text NOT NULL,
    ready boolean NOT NULL,
    claim_holder text,
    resolved_prerequisites_json jsonb DEFAULT '[]'::jsonb NOT NULL
);


--
-- Name: graph_snapshots; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.graph_snapshots (
    id bigint NOT NULL,
    captured_at timestamp with time zone NOT NULL,
    item_count integer NOT NULL,
    last_event_id bigint NOT NULL
);


--
-- Name: graph_snapshots_id_seq; Type: SEQUENCE; Schema: fw_golden_claims; Owner: -
--

CREATE SEQUENCE fw_golden_claims.graph_snapshots_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: graph_snapshots_id_seq; Type: SEQUENCE OWNED BY; Schema: fw_golden_claims; Owner: -
--

ALTER SEQUENCE fw_golden_claims.graph_snapshots_id_seq OWNED BY fw_golden_claims.graph_snapshots.id;


--
-- Name: orchestrator_events; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.orchestrator_events (
    id bigint NOT NULL,
    task_id text NOT NULL,
    worker_id text NOT NULL,
    event_type text NOT NULL,
    version integer NOT NULL,
    lease_expires_at timestamp with time zone NOT NULL,
    occurred_at timestamp with time zone NOT NULL
);


--
-- Name: orchestrator_events_id_seq; Type: SEQUENCE; Schema: fw_golden_claims; Owner: -
--

CREATE SEQUENCE fw_golden_claims.orchestrator_events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: orchestrator_events_id_seq; Type: SEQUENCE OWNED BY; Schema: fw_golden_claims; Owner: -
--

ALTER SEQUENCE fw_golden_claims.orchestrator_events_id_seq OWNED BY fw_golden_claims.orchestrator_events.id;


--
-- Name: orchestrator_schema_version; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.orchestrator_schema_version (
    id integer NOT NULL,
    version integer NOT NULL,
    CONSTRAINT orchestrator_schema_version_id_check CHECK ((id = 1))
);


--
-- Name: orchestrator_session_pauses; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.orchestrator_session_pauses (
    id bigint NOT NULL,
    run_id text DEFAULT ''::text NOT NULL,
    reset_at timestamp with time zone NOT NULL,
    pause_until timestamp with time zone NOT NULL,
    recorded_at timestamp with time zone NOT NULL
);


--
-- Name: orchestrator_session_pauses_id_seq; Type: SEQUENCE; Schema: fw_golden_claims; Owner: -
--

CREATE SEQUENCE fw_golden_claims.orchestrator_session_pauses_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: orchestrator_session_pauses_id_seq; Type: SEQUENCE OWNED BY; Schema: fw_golden_claims; Owner: -
--

ALTER SEQUENCE fw_golden_claims.orchestrator_session_pauses_id_seq OWNED BY fw_golden_claims.orchestrator_session_pauses.id;


--
-- Name: orchestrator_stop_events; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.orchestrator_stop_events (
    id bigint NOT NULL,
    kind text NOT NULL,
    subject text NOT NULL,
    detail text NOT NULL,
    occurred_at timestamp with time zone NOT NULL,
    run_id text DEFAULT ''::text NOT NULL,
    attribution text DEFAULT ''::text NOT NULL
);


--
-- Name: orchestrator_stop_events_id_seq; Type: SEQUENCE; Schema: fw_golden_claims; Owner: -
--

CREATE SEQUENCE fw_golden_claims.orchestrator_stop_events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: orchestrator_stop_events_id_seq; Type: SEQUENCE OWNED BY; Schema: fw_golden_claims; Owner: -
--

ALTER SEQUENCE fw_golden_claims.orchestrator_stop_events_id_seq OWNED BY fw_golden_claims.orchestrator_stop_events.id;


--
-- Name: source_syncs; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.source_syncs (
    id bigint NOT NULL,
    source_kind text NOT NULL,
    source_name text NOT NULL,
    started_at timestamp with time zone NOT NULL,
    finished_at timestamp with time zone,
    status text NOT NULL,
    observed_count integer DEFAULT 0 NOT NULL,
    error text,
    metadata_json jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: source_syncs_id_seq; Type: SEQUENCE; Schema: fw_golden_claims; Owner: -
--

CREATE SEQUENCE fw_golden_claims.source_syncs_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: source_syncs_id_seq; Type: SEQUENCE OWNED BY; Schema: fw_golden_claims; Owner: -
--

ALTER SEQUENCE fw_golden_claims.source_syncs_id_seq OWNED BY fw_golden_claims.source_syncs.id;


--
-- Name: task_claims; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.task_claims (
    task_id text NOT NULL,
    worker_id text NOT NULL,
    claimed_at timestamp with time zone NOT NULL,
    lease_expires_at timestamp with time zone NOT NULL,
    version integer NOT NULL,
    conflict_keys_json jsonb DEFAULT '[]'::jsonb NOT NULL
);


--
-- Name: work_item_dependencies; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.work_item_dependencies (
    task_id text NOT NULL,
    prerequisite_task_id text NOT NULL,
    created_at timestamp with time zone NOT NULL
);


--
-- Name: work_items; Type: TABLE; Schema: fw_golden_claims; Owner: -
--

CREATE TABLE fw_golden_claims.work_items (
    task_id text NOT NULL,
    source_kind text,
    source_ref text,
    source_url text,
    source_version text,
    task_content_hash text,
    priority integer DEFAULT 0 NOT NULL,
    required_capabilities_json jsonb DEFAULT '[]'::jsonb NOT NULL,
    conflict_keys_json jsonb DEFAULT '[]'::jsonb NOT NULL,
    first_seen_at timestamp with time zone NOT NULL,
    last_seen_at timestamp with time zone NOT NULL,
    disappeared_at timestamp with time zone,
    metadata_json jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: graph_snapshots id; Type: DEFAULT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.graph_snapshots ALTER COLUMN id SET DEFAULT nextval('fw_golden_claims.graph_snapshots_id_seq'::regclass);


--
-- Name: orchestrator_events id; Type: DEFAULT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.orchestrator_events ALTER COLUMN id SET DEFAULT nextval('fw_golden_claims.orchestrator_events_id_seq'::regclass);


--
-- Name: orchestrator_session_pauses id; Type: DEFAULT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.orchestrator_session_pauses ALTER COLUMN id SET DEFAULT nextval('fw_golden_claims.orchestrator_session_pauses_id_seq'::regclass);


--
-- Name: orchestrator_stop_events id; Type: DEFAULT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.orchestrator_stop_events ALTER COLUMN id SET DEFAULT nextval('fw_golden_claims.orchestrator_stop_events_id_seq'::regclass);


--
-- Name: source_syncs id; Type: DEFAULT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.source_syncs ALTER COLUMN id SET DEFAULT nextval('fw_golden_claims.source_syncs_id_seq'::regclass);


--
-- Data for Name: graph_snapshot_items; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.graph_snapshot_items VALUES (1, 'task-a', 'directory', '/tasks/a.json', NULL, 'hash-a', 5, '["gpu", "linux"]', '["resource-x", "resource-y"]', 'running', true, 'py-worker', '[]');
INSERT INTO fw_golden_claims.graph_snapshot_items VALUES (1, 'task-b', NULL, NULL, NULL, NULL, 0, '[]', '[]', 'blocked', false, NULL, '["task-a", "task-z"]');


--
-- Data for Name: graph_snapshots; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.graph_snapshots VALUES (1, '2026-05-28 12:00:13+00', 2, 8);


--
-- Data for Name: orchestrator_events; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.orchestrator_events VALUES (1, 'task-a', 'py-worker', 'acquired', 1, '2026-05-28 12:00:30.123456+00', '2026-05-28 12:00:00.123456+00');
INSERT INTO fw_golden_claims.orchestrator_events VALUES (2, 'task-b', 'py-worker', 'acquired', 1, '2026-05-28 12:00:31+00', '2026-05-28 12:00:01+00');
INSERT INTO fw_golden_claims.orchestrator_events VALUES (3, 'task-a', 'py-worker', 'renewed', 2, '2026-05-28 12:00:35+00', '2026-05-28 12:00:05+00');
INSERT INTO fw_golden_claims.orchestrator_events VALUES (4, 'task-b', 'py-worker', 'released', 1, '2026-05-28 12:00:31+00', '2026-05-28 12:00:06+00');
INSERT INTO fw_golden_claims.orchestrator_events VALUES (5, 'task-c', 'py-other', 'acquired', 1, '2026-05-28 12:00:10+00', '2026-05-28 12:00:00+00');
INSERT INTO fw_golden_claims.orchestrator_events VALUES (6, 'task-c', 'py-worker', 'stolen', 2, '2026-05-28 12:00:41+00', '2026-05-28 12:00:11+00');
INSERT INTO fw_golden_claims.orchestrator_events VALUES (7, 'task-d', 'py-dead', 'acquired', 1, '2026-05-28 12:00:05+00', '2026-05-28 12:00:00+00');
INSERT INTO fw_golden_claims.orchestrator_events VALUES (8, 'task-d', 'py-dead', 'expired', 1, '2026-05-28 12:00:05+00', '2026-05-28 12:00:12+00');


--
-- Data for Name: orchestrator_schema_version; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.orchestrator_schema_version VALUES (1, 9);


--
-- Data for Name: orchestrator_session_pauses; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.orchestrator_session_pauses VALUES (1, 'run-1', '2026-05-28 12:00:50+00', '2026-05-28 12:00:36+00', '2026-05-28 12:00:16+00');


--
-- Data for Name: orchestrator_stop_events; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.orchestrator_stop_events VALUES (1, 'dangling-prerequisite', 'task-b', 'needs task-z', '2026-05-28 12:00:14+00', '', '');
INSERT INTO fw_golden_claims.orchestrator_stop_events VALUES (2, 'awaiting-approval', 'task-a', 'manual gate', '2026-05-28 12:00:15+00', 'run-1', '');


--
-- Data for Name: source_syncs; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.source_syncs VALUES (1, 'directory', '/tasks', '2026-05-28 12:00:18+00', '2026-05-28 12:00:19+00', 'ok', 1, NULL, '{}');


--
-- Data for Name: task_claims; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.task_claims VALUES ('task-a', 'py-worker', '2026-05-28 12:00:00.123456+00', '2026-05-28 12:00:35+00', 2, '["resource-x", "resource-y"]');
INSERT INTO fw_golden_claims.task_claims VALUES ('task-c', 'py-worker', '2026-05-28 12:00:11+00', '2026-05-28 12:00:41+00', 2, '[]');


--
-- Data for Name: work_item_dependencies; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.work_item_dependencies VALUES ('task-w', 'task-a', '2026-05-28 12:00:17+00');
INSERT INTO fw_golden_claims.work_item_dependencies VALUES ('task-w', 'task-b', '2026-05-28 12:00:17+00');


--
-- Data for Name: work_items; Type: TABLE DATA; Schema: fw_golden_claims; Owner: -
--

INSERT INTO fw_golden_claims.work_items VALUES ('task-w', 'directory', '/tasks/w.json', NULL, 'v1', '8834826824ec343fe0d810e8ccdad1a64f1abb3fe50e4fe1fac231b7d6538dd7', 3, '["linux"]', '["resource-x"]', '2026-05-28 12:00:17+00', '2026-05-28 12:00:17+00', NULL, '{}');


--
-- Name: graph_snapshots_id_seq; Type: SEQUENCE SET; Schema: fw_golden_claims; Owner: -
--

SELECT pg_catalog.setval('fw_golden_claims.graph_snapshots_id_seq', 1, true);


--
-- Name: orchestrator_events_id_seq; Type: SEQUENCE SET; Schema: fw_golden_claims; Owner: -
--

SELECT pg_catalog.setval('fw_golden_claims.orchestrator_events_id_seq', 8, true);


--
-- Name: orchestrator_session_pauses_id_seq; Type: SEQUENCE SET; Schema: fw_golden_claims; Owner: -
--

SELECT pg_catalog.setval('fw_golden_claims.orchestrator_session_pauses_id_seq', 1, true);


--
-- Name: orchestrator_stop_events_id_seq; Type: SEQUENCE SET; Schema: fw_golden_claims; Owner: -
--

SELECT pg_catalog.setval('fw_golden_claims.orchestrator_stop_events_id_seq', 2, true);


--
-- Name: source_syncs_id_seq; Type: SEQUENCE SET; Schema: fw_golden_claims; Owner: -
--

SELECT pg_catalog.setval('fw_golden_claims.source_syncs_id_seq', 1, true);


--
-- Name: graph_snapshot_items graph_snapshot_items_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.graph_snapshot_items
    ADD CONSTRAINT graph_snapshot_items_pkey PRIMARY KEY (snapshot_id, task_id);


--
-- Name: graph_snapshots graph_snapshots_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.graph_snapshots
    ADD CONSTRAINT graph_snapshots_pkey PRIMARY KEY (id);


--
-- Name: orchestrator_events orchestrator_events_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.orchestrator_events
    ADD CONSTRAINT orchestrator_events_pkey PRIMARY KEY (id);


--
-- Name: orchestrator_schema_version orchestrator_schema_version_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.orchestrator_schema_version
    ADD CONSTRAINT orchestrator_schema_version_pkey PRIMARY KEY (id);


--
-- Name: orchestrator_session_pauses orchestrator_session_pauses_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.orchestrator_session_pauses
    ADD CONSTRAINT orchestrator_session_pauses_pkey PRIMARY KEY (id);


--
-- Name: orchestrator_stop_events orchestrator_stop_events_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.orchestrator_stop_events
    ADD CONSTRAINT orchestrator_stop_events_pkey PRIMARY KEY (id);


--
-- Name: source_syncs source_syncs_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.source_syncs
    ADD CONSTRAINT source_syncs_pkey PRIMARY KEY (id);


--
-- Name: task_claims task_claims_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.task_claims
    ADD CONSTRAINT task_claims_pkey PRIMARY KEY (task_id);


--
-- Name: work_item_dependencies work_item_dependencies_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.work_item_dependencies
    ADD CONSTRAINT work_item_dependencies_pkey PRIMARY KEY (task_id, prerequisite_task_id);


--
-- Name: work_items work_items_pkey; Type: CONSTRAINT; Schema: fw_golden_claims; Owner: -
--

ALTER TABLE ONLY fw_golden_claims.work_items
    ADD CONSTRAINT work_items_pkey PRIMARY KEY (task_id);


--
-- Name: idx_graph_snapshot_items_snapshot; Type: INDEX; Schema: fw_golden_claims; Owner: -
--

CREATE INDEX idx_graph_snapshot_items_snapshot ON fw_golden_claims.graph_snapshot_items USING btree (snapshot_id);


--
-- Name: idx_orchestrator_events_task; Type: INDEX; Schema: fw_golden_claims; Owner: -
--

CREATE INDEX idx_orchestrator_events_task ON fw_golden_claims.orchestrator_events USING btree (task_id, id);


--
-- Name: idx_orchestrator_session_pauses_pause_until; Type: INDEX; Schema: fw_golden_claims; Owner: -
--

CREATE INDEX idx_orchestrator_session_pauses_pause_until ON fw_golden_claims.orchestrator_session_pauses USING btree (pause_until);


--
-- Name: idx_orchestrator_stop_events_subject; Type: INDEX; Schema: fw_golden_claims; Owner: -
--

CREATE INDEX idx_orchestrator_stop_events_subject ON fw_golden_claims.orchestrator_stop_events USING btree (subject, id);


--
-- Name: idx_work_item_dependencies_prerequisite; Type: INDEX; Schema: fw_golden_claims; Owner: -
--

CREATE INDEX idx_work_item_dependencies_prerequisite ON fw_golden_claims.work_item_dependencies USING btree (prerequisite_task_id);


--
-- PostgreSQL database dump complete
--


