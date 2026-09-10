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

ALTER TABLE IF EXISTS ONLY public.tombstones DROP CONSTRAINT IF EXISTS tombstones_pkey;
ALTER TABLE IF EXISTS ONLY public.sync_log DROP CONSTRAINT IF EXISTS sync_log_pkey;
ALTER TABLE IF EXISTS ONLY public.snapshots DROP CONSTRAINT IF EXISTS snapshots_pkey;
ALTER TABLE IF EXISTS ONLY public.pending_ops DROP CONSTRAINT IF EXISTS pending_ops_pkey;
ALTER TABLE IF EXISTS ONLY public.notes DROP CONSTRAINT IF EXISTS notes_pkey;
ALTER TABLE IF EXISTS ONLY public.meta DROP CONSTRAINT IF EXISTS meta_pkey;
ALTER TABLE IF EXISTS ONLY public._sqlx_migrations DROP CONSTRAINT IF EXISTS _sqlx_migrations_pkey;
ALTER TABLE IF EXISTS public.sync_log ALTER COLUMN seq DROP DEFAULT;
ALTER TABLE IF EXISTS public.pending_ops ALTER COLUMN seq DROP DEFAULT;
DROP TABLE IF EXISTS public.tombstones;
DROP SEQUENCE IF EXISTS public.sync_log_seq_seq;
DROP TABLE IF EXISTS public.sync_log;
DROP TABLE IF EXISTS public.snapshots;
DROP SEQUENCE IF EXISTS public.pending_ops_seq_seq;
DROP TABLE IF EXISTS public.pending_ops;
DROP TABLE IF EXISTS public.notes;
DROP TABLE IF EXISTS public.meta;
DROP TABLE IF EXISTS public._sqlx_migrations;
SET default_tablespace = '';

SET default_table_access_method = heap;

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
-- Name: meta; Type: TABLE; Schema: public; Owner: sync
--

CREATE TABLE public.meta (
    key text NOT NULL,
    value text NOT NULL
);


ALTER TABLE public.meta OWNER TO sync;

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
-- Name: pending_ops; Type: TABLE; Schema: public; Owner: sync
--

CREATE TABLE public.pending_ops (
    seq bigint NOT NULL,
    op text NOT NULL
);


ALTER TABLE public.pending_ops OWNER TO sync;

--
-- Name: pending_ops_seq_seq; Type: SEQUENCE; Schema: public; Owner: sync
--

CREATE SEQUENCE public.pending_ops_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.pending_ops_seq_seq OWNER TO sync;

--
-- Name: pending_ops_seq_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: sync
--

ALTER SEQUENCE public.pending_ops_seq_seq OWNED BY public.pending_ops.seq;


--
-- Name: snapshots; Type: TABLE; Schema: public; Owner: sync
--

CREATE TABLE public.snapshots (
    table_name text NOT NULL,
    seq bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb NOT NULL
);


ALTER TABLE public.snapshots OWNER TO sync;

--
-- Name: sync_log; Type: TABLE; Schema: public; Owner: sync
--

CREATE TABLE public.sync_log (
    seq bigint NOT NULL,
    table_name text NOT NULL,
    row_id uuid NOT NULL,
    payload jsonb NOT NULL,
    updated_at timestamp with time zone NOT NULL
);


ALTER TABLE public.sync_log OWNER TO sync;

--
-- Name: sync_log_seq_seq; Type: SEQUENCE; Schema: public; Owner: sync
--

CREATE SEQUENCE public.sync_log_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.sync_log_seq_seq OWNER TO sync;

--
-- Name: sync_log_seq_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: sync
--

ALTER SEQUENCE public.sync_log_seq_seq OWNED BY public.sync_log.seq;


--
-- Name: tombstones; Type: TABLE; Schema: public; Owner: sync
--

CREATE TABLE public.tombstones (
    table_name text NOT NULL,
    id uuid NOT NULL,
    deleted_at timestamp with time zone NOT NULL
);


ALTER TABLE public.tombstones OWNER TO sync;

--
-- Name: pending_ops seq; Type: DEFAULT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.pending_ops ALTER COLUMN seq SET DEFAULT nextval('public.pending_ops_seq_seq'::regclass);


--
-- Name: sync_log seq; Type: DEFAULT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.sync_log ALTER COLUMN seq SET DEFAULT nextval('public.sync_log_seq_seq'::regclass);


--
-- Data for Name: _sqlx_migrations; Type: TABLE DATA; Schema: public; Owner: sync
--

