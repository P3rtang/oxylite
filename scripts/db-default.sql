--
-- PostgreSQL database dump
--

-- Dumped from database version 16.9 (Debian 16.9-1.pgdg120+1)
-- Dumped by pg_dump version 16.9 (Debian 16.9-1.pgdg120+1)

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

ALTER TABLE IF EXISTS ONLY public.notes DROP CONSTRAINT IF EXISTS notes_pkey;
ALTER TABLE IF EXISTS ONLY public._sqlx_migrations DROP CONSTRAINT IF EXISTS _sqlx_migrations_pkey;
ALTER TABLE IF EXISTS ONLY oxylite.tombstones DROP CONSTRAINT IF EXISTS tombstones_pkey;
ALTER TABLE IF EXISTS ONLY oxylite.sync_log DROP CONSTRAINT IF EXISTS sync_log_pkey;
ALTER TABLE IF EXISTS ONLY oxylite.snapshots DROP CONSTRAINT IF EXISTS snapshots_pkey;
ALTER TABLE IF EXISTS ONLY oxylite.pending_ops DROP CONSTRAINT IF EXISTS pending_ops_pkey;
ALTER TABLE IF EXISTS ONLY oxylite.meta DROP CONSTRAINT IF EXISTS meta_pkey;
ALTER TABLE IF EXISTS oxylite.sync_log ALTER COLUMN seq DROP DEFAULT;
ALTER TABLE IF EXISTS oxylite.pending_ops ALTER COLUMN seq DROP DEFAULT;
DROP TABLE IF EXISTS public.notes;
DROP TABLE IF EXISTS public._sqlx_migrations;
DROP TABLE IF EXISTS oxylite.tombstones;
DROP SEQUENCE IF EXISTS oxylite.sync_log_seq_seq;
DROP TABLE IF EXISTS oxylite.sync_log;
DROP TABLE IF EXISTS oxylite.snapshots;
DROP SEQUENCE IF EXISTS oxylite.pending_ops_seq_seq;
DROP TABLE IF EXISTS oxylite.pending_ops;
DROP TABLE IF EXISTS oxylite.meta;
DROP SCHEMA IF EXISTS oxylite;
--
-- Name: oxylite; Type: SCHEMA; Schema: -; Owner: sync
--

CREATE SCHEMA oxylite;


ALTER SCHEMA oxylite OWNER TO sync;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: meta; Type: TABLE; Schema: oxylite; Owner: sync
--

CREATE TABLE oxylite.meta (
    key text NOT NULL,
    value text NOT NULL
);


ALTER TABLE oxylite.meta OWNER TO sync;

--
-- Name: pending_ops; Type: TABLE; Schema: oxylite; Owner: sync
--

CREATE TABLE oxylite.pending_ops (
    seq bigint NOT NULL,
    op text NOT NULL
);


ALTER TABLE oxylite.pending_ops OWNER TO sync;

--
-- Name: pending_ops_seq_seq; Type: SEQUENCE; Schema: oxylite; Owner: sync
--

CREATE SEQUENCE oxylite.pending_ops_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE oxylite.pending_ops_seq_seq OWNER TO sync;

--
-- Name: pending_ops_seq_seq; Type: SEQUENCE OWNED BY; Schema: oxylite; Owner: sync
--

ALTER SEQUENCE oxylite.pending_ops_seq_seq OWNED BY oxylite.pending_ops.seq;


--
-- Name: snapshots; Type: TABLE; Schema: oxylite; Owner: sync
--

CREATE TABLE oxylite.snapshots (
    table_name text NOT NULL,
    seq bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb NOT NULL
);


ALTER TABLE oxylite.snapshots OWNER TO sync;

--
-- Name: sync_log; Type: TABLE; Schema: oxylite; Owner: sync
--

CREATE TABLE oxylite.sync_log (
    seq bigint NOT NULL,
    table_name text NOT NULL,
    row_id uuid NOT NULL,
    payload jsonb NOT NULL,
    updated_at timestamp with time zone NOT NULL
);


ALTER TABLE oxylite.sync_log OWNER TO sync;

--
-- Name: sync_log_seq_seq; Type: SEQUENCE; Schema: oxylite; Owner: sync
--

CREATE SEQUENCE oxylite.sync_log_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE oxylite.sync_log_seq_seq OWNER TO sync;

