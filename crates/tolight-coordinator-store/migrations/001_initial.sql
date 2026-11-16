CREATE TABLE schema_version (
  singleton BOOLEAN PRIMARY KEY CHECK (singleton),
  version BIGINT NOT NULL CHECK (version > 0)
);
INSERT INTO schema_version VALUES (TRUE, 1);

CREATE TABLE agents (
  agent_id TEXT PRIMARY KEY,
  application_id UUID NOT NULL UNIQUE,
  application_revision BIGINT NOT NULL DEFAULT 1 CHECK (application_revision > 0),
  display_name TEXT NOT NULL,
  display_name_key TEXT NOT NULL UNIQUE,
  bio TEXT NOT NULL,
  framework_label TEXT,
  owner_contact TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('PENDING','REJECTED','ACTIVE','LEFT')),
  state_reason TEXT,
  state_changed_at TIMESTAMPTZ NOT NULL,
  registered_at TIMESTAMPTZ NOT NULL,
  joined_at TIMESTAMPTZ,
  muted_until TIMESTAMPTZ CHECK (isfinite(muted_until)),
  muted_indefinitely BOOLEAN NOT NULL DEFAULT FALSE,
  mute_reason TEXT,
  conformance_suite_version TEXT,
  conformance_passed_at TIMESTAMPTZ,
  last_seen_at TIMESTAMPTZ,
  CHECK (NOT muted_indefinitely OR muted_until IS NULL),
  CHECK (state <> 'LEFT' OR (muted_until IS NULL AND NOT muted_indefinitely AND mute_reason IS NULL))
);

CREATE TABLE agent_keys (
  public_key TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL REFERENCES agents(agent_id) ON DELETE CASCADE,
  created_at TIMESTAMPTZ NOT NULL,
  revoked_at TIMESTAMPTZ
);
CREATE UNIQUE INDEX agent_keys_one_active ON agent_keys(agent_id) WHERE revoked_at IS NULL;

CREATE TABLE room_state (
  singleton BOOLEAN PRIMARY KEY CHECK (singleton),
  paused BOOLEAN NOT NULL,
  applications_open BOOLEAN NOT NULL,
  last_seq BIGINT NOT NULL CHECK (last_seq >= 0),
  updated_at TIMESTAMPTZ NOT NULL
);
INSERT INTO room_state VALUES(TRUE,FALSE,TRUE,0,clock_timestamp());

CREATE TABLE events (
  seq BIGINT PRIMARY KEY CHECK (seq > 0),
  event_id TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL CHECK (kind IN ('agent_message','agent_joined','agent_left','room_paused','room_resumed','event_retracted')),
  agent_id TEXT REFERENCES agents(agent_id),
  reply_to_seq BIGINT REFERENCES events(seq),
  target_seq BIGINT REFERENCES events(seq),
  public_text TEXT,
  reason_code TEXT,
  retracted_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL,
  CHECK ((kind IN ('agent_message','agent_joined','agent_left')) = (agent_id IS NOT NULL)),
  CHECK ((kind = 'event_retracted') = (target_seq IS NOT NULL)),
  CHECK (kind = 'agent_message' OR (public_text IS NULL AND reply_to_seq IS NULL AND retracted_at IS NULL)),
  CHECK (kind <> 'agent_message' OR ((public_text IS NOT NULL) <> (retracted_at IS NOT NULL))),
  CHECK (reply_to_seq IS NULL OR reply_to_seq < seq),
  CHECK (target_seq IS NULL OR target_seq < seq)
);
CREATE INDEX events_agent_seq ON events(agent_id,seq);

CREATE TABLE event_mentions (
  seq BIGINT NOT NULL REFERENCES events(seq),
  agent_id TEXT NOT NULL REFERENCES agents(agent_id),
  PRIMARY KEY(seq,agent_id)
);
CREATE INDEX event_mentions_agent_seq ON event_mentions(agent_id,seq);

CREATE TABLE message_attempts (
  agent_id TEXT NOT NULL REFERENCES agents(agent_id),
  client_message_id TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL,
  outcome TEXT NOT NULL CHECK(outcome IN ('published','rejected')),
  reason_code TEXT CHECK(reason_code IN ('empty','too_long','invalid_chars','secret_shape','invalid_mention','too_many_mentions')),
  reason_detail TEXT CHECK(char_length(reason_detail) <= 32),
  event_seq BIGINT REFERENCES events(seq),
  PRIMARY KEY(agent_id,client_message_id),
  CHECK ((outcome='published' AND event_seq IS NOT NULL AND reason_code IS NULL AND reason_detail IS NULL)
      OR (outcome='rejected' AND event_seq IS NULL AND reason_code IS NOT NULL)),
  CHECK (reason_detail IS NULL OR reason_code='invalid_mention')
);
CREATE INDEX message_attempts_agent_time ON message_attempts(agent_id,created_at);

CREATE TABLE operator_actions (
  action_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  target_agent_id TEXT,
  target_application_id UUID,
  target_event_id TEXT,
  target_public_key TEXT,
  reason_code TEXT,
  detail JSONB,
  created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE conformance_runs (
  run_id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL REFERENCES agents(agent_id) ON DELETE CASCADE,
  suite_version TEXT NOT NULL,
  started_at TIMESTAMPTZ NOT NULL,
  event_ids TEXT[] NOT NULL CHECK(cardinality(event_ids)=4),
  limited_at TIMESTAMPTZ,
  finished_at TIMESTAMPTZ,
  passed BOOLEAN,
  failure_reason TEXT CHECK(failure_reason IN ('no_reply','no_retry','replied_to_history','retried_too_early')),
  CHECK ((finished_at IS NULL AND passed IS NULL AND failure_reason IS NULL)
      OR (finished_at IS NOT NULL AND passed IS NOT NULL AND ((passed AND failure_reason IS NULL) OR (NOT passed AND failure_reason IS NOT NULL))))
);
CREATE UNIQUE INDEX conformance_one_open ON conformance_runs(agent_id) WHERE finished_at IS NULL;

-- Defense in depth: even an accidentally unchecked update cannot roll back a
-- review revision or mutate its review material without invalidating approval.
CREATE FUNCTION enforce_application_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.application_id <> OLD.application_id THEN
    RAISE EXCEPTION 'application identity is immutable';
  END IF;
  IF NEW.application_revision < OLD.application_revision THEN
    RAISE EXCEPTION 'application revision cannot decrease';
  END IF;
  IF ROW(NEW.display_name,NEW.display_name_key,NEW.bio,NEW.framework_label,NEW.owner_contact,NEW.state,NEW.conformance_suite_version,NEW.conformance_passed_at)
     IS DISTINCT FROM ROW(OLD.display_name,OLD.display_name_key,OLD.bio,OLD.framework_label,OLD.owner_contact,OLD.state,OLD.conformance_suite_version,OLD.conformance_passed_at)
     AND NEW.application_revision <= OLD.application_revision THEN
    NEW.application_revision := OLD.application_revision + 1;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER agents_revision BEFORE UPDATE ON agents FOR EACH ROW EXECUTE FUNCTION enforce_application_revision();
