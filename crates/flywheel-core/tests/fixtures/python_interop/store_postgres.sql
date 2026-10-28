-- Golden fixture: a flywheel-core schema written by the Python PostgresStore
-- (the store_contract interop seed), dumped with
-- `pg_dump --schema=fw_golden_store --inserts --no-owner --no-privileges`.
-- store_contract.rs rewrites the schema name and loads it into a fresh schema.
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
-- Name: fw_golden_store; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA fw_golden_store;


--
-- Name: grader_results_reject_mutation(); Type: FUNCTION; Schema: fw_golden_store; Owner: -
--

CREATE FUNCTION fw_golden_store.grader_results_reject_mutation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
  RAISE EXCEPTION 'grader_results is append-only; % is not permitted', TG_OP
    USING ERRCODE = 'check_violation';
END;
$$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: attempts; Type: TABLE; Schema: fw_golden_store; Owner: -
--

CREATE TABLE fw_golden_store.attempts (
    run_id text NOT NULL,
    number integer NOT NULL,
    attempt_run_id text,
    started_at timestamp with time zone NOT NULL,
    ended_at timestamp with time zone,
    outcome text,
    agent_output text,
    error text,
    agent_context_json jsonb,
    input_tokens bigint DEFAULT 0 NOT NULL,
    output_tokens bigint DEFAULT 0 NOT NULL,
    cache_creation_input_tokens bigint DEFAULT 0 NOT NULL,
    cache_read_input_tokens bigint DEFAULT 0 NOT NULL,
    iterations_completed integer DEFAULT 0 NOT NULL,
    turns integer DEFAULT 0 NOT NULL,
    total_cost_usd double precision DEFAULT 0 NOT NULL,
    last_activity_at timestamp with time zone
);


--
-- Name: control_commands; Type: TABLE; Schema: fw_golden_store; Owner: -
--

CREATE TABLE fw_golden_store.control_commands (
    id bigint NOT NULL,
    run_id text NOT NULL,
    kind text NOT NULL,
    payload_json jsonb NOT NULL,
    enqueued_at timestamp with time zone NOT NULL,
    claimed_at timestamp with time zone
);


--
-- Name: control_commands_id_seq; Type: SEQUENCE; Schema: fw_golden_store; Owner: -
--

CREATE SEQUENCE fw_golden_store.control_commands_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: control_commands_id_seq; Type: SEQUENCE OWNED BY; Schema: fw_golden_store; Owner: -
--

ALTER SEQUENCE fw_golden_store.control_commands_id_seq OWNED BY fw_golden_store.control_commands.id;


--
-- Name: events; Type: TABLE; Schema: fw_golden_store; Owner: -
--

CREATE TABLE fw_golden_store.events (
    id bigint NOT NULL,
    run_id text NOT NULL,
    attempt_number integer,
    ts timestamp with time zone NOT NULL,
    kind text NOT NULL,
    payload_json jsonb NOT NULL,
    sequence bigint NOT NULL
);


--
-- Name: events_id_seq; Type: SEQUENCE; Schema: fw_golden_store; Owner: -
--

CREATE SEQUENCE fw_golden_store.events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: events_id_seq; Type: SEQUENCE OWNED BY; Schema: fw_golden_store; Owner: -
--

ALTER SEQUENCE fw_golden_store.events_id_seq OWNED BY fw_golden_store.events.id;


--
-- Name: grader_results; Type: TABLE; Schema: fw_golden_store; Owner: -
--

CREATE TABLE fw_golden_store.grader_results (
    id bigint NOT NULL,
    run_id text NOT NULL,
    attempt_number integer NOT NULL,
    ordinal integer NOT NULL,
    grader_type text NOT NULL,
    grader_name text,
    grader_spec_json jsonb NOT NULL,
    passed boolean NOT NULL,
    duration_ms integer NOT NULL,
    payload_json jsonb NOT NULL,
    ts timestamp with time zone NOT NULL
);


--
-- Name: grader_results_id_seq; Type: SEQUENCE; Schema: fw_golden_store; Owner: -
--

CREATE SEQUENCE fw_golden_store.grader_results_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: grader_results_id_seq; Type: SEQUENCE OWNED BY; Schema: fw_golden_store; Owner: -
--

ALTER SEQUENCE fw_golden_store.grader_results_id_seq OWNED BY fw_golden_store.grader_results.id;


--
-- Name: lifecycles; Type: TABLE; Schema: fw_golden_store; Owner: -
--

