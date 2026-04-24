doctor:
    cargo xtask doctor

check:
    cargo xtask check

release-check:
    cargo xtask check publish

ui-snapshot:
    cargo xtask ui snapshot

e2e:
    cargo xtask e2e smoke

e2e-codex:
    cargo xtask e2e codex

e2e-claude:
    cargo xtask e2e claude

stress events="10000":
    cargo xtask stress ingest --events {{events}}
