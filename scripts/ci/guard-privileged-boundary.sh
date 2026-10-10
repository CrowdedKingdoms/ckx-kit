#!/usr/bin/env bash

# Refuse any reference to the SEALED privileged repository from this
# unprivileged tree's workflows and configuration. This is the boundary that
# keeps privileged material out of a tree that a shared CI runner checks out.
#
# THE CORPUS IS A WALK, NOT A GIT QUERY, and that is the whole point of the
# 2026-08-21 rewrite. It used to be:
#
#     git ls-files -z --cached --others --exclude-standard
#
# which never examines an IGNORED file. Three things were wrong with that, in
# increasing order of how much they cost:
#
#  1. The files most likely to name a private repository are exactly the files
#     people gitignore -- local overrides, generated configs, anything holding a
#     clone URL or a deploy-key path. `--exclude-standard` excluded the
#     highest-risk population by construction.
#  2. The corpus depended on WHEN in a job the guard ran. In a fresh checkout
#     there are no untracked files at all, so the flag removes exactly 0 files
#     (measured 2026-08-21) and the guard looks complete. Run it after an
#     install or a build and 2379 files appear, which the flag then hides again.
#     A corpus whose size depends on the preceding step is not a corpus.
#  3. It could not run at all outside a git worktree -- a tarball, a build
#     artifact, an extracted image layer -- which are places this boundary
#     matters most.
#
# A walk consults no index and no ignore rule, so no future flag or `.gitignore`
# edit can narrow it. The nine wrapper gates that survived the 2026-08-21 corpus
# audit all walk; every gate that delegated its corpus was the one with a hole.
#
# SKIP_DIRS is the one list here, and it is deliberately short. Each entry is
# skipped because it is *structurally* uninteresting -- third-party or generated
# -- and NOT because anything ignores it. The counts are printed on every run so
# that narrowing this list is visible in a log rather than silent.

set -euo pipefail

DEFAULT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ROOT="${PRIVILEGED_BOUNDARY_ROOT:-$DEFAULT_ROOT}"

# Assembled at runtime so that this file, which lives in an unprivileged tree,
# does not itself contain the string it forbids.
FORBIDDEN_REPO="$(printf '%s%s%s' 'cks' '-udp-' 'api')"

# Third-party or generated. Skipped by NAME, never by ignore status.
#
# `.docusaurus` earned its place the moment the corpus widened, and it is the
# clearest illustration of what the TYPE filter below is really doing. That
# filter is a proxy: "configuration is where a reference takes effect, prose is
# not the hazard". A documentation build breaks the proxy, because it serialises
# authored prose into generated `.json` -- so seven GraphQL field descriptions
# saying "Row lifecycle is owned by Buddy (<sealed repo>)", which are correct,
# public, and invisible to this guard in their `.mdx` source, reappeared as
# violations in the Docusaurus cache. Generated output is not an authored
# reference. Skipping it by name keeps the proxy honest.
SKIP_DIRS=(
  .git
  node_modules
  target
  dist
  build
  coverage
  out
  .next
  .angular
  .turbo
  .cache
  .docusaurus
  __pycache__
  venv
  .venv
)