CREATE TABLE fw_golden_store.lifecycles (
    run_id text NOT NULL,
    task_id text NOT NULL,
    status text NOT NULL,
    version integer NOT NULL,
    retries integer NOT NULL,
    error text,
    agent_output text,
    session_id text,
    artifacts_dir text,
    worker_id text,
    timestamps_json jsonb NOT NULL,
    updated_at timestamp with time zone NOT NULL,
    blocked_requires_json text,
    task_content_hash text,
    awaiting_manual_ordinal integer,
    source text
);


--
-- Name: schema_version; Type: TABLE; Schema: fw_golden_store; Owner: -
--

CREATE TABLE fw_golden_store.schema_version (
    id integer NOT NULL,
    version integer NOT NULL,
    CONSTRAINT schema_version_id_check CHECK ((id = 1))
);


--
-- Name: task_versions; Type: TABLE; Schema: fw_golden_store; Owner: -
--

CREATE TABLE fw_golden_store.task_versions (
    task_id text NOT NULL,
    content_hash text NOT NULL,
    goal text NOT NULL,
    graders_json jsonb NOT NULL,
    tags_json jsonb NOT NULL,
    context_json jsonb NOT NULL,
    created_at timestamp with time zone NOT NULL,
    insertion_seq bigint NOT NULL
);


--
-- Name: task_versions_insertion_seq_seq; Type: SEQUENCE; Schema: fw_golden_store; Owner: -
--

ALTER TABLE fw_golden_store.task_versions ALTER COLUMN insertion_seq ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME fw_golden_store.task_versions_insertion_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: tasks; Type: TABLE; Schema: fw_golden_store; Owner: -
--

CREATE TABLE fw_golden_store.tasks (
    id text NOT NULL,
    first_seen_at timestamp with time zone NOT NULL,
    updated_at timestamp with time zone NOT NULL
);


--
-- Name: control_commands id; Type: DEFAULT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.control_commands ALTER COLUMN id SET DEFAULT nextval('fw_golden_store.control_commands_id_seq'::regclass);


--
-- Name: events id; Type: DEFAULT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.events ALTER COLUMN id SET DEFAULT nextval('fw_golden_store.events_id_seq'::regclass);


--
-- Name: grader_results id; Type: DEFAULT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.grader_results ALTER COLUMN id SET DEFAULT nextval('fw_golden_store.grader_results_id_seq'::regclass);


--
-- Data for Name: attempts; Type: TABLE DATA; Schema: fw_golden_store; Owner: -
--

INSERT INTO fw_golden_store.attempts VALUES ('run-interop', 1, 'run-interop', '2026-05-28 12:00:03+00', '2026-05-28 12:00:07+00', 'succeeded', 'ported', NULL, '{"model_id": "claude-opus-4-8", "base_commit_sha": "abc123"}', 1200, 300, 40, 5, 2, 6, 0.75, '2026-05-28 12:00:04+00');


--
-- Data for Name: control_commands; Type: TABLE DATA; Schema: fw_golden_store; Owner: -
--

INSERT INTO fw_golden_store.control_commands VALUES (1, 'run-interop', 'say', '{"text": "focus"}', '2026-05-28 12:00:10+00', '2026-05-28 12:00:11+00');
INSERT INTO fw_golden_store.control_commands VALUES (2, 'run-interop', 'interrupt', '{}', '2026-05-28 12:00:12+00', NULL);


--
-- Data for Name: events; Type: TABLE DATA; Schema: fw_golden_store; Owner: -
--