--
-- Name: sync_log_seq_seq; Type: SEQUENCE OWNED BY; Schema: oxylite; Owner: sync
--

ALTER SEQUENCE oxylite.sync_log_seq_seq OWNED BY oxylite.sync_log.seq;


--
-- Name: tombstones; Type: TABLE; Schema: oxylite; Owner: sync
--

CREATE TABLE oxylite.tombstones (
    table_name text NOT NULL,
    id uuid NOT NULL,
    deleted_at timestamp with time zone NOT NULL
);


ALTER TABLE oxylite.tombstones OWNER TO sync;

--
-- Name: _sqlx_migrations; Type: TABLE; Schema: public; Owner: sync
--

CREATE TABLE public._sqlx_migrations (
    version bigint NOT NULL,
    description text NOT NULL,
    installed_on timestamp with time zone DEFAULT now() NOT NULL,
    success boolean NOT NULL,
    checksum bytea NOT NULL,
    execution_time bigint NOT NULL
);


ALTER TABLE public._sqlx_migrations OWNER TO sync;

--
-- Name: notes; Type: TABLE; Schema: public; Owner: sync
--

CREATE TABLE public.notes (
    id uuid NOT NULL,
    title text DEFAULT ''::text NOT NULL,
    body text DEFAULT ''::text NOT NULL,
    updated_at timestamp with time zone NOT NULL
);


ALTER TABLE public.notes OWNER TO sync;

--
-- Name: pending_ops seq; Type: DEFAULT; Schema: oxylite; Owner: sync
--

ALTER TABLE ONLY oxylite.pending_ops ALTER COLUMN seq SET DEFAULT nextval('oxylite.pending_ops_seq_seq'::regclass);


--
-- Name: sync_log seq; Type: DEFAULT; Schema: oxylite; Owner: sync
--

ALTER TABLE ONLY oxylite.sync_log ALTER COLUMN seq SET DEFAULT nextval('oxylite.sync_log_seq_seq'::regclass);


--
-- Data for Name: meta; Type: TABLE DATA; Schema: oxylite; Owner: sync
--

COPY oxylite.meta (key, value) FROM stdin;
\.


--
-- Data for Name: pending_ops; Type: TABLE DATA; Schema: oxylite; Owner: sync
--

COPY oxylite.pending_ops (seq, op) FROM stdin;
\.


--
-- Data for Name: snapshots; Type: TABLE DATA; Schema: oxylite; Owner: sync
--

COPY oxylite.snapshots (table_name, seq, created_at, data) FROM stdin;
\.


--
-- Data for Name: sync_log; Type: TABLE DATA; Schema: oxylite; Owner: sync
--

COPY oxylite.sync_log (seq, table_name, row_id, payload, updated_at) FROM stdin;
1	notes	01980000-0000-7000-8000-000000000001	{"id": "01980000-0000-7000-8000-000000000001", "body": "This note ships with the default database state (scripts/db-default.sql).", "title": "seed: welcome", "updated_at": "2026-01-01T00:00:01+00:00"}	2026-01-01 00:00:01+00
2	notes	01980000-0000-7000-8000-000000000002	{"id": "01980000-0000-7000-8000-000000000002", "body": "Writes land in local PGlite first, then sync over websocket with LWW.", "title": "seed: offline-first", "updated_at": "2026-01-01T00:00:02+00:00"}	2026-01-01 00:00:02+00
3	notes	01980000-0000-7000-8000-000000000003	{"id": "01980000-0000-7000-8000-000000000003", "body": "One engine per browser — subordinate tabs proxy to the leader.", "title": "seed: multi-tab", "updated_at": "2026-01-01T00:00:03+00:00"}	2026-01-01 00:00:03+00
\.


--
-- Data for Name: tombstones; Type: TABLE DATA; Schema: oxylite; Owner: sync
--

COPY oxylite.tombstones (table_name, id, deleted_at) FROM stdin;
\.


--
-- Data for Name: _sqlx_migrations; Type: TABLE DATA; Schema: public; Owner: sync
--

