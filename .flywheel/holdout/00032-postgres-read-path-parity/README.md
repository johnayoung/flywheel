# Held-out graders: 00032 Postgres read-path parity

This directory holds the held-out, backend-parametrized parity tests authored by
the `fw-verify` step. They are the authoritative graders for spec 00032 criteria
1-5, 9, 10. The implementing agent must NOT author, read, edit, or weaken anything
here; these tests are run out-of-band against the committed change.

The tests, once authored, seed identical state through the WRITE protocol
(`save_task`, `append_domain_event`, `save_attempt`, `append_grader_result`) on
both a `SqliteStore` and a live `PostgresStore`, compute the read on each backend,
and assert the two results are field-for-field equal — neither side is a literal
the agent can pre-compute. They skip (not fail) when the Postgres test container is
unreachable, matching `test_store_contract.py`.

Selection expressions the task graders use:
- `history and parity`     -> criterion 1 (history listing parity): asserts (i) the
  set of returned run ids equals EXACTLY the seeded terminal run ids on BOTH
  backends (a seeded non-terminal run is absent), proving the enumeration crossed
  backends through the SI-3 list_lifecycles(statuses=TERMINAL) seam (spec D-6),
  and (ii) field-for-field equal HistoryRow/HistoryRun rows.
- `live_rows and parity`   -> criterion 2 (live-rows parity)
- `latest_lifecycle and parity` -> criterion 3 (latest-lifecycle + resolve_run_id parity)
- `postgres and not refus` -> criterion 4 (factory seam returns un-closed PostgresStore)
- `status and postgres`    -> criterion 5 (status verb end-to-end, no refusal, exit 0)