INSERT INTO fw_golden_store.events VALUES (1, 'run-interop', NULL, '2026-05-28 12:00:00+00', 'lifecycle_initialized', '{"source": "owner/repo#42", "task_id": "interop-task", "worker_id": "w-interop", "artifacts_dir": "/artifacts", "task_content_hash": "e3f7d2a2191dc72871eb3b7b6049e4617b9790d20dc5846dc7d54dbcc88e6ea3"}', 1);
INSERT INTO fw_golden_store.events VALUES (2, 'run-interop', NULL, '2026-05-28 12:00:01+00', 'transitioned_to', '{"error": "", "target": "ready"}', 2);
INSERT INTO fw_golden_store.events VALUES (3, 'run-interop', NULL, '2026-05-28 12:00:02+00', 'transitioned_to', '{"error": "", "target": "running"}', 3);
INSERT INTO fw_golden_store.events VALUES (4, 'run-interop', 1, '2026-05-28 12:00:03+00', 'attempt_started', '{"number": 1, "started_at": "2026-05-28T12:00:03+00:00", "agent_context": {"model_id": "claude-opus-4-8", "base_commit_sha": "abc123"}, "attempt_run_id": "run-interop"}', 4);
INSERT INTO fw_golden_store.events VALUES (5, 'run-interop', NULL, '2026-05-28 12:00:05+00', 'transitioned_to', '{"error": "", "target": "validating"}', 5);
INSERT INTO fw_golden_store.events VALUES (6, 'run-interop', 1, '2026-05-28 12:00:06+00', 'grader_evaluated', '{"passed": true, "ordinal": 0, "payload": {"exit_code": 0, "stdout_tail": "ok"}, "duration_ms": 7, "grader_name": "tests", "grader_spec": {"run": "cargo test", "name": "tests", "type": "command"}, "grader_type": "command"}', 6);
INSERT INTO fw_golden_store.events VALUES (7, 'run-interop', 1, '2026-05-28 12:00:07+00', 'attempt_finalized', '{"error": "", "number": 1, "outcome": "succeeded", "ended_at": "2026-05-28T12:00:07+00:00", "agent_output": "ported"}', 7);
INSERT INTO fw_golden_store.events VALUES (8, 'run-interop', NULL, '2026-05-28 12:00:08+00', 'transitioned_to', '{"error": "", "target": "done"}', 8);
INSERT INTO fw_golden_store.events VALUES (9, 'run-interop', NULL, '2026-05-28 12:00:09+00', 'landing_parked', '{"detail": "base advanced", "park_kind": "divergent-base"}', 9);


--
-- Data for Name: grader_results; Type: TABLE DATA; Schema: fw_golden_store; Owner: -
--

INSERT INTO fw_golden_store.grader_results VALUES (1, 'run-interop', 1, 0, 'command', 'tests', '{"run": "cargo test", "name": "tests", "type": "command"}', true, 7, '{"exit_code": 0, "stdout_tail": "ok"}', '2026-05-28 12:00:06+00');


--
-- Data for Name: lifecycles; Type: TABLE DATA; Schema: fw_golden_store; Owner: -
--

INSERT INTO fw_golden_store.lifecycles VALUES ('run-interop', 'interop-task', 'done', 9, 0, NULL, 'ported', NULL, '/artifacts', 'w-interop', '{"done": "2026-05-28T12:00:08+00:00", "ready": "2026-05-28T12:00:01+00:00", "running": "2026-05-28T12:00:02+00:00", "validating": "2026-05-28T12:00:05+00:00"}', '2026-10-28 14:45:38.77778+00', NULL, 'e3f7d2a2191dc72871eb3b7b6049e4617b9790d20dc5846dc7d54dbcc88e6ea3', NULL, 'owner/repo#42');


--
-- Data for Name: schema_version; Type: TABLE DATA; Schema: fw_golden_store; Owner: -
--

INSERT INTO fw_golden_store.schema_version VALUES (1, 12);


--
-- Data for Name: task_versions; Type: TABLE DATA; Schema: fw_golden_store; Owner: -
--

INSERT INTO fw_golden_store.task_versions OVERRIDING SYSTEM VALUE VALUES ('interop-task', 'e3f7d2a2191dc72871eb3b7b6049e4617b9790d20dc5846dc7d54dbcc88e6ea3', 'Port the stores.', '[{"run": "cargo test", "name": "tests", "type": "command"}, {"name": "review", "type": "rubric", "assertions": ["It works", "It is idiomatic"], "retry_on_fail": true}]', '["rust", "stores"]', '{"notes": "Keep parity.", "relevant": ["crates/flywheel-core"], "non_goals": [], "edge_cases": [], "references": [], "constraints": []}', '2026-05-28 12:00:00+00', 1);


--
-- Data for Name: tasks; Type: TABLE DATA; Schema: fw_golden_store; Owner: -
--

INSERT INTO fw_golden_store.tasks VALUES ('interop-task', '2026-05-28 12:00:00+00', '2026-05-28 12:00:00+00');


--
-- Name: control_commands_id_seq; Type: SEQUENCE SET; Schema: fw_golden_store; Owner: -
--

SELECT pg_catalog.setval('fw_golden_store.control_commands_id_seq', 2, true);


--
-- Name: events_id_seq; Type: SEQUENCE SET; Schema: fw_golden_store; Owner: -
--

SELECT pg_catalog.setval('fw_golden_store.events_id_seq', 9, true);


--
-- Name: grader_results_id_seq; Type: SEQUENCE SET; Schema: fw_golden_store; Owner: -
--

SELECT pg_catalog.setval('fw_golden_store.grader_results_id_seq', 1, true);


--
-- Name: task_versions_insertion_seq_seq; Type: SEQUENCE SET; Schema: fw_golden_store; Owner: -
--

