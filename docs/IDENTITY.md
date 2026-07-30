# Identity & placeholders — the single record

Every place Nostos asserts **who it is** and **where it lives**. Nothing here is
invented: each row is either a real value that works today, or an explicitly
marked placeholder waiting on a registration decision.

**Why this file exists.** The GitHub org `nostos-sync` was invented, written into
`Cargo.toml`, and then cited back as evidence for itself ("create the org
`nostos-sync/nostos` *per Cargo.toml*"). It spread to 24 files — including the
Homebrew formula's download URLs and the landing page's "Star on GitHub" link —
before anyone noticed the real remote is `unfazed-dev/nostos`. **A manifest is not
a source of truth for a fact it got from you.** This file is the source of truth;
manifests copy from it.

## Real today — no action needed

| what | value | where |
|---|---|---|
| GitHub repo | `https://github.com/unfazed-dev/nostos` | `origin` remote; workspace + all 9 SDK manifests; landing page; Homebrew formula; release manifest script |
| GitHub org / handle | `unfazed-dev` | `authors` / `Authors` / `Company` fields |
| Licence | `Apache-2.0` | root `LICENSE` + one per SDK package (9/9) |
| Version | `0.1.0` | workspace `[workspace.package]` + all 9 SDK packages |

## PENDING — placeholders, decision not made

Grep token: **`NOSTOS-IDENTITY-PENDING`**. Every unresolved site carries it in a
comment, so `git grep NOSTOS-IDENTITY-PENDING` lists the complete set.

| what | current stand-in | decide before |
|---|---|---|
| **Primary domain** | the GitHub repo URL is used wherever a homepage is required | first registry publish (npm/pub.dev/NuGet/crates.io show `homepage`) |
| **Legal entity** | `unfazed-dev` (the GitHub handle) | first publish under a company name; also `<Authors>`/`<Company>` in the dotnet csproj |
| **Contact email** | none — security reports route to GitHub's private vulnerability reporting | **before any public launch**; see the note below |
| **Docs site** | `https://github.com/unfazed-dev/nostos/tree/main/docs` | when/if a docs host exists (`documentation` in `Cargo.toml`) |

Candidate domains are discussed in `docs/STRATEGY.md` (§ naming) — that list is
deliberately still open and is *not* a claim that any of them is owned.

### The contact-email problem was the sharpest one

`SECURITY.md` used to instruct researchers to report vulnerabilities privately to
`founders@nostos.run` — **a mailbox on an unregistered domain**. A vulnerability
report sent there reaches nobody, and the reporter has no way to know. That is
worse than having no policy, because it consumes the one disclosure attempt a
good-faith researcher makes before going public.

It now points at **GitHub's private vulnerability reporting**, which works today,
needs no domain, and is the conventional default for a repo-hosted project. Swap
in a real mailbox when one exists.

## Updating when a registration lands

```bash
# 1. see every pending site
git grep -n NOSTOS-IDENTITY-PENDING

# 2. domain (example: nostos.run becomes real)
git grep -l 'github.com/unfazed-dev/nostos"' -- '*.toml' '*.yaml' '*.json' '*.csproj'
#    then set homepage/PackageProjectUrl per manifest — do NOT blanket-replace the
#    repository/RepositoryUrl fields, which should keep pointing at the repo

# 3. entity name
git grep -n 'unfazed-dev' -- Cargo.toml '*.csproj'

# 4. after any change
make ci                      # workspace metadata is compiled
python3 -c "import xml.etree.ElementTree as E;E.parse('sdk/nostos_dotnet/dotnet/Nostos.DotNet.csproj')"
```

## Deliberately NOT placeholders

These use `nostos.run`-shaped values as **test data** and should stay — they are
fixtures, not claims:

- `supabase/schema.sql`, `docker/pg-init/03-seed-booking.sql` — seeded user rows
- `crates/nostos-cloud/src/{routes,store}.rs` — test tenant emails
- `fixtures/flutter/todo/lib/infra/fake_auth_gateway.dart` — a fake gateway

Also not a placeholder: **the Fly.io app name `nostos-sync`** (`fly.toml`,
`crates/nostos-cli/src/commands/deploy.rs`, `deploy/README.md`). It shares a string
with the old fake GitHub org by coincidence and is a *different namespace* —
renaming it would break deploys. Leave it alone.