# Configuration and workflow files: the places a reference would actually take
# effect. Prose that merely mentions the repository is not the hazard.
is_inspected() {
  case "$1" in
    .github/*|*.yml|*.yaml|*.json|*.toml|*.ini|*.cfg|*.conf|.gitmodules) return 0 ;;
    *) return 1 ;;
  esac
}

scan() {
  local root="$1"
  local violations=0 inspected=0 pruned=0 file rel

  local -a prune=()
  local d
  for d in "${SKIP_DIRS[@]}"; do
    prune+=(-name "$d" -o)
  done
  unset 'prune[-1]'

  while IFS= read -r -d '' file; do
    pruned=$((pruned + 1))
  done < <(find "$root" \( "${prune[@]}" \) -type d -prune -print0 2>/dev/null)

  while IFS= read -r -d '' file; do
    rel="${file#"$root"/}"
    is_inspected "$rel" || continue
    inspected=$((inspected + 1))
    while IFS=: read -r line _; do
      [[ -n "$line" ]] || continue
      printf '::error file=%s,line=%s::Privileged repository reference is forbidden in unprivileged workflow/configuration.\n' \
        "$rel" "$line"
      violations=1
    done < <(LC_ALL=C grep -nI -i -F -- "$FORBIDDEN_REPO" "$file" || true)
  done < <(find "$root" \( "${prune[@]}" \) -type d -prune -o -type f -print0 2>/dev/null)

  printf 'privileged-boundary: %d configuration file(s) inspected, %d directory(ies) skipped by name.\n' \
    "$inspected" "$pruned"

  # COULD NOT RUN is a third outcome, and the version this replaced chose
  # "pass" for it: `git ls-files` inside a non-repository writes `fatal:` to
  # stderr, the process substitution yields nothing, `set -e` does not see it
  # because the loop itself succeeded, and the guard printed
  # "boundary check passed" having read zero files. Observed 2026-08-21. A walk
  # can fail the same way -- a root that does not exist -- so the number is
  # checked rather than assumed. No tree this guard protects has zero
  # configuration files.
  if (( inspected == 0 )); then
    echo "privileged-boundary: inspected 0 configuration files under $root." >&2
    echo "  That is a broken corpus, not a clean tree. Refusing rather than passing." >&2
    return 2
  fi

  return "$violations"
}

# --------------------------------------------------------------------------
# The refusal fixture. A gate that has never been observed refusing is
# unproven (field note 2), and for a CORPUS change the only thing worth
# asserting is that the newly-visible population is actually reached: this
# plants an IGNORED file and requires that the walk finds it AND that the old
# git-based corpus would have missed it. Passing only the first half would
# leave "the guard refuses things" proven and "the corpus widened" assumed.
# --------------------------------------------------------------------------
self_test() {
  local tmp status=0
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN

  git -C "$tmp" init -q
  printf 'local/\n' >"$tmp/.gitignore"
  printf 'name: ci\n' >"$tmp/ci.yml"
  git -C "$tmp" add -A >/dev/null 2>&1
  git -C "$tmp" -c user.email=t@t -c user.name=t commit -qm init >/dev/null 2>&1

  mkdir -p "$tmp/local"
  printf 'repo: michaelmarshall/%s\n' "$FORBIDDEN_REPO" >"$tmp/local/settings.yml"

  # 1. The planted file IS ignored -- otherwise the fixture proves nothing.
  if git -C "$tmp" check-ignore -q local/settings.yml; then
    echo "ok   the planted file is ignored by the tree's own .gitignore"
  else
    echo "FAIL the planted file is not ignored, so this fixture cannot test the hole"
    status=1
  fi

  # 2. The OLD corpus misses it. This is the half that proves the widening.
  if git -C "$tmp" ls-files --cached --others --exclude-standard \
    | grep -qx 'local/settings.yml'; then
    echo "FAIL the old git-based corpus already contained it; the fixture is wrong"
    status=1
  else
    echo "ok   the old --exclude-standard corpus did NOT contain it"
  fi

  # 3. The walk refuses it.
  if scan "$tmp" >/dev/null 2>&1; then
    echo "FAIL the walk did not refuse an ignored config naming the sealed repo"
    status=1
  else
    echo "ok   refuses an IGNORED configuration file naming the sealed repo"
  fi

  # 4. And still refuses a tracked one, so the rewrite kept what worked.
  printf 'url: https://github.com/michaelmarshall/%s.git\n' "$FORBIDDEN_REPO" >"$tmp/tracked.json"
  if scan "$tmp" >/dev/null 2>&1; then
    echo "FAIL a TRACKED configuration file naming the sealed repo was allowed"
    status=1
  else
    echo "ok   still refuses a tracked configuration file"
  fi
  rm -f "$tmp/tracked.json" "$tmp/local/settings.yml"

  # 5. A mention in PROSE is not a configuration reference. Asserting the pass
  #    matters as much as asserting the refusal: a guard that refuses every
  #    tree is indistinguishable from one nobody can satisfy.
  printf 'See the %s repository.\n' "$FORBIDDEN_REPO" >"$tmp/README.md"
  if scan "$tmp" >/dev/null 2>&1; then
    echo "ok   a prose mention in a non-configuration file is allowed"
  else
    echo "FAIL prose was refused, which no version of this guard has ever done"
    status=1
  fi

  # 6. A skipped directory is skipped by NAME, not by ignore status -- so a
  #    vendored dependency mentioning the repo is still not our violation.
  mkdir -p "$tmp/node_modules/x"
  printf 'repo: %s\n' "$FORBIDDEN_REPO" >"$tmp/node_modules/x/package.json"
  if scan "$tmp" >/dev/null 2>&1; then
    echo "ok   third-party content under a skipped directory is not inspected"
  else
    echo "FAIL a vendored file was treated as a first-party violation"
    status=1
  fi

  # 7. An EMPTY corpus is refused, not passed. The predecessor's worst
  #    behaviour was reporting success after reading nothing at all.
  local empty
  empty="$(mktemp -d)"
  set +e
  scan "$empty" >/dev/null 2>&1
  local rc=$?
  set -e
  rm -rf "$empty"
  if (( rc == 2 )); then
    echo "ok   an empty corpus exits 2 rather than reporting a clean tree"
  else
    echo "FAIL an empty corpus returned $rc; 'could not run' must not read as 'passed'"
    status=1
  fi

  # 8. And a root that does not exist is the same third outcome.
  set +e
  scan "$tmp/definitely-not-here" >/dev/null 2>&1
  rc=$?
  set -e
  if (( rc == 2 )); then
    echo "ok   a missing root exits 2"
  else
    echo "FAIL a missing root returned $rc"
    status=1
  fi

  if (( status == 0 )); then
    echo "SELF-TEST OK"
  else
    echo "SELF-TEST FAILED"
  fi
  return "$status"
}

if [[ "${1:-}" == "--self-test" ]]; then
  self_test
  exit $?
fi

if scan "$ROOT"; then
  echo "Privileged repository boundary check passed."
else
  exit 1
fi
