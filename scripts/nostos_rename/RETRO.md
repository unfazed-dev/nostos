# Retro rewrite runbook: cairn → nostos

Turns the cairn history (`main`, `v0.1.0`, `v0.2.0`) into `unfazed-dev/nostos`,
with one retro PR per active day. Decisions: `docs/ci/decisions.md` rows 2, 2a, 3,
3b, 3c, 12, 14. Steps 1–6 are local and can be repeated. Steps 7–15 write to
GitHub, and only the user runs them.

| tool | does |
|---|---|
| `retro.py` | groups by committer day in the commit's own tz (a kept tag closes its group), `[arxa-*]` tags, drafts `retro/titles.tsv` |
| `rewrite.py` | stage 1: git-filter-repo on a fresh clone. Stage 2: empty root, one `--no-ff` merge per group, tags moved onto merges. Then verify |
| `postfix.sh` | `cargo fmt --all`, `flutter pub get` + `dart format` (the dirs `ci.yml` checks), uniffi C# regen; fails on anything outside that scope |
| `hashfix.py` | the post-tip PR #39: `--style` commits postfix.sh's output, then old hash cites → final ones plus `docs/ci/retro/*.md` and the release notes; one merge |
| `replay.py` | pushes branches, opens PRs, fast-forwards `main`, pushes tags, creates releases. Dry run by default |
| `test_retro.py` | invariants on a synthetic repo: `python3 scripts/nostos_rename/test_retro.py` |

Prereqs: `python3 -c "import git_filter_repo"` works (`pip install git-filter-repo`),
`cargo`, `flutter` + `dart` (3.47.5, as CI) and `uniffi-bindgen-cs` (`sdk/cairn_dotnet/README.md`) are on PATH,
`gh auth status` is `unfazed-dev`, `scripts/nostos_rename/rules.py` has landed,
and the cairn `main` is frozen (nothing lands until step 12).

```sh
CAIRN=/Volumes/business_ssd/arxa_digital_solutions/cairn
NOSTOS=/Volumes/business_ssd/arxa_digital_solutions/nostos
T=scripts/nostos_rename
export CARGO_TARGET_DIR=/Volumes/business_ssd/arxa_digital_solutions/.nostos-scratch/target  # rm -rf after step 6
```

The global `build-dir` (ADR-0044) still puts intermediates under
`/Volumes/developer_ssd/dev/cargo-build/<hash>/` (about 6 GB for steps 4–5): after step 6,
delete the dirs whose `*.d` files name `$NOSTOS`.

## Local (repeatable)

1. **Titles.** `python3 $T/retro.py titles --repo $CAIRN --rules $T/rules.py` keeps the
   signed-off rows and drafts new ones (a commit that lands today changes the last
   group). Polish the drafts in `$T/retro/titles.tsv`. **The user signs off.** Tags
   `auto` are re-derived from the files at rewrite time (the last day, still growing);
   the row dated `-` is PR #39.
2. **Rewrite.** `python3 $T/rewrite.py --source $CAIRN --work $NOSTOS --rules $T/rules.py`.
   It must end with `verify: OK`. That covers fsck, refs = main + 2 tags,
   noreply-only identities, a 1:1 commit map, first parent = root + merges,
   parents preserved, tags on tree-equal merges, the tip tree == `apply.py`'s forward
   rename of the old tip, and every commit's tree == rules(old tree).
   It takes about 5 min. Output goes to `$NOSTOS/.git/nostos-retro/`:
   `commit-map` (old → final), `groups.json`, `message-cites.txt`, `verify.txt`.
   A failed run leaves `$NOSTOS` behind: `rm -rf` it before you retry.
3. **Look.** `git -C $NOSTOS log --first-parent --format='%h %cs %s' main`, then a
   few `git show --stat <merge>`, then `message-cites.txt`.
4. **Post-rename fixups.** `$T/postfix.sh $NOSTOS` (about 2 min, most of it the
   dotnet release build). It must end `postfix: in scope`: only `.rs`, the CI-formatted
   `.dart` dirs, the atlet + example `pubspec.lock` and atlet's macOS plugin registrant
   (`flutter pub get` re-sorts them) and the checksum lines of `nostos.cs` may change.
   Changes stay uncommitted.