SELECT pg_catalog.setval('fw_golden_store.task_versions_insertion_seq_seq', 1, true);


--
-- Name: attempts attempts_pkey; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.attempts
    ADD CONSTRAINT attempts_pkey PRIMARY KEY (run_id, number);


--
-- Name: control_commands control_commands_pkey; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.control_commands
    ADD CONSTRAINT control_commands_pkey PRIMARY KEY (id);


--
-- Name: events events_pkey; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.events
    ADD CONSTRAINT events_pkey PRIMARY KEY (id);


--
-- Name: events events_run_id_sequence_key; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.events
    ADD CONSTRAINT events_run_id_sequence_key UNIQUE (run_id, sequence);


--
-- Name: grader_results grader_results_pkey; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.grader_results
    ADD CONSTRAINT grader_results_pkey PRIMARY KEY (id);


--
-- Name: lifecycles lifecycles_pkey; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.lifecycles
    ADD CONSTRAINT lifecycles_pkey PRIMARY KEY (run_id);


--
-- Name: schema_version schema_version_pkey; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.schema_version
    ADD CONSTRAINT schema_version_pkey PRIMARY KEY (id);


--
-- Name: task_versions task_versions_pkey; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.task_versions
    ADD CONSTRAINT task_versions_pkey PRIMARY KEY (task_id, content_hash);


--
-- Name: tasks tasks_pkey; Type: CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.tasks
    ADD CONSTRAINT tasks_pkey PRIMARY KEY (id);


--
-- Name: idx_control_commands_pending; Type: INDEX; Schema: fw_golden_store; Owner: -
--

CREATE INDEX idx_control_commands_pending ON fw_golden_store.control_commands USING btree (run_id, id) WHERE (claimed_at IS NULL);


--
-- Name: idx_grader_results_run_attempt; Type: INDEX; Schema: fw_golden_store; Owner: -
--

CREATE INDEX idx_grader_results_run_attempt ON fw_golden_store.grader_results USING btree (run_id, attempt_number, ordinal);


--
-- Name: idx_task_versions_id_created; Type: INDEX; Schema: fw_golden_store; Owner: -
--

CREATE INDEX idx_task_versions_id_created ON fw_golden_store.task_versions USING btree (task_id, created_at);


--
-- Name: grader_results grader_results_no_delete; Type: TRIGGER; Schema: fw_golden_store; Owner: -
--

CREATE TRIGGER grader_results_no_delete BEFORE DELETE ON fw_golden_store.grader_results FOR EACH ROW EXECUTE FUNCTION fw_golden_store.grader_results_reject_mutation();


--
-- Name: grader_results grader_results_no_update; Type: TRIGGER; Schema: fw_golden_store; Owner: -
--

CREATE TRIGGER grader_results_no_update BEFORE UPDATE ON fw_golden_store.grader_results FOR EACH ROW EXECUTE FUNCTION fw_golden_store.grader_results_reject_mutation();


--
-- Name: attempts attempts_run_id_fkey; Type: FK CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.attempts
    ADD CONSTRAINT attempts_run_id_fkey FOREIGN KEY (run_id) REFERENCES fw_golden_store.lifecycles(run_id);


--
-- Name: control_commands control_commands_run_id_fkey; Type: FK CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.control_commands
    ADD CONSTRAINT control_commands_run_id_fkey FOREIGN KEY (run_id) REFERENCES fw_golden_store.lifecycles(run_id);


--
-- Name: events events_run_id_fkey; Type: FK CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.events
    ADD CONSTRAINT events_run_id_fkey FOREIGN KEY (run_id) REFERENCES fw_golden_store.lifecycles(run_id);


--
-- Name: grader_results grader_results_run_id_attempt_number_fkey; Type: FK CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.grader_results
    ADD CONSTRAINT grader_results_run_id_attempt_number_fkey FOREIGN KEY (run_id, attempt_number) REFERENCES fw_golden_store.attempts(run_id, number);


--
-- Name: lifecycles lifecycles_task_id_fkey; Type: FK CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.lifecycles
    ADD CONSTRAINT lifecycles_task_id_fkey FOREIGN KEY (task_id) REFERENCES fw_golden_store.tasks(id);


--
-- Name: task_versions task_versions_task_id_fkey; Type: FK CONSTRAINT; Schema: fw_golden_store; Owner: -
--

ALTER TABLE ONLY fw_golden_store.task_versions
    ADD CONSTRAINT task_versions_task_id_fkey FOREIGN KEY (task_id) REFERENCES fw_golden_store.tasks(id);


--
-- PostgreSQL database dump complete
--


