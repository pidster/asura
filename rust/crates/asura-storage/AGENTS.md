# File and database storage adapters

Follow root and Rust instructions, the [recovery design](../../../docs/designs/persistence-recovery.md)
and the [reviewed memory packet](../../../docs/designs/hybrid-memory-ontology.md#minimum-embedded-slice-hm0).

Own file persistence for config, logs, journals and sessions, plus typed SurrealDB
document/graph persistence. Follow the [storage ownership contract](../../../docs/designs/storage-adapters.md).
Own bounded authority encoding/replay and typed memory storage. The owner permits
reviewed write, initialization and graph work under its governing design. The
current assignments include the reviewed config/log ownership migration and HM0-A: embedded adapter qualification in
private scratch databases. It does not authorize creating or changing the real
user database, repairing evidence or bypassing installation binding.

Platform supplies safe low-level OS/filesystem primitives; the service owns admission, lifecycle
and state publication. A journal writer requires the separate HM0-B recovery
contract before implementation. Preserve existing read-only inspection behavior.
Use the reviewed SHA-256 owner. Keep limits, deadlines, cancellation, uncertain
outcomes and owner settlement explicit. Tests use private fixtures and scratch.
Root owns dependency graph review and serial builds.

The [conversation admission amendment](../../../docs/designs/conversation-admission.md)
adds CA-A: pure format-1 conversation encoding and replay in the existing authority owner.
Preserve diagnostic kinds 1–3 and their fixtures. Request-bearing initialization
uses distinct kinds 10 and 11. Do not change any format, API or protocol number
without explicit owner authorization. The current owner assignment includes CA-B filesystem writer and production graph
initialization under that contract and its required fault/durability evidence.
