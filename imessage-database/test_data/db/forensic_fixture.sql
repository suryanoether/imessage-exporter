-- =============================================================================
-- forensic_fixture.sql
-- =============================================================================
--
-- Documentation for the deliberate scenarios injected on top of `test.db` to
-- produce `forensic_fixture.db`. The fixture is checked in as a binary so the
-- tests can run without a build step, but this file is the source of truth for
-- what's inside it. If you regenerate the fixture, mirror these inserts.
--
-- (0) Scaffolding so messages route to a per-chat file (not orphaned). The
-- per-file forensic scope header only renders for messages that have a
-- resolvable chat:
--
--     INSERT INTO handle (id, country, service, uncanonicalized_id)
--       VALUES ('+15555550100', 'us', 'iMessage', '+15555550100');
--     INSERT INTO chat (guid, chat_identifier, service_name, display_name)
--       VALUES ('forensic-fixture-chat', '+15555550100', 'iMessage',
--               'Forensic Fixture Chat');
--     INSERT INTO chat_handle_join (chat_id, handle_id)
--       VALUES (<new_chat_rowid>, <new_handle_rowid>);
--
-- After (1)-(6) below, every fixture message (including the 3 inherited
-- from test.db) is wired into the chat via chat_message_join:
--
--     INSERT INTO chat_message_join (chat_id, message_id)
--       VALUES (<new_chat_rowid>, <every_fixture_message_rowid>);
--
-- Anchor message (already present in test.db):
--   guid: 0355C6E1-D0C8-4212-AA87-DD8AE4FD1203
--   body: "I'm going to try to eat as quick as possible and then come over"
--   body location: attributedBody blob (NOT the `text` column)
--
-- The anchor's body lives in attributedBody. Every snippet/quote-header path
-- has to call apply_body() on the parent or it gets an empty text and falls
-- back to "[attachment]" / "[no preview]". The fixture exists in large part
-- to keep that regression caught.
--
-- All inserted rows share the anchor's chat (via chat_message_join) so they
-- end up in the same conversation file.
--
-- Time math: dates are nanoseconds-since-2001 (post-iOS 12 format). Each new
-- row offsets the anchor's date by N seconds * 1_000_000_000.

-- -----------------------------------------------------------------------------
-- (1) Reply to the anchor. Body in the plain `text` column.
-- -----------------------------------------------------------------------------
INSERT INTO message (
    guid, text, service, handle_id, date, date_read, date_delivered,
    is_from_me, is_read, item_type,
    thread_originator_guid, thread_originator_part
) VALUES (
    'F0R3N51C-0001-4567-89AB-CDEF12345678',
    'ok cool',
    'iMessage', NULL,
    <anchor_date> + 60 * 1000000000,
    <anchor_date> + 90 * 1000000000,
    <anchor_date> + 65 * 1000000000,
    1, 1, 0,
    '0355C6E1-D0C8-4212-AA87-DD8AE4FD1203',
    '0:0:0'
);

-- -----------------------------------------------------------------------------
-- (2) Added Loved tapback on the anchor. associated_message_type=2000.
-- -----------------------------------------------------------------------------
INSERT INTO message (
    guid, text, service, handle_id, date, date_read, date_delivered,
    is_from_me, is_read, item_type,
    associated_message_guid, associated_message_type
) VALUES (
    'F0R3N51C-0002-LOVE-ADDD-EDFFFFFFFFFF',
    NULL,
    'iMessage', NULL,
    <anchor_date> + 120 * 1000000000,
    0, 0,
    0, 1, 0,
    'p:0/0355C6E1-D0C8-4212-AA87-DD8AE4FD1203',
    2000
);

-- -----------------------------------------------------------------------------
-- (3) Removed Liked tapback on the anchor. associated_message_type=3001.
-- Tests that forensic mode surfaces removal events; default mode hides them.
-- -----------------------------------------------------------------------------
INSERT INTO message (
    guid, text, service, handle_id, date, date_read, date_delivered,
    is_from_me, is_read, item_type,
    associated_message_guid, associated_message_type
) VALUES (
    'F0R3N51C-0003-LIKE-REMD-EDFFFFFFFFFF',
    NULL,
    'iMessage', NULL,
    <anchor_date> + 180 * 1000000000,
    0, 0,
    1, 1, 0,
    'p:0/0355C6E1-D0C8-4212-AA87-DD8AE4FD1203',
    3001
);

-- -----------------------------------------------------------------------------
-- (4) Custom emoji tapback (added). associated_message_type=2006 +
-- associated_message_emoji. The emoji here is U+2615 + U+FE0F (coffee +
-- variation selector) to exercise the non-ASCII display path.
-- -----------------------------------------------------------------------------
INSERT INTO message (
    guid, text, service, handle_id, date, date_read, date_delivered,
    is_from_me, is_read, item_type,
    associated_message_guid, associated_message_type, associated_message_emoji
) VALUES (
    'F0R3N51C-0004-EMJI-CFFE-EDFFFFFFFFFF',
    NULL,
    'iMessage', NULL,
    <anchor_date> + 240 * 1000000000,
    0, 0,
    1, 1, 0,
    'p:0/0355C6E1-D0C8-4212-AA87-DD8AE4FD1203',
    2006,
    '☕️'
);

-- -----------------------------------------------------------------------------
-- (5) Orphan tapback whose target GUID is not present in the fixture. Forensic
-- mode must still render this row as a bubble (no reference header).
-- -----------------------------------------------------------------------------
INSERT INTO message (
    guid, text, service, handle_id, date, date_read, date_delivered,
    is_from_me, is_read, item_type,
    associated_message_guid, associated_message_type
) VALUES (
    'F0R3N51C-0005-LOVE-ORPH-EDFFFFFFFFFF',
    NULL,
    'iMessage', NULL,
    <anchor_date> + 300 * 1000000000,
    0, 0,
    0, 1, 0,
    'p:0/NONEXISTENT-PARENT-GUID-NOT-IN-FIXTURE',
    2000
);

-- -----------------------------------------------------------------------------
-- (6) Edited regular message. date_edited != 0 so is_edited() returns true
-- and the forensic_meta strip shows the "edited" flag.
-- -----------------------------------------------------------------------------
INSERT INTO message (
    guid, text, service, handle_id, date, date_read, date_delivered,
    is_from_me, is_read, item_type, date_edited
) VALUES (
    'F0R3N51C-0006-EDIT-EDIT-EDFFFFFFFFFF',
    'first cut',
    'iMessage', NULL,
    <anchor_date> + 360 * 1000000000,
    <anchor_date> + 365 * 1000000000,
    <anchor_date> + 362 * 1000000000,
    1, 1, 0,
    <anchor_date> + 400 * 1000000000
);

-- Each inserted row is also joined to the anchor's chat:
--   INSERT INTO chat_message_join (chat_id, message_id)
--   VALUES (<anchor_chat_id>, <new_rowid>);