5. **PR #39.** `python3 $T/hashfix.py --work $NOSTOS --style`: commit (a) `style: …` is
   step 4's output, commit (b) is the hash fixups + retro docs, one `[arxa-builder]` merge. Review
   `.git/nostos-retro/hashfix-report.txt`: `kept` = the arxa-studio pins, left on
   purpose; `off-main` = cites of commits that were never on `main`;
   `unresolved` = mostly not hashes (rustc ids, SwiftPM pins, hex constants).
   Then run `python3 $T/rewrite.py --source $CAIRN --work $NOSTOS --rules $T/rules.py --verify-only`,
   which must print `verify: OK` again (it also checks #39 is one merge onto the retro tip).
   Then `ln -s $CARGO_TARGET_DIR $NOSTOS/target && make -C $NOSTOS ci` must be green (the
   link: `e2e_live_replication` looks for `target/debug/examples/e2e_server` and ignores
   `CARGO_TARGET_DIR`; `target` is gitignored).
6. **Dry-run the replay.** `python3 $T/replay.py --repo $NOSTOS`. This prints every
   command and runs none of them. Read it.

To start over, `rm -rf $NOSTOS` and go back to step 2.

## GitHub (user only, in order)

7. **Backup.**
   ```sh
   git clone --mirror git@github.com:unfazed-dev/cairn.git ~/backups/cairn-mirror.git
   git -C ~/backups/cairn-mirror.git bundle create ~/backups/cairn-github.bundle --all
   git -C $CAIRN bundle create ~/backups/cairn-local.bundle --all   # local-only branches too
   git bundle verify ~/backups/cairn-github.bundle && git bundle verify ~/backups/cairn-local.bundle
   ```
8. **Archive the old repo.** Run `gh repo rename cairn-archive --repo unfazed-dev/cairn --yes`,
   then `gh repo archive unfazed-dev/cairn-archive --yes`. Never create a new
   `unfazed-dev/cairn`: the redirect is what keeps arxa-studio's `fabc1a16` pin working.
9. **Create the new repo.** `gh repo create unfazed-dev/nostos --public`. Leave it
   empty: no README, license or .gitignore. Anything created now would take PR/issue #1.
10. **Actions off.** Run `gh api -X PUT repos/unfazed-dev/nostos/actions/permissions -F enabled=false`,
   then check that `gh api repos/unfazed-dev/nostos/actions/permissions` shows `"enabled": false`.
11. **Root.**
    ```sh
    git -C $NOSTOS remote set-url origin git@github.com:unfazed-dev/nostos.git   # origin already exists (https)
    ARXA_ALLOW_MAIN_PUSH=1 git -C $NOSTOS push origin "$(git -C $NOSTOS rev-list --max-parents=0 main):refs/heads/main"
    ```
12. **Replay.** `ARXA_ALLOW_MAIN_PUSH=1 python3 $T/replay.py --repo $NOSTOS --apply`.
    Before it pushes anything it checks: Actions off, remote `main` = root or a retro
    merge, and the newest issue/PR number matches the plan. Per PR it pushes
    `retro/NN-date`, runs `gh pr create`, checks the PR got number #NN (it stops if
    not), then fast-forwards `main` to the merge. GitHub marks the PR merged. Last come
    the tags and the notes-only releases. PR creation is paced under 60/min and 450/hr.
    If it is interrupted, rerun the same command: it resumes from the remote `main`.
    Add `--delete-branches` to remove the `retro/*` branches at the end.
13. **Check.** 39 merged PRs in order, `git ls-remote origin main` == `git -C $NOSTOS rev-parse main`,
    `v0.1.0`/`v0.2.0` on their merges, and two releases marked "historical — no binaries".
14. **Actions on.** `gh api -X PUT repos/unfazed-dev/nostos/actions/permissions -F enabled=true -f allowed_actions=all`.
15. **Protect `main`.** Run the `gh api` call in `docs/ci/setup.md`. It has to come
    after the replay, because protection would refuse the replay's pushes.

Local layout (decision 2c): once step 13 checks out, `mv $CAIRN …/cairn-archive`.
`$NOSTOS` is the working clone.

## Notes

- PR pages show the replay date. The historical date is in the title, the body and
  the merge commit (the merge carries the group's last committer date).
- A PR's diff is exactly its day's commits: its base is the previous merge, whose
  tree equals the old parent's tree.
- `titles.tsv` is keyed on (N, date). If the history moved after sign-off, `rewrite.py`
  refuses until step 1 is rerun.