COPY public._sqlx_migrations (version, description, installed_on, success, checksum, execution_time) FROM stdin;
1	0001_notes	2026-01-01 00:00:00+00	t	\\xae5a865117c76af6650b272f39c491a08fa56866965f2545255e1ed0e1d81b0caff5f4e2781fad533a280caf4abdb323	0
2	0002_sync_log	2026-01-01 00:00:00+00	t	\\x1789b0760832106cd94a5681f74737358853b8d03cd98690d4c5f33b09c2880df29680331bafca73d809707977788ed5	0
3	0003_meta	2026-01-01 00:00:00+00	t	\\xf46c0bc8aa8d73d7e7da477dde7e6313c70dbed0291782b42fe97ed3591ad763dddeea8785907e43f6b8cc55a35860a0	0
4	0004_snapshots	2026-01-01 00:00:00+00	t	\\x1b048966e84bbe256583515ec1f82c0c126f0e95959b6e874a0c2798c191eba9f8c857142ffe16ba4afcea8cbabf38a1	0
5	0005_pending_ops	2026-01-01 00:00:00+00	t	\\xb1b83c65ef8e3a359729ec5be2dd3e6e0fcf379ccd2be04e20607e09e9186b23f987113812f790f287a1b1dbdac64df3	0
6	0006_tombstones	2026-01-01 00:00:00+00	t	\\x98cce88a615c125ef991efe86a33000f09c49b1c431901275675ff68dc3e5ea2f0faed16b478ffd40e4364fa75176ec6	0
7	0007_protocol_timestamps	2026-01-01 00:00:00+00	t	\\xd0abdbec32d884f1bb21daf2282daba44aa7634375f08185fee230e6ea4dc444e6a95eaf59cd39aae1a4f364ba10a601	0
8	0008_notes_timestamptz	2026-01-01 00:00:00+00	t	\\x8a214eb10d3c56806fc67c0c3b04bd3c6ec7715177a53e80e68f7a0f93bb7ac3614a3b02f29b72fbea90e885208254f6	0
\.


--
-- Data for Name: notes; Type: TABLE DATA; Schema: public; Owner: sync
--

COPY public.notes (id, title, body, updated_at) FROM stdin;
01980000-0000-7000-8000-000000000001	seed: welcome	This note ships with the default database state (scripts/db-default.sql).	2026-01-01 00:00:01+00
01980000-0000-7000-8000-000000000002	seed: offline-first	Writes land in local PGlite first, then sync over websocket with LWW.	2026-01-01 00:00:02+00
01980000-0000-7000-8000-000000000003	seed: multi-tab	One engine per browser — subordinate tabs proxy to the leader.	2026-01-01 00:00:03+00
\.


--
-- Name: pending_ops_seq_seq; Type: SEQUENCE SET; Schema: oxylite; Owner: sync
--

SELECT pg_catalog.setval('oxylite.pending_ops_seq_seq', 1, false);


--
-- Name: sync_log_seq_seq; Type: SEQUENCE SET; Schema: oxylite; Owner: sync
--

SELECT pg_catalog.setval('oxylite.sync_log_seq_seq', 3, true);


--
-- Name: meta meta_pkey; Type: CONSTRAINT; Schema: oxylite; Owner: sync
--

ALTER TABLE ONLY oxylite.meta
    ADD CONSTRAINT meta_pkey PRIMARY KEY (key);


--
-- Name: pending_ops pending_ops_pkey; Type: CONSTRAINT; Schema: oxylite; Owner: sync
--

ALTER TABLE ONLY oxylite.pending_ops
    ADD CONSTRAINT pending_ops_pkey PRIMARY KEY (seq);


--
-- Name: snapshots snapshots_pkey; Type: CONSTRAINT; Schema: oxylite; Owner: sync
--

ALTER TABLE ONLY oxylite.snapshots
    ADD CONSTRAINT snapshots_pkey PRIMARY KEY (table_name);


--
-- Name: sync_log sync_log_pkey; Type: CONSTRAINT; Schema: oxylite; Owner: sync
--

ALTER TABLE ONLY oxylite.sync_log
    ADD CONSTRAINT sync_log_pkey PRIMARY KEY (seq);


--
-- Name: tombstones tombstones_pkey; Type: CONSTRAINT; Schema: oxylite; Owner: sync
--

ALTER TABLE ONLY oxylite.tombstones
    ADD CONSTRAINT tombstones_pkey PRIMARY KEY (table_name, id);


--
-- Name: _sqlx_migrations _sqlx_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public._sqlx_migrations
    ADD CONSTRAINT _sqlx_migrations_pkey PRIMARY KEY (version);


--
-- Name: notes notes_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.notes
    ADD CONSTRAINT notes_pkey PRIMARY KEY (id);


--
-- PostgreSQL database dump complete
--