COPY public._sqlx_migrations (version, description, installed_on, success, checksum, execution_time) FROM stdin;
1	notes	2026-01-01 00:00:00+00	t	\\xae5a865117c76af6650b272f39c491a08fa56866965f2545255e1ed0e1d81b0caff5f4e2781fad533a280caf4abdb323	0
2	sync log	2026-01-01 00:00:00+00	t	\\xff1665d404babc8c86cf2fbf4b72ba7670449f97c34a508125c42589c31b4cb2bcc1aae755d55a03f4a197926e636798	0
3	meta	2026-01-01 00:00:00+00	t	\\x1696090c2455a209c5879724b99a609a2ea9daf5ce26ca24332f9d5348d2895513e88811b6daaaf529913cb0cb4fedfa	0
4	snapshots	2026-01-01 00:00:00+00	t	\\x8883d52f517d4e909331f9c855f5786a12bc40f0b466b0fa45d890360267422038f18c75cf58e517406e66fb0916ac9e	0
5	pending ops	2026-01-01 00:00:00+00	t	\\x9de44a7e6a968baec6d7798c6e4dd6f9e480d9af25cb06313ef26fb4bd0e555a0a8bed7c8f53db263adff11ad5516466	0
6	tombstones	2026-01-01 00:00:00+00	t	\\x5639c90752e5ee743abc442db40a390677adeb5b3b27b4a14078e520644e8322fe37496231a63e1c24ed214e3f056c9d	0
7	timestamptz	2026-01-01 00:00:00+00	t	\\xd7d7404cfeb2d222aae6e0ec9e8997565fe3eb619cb8cf75a3887119c94f2203f0cb6cdbcb78da5d52729816e881fdd0	0
\.


--
-- Data for Name: meta; Type: TABLE DATA; Schema: public; Owner: sync
--

COPY public.meta (key, value) FROM stdin;
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
-- Data for Name: pending_ops; Type: TABLE DATA; Schema: public; Owner: sync
--

COPY public.pending_ops (seq, op) FROM stdin;
\.


--
-- Data for Name: snapshots; Type: TABLE DATA; Schema: public; Owner: sync
--

COPY public.snapshots (table_name, seq, created_at, data) FROM stdin;
\.


--
-- Data for Name: sync_log; Type: TABLE DATA; Schema: public; Owner: sync
--

COPY public.sync_log (seq, table_name, row_id, payload, updated_at) FROM stdin;
1	notes	01980000-0000-7000-8000-000000000001	{"id": "01980000-0000-7000-8000-000000000001", "body": "This note ships with the default database state (scripts/db-default.sql).", "title": "seed: welcome", "updated_at": "2026-01-01T00:00:01+00:00"}	2026-01-01 00:00:01+00
2	notes	01980000-0000-7000-8000-000000000002	{"id": "01980000-0000-7000-8000-000000000002", "body": "Writes land in local PGlite first, then sync over websocket with LWW.", "title": "seed: offline-first", "updated_at": "2026-01-01T00:00:02+00:00"}	2026-01-01 00:00:02+00
3	notes	01980000-0000-7000-8000-000000000003	{"id": "01980000-0000-7000-8000-000000000003", "body": "One engine per browser — subordinate tabs proxy to the leader.", "title": "seed: multi-tab", "updated_at": "2026-01-01T00:00:03+00:00"}	2026-01-01 00:00:03+00
\.


--
-- Data for Name: tombstones; Type: TABLE DATA; Schema: public; Owner: sync
--

COPY public.tombstones (table_name, id, deleted_at) FROM stdin;
\.


--
-- Name: pending_ops_seq_seq; Type: SEQUENCE SET; Schema: public; Owner: sync
--

SELECT pg_catalog.setval('public.pending_ops_seq_seq', 1, false);


--
-- Name: sync_log_seq_seq; Type: SEQUENCE SET; Schema: public; Owner: sync
--

SELECT pg_catalog.setval('public.sync_log_seq_seq', 3, true);


--
-- Name: _sqlx_migrations _sqlx_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public._sqlx_migrations
    ADD CONSTRAINT _sqlx_migrations_pkey PRIMARY KEY (version);


--
-- Name: meta meta_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.meta
    ADD CONSTRAINT meta_pkey PRIMARY KEY (key);


--
-- Name: notes notes_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.notes
    ADD CONSTRAINT notes_pkey PRIMARY KEY (id);


--
-- Name: pending_ops pending_ops_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.pending_ops
    ADD CONSTRAINT pending_ops_pkey PRIMARY KEY (seq);


--
-- Name: snapshots snapshots_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.snapshots
    ADD CONSTRAINT snapshots_pkey PRIMARY KEY (table_name);


--
-- Name: sync_log sync_log_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.sync_log
    ADD CONSTRAINT sync_log_pkey PRIMARY KEY (seq);


--
-- Name: tombstones tombstones_pkey; Type: CONSTRAINT; Schema: public; Owner: sync
--

ALTER TABLE ONLY public.tombstones
    ADD CONSTRAINT tombstones_pkey PRIMARY KEY (table_name, id);


--
-- PostgreSQL database dump complete
--

