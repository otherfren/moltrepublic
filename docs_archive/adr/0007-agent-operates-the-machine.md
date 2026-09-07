# ADR-0007: the MCP agent operates the machine, not just the seat

Status: ACCEPTED 2026-09-06 (user decision). Supersedes the host boundary
of the MCP audit 2026-08-26 (`docs_archive/security/mcp-security.md`, "The
host boundary"; commit 2cace65b) and the rule "the recovery phrase leaves
the process on no surface" (review K4 of the same audit).

## Decision

An agent driving a node over MCP is FULLY AUTONOMOUS and operates the
machine: it founds, joins and recovers republics headless, holds the
recovery phrase, sets the host posture, reaches any path the node's user
can reach, and gives the clearnet consent. It is the user's job to make
sure that only a trustworthy LLM founds and steers a node - the software
does not second-guess that trust at the tool boundary.

The user's words (2026-09-06): "Agenten müssen headless gründen können, es
ist die Aufgabe des Users sicherzustellen, dass nur vertrauenswürdige LLMs
gründen und steuern. Der Agent ist voll-autonom und bedient die Maschine."

## Why

Since the audit no node without a GUI could found or join at all: the
phrase was shown in the wizard only and `confirm_seed_backup` needed it
re-typed. That made every headless deployment - the deployment an agent
runs - a second-class node, and it broke the GUI walk (round 3 protocol
D1). The audit's threat model (an untrusted MCP client on a cleartext
localhost port) is real, but the user chose autonomy over it and owns the
consequence: whoever can reach the port with the seat token is the seat's
operator, machine included.

## Consequences

Reverted (each becomes a Seat-scope tool or a served field):

* `SetNodePosture` (headless, workspace_dir, download_dir, mcp_port,
  mcp_allow, mcp_token, mcp_read_token, anonymity, tor_mode, tor_port,
  poke_wake_command) - `patch_settings` accepts them, `save_settings`
  carries them.
* Any-path file access: `share_file` with a path, `export_workspace` and
  `wiki_export` to a path, `download_file` to a path; the exchange-folder
  variants stay as the bare-name convenience.
* `set_mirror_dir`.
* Clearnet consent: `relay_confirm {accept_clearnet: true}` and
  `relay_clearnet_session {unlock: true}`.
* The recovery phrase: `create.seed` and `join.seed` are served during the
  ritual (cleared at the seal), a stored workspace's phrase is a PULL
  (`reveal_seed`; the list only flags `has_seed` - amended 2026-09-08 after
  the v0.0.2 field report: an operator who wants the phrase out of its logs
  must be able to not receive it), `confirm_seed_backup` works headless,
  the MCP export carries the seed like the GUI export.

Kept, because they are not about trust but about WHO speaks:

* `ui_publish` (the window reports what it renders) and every `net_*`
  channel (the transport speaks) stay INTERNAL.
* The read-only key stays read-only and never sees a phrase or a secret.
* Stored secrets (`s3_secret_key`, `mcp_token`, `mcp_read_token`) stay
  write-only in `read_session`: reading them serves no autonomous action.
  (Open to change if a use appears.)

The co-equality test and the read-scope pin list remain the guards: every
`Command` is a tool unless the operator is not the one speaking.

## Execution

`docs_archive/reviews/mcp_agent_friction_fixes_round_3.md` Part F; the audit
section of `mcp-security.md` is rewritten in that change.
